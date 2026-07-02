//! HTTP server: route definitions and request handlers for the OpenAI- and
//! Anthropic-compatible proxy endpoints, plus the analytics dashboard API.

use crate::anthropic::{self, AnthropicStreamState};
use crate::gemini;
use crate::responses as codex;
use crate::state::SharedState;
use crate::store::RequestRecord;
use crate::translate;
use crate::util;
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Builds the application router with all routes mounted.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/models", get(get_models))
        .route("/models", get(get_models))
        .route("/v1/models/full/", get(get_models_full))
        .route("/models/full/", get(get_models_full))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/embeddings", post(embeddings))
        .route("/embeddings", post(embeddings))
        .route("/usage", get(usage))
        .route("/metrics", get(metrics_openmetrics))
        .route("/", get(dashboard))
        .route("/requests", get(requests_page))
        .route("/metrics/dashboard", get(metrics_page))
        .route("/api/stats", get(api_stats))
        .route("/api/requests", get(api_requests))
        .route("/api/audit", get(api_audit))
        .route("/api/audit/summary", get(api_audit_summary))
        .route("/api/config/reload", post(api_reload_config))
        .route("/api/models", get(get_models))
        .route("/v1beta/models/{model_action}", post(gemini_generate))
        .route("/openapi.json", get(openapi_spec))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024 * 1024)) // 20 GB limit
        .with_state(state)
}

/// Whether a request path is an LLM API endpoint that should be guarded by the
/// optional API key. The dashboard UI, static assets, and metrics endpoints are
/// intentionally left open so local monitoring keeps working without a key.
fn is_protected_path(path: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "/v1/",
        "/chat/completions",
        "/responses",
        "/embeddings",
        "/models",
        "/v1beta/",
    ];
    PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Constant-time byte comparison to avoid leaking the key through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extracts a presented API key from the standard provider headers:
/// `Authorization: Bearer <key>`, `x-api-key: <key>`, or `x-goog-api-key: <key>`.
fn presented_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        if let Some(rest) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            return Some(rest.trim().to_string());
        }
    }
    if let Some(v) = headers.get("x-api-key").and_then(|h| h.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    if let Some(v) = headers.get("x-goog-api-key").and_then(|h| h.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    None
}

/// Authentication middleware. When an API key is configured, every request to a
/// protected LLM endpoint must present a matching key. When no key is
/// configured, all requests pass through unchanged.
async fn auth_middleware(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.api_key() else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    if !is_protected_path(path) {
        return next.run(request).await;
    }
    let presented = presented_api_key(request.headers());
    let ok = presented
        .as_deref()
        .map(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if ok {
        next.run(request).await
    } else {
        tracing::warn!("[auth] rejected unauthenticated request to {path}");
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "message": "Missing or invalid API key.",
                    "type": "authentication_error",
                    "code": "invalid_api_key"
                }
            })),
        )
            .into_response()
    }
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(json!({"error": "Not found"})))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn elapsed_secs(start: Instant) -> f64 {
    let secs = start.elapsed().as_secs_f64();
    (secs * 100.0).round() / 100.0
}

/// SSE response headers.
fn sse_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    h.insert("Connection", HeaderValue::from_static("keep-alive"));
    h.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    h
}

fn set_initiator(headers: &mut HeaderMap, agent: bool) {
    let v = if agent { "agent" } else { "user" };
    headers.insert("X-Initiator", HeaderValue::from_static(v));
}

/// Logs an upstream error to `error.log` in the config directory.
fn log_error(endpoint: &str, request: &Value, response: &str, status: u16) {
    let dir = crate::config::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let entry = json!({
        "timestamp": now_iso(),
        "endpoint": endpoint,
        "status_code": status,
        "request": request,
        "response": response,
    });
    let path = dir.join("error.log");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{entry}");
    }
}

/// Logs the body of a request forwarded upstream to the tracing log when debug
/// mode is enabled in the configuration.
fn log_debug_request(state: &SharedState, endpoint: &str, body: &Value) {
    if state.is_debug() {
        tracing::info!(
            "[debug] {endpoint} request body: {}",
            serde_json::to_string(body).unwrap_or_default()
        );
    }
}

/// Logs the body of an upstream response to the tracing log when debug mode is
/// enabled in the configuration.
fn log_debug_response(state: &SharedState, endpoint: &str, body: &str) {
    if state.is_debug() {
        tracing::info!("[debug] {endpoint} response body: {body}");
    }
}

/// Captures a JSON body for the dashboard store when debug mode is enabled,
/// otherwise returns `None` to avoid retaining large payloads in memory.
fn capture_json(state: &SharedState, body: &Value) -> Option<String> {
    state
        .is_debug()
        .then(|| serde_json::to_string(body).unwrap_or_default())
}

/// Captures a string body for the dashboard store when debug mode is enabled.
fn capture_str(state: &SharedState, body: &str) -> Option<String> {
    state.is_debug().then(|| body.to_string())
}

#[allow(clippy::result_large_err)]
fn parse_body(body: &Bytes) -> Result<Value, Response> {
    serde_json::from_slice::<Value>(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid JSON body: {e}")})),
        )
            .into_response()
    })
}

fn error_response(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

// ---------------------------------------------------------------------------
// Audit extraction helpers (Phase 1: Foundation for analytics)
// ---------------------------------------------------------------------------

/// Extract tool information from a request body.
fn extract_tools_from_request(body: &Value) -> (usize, Vec<String>) {
    let tools = match body.get("tools").and_then(|t| t.as_array()) {
        Some(t) => t,
        None => {
            return (0, Vec::new());
        }
    };

    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    (tools.len(), names)
}

/// Extract message count from a request body (conversation turn count).
fn extract_message_count(body: &Value) -> usize {
    body.get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Extract stop reason from SSE response body (may contain multiple events).
fn extract_stop_reason_from_sse(body: &str) -> Option<String> {
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                if let Some(sr) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    return Some(sr.to_string());
                }
            }
        }
    }
    None
}

/// Extract tool calls from SSE response (streaming events).
fn extract_tools_called_from_sse(body: &str) -> Vec<String> {
    let mut tools = Vec::new();

    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                // Check for tool_use in content_block
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                            if !tools.contains(&name.to_string()) {
                                tools.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    tools
}

/// Calculate estimated cost in USD based on token counts and model.
/// Uses simplified rates: Claude $0.003/$0.015 (input/output), GPT-4 $0.03/$0.06, etc.
fn calculate_cost(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let (input_rate, output_rate) = match model {
        m if m.contains("opus-4") => (0.015, 0.075), // claude-opus
        m if m.contains("sonnet") => (0.003, 0.015), // claude-sonnet
        m if m.contains("haiku") => (0.0008, 0.004), // claude-haiku
        m if m.contains("gpt-4") => (0.03, 0.06),    // gpt-4
        m if m.contains("gpt-4o") => (0.005, 0.015), // gpt-4o
        _ => (0.0005, 0.0015),                       // fallback
    };

    (input_tokens as f64 * input_rate + output_tokens as f64 * output_rate) / 1000.0
}

/// Checks if a request is eligible for prompt caching (system prompt is large enough).
/// Anthropic prompt caching requires at least 1024 cache-control-eligible tokens.
#[allow(dead_code)]
fn is_prompt_cache_eligible(req: &Value) -> bool {
    // Check if request has a system prompt
    if let Some(system) = req.get("system") {
        let system_size = match system {
            Value::String(s) => s.len() / 4, // Rough estimate: ~4 chars per token
            Value::Array(blocks) => blocks
                .iter()
                .map(|b| {
                    b.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.len() / 4)
                        .unwrap_or(0)
                })
                .sum(),
            _ => 0,
        };
        system_size > 1024
    } else {
        false
    }
}

