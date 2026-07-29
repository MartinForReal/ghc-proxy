//! Miscellaneous helpers: orphaned tool-result detection/removal, SSE line
//! buffering, and an HTTP request retry wrapper with exponential backoff.

use crate::state::AppState;
use serde_json::Value;
use std::time::Duration;

/// Incremental, chunk-boundary-safe splitter for SSE byte streams.
///
/// Upstream responses arrive as arbitrary byte chunks that do **not** respect
/// either line or UTF-8 character boundaries. Decoding each chunk with
/// `String::from_utf8_lossy` as it arrives corrupts any multi-byte character
/// (CJK, emoji, box-drawing, …) that straddles two chunks, replacing it with
/// `U+FFFD` — and if that character sits inside a `data:` payload, the whole
/// event stops parsing as JSON and is dropped, silently deleting text from the
/// middle of a response.
///
/// This buffer keeps raw bytes until a `\n` is seen and only then decodes, so a
/// partial character simply waits for the rest of its bytes. Anything left over
/// when the stream ends is returned by [`SseLineBuffer::flush`], so a final
/// event that arrives without a trailing newline is not lost either.
#[derive(Default)]
pub struct SseLineBuffer {
    buf: Vec<u8>,
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a chunk and returns every complete line it completed, with the
    /// trailing `\r` (CRLF streams) already stripped.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0usize;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            lines.push(decode_line(&self.buf[start..end]));
            start = end + 1;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
        lines
    }

    /// Returns any buffered bytes not terminated by a newline, consuming them.
    /// Called once the upstream stream ends so a final unterminated event is
    /// still delivered.
    pub fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line = decode_line(&self.buf);
        self.buf.clear();
        if line.is_empty() {
            None
        } else {
            Some(line)
        }
    }
}

/// Decodes one complete SSE line. A complete line is always valid UTF-8 in
/// practice, so the lossy conversion never actually substitutes anything here.
fn decode_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\r')
        .to_string()
}

