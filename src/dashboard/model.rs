//! Merge the daemon's VM inventory with the pooler's live session snapshot and
//! its durable schema↔VM map into a flat list of rows for rendering.
//!
//! Note: this deliberately performs **no guest-console access** (no `commands()`
//! exec, no `files()` read). Those go through the VM's PID-1 serial-console
//! shell on this image and can halt the VM, so the dashboard only reads the
//! daemon's inventory here and live DB stats over the pooler's own PG pool.
//!
//! We fetch `/deployed-sandboxes` as raw JSON (via the SDK's HTTP client) rather
//! than through `Sandbox::list`, because the typed `SandboxInfo` drops the
//! concrete `cpus`/`memory`/`disk_size_gb` the daemon stores — and those (not
//! the mostly-unpopulated `size_class`) are what actually describe a VM's size.
//! `/deployed-sandboxes` alone misses persisted-but-stopped sandboxes the
//! daemon hasn't loaded into memory (see [`fetch_inventory`]), so the inventory
//! also walks `GET /sandboxes/inactive`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::http::Method;
use heyo_sdk::{HeyoClient, RequestOptions, SandboxStatus};
use serde::Deserialize;
use tracing::warn;

use crate::registry::EntrySnapshot;
use crate::store::StoreRecord;
use crate::vm;

use super::state::DashState;

const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Default / maximum rows per page on the browse-all view. The cap bounds how
/// much HTML one request can render no matter what `?per=` a client asks for.
pub const DEFAULT_PER: usize = 25;
pub const MAX_PER: usize = 200;

/// State filter values, in display order. Each is a [`status_str`] label; the
/// sentinel [`STATE_ALL`] disables the filter. The view defaults to showing
/// only running sandboxes — on a busy host the stopped tail dwarfs the live
/// set.
pub const STATE_FILTERS: [&str; 10] = [
    "running",
    "provisioning",
    "stopped",
    "paused",
    "cold-stored",
    "failed",
    "unknown",
    "compacted",
    "frozen",
    "archived",
];
pub const DEFAULT_STATE: &str = "running";
pub const STATE_ALL: &str = "all";

/// The subset of the daemon's raw sandbox record we render. Extra JSON fields
/// are ignored; missing ones default, so this tolerates daemon schema drift.
/// `Clone` because page renders clone the cached inventory to join/filter it.
#[derive(Deserialize, Clone)]
struct RawSandbox {
    id: String,
    #[serde(default)]
    name: String,
    status: SandboxStatus,
    #[serde(default)]
    size_class: Option<String>,
    #[serde(default)]
    uptime_secs: u64,
    #[serde(default)]
    ttl_seconds: Option<u64>,
    #[serde(default)]
    guest_ip: Option<String>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    status_changed_at: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    region: Option<String>,
    // Concrete resource config (from the sandbox record / sandbox.yaml).
    #[serde(default)]
    cpus: Option<u32>,
    #[serde(default)]
    memory: Option<u64>, // bytes
    #[serde(default)]
    disk_size_gb: Option<u32>,
    // Not from the daemon (it never sees these — they were killed). Set on the
    // synthetic rows we splice in for schemas whose data lives only in a local
    // dump ("frozen") or in S3 ("archived").
    #[serde(skip)]
    offload: Option<&'static str>,
}

impl RawSandbox {
    /// A synthetic record for an offloaded schema (frozen locally or archived
    /// to S3): its VM was killed, so it is absent from the daemon inventory.
    /// `status` is a placeholder — the `offload` label drives display and
    /// filtering.
    fn offloaded(schema: &str, sandbox_id: &str, label: &'static str) -> Self {
        RawSandbox {
            id: sandbox_id.to_string(),
            name: format!("pg-{schema}"),
            status: SandboxStatus::Stopped,
            size_class: None,
            uptime_secs: 0,
            ttl_seconds: None,
            guest_ip: None,
            error_message: None,
            status_changed_at: None,
            image: None,
            region: None,
            cpus: None,
            memory: None,
            disk_size_gb: None,
            offload: Some(label),
        }
    }
}

/// State label for a raw record: `archived` for the S3-offloaded synthetic rows,
/// otherwise the daemon status label. Shared by the state filter, pill counts,
/// and search so all three agree on what "archived" matches.
fn row_state_label(s: &RawSandbox) -> &'static str {
    if let Some(label) = s.offload {
        label
    } else {
        status_str(&s.status)
    }
}