/// Detect if a response used prompt caching by checking for cache tokens.
#[allow(dead_code)]
fn extract_prompt_cache_hit(response: &Value) -> Option<bool> {
    let usage = response.get("usage")?;
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    if cache_read > 0 {
        Some(true) // Cache hit (reading from cache)
    } else if cache_creation > 0 {
        Some(false) // Cache write (creating new cache)
    } else {
        None // No caching
    }
}

/// Filter tools to keep only the top N by usage frequency.
/// Reduces request size by removing rarely-used tools.
/// For initial deployment, requires 3+ tools to filter (keep all if <3).
#[allow(dead_code)]
fn filter_tools_by_frequency(tools: &Value, _frequency_threshold: f64, max_tools: usize) -> Value {
    let tools_arr = match tools.as_array() {
        Some(arr) => arr,
        None => return tools.clone(),
    };

    // Need at least 3 tools to make filtering worthwhile
    if tools_arr.len() < 3 {
        return tools.clone();
    }

    // For Phase 2, we'll keep a configured number of tools
    // In production, this would be based on actual usage frequency from audit data
    // For now: keep top 20 tools (or all if fewer)
    if tools_arr.len() <= max_tools {
        return tools.clone();
    }

    // Filter to top max_tools
    Value::Array(tools_arr.iter().take(max_tools).cloned().collect())
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

async fn get_models(State(state): State<SharedState>) -> Response {
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = state
        .ensure_models_fresh(Duration::from_secs(30 * 60))
        .await
    {
        tracing::warn!("model refresh failed: {e}");
    }
    let models = state.models.read().await;
    let data: Vec<Value> = models
        .as_ref()
        .and_then(|m| m.get("data"))
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .map(|m| {
                    let id = m.get("id").cloned().unwrap_or(Value::Null);
                    json!({
                        "id": id,
                        "object": "model",
                        "type": "model",
                        "created": 0,
                        "created_at": "1970-01-01T00:00:00.000Z",
                        "owned_by": m.get("vendor").cloned().unwrap_or(Value::String("unknown".into())),
                        "display_name": m.get("name").cloned().or_else(|| m.get("id").cloned()).unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!({"object": "list", "data": data, "has_more": false})).into_response()
}

async fn get_models_full(State(state): State<SharedState>) -> Response {
    if let Err(e) = state
        .ensure_models_fresh(Duration::from_secs(30 * 60))
        .await
    {
        tracing::warn!("model refresh failed: {e}");
    }
    let models = state.models.read().await;
    Json(models.clone().unwrap_or(Value::Null)).into_response()
}

// ---------------------------------------------------------------------------
// Chat completions
// ---------------------------------------------------------------------------

async fn chat_completions(State(state): State<SharedState>, body: Bytes) -> Response {
    let start = Instant::now();
    let mut req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let original_model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let translated = translate::translate(&state.model_mappings(), &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
    }

    // GitHub Models requests use the raw GitHub token, not the Copilot token, so
    // only ensure the Copilot token when the request routes to Copilot.
    let to_github_models = state.config_snapshot().routes_to_github_models(&translated);
    if !to_github_models {
        if let Err(e) = state.ensure_copilot_token().await {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }
    if let Err(e) = state.apply_request_gate("/v1/chat/completions").await {
        return error_response(StatusCode::TOO_MANY_REQUESTS, e);
    }

    let messages = req
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    let vision = messages.iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("image_url"))
            })
            .unwrap_or(false)
    });
    let agent = messages.iter().any(|m| {
        matches!(
            m.get("role").and_then(|r| r.as_str()),
            Some("assistant") | Some("tool")
        )
    });

    let (url, mut headers, is_github_models) = state.chat_upstream(&translated, vision).await;
    if !is_github_models {
        set_initiator(&mut headers, agent);
    }

    let req_size = body.len();
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    // GitHub Models (strict OpenAI-compatible) only emits a final usage chunk on
    // streaming requests when asked. Copilot emits it unconditionally, so only
    // opt in for GitHub Models and only when the client hasn't set its own.
    if is_github_models && is_stream && req.get("stream_options").is_none() {
        req["stream_options"] = json!({"include_usage": true});
    }
    let payload = serde_json::to_vec(&req).unwrap_or_default();
    log_debug_request(&state, "/v1/chat/completions", &req);

    if is_stream {
        return stream_openai(
            state.clone(),
            &url,
            headers,
            payload,
            "/v1/chat/completions",
            original_model,
            translated,
            req_size,
            start,
        )
        .await;
    }

    let resp = util::post_with_retry(&state, &url, headers, payload, "/v1/chat/completions").await;
    let Some(resp) = resp else {
        return error_response(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Upstream connection error after {} attempts",
                state.max_connection_retries() + 1
            ),
        );
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let resp_size = text.len();
    log_debug_response(&state, "/v1/chat/completions", &text);
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
        let input_tokens = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let (tool_count, tool_names) = extract_tools_from_request(&req);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/chat/completions".into(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &req),
            response_body: capture_str(&state, &text),
            message_count: Some(extract_message_count(&req)),
            tool_count: (tool_count > 0).then_some(tool_count),
            tool_names: (tool_count > 0).then_some(tool_names),
            stop_reason: None, // OpenAI responses don't have stop_reason in JSON
            tools_called: None,
            is_agent_initiated: Some(agent),
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
        Json(parsed).into_response()
    } else {
        log_error("/v1/chat/completions", &req, &text, status.as_u16());
        passthrough_error(status, text)
    }
}

// ---------------------------------------------------------------------------
// Responses (Codex)
// ---------------------------------------------------------------------------

async fn responses(State(state): State<SharedState>, body: Bytes) -> Response {
    let start = Instant::now();
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = state.apply_request_gate("/v1/responses").await {
        return error_response(StatusCode::TOO_MANY_REQUESTS, e);
    }
    let mut req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let original_model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let translated = translate::translate(&state.model_mappings(), &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
    }

    // /v1/responses is the Codex Responses API — Copilot-only.
    // GitHub Models models (publisher/model convention) are not supported here.
    if state
        .config_snapshot()
        .routes_to_github_models(&translated)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "Model '{original_model}' routes to GitHub Models which does not support \
                 the Responses API. Use /v1/chat/completions with '{translated}' instead."
            ),
        );
    }
    if !state
        .model_supports_endpoint(&translated, "/responses")
        .await
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": format!("Model '{original_model}' does not support the /v1/responses endpoint."),
                    "type": "invalid_request_error",
                    "code": "unsupported_model"
                }
            })),
        )
            .into_response();
    }

    // Codex adapters.
    codex::adapt_tools(&mut req);
    if let Some(input) = req.get("input").and_then(|i| i.as_array()) {
        let trimmed = codex::apply_compaction(input);
        req["input"] = Value::Array(trimmed);
    }
    req["service_tier"] = Value::Null;

    let input = req.get("input").cloned().unwrap_or(Value::Null);
    let vision = codex::has_input_image(&input);
    let agent = codex::is_agent_initiator(&input);

    let mut headers = state.copilot_headers(vision).await;
    set_initiator(&mut headers, agent);

    let req_size = body.len();
    let url = format!("{}/responses", state.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let payload = serde_json::to_vec(&req).unwrap_or_default();
    log_debug_request(&state, "/v1/responses", &req);

    if is_stream {
        return stream_responses(
            state.clone(),
            &url,
            headers,
            payload,
            req.clone(),
            original_model,
            translated,
            req_size,
            start,
        )
        .await;
    }

    let resp = util::post_with_retry(&state, &url, headers, payload, "/v1/responses").await;
    let Some(resp) = resp else {
        return error_response(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Upstream connection error after {} attempts",
                state.max_connection_retries() + 1
            ),
        );
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let resp_size = text.len();
    log_debug_response(&state, "/v1/responses", &text);
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
        let input_tokens = usage
            .get("input_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let (tool_count, tool_names) = extract_tools_from_request(&req);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/responses".into(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &req),
            response_body: capture_str(&state, &text),
            message_count: None, // /responses uses "input" not "messages"
            tool_count: (tool_count > 0).then_some(tool_count),
            tool_names: (tool_count > 0).then_some(tool_names),
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: Some(agent),
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
        Json(parsed).into_response()
    } else {
        log_error("/v1/responses", &req, &text, status.as_u16());
        passthrough_error(status, text)
    }
}

