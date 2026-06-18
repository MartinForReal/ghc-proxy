//! Shared application state: HTTP client, configuration, token cache, model
//! list cache, and the in-memory request store. Also provides helpers for
//! token refresh and building upstream request headers.

use crate::auth;
use crate::config::{self, Config, ModelMappings};
use crate::store::RequestStore;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::RwLock as StdRwLock;
use tokio::sync::{Mutex, RwLock};

/// Mutable token state guarded by a mutex.
#[derive(Default)]
pub struct TokenState {
    pub github_token: String,
    pub copilot_token: Option<String>,
    /// Absolute unix expiry timestamp (seconds) of the Copilot token.
    pub expires_at: u64,
}

/// Application-wide shared state.
pub struct AppState {
    pub http: reqwest::Client,
    pub config: StdRwLock<Config>,
    pub tokens: Mutex<TokenState>,
    pub models: RwLock<Option<serde_json::Value>>,
    pub models_loaded_at: Mutex<Option<Instant>>,
    pub store: RequestStore,
    /// Timestamp of the last forwarded request, used for rate limiting.
    pub last_request: Mutex<Option<Instant>>,
    /// Stable 64-hex machine id (`vscode-machineid` header), persisted to disk.
    pub machine_id: String,
    /// Per-process session id (`vscode-sessionid` header): a UUID followed by a
    /// 13-digit millisecond timestamp, matching the real Copilot client format.
    pub session_id: String,
}

