//! HTTP server: route definitions and request handlers for the OpenAI- and
//! Anthropic-compatible proxy endpoints, plus the analytics dashboard API.

use crate::anthropic::{self, AnthropicStreamState};
use crate::gemini;
use crate::responses as codex;
use crate::state::SharedState;
use crate::store::RequestRecord;
use crate::translate;
use crate::util;
use crate::util::TokenUsage;
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path, Query, Request, State,
    },
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
        .route("/v1/models/{model_id}", get(get_model))
        .route("/models/{model_id}", get(get_model))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses).get(ws_responses))
        .route("/responses", post(responses).get(ws_responses))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/embeddings", post(embeddings))
        .route("/embeddings", post(embeddings))
        .route("/usage", get(usage))
        .route("/health", get(health))
        .route("/metrics", get(metrics_openmetrics))
        .route("/", get(dashboard))
        .route("/requests", get(requests_page))
        .route("/metrics/dashboard", get(metrics_page))
        .route("/app.css", get(stylesheet))
        .route("/api/stats", get(api_stats))
        .route("/api/cache", get(api_cache))
        .route("/api/requests", get(api_requests))
        .route("/api/audit", get(api_audit))
        .route("/api/audit/summary", get(api_audit_summary))
        .route("/api/config/reload", post(api_reload_config))
        .route("/api/config/debug", post(api_set_debug))
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
/// optional API key. The dashboard UI, static assets, and read-only metrics
/// endpoints are intentionally left open so local monitoring keeps working
/// without a key.
///
/// `/api/config/` is guarded despite being part of the dashboard: those routes
/// mutate the running process, and one of them turns on body capture, which
/// writes whatever the client sent — credentials included — into the request
/// log. That is not something an unauthenticated caller should be able to do.
fn is_protected_path(path: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "/v1/",
        "/chat/completions",
        "/responses",
        "/embeddings",
        "/models",
        "/v1beta/",
        "/api/config/",
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

/// Maximum size of `error.log` before it is rotated to `error.log.1`.
const ERROR_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Logs an upstream error to `error.log` in the config directory. The file is
/// rotated once it exceeds `ERROR_LOG_MAX_BYTES` so a persistently failing
/// upstream cannot fill the disk.
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
    if std::fs::metadata(&path)
        .map(|m| m.len() > ERROR_LOG_MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, dir.join("error.log.1"));
    }
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

/// Renames `max_tokens` to `max_completion_tokens`, the parameter newer
/// OpenAI-family models require. Returns false when there is nothing to rename,
/// so callers can avoid a pointless retry.
fn rewrite_max_tokens_param(req: &mut Value) -> bool {
    let Some(obj) = req.as_object_mut() else {
        return false;
    };
    if obj.contains_key("max_completion_tokens") {
        // Already migrated; just drop the rejected alias.
        return obj.remove("max_tokens").is_some();
    }
    match obj.remove("max_tokens") {
        Some(v) => {
            obj.insert("max_completion_tokens".to_string(), v);
            true
        }
        None => false,
    }
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

/// Per-1K-token (input, output) rates used for cost estimation.
///
/// Arms are evaluated in order, so more specific model families must come
/// before the prefixes that would otherwise swallow them (`gpt-4o` before
/// `gpt-4`, `opus`/`sonnet`/`haiku` before the generic `claude` fallback).
fn model_rates(model: &str) -> (f64, f64) {
    let m = model.to_ascii_lowercase();
    // Strip a `publisher/` prefix so GitHub Models ids price like their base
    // model (e.g. `openai/gpt-4o` -> `gpt-4o`).
    let m = m.rsplit('/').next().unwrap_or(&m);
    match m {
        m if m.contains("opus") => (0.015, 0.075),
        m if m.contains("sonnet") => (0.003, 0.015),
        m if m.contains("haiku") => (0.0008, 0.004),
        m if m.contains("gpt-4o-mini") => (0.00015, 0.0006),
        m if m.contains("gpt-4o") => (0.005, 0.015),
        m if m.contains("gpt-4.1-mini") => (0.0004, 0.0016),
        m if m.contains("gpt-4.1") => (0.002, 0.008),
        m if m.contains("gpt-4") => (0.03, 0.06),
        m if m.contains("gpt-5-mini") || m.contains("gpt-5.5-mini") => (0.00025, 0.002),
        m if m.contains("gpt-5") => (0.00125, 0.01),
        m if m.contains("o3-mini") || m.contains("o4-mini") => (0.0011, 0.0044),
        m if m.contains("gemini") && m.contains("flash") => (0.0003, 0.0025),
        m if m.contains("gemini") => (0.00125, 0.01),
        m if m.contains("embedding") => (0.00002, 0.0),
        _ => (0.0005, 0.0015),
    }
}

/// Cache writes carry a premium over fresh input, and cache reads a steep
/// discount. These are Anthropic's published multipliers for 5-minute
/// ephemeral caching, which is the tier Copilot passes through.
const CACHE_WRITE_RATE_MULTIPLIER: f64 = 1.25;
const CACHE_READ_RATE_MULTIPLIER: f64 = 0.1;

/// Calculate estimated cost in USD based on token counts and model.
/// Rates are approximate public list prices per 1K tokens and are only used for
/// the dashboard/metrics estimate, never for billing.
///
/// `usage.input_tokens` is the true total prompt size, so the cached buckets
/// are carved back out of it and repriced: billing every cached token at the
/// full input rate would overstate a Claude Code turn several times over,
/// while billing only the uncached remainder — as this did before — understates
/// it by orders of magnitude.
fn calculate_cost(model: &str, usage: &TokenUsage) -> f64 {
    let (input_rate, output_rate) = model_rates(model);
    let cached = usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    let uncached = usage.input_tokens.saturating_sub(cached);
    (uncached as f64 * input_rate
        + usage.cache_creation_input_tokens as f64 * input_rate * CACHE_WRITE_RATE_MULTIPLIER
        + usage.cache_read_input_tokens as f64 * input_rate * CACHE_READ_RATE_MULTIPLIER
        + usage.output_tokens as f64 * output_rate)
        / 1000.0
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

/// Ratio of `part` to `whole` as a **0–1 fraction** rounded to two decimals.
///
/// Every rate the audit API reports goes through here so they all share one
/// scale. Mixing a 0–1 fraction with a 0–100 percentage under two
/// similarly-named JSON keys is the kind of thing that silently corrupts a
/// dashboard downstream. Returns `0.0` for a zero denominator instead of the
/// `NaN` that would serialize as `null`.
fn ratio_2dp(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        return 0.0;
    }
    (part / whole * 100.0).round() / 100.0
}

/// Extracts the client session id from a request's `metadata.user_id`.
///
/// Claude Code sends that field as a JSON *string* containing another JSON
/// object (`{"device_id":…,"session_id":…}`), so it takes two parses. Several
/// client instances routinely share one proxy, and without this their records
/// interleave in the dashboard with nothing to tell them apart.
///
/// Every unexpected shape degrades to `None` — this is diagnostic metadata and
/// must never be able to fail a request.
fn extract_session_id(req: &Value) -> Option<String> {
    let raw = req.get("metadata")?.get("user_id")?.as_str()?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let id = parsed.get("session_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// Records a request that failed before producing a usable response.
///
/// Every early `return` on an inference path routes through here. Without it
/// the request vanishes entirely: records were only ever added on the success
/// path, so a connection error or a 429 left nothing in the dashboard and
/// nothing to diagnose from.
#[allow(clippy::too_many_arguments)]
fn record_failure(
    state: &SharedState,
    endpoint: &str,
    model: &str,
    translated: Option<&str>,
    status: u16,
    kind: &str,
    req_size: usize,
    req_body: Option<String>,
    resp_body: Option<String>,
    start: Instant,
    session_id: Option<String>,
) {
    state.store.add(RequestRecord {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: now_iso(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        translated_model: translated.filter(|t| *t != model).map(String::from),
        status_code: status,
        request_size: req_size,
        response_size: resp_body.as_ref().map(|s| s.len()).unwrap_or(0),
        input_tokens: 0,
        output_tokens: 0,
        output_tokens_final: None,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning_tokens: None,
        billed_nano_aiu: None,
        cache_saved_nano_aiu: None,
        premium_multiplier: None,
        upstream_idle_max_ms: None,
        keepalive_probes: None,
        duration: elapsed_secs(start),
        request_body: req_body,
        // Kept regardless of debug mode: an error body is small and is the
        // entire reason this record exists.
        response_body: resp_body,
        message_count: None,
        tool_count: None,
        tool_names: None,
        stop_reason: None,
        tools_called: None,
        is_agent_initiated: None,
        prompt_cache_hit: None,
        session_id,
        failure_kind: Some(kind.to_string()),
        estimated_cost_usd: None,
    });
}

/// Guarantees a streaming request is recorded on every exit path, including
/// the one that used to lose it entirely.
///
/// When a client hangs up, axum drops the response body, which drops the
/// `async_stream` generator — so a `store.add` written after the loop never
/// runs. Holding the record in a value with a `Drop` impl moves the write onto
/// an unconditional path. `RequestStore::add` takes a synchronous
/// `std::sync::Mutex`, so it is safe to call from `Drop`; anything async (such
/// as the premium multiplier) must be resolved before the stream starts.
/// Running state folded out of the upstream Anthropic SSE stream on the
/// passthrough path (`stream_anthropic_direct`).
///
/// Kept as a plain value rather than as generator locals so the whole
/// event-interpretation rulebook is testable without standing up a stream.
#[derive(Debug, Default)]
struct DirectStreamState {
    usage: TokenUsage,
    /// What Copilot billed, attached to the terminal event beside `usage`.
    billed_nano_aiu: Option<u64>,
    /// What the cache was worth, from the same event's per-token rates.
    cache_saved_nano_aiu: Option<i64>,
    /// Whether `message_delta` — the only event carrying an authoritative
    /// `output_tokens` — has arrived. Until it does, the count taken from
    /// `message_start` is an opening placeholder, so a record finalized early
    /// must not present it as the turn's real output.
    usage_final: bool,
    stop_reason: Option<String>,
    tools_called: Vec<String>,
    saw_message_stop: bool,
}

impl DirectStreamState {
    /// Folds one decoded SSE event into the running state.
    fn observe(&mut self, v: &Value) {
        // Rides along on the terminal event rather than inside `usage`.
        if let Some(n) = util::copilot_billed_nano_aiu(v) {
            self.billed_nano_aiu = Some(n);
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                // The input buckets are only ever stated here, and
                // `input_tokens` alone is the uncached remainder — for a
                // cached Claude Code turn that is single digits against a
                // six-figure prompt.
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    self.usage = util::anthropic_usage(u);
                }
            }
            Some("message_delta") => {
                // `message_delta` restates the running output count and
                // nothing else; adopting it wholesale would zero the input
                // buckets captured at message_start.
                if let Some(o) = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                {
                    self.usage.output_tokens = o;
                    self.usage_final = true;
                }
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.stop_reason = Some(sr.to_string());
                }
            }
            Some("content_block_start") => {
                if let Some(block) = v.get("content_block") {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                            if !self.tools_called.iter().any(|t| t == name) {
                                self.tools_called.push(name.to_string());
                            }
                        }
                    }
                }
            }
            Some("message_stop") => self.saw_message_stop = true,
            _ => {}
        }
    }
}

struct StreamRecorder {
    state: SharedState,
    /// Taken by the first finalize, so `Drop` cannot record a second time.
    rec: Option<RequestRecord>,
    model_for_cost: String,
    start: Instant,
    /// Running counters, kept here rather than as generator locals so `Drop`
    /// can still see how far the stream got.
    usage: TokenUsage,
    /// Mirrors `DirectStreamState::usage_final`, for the same reason: `Drop`
    /// must be able to tell a record cut short mid-stream — whose output count
    /// is still the `message_start` placeholder — from one that saw the real
    /// total in `message_delta`.
    usage_final: bool,
    /// What Copilot billed for this turn, once its terminal event states it.
    billed_nano_aiu: Option<u64>,
    /// What the cache was worth this turn, from the model's own rates.
    cache_saved_nano_aiu: Option<i64>,
    resp_size: usize,
    idle: util::IdleTracker,
    /// Shared with the keepalive layer wrapping this stream, so the record can
    /// state how many probes actually went out during an upstream silence.
    probes: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Raw upstream bytes seen so far, for the same reason: a client
    /// disconnect is exactly when you want to read what did arrive, and a
    /// generator local would be gone by then. Only filled in debug mode.
    debug_raw: Vec<u8>,
}

impl StreamRecorder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: SharedState,
        endpoint: &str,
        model: String,
        translated: String,
        req_size: usize,
        req_body: Option<String>,
        premium_multiplier: Option<f64>,
        start: Instant,
        probes: std::sync::Arc<std::sync::atomic::AtomicU32>,
        session_id: Option<String>,
    ) -> Self {
        let rec = RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: endpoint.to_string(),
            translated_model: (translated != model).then(|| translated.clone()),
            model,
            status_code: 0,
            request_size: req_size,
            response_size: 0,
            input_tokens: 0,
            output_tokens: 0,
            output_tokens_final: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: None,
            billed_nano_aiu: None,
            cache_saved_nano_aiu: None,
            premium_multiplier,
            upstream_idle_max_ms: None,
            keepalive_probes: None,
            duration: 0.0,
            request_body: req_body,
            response_body: None,
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: None,
            prompt_cache_hit: None,
            session_id,
            failure_kind: None,
            estimated_cost_usd: None,
        };
        StreamRecorder {
            state,
            rec: Some(rec),
            model_for_cost: translated,
            start,
            usage: TokenUsage::default(),
            usage_final: false,
            billed_nano_aiu: None,
            cache_saved_nano_aiu: None,
            resp_size: 0,
            idle: util::IdleTracker::new(Instant::now()),
            probes,
            debug_raw: Vec::new(),
        }
    }

    /// Mutable access to the pending record for path-specific fields the
    /// generic finalize does not cover (tool names, message counts, …).
    fn record_mut(&mut self) -> Option<&mut RequestRecord> {
        self.rec.as_mut()
    }

    /// Folds the running counters into the record and hands it to the store.
    /// Idempotent — the second call (typically from `Drop`) is a no-op.
    fn finalize(
        &mut self,
        status: u16,
        failure_kind: Option<&str>,
        stop_reason: Option<String>,
        response_body: Option<String>,
    ) {
        let Some(mut rec) = self.rec.take() else {
            return;
        };
        rec.status_code = status;
        rec.response_size = self.resp_size;
        rec.input_tokens = self.usage.input_tokens;
        rec.output_tokens = self.usage.output_tokens;
        rec.output_tokens_final = Some(self.usage_final);
        rec.cache_read_input_tokens = self.usage.cache_read_input_tokens;
        rec.cache_creation_input_tokens = self.usage.cache_creation_input_tokens;
        rec.reasoning_tokens =
            (self.usage.reasoning_tokens > 0).then_some(self.usage.reasoning_tokens);
        rec.billed_nano_aiu = self.billed_nano_aiu;
        rec.cache_saved_nano_aiu = self.cache_saved_nano_aiu;
        rec.upstream_idle_max_ms = Some(self.idle.max_idle_ms_including_now());
        rec.keepalive_probes = Some(self.probes.load(std::sync::atomic::Ordering::Relaxed));
        rec.duration = elapsed_secs(self.start);
        rec.prompt_cache_hit = cache_disposition(&self.usage);
        rec.estimated_cost_usd = Some(calculate_cost(&self.model_for_cost, &self.usage));
        rec.stop_reason = stop_reason;
        rec.response_body = response_body;
        rec.failure_kind = failure_kind.map(String::from);
        self.state.store.add(rec);
    }
}

