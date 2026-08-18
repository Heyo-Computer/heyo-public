//! Warm-spare VM pool: pre-created, pre-booted, initdb-complete VMs that a
//! cold bring-up can claim instead of paying create + boot + initdb.
//!
//! The expensive part of a cold start — creating the sandbox, booting the
//! kernel, running initdb and first-boot tuning — is identical for every
//! schema, so it can be done *ahead of time*. A background replenisher keeps
//! `PG_VM_POOL_WARM_SPARES` VMs (named `spare-pg-*`, TTL 0) booted with
//! Postgres up and an empty cluster. When a schema needs a brand-new VM
//! (first connect, or a restore from S3 whose old VM was killed), it claims a
//! spare: create the schema database on it, restore if needed, done — the
//! S3-restore path drops from create+boot+initdb+restore to just restore.
//!
//! A claimed spare **keeps its `spare-pg-*` name** (the SDK has no rename);
//! the durable registry's `schema → sandbox-id` mapping is what binds it, as
//! it already does for every VM. Consequences: the dashboard shows the spare
//! name (the schema still resolves via the registry map), and the
//! find-by-name rescue path can't find these VMs if the registry file is ever
//! lost — one more reason `PG_VM_POOL_STATE_FILE` should be an absolute,
//! durable path.
//!
//! Who is a spare, authoritatively: a *running* `spare-pg-*` sandbox whose id
//! is neither bound to a schema in the registry (the claim outlives restarts
//! through that binding) nor claimed in this process's memory. The daemon list
//! is the source of truth, so the pool needs no persistence of its own and
//! adopts surviving spares after a pooler restart.

use std::collections::HashSet;
use std::sync::Mutex as StdMutex;
use std::time::{SystemTime, UNIX_EPOCH};

use heyo_sdk::{Sandbox, SandboxStatus};
use tracing::{info, warn};

use crate::config::Config;
use crate::vm;

/// Name prefix for spare VMs. Deliberately does *not* start with `pg-`: the
/// dashboard and the find-by-name path treat `pg-<schema>` as pooler-managed,
/// and a spare must never be mistaken for (or found as) a schema VM.
pub const SPARE_PREFIX: &str = "spare-pg-";

/// Upper bound on the configured pool size — spares hold RAM and a thin disk
/// each, and a typo like `WARM_SPARES=100` shouldn't boot a fleet.
pub const MAX_SPARES: usize = 16;

pub struct SparePool {
    target: usize,
    /// Sandbox ids claimed by this process (bound to a schema, or mid-claim).
    /// In-memory only: after a restart the registry's id bindings provide the
    /// same exclusion for successful claims. A claim whose bring-up *failed*
    /// downstream is released via [`Self::release_failed`], which kills the
    /// spare — its state is ambiguous after a partial claim, so it must never
    /// return to the pool.
    claimed: StdMutex<HashSet<String>>,
}

impl SparePool {
    pub fn new(target: usize) -> Self {
        Self {
            target: target.min(MAX_SPARES),
            claimed: StdMutex::new(HashSet::new()),
        }
    }

    /// Snapshot of the ids this process has claimed (or is mid-claim on) —
    /// the exclusion set a purge needs so it never deletes a spare that a
    /// bring-up is loading a schema into right now.
    pub fn claimed_ids(&self) -> HashSet<String> {
        self.claimed.lock().unwrap().clone()
    }

    /// Claim one warm spare, excluding `bound` ids (schema-bound per the
    /// registry). Returns a connected handle, or `None` when no spare is
    /// available (caller falls back to a cold create).
    pub async fn take(&self, bound: &HashSet<String>) -> Option<Sandbox> {
        // Retried: a transient list failure here doesn't just skip the spare —
        // it silently downgrades the caller to a full cold create, adding
        // daemon load at exactly the wrong moment.
        let infos = match vm::list_with_retry().await {
            Ok(l) => l,
            Err(e) => {
                warn!("listing sandboxes to claim a warm spare failed: {e:#}");
                return None;
            }
        };
        let id = {
            let mut claimed = self.claimed.lock().unwrap();
            let id = infos
                .iter()
                .filter(|s| {
                    s.status == SandboxStatus::Running
                        && s.name.starts_with(SPARE_PREFIX)
                        && !bound.contains(&s.id)
                        && !claimed.contains(&s.id)
                })
                .map(|s| s.id.clone())
                .next()?;
            claimed.insert(id.clone());
            id
        };
        match Sandbox::connect(id.clone(), vm::local_opts()) {
            Ok(sb) => Some(sb),
            Err(e) => {
                warn!("connecting to claimed warm spare {id} failed: {e:#}");
                self.claimed.lock().unwrap().remove(&id);
                None
            }
        }
    }