/// Append synthetic rows for every offloaded (frozen/archived) schema not
/// already represented in the daemon inventory (deduped by sandbox id).
fn append_offloaded(list: &mut Vec<RawSandbox>, records: &[(String, StoreRecord)]) {
    let existing: std::collections::HashSet<&str> = list.iter().map(|s| s.id.as_str()).collect();
    let mut extra: Vec<RawSandbox> = records
        .iter()
        .filter(|(_, r)| r.offloaded() && !existing.contains(r.sandbox_id.as_str()))
        .map(|(schema, r)| {
            let label = r.tier.as_str();
            RawSandbox::offloaded(schema, &r.sandbox_id, label)
        })
        .collect();
    list.append(&mut extra);
}

/// One VM as shown in the dashboard: daemon facts left-joined with pooler state.
pub struct VmRow {
    pub id: String,
    pub name: String,
    /// Recovered schema (warm entry → store → `pg-` name prefix), if any.
    pub schema: Option<String>,
    /// True when this VM is one the pooler manages (`pg-<schema>`).
    pub pool_managed: bool,
    pub status: SandboxStatus,
    pub size_class: Option<String>,
    pub cpus: Option<u32>,
    pub memory_bytes: Option<u64>,
    pub disk_size_gb: Option<u32>,
    /// Daemon-sampled CPU of the VM's host process(es), `top` convention:
    /// percent of one core, so a busy 4-vCPU guest can read up to ~400.
    /// `None` when the daemon's usage poller doesn't cover this VM (stopped,
    /// or an older daemon without `/system/usage`).
    pub cpu_percent: Option<f32>,
    pub image: Option<String>,
    pub region: Option<String>,
    pub status_changed_at: Option<String>,
    pub uptime_secs: u64,
    pub ttl_seconds: Option<u64>,
    pub guest_ip: Option<String>,
    pub error_message: Option<String>,
    /// Live client sessions through the pooler; `None` when no warm entry exists.
    pub live_sessions: Option<usize>,
    /// Client connection slots (free, total) the pooler will admit to this
    /// VM's Postgres. free == 0 means new clients are queueing at the pooler
    /// rather than being refused by the database.
    pub client_slots: Option<(usize, usize)>,
    pub idle_secs: Option<u64>,
    pub keepalive: bool,
    /// Where the pooler splices client bytes (warm entries only).
    pub target: Option<std::net::SocketAddr>,
    /// True when reached over an iroh tunnel rather than a direct guest IP.
    pub tunneled: Option<bool>,
    /// `Some("frozen")`/`Some("archived")` when this schema's data lives only
    /// in a local dump / in S3 (VM killed). No live sandbox backs it; the next
    /// client connect restores it.
    pub offload: Option<&'static str>,
}

impl VmRow {
    pub fn is_running(&self) -> bool {
        self.status == SandboxStatus::Running
    }
}

/// Page envelope of `GET /sandboxes/inactive`. The records are the daemon's
/// *native* `SandboxInfo` shape, not the compat shape `/deployed-sandboxes`
/// returns — close enough that [`RawSandbox`]'s serde defaults absorb the
/// differences (no `uptime_secs`/`status_changed_at`; same lowercase status
/// strings).
#[derive(Deserialize)]
struct InactivePage {
    #[serde(default)]
    sandboxes: Vec<RawSandbox>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Envelope of `GET /system/usage`: the daemon's latest cached host +
/// per-sandbox CPU/memory sample. `snapshot` is `null` until the daemon's
/// background poller publishes its first sample (~one poll interval after
/// daemon startup).
#[derive(Deserialize)]
struct UsageEnvelope {
    #[serde(default)]
    snapshot: Option<UsageSnapshot>,
}

#[derive(Deserialize)]
struct UsageSnapshot {
    #[serde(default)]
    host: Option<HostUsage>,
    #[serde(default)]
    sandboxes: Vec<SandboxUsage>,
}

/// Whole-machine CPU and memory as heyvmd's poller samples it (the `host` slice
/// of `GET /system/usage`, serialized camelCase). Every field is optional so an
/// older daemon or an unprimed poller degrades field-by-field on the monitoring
/// page rather than failing it. Disk isn't in this snapshot — it's read
/// directly from the host in [`super::host`].
#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HostUsage {
    /// Whole-machine CPU utilization, 0–100 (not the per-core `top` convention).
    #[serde(default)]
    pub cpu_percent: Option<f32>,
    #[serde(default)]
    pub cpu_count: Option<u32>,
    #[serde(default)]
    pub memory_total_bytes: Option<u64>,
    #[serde(default)]
    pub memory_used_bytes: Option<u64>,
}