impl Drop for StreamRecorder {
    fn drop(&mut self) {
        // Only reachable when the generator was dropped before finalize ran,
        // i.e. the client went away mid-stream. 499 follows nginx's
        // "client closed request". Whatever had already arrived from the
        // upstream is preserved — that partial body is the main evidence for
        // diagnosing why the client gave up.
        let partial = (!self.debug_raw.is_empty())
            .then(|| String::from_utf8_lossy(&self.debug_raw).into_owned());
        self.finalize(
            499,
            Some(crate::store::failure::CLIENT_DISCONNECTED),
            None,
            partial,
        );
    }
}

/// Whether an upstream response should be forwarded to the client as an SSE
/// stream.
///
/// A non-2xx upstream returns a JSON error body, not SSE. Wrapping one in a
/// 200 `text/event-stream` produces a stream that never yields a single event,
/// which clients report as a stalled or hung response instead of the actual
/// auth/quota failure underneath. Every streaming path must gate on this.
fn is_streamable_status(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Whether a request touched the prompt cache: `Some(true)` when part of the
/// prompt was served from cache, `Some(false)` on a write-only turn (a cold
/// cache being populated), and `None` when caching was not involved at all.
fn cache_disposition(usage: &TokenUsage) -> Option<bool> {
    if usage.cache_read_input_tokens > 0 {
        Some(true)
    } else if usage.cache_creation_input_tokens > 0 {
        Some(false)
    } else {
        None
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
                    // Limits travel with the list so an operator can see which
                    // ids are worth marking `supports1m` without fetching each
                    // model separately.
                    let limits = m.get("capabilities").and_then(|c| c.get("limits"));
                    let context_window = limits
                        .and_then(|l| l.get("max_context_window_tokens"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let supports_1m = context_window.as_u64().is_some_and(|t| t > 200_000);
                    json!({
                        "id": id,
                        "object": "model",
                        "type": "model",
                        "created": 0,
                        "created_at": "1970-01-01T00:00:00.000Z",
                        "owned_by": m.get("vendor").cloned().unwrap_or(Value::String("unknown".into())),
                        "display_name": m.get("name").cloned().or_else(|| m.get("id").cloned()).unwrap_or(Value::Null),
                        "context_window": context_window,
                        "max_output_tokens": limits
                            .and_then(|l| l.get("max_output_tokens"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "supports_1m_context": supports_1m,
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

/// OpenAI-compatible single model retrieval (`GET /v1/models/{model}`).
///
/// Model ids are matched after translation so aliases configured in
/// `model_mappings` (e.g. `sonnet`) resolve the same way they do on the
/// inference endpoints. Unknown ids return a 404 in the OpenAI error shape.
async fn get_model(State(state): State<SharedState>, Path(model_id): Path<String>) -> Response {
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = state
        .ensure_models_fresh(Duration::from_secs(30 * 60))
        .await
    {
        tracing::warn!("model refresh failed: {e}");
    }
    let translated = translate::translate(&state.model_mappings(), &model_id);
    let found = match state.find_model(&model_id).await {
        Some(m) => Some(m),
        None => state.find_model(&translated).await,
    };
    let Some(entry) = found else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("The model '{model_id}' does not exist."),
                    "type": "invalid_request_error",
                    "code": "model_not_found"
                }
            })),
        )
            .into_response();
    };
    let id = entry.get("id").cloned().unwrap_or(Value::Null);
    Json(json!({
        "id": id,
        "object": "model",
        "type": "model",
        "created": 0,
        "created_at": "1970-01-01T00:00:00.000Z",
        "owned_by": entry.get("vendor").cloned().unwrap_or(Value::String("unknown".into())),
        "display_name": entry.get("name").cloned().or_else(|| entry.get("id").cloned()).unwrap_or(Value::Null),
        "capabilities": entry.get("capabilities").cloned().unwrap_or(Value::Null),
        "supported_endpoints": entry.get("supported_endpoints").cloned().unwrap_or(Value::Null),
    }))
    .into_response()
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

    // Some Copilot models are only reachable through `/responses` and answer a
    // chat-completions call with an opaque `unsupported_api_for_model` 400.
    // Turn that into an actionable message before spending the round trip.
    if !to_github_models
        && !state
            .model_supports_endpoint(&translated, "/chat/completions")
            .await
        && state
            .model_supports_endpoint(&translated, "/responses")
            .await
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": format!(
                        "Model '{original_model}' is not available on /v1/chat/completions. \
                         Use /v1/responses with '{translated}' instead."
                    ),
                    "type": "invalid_request_error",
                    "code": "unsupported_api_for_model"
                }
            })),
        )
            .into_response();
    }

    // Copilot rejects a chat-completions request that omits `max_tokens` for
    // some models. Fill it from the model catalog rather than surfacing an
    // avoidable 400.
    if req.get("max_tokens").is_none() && req.get("max_completion_tokens").is_none() {
        if let Some(limit) = state.model_max_output_tokens(&translated).await {
            req["max_tokens"] = json!(limit);
        }
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
            req,
            "/v1/chat/completions",
            original_model,
            translated,
            req_size,
            start,
        )
        .await;
    }

    let resp = util::post_with_retry(
        &state,
        &url,
        headers.clone(),
        payload,
        "/v1/chat/completions",
    )
    .await;
    let Some(resp) = resp else {
        return error_response(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "Upstream connection error after {} attempts",
                state.max_connection_retries() + 1
            ),
        );
    };
    let mut status = resp.status();
    let mut text = resp.text().await.unwrap_or_default();
    // Newer OpenAI-family models reject `max_tokens` and demand
    // `max_completion_tokens`. Migrate and retry once instead of surfacing a
    // parameter-naming error the caller cannot act on.
    if util::is_max_tokens_unsupported_error(status.as_u16(), &text)
        && rewrite_max_tokens_param(&mut req)
    {
        tracing::info!("[/v1/chat/completions] retrying with max_completion_tokens");
        let retry_payload = serde_json::to_vec(&req).unwrap_or_default();
        if let Some(retry) =
            util::post_with_retry(&state, &url, headers, retry_payload, "/v1/chat/completions")
                .await
        {
            status = retry.status();
            text = retry.text().await.unwrap_or_default();
        }
    }
    let resp_size = text.len();
    log_debug_response(&state, "/v1/chat/completions", &text);
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let usage = util::openai_usage(&parsed.get("usage").cloned().unwrap_or(json!({})));
        let billed = util::copilot_billed_nano_aiu(&parsed);
        let cache_saved = util::cache_saving_nano_aiu(&parsed, &usage);
        let (tool_count, tool_names) = extract_tools_from_request(&req);
        let cost = calculate_cost(&translated, &usage);
        let premium_multiplier = state.model_premium_multiplier(&translated).await;
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/chat/completions".into(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier,
            upstream_idle_max_ms: None,
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &req),
            response_body: capture_str(&state, &text),
            message_count: Some(extract_message_count(&req)),
            tool_count: (tool_count > 0).then_some(tool_count),
            tool_names: (tool_count > 0).then_some(tool_names),
            stop_reason: None, // OpenAI responses don't have stop_reason in JSON
            tools_called: None,
            is_agent_initiated: Some(agent),
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(cost),
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
    if state.config_snapshot().routes_to_github_models(&translated) {
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
        let usage = util::responses_usage(&parsed.get("usage").cloned().unwrap_or(json!({})));
        let billed = util::copilot_billed_nano_aiu(&parsed);
        let cache_saved = util::cache_saving_nano_aiu(&parsed, &usage);
        let (tool_count, tool_names) = extract_tools_from_request(&req);
        let cost = calculate_cost(&translated, &usage);
        let premium_multiplier = state.model_premium_multiplier(&translated).await;
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/responses".into(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier,
            upstream_idle_max_ms: None,
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &req),
            response_body: capture_str(&state, &text),
            message_count: None, // /responses uses "input" not "messages"
            tool_count: (tool_count > 0).then_some(tool_count),
            tool_names: (tool_count > 0).then_some(tool_names),
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: Some(agent),
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(cost),
        });
        Json(parsed).into_response()
    } else {
        log_error("/v1/responses", &req, &text, status.as_u16());
        passthrough_error(status, text)
    }
}

// ---------------------------------------------------------------------------
// Responses over WebSocket
// ---------------------------------------------------------------------------

/// Message type a client must send to start a turn. The upstream rejects
/// anything else with `unsupported message type`.
const WS_RESPONSE_CREATE: &str = "response.create";

/// The catalog's name for this surface.
const WS_RESPONSES_ENDPOINT: &str = "ws:/responses";

/// Sends a `type: error` frame in the shape the upstream itself uses, so a
/// client needs no special handling for failures the proxy originates.
async fn ws_error(socket: &mut WebSocket, code: &str, message: &str) {
    let frame = json!({"type": "error", "error": {"code": code, "message": message}});
    let _ = socket.send(Message::Text(frame.to_string().into())).await;
}

/// The Responses API over WebSocket.
///
/// Several models advertise `ws:/responses` in the catalog and nothing else
/// besides `/responses`; this exposes that transport to clients. The protocol
/// is the same `response.*` event vocabulary as the SSE path, one event per
/// text frame — only the transport differs, so a client already written
/// against the streaming Responses API needs no new parsing.
///
/// Undocumented upstream: the request frame must be flat, with `model` at the
/// top level. Nesting it under `response` is rejected.
async fn ws_responses(ws: WebSocketUpgrade, State(state): State<SharedState>) -> Response {
    ws.on_upgrade(move |socket| handle_ws_responses(socket, state))
}

async fn handle_ws_responses(mut socket: WebSocket, state: SharedState) {
    // The turn does not begin until the client sends something, so there is no
    // work to charge for and no record to write if it just connects and leaves.
    let first = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => break t.to_string(),
            Some(Ok(Message::Binary(b))) => break String::from_utf8_lossy(&b).into_owned(),
            // Keepalive traffic before the request is normal; axum answers pings.
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return,
        }
    };

    let start = Instant::now();
    let req_size = first.len();

    let Ok(req) = serde_json::from_str::<Value>(&first) else {
        // Recorded like any other pre-flight failure: a request that leaves no
        // trace is a request nobody can diagnose.
        record_failure(
            &state,
            "ws:/responses",
            "",
            None,
            400,
            crate::store::failure::PRECONDITION,
            req_size,
            capture_str(&state, &first),
            None,
            start,
            None,
        );
        ws_error(&mut socket, "bad_request", "frame is not JSON").await;
        return;
    };
    let msg_type = req.get("type").and_then(Value::as_str).unwrap_or("");
    if msg_type != WS_RESPONSE_CREATE {
        record_failure(
            &state,
            "ws:/responses",
            req.get("model").and_then(Value::as_str).unwrap_or_default(),
            None,
            400,
            crate::store::failure::PRECONDITION,
            req_size,
            capture_json(&state, &req),
            None,
            start,
            None,
        );
        ws_error(
            &mut socket,
            "bad_request",
            &format!("unsupported message type: {msg_type}"),
        )
        .await;
        return;
    }

    let original_model = req
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let translated = translate::translate(&state.model_mappings(), &original_model);

    // The transport is only available for models that advertise it, and saying
    // so beats an opaque upstream rejection.
    if !state
        .model_supports_endpoint(&translated, WS_RESPONSES_ENDPOINT)
        .await
    {
        record_failure(
            &state,
            "ws:/responses",
            &original_model,
            Some(&translated),
            400,
            crate::store::failure::PRECONDITION,
            req_size,
            capture_json(&state, &req),
            None,
            start,
            None,
        );
        ws_error(
            &mut socket,
            "unsupported_api_for_model",
            &format!(
                "Model '{original_model}' does not support {WS_RESPONSES_ENDPOINT}. \
                 Use POST /v1/responses instead."
            ),
        )
        .await;
        return;
    }

    if let Err(e) = state.ensure_copilot_token().await {
        record_failure(
            &state,
            "ws:/responses",
            &original_model,
            Some(&translated),
            500,
            crate::store::failure::PRECONDITION,
            req_size,
            capture_json(&state, &req),
            None,
            start,
            None,
        );
        ws_error(&mut socket, "internal_error", &e).await;
        return;
    }

    // Forward the model the mapping resolved to, not the alias the client used.
    let mut upstream_req = req.clone();
    if let Some(m) = upstream_req.get_mut("model") {
        *m = json!(translated);
    }

    let (url, headers) = state.ws_responses_upstream().await;
    log_debug_request(&state, "ws:/responses", &upstream_req);

    let mut request =
        match tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        ) {
            Ok(r) => r,
            Err(e) => {
                ws_error(
                    &mut socket,
                    "internal_error",
                    &format!("bad upstream url: {e}"),
                )
                .await;
                return;
            }
        };
    // The handshake carries the same credentials as the HTTP path.
    for (k, v) in headers.iter() {
        request.headers_mut().insert(k.clone(), v.clone());
    }

    let upstream = match tokio_tungstenite::connect_async(request).await {
        Ok((s, _)) => s,
        Err(e) => {
            record_failure(
                &state,
                "ws:/responses",
                &original_model,
                Some(&translated),
                502,
                crate::store::failure::CONNECT,
                req_size,
                capture_json(&state, &req),
                None,
                start,
                None,
            );
            ws_error(&mut socket, "connect_error", &format!("upstream: {e}")).await;
            return;
        }
    };

    pump_ws_responses(
        socket,
        upstream,
        state,
        original_model,
        translated,
        req,
        req_size,
        start,
    )
    .await;
}

