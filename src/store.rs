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
    /// Estimated cost in USD based on token counts and model rates
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

/// Aggregate statistics returned by `/api/stats`.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub request_count: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
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
        inner.stats.request_count += 1;
        inner.stats.total_input_tokens += record.input_tokens;
        inner.stats.total_output_tokens += record.output_tokens;
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