    /// Release a claim whose bring-up failed *after* [`Self::take`] succeeded
    /// (restore error, ready-timeout, …). The spare's state is ambiguous —
    /// the failed attempt may have created the schema's database or partially
    /// restored into it — so it is never returned to the pool: it is killed
    /// (disk and all) and the replenisher builds a fresh, clean one. The id
    /// leaves `claimed` only on a confirmed kill; a spare we couldn't kill
    /// stays claimed forever, which keeps it out of inventory and out of
    /// purge's reach — a dirty spare must never be handed to another schema.
    /// Without this release, every failed claim leaked a running VM plus its
    /// data disk for the life of the process, and the replenisher — which
    /// also can't see claimed ids as inventory — booted a replacement on top.
    pub async fn release_failed(&self, id: &str) {
        match Sandbox::connect(id.to_string(), vm::local_opts()) {
            Ok(sb) => match sb.kill().await {
                Ok(()) => {
                    self.claimed.lock().unwrap().remove(id);
                    info!("warm-spares: killed spare {id} after a failed bring-up (claim released)");
                }
                Err(e) => warn!(
                    "warm-spares: killing spare {id} after a failed bring-up failed: {e:#}; \
                     leaving it claimed so it can never serve another schema — needs manual \
                     cleanup (heyvm delete {id})"
                ),
            },
            Err(e) => warn!(
                "warm-spares: cannot connect to spare {id} to release its failed claim: {e:#}; \
                 leaving it claimed — needs manual cleanup (heyvm delete {id})"
            ),
        }
    }

    /// One replenish pass, in three moves (see [`plan_replenish`]):
    ///
    /// 1. **Restart** stranded stopped spares back into the pool — far
    ///    cheaper than create+boot+initdb, and it heals the leak where a
    ///    stopped spare (pooler restart, bulk stop, daemon hiccup) became
    ///    invisible forever: not Running so never counted as available, never
    ///    claimable, never cleaned up — while the replenisher kept building
    ///    fresh ones on top.
    /// 2. **Create** whatever deficit remains.
    /// 3. **Delete** stopped surplus. Safe by construction: a successfully
    ///    claimed spare is bound in the durable registry (`store.put` is
    ///    fsync'd before the schema is ever served), so a spare that is
    ///    stopped AND unbound AND unclaimed was never anyone's database —
    ///    it holds an empty initdb cluster and nothing else. Running spares
    ///    are never auto-deleted, surplus or not.
    ///
    /// Returns how many sandboxes were acted on, for the supervisor's
    /// heartbeat. A creation failure ends the pass — the supervisor retries
    /// next tick, and grinding on during an outage helps nobody.
    pub async fn replenish(&self, cfg: &Config, bound: &HashSet<String>) -> usize {
        let infos = match Sandbox::list(vm::local_opts()).await {
            Ok(l) => l,
            Err(e) => {
                warn!("warm-spares: listing sandboxes failed; skipping pass: {e:#}");
                return 0;
            }
        };
        let spares: Vec<(String, bool)> = infos
            .iter()
            .filter(|s| s.name.starts_with(SPARE_PREFIX))
            .map(|s| (s.id.clone(), s.status == SandboxStatus::Running))
            .collect();
        let plan = {
            let claimed = self.claimed.lock().unwrap();
            plan_replenish(&spares, bound, &claimed, self.target)
        };

        let mut acted = 0usize;
        for id in &plan.start {
            match Sandbox::connect(id.clone(), vm::local_opts()) {
                Ok(sb) => {
                    // Opening a stopped disk — exclusive with reclaim passes,
                    // and bounded by the bring-up gate like every other boot.
                    let _slot = crate::vm::bringup_slot("warm-spares").await;
                    let _permit = crate::reclaim::boot_permit().await;
                    match sb.start().await {
                        Ok(()) => {
                            info!("warm-spares: restarted stranded spare {id}");
                            acted += 1;
                        }
                        Err(e) => warn!("warm-spares: restarting spare {id} failed: {e:#}"),
                    }
                }
                Err(e) => warn!("warm-spares: connecting to spare {id} failed: {e:#}"),
            }
        }
        for _ in 0..plan.create {
            let name = format!("{SPARE_PREFIX}{}", suffix());
            match vm::create_spare(cfg, &name).await {
                Ok(sb) => {
                    info!("warm-spares: created spare {name} ({})", sb.sandbox_id());
                    acted += 1;
                }
                Err(e) => {
                    warn!("warm-spares: creating {name} failed; ending pass (will retry): {e:#}");
                    break;
                }
            }
        }
        for id in &plan.delete {
            match Sandbox::connect(id.clone(), vm::local_opts()) {
                Ok(sb) => match sb.kill().await {
                    Ok(()) => {
                        info!("warm-spares: deleted surplus stopped spare {id} (never claimed)");
                        acted += 1;
                    }
                    Err(e) => warn!("warm-spares: deleting surplus spare {id} failed: {e:#}"),
                },
                Err(e) => warn!("warm-spares: connecting to surplus spare {id} failed: {e:#}"),
            }
        }
        if acted > 0 {
            info!(
                "warm-spares: pass done — restarted {}, created up to {}, deleted {} surplus \
                 (target {})",
                plan.start.len(),
                plan.create,
                plan.delete.len(),
                self.target
            );
        }
        acted
    }
}