/// Relays frames in both directions until the turn ends, accumulating what the
/// record needs on the way past. Usage only becomes known at
/// `response.completed`, so the totals are folded in as the events go by rather
/// than by re-parsing the transcript afterwards.
#[allow(clippy::too_many_arguments)]
async fn pump_ws_responses(
    mut client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    state: SharedState,
    original_model: String,
    translated: String,
    req: Value,
    req_size: usize,
    start: Instant,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let (mut up_tx, mut up_rx) = upstream.split();

    // The first frame was consumed to route the request; send the rewritten one.
    let mut outbound = req.clone();
    if let Some(m) = outbound.get_mut("model") {
        *m = json!(translated);
    }
    if up_tx
        .send(WsMessage::Text(outbound.to_string().into()))
        .await
        .is_err()
    {
        ws_error(
            &mut client,
            "connect_error",
            "upstream closed before the request was sent",
        )
        .await;
        return;
    }

    let mut usage = TokenUsage::default();
    // Copilot reports what it billed only on the terminal event.
    let mut billed: Option<u64> = None;
    let mut cache_saved: Option<i64> = None;
    let mut status = 200u16;
    let mut failure: Option<&'static str> = None;
    let mut resp_size = 0usize;
    let mut transcript = String::new();
    let capture = state.is_debug();
    let mut idle = util::IdleTracker::new(Instant::now());

    loop {
        tokio::select! {
            // Upstream → client. This is where the answer comes from, so it is
            // also where the turn ends.
            frame = up_rx.next() => match frame {
                Some(Ok(WsMessage::Text(t))) => {
                    idle.mark_now();
                    resp_size += t.len();
                    if capture {
                        transcript.push_str(&t);
                        transcript.push('\n');
                    }
                    let done = ws_absorb_event(
                        &t,
                        &mut usage,
                        &mut billed,
                        &mut cache_saved,
                        &mut status,
                        &mut failure,
                    );
                    if client.send(Message::Text(t.as_str().into())).await.is_err() {
                        failure.get_or_insert(crate::store::failure::CLIENT_DISCONNECTED);
                        status = 499;
                        break;
                    }
                    if done {
                        break;
                    }
                }
                Some(Ok(WsMessage::Binary(b))) => {
                    idle.mark_now();
                    resp_size += b.len();
                    if client.send(Message::Binary(b.to_vec().into())).await.is_err() {
                        failure.get_or_insert(crate::store::failure::CLIENT_DISCONNECTED);
                        status = 499;
                        break;
                    }
                }
                Some(Ok(WsMessage::Close(_))) | None => {
                    // A close before `response.completed` truncated the turn.
                    if failure.is_none() && status == 200 && usage.output_tokens == 0 {
                        failure = Some(crate::store::failure::STREAM_INTERRUPTED);
                        status = 502;
                    }
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::warn!("[ws:/responses] upstream error: {e}");
                    failure = Some(crate::store::failure::STREAM_INTERRUPTED);
                    status = 502;
                    let _ = client
                        .send(Message::Text(
                            json!({"type": "error", "error": {"code": "stream_interrupted",
                                   "message": e.to_string()}})
                            .to_string()
                            .into(),
                        ))
                        .await;
                    break;
                }
            },

            // Client → upstream. Kept open for the whole turn: this transport
            // is bidirectional, and a client may cancel or send another
            // `response.create` on the same connection.
            frame = client.recv() => match frame {
                Some(Ok(Message::Text(t))) => {
                    if up_tx.send(WsMessage::Text(t.as_str().into())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if up_tx.send(WsMessage::Binary(b.to_vec().into())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    failure.get_or_insert(crate::store::failure::CLIENT_DISCONNECTED);
                    status = 499;
                    let _ = up_tx.send(WsMessage::Close(None)).await;
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => {
                    failure.get_or_insert(crate::store::failure::CLIENT_DISCONNECTED);
                    status = 499;
                    break;
                }
            },
        }
    }

    let _ = up_tx.send(WsMessage::Close(None)).await;

    let (tool_count, tool_names) = extract_tools_from_request(&req);
    let cost = calculate_cost(&translated, &usage);
    let premium_multiplier = state.model_premium_multiplier(&translated).await;
    if capture {
        log_debug_response(&state, "ws:/responses", &transcript);
    }
    state.store.add(RequestRecord {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: now_iso(),
        endpoint: "ws:/responses".into(),
        model: original_model.clone(),
        translated_model: (translated != original_model).then_some(translated),
        status_code: status,
        request_size: req_size,
        response_size: resp_size,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        // The upstream sends usage only in the terminal event, so reaching it
        // is exactly what makes the count authoritative.
        output_tokens_final: Some(failure.is_none()),
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
        billed_nano_aiu: billed,
        cache_saved_nano_aiu: cache_saved,
        premium_multiplier,
        upstream_idle_max_ms: Some(idle.max_idle_ms_including_now()),
        keepalive_probes: None,
        duration: elapsed_secs(start),
        // Both halves use the flag as it stood when the stream opened. A socket
        // can stay open for minutes, so re-reading it here would let a toggle
        // mid-turn produce a record with the request captured and the response
        // missing — half a transcript is more misleading than none.
        request_body: capture.then(|| req.to_string()),
        response_body: capture.then_some(transcript),
        message_count: None,
        tool_count: (tool_count > 0).then_some(tool_count),
        tool_names: (tool_count > 0).then_some(tool_names),
        stop_reason: None,
        tools_called: None,
        is_agent_initiated: None,
        session_id: None,
        prompt_cache_hit: cache_disposition(&usage),
        failure_kind: failure.map(String::from),
        estimated_cost_usd: Some(cost),
    });
}

/// Folds one upstream event into the running record state. Returns true when
/// the event terminates the turn.
fn ws_absorb_event(
    text: &str,
    usage: &mut TokenUsage,
    billed: &mut Option<u64>,
    cache_saved: &mut Option<i64>,
    status: &mut u16,
    failure: &mut Option<&'static str>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    // Attached to the terminal event, beside the response rather than inside it.
    if let Some(n) = util::copilot_billed_nano_aiu(&v) {
        *billed = Some(n);
    }
    let done = match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "response.completed" | "response.incomplete" => {
            if let Some(u) = v.pointer("/response/usage") {
                *usage = util::responses_usage(u);
            }
            true
        }
        "response.failed" => {
            if let Some(u) = v.pointer("/response/usage") {
                *usage = util::responses_usage(u);
            }
            *status = 502;
            *failure = Some(crate::store::failure::UPSTREAM_STATUS);
            true
        }
        "error" => {
            *status = 502;
            *failure = Some(crate::store::failure::UPSTREAM_STATUS);
            true
        }
        _ => false,
    };
    // After the arms above, so the token counts it is derived from are the ones
    // the terminal event just stated.
    if let Some(n) = util::cache_saving_nano_aiu(&v, usage) {
        *cache_saved = Some(n);
    }
    done
}

// ---------------------------------------------------------------------------
// Anthropic messages// ---------------------------------------------------------------------------

async fn messages(
    State(state): State<SharedState>,
    client_headers: HeaderMap,
    body: Bytes,
) -> Response {
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

    let client_beta = client_beta_header(&client_headers);
    // What the client sent, counted once. The retry loops downstream mutate the
    // request, so measuring it there would report the proxy's version of it and
    // re-serialise the whole body on every attempt to do so.
    let req_size = body.len();
    if state.use_direct_anthropic(&translated).await {
        messages_direct(
            state,
            req,
            original_model,
            translated,
            client_beta,
            req_size,
            start,
        )
        .await
    } else {
        messages_translated(state, req, original_model, translated, req_size, start).await
    }
}

/// Reads the client's `anthropic-beta` header, if any.
fn client_beta_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string())
}

/// Applies the `anthropic-beta` header for an upstream Anthropic-native call,
/// preserving whatever the client asked for and adding the flags this request
/// needs.
async fn apply_anthropic_beta(
    state: &SharedState,
    headers: &mut HeaderMap,
    model: &str,
    req: &Value,
    client_beta: Option<&str>,
    wants_1m: bool,
) {
    let mut derived: Vec<&str> = Vec::new();
    // Mirror the official Anthropic API pattern for unlocking the 1M-token
    // context window. It is opt-in: the client asks by naming the model's
    // `[1m]` variant, or by sending the beta itself. Deriving it from the
    // catalog alone would put every request on the extended-context tier and
    // leave the standard variant with nothing to distinguish it.
    if wants_1m && state.model_supports_1m(model).await {
        derived.push(anthropic::CONTEXT_1M_BETA);
    }
    // `context_management` is rejected with a misleading "Extra inputs are not
    // permitted" 400 unless the matching beta is requested.
    if anthropic::uses_context_management(req) {
        derived.push(anthropic::CONTEXT_MANAGEMENT_BETA);
    }
    if let Some(value) = anthropic::merge_anthropic_beta(client_beta, &derived) {
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert("anthropic-beta", v);
        }
    }
}

async fn messages_direct(
    state: SharedState,
    req: Value,
    original_model: String,
    translated: String,
    client_beta: Option<String>,
    req_size: usize,
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
    let (_, wants_1m) = translate::split_context_1m(&original_model);
    apply_anthropic_beta(
        &state,
        &mut headers,
        &translated,
        &req,
        client_beta.as_deref(),
        wants_1m,
    )
    .await;
    set_initiator(&mut headers, agent);

    let url = format!("{}/v1/messages", state.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut current = req.clone();
    let mut thinking_adapted = false;
    for _ in 0..4 {
        let mut sanitized = anthropic::sanitize_anthropic_request(&current);
        sanitized = anthropic::adjust_thinking_budget(&sanitized);
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
                Err(e) => {
                    record_failure(
                        &state,
                        "/v1/messages",
                        &original_model,
                        Some(&translated),
                        504,
                        crate::store::failure::CONNECT,
                        req_size,
                        capture_json(&state, &sanitized),
                        Some(e.to_string()),
                        start,
                        extract_session_id(&sanitized),
                    );
                    return anthropic_error(StatusCode::GATEWAY_TIMEOUT, e.to_string());
                }
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
                record_failure(
                    &state,
                    "/v1/messages",
                    &original_model,
                    Some(&translated),
                    status.as_u16(),
                    crate::store::failure::UPSTREAM_STATUS,
                    req_size,
                    capture_json(&state, &sanitized),
                    Some(text.clone()),
                    start,
                    extract_session_id(&sanitized),
                );
                return anthropic_passthrough_error(status, text);
            }
            return stream_anthropic_direct(
                state.clone(),
                upstream,
                RequestMeta {
                    req_body: capture_json(&state, &sanitized),
                    session_id: extract_session_id(&sanitized),
                    original_model,
                    translated,
                    req_size,
                    start,
                },
            )
            .await;
        }

        let resp =
            util::post_with_retry(&state, &url, headers.clone(), payload, "/v1/messages").await;
        let Some(resp) = resp else {
            record_failure(
                &state,
                "/v1/messages",
                &original_model,
                Some(&translated),
                504,
                crate::store::failure::CONNECT,
                req_size,
                capture_json(&state, &sanitized),
                None,
                start,
                extract_session_id(&sanitized),
            );
            return anthropic_error(
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream connection error".into(),
            );
        };
        let status = resp.status();
        if status.is_success() {
            // Read once, then parse from that. Asking reqwest for JSON and
            // re-serialising the tree costs two passes and reports a byte count
            // that is not the one that came off the wire.
            let text = resp.text().await.unwrap_or_default();
            let resp_size = text.len();
            log_debug_response(&state, "/v1/messages", &text);
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
            let usage = util::anthropic_usage(&usage);
            let billed = util::copilot_billed_nano_aiu(&parsed);
            let cache_saved = util::cache_saving_nano_aiu(&parsed, &usage);
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
                response_size: resp_size,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                output_tokens_final: None,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
                billed_nano_aiu: billed,
                cache_saved_nano_aiu: cache_saved,
                premium_multiplier: state.model_premium_multiplier(&translated).await,
                upstream_idle_max_ms: None,
                keepalive_probes: None,
                duration: elapsed_secs(start),
                request_body: capture_json(&state, &sanitized),
                response_body: capture_json(&state, &parsed),
                message_count: Some(extract_message_count(&req)),
                tool_count: (tool_count > 0).then_some(tool_count),
                tool_names: (tool_count > 0).then_some(tool_names),
                stop_reason,
                tools_called: (!tools_called.is_empty()).then_some(tools_called),
                is_agent_initiated: Some(agent),
                session_id: None,
                prompt_cache_hit: cache_disposition(&usage),
                failure_kind: None,
                estimated_cost_usd: Some(calculate_cost(&translated, &usage)),
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
        record_failure(
            &state,
            "/v1/messages",
            &original_model,
            Some(&translated),
            status.as_u16(),
            crate::store::failure::UPSTREAM_STATUS,
            req_size,
            capture_json(&state, &sanitized),
            Some(text.clone()),
            start,
            extract_session_id(&sanitized),
        );
        return anthropic_passthrough_error(status, text);
    }
    anthropic_error(StatusCode::BAD_GATEWAY, "Exhausted retries".into())
}