pub type SharedState = Arc<AppState>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current unix time in milliseconds (13 digits), used for the session id.
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl AppState {
    pub fn new(config: Config, github_token: String) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        AppState {
            http,
            config: StdRwLock::new(config),
            tokens: Mutex::new(TokenState {
                github_token,
                copilot_token: None,
                expires_at: 0,
            }),
            models: RwLock::new(None),
            models_loaded_at: Mutex::new(None),
            store: RequestStore::new(1000),
            last_request: Mutex::new(None),
            machine_id: auth::load_or_create_machine_id(),
            session_id: format!(
                "{}{}",
                uuid::Uuid::new_v4(),
                now_millis()
            ),
        }
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn is_debug(&self) -> bool {
        self.config.read().unwrap().debug
    }

    pub fn max_connection_retries(&self) -> u32 {
        self.config.read().unwrap().max_connection_retries
    }

    pub fn model_mappings(&self) -> ModelMappings {
        self.config.read().unwrap().model_mappings.clone()
    }

    pub fn copilot_base_url(&self) -> String {
        self.config.read().unwrap().copilot_base_url()
    }

    pub fn config_path(&self) -> String {
        config::config_path().display().to_string()
    }

    pub fn reload_config(&self) -> Config {
        let cfg = config::load_config();
        *self.config.write().unwrap() = cfg.clone();
        cfg
    }

    /// Headers used when talking to the GitHub REST API (token exchange).
    fn github_headers(&self, github_token: &str) -> HeaderMap {
        let cfg = self.config_snapshot();
        let mut h = HeaderMap::new();
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert("Accept", HeaderValue::from_static("application/json"));
        insert(&mut h, "Authorization", &format!("token {github_token}"));
        insert(
            &mut h,
            "Editor-Version",
            &format!("vscode/{}", cfg.vscode_version),
        );
        insert(
            &mut h,
            "Editor-Plugin-Version",
            &cfg.editor_plugin_version(),
        );
        insert(&mut h, "User-Agent", &cfg.user_agent());
        insert(&mut h, "X-GitHub-Api-Version", &cfg.api_version);
        h.insert(
            "X-VSCode-User-Agent-Library-Version",
            HeaderValue::from_static("electron-fetch"),
        );
        h
    }

    /// Headers used when talking to the upstream Copilot API.
    ///
    /// `vision` adds the `Copilot-Vision-Request` header. A fresh
    /// `X-Request-Id` is generated for every call.
    pub async fn copilot_headers(&self, vision: bool) -> HeaderMap {
        let cfg = self.config_snapshot();
        let copilot_token = {
            let tokens = self.tokens.lock().await;
            tokens.copilot_token.clone().unwrap_or_default()
        };
        let mut h = HeaderMap::new();
        let auth_value = format!("Bearer {}", copilot_token);
        insert(&mut h, "Authorization", &auth_value);
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert(
            "Copilot-Integration-Id",
            HeaderValue::from_static("vscode-chat"),
        );
        insert(
            &mut h,
            "Editor-Version",
            &format!("vscode/{}", cfg.vscode_version),
        );
        insert(
            &mut h,
            "Editor-Plugin-Version",
            &cfg.editor_plugin_version(),
        );
        insert(&mut h, "User-Agent", &cfg.user_agent());
        h.insert(
            "OpenAI-Intent",
            HeaderValue::from_static("conversation-panel"),
        );
        // The real Copilot client identifies its organization and installation,
        // which helps requests look like genuine editor traffic.
        h.insert(
            "openai-organization",
            HeaderValue::from_static("github-copilot"),
        );
        insert(&mut h, "vscode-machineid", &self.machine_id);
        insert(&mut h, "vscode-sessionid", &self.session_id);
        insert(&mut h, "X-GitHub-Api-Version", &cfg.api_version);
        // The latest Copilot client mirrors the request intent in the
        // `X-Interaction-Type` header for non-subagent/background requests.
        h.insert(
            "X-Interaction-Type",
            HeaderValue::from_static("conversation-panel"),
        );
        // A single request id is shared between `X-Request-Id` and
        // `X-Agent-Task-Id`, matching the latest Copilot client behavior.
        let request_id = uuid::Uuid::new_v4().to_string();
        insert(&mut h, "X-Request-Id", &request_id);
        insert(&mut h, "X-Agent-Task-Id", &request_id);
        h.insert(
            "X-VSCode-User-Agent-Library-Version",
            HeaderValue::from_static("electron-fetch"),
        );
        if vision {
            h.insert("Copilot-Vision-Request", HeaderValue::from_static("true"));
        }
        h
    }

    /// Refreshes the Copilot token if it is missing or within 60 seconds of
    /// expiry.
    pub async fn ensure_copilot_token(&self) -> Result<(), String> {
        {
            let tokens = self.tokens.lock().await;
            if tokens.copilot_token.is_some() && now_secs() < tokens.expires_at.saturating_sub(60) {
                return Ok(());
            }
        }
        self.refresh_copilot_token().await
    }

    /// Forces a Copilot token refresh.
    pub async fn refresh_copilot_token(&self) -> Result<(), String> {
        let github_token = {
            let tokens = self.tokens.lock().await;
            tokens.github_token.clone()
        };
        tracing::info!("Refreshing Copilot token...");
        let headers = self.github_headers(&github_token);
        let (token, expires_at) = auth::fetch_copilot_token(&self.http, headers).await?;
        if self.config_snapshot().show_token {
            tracing::info!("GitHub token: {github_token}");
            tracing::info!("Copilot token: {token}");
        }
        let mut tokens = self.tokens.lock().await;
        tokens.copilot_token = Some(token);
        tokens.expires_at = expires_at;
        tracing::info!("Copilot token refreshed successfully");
        Ok(())
    }

    /// Fetches the list of available models from upstream and caches it.
    pub async fn load_models(&self) -> Result<(), String> {
        self.ensure_copilot_token().await?;
        let url = format!("{}/models", self.copilot_base_url());
        let headers = self.copilot_headers(false).await;
        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("Failed to fetch models: {}", resp.status()));
        }
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let count = json
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        *self.models.write().await = Some(json);
        *self.models_loaded_at.lock().await = Some(Instant::now());
        tracing::info!("Loaded {count} models");
        Ok(())
    }

    pub async fn ensure_models_fresh(&self, max_age: Duration) -> Result<(), String> {
        let needs_refresh = {
            if self.models.read().await.is_none() {
                true
            } else {
                let loaded_at = self.models_loaded_at.lock().await;
                match *loaded_at {
                    Some(t) => t.elapsed() >= max_age,
                    None => true,
                }
            }
        };
        if needs_refresh {
            self.load_models().await?;
        }
        Ok(())
    }

    /// Returns true if the named model advertises support for a given
    /// upstream endpoint (e.g. `/v1/messages` or `/responses`).
    pub async fn model_supports_endpoint(&self, model: &str, endpoint: &str) -> bool {
        let models = self.models.read().await;
        let Some(models) = models.as_ref() else {
            return false;
        };
        models
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter().any(|m| {
                    m.get("id").and_then(|i| i.as_str()) == Some(model)
                        && m.get("supported_endpoints")
                            .and_then(|e| e.as_array())
                            .map(|eps| eps.iter().any(|e| e.as_str() == Some(endpoint)))
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// Whether the model should use the direct Anthropic upstream path.
    pub async fn use_direct_anthropic(&self, model: &str) -> bool {
        if self.config_snapshot().redirect_anthropic {
            return false;
        }
        self.model_supports_endpoint(model, "/v1/messages").await
    }

    /// Whether the named model advertises an extended (>200K) context window,
    /// i.e. the 1M-token tier unlocked by the `context-1m-2025-08-07` beta on
    /// the Anthropic-native endpoint. Reads `max_context_window_tokens` from the
    /// cached model catalog; returns false when the catalog is unavailable.
    pub async fn model_supports_1m(&self, model: &str) -> bool {
        let models = self.models.read().await;
        let Some(models) = models.as_ref() else {
            return false;
        };
        models
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter().any(|m| {
                    m.get("id").and_then(|i| i.as_str()) == Some(model)
                        && m.get("capabilities")
                            .and_then(|c| c.get("limits"))
                            .and_then(|l| l.get("max_context_window_tokens"))
                            .and_then(|t| t.as_u64())
                            .map(|t| t > 200_000)
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// Fetches the Copilot quota/usage summary for the authenticated GitHub
    /// account via `GET /copilot_internal/user`.
    pub async fn fetch_usage(&self) -> Result<serde_json::Value, String> {
        let github_token = {
            let tokens = self.tokens.lock().await;
            tokens.github_token.clone()
        };
        let url = format!("{}/copilot_internal/user", crate::config::GITHUB_API);
        let headers = self.github_headers(&github_token);
        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| format!("Failed to fetch usage: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Failed to fetch usage: {status} {body}"));
        }
        resp.json()
            .await
            .map_err(|e| format!("Failed to parse usage: {e}"))
    }

    /// Applies manual approval and rate limiting before a request is forwarded.
    ///
    /// Returns `Err(message)` when the request should be rejected (HTTP 429
    /// because rate limiting is active and `rate_limit_wait` is disabled);
    /// otherwise returns `Ok(())`, possibly after sleeping or waiting for
    /// interactive approval.
    pub async fn apply_request_gate(&self, endpoint: &str) -> Result<(), String> {
        let cfg = self.config_snapshot();
        if cfg.manual_approve {
            println!("\n[manual] Approve request to {endpoint}? Press Enter to continue...");
            let mut line = String::new();
            // Read a line from stdin without blocking the async runtime.
            let _ = tokio::task::spawn_blocking(move || {
                std::io::stdin().read_line(&mut line)
            })
            .await;
        }

        if let Some(limit) = cfg.rate_limit_seconds {
            if limit > 0 {
                let limit = Duration::from_secs(limit);
                let mut last = self.last_request.lock().await;
                if let Some(prev) = *last {
                    let elapsed = prev.elapsed();
                    if elapsed < limit {
                        let remaining = limit - elapsed;
                        if cfg.rate_limit_wait {
                            tracing::info!(
                                "[rate-limit] waiting {:.1}s before forwarding {endpoint}",
                                remaining.as_secs_f64()
                            );
                            tokio::time::sleep(remaining).await;
                        } else {
                            return Err(format!(
                                "Rate limit exceeded; retry in {:.1}s",
                                remaining.as_secs_f64()
                            ));
                        }
                    }
                }
                *last = Some(Instant::now());
            }
        }
        Ok(())
    }
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let (Ok(n), Ok(v)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(n, v);
    }
}

/// Reshapes the raw `/copilot_internal/user` response into a compact usage
/// summary: the plan, the quota reset date, and a per-category breakdown
/// (entitlement / remaining / percent remaining / unlimited) for each entry in
/// `quota_snapshots`. The original payload is preserved under `raw` so callers
/// never lose information the upstream may add.
pub fn summarize_usage(raw: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;

    let mut quotas = serde_json::Map::new();
    if let Some(snapshots) = raw.get("quota_snapshots").and_then(|s| s.as_object()) {
        for (name, snap) in snapshots {
            let unlimited = snap
                .get("unlimited")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let entitlement = snap.get("entitlement").and_then(|v| v.as_f64());
            let remaining = snap.get("remaining").and_then(|v| v.as_f64());
            let percent_remaining = snap.get("percent_remaining").and_then(|v| v.as_f64());
            quotas.insert(
                name.clone(),
                json!({
                    "unlimited": unlimited,
                    "entitlement": entitlement,
                    "remaining": remaining,
                    "percent_remaining": percent_remaining,
                }),
            );
        }
    }

    let plan = raw
        .get("copilot_plan")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string());
    let reset_date = raw
        .get("quota_reset_date")
        .and_then(|d| d.as_str())
        .map(|s| s.to_string());

    json!({
        "plan": plan,
        "quota_reset_date": reset_date,
        "quotas": quotas,
        "raw": raw,
    })
}
