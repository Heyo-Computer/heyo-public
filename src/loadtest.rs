//! Load harness for the cold-start path — reproduces, in one process, the
//! degradation a pooler host shows once it tracks thousands of sandboxes.
//!
//! WHY THIS EXISTS. A fresh host serves a brand-new schema in about a second.
//! A host that has been up for a while takes 30s+, and during most of that the
//! daemon shows no sign of having been asked for anything — while host CPU and
//! memory sit under 20%. That shape (latency scaling with *inventory* rather
//! than with load) is what this harness measures, and it can't be reproduced
//! against a real daemon without first provisioning thousands of real VMs.
//!
//! HOW. [`Daemon`] is an in-process heyvmd stand-in speaking the same wire
//! protocol the SDK expects, holding a synthetic fleet of arbitrary size. The
//! pooler is aimed at it with `PG_VM_POOL_DAEMON_URL` (see
//! [`crate::vm::daemon_base_url`]), so every code path under test is the real
//! one — the same `resolve_sandbox`, the same store, the same gates.
//!
//! WHAT IT ISOLATES. The stub answers instantly and does no VM work, so any
//! latency that grows with fleet size is the *pooler's own* bookkeeping, not
//! resource contention. It also counts every request and every byte it serves,
//! which is the direct measurement of the N+1 question: how much daemon work
//! does one new VM cost, and does that cost depend on how many VMs already
//! exist?
//!
//! RUNNING. The sweeps are `#[ignore]`d — they take a minute and print a table
//! rather than asserting:
//!
//! ```text
//! cargo test --release loadtest -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Cells share one process-wide stub and one process-wide environment, so
//! every test here takes [`exclusive`] for its duration — `--test-threads=1`
//! only keeps the interleaved output readable.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::json;

use crate::config::Config;

// ---------------------------------------------------------------- the daemon

/// One synthetic sandbox. Only the fields the pooler actually reads.
#[derive(Clone)]
struct Vm {
    id: String,
    name: String,
    running: bool,
}

impl Vm {
    fn view(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "name": self.name,
            "status": if self.running { "running" } else { "stopped" },
            "status_changed_at": "2026-08-19T00:00:00Z",
            "image": "pg",
            "size_class": "micro",
            "is_deployed": true,
            "uptime_secs": 0,
            "urls": [],
            // A host-reachable address the pooler will dial for Postgres.
            // Port 5432 on loopback is (almost certainly) refused, which ends
            // the readiness wait quickly instead of burning the whole timeout.
            "guest_ip": "127.0.0.1",
        })
    }
}

/// Per-endpoint request and byte counters. Bytes are what makes the N+1
/// legible: a cold start that pulls a megabyte off the daemon is doing work
/// proportional to the fleet, whatever the clock says on any given run.
#[derive(Default)]
struct Metrics {
    calls: StdMutex<BTreeMap<&'static str, u64>>,
    bytes: AtomicU64,
}