async fn messages_translated(
    state: SharedState,
    req: Value,
    original_model: String,
    translated: String,
    req_size: usize,
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
        set_initiator(&mut headers, agent);
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
                extract_session_id(&openai_req),
            )
            .await;
        }

        let resp =
            util::post_with_retry(&state, &url, headers, payload, "/v1/messages (translated)")
                .await;
        let Some(resp) = resp else {
            record_failure(
                &state,
                "/v1/messages",
                &original_model,
                Some(&translated),
                504,
                crate::store::failure::CONNECT,
                req_size,
                capture_json(&state, &openai_req),
                None,
                start,
                extract_session_id(&openai_req),
            );
            return anthropic_error(
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream connection error".into(),
            );
        };
        let status = resp.status();
        if status.is_success() {
            // Read once, then parse from that: `resp.json()` followed by a
            // re-serialisation costs two extra passes over the body and reports
            // a size that is not the one that came off the wire.
            let text = resp.text().await.unwrap_or_default();
            let resp_size = text.len();
            log_debug_response(&state, "/v1/messages", &text);
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let anthropic_resp = anthropic::openai_to_anthropic(&parsed);
            let usage = util::openai_usage(&parsed.get("usage").cloned().unwrap_or(json!({})));
            let billed = util::copilot_billed_nano_aiu(&parsed);
            let cache_saved = util::cache_saving_nano_aiu(&parsed, &usage);
            let (tool_count, tool_names) = extract_tools_from_request(&openai_req);
            state.store.add(RequestRecord {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: now_iso(),
                endpoint: "/v1/messages".into(),
                model: original_model.clone(),
                translated_model: (translated != original_model).then_some(translated.clone()),
                status_code: status.as_u16(),
                request_size: req_size,
                response_size: resp_size,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                output_tokens_final: None,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
                billed_nano_aiu: billed,
                cache_saved_nano_aiu: cache_saved,
                premium_multiplier: state.model_premium_multiplier(&translated).await,
                upstream_idle_max_ms: None,
                keepalive_probes: None,
                duration: elapsed_secs(start),
                request_body: capture_json(&state, &openai_req),
                response_body: capture_json(&state, &parsed),
                message_count: Some(extract_message_count(&openai_req)),
                tool_count: (tool_count > 0).then_some(tool_count),
                tool_names: (tool_count > 0).then_some(tool_names),
                stop_reason: None,
                tools_called: None,
                is_agent_initiated: Some(agent),
                session_id: None,
                prompt_cache_hit: cache_disposition(&usage),
                failure_kind: None,
                estimated_cost_usd: Some(calculate_cost(&translated, &usage)),
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
        record_failure(
            &state,
            "/v1/messages",
            &original_model,
            Some(&translated),
            status.as_u16(),
            crate::store::failure::UPSTREAM_STATUS,
            req_size,
            capture_json(&state, &openai_req),
            Some(text.clone()),
            start,
            extract_session_id(&openai_req),
        );
        return anthropic_passthrough_error(status, text);
    }
    anthropic_error(StatusCode::BAD_GATEWAY, "Exhausted retries".into())
}

async fn count_tokens(
    State(state): State<SharedState>,
    client_headers: HeaderMap,
    body: Bytes,
) -> Response {
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

    // Prefer real token counting from upstream whenever the model can plausibly
    // serve it. Copilot's catalog does not advertise
    // `/v1/messages/count_tokens` separately, so any model exposing the native
    // Anthropic `/v1/messages` surface is worth trying.
    let native_count = state
        .model_supports_endpoint(&translated, "/v1/messages/count_tokens")
        .await
        || state
            .model_supports_endpoint(&translated, "/v1/messages")
            .await;
    if native_count {
        let vision = anthropic::has_image(&req);
        let mut headers = state.copilot_headers(vision).await;
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        apply_anthropic_beta(
            &state,
            &mut headers,
            &translated,
            &req,
            client_beta_header(&client_headers).as_deref(),
            translate::split_context_1m(&original_model).1,
        )
        .await;
        let url = format!("{}/v1/messages/count_tokens", state.copilot_base_url());
        let payload =
            serde_json::to_vec(&anthropic::sanitize_anthropic_request(&req)).unwrap_or_default();
        if let Some(resp) =
            util::post_with_retry(&state, &url, headers, payload, "/v1/messages/count_tokens").await
        {
            let status = resp.status();
            if status.is_success() {
                let parsed: Value = resp.json().await.unwrap_or(json!({"input_tokens": 1}));
                if parsed.get("input_tokens").is_some() {
                    return Json(parsed).into_response();
                }
            } else {
                tracing::debug!(
                    "[count_tokens] upstream returned {status} for '{translated}'; \
                     falling back to a local estimate"
                );
            }
        }
    }

    // Fall back to a local tiktoken estimate. Returning an error here would
    // break clients such as Claude Code, which call this endpoint before every
    // request purely to decide when to compact the conversation.
    //
    // A bare text count is systematically low: it ignores the per-message
    // framing tokens and the JSON schema of every tool definition. Under-
    // reporting makes Claude Code compact too late and then hit a hard
    // `prompt token count exceeds the limit` failure, so the estimate adds
    // both back.
    let tokenizer = state.model_tokenizer(&translated).await;
    let total = anthropic::estimate_input_tokens(&req, &tokenizer);
    tracing::debug!(
        "[count_tokens] upstream counting unavailable for '{original_model}'; \
         returning local {tokenizer} estimate of {total} tokens"
    );
    Json(json!({"input_tokens": total, "estimated": true})).into_response()
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
        let usage = util::openai_usage(&parsed.get("usage").cloned().unwrap_or(json!({})));
        let billed = util::copilot_billed_nano_aiu(&parsed);
        let cache_saved = util::cache_saving_nano_aiu(&parsed, &usage);
        let cost = calculate_cost(&translated, &usage);
        let premium_multiplier = state.model_premium_multiplier(&translated).await;
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1beta/models".into(),
            model: raw_model.clone(),
            translated_model: (translated != raw_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier,
            upstream_idle_max_ms: None,
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &openai_req),
            response_body: capture_str(&state, &text),
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: Some(agent),
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(cost),
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
    state.record_quota_headers(upstream.headers());
    let status = upstream.status().as_u16();
    // Surface a non-2xx upstream (JSON error, not SSE) as a normal error.
    if !is_streamable_status(status) {
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
    // Shared with the keepalive wrapper so the record can report how
    // many probes actually went out during an upstream silence.
    let probe_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut lines = util::SseLineBuffer::new();
        let mut usage = TokenUsage::default();
            // Copilot reports what it billed only on the terminal event.
            let mut billed: Option<u64> = None;
            let mut cache_saved: Option<i64> = None;
        let mut resp_size = 0usize;
        let mut finish: Option<String> = None;
        let mut debug_raw: Vec<u8> = Vec::new();
        let mut interrupted: Option<String> = None;
        let mut idle = util::IdleTracker::new(std::time::Instant::now());
        loop {
            let chunk = match byte_stream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => { interrupted = Some(util::error_chain(&e)); break; }
                None => break,
            };
            idle.mark_now();
            if state.is_debug() { debug_raw.extend_from_slice(&chunk); }
            for line in lines.push(&chunk) {
                let Some(data) = util::sse_data(&line) else { continue };
                if data == "[DONE]" || data.is_empty() { continue; }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if let Some(u) = v.get("usage") {
                        if !u.is_null() {
                            usage.merge_stream_update(util::openai_usage(u));
                            billed = util::copilot_billed_nano_aiu(&v).or(billed);
                            cache_saved = util::cache_saving_nano_aiu(&v, &usage).or(cache_saved);
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
        }
        // Final chunk with finish reason + usage. A stream cut short reports
        // `OTHER` so the client can tell the candidate is incomplete rather
        // than treating a partial answer as a finished one.
        if let Some(ref reason) = interrupted {
            tracing::warn!("[/v1beta/models] upstream stream interrupted: {reason}");
            finish = Some("OTHER".to_string());
        }
        let usage_json = json!({"prompt_tokens": usage.input_tokens, "completion_tokens": usage.output_tokens});
        let ev = gemini::gemini_stream_final_chunk(finish.as_deref(), &usage_json, &model_json);
        let payload = format!("data: {}\n\n", serde_json::to_string(&ev).unwrap_or_default());
        resp_size += payload.len();
        yield Ok(Bytes::from(payload));
        let debug_resp = String::from_utf8_lossy(&debug_raw).into_owned();
        log_debug_response(&state, "/v1beta/models", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1beta/models".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: if interrupted.is_some() { 502 } else { status },
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier: state.model_premium_multiplier(&translated).await,
            upstream_idle_max_ms: Some(idle.max_idle_ms_including_now()),
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: interrupted.is_some().then(|| "stream_interrupted".to_string()),
            tools_called: None,
            is_agent_initiated: None,
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(calculate_cost(&translated, &usage)),
        });
    };
    build_sse_response(stream, COMMENT_KEEPALIVE_PROBE, probe_count.clone())
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
        let usage = util::openai_usage(&parsed.get("usage").cloned().unwrap_or(json!({})));
        let billed = util::copilot_billed_nano_aiu(&parsed);
        let cache_saved = util::cache_saving_nano_aiu(&parsed, &usage);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/embeddings".into(),
            model: model.clone(),
            translated_model: None,
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: 0,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            // Embeddings are not billed as premium requests.
            premium_multiplier: None,
            upstream_idle_max_ms: None,
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: capture_json(&state, &req),
            response_body: capture_str(&state, &text),
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: Some(false),
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(calculate_cost(&model, &usage)),
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
    // The live per-SKU snapshot rides along on every proxied response, so it is
    // already current; the upstream call below adds the plan name and the
    // detailed per-category breakdown it does not carry.
    let live = state.quota_snapshot();
    match state.fetch_usage().await {
        Ok(v) => {
            let mut summary = crate::state::summarize_usage(&v);
            if !live.is_empty() {
                summary["live"] = serde_json::to_value(&live).unwrap_or(Value::Null);
            }
            Json(summary).into_response()
        }
        Err(e) => error_response(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Liveness/readiness probe. Always answers without contacting upstream so it
/// stays cheap enough for a service supervisor to poll frequently.
///
/// `ready` is true once a Copilot token has been obtained and the model catalog
/// has been loaded. A degraded proxy answers `200` with `ready: false` so
/// monitoring can distinguish "process alive" from "able to serve traffic";
/// pass `?strict=true` to get a `503` instead when not ready.
async fn health(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (token_present, token_expires_in) = state.copilot_token_status().await;
    let model_count = state.model_count().await;
    let stats = state.store.stats();
    let ready = token_present && model_count > 0;
    let body = json!({
        "status": if ready { "ok" } else { "degraded" },
        "ready": ready,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.uptime_secs(),
        "copilot_token": {
            "present": token_present,
            "expires_in_seconds": token_expires_in,
        },
        "models_loaded": model_count,
        "requests_served": stats.request_count,
        "auth_required": state.api_key().is_some(),
        // Whether request/response bodies are being captured. Surfaced so the
        // dashboard can say why a request has no body to show, and so an
        // operator can notice capture was left on.
        "debug": state.is_debug(),
        // Reported by the upstream on every response, so this costs no extra
        // API call. Empty until the first request has been proxied.
        "quota": state.quota_snapshot(),
    });
    let strict = params
        .get("strict")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if strict && !ready {
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    } else {
        Json(body).into_response()
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

/// The Anthropic error `type` for a status class.
fn anthropic_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// Forwards a non-2xx upstream response on the Anthropic surface, rewritten
/// into the Anthropic error envelope.
///
/// Copilot rejects a request in OpenAI's shape -- `{"error": {"message": ...}}`
/// -- which carries neither the top-level `"type": "error"` nor the
/// `error.type` that Anthropic clients match on. Forwarding that verbatim makes
/// a perfectly well-formed upstream rejection look like a malformed response to
/// the SDK, and the reason for the failure stops being legible.
fn anthropic_passthrough_error(status: StatusCode, text: String) -> Response {
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    // An upstream that already speaks Anthropic is forwarded untouched, so a
    // richer error keeps whatever fields it came with.
    if parsed.as_ref().is_some_and(|v| {
        v.get("type").and_then(Value::as_str) == Some("error")
            && v.get("error").and_then(|e| e.get("type")).is_some()
    }) {
        return passthrough_error(status, text);
    }
    let message = parsed
        .as_ref()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        // A non-JSON body (an HTML error page from an intermediary, say) is
        // still the most informative thing available.
        .unwrap_or(text);
    let message = if message.trim().is_empty() {
        status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_string()
    } else {
        message
    };
    anthropic_error(status, message)
}

fn anthropic_error(status: StatusCode, msg: String) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {"type": anthropic_error_type(status.as_u16()), "message": msg},
        })),
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
    mut req: Value,
    endpoint: &'static str,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
    let req_body = capture_json(&state, &req);
    let mut payload = serde_json::to_vec(&req).unwrap_or_default();
    let mut retried_param = false;
    let upstream = loop {
        let upstream = state
            .http
            .post(url)
            .headers(headers.clone())
            .body(payload.clone())
            .send()
            .await;
        let upstream = match upstream {
            Ok(r) => r,
            Err(e) => return error_response(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
        };
        state.record_quota_headers(upstream.headers());
        let status = upstream.status().as_u16();
        // A non-2xx upstream (e.g. GitHub Models returning 401/403 as JSON when
        // the token lacks the `models: read` permission) is not an SSE stream —
        // surface it as a normal error instead of forwarding a broken "stream".
        if !is_streamable_status(status) {
            let text = upstream.text().await.unwrap_or_default();
            log_debug_response(&state, endpoint, &text);
            // Newer OpenAI-family models reject `max_tokens` in favour of
            // `max_completion_tokens`; migrate and retry once.
            if !retried_param
                && util::is_max_tokens_unsupported_error(status, &text)
                && rewrite_max_tokens_param(&mut req)
            {
                tracing::info!("[{endpoint}] retrying stream with max_completion_tokens");
                payload = serde_json::to_vec(&req).unwrap_or_default();
                retried_param = true;
                continue;
            }
            log_error(endpoint, &json!({"model": &translated}), &text, status);
            return passthrough_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                text,
            );
        }
        break upstream;
    };
    state.record_quota_headers(upstream.headers());
    let status = upstream.status().as_u16();
    let model = translated.clone();
    // Shared with the keepalive wrapper so the record can report how
    // many probes actually went out during an upstream silence.
    let probe_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut lines = util::SseLineBuffer::new();
        let mut usage = TokenUsage::default();
            // Copilot reports what it billed only on the terminal event.
            let mut billed: Option<u64> = None;
            let mut cache_saved: Option<i64> = None;
        let mut resp_size = 0usize;
        let mut debug_raw: Vec<u8> = Vec::new();
        let mut interrupted: Option<String> = None;
        let mut saw_done = false;
        let mut idle = util::IdleTracker::new(std::time::Instant::now());
        loop {
            let chunk = match byte_stream.next().await {
                Some(Ok(chunk)) => chunk,
                // The upstream connection dropped mid-response. Everything
                // emitted so far is a partial answer; record why and tell the
                // client below instead of ending the stream as if it were
                // complete.
                Some(Err(e)) => { interrupted = Some(util::error_chain(&e)); break; }
                None => break,
            };
            idle.mark_now();
            if state.is_debug() { debug_raw.extend_from_slice(&chunk); }
            for line in lines.push(&chunk) {
                let Some(data) = util::sse_data(&line) else { continue };
                if data == "[DONE]" {
                    saw_done = true;
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
                    continue;
                }
                if data.is_empty() { continue; }
                match serde_json::from_str::<Value>(data) {
                    Ok(v) => {
                        if let Some(u) = v.get("usage") {
                            usage.merge_stream_update(util::openai_usage(u));
                            billed = util::copilot_billed_nano_aiu(&v).or(billed);
                            cache_saved = util::cache_saving_nano_aiu(&v, &usage).or(cache_saved);
                        }
                        resp_size += data.len();
                        yield Ok(Bytes::from(format!("data: {data}\n\n")));
                    }
                    // Forward payloads we cannot parse rather than dropping
                    // them: silently deleting a delta removes text from the
                    // middle of the answer with no trace.
                    Err(e) => {
                        tracing::warn!("[{endpoint}] forwarding unparsable SSE payload: {e}");
                        resp_size += data.len();
                        yield Ok(Bytes::from(format!("data: {data}\n\n")));
                    }
                }
            }
        }
        // A final event that arrived without its trailing newline is still a
        // real event; do not drop it.
        if let Some(line) = lines.flush() {
            if let Some(data) = util::sse_data(&line) {
                if data == "[DONE]" {
                    saw_done = true;
                    yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                } else if !data.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(u) = v.get("usage") {
                            usage.merge_stream_update(util::openai_usage(u));
                            billed = util::copilot_billed_nano_aiu(&v).or(billed);
                            cache_saved = util::cache_saving_nano_aiu(&v, &usage).or(cache_saved);
                        }
                    }
                    resp_size += data.len();
                    yield Ok(Bytes::from(format!("data: {data}\n\n")));
                }
            }
        }
        if let Some(ref reason) = interrupted {
            tracing::warn!("[{endpoint}] upstream stream interrupted: {reason}");
            let ev = json!({"error": {
                "message": format!("Upstream stream ended prematurely: {reason}. The response above is incomplete."),
                "type": "upstream_error",
                "code": "stream_interrupted"
            }});
            yield Ok(Bytes::from(format!("data: {ev}\n\n")));
        }
        if !saw_done {
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
        let _ = model;
        let debug_resp = String::from_utf8_lossy(&debug_raw).into_owned();
        log_debug_response(&state, endpoint, &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: endpoint.to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            // An interrupted stream is not a successful request, even though
            // the response headers were already a 200.
            status_code: if interrupted.is_some() { 502 } else { status },
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier: state.model_premium_multiplier(&translated).await,
            upstream_idle_max_ms: Some(idle.max_idle_ms_including_now()),
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None, // Streaming doesn't have access to parsed req
            tool_count: None,
            tool_names: None,
            stop_reason: interrupted.is_some().then(|| "stream_interrupted".to_string()),
            tools_called: None,
            is_agent_initiated: None,
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(calculate_cost(&translated, &usage)),
        });
    };
    build_sse_response(stream, COMMENT_KEEPALIVE_PROBE, probe_count.clone())
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
    state.record_quota_headers(upstream.headers());
    let status = upstream.status().as_u16();
    // A non-2xx upstream returns a JSON error body, not an SSE stream. Forward
    // it as a normal error response instead of wrapping it in a 200 "stream",
    // which would make clients treat an auth/quota failure as a valid answer.
    if !is_streamable_status(status) {
        let text = upstream.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1/responses", &text);
        log_error("/v1/responses", &req, &text, status);
        return passthrough_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        );
    }
    // Shared with the keepalive wrapper so the record can report how
    // many probes actually went out during an upstream silence.
    let probe_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut lines = util::SseLineBuffer::new();
        let mut usage = TokenUsage::default();
            // Copilot reports what it billed only on the terminal event.
            let mut billed: Option<u64> = None;
            let mut cache_saved: Option<i64> = None;
        let mut resp_size = 0usize;
        let mut debug_raw: Vec<u8> = Vec::new();
        let mut interrupted: Option<String> = None;
        let mut completed = false;
        let mut idle = util::IdleTracker::new(std::time::Instant::now());
        loop {
            let chunk = match byte_stream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => { interrupted = Some(util::error_chain(&e)); break; }
                None => break,
            };
            idle.mark_now();
            resp_size += chunk.len();
            // Verbatim passthrough of raw bytes. Bytes are never re-decoded on
            // this path, so a multi-byte character split across chunks reaches
            // the client intact.
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::copy_from_slice(&chunk));
            if state.is_debug() { debug_raw.extend_from_slice(&chunk); }
            for line in lines.push(&chunk) {
                let Some(data) = util::sse_data(&line) else { continue };
                if data == "[DONE]" || data.is_empty() { continue; }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("response.completed") {
                        completed = true;
                        let raw = v.get("response").and_then(|r| r.get("usage")).cloned().unwrap_or(json!({}));
                        usage = util::responses_usage(&raw);
                        billed = util::copilot_billed_nano_aiu(&v).or(billed);
                        cache_saved = util::cache_saving_nano_aiu(&v, &usage).or(cache_saved);
                    }
                }
            }
        }
        // The Responses protocol terminates with `response.completed`. If it
        // never arrived the answer is partial, so emit an explicit `error`
        // event rather than letting the client treat the truncated output as a
        // finished turn.
        if interrupted.is_some() || !completed {
            let reason = interrupted
                .clone()
                .unwrap_or_else(|| "upstream closed the stream before response.completed".to_string());
            tracing::warn!("[/v1/responses] incomplete stream: {reason}");
            let ev = json!({
                "type": "error",
                "code": "stream_interrupted",
                "message": format!("Upstream stream ended prematurely: {reason}. The response above is incomplete."),
                "param": Value::Null,
                "sequence_number": Value::Null
            });
            yield Ok(Bytes::from(format!("event: error\ndata: {ev}\n\n")));
        }
        let debug_resp = String::from_utf8_lossy(&debug_raw).into_owned();
        log_debug_response(&state, "/v1/responses", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/responses".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: if interrupted.is_some() { 502 } else { status },
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier: state.model_premium_multiplier(&translated).await,
            upstream_idle_max_ms: Some(idle.max_idle_ms_including_now()),
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: (interrupted.is_some() || !completed)
                .then(|| "stream_interrupted".to_string()),
            tools_called: None,
            is_agent_initiated: None,
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(calculate_cost(&translated, &usage)),
        });
    };
    build_sse_response(stream, COMMENT_KEEPALIVE_PROBE, probe_count.clone())
}
/// Streams a direct Anthropic SSE response back to the client verbatim.
/// Identity and bookkeeping for one request, threaded unchanged into whichever
/// record it ends up producing.
struct RequestMeta {
    original_model: String,
    translated: String,
    req_size: usize,
    req_body: Option<String>,
    session_id: Option<String>,
    start: Instant,
}