// ---------------------------------------------------------------------------
// Anthropic messages
// ---------------------------------------------------------------------------

async fn messages(State(state): State<SharedState>, body: Bytes) -> Response {
    let start = Instant::now();
    let mut req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let original_model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let translated = translate::translate(&state.model_mappings(), &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
    }

    // /v1/messages is the Anthropic Messages API used by Claude Code.
    // GitHub Models only exposes an OpenAI-compatible chat-completions surface,
    // so we never route this endpoint there — always use Copilot.
    if let Err(e) = state.ensure_copilot_token().await {
        return anthropic_error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = state.apply_request_gate("/v1/messages").await {
        return anthropic_error(StatusCode::TOO_MANY_REQUESTS, e);
    }
    let cfg = state.config_snapshot();
    req = anthropic::apply_system_prompt(&req, &cfg);
    req = anthropic::apply_tool_result_suffix(&req, &cfg);

    if state.use_direct_anthropic(&translated).await {
        messages_direct(state, req, original_model, translated, start).await
    } else {
        messages_translated(state, req, original_model, translated, start).await
    }
}

async fn messages_direct(
    state: SharedState,
    req: Value,
    original_model: String,
    translated: String,
    start: Instant,
) -> Response {
    let vision = anthropic::has_image(&req);
    let agent = req
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        })
        .unwrap_or(false);
    let mut headers = state.copilot_headers(vision).await;
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    // Mirror the official Anthropic API pattern for unlocking the 1M-token
    // context window. Copilot accepts this header harmlessly, so only send it
    // for models whose catalog actually advertises the extended window.
    if state.model_supports_1m(&translated).await {
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("context-1m-2025-08-07"),
        );
    }
    set_initiator(&mut headers, agent);

    let url = format!("{}/v1/messages", state.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut current = req.clone();
    let mut thinking_adapted = false;
    for _ in 0..4 {
        let mut sanitized = anthropic::sanitize_anthropic_request(&current);
        sanitized = anthropic::adjust_thinking_budget(&sanitized);
        let req_size = serde_json::to_vec(&current).map(|v| v.len()).unwrap_or(0);
        let payload = serde_json::to_vec(&sanitized).unwrap_or_default();
        log_debug_request(&state, "/v1/messages", &sanitized);

        if is_stream {
            let upstream = state
                .http
                .post(&url)
                .headers(headers.clone())
                .body(payload)
                .send()
                .await;
            let upstream = match upstream {
                Ok(r) => r,
                Err(e) => return anthropic_error(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
            };
            let status = upstream.status();
            // Inspect 400 responses so we can transparently recover from the
            // adaptive-thinking migration before committing to the SSE stream.
            if status == StatusCode::BAD_REQUEST {
                let text = upstream.text().await.unwrap_or_default();
                log_debug_response(&state, "/v1/messages", &text);
                log_error("/v1/messages", &current, &text, status.as_u16());
                if !thinking_adapted
                    && util::is_thinking_enabled_unsupported_error(status.as_u16(), &text)
                {
                    if let Some(adapted) = anthropic::adapt_thinking_to_adaptive(&current) {
                        tracing::info!("[Direct Anthropic] adapting thinking to adaptive format");
                        current = adapted;
                        thinking_adapted = true;
                        continue;
                    }
                }
                return passthrough_error(status, text);
            }
            return stream_anthropic_direct(
                state.clone(),
                upstream,
                original_model,
                translated,
                req_size,
                capture_json(&state, &sanitized),
                start,
            )
            .await;
        }

        let resp =
            util::post_with_retry(&state, &url, headers.clone(), payload, "/v1/messages").await;
        let Some(resp) = resp else {
            return anthropic_error(
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream connection error".into(),
            );
        };
        let status = resp.status();
        if status.is_success() {
            let parsed: Value = resp.json().await.unwrap_or(Value::Null);
            log_debug_response(
                &state,
                "/v1/messages",
                &serde_json::to_string(&parsed).unwrap_or_default(),
            );
            let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
            let input_tokens = usage
                .get("input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let (tool_count, tool_names) = extract_tools_from_request(&req);
            let tools_called: Vec<String> = parsed
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter(|block| {
                            block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        })
                        .filter_map(|block| {
                            block.get("name").and_then(|n| n.as_str()).map(String::from)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let stop_reason = parsed
                .get("stop_reason")
                .and_then(|sr| sr.as_str())
                .map(String::from);
            state.store.add(RequestRecord {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: now_iso(),
                endpoint: "/v1/messages".into(),
                model: original_model.clone(),
                translated_model: (translated != original_model).then_some(translated.clone()),
                status_code: status.as_u16(),
                request_size: req_size,
                response_size: serde_json::to_vec(&parsed).map(|v| v.len()).unwrap_or(0),
                input_tokens,
                output_tokens,
                duration: elapsed_secs(start),
                request_body: capture_json(&state, &sanitized),
                response_body: capture_json(&state, &parsed),
                message_count: Some(extract_message_count(&req)),
                tool_count: (tool_count > 0).then_some(tool_count),
                tool_names: (tool_count > 0).then_some(tool_names),
                stop_reason,
                tools_called: (!tools_called.is_empty()).then_some(tools_called),
                is_agent_initiated: Some(agent),
                prompt_cache_hit: None,
                estimated_cost_usd: Some(calculate_cost(
                    &original_model,
                    input_tokens,
                    output_tokens,
                )),
            });
            return Json(parsed).into_response();
        }
        let text = resp.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1/messages", &text);
        log_error("/v1/messages", &current, &text, status.as_u16());
        if util::is_orphaned_tool_error(status.as_u16(), &text) {
            let ids = util::extract_orphaned_ids(&text);
            if !ids.is_empty() {
                tracing::info!("[Direct Anthropic] orphaned IDs: {ids:?}");
                if let Some(msgs) = current.get("messages").and_then(|m| m.as_array()).cloned() {
                    let cleaned = util::remove_orphaned_tool_results(&msgs, &ids);
                    current["messages"] = Value::Array(cleaned);
                    continue;
                }
            }
        }
        if !thinking_adapted && util::is_thinking_enabled_unsupported_error(status.as_u16(), &text)
        {
            if let Some(adapted) = anthropic::adapt_thinking_to_adaptive(&current) {
                tracing::info!("[Direct Anthropic] adapting thinking to adaptive format");
                current = adapted;
                thinking_adapted = true;
                continue;
            }
        }
        return passthrough_error(status, text);
    }
    anthropic_error(StatusCode::BAD_GATEWAY, "Exhausted retries".into())
}

async fn messages_translated(
    state: SharedState,
    req: Value,
    original_model: String,
    translated: String,
    start: Instant,
) -> Response {
    let vision = anthropic::has_image(&req);
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut current = req.clone();
    for _ in 0..4 {
        let cfg = state.config_snapshot();
        let openai_req = anthropic::anthropic_to_openai(&current, &cfg);
        let agent = openai_req
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter().any(|m| {
                    matches!(
                        m.get("role").and_then(|r| r.as_str()),
                        Some("assistant") | Some("tool")
                    )
                })
            })
            .unwrap_or(false);
        // /v1/messages always targets Copilot; GitHub Models routing is handled
        // at the /v1/chat/completions level only.
        let url = format!("{}/chat/completions", state.copilot_base_url());
        let mut headers = state.copilot_headers(vision).await;
        let is_github_models = false;
        set_initiator(&mut headers, agent);
        let _ = is_github_models; // used below for store record
        let req_size = serde_json::to_vec(&current).map(|v| v.len()).unwrap_or(0);
        let payload = serde_json::to_vec(&openai_req).unwrap_or_default();
        log_debug_request(&state, "/v1/messages", &openai_req);

        if is_stream {
            return stream_anthropic_translated(
                state.clone(),
                &url,
                headers,
                payload,
                original_model,
                translated,
                req_size,
                start,
            )
            .await;
        }

        let resp =
            util::post_with_retry(&state, &url, headers, payload, "/v1/messages (translated)")
                .await;
        let Some(resp) = resp else {
            return anthropic_error(
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream connection error".into(),
            );
        };
        let status = resp.status();
        if status.is_success() {
            let parsed: Value = resp.json().await.unwrap_or(Value::Null);
            let anthropic_resp = anthropic::openai_to_anthropic(&parsed);
            log_debug_response(
                &state,
                "/v1/messages",
                &serde_json::to_string(&parsed).unwrap_or_default(),
            );
            let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
            let input_tokens = usage
                .get("prompt_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let output_tokens = usage
                .get("completion_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let (tool_count, tool_names) = extract_tools_from_request(&openai_req);
            state.store.add(RequestRecord {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: now_iso(),
                endpoint: "/v1/messages".into(),
                model: original_model.clone(),
                translated_model: (translated != original_model).then_some(translated.clone()),
                status_code: status.as_u16(),
                request_size: req_size,
                response_size: serde_json::to_vec(&anthropic_resp)
                    .map(|v| v.len())
                    .unwrap_or(0),
                input_tokens,
                output_tokens,
                duration: elapsed_secs(start),
                request_body: capture_json(&state, &openai_req),
                response_body: capture_json(&state, &parsed),
                message_count: Some(extract_message_count(&openai_req)),
                tool_count: (tool_count > 0).then_some(tool_count),
                tool_names: (tool_count > 0).then_some(tool_names),
                stop_reason: None,
                tools_called: None,
                is_agent_initiated: Some(agent),
                prompt_cache_hit: None,
                estimated_cost_usd: Some(calculate_cost(
                    &original_model,
                    input_tokens,
                    output_tokens,
                )),
            });
            return Json(anthropic_resp).into_response();
        }
        let text = resp.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1/messages", &text);
        log_error("/v1/messages", &current, &text, status.as_u16());
        if util::is_orphaned_tool_error(status.as_u16(), &text) {
            let ids = util::extract_orphaned_ids(&text);
            if !ids.is_empty() {
                if let Some(msgs) = current.get("messages").and_then(|m| m.as_array()).cloned() {
                    let cleaned = util::remove_orphaned_tool_results(&msgs, &ids);
                    current["messages"] = Value::Array(cleaned);
                    continue;
                }
            }
        }
        return passthrough_error(status, text);
    }
    anthropic_error(StatusCode::BAD_GATEWAY, "Exhausted retries".into())
}

async fn count_tokens(State(state): State<SharedState>, body: Bytes) -> Response {
    if state.ensure_copilot_token().await.is_err() {
        return Json(json!({"input_tokens": 1})).into_response();
    }
    let mut req = match parse_body(&body) {
        Ok(v) => v,
        Err(_) => return Json(json!({"input_tokens": 1})).into_response(),
    };
    let original_model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let translated = translate::translate(&state.model_mappings(), &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
    }

    let _ = state
        .ensure_models_fresh(Duration::from_secs(30 * 60))
        .await;

    // Prefer real token counting from upstream responses whenever the model
    // supports the Anthropic native count-tokens endpoint.
    if state
        .model_supports_endpoint(&translated, "/v1/messages/count_tokens")
        .await
    {
        let vision = anthropic::has_image(&req);
        let mut headers = state.copilot_headers(vision).await;
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        if state.model_supports_1m(&translated).await {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("context-1m-2025-08-07"),
            );
        }
        let url = format!("{}/v1/messages/count_tokens", state.copilot_base_url());
        let payload = serde_json::to_vec(&req).unwrap_or_default();
        if let Some(resp) =
            util::post_with_retry(&state, &url, headers, payload, "/v1/messages/count_tokens").await
        {
            if resp.status().is_success() {
                let parsed: Value = resp.json().await.unwrap_or(json!({"input_tokens": 1}));
                return Json(parsed).into_response();
            }
        }
    }

    error_response(
        StatusCode::BAD_REQUEST,
        format!(
            "Real token counting is unavailable for model '{original_model}'. The upstream endpoint /v1/messages/count_tokens is not supported or failed."
        ),
    )
}

// ---------------------------------------------------------------------------
// Gemini (translated through OpenAI chat completions)
// ---------------------------------------------------------------------------

/// Splits a Gemini path segment like `gemini-2.5-pro:generateContent` into the
/// `(model, action)` pair. A missing action defaults to `generateContent`.
fn split_model_action(seg: &str) -> (String, String) {
    match seg.rsplit_once(':') {
        Some((model, action)) => (model.to_string(), action.to_string()),
        None => (seg.to_string(), "generateContent".to_string()),
    }
}

fn gemini_error(status: StatusCode, msg: String) -> Response {
    (
        status,
        Json(json!({"error": {"code": status.as_u16(), "message": msg, "status": "ERROR"}})),
    )
        .into_response()
}

/// Handles the Gemini `generateContent`, `streamGenerateContent`, and
/// `countTokens` actions by translating to/from the OpenAI chat completions API.
async fn gemini_generate(
    State(state): State<SharedState>,
    Path(model_action): Path<String>,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let (raw_model, action) = split_model_action(&model_action);

    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };

    let translated = translate::translate(&state.model_mappings(), &raw_model);

    // countTokens: translate and defer to the chat-completions-based estimate.
    // This is a local estimate and needs no upstream token.
    if action == "countTokens" || action == "counttokens" {
        let openai_req = gemini::gemini_to_openai(&req, &translated, false);
        let text = collect_text_for_count(&openai_req);
        let tokenizer = state.model_tokenizer(&translated).await;
        let total = crate::filters::count_tokens(&text, &tokenizer);
        return Json(json!({"totalTokens": total})).into_response();
    }

    let is_stream = action == "streamGenerateContent" || action == "streamgeneratecontent";

    // GitHub Models uses the raw GitHub token; only ensure the Copilot token
    // when the request routes to Copilot.
    let to_github_models = state.config_snapshot().routes_to_github_models(&translated);
    if !to_github_models {
        if let Err(e) = state.ensure_copilot_token().await {
            return gemini_error(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }
    if let Err(e) = state.apply_request_gate("/v1beta/models").await {
        return gemini_error(StatusCode::TOO_MANY_REQUESTS, e);
    }

    let openai_req = gemini::gemini_to_openai(&req, &translated, is_stream);
    let vision = gemini::has_image(&req);
    let agent = gemini::is_agent(&req);
    let (url, mut headers, is_github_models) = state.chat_upstream(&translated, vision).await;
    if !is_github_models {
        set_initiator(&mut headers, agent);
    }

    let req_size = body.len();
    let payload = serde_json::to_vec(&openai_req).unwrap_or_default();
    log_debug_request(&state, "/v1beta/models", &openai_req);

    if is_stream {
        return stream_gemini(
            state.clone(),
            &url,
            headers,
            payload,
            raw_model,
            translated,
            req_size,
            start,
        )
        .await;
    }

    let resp = util::post_with_retry(&state, &url, headers, payload, "/v1beta/models").await;
    let Some(resp) = resp else {
        return gemini_error(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Upstream connection error after {} attempts",
                state.max_connection_retries() + 1
            ),
        );
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let resp_size = text.len();
    log_debug_response(&state, "/v1beta/models", &text);
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let gemini_resp = gemini::openai_to_gemini(&parsed);
        let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
        let input_tokens = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1beta/models".into(),
            model: raw_model.clone(),
            translated_model: (translated != raw_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &openai_req),
            response_body: capture_str(&state, &text),
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: Some(agent),
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&raw_model, input_tokens, output_tokens)),
        });
        Json(gemini_resp).into_response()
    } else {
        log_error("/v1beta/models", &openai_req, &text, status.as_u16());
        gemini_error(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        )
    }
}

