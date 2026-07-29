//! Getting records in.
//!
//! # Why push, and why bind every interface
//!
//! The daemon cannot supply application logs: its `GET /sandboxes/:id/logs`
//! only ever holds the output of explicit `execute_command` calls, capped at
//! 1000 in-memory entries and discarded when the sandbox stops. An app started
//! through app-lb's `start_command` writes to a file inside the guest that the
//! daemon never sees. So senders push to us.
//!
//! Each microVM sits on its own /30 — the host is at `guest_ip - 1` — so there
//! is no single address every guest could be pointed at. Guests instead send to
//! their **default gateway**, which is always this host on their subnet, and we
//! bind `0.0.0.0` so we are reachable on all of them.
//!
//! # Dropping is a feature
//!
//! Everything funnels through one bounded queue. When it is full, records are
//! dropped and counted; they are never allowed to block the sender. A customer's
//! application must not stall because the collector fell behind, so losing
//! telemetry is always the correct trade against applying backpressure.

pub mod http;
pub mod syslog;

use crate::store::schema::Record;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// The write end of the ingest queue, shared by every source.
#[derive(Clone)]
pub struct Sink {
    tx: mpsc::Sender<Record>,
    stats: Arc<Stats>,
}

#[derive(Default)]
pub struct Stats {
    pub accepted: AtomicU64,
    pub dropped: AtomicU64,
}

impl Sink {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Record>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                tx,
                stats: Arc::new(Stats::default()),
            },
            rx,
        )
    }

    /// Queue a record. Returns `false` if it was dropped.
    ///
    /// `try_send` rather than `send` is the whole point: `send` would await a
    /// free slot and turn a backed-up collector into latency in whatever is
    /// shipping to us.
    pub fn send(&self, record: Record) -> bool {
        match self.tx.try_send(record) {
            Ok(()) => {
                self.stats.accepted.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn accepted(&self) -> u64 {
        self.stats.accepted.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.stats.dropped.load(Ordering::Relaxed)
    }
}

/// Constant-time comparison for the ingest token, so a matching prefix can't be
/// timed out of it. Same approach as app-lb's dashboard auth.
pub fn token_matches(expected: &str, presented: Option<&str>) -> bool {
    let Some(presented) = presented.and_then(|v| v.strip_prefix("Bearer ")) else {
        return false;
    };
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::LogRecord;

    fn log() -> Record {
        Record::Log(LogRecord {
            ts_millis: 1_785_260_096_000,
            deployment: "demo".into(),
            backend: None,
            source: "stdout".into(),
            level: None,
            message: "x".into(),
            fields: None,
            host: None,
        })
    }

    #[tokio::test]
    async fn a_full_queue_drops_instead_of_blocking() {
        // The defining property of this layer. If this ever blocks, a slow
        // collector becomes latency in somebody's application.
        let (sink, _rx) = Sink::new(2);
        assert!(sink.send(log()));
        assert!(sink.send(log()));

        // Third send has nowhere to go. It must return immediately, not await.
        let overflow = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            async { sink.send(log()) },
        )
        .await
        .expect("send must never block");

        assert!(!overflow, "the record was dropped");
        assert_eq!(sink.accepted(), 2);
        assert_eq!(sink.dropped(), 1);
    }

    #[tokio::test]
    async fn draining_the_queue_makes_room_again() {
        let (sink, mut rx) = Sink::new(1);
        assert!(sink.send(log()));
        assert!(!sink.send(log()));
        rx.recv().await.unwrap();
        assert!(sink.send(log()), "a drained slot is reusable");
    }

    #[test]
    fn token_comparison_requires_the_bearer_scheme_and_exact_value() {
        assert!(token_matches("s3cret", Some("Bearer s3cret")));
        assert!(!token_matches("s3cret", Some("Bearer wrong")));
        assert!(!token_matches("s3cret", Some("Bearer s3cre")), "prefix");
        assert!(!token_matches("s3cret", Some("s3cret")), "no scheme");
        assert!(!token_matches("s3cret", Some("Basic s3cret")), "wrong scheme");
        assert!(!token_matches("s3cret", None));
    }
}