async fn stream_anthropic_direct(
    state: SharedState,
    upstream: reqwest::Response,
    meta: RequestMeta,
) -> Response {
    let RequestMeta {
        original_model,
        translated,
        req_size,
        req_body,
        session_id,
        start,
    } = meta;
    state.record_quota_headers(upstream.headers());
    let status = upstream.status().as_u16();
    // Without this the error body is wrapped in a 200 `text/event-stream`, so
    // the client waits on a stream that never produces an event and reports a
    // stalled response instead of the rate limit or auth failure that actually
    // happened. The other three streaming paths already gate on this.
    if !is_streamable_status(status) {
        let text = upstream.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1/messages", &text);
        log_error(
            "/v1/messages",
            &json!({"model": &translated}),
            &text,
            status,
        );
        record_failure(
            &state,
            "/v1/messages",
            &translated,
            Some(&translated),
            status,
            crate::store::failure::UPSTREAM_STATUS,
            req_size,
            req_body.clone(),
            Some(text.clone()),
            start,
            session_id.clone(),
        );
        return anthropic_passthrough_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        );
    }
    // Resolved before the stream starts: the recorder's Drop path is
    // synchronous and cannot await.
    let premium_multiplier = state.model_premium_multiplier(&translated).await;
    // Shared with the keepalive wrapper so the record can report how
    // many probes actually went out during an upstream silence.
    let probe_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut recorder = StreamRecorder::new(
        state.clone(),
        "/v1/messages",
        original_model,
        translated,
        req_size,
        req_body,
        premium_multiplier,
        start,
        probe_count.clone(),
        session_id.clone(),
    );
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut lines = util::SseLineBuffer::new();
        let mut interrupted: Option<String> = None;
        let mut st = DirectStreamState::default();
        loop {
            let chunk = match byte_stream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => { interrupted = Some(util::error_chain(&e)); break; }
                None => break,
            };
            recorder.idle.mark_now();
            recorder.resp_size += chunk.len();
            // Verbatim passthrough: the client receives the exact upstream
            // bytes, so characters split across chunks are never mangled.
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::copy_from_slice(&chunk));
            if state.is_debug() { recorder.debug_raw.extend_from_slice(&chunk); }
            for line in lines.push(&chunk) {
                let Some(data) = util::sse_data(&line) else { continue };
                if data.is_empty() { continue; }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                st.observe(&v);
                // Mirrored onto the recorder because a client disconnect drops
                // this generator: `Drop` can only report what the recorder
                // itself holds.
                recorder.usage = st.usage;
                recorder.billed_nano_aiu = st.billed_nano_aiu;
                recorder.cache_saved_nano_aiu = st.cache_saved_nano_aiu;
                recorder.usage_final = st.usage_final;
            }
        }
        // Anthropic clients wait for `message_stop`. Without it they either
        // hang or record the partial text as a completed assistant turn, which
        // then poisons every following request in the conversation.
        if interrupted.is_some() || !st.saw_message_stop {
            let reason = interrupted
                .clone()
                .unwrap_or_else(|| "upstream closed the stream before message_stop".to_string());
            tracing::warn!("[/v1/messages] incomplete stream: {reason}");
            let err = json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": format!("Upstream stream ended prematurely: {reason}. The response above is incomplete.")
                }
            });
            yield Ok(Bytes::from(format!("event: error\ndata: {err}\n\n")));
            st.stop_reason = Some("stream_interrupted".to_string());
        }
        let debug_resp = String::from_utf8_lossy(&recorder.debug_raw).into_owned();
        log_debug_response(&state, "/v1/messages", &debug_resp);
        if let Some(r) = recorder.record_mut() {
            r.tools_called = (!st.tools_called.is_empty()).then_some(st.tools_called);
        }
        recorder.finalize(
            if interrupted.is_some() { 502 } else { status },
            (interrupted.is_some() || !st.saw_message_stop)
                .then_some(crate::store::failure::STREAM_INTERRUPTED),
            st.stop_reason,
            state.is_debug().then_some(debug_resp),
        );
    };
    build_sse_response(stream, ANTHROPIC_KEEPALIVE_PROBE, probe_count.clone())
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
    session_id: Option<String>,
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
    state.record_quota_headers(upstream.headers());
    let status = upstream.status().as_u16();
    // Surface a non-2xx upstream (JSON error, not SSE) as a normal error.
    if !is_streamable_status(status) {
        let text = upstream.text().await.unwrap_or_default();
        log_debug_response(&state, "/v1/messages", &text);
        log_error(
            "/v1/messages",
            &json!({"model": &translated}),
            &text,
            status,
        );
        record_failure(
            &state,
            "/v1/messages",
            &translated,
            Some(&translated),
            status,
            crate::store::failure::UPSTREAM_STATUS,
            req_size,
            req_body.clone(),
            Some(text.clone()),
            start,
            session_id.clone(),
        );
        return anthropic_passthrough_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        );
    }
    // Shared with the keepalive wrapper so the record can report how
    // many probes actually went out during an upstream silence.
    let probe_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut lines = util::SseLineBuffer::new();
        let mut conv = AnthropicStreamState::new();
        let mut chunks: Vec<Value> = Vec::new();
        let mut usage = TokenUsage::default();
            // Copilot reports what it billed only on the terminal event.
            let mut billed: Option<u64> = None;
            let mut cache_saved: Option<i64> = None;
        let mut resp_size = 0usize;
        let mut debug_raw: Vec<u8> = Vec::new();
        let mut interrupted: Option<String> = None;
        let mut idle = util::IdleTracker::new(std::time::Instant::now());
        loop {
            let chunk = match byte_stream.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => { interrupted = Some(util::error_chain(&e)); break; }
                None => break,
            };
            idle.mark_now();
            if state.is_debug() { debug_raw.extend_from_slice(&chunk); }
            for line in lines.push(&chunk) {
                let Some(data) = util::sse_data(&line) else { continue };
                if data == "[DONE]" || data.is_empty() { continue; }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    tracing::warn!("[/v1/messages] skipping unparsable upstream SSE payload");
                    continue;
                };
                if let Some(u) = v.get("usage") {
                    usage.merge_stream_update(util::openai_usage(u));
                            billed = util::copilot_billed_nano_aiu(&v).or(billed);
                            cache_saved = util::cache_saving_nano_aiu(&v, &usage).or(cache_saved);
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
        // The upstream may stop without ever sending a `finish_reason`, in
        // which case no `message_stop` was emitted and an Anthropic client
        // would wait forever — or, worse, keep the partial text as a finished
        // assistant turn. Always close the event sequence explicitly.
        if let Some(ref reason) = interrupted {
            tracing::warn!("[/v1/messages] upstream stream interrupted: {reason}");
        }
        let stop_reason = if interrupted.is_some() { "error" } else { "end_turn" };
        let mut synthesized_stop = false;
        for event in conv.finish(stop_reason) {
            synthesized_stop = true;
            let ev_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("message");
            let payload = format!("event: {ev_type}\ndata: {}\n\n", serde_json::to_string(&event).unwrap_or_default());
            resp_size += payload.len();
            yield Ok(Bytes::from(payload));
        }
        if let Some(ref reason) = interrupted {
            let err = json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": format!("Upstream stream ended prematurely: {reason}. The response above is incomplete.")
                }
            });
            yield Ok(Bytes::from(format!("event: error\ndata: {err}\n\n")));
        }
        // Fall back to merged usage if streaming chunks did not carry usage.
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            let merged = anthropic::merge_chat_chunks(&chunks);
            if let Some(raw) = merged.get("usage") {
                usage = util::openai_usage(raw);
            }
        }
        let debug_resp = String::from_utf8_lossy(&debug_raw).into_owned();
        log_debug_response(&state, "/v1/messages", &debug_resp);
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/messages".to_string(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated.clone()),
            status_code: if interrupted.is_some() { 502 } else { status },
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            output_tokens_final: None,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,            reasoning_tokens: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),            billed_nano_aiu: billed,
            cache_saved_nano_aiu: cache_saved,
            premium_multiplier: state.model_premium_multiplier(&translated).await,
            upstream_idle_max_ms: Some(idle.max_idle_ms_including_now()),
            keepalive_probes: None,
            duration: elapsed_secs(start),
            request_body: req_body,
            response_body: if state.is_debug() { Some(debug_resp) } else { None },
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: synthesized_stop.then(|| stop_reason.to_string()),
            tools_called: None,
            is_agent_initiated: None,
            session_id: None,
            prompt_cache_hit: cache_disposition(&usage),
            failure_kind: None,
            estimated_cost_usd: Some(calculate_cost(&translated, &usage)),
        });
    };
    build_sse_response(stream, ANTHROPIC_KEEPALIVE_PROBE, probe_count.clone())
}