/// Concatenates all text payloads in an OpenAI request for token estimation.
fn collect_text_for_count(openai_req: &Value) -> String {
    let mut out = String::new();
    if let Some(messages) = openai_req.get("messages").and_then(|m| m.as_array()) {
        for m in messages {
            match m.get("content") {
                Some(Value::String(s)) => {
                    out.push_str(s);
                    out.push('\n');
                }
                Some(Value::Array(parts)) => {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                            out.push('\n');
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Streams an OpenAI chat-completions SSE stream translated into Gemini
/// `streamGenerateContent` SSE events (`data: {json}` lines).
#[allow(clippy::too_many_arguments)]
async fn stream_gemini(
    state: SharedState,
    url: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
    let req_body = state
        .is_debug()
        .then(|| String::from_utf8_lossy(&payload).into_owned());
    let upstream = state
        .http
        .post(url)
        .headers(headers)
        .body(payload)
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => return gemini_error(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
    };
    let status = upstream.status().as_u16();
    // Surface a non-2xx upstream (JSON error, not SSE) as a normal error.
    if !(200..300).contains(&status) {
        let text = upstream.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1beta/models", &text);
        log_error(
            "/v1beta/models",
            &json!({"model": &translated}),
            &text,
            status,
        );
        return gemini_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        );
    }
    let model_json = Value::String(translated.clone());
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut buf = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        let mut finish: Option<String> = None;
        let mut debug_resp = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            let chunk_str = String::from_utf8_lossy(&chunk);
            if state.is_debug() { debug_resp.push_str(&chunk_str); }
            buf.push_str(&chunk_str);
            let mut lines: Vec<&str> = buf.split('\n').collect();
            let remainder = lines.pop().unwrap_or("").to_string();
            for line in lines {
                let line = line.trim_end_matches('\r');
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if data == "[DONE]" { continue; }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(u) = v.get("usage") {
                        if !u.is_null() {
                            input_tokens = u.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(input_tokens);
                            output_tokens = u.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(output_tokens);
                        }
                    }
                    if let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) {
                        if let Some(text) = choice.get("delta").and_then(|d| d.get("content")).and_then(|c| c.as_str()) {
                            if !text.is_empty() {
                                let ev = gemini::gemini_stream_text_chunk(text, &model_json);
                                let payload = format!("data: {}\n\n", serde_json::to_string(&ev).unwrap_or_default());
                                resp_size += payload.len();
                                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(payload));
                            }
                        }
                        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                            finish = Some(fr.to_string());
                        }
                    }
                }
            }
            buf = remainder;
        }
        // Final chunk with finish reason + usage.
        let usage = json!({"prompt_tokens": input_tokens, "completion_tokens": output_tokens});
        let ev = gemini::gemini_stream_final_chunk(finish.as_deref(), &usage, &model_json);
        let payload = format!("data: {}\n\n", serde_json::to_string(&ev).unwrap_or_default());
        resp_size += payload.len();
        yield Ok(Bytes::from(payload));
        log_debug_response(&state, "/v1beta/models", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1beta/models".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: status,
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: None,
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
    };
    build_sse_response(stream)
}

