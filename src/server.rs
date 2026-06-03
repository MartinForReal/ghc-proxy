//! HTTP server: route definitions and request handlers for the OpenAI- and
//! Anthropic-compatible proxy endpoints, plus the analytics dashboard API.

use crate::anthropic::{self, AnthropicStreamState};
use crate::responses as codex;
use crate::state::SharedState;
use crate::store::RequestRecord;
use crate::translate;
use crate::util;
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;

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
        .route("/", get(dashboard))
        .route("/requests", get(requests_page))
        .route("/api/stats", get(api_stats))
        .route("/api/requests", get(api_requests))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024 * 1024)) // 20 GB limit
        .with_state(state)
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
// Models
// ---------------------------------------------------------------------------

async fn get_models(State(state): State<SharedState>) -> Response {
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if state.models.read().await.is_none() {
        let _ = state.load_models().await;
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
    let models = state.models.read().await;
    Json(models.clone().unwrap_or(Value::Null)).into_response()
}

// ---------------------------------------------------------------------------
// Chat completions
// ---------------------------------------------------------------------------

async fn chat_completions(State(state): State<SharedState>, body: Bytes) -> Response {
    let start = Instant::now();
    if let Err(e) = state.ensure_copilot_token().await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
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
    let translated = translate::translate(&state.config.model_mappings, &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
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

    let mut headers = state.copilot_headers(vision).await;
    set_initiator(&mut headers, agent);

    let req_size = body.len();
    let url = format!("{}/chat/completions", state.config.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let payload = serde_json::to_vec(&req).unwrap_or_default();

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
                state.config.max_connection_retries + 1
            ),
        );
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let resp_size = text.len();
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/chat/completions".into(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            duration: elapsed_secs(start),
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
    let mut req = match parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let original_model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();
    let translated = translate::translate(&state.config.model_mappings, &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
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
    let url = format!("{}/responses", state.config.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let payload = serde_json::to_vec(&req).unwrap_or_default();

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
                state.config.max_connection_retries + 1
            ),
        );
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let resp_size = text.len();
    if status.is_success() {
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
        state.store.add(RequestRecord {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_iso(),
            endpoint: "/v1/responses".into(),
            model: original_model.clone(),
            translated_model: (translated != original_model).then_some(translated),
            status_code: status.as_u16(),
            request_size: req_size,
            response_size: resp_size,
            input_tokens: usage
                .get("input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            duration: elapsed_secs(start),
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
    if let Err(e) = state.ensure_copilot_token().await {
        return anthropic_error(StatusCode::INTERNAL_SERVER_ERROR, e);
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
    let translated = translate::translate(&state.config.model_mappings, &original_model);
    if translated != original_model {
        req["model"] = Value::String(translated.clone());
    }
    req = anthropic::apply_system_prompt(&req, &state.config);
    req = anthropic::apply_tool_result_suffix(&req, &state.config);

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
    set_initiator(&mut headers, agent);

    let url = format!("{}/v1/messages", state.config.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut current = req.clone();
    for _ in 0..4 {
        let mut sanitized = anthropic::sanitize_anthropic_request(&current);
        sanitized = anthropic::adjust_thinking_budget(&sanitized);
        let req_size = serde_json::to_vec(&current).map(|v| v.len()).unwrap_or(0);
        let payload = serde_json::to_vec(&sanitized).unwrap_or_default();

        if is_stream {
            return stream_anthropic_direct(
                state.clone(),
                &url,
                headers.clone(),
                payload,
                original_model,
                translated,
                req_size,
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
            let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
            state.store.add(RequestRecord {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: now_iso(),
                endpoint: "/v1/messages".into(),
                model: original_model.clone(),
                translated_model: (translated != original_model).then_some(translated.clone()),
                status_code: status.as_u16(),
                request_size: req_size,
                response_size: serde_json::to_vec(&parsed).map(|v| v.len()).unwrap_or(0),
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
                duration: elapsed_secs(start),
            });
            return Json(parsed).into_response();
        }
        let text = resp.text().await.unwrap_or_default();
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
    let url = format!("{}/chat/completions", state.config.copilot_base_url());
    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut current = req.clone();
    for _ in 0..4 {
        let openai_req = anthropic::anthropic_to_openai(&current, &state.config);
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
        let mut headers = state.copilot_headers(vision).await;
        set_initiator(&mut headers, agent);
        let req_size = serde_json::to_vec(&current).map(|v| v.len()).unwrap_or(0);
        let payload = serde_json::to_vec(&openai_req).unwrap_or_default();

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
            let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
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
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
                duration: elapsed_secs(start),
            });
            return Json(anthropic_resp).into_response();
        }
        let text = resp.text().await.unwrap_or_default();
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
    let req = match parse_body(&body) {
        Ok(v) => v,
        Err(_) => return Json(json!({"input_tokens": 1})).into_response(),
    };
    let model = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let est = crate::filters::estimate_tokens;

    let model_meta = {
        let models = state.models.read().await;
        models
            .as_ref()
            .and_then(|m| m.get("data"))
            .and_then(|d| d.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
                    .cloned()
            })
    };
    let Some(model_meta) = model_meta else {
        return Json(json!({"input_tokens": 1})).into_response();
    };

    let mut total: u64 = 0;
    match req.get("system") {
        Some(Value::String(s)) => total += est(s),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    total += est(b.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                }
            }
        }
        _ => {}
    }
    for msg in req
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
    {
        match msg.get("content") {
            Some(Value::String(s)) => total += est(s),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            total += est(b.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                        }
                        Some("tool_result") => {
                            if let Some(s) = b.get("content").and_then(|c| c.as_str()) {
                                total += est(s);
                            }
                        }
                        Some("tool_use") => {
                            let input = b.get("input").cloned().unwrap_or(json!({}));
                            total += est(&serde_json::to_string(&input).unwrap_or_default());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        if !tools.is_empty() {
            total += if model.starts_with("grok") { 480 } else { 346 };
            for t in tools {
                total += est(t.get("name").and_then(|n| n.as_str()).unwrap_or(""));
                total += est(t.get("description").and_then(|d| d.as_str()).unwrap_or(""));
                let schema = t.get("input_schema").cloned().unwrap_or(json!({}));
                total += est(&serde_json::to_string(&schema).unwrap_or_default());
            }
        }
    }
    let vendor = model_meta
        .get("vendor")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if vendor != "Anthropic" {
        let factor = if model.starts_with("grok") {
            1.03
        } else {
            1.05
        };
        total = ((total as f64) * factor).ceil() as u64;
    }
    Json(json!({"input_tokens": total})).into_response()
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
    let model = translated.clone();
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut buf = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            buf.push_str(&String::from_utf8_lossy(&chunk));
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
    _req: Value,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
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
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            resp_size += chunk.len();
            // Verbatim passthrough of raw bytes.
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::copy_from_slice(&chunk));
            buf.push_str(&String::from_utf8_lossy(&chunk));
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
        });
    };
    build_sse_response(stream)
}

/// Streams a direct Anthropic SSE response back to the client verbatim.
#[allow(clippy::too_many_arguments)]
async fn stream_anthropic_direct(
    state: SharedState,
    url: &str,
    headers: HeaderMap,
    payload: Vec<u8>,
    original_model: String,
    translated: String,
    req_size: usize,
    start: Instant,
) -> Response {
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
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        let mut buf = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            resp_size += chunk.len();
            yield Ok::<Bytes, std::convert::Infallible>(Bytes::copy_from_slice(&chunk));
            buf.push_str(&String::from_utf8_lossy(&chunk));
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
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = upstream.bytes_stream();
        let mut buf = String::new();
        let mut conv = AnthropicStreamState::new();
        let mut chunks: Vec<Value> = Vec::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut resp_size = 0usize;
        while let Some(chunk) = byte_stream.next().await {
            let Ok(chunk) = chunk else { break; };
            buf.push_str(&String::from_utf8_lossy(&chunk));
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

async fn requests_page() -> Response {
    serve_asset("requests.html", include_str!("../public/requests.html"))
}

fn serve_asset(_name: &str, contents: &'static str) -> Response {
    let mut resp = Response::new(Body::from(contents));
    resp.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
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
