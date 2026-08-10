//! A bounded, lock-free ring of recent invocations per function.
//!
//! This is what the dashboard's "recent invocations" table renders. It is
//! deliberately *not* the durable record — that is the `_RESULTS` JetStream
//! stream, which survives a restart and can be replayed. This exists because
//! polling a JetStream stream every two seconds to fill a table would be a lot
//! of machinery for a view that only ever wants the last few dozen rows.
//!
//! Bounded on two axes, both of which have bitten real systems: a fixed slot
//! count so a busy function cannot grow it without limit, and per-record output
//! truncation so a single chatty invocation cannot either.

use arc_swap::ArcSwap;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bus::{InvokeEvent, InvokeResult, Outcome};

/// Per-record output cap. Enough to see what a function printed; far short of
/// what it would take to make the ring a memory problem.
pub const MAX_OUTPUT_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct InvocationRecord {
    pub invocation_id: String,
    pub function_id: String,
    pub sandbox_id: String,
    pub source: String,
    pub outcome: Outcome,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// True when the output above was cut, so the UI can say so rather than
    /// silently showing a partial line as if it were the whole thing.
    pub truncated: bool,
    pub duration_ms: u64,
    pub queue_wait_ms: u64,
    pub attempt: u32,
    pub finished_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Cut a string to at most `max` bytes without splitting a UTF-8 character.
fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    // Walk back to a character boundary; slicing mid-character would panic.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

impl InvocationRecord {
    pub fn build(
        event: &InvokeEvent,
        result: &InvokeResult,
        queue_wait_ms: u64,
        finished_at_ms: u64,
    ) -> Self {
        let (stdout, out_cut) = truncate(&result.stdout, MAX_OUTPUT_BYTES);
        let (stderr, err_cut) = truncate(&result.stderr, MAX_OUTPUT_BYTES);
        Self {
            invocation_id: result.invocation_id.clone(),
            function_id: result.function_id.clone(),
            sandbox_id: result.sandbox_id.clone(),
            source: event.source.as_str().into(),
            outcome: result.outcome,
            exit_code: result.exit_code,
            stdout,
            stderr,
            truncated: out_cut || err_cut,
            duration_ms: result.duration_ms,
            queue_wait_ms,
            attempt: result.attempt,
            finished_at_ms,
            error: result.error.clone(),
        }
    }
}

/// A fixed-slot ring with a monotonic write cursor.
///
/// Writers take no lock: bump the cursor, store into `cursor % len`. A reader
/// walking backwards from the cursor can race a writer and observe a slot that
/// has already been overwritten by a newer record — acceptable for a monitoring
/// view, and far cheaper than serialising every invocation behind a mutex.
#[derive(Debug)]
struct Ring {
    slots: Box<[ArcSwap<Option<InvocationRecord>>]>,
    cursor: AtomicU64,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            slots: (0..capacity)
                .map(|_| ArcSwap::from_pointee(None))
                .collect(),
            cursor: AtomicU64::new(0),
        }
    }

    fn push(&self, record: InvocationRecord) {
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) as usize % self.slots.len();
        self.slots[idx].store(Arc::new(Some(record)));
    }

    /// Newest first, up to `limit`.
    fn recent(&self, limit: usize) -> Vec<InvocationRecord> {
        let written = self.cursor.load(Ordering::Relaxed);
        let take = limit.min(self.slots.len()).min(written as usize);
        (0..take)
            .filter_map(|i| {
                // Walk backwards from the most recently written slot.
                let idx = (written - 1 - i as u64) as usize % self.slots.len();
                self.slots[idx].load().as_ref().clone()
            })
            .collect()
    }
}

/// Recent invocations for every function.
#[derive(Debug)]
pub struct Results {
    per_function: ArcSwap<HashMap<String, Arc<Ring>>>,
    capacity: usize,
}