// ---------------------------------------------------------------------------
// Embeddings
// ---------------------------------------------------------------------------

async fn embeddings(State(state): State<SharedState>, body: Bytes) -> Response {
    let start = Instant::now();
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = state.apply_request_gate("/v1/embeddings").await {
        return error_response(StatusCode::TOO_MANY_REQUESTS, e);
    }
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let req_size = body.len();
    let headers = state.copilot_headers(false).await;
    let url = format!("{}/embeddings", state.copilot_base_url());
    let payload = serde_json::to_vec(&req).unwrap_or_default();
    log_debug_request(&state, "/v1/embeddings", &req);

    let resp = util::post_with_retry(&state, &url, headers, payload, "/v1/embeddings").await;
    let Some(resp) = resp else {
        return error_response(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Upstream connection error after {} attempts",
                state.max_connection_retries() + 1
            ),
        );
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let resp_size = text.len();
    log_debug_response(&state, "/v1/embeddings", &text);
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
        let input_tokens = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/embeddings".into(),
            model: model.clone(),
            translated_model: None,
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens: 0,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &req),
            response_body: capture_str(&state, &text),
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: Some(false),
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&model, input_tokens, 0)),
        });
        Json(parsed).into_response()
    } else {
        log_error("/v1/embeddings", &req, &text, status.as_u16());
        passthrough_error(status, text)
    }
}

// ---------------------------------------------------------------------------
// Usage / quota
// ---------------------------------------------------------------------------