/// How long a stream may stay silent before a keepalive comment is emitted.
///
/// GitHub's upstream load balancer, and most intermediaries, drop an idle
/// connection at around 60 seconds. A model doing extended thinking can easily
/// produce no tokens for longer than that, which surfaces to the user as
/// `user_request_timeout` or a stream that simply dies mid-answer.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Wraps an SSE stream so a comment line is emitted whenever it goes quiet.
///
/// `: keepalive` is an SSE comment: the spec requires clients to ignore it, so
/// it holds the connection open without being visible to the consumer. It is
/// only injected at an event boundary — after bytes ending in a blank line, or
/// before anything has been sent — because the verbatim passthrough paths yield
/// raw upstream chunks and splicing a comment into a half-written event would
/// corrupt it.
/// Keepalive probe for the Anthropic Messages protocol.
///
/// Anthropic's own streaming API emits `ping` events during long silences, and
/// clients reset their idle watchdog on *events*. An SSE comment is discarded
/// by the parser per spec, so it keeps the TCP connection and intermediaries
/// alive while never reaching the application — which is how a connection that
/// is provably still flowing bytes gets aborted as `Response stalled
/// mid-stream`. Measured against the real upstream: GitHub Copilot's Anthropic
/// endpoint never emits a ping of its own, so this probe is the only keepalive
/// signal on the link.
const ANTHROPIC_KEEPALIVE_PROBE: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";

/// Keepalive probe for protocols that have no ping event of their own (OpenAI
/// chat completions, OpenAI Responses, Gemini). A comment is the only thing
/// guaranteed not to be mistaken for content by those parsers.
const COMMENT_KEEPALIVE_PROBE: &[u8] = b": keepalive\n\n";

fn keepalive<S>(
    stream: S,
    probe: &'static [u8],
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::convert::Infallible>>
where
    S: futures_util::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static,
{
    keepalive_with_interval(stream, SSE_KEEPALIVE_INTERVAL, probe, counter)
}

/// Byte offset just past the last **complete** SSE event in `buf`, or `None`
/// when `buf` does not yet contain a whole event.
fn last_event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).rposition(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buf
        .windows(4)
        .rposition(|w| w == b"\r\n\r\n")
        .map(|i| i + 4);
    lf.max(crlf)
}

/// Keepalive with an injectable interval so the stall behavior is testable
/// without waiting out the production interval.
///
/// Upstream chunks are re-aligned to event boundaries: bytes that do not yet
/// complete an event are held back rather than forwarded. That is what makes
/// the keepalive unconditional — everything already sent downstream ends on a
/// blank line, so a comment can never land inside a half-written event.
///
/// The previous implementation forwarded chunks verbatim and only emitted a
/// keepalive when the *last chunk* happened to end on a boundary. A TCP split
/// mid-event left that flag stuck `false` — and since only a new chunk could
/// clear it, an upstream that went quiet right then silenced the keepalive for
/// as long as it stayed quiet. The client saw zero bytes and aborted the
/// stream, which surfaced as `Response stalled mid-stream`.
fn keepalive_with_interval<S>(
    stream: S,
    interval: Duration,
    probe: &'static [u8],
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::convert::Infallible>>
where
    S: futures_util::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static,
{
    async_stream::stream! {
        use futures_util::StreamExt;
        let mut inner = Box::pin(stream);
        // Upstream bytes not yet forming a complete event.
        let mut partial: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                biased;
                item = inner.next() => {
                    match item {
                        Some(Ok(bytes)) => {
                            partial.extend_from_slice(&bytes);
                            if let Some(cut) = last_event_boundary(&partial) {
                                let whole: Vec<u8> = partial.drain(..cut).collect();
                                yield Ok(Bytes::from(whole));
                            }
                        }
                        Some(Err(e)) => yield Err(e),
                        None => break,
                    }
                }
                _ = tokio::time::sleep(interval) => {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    yield Ok(Bytes::from_static(probe));
                }
            }
        }
        // A final event that never received its blank line is still real data.
        if !partial.is_empty() {
            yield Ok(Bytes::from(partial));
        }
    }
}