/// Returns the payload of an SSE `data:` line, or `None` for any other field
/// (`event:`, `id:`, `retry:`, comments, blank separators).
pub fn sse_data(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    // The spec allows exactly one optional space after the colon.
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

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

/// Detects the upstream 400 returned by newer OpenAI-family models that
/// replaced `max_tokens` with `max_completion_tokens`.
pub fn is_max_tokens_unsupported_error(status: u16, body: &str) -> bool {
    status == 400 && body.contains("max_tokens") && body.contains("max_completion_tokens")
}

/// Fetches the latest stable VS Code version from Microsoft's own update
/// service (`update.code.visualstudio.com`), the same endpoint the editor's
/// updater uses. Returns `None` on any network or parse error so callers can
/// fall back to a configured default.
///
/// This deliberately does **not** scrape the Arch User Repository PKGBUILD:
/// the AUR maintainers asked proxies of this kind to stop, having become the
/// single most-requested endpoint on their service.
pub async fn fetch_latest_vscode_version(client: &reqwest::Client) -> Option<String> {
    const URL: &str = "https://update.code.visualstudio.com/api/releases/stable";
    let resp = client
        .get(URL)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let releases: Vec<String> = resp.json().await.ok()?;
    // Newest first; keep only plain `major.minor.patch` entries so insider or
    // recovery builds never leak into the `Editor-Version` header.
    releases.into_iter().find(|v| is_release_version(v))
}

/// Whether a string is a plain `major.minor.patch` version.
fn is_release_version(v: &str) -> bool {
    let mut parts = 0;
    for part in v.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 3
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
/// Detects whether a response status code is retryable.
/// Terminal errors (4xx except 429, 5xx except 502-504) should not be retried.
/// Connection errors and transient 5xx errors should be retried.
pub fn is_retryable_error(status: u16) -> bool {
    match status {
        // Client errors: only retry 429 (rate limit)
        400..=428 | 430..=499 => false,
        429 => true, // Rate limit - retry with backoff
        // Server errors: retry 502, 503, 504 (gateway/service issues)
        502..=504 => true,
        // Other 5xx errors (500, 501, etc) - don't retry (likely permanent)
        500..=501 | 505..=599 => false,
        _ => false,
    }
}

pub async fn post_with_retry(
    state: &AppState,
    url: &str,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
    endpoint: &str,
) -> Option<reqwest::Response> {
    let max = state.max_connection_retries();
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
            Ok(resp) => {
                let status = resp.status().as_u16();
                // If response is successful or a non-retryable error, return it
                if status < 400 || !is_retryable_error(status) {
                    return Some(resp);
                }
                // Response is a retryable error - log and possibly retry
                let _ = state.ensure_copilot_token().await;
                if attempt < max {
                    let backoff = if status == 429 {
                        2u64.pow(attempt + 1)
                    } else {
                        2u64.pow(attempt)
                    }
                    .min(8);
                    tracing::warn!(
                        "[{endpoint}] Retryable error {status} (attempt {}/{}), backing off {backoff}s",
                        attempt + 1,
                        max + 1
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    attempt += 1;
                } else {
                    tracing::warn!("[{endpoint}] Retryable error {status} (final attempt)");
                    return Some(resp);
                }
            }
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

/// Token counts for one request, split by how the input was served.
///
/// `input_tokens` is always the **true total** prompt size. Providers disagree
/// on how they slice that total, so the extractors below normalize it:
/// Anthropic reports three disjoint buckets that must be summed, while
/// OpenAI's `prompt_tokens` already contains its cached subset.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TokenUsage {
    /// Total prompt tokens, cached and uncached alike.
    pub input_tokens: u64,
    /// Portion of `input_tokens` served from a warm cache.
    pub cache_read_input_tokens: u64,
    /// Portion of `input_tokens` written into the cache by this request.
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    /// Fraction of the prompt served from cache, or `None` when the request
    /// had no input at all (which would make the ratio meaningless rather
    /// than zero).
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        (self.input_tokens > 0)
            .then(|| self.cache_read_input_tokens as f64 / self.input_tokens as f64)
    }

    /// Adopts a usage object parsed from a streaming chunk, keeping the
    /// previous totals when the new one is empty.
    ///
    /// Streams repeat `usage` across chunks — frequently as `null` or an empty
    /// object until the final event — so overwriting unconditionally would
    /// zero out counts a later chunk simply did not restate.
    pub fn merge_stream_update(&mut self, next: TokenUsage) {
        if next.input_tokens > 0 || next.output_tokens > 0 {
            *self = next;
        }
    }
}

/// Reads a `u64` field, treating absent/malformed values as zero.
fn usage_field(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(|t| t.as_u64()).unwrap_or(0)
}

/// Normalizes an Anthropic `usage` object.
///
/// `input_tokens`, `cache_read_input_tokens` and `cache_creation_input_tokens`
/// are **disjoint**, so the real prompt size is their sum. Reading
/// `input_tokens` on its own reports single digits for a fully-cached
/// hundred-thousand-token conversation, because Claude Code covers almost the
/// entire prompt — including the newest message — with `cache_control`.
pub fn anthropic_usage(usage: &Value) -> TokenUsage {
    let cache_read = usage_field(usage, "cache_read_input_tokens");
    let cache_creation = usage_field(usage, "cache_creation_input_tokens");
    TokenUsage {
        input_tokens: usage_field(usage, "input_tokens") + cache_read + cache_creation,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        output_tokens: usage_field(usage, "output_tokens"),
    }
}

/// Normalizes an OpenAI `usage` object.
///
/// Unlike Anthropic's disjoint buckets, `prompt_tokens` is already the total
/// and `prompt_tokens_details.cached_tokens` is a subset of it — adding them
/// would double-count. OpenAI has no cache-write concept, so that bucket stays
/// zero.
pub fn openai_usage(usage: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage_field(usage, "prompt_tokens"),
        cache_read_input_tokens: usage
            .get("prompt_tokens_details")
            .map(|d| usage_field(d, "cached_tokens"))
            .unwrap_or(0),
        cache_creation_input_tokens: 0,
        output_tokens: usage_field(usage, "completion_tokens"),
    }
}

/// Normalizes an OpenAI **Responses API** `usage` object.
///
/// A trap worth naming: this API spells its totals `input_tokens` /
/// `output_tokens` — the same keys Anthropic uses — but with OpenAI's
/// semantics, where `input_tokens` is already the grand total and
/// `input_tokens_details.cached_tokens` is a subset of it. Feeding one of
/// these payloads to [`anthropic_usage`] would double-count the cached half.
pub fn responses_usage(usage: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage_field(usage, "input_tokens"),
        cache_read_input_tokens: usage
            .get("input_tokens_details")
            .map(|d| usage_field(d, "cached_tokens"))
            .unwrap_or(0),
        cache_creation_input_tokens: 0,
        output_tokens: usage_field(usage, "output_tokens"),
    }
}

/// Tracks the longest silence between upstream chunks on a streaming response.
///
/// When a stream stalls, this is the number that assigns blame: a large value
/// means the upstream itself went quiet, while a small one means the upstream
/// kept sending and the stall happened on the proxy or client side. Without it
/// the two are indistinguishable after the fact.
///
/// The wait before the first chunk counts as idle time too — a stream that
/// never produces anything is the worst stall of all.
pub struct IdleTracker {
    last: std::time::Instant,
    max_idle_ms: u64,
}

impl IdleTracker {
    pub fn new(start: std::time::Instant) -> Self {
        IdleTracker {
            last: start,
            max_idle_ms: 0,
        }
    }