/// The slice of the daemon's per-sandbox usage sample we render (the snapshot
/// serializes camelCase, unlike the sandbox records).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandboxUsage {
    sandbox_id: String,
    cpu_percent: f32,
}

/// Fetch the daemon's cached per-sandbox usage sample, keyed by sandbox id.
/// Best-effort: an older daemon without the route, a timeout, or a poller
/// that hasn't sampled yet all degrade to "no CPU shown" rather than failing
/// the page. The endpoint serves the poller's cache — it never samples inline.
async fn fetch_usage(client: &HeyoClient) -> HashMap<String, f32> {
    let resp = tokio::time::timeout(
        LIST_TIMEOUT,
        client.request::<UsageEnvelope>(
            Method::GET,
            "/system/usage",
            None::<&()>,
            RequestOptions::default(),
        ),
    )
    .await;
    let snapshot = match resp {
        Ok(Ok(env)) => env.snapshot,
        Ok(Err(e)) => {
            warn!("fetching usage failed (hiding cpu): {e:#}");
            None
        }
        Err(_) => {
            warn!("fetching usage timed out (hiding cpu)");
            None
        }
    };
    snapshot
        .map(|s| {
            s.sandboxes
                .into_iter()
                .map(|u| (u.sandbox_id, u.cpu_percent))
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch the daemon's cached whole-host CPU/memory sample for the monitoring
/// page. Bounded and best-effort, mirroring [`fetch_usage`]: an older daemon
/// without the route, a timeout, or an unprimed poller all yield `None` so the
/// page shows "unavailable" rather than erroring. Serves the poller's cache;
/// never samples inline.
pub async fn fetch_host_usage(st: &DashState) -> Option<HostUsage> {
    let _ = st; // symmetry with the other model builders; the client is local.
    let client = match HeyoClient::new(vm::local_opts()) {
        Ok(c) => c,
        Err(e) => {
            warn!("building heyo client for host usage failed: {e:#}");
            return None;
        }
    };
    let resp = tokio::time::timeout(
        LIST_TIMEOUT,
        client.request::<UsageEnvelope>(
            Method::GET,
            "/system/usage",
            None::<&()>,
            RequestOptions::default(),
        ),
    )
    .await;
    match resp {
        Ok(Ok(env)) => env.snapshot.and_then(|s| s.host),
        Ok(Err(e)) => {
            warn!("fetching host usage failed: {e:#}");
            None
        }
        Err(_) => {
            warn!("fetching host usage timed out");
            None
        }
    }
}

/// How long a cached inventory snapshot is served without kicking a refresh.
/// Auto-refreshing dashboards (2–5s presets), the 60s history sampler, and the
/// alerts evaluator all read through the cache, so heyvmd sees at most one
/// inventory walk per this interval instead of one per request.
const INVENTORY_FRESH: Duration = Duration::from_secs(5);

/// One fetched-and-parsed daemon inventory: the merged sandbox list plus the
/// per-sandbox CPU sample, wrapped in `Arc`s so serving a cached copy is a
/// pointer clone.
#[derive(Clone)]
struct InvSnapshot {
    at: Instant,
    list: Arc<Vec<RawSandbox>>,
    usage: Arc<HashMap<String, f32>>,
}

/// Cached daemon inventory with stale-while-revalidate semantics. Page renders
/// were previously O(daemon latency) — one `/deployed-sandboxes` call plus a
/// cursor walk of `/sandboxes/inactive`, each leg with its own fresh 10s
/// budget, so a daemon slowed by a sweep's start/dump/kill cycles could
/// stretch a single render past 30s without any one call timing out. With the
/// cache, a render is an in-memory read: a fresh snapshot is served as-is, a
/// stale one is served *immediately* while one background task (single-flight)
/// refreshes it, and only the very first render after startup fetches inline.
pub struct InventoryCache {
    snapshot: Mutex<Option<InvSnapshot>>,
    refreshing: AtomicBool,
}

/// What `cached_inventory` should do for a snapshot of a given age. Factored
/// out for testability.
#[derive(PartialEq, Debug)]
enum Serve {
    Fresh,
    StaleRefresh,
    ColdFetch,
}

fn serve_plan(age: Option<Duration>) -> Serve {
    match age {
        None => Serve::ColdFetch,
        Some(a) if a < INVENTORY_FRESH => Serve::Fresh,
        Some(_) => Serve::StaleRefresh,
    }
}

impl InventoryCache {
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(None),
            refreshing: AtomicBool::new(false),
        }
    }
}

/// Clears the refresh single-flight flag on drop, so a panicking or cancelled
/// refresh task can't permanently wedge the cache into never refreshing again.
struct RefreshGuard(Arc<InventoryCache>);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0.refreshing.store(false, Ordering::SeqCst);
    }
}

