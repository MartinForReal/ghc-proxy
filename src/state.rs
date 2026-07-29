//! Shared application state: HTTP client, configuration, token cache, model
//! list cache, and the in-memory request store. Also provides helpers for
//! token refresh and building upstream request headers.

use crate::auth;
use crate::config::{self, Config, ModelMappings};
use crate::store::RequestStore;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};

/// Mutable token state guarded by a mutex.#[derive(Default)]
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
    /// Instant the process started serving, used to report uptime on `/health`.
    pub started_at: Instant,
    /// Latest per-SKU quota reported by the upstream, keyed by SKU name.
    pub quotas: StdRwLock<BTreeMap<String, QuotaSnapshot>>,
}

pub type SharedState = Arc<AppState>;

/// Prefix of the per-SKU quota headers Copilot attaches to every response.
const QUOTA_HEADER_PREFIX: &str = "x-quota-snapshot-";

/// A quota snapshot for one billing SKU, as reported by the upstream on every
/// response.
///
/// Copilot returns these on each request (`x-quota-snapshot-chat`,
/// `-completions`, `-premium_interactions`), so live quota costs nothing —
/// unlike `/copilot_internal/user`, which is a separate API call.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct QuotaSnapshot {
    /// Allowance for the period. Negative means unlimited.
    pub entitlement: f64,
    /// Amount consumed beyond the entitlement.
    pub overage: f64,
    /// Whether going past the entitlement is allowed at all.
    pub overage_permitted: bool,
    /// Percentage of the entitlement still available.
    pub percent_remaining: f64,
    /// True when the SKU has no cap.
    pub unlimited: bool,
    /// When the allowance resets, if the upstream reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_date: Option<String>,
}

