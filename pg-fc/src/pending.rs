//! Durable ledger of in-flight VM creates: `schema -> (sandbox_id, started)`.
//!
//! Closes the orphan window the 2026-08-18 incident exposed: `Sandbox::create`
//! hands back an id the instant the daemon 202-accepts the deploy, but the
//! schema→id binding is only written to the registry (`store.put`) after the
//! *whole* bring-up succeeds — boot, tunnel, `CREATE DATABASE`, restore. Every
//! failure in between used to leave a daemon-side record bound to nothing:
//! invisible to the idle reaper (no warm entry), to purge (no registry row,
//! not spare-named), and to the orphan-disk sweep (the daemon still reports
//! the record, so it never reads `Gone`). Hundreds of those accumulated with
//! their disks pinned, with manual deletion the only way out.
//!
//! The ledger records the id the moment create returns, so a failed bring-up
//! is a *named* leak: the failure path kills it by id (no listing round-trip —
//! the list is exactly what's unreachable during the incidents that strand
//! VMs), and the janitor (`registry::spawn_pending_janitor`) reaps any entry
//! that outlives `ready_timeout` without gaining a registry binding — covering
//! pooler crashes mid-bring-up and kills that failed on the spot.
//!
//! Persistence is best-effort: one `schema\tsandbox_id\tstarted_unix` line per
//! entry, rewritten whole (the file is a handful of in-flight rows) via
//! temp+rename. A write lost to a crash costs one orphan — the pre-ledger
//! behavior — never data. Schema names are validated upstream (no control
//! chars), same as the registry file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

struct Ledger {
    path: PathBuf,
    /// schema -> (sandbox_id, started_unix)
    entries: Mutex<HashMap<String, (String, u64)>>,
    /// Serializes file rewrites so a slow write can't clobber a newer snapshot.
    write_order: tokio::sync::Mutex<()>,
}

static LEDGER: OnceLock<Ledger> = OnceLock::new();

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the ledger from `path` (entries left by a previous process are the
/// crash-leftovers the janitor exists for). Call once at startup, before the
/// listener accepts connections.
pub fn init(path: PathBuf) {
    let mut entries = HashMap::new();
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            for line in s.lines() {
                let mut f = line.split('\t');
                if let (Some(schema), Some(id), Some(ts)) = (f.next(), f.next(), f.next())
                    && let Ok(ts) = ts.parse::<u64>()
                {
                    entries.insert(schema.to_string(), (id.to_string(), ts));
                }
            }
            if !entries.is_empty() {
                info!(
                    "pending-bringups: loaded {} unresolved entr{} from {} — a previous \
                     process died mid-bring-up; the janitor will reconcile them",
                    entries.len(),
                    if entries.len() == 1 { "y" } else { "ies" },
                    path.display()
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("pending-bringups: cannot read {}: {e}", path.display()),
    }
    let _ = LEDGER.set(Ledger {
        path,
        entries: Mutex::new(entries),
        write_order: tokio::sync::Mutex::new(()),
    });
}

/// Persist the current map. Snapshot under the write-order lock so concurrent
/// record/clear calls serialize into consistent file states.
async fn persist(ledger: &'static Ledger) {
    let _order = ledger.write_order.lock().await;
    let snapshot: String = {
        let entries = ledger.entries.lock().unwrap();
        entries
            .iter()
            .map(|(schema, (id, ts))| format!("{schema}\t{id}\t{ts}\n"))
            .collect()
    };
    let path = ledger.path.clone();
    let res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tsv.tmp");
        std::fs::write(&tmp, snapshot)?;
        std::fs::rename(&tmp, &path)
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("pending-bringups: persisting ledger failed: {e}"),
        Err(e) => warn!("pending-bringups: persist task failed: {e}"),
    }
}

/// Record that a create for `schema` was accepted by the daemon as `id`.
pub async fn record(schema: &str, id: &str) {
    let Some(ledger) = LEDGER.get() else { return };
    ledger
        .entries
        .lock()
        .unwrap()
        .insert(schema.to_string(), (id.to_string(), now_unix()));
    persist(ledger).await;
}

/// Drop `schema`'s entry — the bring-up resolved (bound in the registry, or
/// its VM was confirmed killed/gone). No-op if absent.
pub async fn clear(schema: &str) {
    let Some(ledger) = LEDGER.get() else { return };
    let removed = ledger.entries.lock().unwrap().remove(schema).is_some();
    if removed {
        persist(ledger).await;
    }
}

/// The sandbox id the in-flight (or crashed) create for `schema` produced.
pub fn get(schema: &str) -> Option<String> {
    let ledger = LEDGER.get()?;
    let entries = ledger.entries.lock().unwrap();
    entries.get(schema).map(|(id, _)| id.clone())
}

/// Entries older than `min_age` — candidates for the janitor. Age is measured
/// from create-accept; anything past the ready timeout with no registry
/// binding is a leak, not a slow boot.
pub fn stale(min_age: Duration) -> Vec<(String, String)> {
    let Some(ledger) = LEDGER.get() else {
        return Vec::new();
    };
    let cutoff = now_unix().saturating_sub(min_age.as_secs());
    let entries = ledger.entries.lock().unwrap();
    entries
        .iter()
        .filter(|(_, (_, ts))| *ts <= cutoff)
        .map(|(schema, (id, _))| (schema.clone(), id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // LEDGER is process-global and OnceLock only sets once, so the tests share
    // one init and use distinct schema keys.
    fn test_init() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pending-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pending-bringups.tsv");
        init(path.clone());
        LEDGER.get().unwrap().path.clone()
    }

    #[tokio::test]
    async fn record_get_clear_roundtrip() {
        let path = test_init();
        record("alpha", "sb-11111111").await;
        assert_eq!(get("alpha").as_deref(), Some("sb-11111111"));
        // Persisted: the file names the entry.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("alpha\tsb-11111111\t"));
        clear("alpha").await;
        assert_eq!(get("alpha"), None);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("alpha"));
    }

    #[tokio::test]
    async fn stale_respects_min_age() {
        test_init();
        record("beta", "sb-22222222").await;
        // Fresh entries are not stale…
        assert!(!stale(Duration::from_secs(60)).iter().any(|(s, _)| s == "beta"));
        // …but everything is stale at age zero.
        assert!(stale(Duration::ZERO).iter().any(|(s, _)| s == "beta"));
        clear("beta").await;
    }
}