/// One full inventory fetch straight from the daemon (no cache).
async fn fetch_snapshot() -> Result<InvSnapshot> {
    let client = HeyoClient::new(vm::local_opts()).context("building heyo client")?;
    let (list, usage) = tokio::join!(fetch_inventory(&client), fetch_usage(&client));
    Ok(InvSnapshot {
        at: Instant::now(),
        list: Arc::new(list?),
        usage: Arc::new(usage),
    })
}

/// The daemon inventory for a page render, via the cache. Serves stale data
/// (refreshing in the background) rather than making the request wait — for an
/// admin view, seconds-old data beats a spinner every time.
async fn cached_inventory(
    st: &DashState,
) -> Result<(Arc<Vec<RawSandbox>>, Arc<HashMap<String, f32>>)> {
    let cache = &st.inventory;
    let current = cache.snapshot.lock().unwrap().clone();
    match serve_plan(current.as_ref().map(|s| s.at.elapsed())) {
        Serve::Fresh => {
            let s = current.unwrap();
            Ok((s.list, s.usage))
        }
        Serve::StaleRefresh => {
            let s = current.unwrap();
            if !cache.refreshing.swap(true, Ordering::SeqCst) {
                let cache = st.inventory.clone();
                tokio::spawn(async move {
                    let _guard = RefreshGuard(cache.clone());
                    match fetch_snapshot().await {
                        Ok(snap) => *cache.snapshot.lock().unwrap() = Some(snap),
                        // Keep serving the stale snapshot; the next stale read
                        // retries. The daemon being briefly unlistable must
                        // not blank a page that has data to show.
                        Err(e) => warn!("background inventory refresh failed: {e:#}"),
                    }
                });
            }
            Ok((s.list, s.usage))
        }
        Serve::ColdFetch => {
            let snap = fetch_snapshot().await?;
            *cache.snapshot.lock().unwrap() = Some(snap.clone());
            Ok((snap.list, snap.usage))
        }
    }
}

const INACTIVE_PAGE_SIZE: usize = 200;
/// Backstop on the inactive-list walk so a pathological daemon answer can't
/// spin the dashboard: 25 pages × 200 = 5000 sandboxes per request, far above
/// any real inventory.
const MAX_INACTIVE_PAGES: usize = 25;

