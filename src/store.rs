//! Tiny persistent per-schema registry: `schema -> (sandbox-id, last-active,
//! state)`.
//!
//! When the pooler brings up a VM for a schema it records the sandbox id here
//! and flushes it to disk. On the next bring-up — including after a full pooler
//! restart — it reattaches to *that* VM by id instead of finding one by name.
//! That closes a data-loss race: a VM that was just stopped is briefly absent
//! from list-by-name, and reattaching by name in that window would create a
//! duplicate VM with a fresh, empty data disk. The schema (the client's db name)
//! is the key, so the file records both the db name and its VM.
//!
//! Two more fields drive the S3 eviction tier:
//!   - `last_active`: unix seconds of the last client checkout. This survives
//!     the VM leaving the warm map (idle-stop), so the hourly archive sweep can
//!     find schemas untouched for a week even though their VM is already
//!     stopped — the in-memory `SchemaEntry::last_active` is gone by then.
//!   - `state`: the storage tier — `live`, `frozen` (VM killed, data in a
//!     local dump file), or `archived` (data only in S3). Both offloaded tiers
//!     mean the next checkout must restore before serving.
//!
//! Format: one `schema\tsandbox_id\tlast_active_unix\tstate` line per entry.
//! Older 2-column files (`schema\tsandbox_id`) still parse: the missing
//! `last_active` defaults to *load time* (so an upgrade doesn't make every
//! pre-existing schema instantly eligible for eviction) and state to `live`.
//! Schema names are validated upstream to contain no control chars (so never a
//! tab or newline), so this needs no escaping.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::{info, warn};

/// `touch()` bumps `last_active` in memory on every client checkout, but only
/// flushes to disk when the on-disk value is older than this — the sweep reads
/// the in-memory value, so disk freshness only matters across a pooler restart,
/// where minutes of staleness are harmless against a week-long threshold. Keeps
/// a busy schema from fsync-storming the registry on every connect.
const FLUSH_DEBOUNCE_SECS: u64 = 60;

/// Where a schema's data currently lives — the storage tier ladder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// A VM (running or merely stopped) holds the data on its disk.
    Live,
    /// The VM (and its disk) was deleted; the data lives as a trimmed,
    /// zstd-compressed raw disk image under the pooler's compact dir — a
    /// stopped schema at a fraction of its ext4 footprint (~5-25x smaller),
    /// thawed by decompressing onto a fresh VM's disk: no boot at compact
    /// time, no pg_restore at thaw time. The next checkout materializes it.
    Compacted,
    /// The VM was killed; the data lives in a local dump file under the
    /// pooler's dump dir. The next checkout restores from it.
    Frozen,
    /// The data lives only in S3. The next checkout restores from there.
    Archived,
}

impl Tier {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Tier::Live => "live",
            Tier::Compacted => "compacted",
            Tier::Frozen => "frozen",
            Tier::Archived => "archived",
        }
    }

    // NOTE: an older binary reading a registry with "compacted" rows parses
    // them as Live (the historical default) — the schema then looks live with
    // no VM, and the next connect builds a fresh empty one while the compact
    // file still holds the data. Don't roll back across this without first
    // thawing (or manually decompressing) compacted schemas.
    fn parse(s: &str) -> Tier {
        match s {
            "archived" => Tier::Archived,
            "frozen" => Tier::Frozen,
            "compacted" => Tier::Compacted,
            _ => Tier::Live,
        }
    }
}

/// A durable, owned view of one schema's registry entry.
#[derive(Clone)]
pub struct StoreRecord {
    pub sandbox_id: String,
    /// Unix seconds of the last client checkout for this schema.
    pub last_active: u64,
    /// Storage tier: whether a VM disk, a local dump, or S3 holds the data.
    pub tier: Tier,
}

impl StoreRecord {
    /// The VM is gone and a restore (local or S3) is needed before serving.
    pub fn offloaded(&self) -> bool {
        self.tier != Tier::Live
    }
}

/// Internal map value: a [`StoreRecord`] plus the `last_active` value currently
/// on disk, used to debounce flushes.
struct Rec {
    sandbox_id: String,
    last_active: u64,
    tier: Tier,
    flushed_last_active: u64,
}

