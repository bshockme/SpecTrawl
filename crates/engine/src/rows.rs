//! Waterfall rows: windowed power averaged over a few FFT blocks.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

pub use api::Row;

/// The most recent `depth` rows of one source.
pub struct RowHistory {
    depth: usize,
    rows: Mutex<VecDeque<Arc<Row>>>,
}

impl RowHistory {
    pub fn new(depth: usize) -> RowHistory {
        RowHistory { depth, rows: Mutex::new(VecDeque::with_capacity(depth + 1)) }
    }

    pub fn latest(&self) -> Option<Arc<Row>> {
        self.rows.lock().back().cloned()
    }

    /// Rows with `end_sample > end_sample_after`, oldest first.
    pub fn since(&self, end_sample_after: u64) -> Vec<Arc<Row>> {
        self.rows.lock().iter().filter(|r| r.end_sample > end_sample_after).cloned().collect()
    }

    /// The newest `n` rows (fewer if fewer are held), oldest first.
    pub fn newest(&self, n: usize) -> Vec<Arc<Row>> {
        let rows = self.rows.lock();
        rows.iter().skip(rows.len().saturating_sub(n)).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.rows.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.lock().is_empty()
    }

    /// Append `row`, dropping the oldest past `depth`.
    pub(crate) fn push(&self, row: Row) {
        let mut rows = self.rows.lock();
        rows.push_back(Arc::new(row));
        while rows.len() > self.depth {
            rows.pop_front();
        }
    }
}
