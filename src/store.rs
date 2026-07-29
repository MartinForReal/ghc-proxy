//! Lightweight in-memory request store powering the analytics dashboard.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Mutex;

/// A single recorded request/response pair plus metadata.
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecord {
    pub id: String,
    pub timestamp: String,
    pub endpoint: String,
    pub model: String,
    pub translated_model: Option<String>,
    pub status_code: u16,
    pub request_size: usize,
    pub response_size: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Whether `output_tokens` is the upstream's final figure.
    ///
    /// On an Anthropic stream the authoritative count arrives only in
    /// `message_delta`, at the very end. A record finalized before that — a
    /// client stall abort, an interrupted stream — carries the opening
    /// placeholder from `message_start` instead, which is a single digit
    /// against an eventual five-figure total. `Some(false)` marks the number
    /// as unknown rather than letting the dashboard present it as fact.
    /// `None` on paths that do not track this, so their counts render as
    /// before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_final: Option<bool>,
    /// Portion of `input_tokens` that was served from a warm prompt cache.
    /// Always serialized: the dashboard derives the cache hit ratio from it.
    pub cache_read_input_tokens: u64,
    /// Portion of `input_tokens` this request wrote into the cache.
    pub cache_creation_input_tokens: u64,
    /// Output tokens the model spent reasoning without emitting. Billed, but
    /// never visible in the answer — which is how a turn can cost a full budget
    /// and show nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// What Copilot itself billed, in nano-AI-units, from the `copilot_usage`
    /// object on the response. Authoritative, unlike `estimated_cost_usd`
    /// beside it, which is a list-price guess that cannot know a model is
    /// included at no charge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billed_nano_aiu: Option<u64>,
    /// What the prompt cache was worth this turn, in nano-AI-units, computed
    /// from the model's own rates. Negative on a turn that populated a cache
    /// it did not get to read. `None` when the model reported no rates — not
    /// every surface charges for the same token types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_saved_nano_aiu: Option<i64>,
    /// Copilot premium-request cost, from the model catalog's
    /// `billing.multiplier`. `None` when the model is unknown or the endpoint
    /// carries no premium cost — never guessed as 1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium_multiplier: Option<f64>,
    /// Longest silence between two upstream chunks, in milliseconds. Only set
    /// on streaming responses. When a stream stalls this is what assigns
    /// blame: a large value means the upstream went quiet, a small one means
    /// it kept sending and the stall was downstream of here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_idle_max_ms: Option<u64>,
    pub duration: f64,
    /// Captured request body. Only populated when debug mode is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    /// Captured upstream response body. Only populated when debug mode is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,

    // Audit fields: extracted from request/response bodies for analysis
    /// Number of messages in the request (conversation turn count)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    /// Number of tools sent in the request
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    /// Names of tools sent in the request
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_names: Option<Vec<String>>,
    /// Reason why the response stopped: "end_turn", "tool_use", "max_tokens", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Names of tools actually called by the model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_called: Option<Vec<String>>,
    /// Whether this request was initiated by an agent (vs user)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_agent_initiated: Option<bool>,
    /// Whether prompt caching was used (hit or write)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit: Option<bool>,
    /// Client session this request belongs to, from `metadata.user_id`.
    /// Several Claude Code instances can share one proxy; without this their
    /// records interleave in the dashboard with no way to tell them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Keepalive probes actually written to the client during this response.
    /// Pairs with `upstream_idle_max_ms` to place blame on a stalled stream:
    /// a long idle with probes sent means the proxy kept signalling and the
    /// client ignored it; a long idle with zero probes means the keepalive
    /// itself failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_probes: Option<u32>,
    /// Why the request failed, when it did. `None` on a successful request.
    /// Answers "which step broke" without needing a packet capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    /// Estimated cost in USD based on token counts and model rates
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

impl RequestRecord {
    /// Whether this attempt never produced a usable answer.
    ///
    /// One definition, because three places ask: the failures-only filter, the
    /// statistics that must exclude these, and the dashboard. A non-2xx status
    /// is the obvious case; `failure_kind` catches the ones that carry a
    /// success status but did not succeed — a stream cut off mid-answer, or a
    /// client that hung up.
    pub fn failed(&self) -> bool {
        self.status_code >= 400 || self.failure_kind.is_some()
    }
}

/// Values for [`RequestRecord::failure_kind`], ordered by how far the request
/// got before dying.
pub mod failure {
    /// Never left the proxy — token refresh or the rate gate rejected it.
    pub const PRECONDITION: &str = "precondition_failed";
    /// Sent, but the upstream never produced a response.
    pub const CONNECT: &str = "connect_error";
    /// The upstream answered with a non-2xx status.
    pub const UPSTREAM_STATUS: &str = "upstream_status";
    /// The upstream stream died partway through.
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
    /// The client hung up before the response finished.
    pub const CLIENT_DISCONNECTED: &str = "client_disconnected";
}

/// Aggregate statistics returned by `/api/stats`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    /// Requests that produced a usable answer. Failed attempts are counted
    /// separately rather than here: they consume nothing, and folding them in
    /// would dilute every rate derived from this — a burst of rejected calls
    /// would look like a collapse in cache hit rate.
    pub request_count: u64,
    /// Attempts that never produced a usable answer.
    pub failed_requests: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Input tokens served from cache, across every recorded request.
    pub total_cache_read_tokens: u64,
    /// Input tokens written into the cache, across every recorded request.
    pub total_cache_creation_tokens: u64,
    /// Output tokens spent on reasoning that was never emitted.
    pub total_reasoning_tokens: u64,
    /// What Copilot billed in total, in nano-AI-units. Since the move to
    /// usage-based billing this, not the premium-request count, is the meter.
    pub total_nano_aiu: u64,
    /// Net effect of the prompt cache on the bill, in nano-AI-units.
    pub cache_saved_nano_aiu: i64,
    /// Copilot premium requests consumed. Fractional because some models
    /// bill at a discounted multiplier.
    pub premium_requests: f64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

struct Inner {
    records: VecDeque<RequestRecord>,
    stats: Stats,
}

/// Bounded ring-buffer store of recent requests with running totals.
pub struct RequestStore {
    inner: Mutex<Inner>,
    max_entries: usize,
}

impl RequestStore {
    pub fn new(max_entries: usize) -> Self {
        RequestStore {
            inner: Mutex::new(Inner {
                records: VecDeque::new(),
                stats: Stats::default(),
            }),
            max_entries,
        }
    }

    /// Records a completed request and updates aggregate statistics.
    pub fn add(&self, record: RequestRecord) {
        let mut inner = self.inner.lock().unwrap();
        if record.failed() {
            inner.stats.failed_requests += 1;
        } else {
            inner.stats.request_count += 1;
        }
        // Token and billing totals count either way. A stream cut off partway
        // still consumed what it consumed, and hiding that would understate
        // the bill.
        inner.stats.total_input_tokens += record.input_tokens;
        inner.stats.total_output_tokens += record.output_tokens;
        inner.stats.total_cache_read_tokens += record.cache_read_input_tokens;
        inner.stats.total_cache_creation_tokens += record.cache_creation_input_tokens;
        inner.stats.total_reasoning_tokens += record.reasoning_tokens.unwrap_or(0);
        inner.stats.total_nano_aiu += record.billed_nano_aiu.unwrap_or(0);
        inner.stats.cache_saved_nano_aiu += record.cache_saved_nano_aiu.unwrap_or(0);
        // Only count what the catalog actually priced; an unknown multiplier
        // is left out rather than assumed to be a full premium request.
        if let Some(multiplier) = record.premium_multiplier {
            inner.stats.premium_requests += multiplier;
        }
        inner.stats.bytes_received += record.request_size as u64;
        inner.stats.bytes_sent += record.response_size as u64;
        inner.records.push_front(record);
        while inner.records.len() > self.max_entries {
            inner.records.pop_back();
        }
    }

    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats.clone()
    }

    /// Returns a page of the most recent records and the total count.
    pub fn recent(&self, per_page: usize, offset: usize) -> (Vec<RequestRecord>, usize) {
        let inner = self.inner.lock().unwrap();
        let total = inner.records.len();
        let items = inner
            .records
            .iter()
            .skip(offset)
            .take(per_page)
            .cloned()
            .collect();
        (items, total)
    }

    /// Runs `f` over the retained records (newest first) while holding the
    /// store lock. Used by aggregation endpoints (`/metrics`, the audit APIs)
    /// so they never clone the entire ring buffer — records may carry captured
    /// request/response bodies when debug mode is enabled.
    pub fn with_records<R>(
        &self,
        f: impl FnOnce(&mut dyn Iterator<Item = &RequestRecord>) -> R,
    ) -> R {
        let inner = self.inner.lock().unwrap();
        let mut iter = inner.records.iter();
        f(&mut iter)
    }

    /// Returns the records matching `predicate` for a page, plus the total
    /// number of matches. Filtering happens under the lock so only the records
    /// actually returned are cloned.
    pub fn filtered_page(
        &self,
        per_page: usize,
        offset: usize,
        predicate: impl Fn(&RequestRecord) -> bool,
    ) -> (Vec<RequestRecord>, usize) {
        let inner = self.inner.lock().unwrap();
        let total = inner.records.iter().filter(|r| predicate(r)).count();
        let items = inner
            .records
            .iter()
            .filter(|r| predicate(r))
            .skip(offset)
            .take(per_page)
            .cloned()
            .collect();
        (items, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(endpoint: &str, status: u16) -> RequestRecord {
        RequestRecord {
            id: endpoint.to_string(),
            timestamp: String::new(),
            endpoint: endpoint.to_string(),
            model: "m".into(),
            translated_model: None,
            status_code: status,
            request_size: 1,
            response_size: 2,
            input_tokens: 3,
            output_tokens: 4,
            output_tokens_final: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_tokens: None,
            billed_nano_aiu: None,
            cache_saved_nano_aiu: None,
            premium_multiplier: None,
            upstream_idle_max_ms: None,
            session_id: None,
            keepalive_probes: None,
            duration: 0.5,
            request_body: None,
            response_body: None,
            message_count: None,
            tool_count: None,
            tool_names: None,
            stop_reason: None,
            tools_called: None,
            is_agent_initiated: None,
            estimated_cost_usd: None,
            prompt_cache_hit: None,
            failure_kind: None,
        }
    }

    #[test]
    fn evicts_oldest_beyond_capacity() {
        let store = RequestStore::new(2);
        store.add(record("a", 200));
        store.add(record("b", 200));
        store.add(record("c", 200));
        let (items, total) = store.recent(10, 0);
        assert_eq!(total, 2);
        // Newest first; the oldest record was evicted.
        assert_eq!(items[0].endpoint, "c");
        assert_eq!(items[1].endpoint, "b");
        // Aggregate stats keep counting evicted requests.
        assert_eq!(store.stats().request_count, 3);
        assert_eq!(store.stats().total_input_tokens, 9);
    }

    #[test]
    fn filtered_page_paginates_matches_only() {
        let store = RequestStore::new(10);
        store.add(record("/v1/messages", 200));
        store.add(record("/v1/chat/completions", 500));
        store.add(record("/v1/messages", 429));

        let (items, total) = store.filtered_page(10, 0, |r| r.endpoint == "/v1/messages");
        assert_eq!(total, 2);
        assert_eq!(items.len(), 2);

        // Offset applies to the filtered set, not the raw store.
        let (items, total) = store.filtered_page(1, 1, |r| r.endpoint == "/v1/messages");
        assert_eq!(total, 2);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status_code, 200);
    }

    #[test]
    fn stats_track_cache_buckets_and_premium_requests() {
        let store = RequestStore::new(10);

        // A fully-cached Claude Code turn: almost the entire prompt is a
        // cache read, so the uncached remainder is a rounding error.
        let mut cached = record("/v1/messages", 200);
        cached.input_tokens = 103_673;
        cached.cache_read_input_tokens = 101_940;
        cached.cache_creation_input_tokens = 1_731;
        cached.premium_multiplier = Some(1.0);
        store.add(cached);

        // A discounted model still counts, at its own rate.
        let mut cheap = record("/v1/messages", 200);
        cheap.input_tokens = 500;
        cheap.premium_multiplier = Some(0.33);
        store.add(cheap);

        // Embeddings burn no premium allowance; an unknown multiplier must
        // not be invented as 1.0.
        let mut free = record("/v1/embeddings", 200);
        free.input_tokens = 20;
        free.premium_multiplier = None;
        store.add(free);

        let s = store.stats();
        assert_eq!(s.total_input_tokens, 103_673 + 500 + 20);
        assert_eq!(s.total_cache_read_tokens, 101_940);
        assert_eq!(s.total_cache_creation_tokens, 1_731);
        assert!(
            (s.premium_requests - 1.33).abs() < 1e-9,
            "premium_requests was {}",
            s.premium_requests
        );
    }

    #[test]
    fn with_records_visits_every_record() {
        let store = RequestStore::new(10);
        store.add(record("a", 200));
        store.add(record("b", 404));
        let (count, errors) = store.with_records(|records| {
            let mut count = 0;
            let mut errors = 0;
            for r in records {
                count += 1;
                if r.status_code >= 400 {
                    errors += 1;
                }
            }
            (count, errors)
        });
        assert_eq!(count, 2);
        assert_eq!(errors, 1);
    }
}
