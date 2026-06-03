//! Miscellaneous helpers: orphaned tool-result detection/removal and an HTTP
//! request retry wrapper with exponential backoff.

use crate::state::AppState;
use serde_json::Value;
use std::time::Duration;

/// Detects whether an upstream error indicates an orphaned `tool_use_id` in a
/// `tool_result` block.
pub fn is_orphaned_tool_error(status: u16, body: &str) -> bool {
    status == 400 && body.contains("tool_use_id") && body.contains("tool_result")
}

/// Detects the upstream 400 error returned by models that no longer accept
/// `thinking.type: "enabled"` and instead require the adaptive thinking format
/// (`thinking.type: "adaptive"` plus `output_config.effort`).
pub fn is_thinking_enabled_unsupported_error(status: u16, body: &str) -> bool {
    status == 400 && body.contains("thinking.type.enabled") && body.contains("adaptive")
}

/// Extracts orphaned tool-use ids referenced in an error message.
pub fn extract_orphaned_ids(body: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let marker = "unexpected `tool_use_id` found in `tool_result` blocks: ";
    if let Some(pos) = body.find(marker) {
        let start = pos + marker.len();
        let rest = &body[start..];
        let end = rest
            .find(['.', ' ', '"', '\'', '\\', '\n'])
            .unwrap_or(rest.len());
        let id = rest[..end].trim();
        if !id.is_empty() {
            ids.push(id.to_string());
        }
    }
    if ids.is_empty() {
        let mut s = body;
        while let Some(pos) = s.find("toolu_") {
            let rest = &s[pos..];
            let end = rest
                .char_indices()
                .find(|(i, c)| *i > 0 && !(c.is_alphanumeric() || *c == '_' || *c == '-'))
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let id = &rest[..end];
            if !id.is_empty() && !ids.contains(&id.to_string()) {
                ids.push(id.to_string());
            }
            s = &rest[end..];
        }
    }
    ids
}

/// Removes user `tool_result` blocks whose `tool_use_id` is in `orphaned`.
pub fn remove_orphaned_tool_results(messages: &[Value], orphaned: &[String]) -> Vec<Value> {
    if orphaned.is_empty() {
        return messages.to_vec();
    }
    let set: std::collections::HashSet<&str> = orphaned.iter().map(|s| s.as_str()).collect();
    let mut out: Vec<Value> = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            out.push(msg.clone());
            continue;
        }
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            out.push(msg.clone());
            continue;
        };
        let filtered: Vec<Value> = content
            .iter()
            .filter(|b| {
                let is_orphan = b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    && b.get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .map(|id| set.contains(id))
                        .unwrap_or(false);
                !is_orphan
            })
            .cloned()
            .collect();
        if !filtered.is_empty() {
            let mut nm = msg.clone();
            nm["content"] = Value::Array(filtered);
            out.push(nm);
        }
    }
    out
}

/// Performs a POST request with retry and exponential backoff on connection
/// errors, refreshing the Copilot token between attempts. Returns the response
/// or `None` if all attempts fail at the transport level.
pub async fn post_with_retry(
    state: &AppState,
    url: &str,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
    endpoint: &str,
) -> Option<reqwest::Response> {
    let max = state.config.max_connection_retries;
    let mut attempt = 0u32;
    loop {
        let result = state
            .http
            .post(url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;
        match result {
            Ok(resp) => return Some(resp),
            Err(e) => {
                let _ = state.ensure_copilot_token().await;
                if attempt < max {
                    let backoff = 2u64.pow(attempt).min(8);
                    tracing::warn!(
                        "[{endpoint}] Connection error (attempt {}/{}): {e}",
                        attempt + 1,
                        max + 1
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    attempt += 1;
                } else {
                    tracing::warn!("[{endpoint}] Connection error (final attempt): {e}");
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_thinking_enabled_unsupported_error() {
        let body = "\"thinking.type.enabled\" is not supported for this model. \
            Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior.";
        assert!(is_thinking_enabled_unsupported_error(400, body));
        // Wrong status code.
        assert!(!is_thinking_enabled_unsupported_error(200, body));
        // Unrelated 400 error.
        assert!(!is_thinking_enabled_unsupported_error(
            400,
            "some other validation error"
        ));
    }
}