impl Metrics {
    fn hit(&self, endpoint: &'static str, bytes: usize) {
        *self.calls.lock().unwrap().entry(endpoint).or_default() += 1;
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn reset(&self) {
        self.calls.lock().unwrap().clear();
        self.bytes.store(0, Ordering::Relaxed);
    }

    fn total_calls(&self) -> u64 {
        self.calls.lock().unwrap().values().sum()
    }

    fn total_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// `GET /deployed-sandboxes x8, POST /sandbox-deploy x8` — the per-cell
    /// breakdown, so a surprising byte count can be traced to an endpoint.
    fn breakdown(&self) -> String {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(ep, n)| format!("{ep} x{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

struct Daemon {
    vms: StdMutex<BTreeMap<String, Vm>>,
    metrics: Metrics,
    next_id: AtomicU64,
    /// What one full listing costs the real daemon, and whether those cost are
    /// paid concurrently.
    ///
    /// The stub's own answer is a `serde_json` encode, which is nothing like
    /// heyvmd: there, `GET /deployed-sandboxes` clones every VM's metadata
    /// under a lock and probes every live handle. That is the multiplier
    /// between "the pooler demands the whole inventory per cold start" (which
    /// the byte counters measure directly) and the wall-clock a client feels.
    /// Set it from a real host's measured listing time to model that host.
    list_delay: StdMutex<Duration>,
    /// Held across the delay when set, modelling a daemon that serves listings
    /// under one lock — the state heyvmd is in when its workers are parked on
    /// blocking VM work.
    list_serial: StdMutex<bool>,
    list_gate: tokio::sync::Mutex<()>,
    /// Whether the stub honors `GET /deployed-sandboxes?name=` (a current
    /// heyvmd) or ignores the query and serves the full inventory (an old
    /// one). Default true; flip off to model the fallback.
    supports_name_filter: StdMutex<bool>,
}

impl Daemon {
    /// Replace the fleet with `n` stopped VMs named `pg-seed-<i>`, as a pooler
    /// host looks after `n` schemas have been served and idle-reaped.
    fn seed(&self, n: usize) {
        let mut vms = self.vms.lock().unwrap();
        vms.clear();
        for i in 0..n {
            let id = seed_id(i);
            vms.insert(
                id.clone(),
                Vm {
                    id,
                    name: format!("pg-{}", seed_schema(i)),
                    running: false,
                },
            );
        }
    }
}

fn seed_id(i: usize) -> String {
    format!("sb-seed-{i:06}")
}

fn seed_schema(i: usize) -> String {
    format!("seed-{i:06}")
}

async fn list(
    State(d): State<Arc<Daemon>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // A current heyvmd answers `?name=` with the one exact match (or an empty
    // array) straight off its name index — no fleet scan, no listing delay.
    // An old daemon's handler took only `State`, so with the filter off the
    // param is ignored and the full inventory goes over the wire.
    if *d.supports_name_filter.lock().unwrap()
        && let Some(name) = params.get("name")
    {
        let found: Vec<serde_json::Value> = d
            .vms
            .lock()
            .unwrap()
            .values()
            .filter(|vm| &vm.name == name)
            .map(Vm::view)
            .collect();
        let body = serde_json::to_string(&found).unwrap();
        d.metrics.hit("GET /deployed-sandboxes?name", body.len());
        return ([("content-type", "application/json")], body);
    }
    let all: Vec<serde_json::Value> = d.vms.lock().unwrap().values().map(Vm::view).collect();
    let body = serde_json::to_string(&all).unwrap();
    d.metrics.hit("GET /deployed-sandboxes", body.len());
    let delay = *d.list_delay.lock().unwrap();
    if !delay.is_zero() {
        let serial = *d.list_serial.lock().unwrap();
        if serial {
            let _one_at_a_time = d.list_gate.lock().await;
            tokio::time::sleep(delay).await;
        } else {
            tokio::time::sleep(delay).await;
        }
    }
    ([("content-type", "application/json")], body)
}

async fn get_one(State(d): State<Arc<Daemon>>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    let found = d.vms.lock().unwrap().get(&id).map(Vm::view);
    match found {
        Some(v) => {
            let body = serde_json::to_string(&v).unwrap();
            d.metrics.hit("GET /deployed-sandboxes/{id}", body.len());
            ([("content-type", "application/json")], body).into_response()
        }
        None => {
            d.metrics.hit("GET /deployed-sandboxes/{id} (404)", 0);
            (StatusCode::NOT_FOUND, format!("Sandbox not found: {id}")).into_response()
        }
    }
}

/// The deploy. Answers like a current heyvmd — 202-equivalent, VM immediately
/// visible and `running` — so the harness measures the pooler's cost of asking,
/// with the daemon's own build cost set to zero.
async fn deploy(State(d): State<Arc<Daemon>>, body: String) -> impl IntoResponse {
    let name = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_default();
    let id = format!("sb-new-{:06}", d.next_id.fetch_add(1, Ordering::Relaxed));
    d.vms.lock().unwrap().insert(
        id.clone(),
        Vm {
            id: id.clone(),
            name,
            running: true,
        },
    );
    let out = json!({"id": id, "status": "running"}).to_string();
    d.metrics.hit("POST /sandbox-deploy", out.len());
    ([("content-type", "application/json")], out)
}

async fn start(State(d): State<Arc<Daemon>>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    let known = match d.vms.lock().unwrap().get_mut(&id) {
        Some(vm) => {
            vm.running = true;
            true
        }
        None => false,
    };
    if known {
        d.metrics.hit("POST /sandbox/{id}/start", 2);
        ([("content-type", "application/json")], "{}").into_response()
    } else {
        d.metrics.hit("POST /sandbox/{id}/start (404)", 0);
        (StatusCode::NOT_FOUND, format!("Sandbox not found: {id}")).into_response()
    }
}

async fn stop(State(d): State<Arc<Daemon>>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    if let Some(vm) = d.vms.lock().unwrap().get_mut(&id) {
        vm.running = false;
    }
    d.metrics.hit("POST /sandbox/{id}/stop", 2);
    ([("content-type", "application/json")], "{}")
}

async fn kill(State(d): State<Arc<Daemon>>, AxPath(id): AxPath<String>) -> impl IntoResponse {
    d.vms.lock().unwrap().remove(&id);
    d.metrics.hit("DELETE /deployed-sandboxes/{id}", 2);
    ([("content-type", "application/json")], "{}")
}

async fn ok_json(State(d): State<Arc<Daemon>>, AxPath(_id): AxPath<String>) -> impl IntoResponse {
    d.metrics.hit("POST /deployed-sandboxes/{id}/*", 2);
    ([("content-type", "application/json")], "{}")
}

/// The process-wide stub, started once on its own runtime.
///
/// Its own runtime and thread, deliberately: every `#[tokio::test]` builds and
/// drops a runtime of its own, and a server spawned onto one of those would
/// die with the first cell. Binding synchronously also means the port — and so
/// `PG_VM_POOL_DAEMON_URL` — is known before any pooler code runs, which
/// matters because `daemon_base_url()` caches its answer on first read.
fn daemon() -> &'static Arc<Daemon> {
    static STUB: OnceLock<Arc<Daemon>> = OnceLock::new();
    STUB.get_or_init(|| {
        let d = Arc::new(Daemon {
            vms: StdMutex::new(BTreeMap::new()),
            metrics: Metrics::default(),
            next_id: AtomicU64::new(0),
            list_delay: StdMutex::new(Duration::ZERO),
            list_serial: StdMutex::new(false),
            list_gate: tokio::sync::Mutex::const_new(()),
            supports_name_filter: StdMutex::new(true),
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding daemon stub");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        // SAFETY: single-threaded setup — this runs inside a `OnceLock`
        // initializer, before any cell has spawned a task that reads the
        // environment.
        unsafe { std::env::set_var("PG_VM_POOL_DAEMON_URL", format!("http://{addr}")) };

        let app = axum::Router::new()
            .route("/deployed-sandboxes", get(list))
            .route(
                "/deployed-sandboxes/{id}",
                get(get_one).delete(kill),
            )
            .route("/deployed-sandboxes/{id}/ttl", post(ok_json))
            .route("/deployed-sandboxes/{id}/restart", post(ok_json))
            .route("/sandbox-deploy", post(deploy))
            .route("/sandbox/{id}/start", post(start))
            .route("/sandbox/{id}/stop", post(stop))
            .with_state(d.clone());

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("daemon stub runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        d
    })
}

// --------------------------------------------------------------- the harness

/// Guards the process environment while a `Config` is built from it.
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

/// Exclusive use of the stub for the caller's whole test. Shared with every
/// other test that touches process-wide VM state — a bring-up here takes a
/// reclaim boot permit, so `reclaim`'s tests must not run alongside these.
async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    crate::vm::test_exclusive().await
}

/// A pooler configuration pointed at the stub, with a state file holding
/// `fleet` schema→VM rows — the durable inventory whose size is the
/// independent variable.
///
/// `ready_timeout` is deliberately short. The stub's VMs have no Postgres
/// behind them, so every bring-up ends in a readiness failure; a small budget
/// makes that tail a constant the cells share instead of the thing being
/// measured.
fn config_for(fleet: usize, warm_spares: usize) -> Config {
    let _env = ENV_LOCK.lock().unwrap();
    daemon(); // sets PG_VM_POOL_DAEMON_URL before anything reads it

    let dir = std::env::temp_dir().join(format!("pg-fc-loadtest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating harness state dir");
    let state = dir.join(format!("registry-{fleet}.tsv"));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut rows = String::new();
    for i in 0..fleet {
        rows.push_str(&format!("{}\t{}\t{now}\tlive\n", seed_schema(i), seed_id(i)));
    }
    std::fs::write(&state, rows).expect("seeding the state file");

    let set = |k: &str, v: &str| {
        // SAFETY: `ENV_LOCK` is held, and cells run single-threaded (see the
        // module docs on `--test-threads=1`).
        unsafe { std::env::set_var(k, v) }
    };
    set("PG_VM_POOL_STATE_FILE", state.to_str().unwrap());
    set("PG_VM_POOL_READY_TIMEOUT_SECS", "2");
    set("PG_VM_POOL_CONNECT_TIMEOUT_SECS", "2");
    set("PG_VM_POOL_ADMIT_TIMEOUT_SECS", "5");
    set("PG_VM_POOL_IDLE_TIMEOUT_SECS", "0");
    set("PG_VM_POOL_WARM_SPARES", &warm_spares.to_string());

    crate::pending::init(dir.join("pending.tsv"));
    // Process-wide state the cells would otherwise leak into each other: the
    // name→id cache warms as a side effect of every resolve, and the stub's
    // filter support is a per-cell scenario knob.
    crate::inventory::reset();
    *daemon().supports_name_filter.lock().unwrap() = true;
    Config::from_env().expect("building the harness config")
}

/// Latency distribution for one cell.
struct Stats {
    label: String,
    fleet: usize,
    conc: usize,
    p50: Duration,
    p95: Duration,
    max: Duration,
    calls_per_op: f64,
    bytes_per_op: f64,
    breakdown: String,
}

fn summarize(
    label: String,
    fleet: usize,
    conc: usize,
    mut samples: Vec<Duration>,
    ops: usize,
) -> Stats {
    samples.sort();
    let at = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    let m = &daemon().metrics;
    Stats {
        label,
        fleet,
        conc,
        p50: at(0.50),
        p95: at(0.95),
        max: *samples.last().unwrap(),
        calls_per_op: m.total_calls() as f64 / ops as f64,
        bytes_per_op: m.total_bytes() as f64 / ops as f64,
        breakdown: m.breakdown(),
    }
}

fn header() {
    println!(
        "\n{:<22} {:>7} {:>5} {:>9} {:>9} {:>9} {:>9} {:>12}",
        "scenario", "fleet", "conc", "p50", "p95", "max", "calls/op", "bytes/op"
    );
    println!("{}", "-".repeat(92));
}

impl Stats {
    fn print(&self) {
        println!(
            "{:<22} {:>7} {:>5} {:>9} {:>9} {:>9} {:>9.1} {:>12}",
            self.label,
            self.fleet,
            self.conc,
            format!("{:.0}ms", self.p50.as_secs_f64() * 1000.0),
            format!("{:.0}ms", self.p95.as_secs_f64() * 1000.0),
            format!("{:.0}ms", self.max.as_secs_f64() * 1000.0),
            self.calls_per_op,
            human(self.bytes_per_op),
        );
        println!("{:>22}   └ {}", "", self.breakdown);
    }
}

fn human(bytes: f64) -> String {
    match bytes {
        b if b >= 1_048_576.0 => format!("{:.1}MB", b / 1_048_576.0),
        b if b >= 1024.0 => format!("{:.1}KB", b / 1024.0),
        b => format!("{b:.0}B"),
    }
}

/// How the modelled daemon answers a full listing.
///
/// The stub's own encode cost is nothing like heyvmd's, so leaving it at
/// [`Listing::Instant`] measures the pooler's *demand* and nothing else. The
/// other two put a measured real-host listing time behind that demand, which
/// is what turns a shape into a wall-clock a client would feel.
#[derive(Clone, Copy)]
enum Listing {
    /// As fast as JSON can be encoded — isolates the pooler's own cost.
    Instant,
    /// `d` per listing, answered concurrently: a healthy daemon that is simply
    /// slow because the inventory is large.
    Concurrent(Duration),
    /// `d` per listing, one at a time: a daemon whose async workers are parked
    /// on blocking VM work (`mke2fs`, `debugfs`, rootfs clones), which is the
    /// state a bring-up burst puts it in.
    Serialized(Duration),
}

impl Listing {
    fn apply(self, d: &Daemon) {
        let (delay, serial) = match self {
            Listing::Instant => (Duration::ZERO, false),
            Listing::Concurrent(d) => (d, false),
            Listing::Serialized(d) => (d, true),
        };
        *d.list_delay.lock().unwrap() = delay;
        *d.list_serial.lock().unwrap() = serial;
    }
}

/// Whether the schemas a cell asks for are already bound to a VM in the store.
#[derive(Clone, Copy, PartialEq)]
enum Ask {
    /// A schema the pooler has never served: no stored id, so `resolve_sandbox`
    /// falls through to find-by-name. This is what a *new* customer hits.
    New,
    /// A schema whose VM the pooler already knows by id — the reattach path,
    /// which skips find-by-name entirely.
    Known,
}

/// Run `conc` concurrent cold resolves against a fleet of `fleet` VMs and
/// return the distribution.
///
/// Resolve rather than a full `checkout`: everything after it needs a real
/// Postgres inside the VM, and its failure against the stub would add a
/// constant that buries the signal. Resolve *is* the window in question — the
/// span between a client connecting and the daemon being asked to do anything
/// about its VM.
async fn resolve_cell(
    label: &str,
    fleet: usize,
    conc: usize,
    ask: Ask,
    listing: Listing,
) -> Stats {
    let cfg = config_for(fleet, 12);
    daemon().seed(fleet);
    daemon().metrics.reset();
    listing.apply(daemon());

    // Warm-spare pool present but empty, as it is on a host whose spares are
    // all claimed — so `bound_ids()` (an O(fleet) clone of the whole store) is
    // still computed per bring-up, exactly as in production.
    let spares = crate::spares::SparePool::new(12);
    let bound: HashSet<String> = (0..fleet).map(seed_id).collect();

    let cfg = Arc::new(cfg);
    let spares = Arc::new(spares);
    let bound = Arc::new(bound);

    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..conc {
        let (cfg, spares, bound) = (cfg.clone(), spares.clone(), bound.clone());
        let (name, known_id) = match ask {
            // Spread the picks across the fleet so no cell is accidentally
            // measuring one hot map bucket.
            Ask::Known => {
                let idx = if fleet == 0 { 0 } else { i * (fleet / conc.max(1)).max(1) % fleet };
                (format!("pg-{}", seed_schema(idx)), Some(seed_id(idx)))
            }
            Ask::New => (format!("pg-fresh-{i}"), None),
        };
        tasks.push(tokio::spawn(async move {
            let t = Instant::now();
            let _ = crate::vm::resolve_sandbox(
                &cfg,
                &name,
                false,
                known_id.as_deref(),
                Some((&spares, &bound)),
            )
            .await;
            t.elapsed()
        }));
    }
    let mut samples = Vec::with_capacity(conc);
    for t in tasks {
        samples.push(t.await.expect("resolve task panicked"));
    }
    let wall = started.elapsed();
    let stats = summarize(label.to_string(), fleet, conc, samples, conc);
    println!("{:>22}   (wall {:.1}s)", "", wall.as_secs_f64());
    stats
}

// ----------------------------------------------------------------- the sweeps

/// Does the cost of bringing up ONE new VM depend on how many VMs the host
/// already tracks? Concurrency is fixed, so fleet size is the only variable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load sweep: run explicitly with --ignored --nocapture --test-threads=1"]
async fn cold_start_cost_versus_fleet_size() {
    let _exclusive = exclusive().await;
    header();
    let mut rows = Vec::new();
    for fleet in [0, 250, 1_000, 5_000] {
        rows.push(resolve_cell("new schema", fleet, 8, Ask::New, Listing::Instant).await);
        rows.last().unwrap().print();
    }
    // No fleet-0 cell here: with nothing seeded there is no known schema to
    // ask for, and the stored id would 404 into the new-schema path — a row
    // labelled "known" that measured the opposite.
    for fleet in [250, 1_000, 5_000] {
        rows.push(resolve_cell("known schema", fleet, 8, Ask::Known, Listing::Instant).await);
        rows.last().unwrap().print();
    }
    println!(
        "\nIf 'new schema' climbs with fleet and 'known schema' does not, the cost is \
         find-by-name: every first connect pulls the entire inventory.\n"
    );
}

/// Does it depend on how many clients arrive at once? Fleet is fixed, so
/// concurrency is the only variable — the control for the sweep above.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load sweep: run explicitly with --ignored --nocapture --test-threads=1"]
async fn cold_start_cost_versus_concurrency() {
    let _exclusive = exclusive().await;
    header();
    for conc in [1, 4, 16, 64] {
        let s = resolve_cell("new schema", 2_000, conc, Ask::New, Listing::Instant).await;
        s.print();
    }
    println!(
        "\nPer-op cost that is flat in concurrency but high in absolute terms points at \
         per-request work; cost that climbs points at a shared lock or a gate.\n"
    );
}

/// What the demand above costs a client once a *real* daemon is behind it.
///
/// The sweeps hold the daemon at zero cost, which measures the pooler's demand
/// but understates the clock: heyvmd answers `GET /deployed-sandboxes` by
/// cloning every VM's metadata under a lock and probing every live handle, so
/// on a full host one listing is most of a second even when it is the only
/// thing happening. Feed that measured time back in — `curl -s -o /dev/null
/// -w '%{time_total}' localhost:34099/deployed-sandboxes` on the host in
/// question — and the arithmetic finishes itself: the cold path spends one
/// listing per new schema, so a burst of K spends K of them, and when the
/// daemon is serving them one at a time the last client in the burst waits for
/// all of them.
///
/// The default 800ms is a placeholder for a host with a few thousand VMs.
/// Override with `LOADTEST_LISTING_MS` to model a specific one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load sweep: run explicitly with --ignored --nocapture --test-threads=1"]
async fn cold_start_behind_a_realistic_daemon() {
    let _exclusive = exclusive().await;
    let ms: u64 = std::env::var("LOADTEST_LISTING_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(800);
    let d = Duration::from_millis(ms);
    println!("\nmodelling a daemon that answers one full listing in {ms}ms");
    header();
    for conc in [1, 8, 32] {
        resolve_cell("healthy daemon", 5_000, conc, Ask::New, Listing::Concurrent(d))
            .await
            .print();
    }
    for conc in [1, 8, 32] {
        resolve_cell("listings serialized", 5_000, conc, Ask::New, Listing::Serialized(d))
            .await
            .print();
    }
    println!(
        "\nThe 'known schema' rows of the fleet sweep pay none of this: reattach by id \
         never lists.\n"
    );
}

/// The regression guard, and the statement of what "fixed" means: the daemon
/// traffic one cold start generates must not scale with the size of the fleet.
///
/// A new schema currently resolves through find-by-name, which is
/// `GET /deployed-sandboxes` — the *whole* inventory — so the bytes one
/// bring-up pulls grow linearly with every VM the host has ever created. That
/// is the N+1: N is the fleet, and the +1 is the one VM anybody actually asked
/// for. It is invisible in a CPU graph because the pooler spends the time
/// waiting on the daemon to serialize a list it then throws away.
///
/// This was the acceptance criterion for removing the listing from the cold
/// path, and it holds now: a new schema resolves through the name→id cache
/// and the daemon's `?name=` lookup (`vm::find_by_name_with_retry`), so the
/// bytes one bring-up pulls are flat in fleet size. Runs in the normal suite
/// as the regression guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_cold_start_must_not_cost_the_whole_inventory() {
    let _exclusive = exclusive().await;
    let small = resolve_cell("guard/small", 100, 1, Ask::New, Listing::Instant).await;
    let large = resolve_cell("guard/large", 5_000, 1, Ask::New, Listing::Instant).await;
    header();
    small.print();
    large.print();
    // 50x the fleet must not mean meaningfully more daemon traffic per
    // bring-up. 2x leaves room for the id and name growing a few bytes.
    assert!(
        large.bytes_per_op < small.bytes_per_op * 2.0,
        "one cold start pulls {} from the daemon at a fleet of 5000 vs {} at 100 — \
         bring-up cost scales with inventory",
        human(large.bytes_per_op),
        human(small.bytes_per_op),
    );
}

/// The fixed cold path itself: resolving a schema whose VM exists on the
/// daemon but is unknown to the pooler (fresh cache, no registry row) must
/// cost one `?name=` lookup — not an inventory pull, and above all not a
/// deploy (a duplicate VM with an empty data disk is the data-loss case the
/// authoritative lookup exists to prevent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn by_name_lookup_finds_without_pulling_inventory() {
    let _exclusive = exclusive().await;
    let cfg = config_for(500, 0);
    daemon().seed(500);
    daemon().metrics.reset();

    let name = format!("pg-{}", seed_schema(42));
    let (sb, provenance) = crate::vm::resolve_sandbox(&cfg, &name, false, None, None)
        .await
        .expect("resolving a daemon-known schema");
    assert_eq!(sb.sandbox_id(), seed_id(42));
    assert_eq!(provenance, crate::vm::Provenance::Existing);

    let calls = daemon().metrics.calls.lock().unwrap().clone();
    assert_eq!(
        calls.get("GET /deployed-sandboxes").copied().unwrap_or(0),
        0,
        "the cold path must not pull the inventory: {calls:?}"
    );
    assert_eq!(
        calls.get("GET /deployed-sandboxes?name").copied().unwrap_or(0),
        1,
        "exactly one by-name lookup: {calls:?}"
    );
    assert_eq!(
        calls.get("POST /sandbox-deploy").copied().unwrap_or(0),
        0,
        "an existing VM must be reattached, never recreated: {calls:?}"
    );
}

/// The duplicate-VM guard behind the positive-only cache design: a VM the
/// daemon knows about but the cache doesn't (pooler restarted, cache cold, no
/// registry row) must be found by the authoritative by-name call and
/// reattached — a cache miss is never treated as "doesn't exist".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn existing_vm_absent_from_cache_is_reattached_not_recreated() {
    let _exclusive = exclusive().await;
    let cfg = config_for(50, 0);
    daemon().seed(50);

    // Warm the cache with one resolve, then wipe it — the "restarted pooler"
    // state, with the VM still very much alive daemon-side.
    let name = format!("pg-{}", seed_schema(7));
    crate::vm::resolve_sandbox(&cfg, &name, false, None, None)
        .await
        .expect("first resolve");
    crate::inventory::reset();
    daemon().metrics.reset();

    let (sb, _) = crate::vm::resolve_sandbox(&cfg, &name, false, None, None)
        .await
        .expect("resolving after a cache wipe");
    assert_eq!(sb.sandbox_id(), seed_id(7), "the same VM, not a duplicate");

    let calls = daemon().metrics.calls.lock().unwrap().clone();
    assert_eq!(
        calls.get("POST /sandbox-deploy").copied().unwrap_or(0),
        0,
        "a cache miss must go to the daemon, never straight to create: {calls:?}"
    );
    assert_eq!(
        calls.get("GET /deployed-sandboxes?name").copied().unwrap_or(0),
        1,
        "the authoritative by-name lookup answers the miss: {calls:?}"
    );
}

/// Version skew: an old daemon ignores `?name=` and serves the full
/// inventory. Resolution must still land on the exact match (client-side
/// filter), the full list must warm the cache for free, and a second resolve
/// must be answered from the cache without another listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_daemon_full_list_fallback_still_resolves() {
    let _exclusive = exclusive().await;
    let cfg = config_for(100, 0);
    daemon().seed(100);
    *daemon().supports_name_filter.lock().unwrap() = false;
    daemon().metrics.reset();

    let name = format!("pg-{}", seed_schema(3));
    let (sb, _) = crate::vm::resolve_sandbox(&cfg, &name, false, None, None)
        .await
        .expect("resolving against an old daemon");
    assert_eq!(sb.sandbox_id(), seed_id(3));

    let full_lists = |calls: &BTreeMap<&'static str, u64>| {
        calls.get("GET /deployed-sandboxes").copied().unwrap_or(0)
    };
    let calls = daemon().metrics.calls.lock().unwrap().clone();
    assert_eq!(full_lists(&calls), 1, "old daemon: one full-list fallback: {calls:?}");
    assert_eq!(calls.get("POST /sandbox-deploy").copied().unwrap_or(0), 0);

    // The absorbed full list covers every seeded name — the next cold resolve
    // is a cache hit and never lists.
    let other = format!("pg-{}", seed_schema(90));
    let (sb, _) = crate::vm::resolve_sandbox(&cfg, &other, false, None, None)
        .await
        .expect("second resolve");
    assert_eq!(sb.sandbox_id(), seed_id(90));
    let calls = daemon().metrics.calls.lock().unwrap().clone();
    assert_eq!(
        full_lists(&calls),
        1,
        "the second resolve must be served from the warmed cache: {calls:?}"
    );
}

// ------------------------------------------------------------- harness checks

/// The harness is only worth its output if the stub is really the daemon the
/// pooler talks to, and if the counters really count. Cheap enough to run in
/// the normal suite.
#[tokio::test]
async fn the_stub_stands_in_for_the_daemon_and_counts_what_it_serves() {
    let _exclusive = exclusive().await;
    let _cfg = config_for(3, 0);
    daemon().seed(3);
    daemon().metrics.reset();

    let all = crate::vm::list_with_retry()
        .await
        .expect("the pooler's own listing must reach the stub");
    assert_eq!(all.len(), 3, "the stub served the seeded fleet");
    assert_eq!(all[0].name, "pg-seed-000000");
    assert_eq!(all[0].guest_ip.as_deref(), Some("127.0.0.1"));

    assert_eq!(daemon().metrics.total_calls(), 1, "one listing, one request");
    assert!(
        daemon().metrics.total_bytes() > 0,
        "the byte counter is what the scaling assertions read"
    );
}

/// The measurement rests on the fleet actually reaching the wire, so pin that
/// a bigger fleet really does cost more bytes to list.
#[tokio::test]
async fn listing_cost_tracks_fleet_size() {
    let _exclusive = exclusive().await;
    let _cfg = config_for(0, 0);

    daemon().seed(10);
    daemon().metrics.reset();
    crate::vm::list_with_retry().await.unwrap();
    let small = daemon().metrics.total_bytes();

    daemon().seed(1_000);
    daemon().metrics.reset();
    crate::vm::list_with_retry().await.unwrap();
    let large = daemon().metrics.total_bytes();

    assert!(
        large > small * 50,
        "listing 1000 VMs ({large}B) must cost far more than listing 10 ({small}B)"
    );
}