async fn usage(State(state): State<SharedState>) -> Response {
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    match state.fetch_usage().await {
        Ok(v) => Json(crate::state::summarize_usage(&v)).into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Streaming helpers
// ---------------------------------------------------------------------------

fn passthrough_error(status: StatusCode, text: String) -> Response {
    let mut resp = (status, text).into_response();
    resp.headers_mut()
        .insert("Content-Type", HeaderValue::from_static("application/json"));
    resp
}

fn anthropic_error(status: StatusCode, msg: String) -> Response {
    (
        status,
        Json(json!({"type": "error", "error": {"type": "api_error", "message": msg}})),
    )
        .into_response()
}

/// Streams an OpenAI chat-completions SSE response back to the client,
/// re-emitting `data:` lines while accumulating token usage for analytics.
#[allow(clippy::too_many_arguments)]
async fn stream_openai(
    state: SharedState,
    url: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
    endpoint: &'static str,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
    let req_body = state
        .is_debug()
        .then(|| String::from_utf8_lossy(&payload).into_owned());
    let upstream = state
        .http
        .post(url)
        .headers(headers)
        .body(payload)
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
    };
    let status = upstream.status().as_u16();
    // A non-2xx upstream (e.g. GitHub Models returning 401/403 as JSON when the
    // token lacks the `models` scope) is not an SSE stream — surface it as a
    // normal error instead of forwarding a broken "stream".
    if !(200..300).contains(&status) {
        let text = upstream.text().await.unwrap_or_default();
        log_debug_response(&state, endpoint, &text);
        log_error(endpoint, &json!({"model": &translated}), &text, status);
        return passthrough_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        );
    }
    let model = translated.clone();
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut buf = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        let mut debug_resp = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            let chunk_str = String::from_utf8_lossy(&chunk);
            if state.is_debug() { debug_resp.push_str(&chunk_str); }
            buf.push_str(&chunk_str);
            let mut lines: Vec<&str> = buf.split('\n').collect();
            let remainder = lines.pop().unwrap_or("").to_string();
            for line in lines {
                let line = line.trim_end_matches('\r');
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if data == "[DONE]" {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(u) = v.get("usage") {
                        input_tokens = u.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(input_tokens);
                        output_tokens = u.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(output_tokens);
                    }
                    resp_size += data.len();
                    yield Ok(Bytes::from(format!("data: {data}\n\n")));
                }
            }
            buf = remainder;
        }
        let _ = model;
        log_debug_response(&state, endpoint, &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: endpoint.to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: status,
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None, // Streaming doesn't have access to parsed req
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: None,
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
    };
    build_sse_response(stream)
}

/// Streams an OpenAI Responses SSE stream back to the client verbatim while
/// extracting usage from the `response.completed` event.
#[allow(clippy::too_many_arguments)]
async fn stream_responses(
    state: SharedState,
    url: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
    req: Value,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
    let req_body = capture_json(&state, &req);
    let upstream = state
        .http
        .post(url)
        .headers(headers)
        .body(payload)
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
    };
    let status = upstream.status().as_u16();
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut buf = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        let mut debug_resp = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            resp_size += chunk.len();
            // Verbatim passthrough of raw bytes.
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::copy_from_slice(&chunk));
            let chunk_str = String::from_utf8_lossy(&chunk);
            if state.is_debug() { debug_resp.push_str(&chunk_str); }
            buf.push_str(&chunk_str);
            let mut lines: Vec<&str> = buf.split('\n').collect();
            let remainder = lines.pop().unwrap_or("").to_string();
            for line in lines {
                let line = line.trim_end_matches('\r');
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if data == "[DONE]" { continue; }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("response.completed") {
                        let usage = v.get("response").and_then(|r| r.get("usage")).cloned().unwrap_or(json!({}));
                        input_tokens = usage.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        output_tokens = usage.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    }
                }
            }
            buf = remainder;
        }
        log_debug_response(&state, "/v1/responses", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/responses".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: status,
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: None,
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
    };
    build_sse_response(stream)
}

/// Streams a direct Anthropic SSE response back to the client verbatim.
async fn stream_anthropic_direct(
    state: SharedState,
    upstream: reqwest::Response,
    original_model: String,
    translated: String,
    req_size: usize,
    req_body: Option<String>,
    start: Instant,
) -> Response {
    let status = upstream.status().as_u16();
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        let mut buf = String::new();
        let mut debug_resp = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            resp_size += chunk.len();
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::copy_from_slice(&chunk));
            let chunk_str = String::from_utf8_lossy(&chunk);
            if state.is_debug() { debug_resp.push_str(&chunk_str); }
            buf.push_str(&chunk_str);
            let mut lines: Vec<&str> = buf.split('\n').collect();
            let remainder = lines.pop().unwrap_or("").to_string();
            for line in lines {
                let line = line.trim_end_matches('\r');
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("message_start") => {
                            input_tokens = v.get("message").and_then(|m| m.get("usage")).and_then(|u| u.get("input_tokens")).and_then(|t| t.as_u64()).unwrap_or(0);
                        }
                        Some("message_delta") => {
                            output_tokens = v.get("usage").and_then(|u| u.get("output_tokens")).and_then(|t| t.as_u64()).unwrap_or(output_tokens);
                        }
                        _ => {}
                    }
                }
            }
            buf = remainder;
        }
        log_debug_response(&state, "/v1/messages", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/messages".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: status,
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp.clone()) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: extract_stop_reason_from_sse(&debug_resp),
            tools_called: (!extract_tools_called_from_sse(&debug_resp).is_empty()).then(|| extract_tools_called_from_sse(&debug_resp)),
            is_agent_initiated: None,
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
    };
    build_sse_response(stream)
}

/// Streams an OpenAI chat-completions SSE stream translated into Anthropic
/// Messages SSE events.
#[allow(clippy::too_many_arguments)]
async fn stream_anthropic_translated(
    state: SharedState,
    url: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
    let req_body = state
        .is_debug()
        .then(|| String::from_utf8_lossy(&payload).into_owned());
    let upstream = state
        .http
        .post(url)
        .headers(headers)
        .body(payload)
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => return anthropic_error(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
    };
    let status = upstream.status().as_u16();
    // Surface a non-2xx upstream (JSON error, not SSE) as a normal error.
    if !(200..300).contains(&status) {
        let text = upstream.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1/messages", &text);
        log_error(
            "/v1/messages",
            &json!({"model": &translated}),
            &text,
            status,
        );
        return passthrough_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        );
    }
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut buf = String::new();
        let mut conv = AnthropicStreamState::new();
        let mut chunks: Vec<Value> = Vec::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        let mut debug_resp = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            let chunk_str = String::from_utf8_lossy(&chunk);
            if state.is_debug() { debug_resp.push_str(&chunk_str); }
            buf.push_str(&chunk_str);
            let mut lines: Vec<&str> = buf.split('\n').collect();
            let remainder = lines.pop().unwrap_or("").to_string();
            for line in lines {
                let line = line.trim_end_matches('\r');
                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if data == "[DONE]" { continue; }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(u) = v.get("usage") {
                        input_tokens = u.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(input_tokens);
                        output_tokens = u.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(output_tokens);
                    }
                    chunks.push(v.clone());
                    for event in conv.process(&v) {
                        let ev_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("message");
                        let payload = format!("event: {ev_type}\ndata: {}\n\n", serde_json::to_string(&event).unwrap_or_default());
                        resp_size += payload.len();
                        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(payload));
                    }
                }
            }
            buf = remainder;
        }
        // Fall back to merged usage if streaming chunks did not carry usage.
        if input_tokens == 0 && output_tokens == 0 {
            let merged = anthropic::merge_chat_chunks(&chunks);
            if let Some(usage) = merged.get("usage") {
                input_tokens = usage.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                output_tokens = usage.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
            }
        }
        log_debug_response(&state, "/v1/messages", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/messages".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: status,
            request_size: req_size,
            response_size: resp_size,
            input_tokens,
            output_tokens,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: None,
            prompt_cache_hit: None,
            estimated_cost_usd: Some(calculate_cost(&original_model, input_tokens, output_tokens)),
        });
    };
    build_sse_response(stream)
}