    /// Records that a chunk arrived at `now`.
    pub fn mark(&mut self, now: std::time::Instant) {
        let gap = now.saturating_duration_since(self.last).as_millis() as u64;
        self.max_idle_ms = self.max_idle_ms.max(gap);
        self.last = now;
    }

    /// Records a chunk arriving right now.
    pub fn mark_now(&mut self) {
        self.mark(std::time::Instant::now());
    }

    /// Longest observed gap, including any trailing silence up to `now`.
    pub fn max_idle_ms_including_now(&self) -> u64 {
        let trailing = self.last.elapsed().as_millis() as u64;
        self.max_idle_ms.max(trailing)
    }

    pub fn max_idle_ms(&self) -> u64 {
        self.max_idle_ms
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

    #[test]
    fn sse_buffer_splits_on_newlines_only() {
        let mut b = SseLineBuffer::new();
        assert_eq!(
            b.push(b"data: one\ndata: two\n"),
            vec!["data: one", "data: two"]
        );
        // A partial line is held back until its newline arrives.
        assert!(b.push(b"data: thr").is_empty());
        assert_eq!(b.push(b"ee\n"), vec!["data: three"]);
        assert!(b.flush().is_none());
    }

    #[test]
    fn sse_buffer_strips_crlf() {
        let mut b = SseLineBuffer::new();
        assert_eq!(b.push(b"data: x\r\n\r\n"), vec!["data: x", ""]);
    }

    #[test]
    fn sse_buffer_survives_split_multibyte_characters() {
        // "你好" split in the middle of the first character's 3 bytes. Decoding
        // each chunk independently would yield U+FFFD and destroy the payload.
        let full = "data: {\"t\":\"你好🎉\"}\n".as_bytes().to_vec();
        let cut = 9; // inside the first multi-byte character
        let mut b = SseLineBuffer::new();
        assert!(b.push(&full[..cut]).is_empty());
        let lines = b.push(&full[cut..]);
        assert_eq!(lines, vec!["data: {\"t\":\"你好🎉\"}"]);
        assert!(!lines[0].contains('\u{FFFD}'));
        // The payload still parses as JSON, which is what keeps the delta alive.
        let data = sse_data(&lines[0]).unwrap();
        let v: Value = serde_json::from_str(data).unwrap();
        assert_eq!(v["t"], "你好🎉");
    }

    #[test]
    fn sse_buffer_survives_byte_at_a_time_delivery() {
        let payload = "data: {\"t\":\"日本語テキスト\"}\n";
        let mut b = SseLineBuffer::new();
        let mut lines = Vec::new();
        for byte in payload.as_bytes() {
            lines.extend(b.push(&[*byte]));
        }
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(sse_data(&lines[0]).unwrap()).unwrap();
        assert_eq!(v["t"], "日本語テキスト");
    }

    #[test]
    fn sse_buffer_flush_returns_unterminated_tail() {
        let mut b = SseLineBuffer::new();
        assert!(b.push(b"data: last-event-no-newline").is_empty());
        assert_eq!(b.flush().as_deref(), Some("data: last-event-no-newline"));
        // Flushing twice is a no-op.
        assert!(b.flush().is_none());
    }

    #[test]
    fn sse_data_extracts_payload_only() {
        assert_eq!(sse_data("data: {}"), Some("{}"));
        // No space after the colon is also valid.
        assert_eq!(sse_data("data:{}"), Some("{}"));
        // Only a single leading space is consumed.
        assert_eq!(sse_data("data:  x"), Some(" x"));
        assert_eq!(sse_data("event: message"), None);
        assert_eq!(sse_data(": comment"), None);
        assert_eq!(sse_data(""), None);
    }

    #[test]
    fn release_versions_exclude_insider_builds() {
        assert!(is_release_version("1.130.0"));
        assert!(is_release_version("1.99.12"));
        assert!(!is_release_version("1.130.0-insider"));
        assert!(!is_release_version("1.130"));
        assert!(!is_release_version("1.130.0.1"));
        assert!(!is_release_version(""));
    }

    #[test]
    fn detects_max_tokens_parameter_migration() {
        let body = "Unsupported parameter: 'max_tokens' is not supported with this model. \
            Use 'max_completion_tokens' instead.";
        assert!(is_max_tokens_unsupported_error(400, body));
        assert!(!is_max_tokens_unsupported_error(200, body));
        assert!(!is_max_tokens_unsupported_error(400, "some other error"));
    }

    #[test]
    fn anthropic_usage_sums_all_three_input_buckets() {
        // Verbatim from a real Copilot `/v1/messages` message_start event.
        // Reading only `input_tokens` reports 2 for a 100k-token prompt.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_read_input_tokens": 101940,
            "cache_creation_input_tokens": 1731,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 1731,
                "ephemeral_1h_input_tokens": 0
            },
            "output_tokens": 2
        });
        let u = anthropic_usage(&usage);
        assert_eq!(u.input_tokens, 103_673);
        assert_eq!(u.cache_read_input_tokens, 101_940);
        assert_eq!(u.cache_creation_input_tokens, 1_731);
        assert_eq!(u.output_tokens, 2);
    }

    #[test]
    fn anthropic_usage_tolerates_absent_cache_fields() {
        // A cache-less turn: the total is just the uncached input.
        let usage = serde_json::json!({"input_tokens": 4_096, "output_tokens": 128});
        let u = anthropic_usage(&usage);
        assert_eq!(u.input_tokens, 4_096);
        assert_eq!(u.cache_read_input_tokens, 0);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.output_tokens, 128);
    }

    #[test]
    fn openai_usage_does_not_double_count_cached_tokens() {
        // OpenAI's `prompt_tokens` ALREADY includes `cached_tokens`, unlike
        // Anthropic's disjoint buckets. Summing them would inflate the total.
        let usage = serde_json::json!({
            "prompt_tokens": 5_000,
            "completion_tokens": 300,
            "prompt_tokens_details": {"cached_tokens": 4_500}
        });
        let u = openai_usage(&usage);
        assert_eq!(u.input_tokens, 5_000);
        assert_eq!(u.cache_read_input_tokens, 4_500);
        // OpenAI has no cache-write concept.
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.output_tokens, 300);
    }

    #[test]
    fn idle_tracker_reports_the_longest_upstream_gap() {
        let t0 = std::time::Instant::now();
        let mut t = IdleTracker::new(t0);
        // Chunks arriving steadily.
        t.mark(t0 + Duration::from_millis(100));
        t.mark(t0 + Duration::from_millis(150));
        assert_eq!(t.max_idle_ms(), 100);
        // A long think between chunks — this is the number that distinguishes
        // "upstream went quiet" from "proxy failed to forward".
        t.mark(t0 + Duration::from_millis(2_150));
        assert_eq!(t.max_idle_ms(), 2_000);
        // A later short gap must not lower the recorded maximum.
        t.mark(t0 + Duration::from_millis(2_200));
        assert_eq!(t.max_idle_ms(), 2_000);
    }

    #[test]
    fn idle_tracker_counts_the_wait_before_the_first_chunk() {
        // Time-to-first-byte is itself an upstream stall; a stream that never
        // produces a chunk at all must not report zero idle time.
        let t0 = std::time::Instant::now();
        let mut t = IdleTracker::new(t0);
        t.mark(t0 + Duration::from_millis(4_000));
        assert_eq!(t.max_idle_ms(), 4_000);
    }

    #[test]
    fn responses_usage_reads_the_responses_api_field_names() {
        // The Responses API reuses Anthropic's key names for a DIFFERENT
        // meaning: its `input_tokens` is already the grand total and
        // `input_tokens_details.cached_tokens` is a subset, so these must not
        // be summed the way anthropic_usage sums its disjoint buckets.
        let usage = serde_json::json!({
            "input_tokens": 8_000,
            "output_tokens": 250,
            "input_tokens_details": {"cached_tokens": 6_000}
        });
        let u = responses_usage(&usage);
        assert_eq!(u.input_tokens, 8_000);
        assert_eq!(u.cache_read_input_tokens, 6_000);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.output_tokens, 250);
    }

    #[test]
    fn stream_usage_updates_ignore_empty_repeats() {
        let mut running = TokenUsage::default();
        running.merge_stream_update(TokenUsage {
            input_tokens: 5_000,
            cache_read_input_tokens: 4_000,
            output_tokens: 10,
            ..TokenUsage::default()
        });
        assert_eq!(running.input_tokens, 5_000);

        // Streams repeat `usage` across chunks, often empty until the last
        // one. Adopting an empty repeat would wipe totals already collected.
        running.merge_stream_update(TokenUsage::default());
        assert_eq!(running.input_tokens, 5_000);
        assert_eq!(running.cache_read_input_tokens, 4_000);
        assert_eq!(running.output_tokens, 10);

        // A later chunk carrying real numbers does win.
        running.merge_stream_update(TokenUsage {
            input_tokens: 5_000,
            cache_read_input_tokens: 4_000,
            output_tokens: 250,
            ..TokenUsage::default()
        });
        assert_eq!(running.output_tokens, 250);
    }

    #[test]
    fn cache_hit_ratio_is_none_without_input() {
        let none = TokenUsage::default();
        assert_eq!(none.cache_hit_ratio(), None);
        let hit = TokenUsage {
            input_tokens: 103_673,
            cache_read_input_tokens: 101_940,
            ..TokenUsage::default()
        };
        let ratio = hit.cache_hit_ratio().unwrap();
        assert!((ratio - 0.983).abs() < 0.001, "ratio was {ratio}");
    }
}