/// Wraps an SSE stream in a response, injecting `probe` during upstream
/// silences. The probe is protocol-specific — see the two `*_KEEPALIVE_PROBE`
/// constants; sending the wrong one either breaks the client's parser or fails
/// to reset its idle watchdog.
fn build_sse_response<S>(
    stream: S,
    probe: &'static [u8],
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static,
{
    let body = Body::from_stream(keepalive(stream, probe, counter));
    let mut resp = Response::new(body);
    *resp.headers_mut() = sse_headers();
    resp
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async fn dashboard() -> Response {
    serve_asset(
        include_str!("../public/dashboard.html"),
        "text/html; charset=utf-8",
    )
}

/// Design system shared by the three dashboard pages. Served separately rather
/// than inlined into each page so the pages cannot drift apart visually.
async fn stylesheet() -> Response {
    serve_asset(include_str!("../public/app.css"), "text/css; charset=utf-8")
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
            "/v1/models/{model}": {
                "get": {
                    "summary": "Retrieve a single model",
                    "parameters": [{ "name": "model", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": { "description": "Model description" },
                        "404": { "description": "Unknown model" }
                    }
                }
            },
            "/health": {
                "get": {
                    "summary": "Liveness/readiness probe",
                    "parameters": [{ "name": "strict", "in": "query", "required": false, "schema": { "type": "boolean" }, "description": "Return 503 instead of 200 when the proxy is not ready." }],
                    "responses": {
                        "200": { "description": "Health report" },
                        "503": { "description": "Not ready (only with strict=true)" }
                    }
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
    serve_asset(
        include_str!("../public/requests.html"),
        "text/html; charset=utf-8",
    )
}

async fn metrics_page() -> Response {
    serve_asset(
        include_str!("../public/metrics.html"),
        "text/html; charset=utf-8",
    )
}

fn serve_asset(contents: &'static str, content_type: &'static str) -> Response {
    let mut resp = Response::new(Body::from(contents));
    resp.headers_mut()
        .insert("Content-Type", HeaderValue::from_static(content_type));
    resp
}

fn metrics_label_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn metrics_openmetrics(State(state): State<SharedState>) -> Response {
    let stats = state.store.stats();

    let mut req_total_by_labels: HashMap<(String, u16, String), u64> = HashMap::new();
    let mut in_tokens_by_model: HashMap<String, u64> = HashMap::new();
    let mut out_tokens_by_model: HashMap<String, u64> = HashMap::new();
    let mut cache_read_by_model: HashMap<String, u64> = HashMap::new();
    let mut cache_creation_by_model: HashMap<String, u64> = HashMap::new();
    let mut premium_by_model: HashMap<String, f64> = HashMap::new();
    let mut duration_sum_by_endpoint: HashMap<String, f64> = HashMap::new();
    let mut duration_count_by_endpoint: HashMap<String, u64> = HashMap::new();
    let mut cost_total = 0.0_f64;
    let mut record_count = 0usize;

    state.store.with_records(|records| {
        for rec in records {
            record_count += 1;
            let model = rec
                .translated_model
                .as_ref()
                .unwrap_or(&rec.model)
                .to_string();
            *req_total_by_labels
                .entry((rec.endpoint.clone(), rec.status_code, model.clone()))
                .or_insert(0) += 1;
            *in_tokens_by_model.entry(model.clone()).or_insert(0) += rec.input_tokens;
            *cache_read_by_model.entry(model.clone()).or_insert(0) += rec.cache_read_input_tokens;
            *cache_creation_by_model.entry(model.clone()).or_insert(0) +=
                rec.cache_creation_input_tokens;
            if let Some(multiplier) = rec.premium_multiplier {
                *premium_by_model.entry(model.clone()).or_insert(0.0) += multiplier;
            }
            *out_tokens_by_model.entry(model).or_insert(0) += rec.output_tokens;
            *duration_sum_by_endpoint
                .entry(rec.endpoint.clone())
                .or_insert(0.0) += rec.duration;
            *duration_count_by_endpoint
                .entry(rec.endpoint.clone())
                .or_insert(0) += 1;
            cost_total += rec.estimated_cost_usd.unwrap_or(0.0);
        }
    });

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
        "# HELP ghc_proxy_cache_read_tokens_total Input tokens served from the prompt cache.\n",
    );
    out.push_str("# TYPE ghc_proxy_cache_read_tokens_total counter\n");
    for (model, total) in cache_read_by_model {
        out.push_str(&format!(
            "ghc_proxy_cache_read_tokens_total{{model=\"{}\"}} {}\n",
            metrics_label_escape(&model),
            total
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_cache_creation_tokens_total Input tokens written into the prompt cache.\n",
    );
    out.push_str("# TYPE ghc_proxy_cache_creation_tokens_total counter\n");
    for (model, total) in cache_creation_by_model {
        out.push_str(&format!(
            "ghc_proxy_cache_creation_tokens_total{{model=\"{}\"}} {}\n",
            metrics_label_escape(&model),
            total
        ));
    }

    out.push_str(
        "# HELP ghc_proxy_premium_requests_total Copilot premium requests consumed, per the catalog's billing multiplier.\n",
    );
    out.push_str("# TYPE ghc_proxy_premium_requests_total counter\n");
    for (model, total) in premium_by_model {
        out.push_str(&format!(
            "ghc_proxy_premium_requests_total{{model=\"{}\"}} {:.4}\n",
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
    out.push_str(&format!("ghc_proxy_store_records {}\n", record_count));

    out.push_str("# HELP ghc_proxy_uptime_seconds Seconds since the proxy started.\n");
    out.push_str("# TYPE ghc_proxy_uptime_seconds gauge\n");
    out.push_str(&format!(
        "ghc_proxy_uptime_seconds {}\n",
        state.uptime_secs()
    ));

    // Quota comes from headers the upstream attaches to every response, so
    // scraping this endpoint never costs an extra API call.
    let quotas = state.quota_snapshot();
    if !quotas.is_empty() {
        out.push_str(
            "# HELP ghc_proxy_quota_percent_remaining Percent of the entitlement still available.\n",
        );
        out.push_str("# TYPE ghc_proxy_quota_percent_remaining gauge\n");
        for (sku, q) in &quotas {
            out.push_str(&format!(
                "ghc_proxy_quota_percent_remaining{{sku=\"{}\"}} {}\n",
                metrics_label_escape(sku),
                q.percent_remaining
            ));
        }

        out.push_str(
            "# HELP ghc_proxy_quota_entitlement Allowance for the period; negative means unlimited.\n",
        );
        out.push_str("# TYPE ghc_proxy_quota_entitlement gauge\n");
        for (sku, q) in &quotas {
            out.push_str(&format!(
                "ghc_proxy_quota_entitlement{{sku=\"{}\"}} {}\n",
                metrics_label_escape(sku),
                q.entitlement
            ));
        }

        out.push_str("# HELP ghc_proxy_quota_overage Amount consumed beyond the entitlement.\n");
        out.push_str("# TYPE ghc_proxy_quota_overage gauge\n");
        for (sku, q) in &quotas {
            out.push_str(&format!(
                "ghc_proxy_quota_overage{{sku=\"{}\"}} {}\n",
                metrics_label_escape(sku),
                q.overage
            ));
        }
    }

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

/// Turns request/response body capture on or off without a restart.
///
/// Capturing bodies is the only way to see what a client actually sent, but it
/// used to require stopping the proxy and relaunching it with `--debug` — by
/// which point the request you wanted to inspect is long gone. The flag is read
/// live on every request, so flipping it here applies from the next call.
///
/// Deliberately not written back to `config.yaml`: capture puts prompts, tool
/// output and any credentials they carry into memory and the log, so it should
/// lapse on restart rather than stay on because someone forgot.
async fn api_set_debug(State(state): State<SharedState>, body: Option<Json<Value>>) -> Response {
    let requested = body
        .as_ref()
        .and_then(|Json(v)| v.get("debug"))
        .and_then(Value::as_bool);
    let Some(debug) = requested else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "expected a JSON body of {\"debug\": true} or {\"debug\": false}"
            })),
        )
            .into_response();
    };
    state.set_debug(debug);
    let action = if debug { "enabled" } else { "disabled" };
    tracing::info!("[debug] body capture {action} from the dashboard");
    Json(json!({ "ok": true, "debug": debug })).into_response()
}

async fn api_stats(State(state): State<SharedState>) -> Response {
    Json(state.store.stats()).into_response()
}

/// Running totals for one model's prompt-cache behaviour.
#[derive(Default)]
struct CacheAgg {
    requests: u64,
    input: u64,
    read: u64,
    write: u64,
    /// Net effect on the bill, in nano-AI-units, from the model's own rates.
    saved_nano_aiu: i64,
    /// Whether any response for this model reported rates to compute the above
    /// from. Without it a genuine zero — nothing was cached, so nothing was
    /// saved — is indistinguishable from an unpriced model.
    priced: bool,
}

/// Prompt-cache statistics.
///
/// The hit rate is the early warning for a broken prompt prefix: on an agent
/// workload it should sit high and stable, and a sudden drop means the prompt
/// stopped matching and every turn is paying full input price again. Reporting
/// it per model is what makes that actionable — a single global number cannot
/// tell you *which* conversation broke.
///
/// Totals come from the all-time running counters. The per-model breakdown is
/// derived from the retained ring buffer, so it describes the most recent
/// `sampled_requests` calls rather than every one ever served; the response
/// says so explicitly rather than letting the two silently disagree.
async fn api_cache(State(state): State<SharedState>) -> Response {
    let stats = state.store.stats();

    let (by_model, hit, write_only, uncached, sampled) = state.store.with_records(|records| {
        let mut by_model: HashMap<String, CacheAgg> = HashMap::new();
        let (mut hit, mut write_only, mut uncached, mut sampled) = (0u64, 0u64, 0u64, 0u64);

        for r in records {
            // A rejected attempt has no tokens and no model worth a row; counting
            // it would add an empty-named entry and drag every disposition
            // toward "uncached".
            if r.failed() {
                continue;
            }
            sampled += 1;
            match r.prompt_cache_hit {
                Some(true) => hit += 1,
                Some(false) => write_only += 1,
                None => uncached += 1,
            }

            // Price against the model that actually served the request, not the
            // alias the client asked for.
            let model = r.translated_model.as_deref().unwrap_or(&r.model);
            let agg = by_model.entry(model.to_string()).or_default();
            agg.requests += 1;
            agg.input += r.input_tokens;
            agg.read += r.cache_read_input_tokens;
            agg.write += r.cache_creation_input_tokens;
            // Derived upstream from the rates that response reported, so a
            // model Copilot includes at no charge contributes nothing rather
            // than an imagined saving.
            agg.saved_nano_aiu += r.cache_saved_nano_aiu.unwrap_or(0);
            agg.priced |= r.cache_saved_nano_aiu.is_some();
        }
        (by_model, hit, write_only, uncached, sampled)
    });

    let mut models: Vec<Value> = by_model
        .into_iter()
        .map(|(model, a)| {
            json!({
                "model": model,
                "requests": a.requests,
                "input_tokens": a.input,
                "cache_read_tokens": a.read,
                "cache_creation_tokens": a.write,
                "fresh_tokens": a.input.saturating_sub(a.read + a.write),
                "hit_rate": if a.input > 0 { a.read as f64 / a.input as f64 } else { 0.0 },
                "saved_nano_aiu": a.priced.then_some(a.saved_nano_aiu),
            })
        })
        .collect();
    // Biggest prompt first: that is where a broken prefix costs the most.
    models.sort_by(|a, b| {
        let key = |v: &Value| v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        key(b).cmp(&key(a))
    });

    let total_in = stats.total_input_tokens;
    let cached = stats.total_cache_read_tokens + stats.total_cache_creation_tokens;
    Json(json!({
        "totals": {
            "input_tokens": total_in,
            "cache_read_tokens": stats.total_cache_read_tokens,
            "cache_creation_tokens": stats.total_cache_creation_tokens,
            "fresh_tokens": total_in.saturating_sub(cached),
            "hit_rate": if total_in > 0 {
                stats.total_cache_read_tokens as f64 / total_in as f64
            } else { 0.0 },
            "request_count": stats.request_count,
        },
        "dispositions": {
            "served_from_cache": hit,
            "wrote_to_cache": write_only,
            "no_cache": uncached,
        },
        "sampled_requests": sampled,
        "by_model": models,
    }))
    .into_response()
}

/// Largest page size a dashboard API will honour. Keeps a hostile `per_page`
/// from forcing an unbounded clone of the request store.
const MAX_PAGE_SIZE: usize = 500;

/// Parses `page`/`per_page` query parameters into a `(page, per_page, offset)`
/// triple. `per_page` is clamped to `MAX_PAGE_SIZE` and the offset uses
/// saturating arithmetic so extreme values cannot overflow.
fn parse_pagination(params: &HashMap<String, String>) -> (usize, usize, usize) {
    let page = params
        .get("page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let per_page = params
        .get("per_page")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, MAX_PAGE_SIZE);
    let offset = page.saturating_sub(1).saturating_mul(per_page);
    (page, per_page, offset)
}

async fn api_requests(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (page, per_page, offset) = parse_pagination(&params);
    // `failed=true` narrows to requests that never produced a usable answer —
    // the ones worth looking at when something went wrong.
    let failed_only = params.get("failed").map(|v| v == "true").unwrap_or(false);
    // Several clients share one proxy, so isolating a single session is the
    // difference between reading a log and guessing at one.
    let session = params.get("session").map(|s| s.as_str());
    let (items, total) = if failed_only || session.is_some() {
        state.store.filtered_page(per_page, offset, |r| {
            if failed_only && !r.failed() {
                return false;
            }
            match session {
                Some(want) => r
                    .session_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(want)),
                None => true,
            }
        })
    } else {
        state.store.recent(per_page, offset)
    };
    let total_pages = total.div_ceil(per_page);
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
/// Query params: endpoint=, status=, tool_name=, agent=true|false, model=, page=, per_page=
async fn api_audit(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (page, per_page, offset) = parse_pagination(&params);
    let endpoint_filter = params.get("endpoint").map(|s| s.as_str());
    let status_filter = params.get("status").and_then(|s| s.parse::<u16>().ok());
    let tool_filter = params.get("tool_name").map(|s| s.as_str());
    let agent_filter = params.get("agent").and_then(|s| s.parse::<bool>().ok());
    let model_filter = params.get("model").map(|s| s.as_str());

    // Filter under the store lock so only the returned page is cloned.
    let (items, filtered_total) = state.store.filtered_page(per_page, offset, |rec| {
        if let Some(ep) = endpoint_filter {
            if !rec.endpoint.contains(ep) {
                return false;
            }
        }
        if let Some(st) = status_filter {
            if rec.status_code != st {
                return false;
            }
        }
        if let Some(tool) = tool_filter {
            match rec.tool_names {
                Some(ref tools) if tools.iter().any(|t| t.contains(tool)) => {}
                _ => return false,
            }
        }
        if let Some(is_agent) = agent_filter {
            if rec.is_agent_initiated != Some(is_agent) {
                return false;
            }
        }
        if let Some(model) = model_filter {
            let served = rec.translated_model.as_deref().unwrap_or(&rec.model);
            if !rec.model.contains(model) && !served.contains(model) {
                return false;
            }
        }
        true
    });

    let total_pages = filtered_total.div_ceil(per_page);

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
    let mut tool_usage: HashMap<String, usize> = HashMap::new();
    let mut stop_reason_counts: HashMap<String, usize> = HashMap::new();
    let mut total_cost = 0.0;
    let mut agent_count = 0usize;
    let mut cache_hit_count = 0usize;
    let mut cache_write_count = 0usize;
    let mut record_count = 0usize;
    let mut total_input_tokens = 0u64;
    let mut total_cache_read_tokens = 0u64;
    let mut total_cache_creation_tokens = 0u64;
    let mut premium_requests = 0.0_f64;

    state.store.with_records(|records| {
        for rec in records {
            record_count += 1;
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

            // Token-level cache accounting. The per-request counters above
            // only say how many turns touched the cache; these say how much of
            // the prompt volume it actually absorbed.
            total_input_tokens += rec.input_tokens;
            total_cache_read_tokens += rec.cache_read_input_tokens;
            total_cache_creation_tokens += rec.cache_creation_input_tokens;
            if let Some(multiplier) = rec.premium_multiplier {
                premium_requests += multiplier;
            }
        }
    });

    // Sort tools by usage
    let mut tools_sorted: Vec<_> = tool_usage.into_iter().collect();
    tools_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let top_tools: Vec<_> = tools_sorted.into_iter().take(20).collect();

    // Sort stop reasons by count
    let mut stop_reasons_sorted: Vec<_> = stop_reason_counts.into_iter().collect();
    stop_reasons_sorted.sort_by_key(|b| std::cmp::Reverse(b.1));

    Json(json!({
        "total_requests": record_count,
        "agent_initiated": agent_count,
        "total_cost_usd": (total_cost * 10_000.0).round() / 10_000.0,
        "avg_cost_usd": if record_count > 0 { (total_cost / record_count as f64 * 10_000.0).round() / 10_000.0 } else { 0.0 },
        "top_tools": top_tools,
        "stop_reasons": stop_reasons_sorted,
        "cache_hits": cache_hit_count,
        "cache_writes": cache_write_count,
        // Share of requests that read from cache — a per-request count, not a
        // token volume. Both rates below are 0–1 fractions.
        "cache_hit_rate": ratio_2dp(
            cache_hit_count as f64,
            (cache_hit_count + cache_write_count) as f64,
        ),
        "total_input_tokens": total_input_tokens,
        "total_cache_read_tokens": total_cache_read_tokens,
        "total_cache_creation_tokens": total_cache_creation_tokens,
        // Share of total prompt volume served from cache, which is what
        // actually drives cost and latency — unlike the per-request rate above.
        "token_cache_hit_rate": ratio_2dp(
            total_cache_read_tokens as f64,
            total_input_tokens as f64,
        ),
        "premium_requests": (premium_requests * 100.0).round() / 100.0,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn output_tokens_stay_provisional_until_message_delta() {
        let mut st = super::DirectStreamState::default();
        // `message_start` opens with a placeholder output count. A stream cut
        // short here (client stall abort) would otherwise publish "3" as the
        // output of a turn that went on to emit 35899 — the exact numbers from
        // the stalled Write call this guard exists for.
        st.observe(&serde_json::json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 2,
                "cache_read_input_tokens": 342437,
                "cache_creation_input_tokens": 6044,
                "output_tokens": 3
            }}
        }));
        assert!(!st.usage_final);
        assert_eq!(st.usage.output_tokens, 3);
        assert_eq!(st.usage.input_tokens, 2 + 342437 + 6044);

        st.observe(&serde_json::json!({
            "type": "message_delta",
            "usage": {"output_tokens": 35899},
            "delta": {"stop_reason": "tool_use"}
        }));
        assert!(st.usage_final);
        assert_eq!(st.usage.output_tokens, 35899);
        // `message_delta` restates output only; the input buckets captured at
        // `message_start` must survive it.
        assert_eq!(st.usage.input_tokens, 2 + 342437 + 6044);
        assert_eq!(st.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn tool_names_are_collected_once_each() {
        let mut st = super::DirectStreamState::default();
        for _ in 0..2 {
            st.observe(&serde_json::json!({
                "type": "content_block_start",
                "content_block": {"type": "tool_use", "name": "Write"}
            }));
        }
        st.observe(&serde_json::json!({
            "type": "content_block_start",
            "content_block": {"type": "text"}
        }));
        assert_eq!(st.tools_called, vec!["Write"]);
        assert!(!st.saw_message_stop);
        st.observe(&serde_json::json!({"type": "message_stop"}));
        assert!(st.saw_message_stop);
    }

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

    /// Read-only dashboard APIs stay open so local monitoring works without a
    /// key, but anything that mutates the running process must not. Turning on
    /// body capture writes whatever the client sent — credentials included —
    /// into the request log, so it is guarded like an LLM endpoint.
    #[test]
    fn protected_paths_cover_config_mutations() {
        assert!(is_protected_path("/api/config/debug"));
        assert!(is_protected_path("/api/config/reload"));
        assert!(!is_protected_path("/api/cache"));
        assert!(!is_protected_path("/api/requests"));
        assert!(!is_protected_path("/api/audit/summary"));
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

    #[test]
    fn health_path_is_not_protected() {
        assert!(!is_protected_path("/health"));
        assert!(!is_protected_path("/openapi.json"));
        // Single-model retrieval is an LLM endpoint and must stay guarded.
        assert!(is_protected_path("/v1/models/claude-opus-4.8"));
    }

    #[test]
    fn model_rates_prefer_the_most_specific_family() {
        // `gpt-4o` must not be swallowed by the broader `gpt-4` arm.
        assert_eq!(model_rates("gpt-4o"), (0.005, 0.015));
        assert_eq!(model_rates("gpt-4-turbo"), (0.03, 0.06));
        assert_eq!(model_rates("gpt-4o-mini"), (0.00015, 0.0006));
        // Publisher-qualified GitHub Models ids price like their base model.
        assert_eq!(model_rates("openai/gpt-4o"), model_rates("gpt-4o"));
        // Claude tiers are distinct.
        assert_eq!(model_rates("claude-opus-4.8"), (0.015, 0.075));
        assert_eq!(model_rates("claude-haiku-4.5"), (0.0008, 0.004));
        // Unknown models fall back rather than panicking.
        assert_eq!(model_rates("something-new"), (0.0005, 0.0015));
    }

    /// Minimal state for exercising the recording helpers.
    fn test_state() -> SharedState {
        std::sync::Arc::new(crate::state::AppState::new(
            crate::config::Config::default(),
            "test-token".into(),
        ))
    }

    #[test]
    fn record_failure_captures_a_request_that_never_produced_a_response() {
        let state = test_state();
        record_failure(
            &state,
            "/v1/messages",
            "claude-opus-5",
            Some("claude-opus-5"),
            429,
            crate::store::failure::UPSTREAM_STATUS,
            1234,
            None,
            Some(r#"{"error":{"message":"rate limit"}}"#.into()),
            Instant::now(),
            Some("test-session".into()),
        );
        let (items, total) = state.store.recent(10, 0);
        assert_eq!(total, 1, "a failed request must still be recorded");
        let rec = &items[0];
        assert_eq!(rec.status_code, 429);
        assert_eq!(rec.failure_kind.as_deref(), Some("upstream_status"));
        assert_eq!(rec.request_size, 1234);
        // The error body is the whole point of the record, so it is kept even
        // with debug off.
        assert!(rec.response_body.as_deref().unwrap().contains("rate limit"));
    }

    #[test]
    fn stream_recorder_records_a_client_disconnect_when_dropped_early() {
        let state = test_state();
        {
            let mut rec = StreamRecorder::new(
                state.clone(),
                "/v1/messages",
                "claude-opus-5".into(),
                "claude-opus-5".into(),
                500,
                None,
                None,
                Instant::now(),
                std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                Some("test-session".into()),
            );
            rec.resp_size = 4096;
            rec.usage.output_tokens = 77;
            rec.debug_raw
                .extend_from_slice(b"event: message_start\ndata: {}\n\n");
            // Deliberately no finalize() — this models axum dropping the
            // response body when the client hangs up mid-stream.
        }
        let (items, total) = state.store.recent(10, 0);
        assert_eq!(total, 1, "a client disconnect must leave a record behind");
        let rec = &items[0];
        assert_eq!(rec.status_code, 499);
        assert_eq!(rec.failure_kind.as_deref(), Some("client_disconnected"));
        // Whatever had streamed so far is preserved, not zeroed.
        assert_eq!(rec.response_size, 4096);
        assert_eq!(rec.output_tokens, 77);
        // The partial upstream body is the main evidence for why the client
        // gave up, so it must survive the disconnect too.
        assert!(
            rec.response_body
                .as_deref()
                .unwrap_or_default()
                .contains("message_start"),
            "partial response body lost on client disconnect"
        );
    }

    #[test]
    fn stream_recorder_does_not_double_record_after_finalize() {
        let state = test_state();
        {
            let mut rec = StreamRecorder::new(
                state.clone(),
                "/v1/messages",
                "claude-opus-5".into(),
                "claude-opus-5".into(),
                500,
                None,
                None,
                Instant::now(),
                std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                Some("test-session".into()),
            );
            rec.finalize(200, None, None, None);
        } // Drop runs here and must be a no-op.
        let (_, total) = state.store.recent(10, 0);
        assert_eq!(
            total, 1,
            "finalize followed by drop must record exactly once"
        );
    }

    #[tokio::test]
    async fn keepalive_probes_are_counted_for_diagnosis() {
        use futures_util::StreamExt;
        // A stalled stream is ambiguous after the fact: did the proxy stop
        // signalling, or did the client ignore the signal? The counter is what
        // separates the two, so it must track what actually went out.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let upstream = futures_util::stream::pending::<Result<Bytes, std::convert::Infallible>>();
        let mut out = Box::pin(keepalive_with_interval(
            upstream,
            Duration::from_millis(25),
            ANTHROPIC_KEEPALIVE_PROBE,
            counter.clone(),
        ));
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(300), out.next()).await;
        }
        assert!(
            counter.load(std::sync::atomic::Ordering::Relaxed) >= 3,
            "expected at least 3 probes, counted {}",
            counter.load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn session_id_is_read_from_the_nested_metadata_string() {
        // `metadata.user_id` is a JSON *string* holding another JSON object,
        // so it takes two parses.
        let req = json!({"metadata": {"user_id":
            "{\"device_id\":\"d3427\",\"account_uuid\":\"\",\"session_id\":\"7dea551a-c9f5-4ba1\"}"}});
        assert_eq!(
            extract_session_id(&req).as_deref(),
            Some("7dea551a-c9f5-4ba1")
        );
    }

    #[test]
    fn session_id_degrades_to_none_on_any_unexpected_shape() {
        // This is diagnostic metadata — a malformed value must never take the
        // request down with it.
        for req in [
            json!({}),
            json!({"metadata": {}}),
            json!({"metadata": {"user_id": "not json at all"}}),
            json!({"metadata": {"user_id": 12345}}),
            json!({"metadata": {"user_id": "{\"device_id\":\"x\"}"}}),
            json!({"metadata": {"user_id": "{\"session_id\":null}"}}),
            json!({"metadata": {"user_id": "{\"session_id\":\"\"}"}}),
        ] {
            assert_eq!(extract_session_id(&req), None, "req was {req}");
        }
    }

    #[test]
    fn keepalive_probes_are_wellformed_for_their_protocol() {
        // Both must terminate an SSE block, or they fuse with the next event.
        assert!(ANTHROPIC_KEEPALIVE_PROBE.ends_with(b"\n\n"));
        assert!(COMMENT_KEEPALIVE_PROBE.ends_with(b"\n\n"));
        // Anthropic clients reset their idle watchdog on *events*. A comment
        // is discarded by the SSE parser and never reaches them, which is how
        // a live connection still gets aborted as "stalled".
        assert!(ANTHROPIC_KEEPALIVE_PROBE.starts_with(b"event: ping"));
        assert!(!ANTHROPIC_KEEPALIVE_PROBE.starts_with(b":"));
        // OpenAI and Gemini have no ping event; sending one would surface as
        // an unknown event to their parsers, so those paths keep the comment.
        assert!(COMMENT_KEEPALIVE_PROBE.starts_with(b":"));
        // The ping payload must be valid JSON — clients parse `data:`.
        let payload = std::str::from_utf8(ANTHROPIC_KEEPALIVE_PROBE)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("ping carries a data line");
        let v: Value = serde_json::from_str(payload).expect("ping data is valid JSON");
        assert_eq!(v["type"], "ping");
    }

    #[tokio::test]
    async fn anthropic_stall_emits_a_ping_event_rather_than_a_comment() {
        use futures_util::StreamExt;
        let upstream = futures_util::stream::pending::<Result<Bytes, std::convert::Infallible>>();
        let mut out = Box::pin(keepalive_with_interval(
            upstream,
            Duration::from_millis(30),
            ANTHROPIC_KEEPALIVE_PROBE,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        let probe = tokio::time::timeout(Duration::from_millis(500), out.next())
            .await
            .expect("a probe must be emitted while the upstream is silent")
            .unwrap()
            .unwrap();
        assert_eq!(&probe[..], ANTHROPIC_KEEPALIVE_PROBE);
    }

    #[tokio::test]
    async fn non_anthropic_stall_keeps_using_a_comment() {
        use futures_util::StreamExt;
        let upstream = futures_util::stream::pending::<Result<Bytes, std::convert::Infallible>>();
        let mut out = Box::pin(keepalive_with_interval(
            upstream,
            Duration::from_millis(30),
            COMMENT_KEEPALIVE_PROBE,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        let probe = tokio::time::timeout(Duration::from_millis(500), out.next())
            .await
            .expect("a probe must be emitted while the upstream is silent")
            .unwrap()
            .unwrap();
        assert_eq!(&probe[..], b": keepalive\n\n");
    }

    #[test]
    fn last_event_boundary_finds_the_end_of_the_final_complete_event() {
        assert_eq!(last_event_boundary(b"data: a\n\n"), Some(9));
        assert_eq!(last_event_boundary(b"data: a\r\n\r\n"), Some(11));
        // Two events: the cut is after the second one.
        assert_eq!(last_event_boundary(b"data: a\n\ndata: b\n\n"), Some(18));
        // A trailing partial event is excluded from the cut.
        assert_eq!(last_event_boundary(b"data: a\n\nevent: par"), Some(9));
        // Nothing complete yet.
        assert_eq!(last_event_boundary(b"event: par"), None);
        assert_eq!(last_event_boundary(b""), None);
    }

    #[tokio::test]
    async fn keepalive_still_fires_when_upstream_stalls_mid_event() {
        use futures_util::StreamExt;
        // One complete event followed by the first bytes of a second, then
        // silence — exactly what a TCP split mid-event looks like when the
        // model then goes quiet to think.
        let upstream = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(
            Bytes::from("event: content_block_delta\ndata: {}\n\nevent: content_bl"),
        )])
        .chain(futures_util::stream::pending());

        let mut out = Box::pin(keepalive_with_interval(
            upstream,
            Duration::from_millis(30),
            ANTHROPIC_KEEPALIVE_PROBE,
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));

        // Only the complete event is forwarded; the partial one is held back
        // so the downstream byte stream stays on an event boundary.
        let first = out.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"event: content_block_delta\ndata: {}\n\n");

        // The upstream is now parked mid-event. A keepalive MUST still be
        // emitted — otherwise the connection goes fully silent and the client
        // aborts with "Response stalled mid-stream".
        let next = tokio::time::timeout(Duration::from_millis(500), out.next()).await;
        assert!(
            next.is_ok(),
            "no keepalive emitted while upstream was stalled mid-event"
        );
        assert_eq!(
            &next.unwrap().unwrap().unwrap()[..],
            ANTHROPIC_KEEPALIVE_PROBE
        );
    }

    #[test]
    fn only_2xx_upstreams_are_streamed_as_sse() {
        assert!(is_streamable_status(200));
        assert!(is_streamable_status(299));
        // These carry a JSON error body, not SSE. Streaming one yields a 200
        // with no events, which surfaces to the user as a stalled stream
        // rather than the actual auth/quota failure.
        for status in [401, 403, 429, 500, 502, 503] {
            assert!(!is_streamable_status(status), "status {status}");
        }
    }

    #[test]
    fn ratios_are_two_decimal_fractions_never_percentages() {
        // Half the prompt volume served from cache.
        assert_eq!(ratio_2dp(7203.0, 14434.0), 0.5);
        // A realistic Claude Code session: 98% cached, expressed as 0.98 —
        // the same scale as the per-request `cache_hit_rate` beside it, so
        // the two are not silently on different units.
        assert_eq!(ratio_2dp(101_940.0, 103_673.0), 0.98);
        assert_eq!(ratio_2dp(1.0, 1.0), 1.0);
        // A zero denominator must yield 0.0 rather than NaN, which would
        // serialize as `null` and break consumers.
        assert_eq!(ratio_2dp(0.0, 0.0), 0.0);
        assert!(ratio_2dp(5.0, 0.0).is_finite());
    }

    #[test]
    fn cost_scales_with_tokens() {
        // With no caching involved the whole prompt bills at the base rate,
        // exactly as before cache-aware pricing was introduced.
        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 1000,
            ..TokenUsage::default()
        };
        let cost = calculate_cost("claude-opus-4.8", &usage);
        assert!((cost - (0.015 + 0.075)).abs() < 1e-9);
        assert_eq!(calculate_cost("gpt-4o", &TokenUsage::default()), 0.0);
    }

    #[test]
    fn cost_prices_cache_reads_and_writes_at_their_own_rates() {
        // 3k prompt: 1k uncached, 1k read from cache, 1k written to cache.
        let usage = TokenUsage {
            input_tokens: 3000,
            cache_read_input_tokens: 1000,
            cache_creation_input_tokens: 1000,
            output_tokens: 1000,
            reasoning_tokens: 0,
        };
        let (base_in, base_out) = model_rates("claude-opus-4.8");
        let expected = base_in                      // uncached remainder
            + base_in * CACHE_WRITE_RATE_MULTIPLIER // cache write premium
            + base_in * CACHE_READ_RATE_MULTIPLIER  // cache read discount
            + base_out;
        let cost = calculate_cost("claude-opus-4.8", &usage);
        assert!((cost - expected).abs() < 1e-9, "cost was {cost}");
    }

    #[test]
    fn cached_prompts_cost_far_less_than_billing_them_at_full_rate() {
        // The real shape of a Claude Code turn: ~98% of the prompt is a cache
        // read. Charging the full input rate for all of it overstates the
        // cost several times over.
        let usage = TokenUsage {
            input_tokens: 103_673,
            cache_read_input_tokens: 101_940,
            cache_creation_input_tokens: 1_731,
            output_tokens: 561,
            reasoning_tokens: 0,
        };
        let cache_aware = calculate_cost("claude-opus-5", &usage);
        let (base_in, base_out) = model_rates("claude-opus-5");
        let naive =
            (usage.input_tokens as f64 * base_in + usage.output_tokens as f64 * base_out) / 1000.0;
        assert!(
            cache_aware < naive,
            "cache-aware {cache_aware} should be below naive {naive}"
        );
        // ...but still far above counting only the 2 uncached tokens, which is
        // what the dashboard used to report.
        let broken = (2.0 * base_in + usage.output_tokens as f64 * base_out) / 1000.0;
        assert!(
            cache_aware > broken * 2.0,
            "cache-aware {cache_aware} should dwarf the uncached-only {broken}"
        );
    }

    #[test]
    fn pagination_is_clamped_and_overflow_safe() {
        let mut p = HashMap::new();
        // Defaults.
        assert_eq!(parse_pagination(&p), (1, 50, 0));
        // Page 3 of 10.
        p.insert("page".into(), "3".into());
        p.insert("per_page".into(), "10".into());
        assert_eq!(parse_pagination(&p), (3, 10, 20));
        // Hostile values must clamp instead of overflowing usize.
        p.insert("page".into(), usize::MAX.to_string());
        p.insert("per_page".into(), usize::MAX.to_string());
        let (page, per_page, offset) = parse_pagination(&p);
        assert_eq!(page, usize::MAX);
        assert_eq!(per_page, MAX_PAGE_SIZE);
        assert_eq!(offset, usize::MAX);
        // Zero/garbage page sizes fall back to a usable window.
        p.insert("page".into(), "0".into());
        p.insert("per_page".into(), "0".into());
        assert_eq!(parse_pagination(&p), (1, 1, 0));
        p.insert("per_page".into(), "abc".into());
        assert_eq!(parse_pagination(&p).1, 50);
    }

    #[test]
    fn model_routes_do_not_conflict() {
        // `/v1/models/full/` and `/v1/models/{model_id}` are registered on the
        // same prefix; building the routes must not panic on a route conflict.
        async fn noop() {}
        let _: Router = Router::new()
            .route("/v1/models", get(noop))
            .route("/v1/models/full/", get(noop))
            .route("/v1/models/{model_id}", get(noop))
            .route("/models", get(noop))
            .route("/models/full/", get(noop))
            .route("/models/{model_id}", get(noop));
    }

    #[test]
    fn max_tokens_is_renamed_for_newer_models() {
        let mut req = json!({"model": "gpt-5.3-codex", "max_tokens": 4096});
        assert!(rewrite_max_tokens_param(&mut req));
        assert!(req.get("max_tokens").is_none());
        assert_eq!(req["max_completion_tokens"], 4096);
        // Nothing left to rename, so no pointless retry.
        assert!(!rewrite_max_tokens_param(&mut req));
    }

    #[test]
    fn max_tokens_rename_drops_the_alias_when_both_are_present() {
        let mut req = json!({"max_tokens": 10, "max_completion_tokens": 20});
        assert!(rewrite_max_tokens_param(&mut req));
        assert!(req.get("max_tokens").is_none());
        assert_eq!(req["max_completion_tokens"], 20);
    }

    #[test]
    fn keepalive_holds_back_a_partial_event_until_it_completes() {
        // The old boundary check treated these as "unsafe to inject after".
        // Now they are simply held back until the rest of the event arrives,
        // so the downstream stream never sits mid-event.
        assert_eq!(last_event_boundary(b"data: {\"partial\":"), None);
        assert_eq!(last_event_boundary(b"data: {}\n"), None);
        // Once the blank line lands, the whole event is released.
        assert_eq!(last_event_boundary(b"data: {}\n\n"), Some(10));
    }

    #[test]
    fn client_beta_header_is_read() {
        let mut h = HeaderMap::new();
        assert_eq!(client_beta_header(&h), None);
        h.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219"),
        );
        assert_eq!(
            client_beta_header(&h).as_deref(),
            Some("claude-code-20250219")
        );
    }

    /// Body of an `axum` response, for asserting on what a client would see.
    fn body_json(resp: Response) -> Value {
        let bytes = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { axum::body::to_bytes(resp.into_body(), usize::MAX).await })
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Copilot rejects in OpenAI's shape; an Anthropic client needs the
    /// envelope to recognise it as an error at all.
    #[test]
    fn openai_shaped_upstream_error_is_rewrapped() {
        let upstream = r#"{"error":{"message":"The use of the web search tool is not supported.","code":"unsupported_value"}}"#;
        let body = body_json(anthropic_passthrough_error(
            StatusCode::BAD_REQUEST,
            upstream.to_string(),
        ));
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(
            body["error"]["message"],
            "The use of the web search tool is not supported."
        );
    }

    #[test]
    fn already_anthropic_shaped_error_is_left_alone() {
        let upstream = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1"}"#;
        let body = body_json(anthropic_passthrough_error(
            StatusCode::TOO_MANY_REQUESTS,
            upstream.to_string(),
        ));
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert_eq!(body["request_id"], "req_1");
    }

    #[test]
    fn non_json_upstream_body_becomes_the_message() {
        let body = body_json(anthropic_passthrough_error(
            StatusCode::BAD_GATEWAY,
            "<html>502 Bad Gateway</html>".to_string(),
        ));
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["message"], "<html>502 Bad Gateway</html>");
    }

    #[test]
    fn empty_upstream_body_falls_back_to_the_status_reason() {
        let body = body_json(anthropic_passthrough_error(
            StatusCode::UNAUTHORIZED,
            String::new(),
        ));
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["message"], "Unauthorized");
    }

    #[test]
    fn status_maps_onto_the_anthropic_error_type() {
        for (status, expected) in [
            (400, "invalid_request_error"),
            (401, "authentication_error"),
            (403, "permission_error"),
            (404, "not_found_error"),
            (429, "rate_limit_error"),
            (500, "api_error"),
            (529, "overloaded_error"),
        ] {
            assert_eq!(anthropic_error_type(status), expected, "status {status}");
        }
    }
}
