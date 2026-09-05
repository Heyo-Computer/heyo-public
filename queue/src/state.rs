//! What the dashboard is looking at: one scrape, normalised, plus the history
//! needed to turn counters into rates.
//!
//! ## Why anything is computed here at all
//!
//! `/varz` reports `in_msgs` and `out_msgs` as totals since the server started.
//! A single scrape of a cumulative counter says nothing about throughput — a
//! server that handled ten million messages last week and none since looks
//! identical to one saturating a link right now. **Throughput is a difference
//! between two scrapes**, so somebody has to hold the previous one, and that is
//! the job this module exists to do.
//!
//! The same is true one level down. A stream's `messages` is depth, not volume:
//! on a `WorkQueue` stream it sits at zero however busy the stream is, because
//! every message is deleted the moment it is acked. The publish rate has to come
//! from `last_seq`, which only goes up, and the drain rate from the consumer's
//! `ack_floor`. Depth alone cannot tell a quiet queue from a fast one.
//!
//! ## Counter resets are a gap, not a spike
//!
//! nats-server restarting sets every counter back to zero. Subtracting across
//! that boundary yields either a negative number or, unsigned, an enormous one —
//! a spike on the chart at exactly the moment somebody is trying to read it. So
//! each sample carries `Option<f64>`, the reset case is `None`, and the chart
//! draws a gap. The run is identified by `varz.start`, which changes on restart
//! even when the counters happen not to have gone backwards yet.

use crate::config::Config;
use crate::monitor::{ConnInfo, Jsz, Monitor, Varz};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

/// Subjects listed per client before the row is truncated.
///
/// The full list is unbounded — a service with a wildcard subscription per
/// function can hold hundreds — and a table cell is not where anyone reads
/// those. The row keeps the total count beside the sample so it is obvious that
/// it is one.
const MAX_CLIENT_SUBJECTS: usize = 8;

/// Floor on how long an unmoving `ack_floor` has to sit before a consumer is
/// called stalled, whatever its `ack_wait` says.
///
/// A consumer with a one-second `ack_wait` is not in trouble a second after its
/// last ack; it is between messages. Without a floor, the fast consumers — the
/// healthy ones — would be the ones permanently flagged.
const STALL_FLOOR_MS: i64 = 60_000;

/// Multiple of `ack_wait` an unmoving `ack_floor` is given before it counts.
///
/// One `ack_wait` is the redelivery deadline: a consumer that has not acked in
/// that long is late, but a single missed deadline is ordinary. Two is late
/// twice over.
const STALL_ACK_WAIT_MULTIPLE: i64 = 2;

/// A consumer with no configured `ack_wait` — the server's default is 30s, but
/// an absent field here means the config was not reported — is given this
/// instead, so the stall rule never divides by an assumed zero.
const STALL_UNKNOWN_ACK_WAIT_MS: i64 = 300_000;

// ---------------------------------------------------------------------------
// The shape the API serves
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub generated_at_ms: i64,
    /// The scrape interval, so the page can size its own refresh and say how
    /// old "fresh" is rather than guessing.
    pub poll_secs: u64,
    /// Whether the last scrape succeeded. A `false` here with a `server` block
    /// still present is the normal shape of a server that just went away: the
    /// last known reading is kept and stamped, because a stale reading clearly
    /// marked stale beats a blank page.
    pub connected: bool,
    pub polled_at_ms: Option<i64>,
    pub error: Option<String>,
    pub monitor_url: String,
    pub server: Option<ServerRow>,
    pub throughput: Option<Rates>,
    pub totals: Totals,
    pub accounts: Vec<AccountRow>,
    pub clients: Clients,
    pub history: Vec<Sample>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ServerRow {
    pub name: String,
    pub id: String,
    pub version: String,
    pub host: String,
    pub port: u16,
    pub uptime: String,
    pub started_at: String,
    pub connections: u64,
    pub total_connections: u64,
    pub subscriptions: u64,
    /// Cumulative. Non-zero means the server gave up writing to a client at
    /// some point — on a WorkQueue stream, work that had to be redelivered.
    pub slow_consumers: u64,
    pub mem_bytes: u64,
    pub cores: u32,
    pub cpu_percent: f64,
    pub max_connections: u64,
    pub max_payload: u64,
    pub js_memory_bytes: u64,
    pub js_storage_bytes: u64,
    pub js_max_storage_bytes: u64,
    pub js_store_dir: String,
    pub api_total: u64,
    pub api_errors: u64,
}