/// Decodes the `%XX` escapes used in the header's date field.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses a quota header value such as
/// `ent=-1&ov=0.0&ovPerm=true&rem=100.0&rst=2026-08-01T00%3A00%3A00Z`.
///
/// Returns `None` when nothing recognizable is present, so an upstream that
/// stops sending these (they are undocumented) simply leaves the cache empty
/// rather than reporting zeroed quota.
pub fn parse_quota_snapshot(value: &str) -> Option<QuotaSnapshot> {
    let mut snapshot = QuotaSnapshot::default();
    let mut saw_field = false;
    for pair in value.split('&') {
        let Some((key, raw)) = pair.split_once('=') else {
            continue;
        };
        match key.trim() {
            "ent" => {
                if let Ok(v) = raw.parse::<f64>() {
                    snapshot.entitlement = v;
                    snapshot.unlimited = v < 0.0;
                    saw_field = true;
                }
            }
            "ov" => {
                if let Ok(v) = raw.parse::<f64>() {
                    snapshot.overage = v;
                    saw_field = true;
                }
            }
            "ovPerm" => {
                snapshot.overage_permitted = raw.eq_ignore_ascii_case("true");
                saw_field = true;
            }
            "rem" => {
                if let Ok(v) = raw.parse::<f64>() {
                    snapshot.percent_remaining = v;
                    saw_field = true;
                }
            }
            "rst" => {
                let decoded = percent_decode(raw);
                if !decoded.is_empty() {
                    snapshot.reset_date = Some(decoded);
                    saw_field = true;
                }
            }
            _ => {}
        }
    }
    saw_field.then_some(snapshot)
}

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
        // No global request timeout: SSE responses are long-lived streams, and
        // capping their total duration would cut off legitimate long answers.
        // Instead the connect phase, idle pooled connections, and the gap
        // *between* reads are bounded. The read timeout is what turns a
        // half-open upstream into an error the streaming paths can report,
        // rather than a request that hangs forever.
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90));
        if config.upstream_read_timeout_seconds > 0 {
            builder =
                builder.read_timeout(Duration::from_secs(config.upstream_read_timeout_seconds));
        }
        let http = builder.build().expect("failed to build HTTP client");
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
            session_id: format!("{}{}", uuid::Uuid::new_v4(), now_millis()),
            started_at: Instant::now(),
            quotas: StdRwLock::new(BTreeMap::new()),
        }
    }

    /// Records the per-SKU quota headers attached to an upstream response.
    ///
    /// Called for every Copilot response, so quota stays current without the
    /// extra `/copilot_internal/user` round trip. Headers that do not parse are
    /// ignored, leaving the previous value in place.
    pub fn record_quota_headers(&self, headers: &HeaderMap) {
        let mut parsed: Vec<(String, QuotaSnapshot)> = Vec::new();
        for (name, value) in headers {
            let Some(sku) = name.as_str().strip_prefix(QUOTA_HEADER_PREFIX) else {
                continue;
            };
            let Ok(value) = value.to_str() else { continue };
            if let Some(snapshot) = parse_quota_snapshot(value) {
                parsed.push((sku.to_string(), snapshot));
            }
        }
        if parsed.is_empty() {
            return;
        }
        if let Ok(mut quotas) = self.quotas.write() {
            for (sku, snapshot) in parsed {
                quotas.insert(sku, snapshot);
            }
        }
    }

    /// The most recent per-SKU quota reported by the upstream.
    pub fn quota_snapshot(&self) -> BTreeMap<String, QuotaSnapshot> {
        self.quotas.read().map(|q| q.clone()).unwrap_or_default()
    }

    /// Seconds this process has been running.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Copilot token status as `(present, seconds_until_expiry)`. The remaining
    /// lifetime is zero when the token is missing or already expired.
    pub async fn copilot_token_status(&self) -> (bool, u64) {
        let tokens = self.tokens.lock().await;
        let present = tokens.copilot_token.is_some();
        let remaining = tokens.expires_at.saturating_sub(now_secs());
        (present, if present { remaining } else { 0 })
    }

    /// Number of models currently held in the catalog cache.
    pub async fn model_count(&self) -> usize {
        self.models
            .read()
            .await
            .as_ref()
            .and_then(|m| m.get("data"))
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    }

    /// Looks up a single entry from the cached model catalog by id.
    pub async fn find_model(&self, id: &str) -> Option<serde_json::Value> {
        self.models
            .read()
            .await
            .as_ref()
            .and_then(|m| m.get("data"))
            .and_then(|d| d.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(id))
                    .cloned()
            })
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

    /// The configured endpoint API key, if authentication is enabled.
    pub fn api_key(&self) -> Option<String> {
        self.config
            .read()
            .unwrap()
            .api_key
            .as_ref()
            .filter(|k| !k.is_empty())
            .cloned()
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

    /// Token used for GitHub Models requests: the dedicated `github_models.token`
    /// when configured, otherwise the resolved GitHub token. This token must
    /// carry the `models: read` permission (a fine-grained PAT); the Device Flow
    /// token does not have it.
    pub async fn github_models_token(&self) -> String {
        if let Some(token) = self.config_snapshot().github_models.token {
            if !token.is_empty() {
                return token;
            }
        }
        self.tokens.lock().await.github_token.clone()
    }

    /// Headers for a GitHub Models inference request. Unlike the Copilot path,
    /// this authenticates with the raw GitHub token via `Authorization: Bearer`
    /// and sends the standard GitHub REST API headers. None of the Copilot
    /// impersonation headers are included.
    pub async fn github_models_headers(&self) -> HeaderMap {
        let cfg = self.config_snapshot();
        let token = self.github_models_token().await;
        let mut h = HeaderMap::new();
        insert(&mut h, "Authorization", &format!("Bearer {token}"));
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert(
            "Accept",
            HeaderValue::from_static("application/vnd.github+json"),
        );
        insert(
            &mut h,
            "X-GitHub-Api-Version",
            &cfg.github_models.api_version,
        );
        insert(&mut h, "User-Agent", &cfg.user_agent());
        h
    }

    /// Resolves the upstream chat-completions `(url, headers, is_github_models)`
    /// for a given (translated) model. Routes to GitHub Models inference when
    /// the model uses the `publisher/model` convention and GitHub Models is
    /// enabled; otherwise uses the Copilot upstream. The `vision` flag only
    /// affects the Copilot headers.
    pub async fn chat_upstream(&self, model: &str, vision: bool) -> (String, HeaderMap, bool) {
        if self.config_snapshot().routes_to_github_models(model) {
            let url = self.config_snapshot().github_models_inference_url();
            (url, self.github_models_headers().await, true)
        } else {
            let url = format!("{}/chat/completions", self.copilot_base_url());
            (url, self.copilot_headers(vision).await, false)
        }
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

    /// Fetches the list of available models from upstream and caches it. When
    /// GitHub Models is enabled, its catalog is appended (best-effort) so those
    /// models also appear in `/v1/models` and the dashboard.
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
        let mut json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let mut count = json
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        if self.config_snapshot().github_models.enabled {
            match self.load_github_models_catalog().await {
                Ok(entries) => {
                    let added = entries.len();
                    if let Some(data) = json.get_mut("data").and_then(|d| d.as_array_mut()) {
                        data.extend(entries);
                        count += added;
                    }
                    tracing::info!("Loaded {added} GitHub Models catalog entries");
                }
                Err(e) => tracing::warn!("GitHub Models catalog unavailable: {e}"),
            }
        }

        *self.models.write().await = Some(json);
        *self.models_loaded_at.lock().await = Some(Instant::now());
        tracing::info!("Loaded {count} models");
        Ok(())
    }

    /// Fetches the GitHub Models catalog (`GET /catalog/models`) and normalizes
    /// each entry into the model-list shape used by `/v1/models`
    /// (`id` / `name` / `vendor`). Returns an error the caller can log without
    /// failing the primary Copilot model load.
    async fn load_github_models_catalog(&self) -> Result<Vec<serde_json::Value>, String> {
        let url = self.config_snapshot().github_models_catalog_url();
        let headers = self.github_models_headers().await;
        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("catalog fetch returned {}", resp.status()));
        }
        let catalog: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        // The catalog is documented as a bare array; tolerate `{models|data:[…]}`
        // wrappers as well.
        let arr = catalog
            .as_array()
            .cloned()
            .or_else(|| catalog.get("models").and_then(|m| m.as_array()).cloned())
            .or_else(|| catalog.get("data").and_then(|m| m.as_array()).cloned())
            .unwrap_or_default();
        let entries = arr
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|i| i.as_str())?;
                let name = m.get("name").and_then(|n| n.as_str()).unwrap_or(id);
                let vendor = m
                    .get("publisher")
                    .and_then(|p| p.as_str())
                    .unwrap_or("github-models");
                Some(serde_json::json!({
                    "id": id,
                    "name": name,
                    "vendor": vendor,
                    "source": "github-models",
                }))
            })
            .collect();
        Ok(entries)
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

    /// Copilot premium-request cost for a model, from the catalog's
    /// `billing.multiplier`. `None` when the catalog does not price the model.
    pub async fn model_premium_multiplier(&self, model: &str) -> Option<f64> {
        self.models
            .read()
            .await
            .as_ref()
            .and_then(|catalog| premium_multiplier_from_catalog(catalog, model))
    }

    /// Maximum output tokens advertised for a model
    /// (`capabilities.limits.max_output_tokens`). Used to fill in a missing
    /// `max_tokens`, which some Copilot models reject the request without.
    pub async fn model_max_output_tokens(&self, model: &str) -> Option<u64> {
        self.models
            .read()
            .await
            .as_ref()
            .and_then(|m| m.get("data"))
            .and_then(|d| d.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
                    .and_then(|m| m.get("capabilities"))
                    .and_then(|c| c.get("limits"))
                    .and_then(|l| l.get("max_output_tokens"))
                    .and_then(|t| t.as_u64())
            })
    }

    /// Returns the tokenizer name advertised by the model's catalog entry
    /// (`capabilities.tokenizer`), used to pick a tiktoken encoder for local
    /// token estimation. Falls back to `cl100k_base` when unknown.
    pub async fn model_tokenizer(&self, model: &str) -> String {
        let models = self.models.read().await;
        models
            .as_ref()
            .and_then(|m| m.get("data"))
            .and_then(|d| d.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
                    .and_then(|m| m.get("capabilities"))
                    .and_then(|c| c.get("tokenizer"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "cl100k_base".to_string())
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
            let _ =
                tokio::task::spawn_blocking(move || std::io::stdin().read_line(&mut line)).await;
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

/// Extracts a model's Copilot premium-request cost from a `/models` catalog
/// payload.
///
/// Returns `None` when the model is absent from the catalog or carries no
/// `billing.multiplier`, so a model the catalog never priced is not silently
/// counted as a full premium request.
fn premium_multiplier_from_catalog(catalog: &serde_json::Value, model: &str) -> Option<f64> {
    catalog
        .get("data")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))?
        .get("billing")?
        .get("multiplier")?
        .as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn parses_a_real_quota_header() {
        // Captured verbatim from a live Copilot response.
        let q = parse_quota_snapshot(
            "ent=-1&ov=0.0&ovPerm=true&rem=100.0&rst=2026-08-01T00%3A00%3A00Z",
        )
        .expect("header should parse");
        assert_eq!(q.entitlement, -1.0);
        assert!(q.unlimited, "a negative entitlement means unlimited");
        assert_eq!(q.overage, 0.0);
        assert!(q.overage_permitted);
        assert_eq!(q.percent_remaining, 100.0);
        // The date arrives percent-encoded and must be decoded.
        assert_eq!(q.reset_date.as_deref(), Some("2026-08-01T00:00:00Z"));
    }

    #[test]
    fn parses_a_capped_sku() {
        let q = parse_quota_snapshot("ent=300&ov=12.5&ovPerm=false&rem=42.5").unwrap();
        assert_eq!(q.entitlement, 300.0);
        assert!(!q.unlimited);
        assert_eq!(q.overage, 12.5);
        assert!(!q.overage_permitted);
        assert_eq!(q.percent_remaining, 42.5);
        assert!(q.reset_date.is_none());
    }

    #[test]
    fn unrecognized_quota_headers_are_ignored() {
        // These headers are undocumented; if the shape changes we must report
        // nothing rather than a confident zero.
        assert!(parse_quota_snapshot("").is_none());
        assert!(parse_quota_snapshot("garbage").is_none());
        assert!(parse_quota_snapshot("unknown=1&other=2").is_none());
        // A partially recognizable value still yields what it does contain.
        assert_eq!(
            parse_quota_snapshot("rem=7.5&junk=x")
                .unwrap()
                .percent_remaining,
            7.5
        );
    }

    #[test]
    fn quota_headers_are_recorded_per_sku() {
        let state = AppState::new(Config::default(), "t".into());
        assert!(state.quota_snapshot().is_empty());

        let mut h = HeaderMap::new();
        h.insert(
            "x-quota-snapshot-chat",
            HeaderValue::from_static("ent=-1&ov=0.0&ovPerm=false&rem=100.0"),
        );
        h.insert(
            "x-quota-snapshot-premium_interactions",
            HeaderValue::from_static("ent=300&ov=0.0&ovPerm=true&rem=88.0"),
        );
        h.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        state.record_quota_headers(&h);

        let q = state.quota_snapshot();
        assert_eq!(q.len(), 2, "only the quota headers are captured: {q:?}");
        assert_eq!(q["chat"].percent_remaining, 100.0);
        assert_eq!(q["premium_interactions"].percent_remaining, 88.0);
        assert_eq!(q["premium_interactions"].entitlement, 300.0);

        // A later response updates in place.
        let mut h2 = HeaderMap::new();
        h2.insert(
            "x-quota-snapshot-premium_interactions",
            HeaderValue::from_static("ent=300&ov=0.0&ovPerm=true&rem=87.0"),
        );
        state.record_quota_headers(&h2);
        let q = state.quota_snapshot();
        assert_eq!(q["premium_interactions"].percent_remaining, 87.0);
        // Untouched SKUs keep their previous value.
        assert_eq!(q["chat"].percent_remaining, 100.0);
    }

    #[test]
    fn responses_without_quota_headers_leave_the_cache_alone() {
        let state = AppState::new(Config::default(), "t".into());
        let mut h = HeaderMap::new();
        h.insert(
            "x-quota-snapshot-chat",
            HeaderValue::from_static("ent=-1&rem=55.0"),
        );
        state.record_quota_headers(&h);

        // An upstream that stops sending them must not wipe what we know.
        state.record_quota_headers(&HeaderMap::new());
        assert_eq!(state.quota_snapshot()["chat"].percent_remaining, 55.0);
    }

    /// Serves one request: SSE headers plus half an event, then stalls forever
    /// without ever closing the socket. Returns the bound address.
    async fn stalling_upstream() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Type: text/event-stream\r\n\
                          Transfer-Encoding: chunked\r\n\r\n",
                    )
                    .await;
                // A chunk that stops in the middle of a `data:` line.
                let partial: &[u8] = b"data: {\"partial\":";
                let _ = sock
                    .write_all(format!("{:X}\r\n", partial.len()).as_bytes())
                    .await;
                let _ = sock.write_all(partial).await;
                let _ = sock.write_all(b"\r\n").await;
                let _ = sock.flush().await;
                std::future::pending::<()>().await;
            }
        });
        format!("http://{addr}/")
    }

    /// The streaming paths only notice a dead upstream when the body stream
    /// yields an error. Without a read timeout a half-open connection yields
    /// neither data nor error nor end-of-stream, so the request hangs forever
    /// and none of the stream-interrupted handling ever runs.
    #[tokio::test]
    async fn read_timeout_surfaces_a_stalled_upstream_as_an_error() {
        let url = stalling_upstream().await;
        let cfg = Config {
            upstream_read_timeout_seconds: 1,
            ..Default::default()
        };
        let state = AppState::new(cfg, "token".into());

        let resp = state.http.get(&url).send().await.expect("headers arrive");
        assert!(resp.status().is_success());

        let mut stream = resp.bytes_stream();
        let first = stream.next().await.expect("first chunk").expect("is data");
        assert_eq!(&first[..], b"data: {\"partial\":");

        let outcome = tokio::time::timeout(Duration::from_secs(15), stream.next()).await;
        let item = outcome.expect("must not hang past the read timeout");
        let err = item.expect("stream must yield an item").unwrap_err();
        assert!(
            err.is_timeout(),
            "expected a timeout error, got: {err:?} ({err})"
        );
    }

    /// Setting the timeout to zero disables it, for operators who would rather
    /// let a stream run indefinitely.
    #[tokio::test]
    async fn read_timeout_can_be_disabled() {
        let url = stalling_upstream().await;
        let cfg = Config {
            upstream_read_timeout_seconds: 0,
            ..Default::default()
        };
        let state = AppState::new(cfg, "token".into());
        let resp = state.http.get(&url).send().await.expect("headers arrive");
        let mut stream = resp.bytes_stream();
        let _ = stream.next().await;
        let outcome = tokio::time::timeout(Duration::from_secs(3), stream.next()).await;
        assert!(outcome.is_err(), "expected the stream to still be hanging");
    }

    #[test]
    fn premium_multiplier_reads_billing_from_the_catalog() {
        // Shape taken from a real `GET /models` response.
        let catalog = json!({"data": [
            {"id": "claude-opus-5", "billing": {"is_premium": true, "multiplier": 1.0}},
            {"id": "gpt-4.1", "billing": {"is_premium": false, "multiplier": 0.0}},
            {"id": "discounted", "billing": {"is_premium": true, "multiplier": 0.33}},
            {"id": "no-billing-block"}
        ]});
        assert_eq!(
            premium_multiplier_from_catalog(&catalog, "claude-opus-5"),
            Some(1.0)
        );
        assert_eq!(
            premium_multiplier_from_catalog(&catalog, "gpt-4.1"),
            Some(0.0)
        );
        assert_eq!(
            premium_multiplier_from_catalog(&catalog, "discounted"),
            Some(0.33)
        );
        // A model without billing metadata, or one that is not in the catalog
        // at all, must stay unknown rather than defaulting to a full request.
        assert_eq!(
            premium_multiplier_from_catalog(&catalog, "no-billing-block"),
            None
        );
        assert_eq!(
            premium_multiplier_from_catalog(&catalog, "nonexistent"),
            None
        );
    }

    #[test]
    fn premium_multiplier_survives_a_malformed_catalog() {
        assert_eq!(
            premium_multiplier_from_catalog(&json!({}), "claude-opus-5"),
            None
        );
        assert_eq!(
            premium_multiplier_from_catalog(&json!({"data": "not-an-array"}), "x"),
            None
        );
    }
}
