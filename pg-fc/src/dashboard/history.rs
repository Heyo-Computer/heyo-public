//! In-memory time series of the live-VM count for the monitoring page's chart.
//!
//! A background task samples "how many VMs are running" on a fixed interval and
//! appends to a bounded ring buffer; the monitoring page renders whatever is in
//! it as an inline SVG. Deliberately **not persisted** — it's a recent-history
//! chart, not an audit trail, so it resets on restart rather than carrying the
//! complexity of a durable series. At the default cadence the buffer holds a few
//! hours, which is what a "is the fleet growing / did that reap land" glance
//! wants.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use super::model;
use super::state::DashState;

/// How often the sampler records the live-VM count.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
/// How many samples to retain (≈ `SAMPLE_INTERVAL * CAPACITY` of history — 6h at
/// the defaults). Bounds both memory and the width of the rendered chart.
pub const CAPACITY: usize = 360;

/// One observation: unix seconds and the number of running VMs then.
#[derive(Clone, Copy)]
pub struct Sample {
    pub t: u64,
    pub live: u32,
}

/// Bounded, thread-safe ring buffer of [`Sample`]s. Oldest is dropped once full.
pub struct VmHistory {
    samples: Mutex<VecDeque<Sample>>,
    capacity: usize,
}

impl VmHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    fn push(&self, s: Sample) {
        let mut q = self.samples.lock().unwrap();
        while q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// Oldest-to-newest copy of the retained samples, for rendering.
    pub fn snapshot(&self) -> Vec<Sample> {
        self.samples.lock().unwrap().iter().copied().collect()
    }
}

/// Spawn the background sampler: record the running-VM count every
/// [`SAMPLE_INTERVAL`], appending to `st.history`. Samples once up front so the
/// chart isn't empty for the first interval. A failed daemon read is logged and
/// skipped (no gap-filling); the next tick tries again.
pub fn spawn_sampler(st: DashState) {
    let history = st.history.clone();
    tokio::spawn(async move {
        loop {
            match model::build_rows(&st).await {
                Ok(rows) => {
                    let live = rows.iter().filter(|r| r.is_running()).count() as u32;
                    history.push(Sample {
                        t: now_unix(),
                        live,
                    });
                    debug!("vm-history: sampled {live} live VM(s)");
                }
                Err(e) => warn!("vm-history sampler: reading VM inventory failed: {e:#}"),
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
