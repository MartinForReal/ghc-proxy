//! Shared application state: HTTP client, configuration, token cache, model
//! list cache, and the in-memory request store. Also provides helpers for
//! token refresh and building upstream request headers.

use crate::auth;
use crate::config::Config;
use crate::store::RequestStore;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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
    pub config: Config,
    pub tokens: Mutex<TokenState>,
    pub models: RwLock<Option<serde_json::Value>>,
    pub store: RequestStore,
}

pub type SharedState = Arc<AppState>;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AppState {
    pub fn new(config: Config, github_token: String) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("failed to build HTTP client");
        AppState {
            http,
            config,
            tokens: Mutex::new(TokenState {
                github_token,
                copilot_token: None,
                expires_at: 0,
            }),
            models: RwLock::new(None),
            store: RequestStore::new(1000),
        }
    }

    /// Headers used when talking to the GitHub REST API (token exchange).
    fn github_headers(&self, github_token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert("Accept", HeaderValue::from_static("application/json"));
        insert(&mut h, "Authorization", &format!("token {github_token}"));
        insert(
            &mut h,
            "Editor-Version",
            &format!("vscode/{}", self.config.vscode_version),
        );
        insert(
            &mut h,
            "Editor-Plugin-Version",
            &self.config.editor_plugin_version(),
        );
        insert(&mut h, "User-Agent", &self.config.user_agent());
        insert(&mut h, "X-GitHub-Api-Version", &self.config.api_version);
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
            &format!("vscode/{}", self.config.vscode_version),
        );
        insert(
            &mut h,
            "Editor-Plugin-Version",
            &self.config.editor_plugin_version(),
        );
        insert(&mut h, "User-Agent", &self.config.user_agent());
        h.insert(
            "OpenAI-Intent",
            HeaderValue::from_static("conversation-panel"),
        );
        insert(&mut h, "X-GitHub-Api-Version", &self.config.api_version);
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
        let mut tokens = self.tokens.lock().await;
        tokens.copilot_token = Some(token);
        tokens.expires_at = expires_at;
        tracing::info!("Copilot token refreshed successfully");
        Ok(())
    }

    /// Fetches the list of available models from upstream and caches it.
    pub async fn load_models(&self) -> Result<(), String> {
        self.ensure_copilot_token().await?;
        let url = format!("{}/models", self.config.copilot_base_url());
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
        tracing::info!("Loaded {count} models");
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
        if self.config.redirect_anthropic {
            return false;
        }
        self.model_supports_endpoint(model, "/v1/messages").await
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