/// Per-second rates over the window between the last two scrapes.
///
/// Every field is optional for one reason: the first scrape after startup, and
/// the first after a server restart, have nothing to difference against.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Rates {
    pub in_msgs_per_sec: Option<f64>,
    pub out_msgs_per_sec: Option<f64>,
    pub in_bytes_per_sec: Option<f64>,
    pub out_bytes_per_sec: Option<f64>,
    /// How far apart the two scrapes actually were, which is not always
    /// `poll_secs` — a slow scrape stretches it.
    pub window_secs: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub accounts: usize,
    pub streams: usize,
    pub consumers: usize,
    /// Messages held across every stream: the queue depth of the whole server.
    pub depth: u64,
    pub bytes: u64,
    /// Summed consumer `num_pending`: work matched to a worker that has not
    /// been handed to it yet. Distinct from `depth`, which counts messages
    /// nothing is subscribed to as well.
    pub pending: u64,
    pub ack_pending: u64,
    pub redelivered: u64,
    pub api_errors: u64,
    /// Streams whose retention deletes on ack and that have no consumer at all.
    /// Nothing will ever drain them; on a WorkQueue stream that is a backlog
    /// with no worker, which is the failure this page exists to make obvious.
    pub orphaned_streams: usize,
    pub stalled_consumers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountRow {
    pub name: String,
    pub storage_bytes: u64,
    pub memory_bytes: u64,
    pub api_total: u64,
    pub api_errors: u64,
    pub streams: Vec<StreamRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamRow {
    pub account: String,
    pub name: String,
    pub subjects: Vec<String>,
    pub retention: String,
    pub storage: String,
    pub discard: String,
    pub max_age_secs: Option<u64>,
    pub messages: u64,
    pub bytes: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub last_message_ms: Option<i64>,
    pub consumer_count: u64,
    /// Messages published per second, from the `last_seq` delta. `None` on the
    /// first sight of a stream, and whenever the sequence went backwards —
    /// which means the stream was deleted and recreated under the same name.
    pub published_per_sec: Option<f64>,
    /// A WorkQueue or Interest stream with no consumers. Nothing is draining
    /// it, and on WorkQueue nothing ever will without one.
    pub orphaned: bool,
    pub consumers: Vec<ConsumerRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsumerRow {
    pub name: String,
    pub filter_subject: String,
    pub ack_policy: String,
    pub ack_wait_secs: Option<f64>,
    pub max_deliver: i64,
    pub num_pending: u64,
    pub num_ack_pending: u64,
    pub num_redelivered: u64,
    pub num_waiting: u64,
    pub delivered_stream_seq: u64,
    pub ack_floor_stream_seq: u64,
    /// Messages acked per second, from the `ack_floor` delta — the rate this
    /// worker is actually draining at, which beside `published_per_sec` says
    /// whether a queue is filling or emptying.
    pub acked_per_sec: Option<f64>,
    /// Work is outstanding and `ack_floor` has not moved for longer than this
    /// consumer's redelivery deadline allows. That is a worker that took
    /// messages and stopped, not one that is between them.
    pub stalled: bool,
    /// How long `ack_floor` has been where it is. Present whenever the value
    /// has been observed twice.
    pub idle_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Clients {
    /// What the server says it has, which is larger than `rows.len()` whenever
    /// the limit truncated the page.
    pub total: u64,
    pub truncated: bool,
    pub rows: Vec<ClientRow>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientRow {
    pub cid: u64,
    pub name: String,
    pub kind: String,
    pub account: String,
    pub lang: String,
    pub version: String,
    pub address: String,
    pub uptime: String,
    pub idle: String,
    pub rtt: String,
    pub subscriptions: u64,
    /// A sample, capped at [`MAX_CLIENT_SUBJECTS`]; `subscriptions` is the real
    /// count.
    pub subjects: Vec<String>,
    pub in_msgs: u64,
    pub out_msgs: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    /// Bytes the server is holding for a client that is not reading them. This
    /// climbing is what precedes a slow-consumer disconnect.
    pub pending_bytes: u64,
}

/// One point on the charts.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Sample {
    pub at_ms: i64,
    pub in_msgs_per_sec: Option<f64>,
    pub out_msgs_per_sec: Option<f64>,
    pub in_bytes_per_sec: Option<f64>,
    pub out_bytes_per_sec: Option<f64>,
    pub connections: u64,
    pub depth: u64,
    pub pending: u64,
}

// ---------------------------------------------------------------------------
// Held between scrapes
// ---------------------------------------------------------------------------

/// The previous scrape, kept only so the next one can be differenced against it.
#[derive(Debug, Clone, Default)]
struct Previous {
    at_ms: i64,
    /// `varz.start`. A change means a new server run and so new counters.
    run: String,
    in_msgs: u64,
    out_msgs: u64,
    in_bytes: u64,
    out_bytes: u64,
    /// `account/stream` → `last_seq`.
    streams: HashMap<String, u64>,
    /// `account/stream/consumer` → its ack floor and when it last moved.
    consumers: HashMap<String, ConsumerPrevious>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ConsumerPrevious {
    ack_floor_stream_seq: u64,
    /// When `ack_floor_stream_seq` was last observed to change. Deliberately
    /// *not* updated on every scrape — the whole point is how long it has been
    /// still.
    since_ms: i64,
}

/// Everything the API reads, behind one lock.
///
/// A `std::sync::RwLock` rather than tokio's: nothing awaits while it is held —
/// the writer builds the whole snapshot before taking it, and readers serialize
/// under it — so the async-aware lock would buy nothing but a dependency on
/// being called from a runtime. Poisoning is tolerated rather than propagated,
/// because a panic in one scrape is not a reason to stop serving the last good
/// reading.
pub struct Store {
    inner: RwLock<Inner>,
    poll_secs: u64,
    monitor_url: String,
    history_points: usize,
}

#[derive(Default)]
struct Inner {
    connected: bool,
    polled_at_ms: Option<i64>,
    error: Option<String>,
    server: Option<ServerRow>,
    throughput: Option<Rates>,
    totals: Totals,
    accounts: Vec<AccountRow>,
    clients: Clients,
    history: VecDeque<Sample>,
    previous: Option<Previous>,
}

impl Store {
    pub fn new(cfg: &Config) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            poll_secs: cfg.poll_interval.as_secs().max(1),
            monitor_url: cfg.monitor_url.clone(),
            history_points: cfg.history_points,
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn overview(&self) -> Overview {
        let inner = self.read();
        Overview {
            generated_at_ms: now_ms(),
            poll_secs: self.poll_secs,
            connected: inner.connected,
            polled_at_ms: inner.polled_at_ms,
            error: inner.error.clone(),
            monitor_url: self.monitor_url.clone(),
            server: inner.server.clone(),
            throughput: inner.throughput,
            totals: inner.totals.clone(),
            accounts: inner.accounts.clone(),
            clients: inner.clients.clone(),
            history: inner.history.iter().copied().collect(),
        }
    }

    /// Record a failed scrape.
    ///
    /// The last good reading is deliberately left in place: one failed poll
    /// against a server that is merely restarting should stamp the page stale,
    /// not empty it. `connected: false` plus `polled_at_ms` is what the header
    /// renders that from.
    pub fn record_error(&self, error: String) {
        let mut inner = self.write();
        inner.connected = false;
        inner.polled_at_ms = Some(now_ms());
        inner.error = Some(error);
    }

    /// Fold one successful scrape in, deriving every rate against the previous.
    pub fn record(&self, varz: Varz, jsz: Jsz, connz: crate::monitor::Connz, max_clients: usize) {
        let at_ms = now_ms();
        let mut inner = self.write();

        // Take the previous scrape out rather than borrowing it: the folding
        // below builds the next one as it goes.
        let previous = inner.previous.take();
        let window = previous.as_ref().and_then(|p| {
            let dt = (at_ms - p.at_ms) as f64 / 1000.0;
            // Same-millisecond scrapes divide by ~zero. Two scrapes closer than
            // 100ms cannot say anything useful about a per-second rate anyway.
            (dt >= 0.1 && p.run == varz.start).then_some(dt)
        });

        let throughput = window.map(|dt| {
            let p = previous.as_ref().expect("window implies a previous scrape");
            Rates {
                in_msgs_per_sec: rate(varz.in_msgs, p.in_msgs, dt),
                out_msgs_per_sec: rate(varz.out_msgs, p.out_msgs, dt),
                in_bytes_per_sec: rate(varz.in_bytes, p.in_bytes, dt),
                out_bytes_per_sec: rate(varz.out_bytes, p.out_bytes, dt),
                window_secs: Some(dt),
            }
        });

        let (accounts, totals, next_streams, next_consumers) =
            fold_jsz(&jsz, previous.as_ref(), window, at_ms);
        let clients = fold_connz(&connz, max_clients);

        inner.history.push_back(Sample {
            at_ms,
            in_msgs_per_sec: throughput.and_then(|t| t.in_msgs_per_sec),
            out_msgs_per_sec: throughput.and_then(|t| t.out_msgs_per_sec),
            in_bytes_per_sec: throughput.and_then(|t| t.in_bytes_per_sec),
            out_bytes_per_sec: throughput.and_then(|t| t.out_bytes_per_sec),
            connections: varz.connections,
            depth: totals.depth,
            pending: totals.pending,
        });
        while inner.history.len() > self.history_points {
            inner.history.pop_front();
        }

        inner.previous = Some(Previous {
            at_ms,
            run: varz.start.clone(),
            in_msgs: varz.in_msgs,
            out_msgs: varz.out_msgs,
            in_bytes: varz.in_bytes,
            out_bytes: varz.out_bytes,
            streams: next_streams,
            consumers: next_consumers,
        });
        inner.server = Some(server_row(&varz));
        inner.throughput = throughput;
        inner.totals = totals;
        inner.accounts = accounts;
        inner.clients = clients;
        inner.connected = true;
        inner.polled_at_ms = Some(at_ms);
        inner.error = None;
    }
}

/// Scrape on a timer until told to stop.
///
/// `MissedTickBehavior::Delay`, so a scrape that overran its interval does not
/// come back to a burst of catch-up ticks against a server that is already
/// struggling to answer.
pub async fn run(
    store: Arc<Store>,
    monitor: Monitor,
    interval: std::time::Duration,
    max_clients: usize,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // All three in one go: they describe one moment, and serialising
                // them would smear the rates across three round trips.
                let (varz, jsz, connz) = tokio::join!(
                    monitor.varz(),
                    monitor.jsz(),
                    monitor.connz(max_clients),
                );
                match (varz, jsz, connz) {
                    (Ok(varz), Ok(jsz), Ok(connz)) => store.record(varz, jsz, connz, max_clients),
                    // Any one of the three failing fails the scrape. A partial
                    // reading would advance the counters the next rate is
                    // differenced against while leaving the panel it belongs to
                    // stale, which is a worse lie than "this poll failed".
                    (varz, jsz, connz) => {
                        let error = first_error(varz.err(), jsz.err(), connz.err());
                        tracing::warn!(error = %error, "monitoring scrape failed");
                        store.record_error(error);
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
}

fn first_error(
    varz: Option<crate::monitor::MonitorError>,
    jsz: Option<crate::monitor::MonitorError>,
    connz: Option<crate::monitor::MonitorError>,
) -> String {
    varz.map(|e| e.to_string())
        .or_else(|| jsz.map(|e| e.to_string()))
        .or_else(|| connz.map(|e| e.to_string()))
        .unwrap_or_else(|| "unknown error".into())
}

// ---------------------------------------------------------------------------
// Pure folding, which is the part worth testing
// ---------------------------------------------------------------------------

fn server_row(varz: &Varz) -> ServerRow {
    ServerRow {
        name: varz.server_name.clone(),
        id: varz.server_id.clone(),
        version: varz.version.clone(),
        host: varz.host.clone(),
        port: varz.port,
        uptime: varz.uptime.clone(),
        started_at: varz.start.clone(),
        connections: varz.connections,
        total_connections: varz.total_connections,
        subscriptions: varz.subscriptions,
        slow_consumers: varz.slow_consumers,
        mem_bytes: varz.mem,
        cores: varz.cores,
        cpu_percent: varz.cpu,
        max_connections: varz.max_connections,
        max_payload: varz.max_payload,
        js_memory_bytes: varz.jetstream.stats.memory,
        js_storage_bytes: varz.jetstream.stats.storage,
        js_max_storage_bytes: varz.jetstream.config.max_storage,
        js_store_dir: varz.jetstream.config.store_dir.clone(),
        api_total: varz.jetstream.stats.api.total,
        api_errors: varz.jetstream.stats.api.errors,
    }
}

type Folded = (
    Vec<AccountRow>,
    Totals,
    HashMap<String, u64>,
    HashMap<String, ConsumerPrevious>,
);

fn fold_jsz(jsz: &Jsz, previous: Option<&Previous>, window: Option<f64>, at_ms: i64) -> Folded {
    let mut accounts = Vec::with_capacity(jsz.account_details.len());
    let mut totals = Totals {
        accounts: jsz.account_details.len(),
        ..Default::default()
    };
    let mut next_streams = HashMap::new();
    let mut next_consumers = HashMap::new();

    for account in &jsz.account_details {
        let mut streams = Vec::with_capacity(account.stream_detail.len());
        for stream in &account.stream_detail {
            let stream_key = format!("{}/{}", account.name, stream.name);
            next_streams.insert(stream_key.clone(), stream.state.last_seq);

            let published_per_sec = window.zip(previous).and_then(|(dt, p)| {
                p.streams
                    .get(&stream_key)
                    .and_then(|&was| rate(stream.state.last_seq, was, dt))
            });

            let mut consumers = Vec::with_capacity(stream.consumer_detail.len());
            for consumer in &stream.consumer_detail {
                let key = format!("{stream_key}/{}", consumer.name);
                let floor = consumer.ack_floor.stream_seq;
                let was = previous.and_then(|p| p.consumers.get(&key)).copied();

                // `since_ms` only advances when the floor actually moves. An
                // unchanged floor keeps the timestamp it was first seen at,
                // which is what makes "how long has this been stuck" a fact
                // rather than an estimate.
                let since_ms = match was {
                    Some(prev) if prev.ack_floor_stream_seq == floor => prev.since_ms,
                    _ => at_ms,
                };
                next_consumers.insert(
                    key,
                    ConsumerPrevious {
                        ack_floor_stream_seq: floor,
                        since_ms,
                    },
                );

                let acked_per_sec = window
                    .zip(was)
                    .and_then(|(dt, prev)| rate(floor, prev.ack_floor_stream_seq, dt));
                let idle_ms = was.map(|_| at_ms - since_ms);
                let outstanding = consumer.num_pending + consumer.num_ack_pending;
                let stalled = outstanding > 0
                    && idle_ms.is_some_and(|idle| idle >= stall_after_ms(consumer.config.ack_wait));

                totals.pending += consumer.num_pending;
                totals.ack_pending += consumer.num_ack_pending;
                totals.redelivered += consumer.num_redelivered;
                if stalled {
                    totals.stalled_consumers += 1;
                }

                consumers.push(ConsumerRow {
                    name: consumer.name.clone(),
                    filter_subject: consumer.config.filter_subject.clone(),
                    ack_policy: consumer.config.ack_policy.clone(),
                    ack_wait_secs: (consumer.config.ack_wait > 0)
                        .then(|| consumer.config.ack_wait as f64 / 1_000_000_000.0),
                    max_deliver: consumer.config.max_deliver,
                    num_pending: consumer.num_pending,
                    num_ack_pending: consumer.num_ack_pending,
                    num_redelivered: consumer.num_redelivered,
                    num_waiting: consumer.num_waiting,
                    delivered_stream_seq: consumer.delivered.stream_seq,
                    ack_floor_stream_seq: floor,
                    acked_per_sec,
                    stalled,
                    idle_ms,
                });
            }

            let orphaned = drains_on_ack(&stream.config.retention) && consumers.is_empty();
            if orphaned {
                totals.orphaned_streams += 1;
            }
            totals.streams += 1;
            totals.consumers += consumers.len();
            totals.depth += stream.state.messages;
            totals.bytes += stream.state.bytes;

            streams.push(StreamRow {
                account: account.name.clone(),
                name: stream.name.clone(),
                subjects: stream.config.subjects.clone(),
                retention: stream.config.retention.clone(),
                storage: stream.config.storage.clone(),
                discard: stream.config.discard.clone(),
                max_age_secs: (stream.config.max_age > 0)
                    .then_some((stream.config.max_age / 1_000_000_000) as u64),
                messages: stream.state.messages,
                bytes: stream.state.bytes,
                first_seq: stream.state.first_seq,
                last_seq: stream.state.last_seq,
                last_message_ms: rfc3339_to_ms(&stream.state.last_ts),
                consumer_count: stream.state.consumer_count,
                published_per_sec,
                orphaned,
                consumers,
            });
        }

        totals.api_errors += account.api.errors;
        accounts.push(AccountRow {
            name: account.name.clone(),
            storage_bytes: account.storage,
            memory_bytes: account.memory,
            api_total: account.api.total,
            api_errors: account.api.errors,
            streams,
        });
    }

    (accounts, totals, next_streams, next_consumers)
}

fn fold_connz(connz: &crate::monitor::Connz, max_clients: usize) -> Clients {
    let rows = connz
        .connections
        .iter()
        .take(max_clients)
        .map(client_row)
        .collect::<Vec<_>>();
    Clients {
        total: connz.total.max(connz.num_connections),
        truncated: (rows.len() as u64) < connz.total,
        rows,
    }
}

fn client_row(conn: &ConnInfo) -> ClientRow {
    ClientRow {
        cid: conn.cid,
        name: conn.name.clone(),
        kind: conn.kind.clone(),
        account: conn.account.clone(),
        lang: conn.lang.clone(),
        version: conn.version.clone(),
        address: format!("{}:{}", conn.ip, conn.port),
        uptime: conn.uptime.clone(),
        idle: conn.idle.clone(),
        rtt: conn.rtt.clone(),
        subscriptions: conn.subscriptions,
        subjects: conn
            .subscriptions_list
            .iter()
            .take(MAX_CLIENT_SUBJECTS)
            .cloned()
            .collect(),
        in_msgs: conn.in_msgs,
        out_msgs: conn.out_msgs,
        in_bytes: conn.in_bytes,
        out_bytes: conn.out_bytes,
        pending_bytes: conn.pending_bytes,
    }
}

/// A per-second rate, or `None` where one cannot honestly be computed.
///
/// The `current < previous` case is a server that restarted between scrapes.
/// Unsigned subtraction there would wrap to something astronomical, which is
/// how a restart turns into the largest spike on the chart.
fn rate(current: u64, previous: u64, window_secs: f64) -> Option<f64> {
    if window_secs <= 0.0 || current < previous {
        return None;
    }
    Some((current - previous) as f64 / window_secs)
}

/// How long an unmoving `ack_floor` is given before the consumer counts as
/// stalled.
fn stall_after_ms(ack_wait_nanos: i64) -> i64 {
    if ack_wait_nanos <= 0 {
        return STALL_UNKNOWN_ACK_WAIT_MS;
    }
    let ack_wait_ms = ack_wait_nanos / 1_000_000;
    (ack_wait_ms.saturating_mul(STALL_ACK_WAIT_MULTIPLE)).max(STALL_FLOOR_MS)
}

/// Retention policies under which a message leaves the stream once it is acked,
/// and so under which having no consumer means having no drain.
fn drains_on_ack(retention: &str) -> bool {
    matches!(retention, "workqueue" | "interest")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// RFC3339 to epoch milliseconds, with Go's zero time read as "never".
///
/// Go marshals a zero `time.Time` as `0001-01-01T00:00:00Z`, and NATS sends it
/// for a stream that has never held a message. Passing that through as a
/// timestamp puts "56000 years ago" on the page; `None` renders as an em dash.
pub fn rfc3339_to_ms(raw: &str) -> Option<i64> {
    if raw.is_empty() || raw.starts_with("0001-01-01") {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{JszAccount, JszConsumer, JszConsumerConfig, JszSeqPair, JszStream};

    fn stream(name: &str, retention: &str, messages: u64, last_seq: u64) -> JszStream {
        let mut s = JszStream {
            name: name.into(),
            ..Default::default()
        };
        s.config.retention = retention.into();
        s.state.messages = messages;
        s.state.last_seq = last_seq;
        s
    }

    fn consumer(name: &str, ack_floor: u64, pending: u64, ack_wait_secs: i64) -> JszConsumer {
        JszConsumer {
            name: name.into(),
            config: JszConsumerConfig {
                ack_wait: ack_wait_secs * 1_000_000_000,
                ..Default::default()
            },
            ack_floor: JszSeqPair {
                stream_seq: ack_floor,
                ..Default::default()
            },
            num_pending: pending,
            ..Default::default()
        }
    }

    fn jsz_of(streams: Vec<JszStream>) -> Jsz {
        Jsz {
            account_details: vec![JszAccount {
                name: "$G".into(),
                stream_detail: streams,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The whole reason this module exists: two scrapes of a cumulative counter
    /// become a rate, and one scrape becomes nothing.
    #[test]
    fn throughput_needs_two_scrapes_and_reports_the_difference_between_them() {
        assert_eq!(rate(1_000, 900, 10.0), Some(10.0));
        assert_eq!(rate(1_000, 1_000, 10.0), Some(0.0), "flat is a real zero");
        assert_eq!(rate(1_000, 900, 0.0), None, "no window, no rate");
    }

    /// A restart sets the counters back to zero. Unsigned subtraction would
    /// wrap and put the largest spike on the chart at the least convenient
    /// moment.
    #[test]
    fn a_counter_that_went_backwards_is_a_gap_rather_than_a_spike() {
        assert_eq!(
            rate(5, 1_000_000, 10.0),
            None,
            "a restart must draw a gap, never a rate",
        );
    }

    /// `varz.start` catches the restart the counters do not: a server that came
    /// back and has already passed its old totals would otherwise be
    /// differenced across the boundary.
    #[test]
    fn a_new_server_run_suppresses_the_rate_even_when_counters_did_not_go_backwards() {
        let cfg = Config::default();
        let store = Store::new(&cfg);
        let first = Varz {
            start: "2026-09-05T01:00:00Z".into(),
            in_msgs: 100,
            ..Default::default()
        };
        store.record(first, Jsz::default(), Default::default(), 10);
        let restarted = Varz {
            start: "2026-09-05T02:00:00Z".into(),
            in_msgs: 900,
            ..Default::default()
        };
        store.record(restarted, Jsz::default(), Default::default(), 10);
        assert!(
            store.overview().throughput.is_none(),
            "a rate across a restart boundary is not a rate",
        );
    }

    /// Depth is a gauge and volume is not. A WorkQueue stream that has moved a
    /// million messages holds zero, so `last_seq` is the only field a publish
    /// rate can come from.
    #[test]
    fn a_workqueue_stream_reports_a_publish_rate_while_holding_no_messages() {
        let (_, _, first_streams, _) = fold_jsz(
            &jsz_of(vec![stream("JOBS", "workqueue", 0, 100)]),
            None,
            None,
            0,
        );
        let previous = Previous {
            at_ms: 0,
            streams: first_streams,
            ..Default::default()
        };
        let (accounts, totals, ..) = fold_jsz(
            &jsz_of(vec![stream("JOBS", "workqueue", 0, 150)]),
            Some(&previous),
            Some(10.0),
            10_000,
        );
        assert_eq!(totals.depth, 0, "an empty workqueue really is empty");
        assert_eq!(
            accounts[0].streams[0].published_per_sec,
            Some(5.0),
            "50 sequences in 10s is 5/s, which depth alone could never show",
        );
    }

    /// A stream deleted and recreated under the same name restarts at sequence
    /// one. That is the same wrap hazard as a server restart, one level down.
    #[test]
    fn a_stream_recreated_under_the_same_name_reports_no_rate_rather_than_a_negative_one() {
        let previous = Previous {
            at_ms: 0,
            streams: HashMap::from([("$G/JOBS".to_string(), 5_000_u64)]),
            ..Default::default()
        };
        let (accounts, ..) = fold_jsz(
            &jsz_of(vec![stream("JOBS", "workqueue", 0, 3)]),
            Some(&previous),
            Some(10.0),
            10_000,
        );
        assert_eq!(accounts[0].streams[0].published_per_sec, None);
    }

    /// The signal the page exists for: a WorkQueue stream with no consumer will
    /// never drain, because on WorkQueue retention nothing but an ack removes a
    /// message.
    #[test]
    fn a_workqueue_stream_with_no_consumer_is_flagged_as_having_no_drain() {
        let (accounts, totals, ..) = fold_jsz(
            &jsz_of(vec![
                stream("JOBS", "workqueue", 42, 42),
                stream("EVENTS", "limits", 42, 42),
            ]),
            None,
            None,
            0,
        );
        assert!(
            accounts[0].streams[0].orphaned,
            "workqueue with no consumer"
        );
        assert!(
            !accounts[0].streams[1].orphaned,
            "a limits stream is aged out by the server, so no consumer is normal",
        );
        assert_eq!(totals.orphaned_streams, 1);
    }

    /// A stalled consumer is one holding work whose ack floor has not moved for
    /// longer than its own redelivery deadline allows — not merely one that is
    /// idle.
    #[test]
    fn a_consumer_holding_work_with_an_unmoving_ack_floor_is_stalled() {
        let mut s = stream("JOBS", "workqueue", 10, 100);
        s.consumer_detail = vec![consumer("worker", 90, 10, 30)];
        let (_, _, _, first_consumers) = fold_jsz(&jsz_of(vec![s.clone()]), None, None, 0);
        let previous = Previous {
            at_ms: 0,
            consumers: first_consumers,
            ..Default::default()
        };
        // 30s ack_wait, doubled, is 60s; 90s of no movement is past it.
        let (accounts, totals, ..) =
            fold_jsz(&jsz_of(vec![s]), Some(&previous), Some(90.0), 90_000);
        let c = &accounts[0].streams[0].consumers[0];
        assert!(c.stalled, "held 10 messages and acked nothing for 90s");
        assert_eq!(c.idle_ms, Some(90_000));
        assert_eq!(totals.stalled_consumers, 1);
    }

    /// A fast consumer between messages must not be flagged. Without the floor,
    /// a one-second `ack_wait` would make the healthiest workers the loudest.
    #[test]
    fn a_consumer_with_a_short_ack_wait_is_not_stalled_seconds_after_its_last_ack() {
        assert_eq!(stall_after_ms(1_000_000_000), STALL_FLOOR_MS);
        assert_eq!(stall_after_ms(60_000_000_000), 120_000);
        assert_eq!(stall_after_ms(0), STALL_UNKNOWN_ACK_WAIT_MS);
    }

    /// An idle consumer with nothing outstanding is idle, which is not a fault.
    #[test]
    fn a_consumer_with_no_outstanding_work_is_never_stalled() {
        let mut s = stream("JOBS", "workqueue", 0, 100);
        s.consumer_detail = vec![consumer("worker", 100, 0, 30)];
        let (_, _, _, first) = fold_jsz(&jsz_of(vec![s.clone()]), None, None, 0);
        let previous = Previous {
            at_ms: 0,
            consumers: first,
            ..Default::default()
        };
        let (accounts, totals, ..) =
            fold_jsz(&jsz_of(vec![s]), Some(&previous), Some(600.0), 600_000);
        assert!(!accounts[0].streams[0].consumers[0].stalled);
        assert_eq!(totals.stalled_consumers, 0);
    }

    /// A failed scrape must stamp the page stale, not blank it. The evidence
    /// from the last good reading is what somebody diagnosing the outage has.
    #[test]
    fn a_failed_scrape_keeps_the_last_good_reading() {
        let store = Store::new(&Config::default());
        store.record(
            Varz {
                server_name: "qfn-nats-1".into(),
                connections: 4,
                ..Default::default()
            },
            jsz_of(vec![stream("JOBS", "workqueue", 7, 7)]),
            Default::default(),
            10,
        );
        store.record_error("connection refused".into());

        let overview = store.overview();
        assert!(!overview.connected);
        assert_eq!(overview.error.as_deref(), Some("connection refused"));
        assert_eq!(
            overview.server.expect("kept").name,
            "qfn-nats-1",
            "the last good reading survives a failed poll",
        );
        assert_eq!(overview.totals.depth, 7);
    }

    #[test]
    fn history_is_bounded_by_the_configured_point_count() {
        let cfg = Config {
            history_points: 3,
            ..Default::default()
        };
        let store = Store::new(&cfg);
        for i in 0..10 {
            store.record(
                Varz {
                    in_msgs: i * 10,
                    ..Default::default()
                },
                Jsz::default(),
                Default::default(),
                10,
            );
        }
        assert_eq!(store.overview().history.len(), 3);
    }

    /// Go's zero time is "never", and rendering it as a date puts the year one
    /// on the page.
    #[test]
    fn the_go_zero_timestamp_reads_as_no_data_rather_than_year_one() {
        assert_eq!(rfc3339_to_ms("0001-01-01T00:00:00Z"), None);
        assert_eq!(rfc3339_to_ms(""), None);
        assert_eq!(rfc3339_to_ms("not a date"), None);
        assert_eq!(
            rfc3339_to_ms("2026-09-05T01:00:00Z"),
            Some(1_788_570_000_000),
        );
    }

    /// `connz` pages; the row count is not the connection count, and a page
    /// that quietly showed 256 of 4000 would be read as the whole truth.
    #[test]
    fn a_truncated_client_page_says_so_and_keeps_the_real_total() {
        let connz = crate::monitor::Connz {
            num_connections: 3,
            total: 900,
            connections: vec![ConnInfo::default(); 3],
            ..Default::default()
        };
        let clients = fold_connz(&connz, 2);
        assert_eq!(clients.rows.len(), 2);
        assert_eq!(clients.total, 900);
        assert!(clients.truncated);
    }
}