impl Rec {
    fn view(&self) -> StoreRecord {
        StoreRecord {
            sandbox_id: self.sandbox_id.clone(),
            last_active: self.last_active,
            tier: self.tier,
        }
    }
}

pub struct Store {
    path: PathBuf,
    map: Mutex<HashMap<String, Rec>>,
    /// Monotone snapshot generation, assigned under the `map` lock when a
    /// snapshot is produced. The writer refuses to write a generation older
    /// than the newest already written, so out-of-order write tasks can never
    /// roll the file back to a stale snapshot.
    seq: AtomicU64,
    /// Serializes the actual file writes and remembers the newest generation
    /// written. Deliberately separate from `map`: the write path fsyncs, and
    /// on a saturated disk that stalls for seconds — it must never hold up
    /// readers or in-memory updates. `Arc` so write closures can own it.
    written: Arc<Mutex<u64>>,
}

impl Store {
    /// Load the store from `path`. A missing file starts empty; a partially
    /// corrupt file keeps whatever lines parse (never fatal — a lost mapping
    /// only costs us a find-by-name on next connect).
    pub fn load(path: PathBuf) -> Self {
        let now = now_unix();
        let map = match std::fs::read_to_string(&path) {
            Ok(s) => parse(&s, now),
            Err(_) => HashMap::new(),
        };
        if !map.is_empty() {
            info!(
                "loaded {} schema→VM mapping(s) from {}",
                map.len(),
                path.display()
            );
        }
        Store {
            path,
            map: Mutex::new(map),
            seq: AtomicU64::new(0),
            written: Arc::new(Mutex::new(0)),
        }
    }

    /// Stamp a freshly serialized snapshot with the next generation. Must be
    /// called while the `map` lock is held, so generation order matches
    /// snapshot content order.
    fn stamp(&self, snapshot: String) -> (u64, String) {
        (self.seq.fetch_add(1, Ordering::SeqCst) + 1, snapshot)
    }

    /// The full durable record for `schema`, if any.
    pub fn record(&self, schema: &str) -> Option<StoreRecord> {
        self.map.lock().unwrap().get(schema).map(Rec::view)
    }