impl Results {
    pub fn new(capacity: usize) -> Self {
        Self {
            per_function: ArcSwap::from_pointee(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Get, or insert-then-get, a function's ring. Copy-on-write insert, so the
    /// clone happens at most once per function id ever seen.
    fn ring(&self, function_id: &str) -> Arc<Ring> {
        if let Some(r) = self.per_function.load().get(function_id) {
            return r.clone();
        }
        let created = Arc::new(Ring::new(self.capacity));
        let mut installed = created.clone();
        self.per_function.rcu(|current| {
            let mut next = (**current).clone();
            installed = next
                .entry(function_id.to_string())
                .or_insert(created.clone())
                .clone();
            next
        });
        installed
    }

    pub fn push(&self, record: InvocationRecord) {
        self.ring(&record.function_id).push(record);
    }

    /// Newest first. An unknown function yields an empty list rather than an
    /// error, so a freshly-registered function still renders a table.
    pub fn recent(&self, function_id: &str, limit: usize) -> Vec<InvocationRecord> {
        self.per_function
            .load()
            .get(function_id)
            .map(|r| r.recent(limit))
            .unwrap_or_default()
    }

    /// Drop a function's history, e.g. when it is deregistered.
    pub fn forget(&self, function_id: &str) {
        self.per_function.rcu(|current| {
            let mut next = (**current).clone();
            next.remove(function_id);
            next
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventSource;

    fn record(id: &str) -> InvocationRecord {
        InvocationRecord {
            invocation_id: id.into(),
            function_id: "demo".into(),
            sandbox_id: "sb-1".into(),
            source: "invoke".into(),
            outcome: Outcome::Success,
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 5,
            queue_wait_ms: 1,
            attempt: 1,
            finished_at_ms: 0,
            error: None,
        }
    }

    #[test]
    fn recent_returns_newest_first() {
        let r = Results::new(10);
        for i in 0..3 {
            r.push(record(&format!("inv-{i}")));
        }
        let ids: Vec<_> = r
            .recent("demo", 10)
            .into_iter()
            .map(|x| x.invocation_id)
            .collect();
        assert_eq!(ids, ["inv-2", "inv-1", "inv-0"]);
    }

    /// Regression: the ring reported `capacity` rows from the moment it was
    /// created, so a function with three invocations rendered a table padded
    /// with empty rows.
    #[test]
    fn a_partially_filled_ring_yields_only_what_was_written() {
        let r = Results::new(100);
        r.push(record("inv-0"));
        assert_eq!(r.recent("demo", 100).len(), 1);
    }

    #[test]
    fn the_ring_wraps_and_keeps_the_newest() {
        let r = Results::new(3);
        for i in 0..5 {
            r.push(record(&format!("inv-{i}")));
        }
        let ids: Vec<_> = r
            .recent("demo", 10)
            .into_iter()
            .map(|x| x.invocation_id)
            .collect();
        assert_eq!(
            ids,
            ["inv-4", "inv-3", "inv-2"],
            "a full ring drops the oldest, not the newest",
        );
    }

    #[test]
    fn an_unknown_function_yields_an_empty_list() {
        let r = Results::new(10);
        assert!(r.recent("never-seen", 10).is_empty());
    }

    #[test]
    fn forget_drops_a_functions_history() {
        let r = Results::new(10);
        r.push(record("inv-0"));
        assert_eq!(r.recent("demo", 10).len(), 1);
        r.forget("demo");
        assert!(r.recent("demo", 10).is_empty());
    }

    #[test]
    fn functions_do_not_share_a_ring() {
        let r = Results::new(10);
        r.push(record("a-0"));
        let mut other = record("b-0");
        other.function_id = "other".into();
        r.push(other);
        assert_eq!(r.recent("demo", 10).len(), 1);
        assert_eq!(r.recent("other", 10).len(), 1);
    }

    /// Regression: truncation sliced at a fixed byte offset, which panicked the
    /// dispatcher whenever a function's output happened to have a multi-byte
    /// character straddling the cut.
    #[test]
    fn truncation_does_not_split_a_utf8_character() {
        // 'é' is two bytes, so a cut at an odd offset lands mid-character.
        let s = "é".repeat(4000);
        let (cut, truncated) = truncate(&s, MAX_OUTPUT_BYTES);
        assert!(truncated);
        assert!(cut.len() <= MAX_OUTPUT_BYTES);
        assert!(
            cut.chars().all(|c| c == 'é'),
            "no partial character may survive the cut"
        );
    }

    #[test]
    fn short_output_is_not_marked_truncated() {
        let (out, truncated) = truncate("hello", MAX_OUTPUT_BYTES);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    /// A single chatty invocation must not be able to grow the ring; the record
    /// says so explicitly so the UI does not present a partial line as complete.
    #[test]
    fn a_record_caps_its_output_and_admits_it() {
        let event = InvokeEvent::new("demo", None, EventSource::Invoke);
        let result = InvokeResult {
            invocation_id: "inv-1".into(),
            function_id: "demo".into(),
            sandbox_id: "sb-1".into(),
            outcome: Outcome::Success,
            exit_code: 0,
            stdout: "x".repeat(MAX_OUTPUT_BYTES * 10),
            stderr: String::new(),
            duration_ms: 1,
            attempt: 1,
            error: None,
        };
        let rec = InvocationRecord::build(&event, &result, 0, 0);
        assert_eq!(rec.stdout.len(), MAX_OUTPUT_BYTES);
        assert!(rec.truncated);
    }

    #[test]
    fn a_zero_capacity_ring_does_not_divide_by_zero() {
        let r = Results::new(0);
        r.push(record("inv-0"));
        assert_eq!(
            r.recent("demo", 10).len(),
            1,
            "capacity is floored at 1 rather than panicking on a modulo by zero"
        );
    }
}
