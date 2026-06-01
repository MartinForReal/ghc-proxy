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
}