    /// Every `(schema, record)` known to the store — the durable list of schemas
    /// the pooler has backed, including those whose VM is stopped or archived.
    pub fn records(&self) -> Vec<(String, StoreRecord)> {
        self.map
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.view()))
            .collect()
    }

    /// Record `schema -> id` (a fresh, live VM) and flush to disk. Refreshes
    /// `last_active` and resets the tier to [`Tier::Live`]. Best-effort: a
    /// write failure is logged, not fatal. Skips the write when the mapping is
    /// unchanged and still live (then it behaves like a debounced `touch`).
    pub fn put(&self, schema: &str, id: &str) {
        let now = now_unix();
        let snapshot = {
            let mut map = self.map.lock().unwrap();
            match map.get_mut(schema) {
                Some(r) if r.sandbox_id == id && r.tier == Tier::Live => {
                    // Unchanged live mapping: just a debounced activity bump.
                    r.last_active = now;
                    if now.saturating_sub(r.flushed_last_active) >= FLUSH_DEBOUNCE_SECS {
                        r.flushed_last_active = now;
                        Some(self.stamp(serialize(&map)))
                    } else {
                        None
                    }
                }
                _ => {
                    map.insert(
                        schema.to_string(),
                        Rec {
                            sandbox_id: id.to_string(),
                            last_active: now,
                            tier: Tier::Live,
                            flushed_last_active: now,
                        },
                    );
                    Some(self.stamp(serialize(&map)))
                }
            }
        };
        self.write_detached(snapshot);
    }

    /// Bump `last_active` for `schema` to now (in memory), flushing only past
    /// [`FLUSH_DEBOUNCE_SECS`]. No-op if the schema isn't known yet.
    pub fn touch(&self, schema: &str) {
        let now = now_unix();
        let snapshot = {
            let mut map = self.map.lock().unwrap();
            let Some(r) = map.get_mut(schema) else {
                return;
            };
            r.last_active = now;
            if now.saturating_sub(r.flushed_last_active) >= FLUSH_DEBOUNCE_SECS {
                r.flushed_last_active = now;
                Some(self.stamp(serialize(&map)))
            } else {
                None
            }
        };
        self.write_detached(snapshot);
    }

    /// Move `schema` to an offloaded tier ([`Tier::Frozen`] or
    /// [`Tier::Archived`]) and flush. The tier is *reset* to live by
    /// [`Self::put`] when a fresh VM id is recorded after a restore — that's
    /// the same event that makes the data live again.
    ///
    /// Unlike `put`/`touch` this **waits for the write to reach disk**: the
    /// caller kills the VM (or deletes the local dump) right after, and the
    /// tier must be durable *before* that — a crash in between with the tier
    /// unwritten would leave a "live" record pointing at a dead VM, and the
    /// next connect would build a fresh empty database instead of restoring.
    pub async fn set_tier(&self, schema: &str, tier: Tier) {
        let snapshot = {
            let mut map = self.map.lock().unwrap();
            let Some(r) = map.get_mut(schema) else {
                return;
            };
            r.tier = tier;
            self.stamp(serialize(&map))
        };
        let (seq, contents) = snapshot;
        let path = self.path.clone();
        let written = self.written.clone();
        let res =
            tokio::task::spawn_blocking(move || write_latest(&path, &written, seq, &contents))
                .await;
        if let Err(e) = res {
            warn!(
                "persisting archived flag to {} did not complete: {e}",
                self.path.display()
            );
        }
    }

    /// Queue a stamped snapshot for writing without blocking this thread on
    /// disk I/O. The write (create + fsync + rename) runs on the blocking
    /// pool — an fsync stalled behind heavy writeback must never pin an async
    /// worker thread, or enough of them starve the whole runtime (the
    /// "dashboard times out during sweeps" failure). Best-effort by design:
    /// `put`/`touch` losing their last write to a crash only costs a
    /// find-by-name or a slightly stale idle clock on the next boot.
    /// Generation stamping keeps concurrent writes from ever regressing the
    /// file. Outside a tokio runtime (unit tests, sync callers) it writes
    /// inline.
    fn write_detached(&self, snapshot: Option<(u64, String)>) {
        let Some((seq, contents)) = snapshot else {
            return;
        };
        let path = self.path.clone();
        let written = self.written.clone();
        let write = move || write_latest(&path, &written, seq, &contents);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(write);
            }
            Err(_) => write(),
        }
    }
}

/// Write `contents` (generation `seq`) unless a newer generation has already
/// been written. Holds the `written` lock across the write so writers
/// serialize; on success records the generation, on failure logs and leaves
/// the previous generation in place (a later snapshot will retry the state).
fn write_latest(path: &Path, written: &Mutex<u64>, seq: u64, contents: &str) {
    let mut newest = written.lock().unwrap();
    if seq <= *newest {
        return;
    }
    match write_atomic(path, contents) {
        Ok(()) => *newest = seq,
        Err(e) => warn!("failed to persist pooler registry to {}: {e:#}", path.display()),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse the TSV, tolerating the legacy 2-column format. `now` fills in a
/// missing `last_active` so upgraded entries start their eviction clock fresh.
fn parse(s: &str, now: u64) -> HashMap<String, Rec> {
    s.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let schema = f.next()?;
            let id = f.next()?;
            if schema.is_empty() || id.is_empty() {
                return None;
            }
            let last_active = f.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(now);
            let tier = Tier::parse(f.next().unwrap_or("live"));
            Some((
                schema.to_string(),
                Rec {
                    sandbox_id: id.to_string(),
                    last_active,
                    tier,
                    flushed_last_active: last_active,
                },
            ))
        })
        .collect()
}

fn serialize(map: &HashMap<String, Rec>) -> String {
    let mut out = String::new();
    for (k, v) in map {
        let state = v.tier.as_str();
        out.push_str(k);
        out.push('\t');
        out.push_str(&v.sandbox_id);
        out.push('\t');
        out.push_str(&v.last_active.to_string());
        out.push('\t');
        out.push_str(state);
        out.push('\n');
    }
    out
}