fn build_sse_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static,
{
    let body = Body::from_stream(stream);
    let mut resp = Response::new(body);
    *resp.headers_mut() = sse_headers();
    resp
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async fn dashboard() -> Response {
    serve_asset("dashboard.html", include_str!("../public/dashboard.html"))
}

/// Serves a machine-readable OpenAPI v3 specification describing the proxy's
/// LLM endpoints (OpenAI, Anthropic, and Gemini surfaces). Mirrors the
/// discovery endpoint exposed by agent-maestro so the same tooling works here.
async fn openapi_spec() -> Response {
    let spec = json!({
        "openapi": "3.0.3",
        "info": {
            "title": "ghc-proxy",
            "description": "GitHub Copilot API proxy exposing OpenAI-, Anthropic-, and Gemini-compatible endpoints.",
            "version": env!("CARGO_PKG_VERSION")
        },
        "servers": [{ "url": "/" }],
        "paths": {
            "/v1/chat/completions": {
                "post": {
                    "summary": "OpenAI Chat Completions",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Chat completion (JSON or SSE when stream=true)" } }
                }
            },
            "/v1/responses": {
                "post": {
                    "summary": "OpenAI Responses API (Codex)",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Response (JSON or SSE when stream=true)" } }
                }
            },
            "/v1/messages": {
                "post": {
                    "summary": "Anthropic Messages API",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Anthropic message (JSON or SSE when stream=true)" } }
                }
            },
            "/v1/messages/count_tokens": {
                "post": {
                    "summary": "Anthropic token counting",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Token count" } }
                }
            },
            "/v1/embeddings": {
                "post": {
                    "summary": "OpenAI Embeddings",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Embedding vectors" } }
                }
            },
            "/v1/models": {
                "get": {
                    "summary": "List available models",
                    "responses": { "200": { "description": "Model list" } }
                }
            },
            "/v1beta/models/{model}:generateContent": {
                "post": {
                    "summary": "Gemini generateContent",
                    "parameters": [{ "name": "model", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Gemini candidate response" } }
                }
            },
            "/v1beta/models/{model}:streamGenerateContent": {
                "post": {
                    "summary": "Gemini streamGenerateContent (SSE)",
                    "parameters": [{ "name": "model", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Gemini streaming response" } }
                }
            },
            "/v1beta/models/{model}:countTokens": {
                "post": {
                    "summary": "Gemini countTokens",
                    "parameters": [{ "name": "model", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                    "responses": { "200": { "description": "Token count" } }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "ApiKeyAuth": { "type": "apiKey", "in": "header", "name": "x-api-key" },
                "BearerAuth": { "type": "http", "scheme": "bearer" }
            }
        }
    });
    Json(spec).into_response()
}

async fn requests_page() -> Response {
    serve_asset("requests.html", include_str!("../public/requests.html"))
}

async fn metrics_page() -> Response {
    serve_asset("metrics.html", include_str!("../public/metrics.html"))
}

fn serve_asset(_name: &str, contents: &'static str) -> Response {
    let mut resp = Response::new(Body::from(contents));
    resp.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

fn metrics_label_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn metrics_openmetrics(State(state): State<SharedState>) -> Response {
    let stats = state.store.stats();
    let (records, _) = state.store.recent(usize::MAX, 0);

    let mut req_total_by_labels: HashMap<(String, u16, String), u64> = HashMap::new();
    let mut in_tokens_by_model: HashMap<String, u64> = HashMap::new();
    let mut out_tokens_by_model: HashMap<String, u64> = HashMap::new();
    let mut duration_sum_by_endpoint: HashMap<String, f64> = HashMap::new();
    let mut duration_count_by_endpoint: HashMap<String, u64> = HashMap::new();
    let mut cost_total = 0.0_f64;

    for rec in &records {
        let model = rec
            .translated_model
            .as_ref()
            .unwrap_or(&rec.model)
            .to_string();
        *req_total_by_labels
            .entry((rec.endpoint.clone(), rec.status_code, model.clone()))
            .or_insert(0) += 1;
        *in_tokens_by_model.entry(model.clone()).or_insert(0) += rec.input_tokens;
        *out_tokens_by_model.entry(model).or_insert(0) += rec.output_tokens;
        *duration_sum_by_endpoint
            .entry(rec.endpoint.clone())
            .or_insert(0.0) += rec.duration;
        *duration_count_by_endpoint
            .entry(rec.endpoint.clone())
            .or_insert(0) += 1;
        cost_total += rec.estimated_cost_usd.unwrap_or(0.0);
    }

    let mut out = String::new();
    out.push_str(
        "# HELP ghc_proxy_requests_total Total proxied requests by endpoint/status/model.\n",
    );
    out.push_str("# TYPE ghc_proxy_requests_total counter\n");
    for ((endpoint, status, model), count) in req_total_by_labels {
        out.push_str(&format!(
            "ghc_proxy_requests_total{{endpoint=\"{}\",status=\"{}\",model=\"{}\"}} {}\n",
            metrics_label_escape(&endpoint),
            status,
            metrics_label_escape(&model),
            count
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_input_tokens_total Total input tokens from real upstream usage.\n",
    );
    out.push_str("# TYPE ghc_proxy_input_tokens_total counter\n");
    for (model, total) in in_tokens_by_model {
        out.push_str(&format!(
            "ghc_proxy_input_tokens_total{{model=\"{}\"}} {}\n",
            metrics_label_escape(&model),
            total
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_output_tokens_total Total output tokens from real upstream usage.\n",
    );
    out.push_str("# TYPE ghc_proxy_output_tokens_total counter\n");
    for (model, total) in out_tokens_by_model {
        out.push_str(&format!(
            "ghc_proxy_output_tokens_total{{model=\"{}\"}} {}\n",
            metrics_label_escape(&model),
            total
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_request_duration_seconds_sum Sum of request durations by endpoint.\n",
    );
    out.push_str("# TYPE ghc_proxy_request_duration_seconds_sum counter\n");
    for (endpoint, sum) in &duration_sum_by_endpoint {
        out.push_str(&format!(
            "ghc_proxy_request_duration_seconds_sum{{endpoint=\"{}\"}} {:.6}\n",
            metrics_label_escape(endpoint),
            sum
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_request_duration_seconds_count Count of requests by endpoint.\n",
    );
    out.push_str("# TYPE ghc_proxy_request_duration_seconds_count counter\n");
    for (endpoint, count) in duration_count_by_endpoint {
        out.push_str(&format!(
            "ghc_proxy_request_duration_seconds_count{{endpoint=\"{}\"}} {}\n",
            metrics_label_escape(&endpoint),
            count
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_store_records Number of request records currently retained in memory.\n",
    );
    out.push_str("# TYPE ghc_proxy_store_records gauge\n");
    out.push_str(&format!("ghc_proxy_store_records {}\n", records.len()));

    out.push_str(
        "# HELP ghc_proxy_estimated_cost_usd_total Total estimated request cost in USD.\n",
    );
    out.push_str("# TYPE ghc_proxy_estimated_cost_usd_total counter\n");
    out.push_str(&format!(
        "ghc_proxy_estimated_cost_usd_total {:.8}\n",
        cost_total
    ));

    out.push_str(
        "# HELP ghc_proxy_stats_request_count Total request count from aggregate store stats.\n",
    );
    out.push_str("# TYPE ghc_proxy_stats_request_count counter\n");
    out.push_str(&format!(
        "ghc_proxy_stats_request_count {}\n",
        stats.request_count
    ));

    out.push_str("# EOF\n");

    let mut resp = Response::new(Body::from(out));
    resp.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
    );
    resp
}

async fn api_reload_config(State(state): State<SharedState>) -> Response {
    let cfg = state.reload_config();
    Json(json!({
        "ok": true,
        "config_path": state.config_path(),
        "config": {
            "address": cfg.address,
            "port": cfg.port,
            "debug": cfg.debug,
            "account_type": cfg.account_type,
            "max_connection_retries": cfg.max_connection_retries,
            "redirect_anthropic": cfg.redirect_anthropic,
            "rate_limit_seconds": cfg.rate_limit_seconds,
            "rate_limit_wait": cfg.rate_limit_wait,
            "manual_approve": cfg.manual_approve
        }
    }))
    .into_response()
}

async fn api_stats(State(state): State<SharedState>) -> Response {
    Json(state.store.stats()).into_response()
}

async fn api_requests(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page: usize = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let per_page: usize = params
        .get("per_page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(50);
    let offset = (page - 1) * per_page;
    let (items, total) = state.store.recent(per_page, offset);
    let total_pages = if per_page > 0 {
        total.div_ceil(per_page)
    } else {
        0
    };
    Json(json!({
        "items": items,
        "total": total,
        "page": page,
        "per_page": per_page,
        "total_pages": total_pages,
    }))
    .into_response()
}

/// Audit API: Returns filtered request records with audit fields.
/// Query params: endpoint=, status=, tool_name=, agent=true|false, page=, per_page=
async fn api_audit(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page: usize = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let per_page: usize = params
        .get("per_page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(50);
    let endpoint_filter = params.get("endpoint").map(|s| s.as_str());
    let status_filter = params.get("status").and_then(|s| s.parse::<u16>().ok());
    let tool_filter = params.get("tool_name").map(|s| s.as_str());
    let agent_filter = params.get("agent").and_then(|s| s.parse::<bool>().ok());

    let (records, _total) = state.store.recent(usize::MAX, 0);

    // Apply filters
    let filtered: Vec<RequestRecord> = records
        .into_iter()
        .filter(|rec| {
            // Endpoint filter
            if let Some(ep) = endpoint_filter {
                if !rec.endpoint.contains(ep) {
                    return false;
                }
            }
            // Status code filter
            if let Some(st) = status_filter {
                if rec.status_code != st {
                    return false;
                }
            }
            // Tool name filter
            if let Some(tool) = tool_filter {
                if let Some(ref tools) = rec.tool_names {
                    if !tools.iter().any(|t| t.contains(tool)) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            // Agent filter
            if let Some(is_agent) = agent_filter {
                if rec.is_agent_initiated != Some(is_agent) {
                    return false;
                }
            }
            true
        })
        .collect();

    let filtered_total = filtered.len();
    let offset = (page - 1) * per_page;
    let items = filtered
        .into_iter()
        .skip(offset)
        .take(per_page)
        .collect::<Vec<_>>();
    let total_pages = if per_page > 0 {
        filtered_total.div_ceil(per_page)
    } else {
        0
    };

    Json(json!({
        "items": items,
        "total": filtered_total,
        "page": page,
        "per_page": per_page,
        "total_pages": total_pages,
    }))
    .into_response()
}

/// Audit summary API: Returns aggregated statistics about tools, stop reasons, and costs.
async fn api_audit_summary(State(state): State<SharedState>) -> Response {
    let (records, _) = state.store.recent(usize::MAX, 0);

    let mut tool_usage: HashMap<String, usize> = HashMap::new();
    let mut stop_reason_counts: HashMap<String, usize> = HashMap::new();
    let mut total_cost = 0.0;
    let mut agent_count = 0usize;
    let mut cache_hit_count = 0usize;
    let mut cache_write_count = 0usize;

    for rec in &records {
        // Tool usage aggregation
        if let Some(ref tools) = rec.tool_names {
            for tool in tools {
                *tool_usage.entry(tool.clone()).or_insert(0) += 1;
            }
        }

        // Stop reason aggregation
        if let Some(ref sr) = rec.stop_reason {
            *stop_reason_counts.entry(sr.clone()).or_insert(0) += 1;
        }

        // Cost aggregation
        if let Some(cost) = rec.estimated_cost_usd {
            total_cost += cost;
        }

        // Agent tracking
        if rec.is_agent_initiated == Some(true) {
            agent_count += 1;
        }

        // Cache tracking
        if rec.prompt_cache_hit == Some(true) {
            cache_hit_count += 1;
        } else if rec.prompt_cache_hit == Some(false) {
            cache_write_count += 1;
        }
    }

    // Sort tools by usage
    let mut tools_sorted: Vec<_> = tool_usage.into_iter().collect();
    tools_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let top_tools: Vec<_> = tools_sorted.into_iter().take(20).collect();

    // Sort stop reasons by count
    let mut stop_reasons_sorted: Vec<_> = stop_reason_counts.into_iter().collect();
    stop_reasons_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));

    Json(json!({
        "total_requests": records.len(),
        "agent_initiated": agent_count,
        "total_cost_usd": (total_cost * 100.0).round() / 100.0,
        "avg_cost_usd": if !records.is_empty() { (total_cost / records.len() as f64 * 100.0).round() / 100.0 } else { 0.0 },
        "top_tools": top_tools,
        "stop_reasons": stop_reasons_sorted,
        "cache_hits": cache_hit_count,
        "cache_writes": cache_write_count,
        "cache_hit_rate": if cache_hit_count + cache_write_count > 0 {
            (cache_hit_count as f64 / (cache_hit_count + cache_write_count) as f64 * 100.0).round() / 100.0
        } else {
            0.0
        },
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrey"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn protected_paths_cover_llm_endpoints() {
        assert!(is_protected_path("/v1/chat/completions"));
        assert!(is_protected_path("/v1/messages"));
        assert!(is_protected_path("/v1/responses"));
        assert!(is_protected_path("/chat/completions"));
        assert!(is_protected_path(
            "/v1beta/models/gemini-2.5-pro:generateContent"
        ));
        assert!(!is_protected_path("/"));
        assert!(!is_protected_path("/metrics"));
        assert!(!is_protected_path("/api/stats"));
        assert!(!is_protected_path("/requests"));
    }

    #[test]
    fn presented_key_from_bearer() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer abc123"));
        assert_eq!(presented_api_key(&h).as_deref(), Some("abc123"));
    }

    #[test]
    fn presented_key_from_x_api_key() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("k-456"));
        assert_eq!(presented_api_key(&h).as_deref(), Some("k-456"));
    }

    #[test]
    fn presented_key_from_goog_header() {
        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", HeaderValue::from_static("g-789"));
        assert_eq!(presented_api_key(&h).as_deref(), Some("g-789"));
    }

    #[test]
    fn presented_key_absent() {
        let h = HeaderMap::new();
        assert_eq!(presented_api_key(&h), None);
    }
}