/// What one replenish pass should do. `spares` is every spare-named sandbox
/// as `(id, is_running)`; `bound` are registry-bound ids (claimed spares),
/// `claimed` this process's in-flight claims. Free running spares count
/// toward the target; the deficit is filled from free STOPPED spares first
/// (restart beats create), then by creating; free stopped spares beyond that
/// are surplus to delete. Bound/claimed spares are untouchable in every set.
struct ReplenishPlan {
    start: Vec<String>,
    create: usize,
    delete: Vec<String>,
}

fn plan_replenish(
    spares: &[(String, bool)],
    bound: &HashSet<String>,
    claimed: &HashSet<String>,
    target: usize,
) -> ReplenishPlan {
    let free = |id: &String| !bound.contains(id) && !claimed.contains(id);
    let running = spares.iter().filter(|(id, up)| *up && free(id)).count();
    let stopped: Vec<&String> = spares
        .iter()
        .filter(|(id, up)| !*up && free(id))
        .map(|(id, _)| id)
        .collect();
    let deficit = target.saturating_sub(running);
    let start: Vec<String> = stopped.iter().take(deficit).map(|s| (*s).clone()).collect();
    ReplenishPlan {
        create: deficit - start.len(),
        delete: stopped.iter().skip(deficit).map(|s| (*s).clone()).collect(),
        start,
    }
}

/// Short unique-enough suffix for a spare name (time-derived; collisions are
/// rejected by the daemon's create as a duplicate name and retried next pass).
fn suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0);
    format!("{:08x}", (nanos ^ (std::process::id() as u64)) & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_is_capped() {
        assert_eq!(SparePool::new(100).target, MAX_SPARES);
        assert_eq!(SparePool::new(2).target, 2);
    }

    #[test]
    fn spare_prefix_is_not_schema_shaped() {
        // The dashboard/find-by-name convention treats `pg-<schema>` as a
        // schema VM; a spare name must never parse that way.
        assert!(!SPARE_PREFIX.starts_with("pg-"));
    }

    fn ids(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plan_restarts_stranded_before_creating_and_deletes_surplus() {
        let spares: Vec<(String, bool)> = vec![
            ("run1".into(), true),   // free, running → counts toward target
            ("stop1".into(), false), // free, stopped → restart candidate
            ("stop2".into(), false), // free, stopped → restart candidate
            ("stop3".into(), false), // free, stopped → surplus once target met
        ];
        let none = HashSet::new();
        // Target 3: 1 running + restart 2 stopped; the 3rd stopped is surplus.
        let p = plan_replenish(&spares, &none, &none, 3);
        assert_eq!(p.start, vec!["stop1".to_string(), "stop2".into()]);
        assert_eq!(p.create, 0);
        assert_eq!(p.delete, vec!["stop3".to_string()]);

        // Target 5: all stopped restarted, remainder created, nothing deleted.
        let p = plan_replenish(&spares, &none, &none, 5);
        assert_eq!(p.start.len(), 3);
        assert_eq!(p.create, 1);
        assert!(p.delete.is_empty());

        // Target met by running spares alone: nothing started or created,
        // every free stopped spare is surplus.
        let p = plan_replenish(&spares, &none, &none, 1);
        assert!(p.start.is_empty());
        assert_eq!(p.create, 0);
        assert_eq!(p.delete.len(), 3);
    }

    #[test]
    fn plan_never_touches_bound_or_claimed_spares() {
        // The data-safety property: a spare bound to a schema in the registry
        // (a claimed spare that was later idle-stopped) must never appear in
        // start OR delete — it is that schema's database.
        let spares: Vec<(String, bool)> = vec![
            ("bound-stopped".into(), false),
            ("claimed-stopped".into(), false),
            ("free-stopped".into(), false),
        ];
        let bound = ids(&["bound-stopped"]);
        let claimed = ids(&["claimed-stopped"]);
        let p = plan_replenish(&spares, &bound, &claimed, 0);
        assert!(p.start.is_empty());
        assert_eq!(
            p.delete,
            vec!["free-stopped".to_string()],
            "only the never-claimed spare is deletable"
        );
        // And bound/claimed running spares don't count as available either.
        let running: Vec<(String, bool)> = vec![("bound-run".into(), true)];
        let p = plan_replenish(&running, &ids(&["bound-run"]), &HashSet::new(), 1);
        assert_eq!(p.create, 1, "a bound spare is not pool inventory");
    }
}