/// Fetch the daemon's full sandbox inventory, no guest access anywhere:
///
/// * `GET /deployed-sandboxes` — sandboxes in the daemon's in-memory map.
///   This is *not* everything: for the Firecracker backend the daemon only
///   reconciles **running** VMs from disk into memory, so a persisted sandbox
///   stopped before the last daemon restart never shows up here.
/// * `GET /sandboxes/inactive` (cursor-paged) — exactly those on-disk,
///   not-in-memory sandboxes, reported as `stopped`. Best-effort: an older
///   daemon without the route, or a walk timeout, degrades to the deployed
///   list rather than failing the page.
///
/// Every HTTP call is bounded by [`LIST_TIMEOUT`]; the walk is bounded by
/// [`MAX_INACTIVE_PAGES`]. Neither endpoint filters server-side, so callers
/// slice/filter in-process.
async fn fetch_inventory(client: &HeyoClient) -> Result<Vec<RawSandbox>> {
    let mut all = tokio::time::timeout(
        LIST_TIMEOUT,
        client.request::<Vec<RawSandbox>>(
            Method::GET,
            "/deployed-sandboxes",
            None::<&()>,
            RequestOptions::default(),
        ),
    )
    .await
    .context("listing sandboxes timed out")?
    .context("listing sandboxes")?;

    // The two lists are disjoint by construction (inactive = on disk but not
    // in memory), but a sandbox can start between the two reads — dedupe by
    // id, keeping the live deployed record.
    let mut seen: std::collections::HashSet<String> = all.iter().map(|s| s.id.clone()).collect();

    let mut cursor: Option<String> = None;
    for _ in 0..MAX_INACTIVE_PAGES {
        let mut opts = RequestOptions::default();
        opts.query
            .push(("count".to_string(), INACTIVE_PAGE_SIZE.to_string()));
        if let Some(c) = &cursor {
            opts.query.push(("cursor".to_string(), c.clone()));
        }
        let page = match tokio::time::timeout(
            LIST_TIMEOUT,
            client.request::<InactivePage>(Method::GET, "/sandboxes/inactive", None::<&()>, opts),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!("listing inactive sandboxes failed (showing deployed only): {e:#}");
                break;
            }
            Err(_) => {
                warn!("listing inactive sandboxes timed out (showing deployed only)");
                break;
            }
        };
        for s in page.sandboxes {
            if seen.insert(s.id.clone()) {
                all.push(s);
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    Ok(all)
}

/// The pooler-side join inputs: warm-entry snapshots keyed by sandbox id, the
/// durable id→schema map, and the full durable records (which carry the
/// `archived` flag used to splice in S3-only schemas). All cheap in-process
/// reads (one brief map lock, no I/O).
async fn pooler_maps(
    st: &DashState,
) -> (
    HashMap<String, EntrySnapshot>,
    HashMap<String, String>,
    Vec<(String, StoreRecord)>,
) {
    let snap = st
        .registry
        .snapshot()
        .await
        .into_iter()
        .map(|e| (e.sandbox_id.clone(), e))
        .collect();
    let records = st.registry.store_records();
    let store = records
        .iter()
        .map(|(schema, r)| (r.sandbox_id.clone(), schema.clone()))
        .collect();
    (snap, store, records)
}

/// Left-join one raw daemon record with the pooler's state and the daemon's
/// usage sample.
fn join_row(
    s: RawSandbox,
    snap: &HashMap<String, EntrySnapshot>,
    store: &HashMap<String, String>,
    usage: &HashMap<String, f32>,
) -> VmRow {
    let entry = snap.get(&s.id);
    let schema = entry
        .map(|e| e.schema.clone())
        .or_else(|| store.get(&s.id).cloned())
        .or_else(|| s.name.strip_prefix("pg-").map(str::to_string));
    let pool_managed = schema.is_some() || s.name.starts_with("pg-");
    let cpu_percent = usage.get(&s.id).copied();
    VmRow {
        id: s.id,
        name: s.name,
        schema,
        pool_managed,
        status: s.status,
        size_class: s.size_class,
        cpus: s.cpus,
        memory_bytes: s.memory,
        disk_size_gb: s.disk_size_gb,
        cpu_percent,
        image: s.image,
        region: s.region,
        status_changed_at: s.status_changed_at,
        uptime_secs: s.uptime_secs,
        ttl_seconds: s.ttl_seconds,
        guest_ip: s.guest_ip,
        error_message: s.error_message,
        live_sessions: entry.map(|e| e.active),
        client_slots: entry.map(|e| (e.free_slots, e.slot_limit)),
        idle_secs: entry.map(|e| e.idle_secs),
        keepalive: entry.map(|e| e.keepalive).unwrap_or(false),
        target: entry.map(|e| e.target),
        tunneled: entry.map(|e| e.tunneled),
        offload: s.offload,
    }
}

/// Build the full VM list: read the (cached) sandbox inventory and daemon
/// usage sample, join the registry snapshot + store. No guest access — safe
/// to call on every page load/refresh.
pub async fn build_rows(st: &DashState) -> Result<Vec<VmRow>> {
    let (list, usage) = cached_inventory(st).await?;
    let mut list = (*list).clone();
    let (snap, store, records) = pooler_maps(st).await;
    append_offloaded(&mut list, &records);

    let mut rows: Vec<VmRow> = list
        .into_iter()
        .map(|s| join_row(s, &snap, &store, &usage))
        .collect();

    // Pooler-managed VMs first, then alphabetical by name.
    rows.sort_by(|a, b| {
        b.pool_managed
            .cmp(&a.pool_managed)
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(rows)
}

/// One page of the browse-all view.
pub struct SandboxPage {
    /// Only the rows on the current page — everything off-page stays a raw
    /// record and is never joined or rendered.
    pub rows: Vec<VmRow>,
    /// Total sandboxes the daemon reported (pre-filter).
    pub total: usize,
    /// How many matched the search.
    pub matched: usize,
    /// Current page, 1-based, clamped to `pages`.
    pub page: usize,
    /// Total pages for the current match set (>= 1).
    pub pages: usize,
    pub per: usize,
    /// The search text as applied (trimmed), echoed back into the form.
    pub q: String,
    /// The state filter as applied: a [`STATE_FILTERS`] label or [`STATE_ALL`].
    pub state: String,
    /// Sandboxes per state label among the search-matched set (state filter
    /// not applied), in [`STATE_FILTERS`] order — drives the filter pills.
    pub state_counts: Vec<(&'static str, usize)>,
}

/// Build one page of the searchable all-sandboxes view. Same system footprint
/// as [`build_rows`] — bounded daemon reads plus in-process pooler lookups,
/// no guest access — but lazier: the filters run on the raw records and only
/// the `per` rows in the page window are joined into full [`VmRow`]s.
pub async fn build_page(
    st: &DashState,
    q: &str,
    state: &str,
    page: usize,
    per: usize,
) -> Result<SandboxPage> {
    let (list, usage) = cached_inventory(st).await?;
    let mut list = (*list).clone();
    let (snap, store, records) = pooler_maps(st).await;
    append_offloaded(&mut list, &records);
    let total = list.len();

    let needle = q.trim().to_lowercase();
    let state = {
        let s = state.trim().to_lowercase();
        if s.is_empty() {
            DEFAULT_STATE.to_string()
        } else {
            s
        }
    };

    // Search filter first (state-agnostic) — this set also feeds the per-state
    // pill counts, so switching state filters shows where the matches live.
    let q_hits: Vec<RawSandbox> = list
        .into_iter()
        .filter(|s| {
            needle.is_empty() || {
                // Recovered schema names come from the pooler maps; a schema
                // implied by a `pg-` name prefix is already covered by the
                // name match.
                let schema = snap
                    .get(&s.id)
                    .map(|e| e.schema.as_str())
                    .or_else(|| store.get(&s.id).map(String::as_str));
                matches_query(s, schema, &needle)
            }
        })
        .collect();

    let mut state_counts: Vec<(&'static str, usize)> =
        STATE_FILTERS.iter().map(|l| (*l, 0)).collect();
    for s in &q_hits {
        let label = row_state_label(s);
        if let Some(c) = state_counts.iter_mut().find(|(l, _)| *l == label) {
            c.1 += 1;
        }
    }

    let mut hits: Vec<RawSandbox> = q_hits
        .into_iter()
        .filter(|s| state_matches(s, &state))
        .collect();
    // Stable order (name, then id as tie-break) so page boundaries don't
    // shuffle between requests.
    hits.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

    let matched = hits.len();
    let (page, pages) = page_window(matched, page, per);
    let rows = hits
        .into_iter()
        .skip((page - 1) * per)
        .take(per)
        .map(|s| join_row(s, &snap, &store, &usage))
        .collect();

    Ok(SandboxPage {
        rows,
        total,
        matched,
        page,
        pages,
        per,
        q: q.trim().to_string(),
        state,
        state_counts,
    })
}

/// Does this sandbox pass the state filter? `state` is a lowercased
/// [`STATE_FILTERS`] label or [`STATE_ALL`]; an unrecognized value simply
/// matches nothing (an admin typo'd a URL — an empty page is clearer than a
/// silently ignored filter).
fn state_matches(s: &RawSandbox, state: &str) -> bool {
    state == STATE_ALL || row_state_label(s) == state
}

/// Case-insensitive substring match against the fields an operator actually
/// searches by. `needle` must already be lowercased and non-empty.
fn matches_query(s: &RawSandbox, schema: Option<&str>, needle: &str) -> bool {
    let hay = |v: &str| v.to_lowercase().contains(needle);
    hay(&s.id)
        || hay(&s.name)
        || schema.is_some_and(hay)
        || hay(row_state_label(s))
        || s.image.as_deref().is_some_and(hay)
        || s.guest_ip.as_deref().is_some_and(hay)
}

/// Clamp a 1-based page request against the match count → `(page, pages)`.
/// Zero matches still yields one (empty) page so the view always has a valid
/// page number to render.
fn page_window(matched: usize, page: usize, per: usize) -> (usize, usize) {
    let pages = matched.div_ceil(per).max(1);
    (page.clamp(1, pages), pages)
}

/// Human label for a sandbox status — shared by the status badges and the
/// search filter, so searching "running" matches what the badge shows.
pub fn status_str(status: &SandboxStatus) -> &'static str {
    match status {
        SandboxStatus::Running => "running",
        SandboxStatus::Provisioning => "provisioning",
        SandboxStatus::Stopped => "stopped",
        SandboxStatus::Paused => "paused",
        SandboxStatus::Failed => "failed",
        SandboxStatus::ColdStored => "cold-stored",
        SandboxStatus::Unknown => "unknown",
    }
}

/// Find one VM row by id (rebuilds the full list; fine for an admin tool with a
/// handful of VMs).
pub async fn find_row(st: &DashState, id: &str) -> Result<Option<VmRow>> {
    Ok(build_rows(st).await?.into_iter().find(|r| r.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_sandbox_captures_cpus_and_memory() {
        // Shape mirrors the daemon's sandbox record (cf. the sandbox.yaml a
        // `large` VM reports: cpus: 4, memory: 8589934592, disk_size_gb: 4).
        let json = r#"{
            "id": "sb-f88677f0",
            "name": "pg-aNb6mPp6",
            "status": "running",
            "image": "pg",
            "ttl_seconds": 0,
            "disk_size_gb": 4,
            "cpus": 4,
            "memory": 8589934592,
            "guest_ip": "172.25.223.194",
            "status_changed_at": "2026-07-09T00:00:00Z"
        }"#;
        let s: RawSandbox = serde_json::from_str(json).unwrap();
        assert_eq!(s.cpus, Some(4));
        assert_eq!(s.memory, Some(8_589_934_592));
        assert_eq!(s.disk_size_gb, Some(4));
        assert_eq!(s.status, SandboxStatus::Running);
    }

    #[test]
    fn raw_sandbox_tolerates_missing_size_fields() {
        // Older/other daemons may omit cpus/memory — must not fail the list.
        let json = r#"{"id":"sb-x","name":"pg-y","status":"stopped","status_changed_at":"t"}"#;
        let s: RawSandbox = serde_json::from_str(json).unwrap();
        assert_eq!(s.cpus, None);
        assert_eq!(s.memory, None);
        assert_eq!(s.status, SandboxStatus::Stopped);
    }

    #[test]
    fn inactive_page_parses_native_sandbox_info() {
        // The native `SandboxInfo` shape the /sandboxes/inactive route returns:
        // `image` is a bare string, `uptime` is a serde Duration object, and
        // there is no `uptime_secs`/`status_changed_at`/`cpus`/`memory`.
        let json = r#"{
            "sandboxes": [{
                "id": "sb-cold1",
                "name": "pg-old",
                "sandbox_type": "vm",
                "status": "stopped",
                "image": "pg",
                "uptime": {"secs": 0, "nanos": 0},
                "cpu_usage": null,
                "memory_usage": null,
                "remotely_accessible": false,
                "guest_ip": "172.25.0.2"
            }],
            "cursor": null,
            "next_cursor": "sb-cold1"
        }"#;
        let page: InactivePage = serde_json::from_str(json).unwrap();
        assert_eq!(page.sandboxes.len(), 1);
        let s = &page.sandboxes[0];
        assert_eq!(s.status, SandboxStatus::Stopped);
        assert_eq!(s.uptime_secs, 0);
        assert_eq!(s.guest_ip.as_deref(), Some("172.25.0.2"));
        assert_eq!(page.next_cursor.as_deref(), Some("sb-cold1"));
    }

    #[test]
    fn usage_envelope_parses_camel_case_snapshot() {
        // The shape `GET /system/usage` returns once the daemon's poller has
        // published a sample (snapshot serializes camelCase).
        let json = r#"{
            "available": true,
            "snapshot": {
                "sampledAtMs": 1752000000000,
                "host": {"cpuPercent": 12.5, "cpuCount": 16,
                         "memoryTotalBytes": 68719476736, "memoryUsedBytes": 8589934592},
                "sandboxes": [
                    {"sandboxId": "sb-f88677f0", "name": "pg-aNb6mPp6",
                     "cpuPercent": 137.2, "memoryBytes": 524288000, "pids": [4242]}
                ]
            }
        }"#;
        let env: UsageEnvelope = serde_json::from_str(json).unwrap();
        let snap = env.snapshot.unwrap();
        assert_eq!(snap.sandboxes.len(), 1);
        assert_eq!(snap.sandboxes[0].sandbox_id, "sb-f88677f0");
        assert!((snap.sandboxes[0].cpu_percent - 137.2).abs() < 1e-3);
        // The whole-host slice the monitoring page renders.
        let host = snap.host.unwrap();
        assert!((host.cpu_percent.unwrap() - 12.5).abs() < 1e-3);
        assert_eq!(host.cpu_count, Some(16));
        assert_eq!(host.memory_total_bytes, Some(68_719_476_736));
        assert_eq!(host.memory_used_bytes, Some(8_589_934_592));
    }

    #[test]
    fn usage_envelope_tolerates_unprimed_poller() {
        // Before the poller's first sample the daemon reports a null snapshot.
        let json = r#"{"available": false, "snapshot": null}"#;
        let env: UsageEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.snapshot.is_none());
    }

    fn raw(id: &str, name: &str, status: &str) -> RawSandbox {
        let json = format!(r#"{{"id":"{id}","name":"{name}","status":"{status}"}}"#);
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn query_matches_id_name_schema_and_status_case_insensitively() {
        let s = raw("sb-F88677", "pg-aNb6mPp6", "running");
        assert!(matches_query(&s, None, "sb-f88"));
        assert!(matches_query(&s, None, "anb6"));
        assert!(matches_query(&s, Some("tenant_west"), "west"));
        assert!(matches_query(&s, None, "running"));
        assert!(!matches_query(&s, None, "stopped"));
        assert!(!matches_query(&s, Some("tenant_west"), "east"));
    }

    #[test]
    fn state_filter_matches_label_or_all() {
        let running = raw("sb-1", "pg-a", "running");
        let stopped = raw("sb-2", "pg-b", "stopped");
        assert!(state_matches(&running, "running"));
        assert!(!state_matches(&stopped, "running"));
        assert!(state_matches(&stopped, "stopped"));
        assert!(state_matches(&running, STATE_ALL));
        assert!(state_matches(&stopped, STATE_ALL));
        // Unrecognized filter matches nothing rather than everything.
        assert!(!state_matches(&running, "bogus"));
        // Every filter label is a status_str label, so pill counts line up.
        assert!(STATE_FILTERS.contains(&status_str(&running.status)));
    }

    #[test]
    fn serve_plan_prefers_stale_over_waiting() {
        // No snapshot yet: the render must fetch inline — there is nothing to show.
        assert_eq!(serve_plan(None), Serve::ColdFetch);
        // Fresh: serve as-is, no daemon traffic.
        assert_eq!(serve_plan(Some(Duration::from_secs(1))), Serve::Fresh);
        // Stale — even *very* stale (daemon was down a while): still serve the
        // data immediately and refresh behind the request, never block a render
        // on the daemon.
        assert_eq!(serve_plan(Some(INVENTORY_FRESH)), Serve::StaleRefresh);
        assert_eq!(serve_plan(Some(Duration::from_secs(3600))), Serve::StaleRefresh);
    }

    #[test]
    fn page_window_clamps_to_valid_pages() {
        // Zero matches still renders one empty page.
        assert_eq!(page_window(0, 1, 25), (1, 1));
        assert_eq!(page_window(0, 7, 25), (1, 1));
        // 51 rows at 25/page → 3 pages; out-of-range requests clamp.
        assert_eq!(page_window(51, 1, 25), (1, 3));
        assert_eq!(page_window(51, 3, 25), (3, 3));
        assert_eq!(page_window(51, 9, 25), (3, 3));
        assert_eq!(page_window(51, 0, 25), (1, 3));
        // Exact multiple has no phantom extra page.
        assert_eq!(page_window(50, 2, 25), (2, 2));
    }
}
