//! Authentication: GitHub token acquisition (environment variable, token file,
//! or Device Flow) and Copilot token exchange / refresh.

use crate::config::{config_dir, GITHUB_API, GITHUB_CLIENT_ID};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// Path to the persisted GitHub token file inside the config directory.
pub fn token_file_path() -> PathBuf {
    config_dir().join("github_token.txt")
}

/// Path to the persisted machine-id file inside the config directory.
fn machine_id_path() -> PathBuf {
    config_dir().join("machine_id.txt")
}

/// Returns a stable 64-hex-character machine id, persisted across runs to mimic
/// the `vscode-machineid` header sent by the real Copilot client. The value is
/// created on first use and reused thereafter.
pub fn load_or_create_machine_id() -> String {
    let path = machine_id_path();
    if let Ok(contents) = std::fs::read_to_string(&path) {
        let trimmed = contents.trim().to_string();
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return trimmed;
        }
    }
    // 64 hex chars = two simple (dashless) UUIDs concatenated.
    let id = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    if let Err(e) = std::fs::create_dir_all(config_dir()) {
        tracing::warn!("Failed to create config dir: {e}");
    } else if let Err(e) = std::fs::write(&path, &id) {
        tracing::warn!("Failed to save machine id file: {e}");
    } else {
        restrict_token_permissions(&path);
    }
    id
}

/// Reads a previously saved GitHub token from disk, if present.
pub fn load_saved_token() -> Option<String> {
    let path = token_file_path();
    let contents = std::fs::read_to_string(&path).ok()?;
    let trimmed = contents.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        tracing::info!("Loaded GitHub token from {}", path.display());
        Some(trimmed)
    }
}

/// Persists the GitHub token to disk for reuse across runs.
pub fn save_token(token: &str) {
    let dir = config_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create config dir: {e}");
        return;
    }
    let path = token_file_path();
    match std::fs::write(&path, token) {
        Ok(_) => {
            restrict_token_permissions(&path);
            tracing::info!("Saved GitHub token to {}", path.display());
        }
        Err(e) => tracing::warn!("Failed to save token file: {e}"),
    }
}

/// Restricts the saved token file to owner read/write (`0600`) on Unix.
/// No-op on other platforms.
#[cfg(unix)]
fn restrict_token_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!("Failed to set token file permissions: {e}");
    }
}

#[cfg(not(unix))]
fn restrict_token_permissions(_path: &std::path::Path) {}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_expires")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_expires() -> u64 {
    900
}
fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Runs the GitHub Device Flow, printing instructions to stdout and polling
/// until the user authorizes (or the request times out). Returns the access
/// token on success.
pub async fn device_flow(client: &reqwest::Client) -> Option<String> {
    println!("\n{}", "=".repeat(60));
    println!("GitHub Device Flow Authentication");
    println!("{}", "=".repeat(60));

    let resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", GITHUB_CLIENT_ID),
            // `models` unlocks the GitHub Models inference API
            // (https://models.github.ai). Classic/OAuth tokens use the `models`
            // scope; fine-grained PATs use the `models: read` permission. Without
            // it the models endpoint returns 401/Unauthorized.
            ("scope", "read:user copilot models"),
        ])
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            println!("Failed to get device code: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        println!(
            "Failed to get device code: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        return None;
    }
    let dc: DeviceCodeResponse = match resp.json().await {
        Ok(d) => d,
        Err(e) => {
            println!("Failed to parse device code response: {e}");
            return None;
        }
    };

    println!("\nPlease visit: {}", dc.verification_uri);
    println!("And enter the code: {}", dc.user_code);
    println!(
        "\nWaiting for authorization (expires in {} seconds)...",
        dc.expires_in
    );

    let mut interval = dc.interval;
    let deadline = std::time::Instant::now() + Duration::from_secs(dc.expires_in);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", GITHUB_CLIENT_ID),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        let body: AccessTokenResponse = match resp.json().await {
            Ok(b) => b,
            Err(_) => continue,
        };
        match body.error.as_deref() {
            Some("authorization_pending") => {
                continue;
            }
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some("expired_token") => {
                println!("\nAuthorization expired. Please try again.");
                return None;
            }
            Some("access_denied") => {
                println!("\nAuthorization denied by user.");
                return None;
            }
            Some(other) => {
                println!(
                    "\nError: {}",
                    body.error_description.as_deref().unwrap_or(other)
                );
                return None;
            }
            None => {}
        }
        if let Some(token) = body.access_token {
            println!("\n\nAuthorization successful!");
            return Some(token);
        }
    }
    println!("\nAuthorization timed out. Please try again.");
    None
}

/// Resolves a GitHub token from an environment variable, the saved token file,
/// or by running the Device Flow (saving the result).
///
/// Environment variables are checked in the same priority order used by the
/// GitHub Copilot SDK: `COPILOT_GITHUB_TOKEN`, then `GH_TOKEN`, then
/// `GITHUB_TOKEN`.
pub async fn resolve_github_token(client: &reqwest::Client) -> Option<String> {
    for name in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(name) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                tracing::info!("Using GitHub token from {name} environment variable");
                return Some(token);
            }
        }
    }
    if let Some(token) = load_saved_token() {
        return Some(token);
    }
    println!("\nNo GitHub token found. Starting GitHub Device Flow authentication...");
    let token = device_flow(client).await?;
    save_token(&token);
    Some(token)
}

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: String,
    #[serde(default)]
    refresh_in: Option<u64>,
}

/// Exchanges a GitHub token for a short-lived Copilot token via the
/// `copilot_internal/v2/token` endpoint. Returns the token and the absolute
/// unix expiry timestamp (seconds).
pub async fn fetch_copilot_token(
    client: &reqwest::Client,
    headers: reqwest::header::HeaderMap,
) -> Result<(String, u64), String> {
    let url = format!("{GITHUB_API}/copilot_internal/v2/token");
    let resp = client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| format!("Failed to get Copilot token: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to get Copilot token: {status} {body}"));
    }
    let parsed: CopilotTokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Copilot token: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_at = now + parsed.refresh_in.unwrap_or(1800);
    Ok((parsed.token, expires_at))
}