/// Write via a temp file + rename so a crash mid-write can't corrupt the store.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_new_four_column_format() {
        let map = parse(
            "wb1\tsb-1\t1700000000\tlive\nwb2\tsb-2\t1700000500\tarchived\n\
             wb3\tsb-3\t1700000900\tfrozen\nwb4\tsb-4\t1700001000\tcompacted\n",
            999,
        );
        let wb1 = map.get("wb1").unwrap();
        assert_eq!(wb1.sandbox_id, "sb-1");
        assert_eq!(wb1.last_active, 1_700_000_000);
        assert_eq!(wb1.tier, Tier::Live);
        let wb2 = map.get("wb2").unwrap();
        assert_eq!(wb2.tier, Tier::Archived);
        assert_eq!(wb2.last_active, 1_700_000_500);
        assert_eq!(map.get("wb3").unwrap().tier, Tier::Frozen);
        assert_eq!(map.get("wb4").unwrap().tier, Tier::Compacted);
        assert_eq!(Tier::parse(Tier::Compacted.as_str()), Tier::Compacted);
    }

    #[test]
    fn legacy_two_column_format_defaults_to_now_and_live() {
        // The pre-eviction on-disk format. Missing last_active must default to
        // load time — NOT 0, or every upgraded schema would look week-stale and
        // get mass-archived on the first sweep after upgrade.
        let map = parse("wb\tsb-legacy\n", 12345);
        let r = map.get("wb").unwrap();
        assert_eq!(r.sandbox_id, "sb-legacy");
        assert_eq!(r.last_active, 12345);
        assert_eq!(r.tier, Tier::Live);
    }

    #[test]
    fn round_trips_through_serialize() {
        let map = parse("a\tsb-a\t100\tlive\nb\tsb-b\t200\tarchived\nc\tsb-c\t300\tfrozen\n", 0);
        let reparsed = parse(&serialize(&map), 0);
        assert_eq!(reparsed.get("a").unwrap().sandbox_id, "sb-a");
        assert_eq!(reparsed.get("a").unwrap().last_active, 100);
        assert_eq!(reparsed.get("a").unwrap().tier, Tier::Live);
        assert_eq!(reparsed.get("b").unwrap().tier, Tier::Archived);
        assert_eq!(reparsed.get("b").unwrap().last_active, 200);
        assert_eq!(reparsed.get("c").unwrap().tier, Tier::Frozen);
    }

    #[tokio::test]
    async fn put_touch_mark_clear_flow() {
        let dir = std::env::temp_dir().join(format!("pgvmpool-store-{}", std::process::id()));
        let path = dir.join("registry.tsv");
        let _ = std::fs::remove_file(&path);
        let store = Store::load(path.clone());

        store.put("wb", "sb-1");
        let r = store.record("wb").unwrap();
        assert_eq!(r.sandbox_id, "sb-1");
        assert_eq!(r.tier, Tier::Live);

        // Durable once it returns — the reload below must see it even if the
        // detached `put` write above hasn't landed (generation order covers it).
        store.set_tier("wb", Tier::Frozen).await;
        assert_eq!(store.record("wb").unwrap().tier, Tier::Frozen);
        store.set_tier("wb", Tier::Archived).await;

        // Reload from disk: the tier survives a restart.
        let reloaded = Store::load(path.clone());
        assert_eq!(reloaded.record("wb").unwrap().tier, Tier::Archived);

        // Recording a fresh VM id (a restore) resets the tier — the schema is
        // live again.
        reloaded.put("wb", "sb-2");
        let r = reloaded.record("wb").unwrap();
        assert_eq!(r.tier, Tier::Live);
        assert_eq!(r.sandbox_id, "sb-2");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stale_generations_never_regress_the_file() {
        let dir = std::env::temp_dir().join(format!("pgvmpool-seq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.tsv");
        let written = Mutex::new(0u64);
        write_latest(&path, &written, 2, "newer\tsb-2\t2\tlive\n");
        // A write task carrying an older snapshot finishes late: skipped.
        write_latest(&path, &written, 1, "older\tsb-1\t1\tlive\n");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("newer"));
        assert!(!on_disk.contains("older"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
