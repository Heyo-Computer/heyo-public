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
//!
//! Two properties the implementation is built around:
//!
//! * **Claiming must not talk to the daemon's listing.** `GET
//!   /deployed-sandboxes` is the single most expensive, most lock-contended
//!   call heyvmd serves (it drains its whole handle map under a write lock),
//!   and on a host with thousands of sandboxes it is seconds, not
//!   milliseconds. A spare exists to make a bring-up instant, so listing on the
//!   claim path spends the entire saving before the schema is served. The
//!   replenisher lists on its own cadence and publishes the ids it verified;
//!   [`SparePool::take`] pops one from that inventory and does a single
//!   by-id lookup to confirm it is still there.
//! * **A spare counts only when its Postgres answers.** The daemon reports
//!   `running` as soon as the guest signals ready, which is not the same as a
//!   healthy postmaster: a boot that half-failed (sick disk, `initdb` that
//!   never finished) leaves a "running" spare that poisons the first claim that
//!   takes it — the claim fails, the schema falls back to a cold create anyway,
//!   and the pool reports itself full the whole time. Every pass probes each
//!   spare's 5432; a spare that stays unreachable is deleted and rebuilt.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use heyo_sdk::{HeyoError, Sandbox, SandboxStatus};
use tokio::sync::Notify;
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

/// How many spares one pass builds (creates or restarts) at a time.
///
/// Not one — a pass that builds serially takes `target × boot` to fill an
/// empty pool, which on a cold host is many minutes during which the pool is
/// reported empty and every schema pays a cold create. Not unbounded either:
/// each build is a daemon-side rootfs clone + `mke2fs` + guest boot, and
/// heyvmd runs those on its async workers. This sits at the daemon's own
/// concurrent-create ceiling; the pooler's bring-up gate meters the calls
/// themselves, so client bring-ups still interleave.
const SPARE_BUILD_CONCURRENCY: usize = 3;

/// How many spares are health-probed at a time. A probe is a by-id lookup plus
/// a TCP connect, so this can be wider than the build fan-out.
const SPARE_PROBE_CONCURRENCY: usize = 8;

/// Probe attempts for a spare this pass just built, and the gap between them.
/// heyvmd reports a VM running the moment the guest signals ready; the
/// postmaster can bind a moment later, and a single miss there would hold a
/// good spare off the shelf for a whole tick.
const FRESH_PROBE_ATTEMPTS: usize = 4;
const PROBE_RETRY_DELAY: Duration = Duration::from_secs(2);

/// How long a spare may fail its health probe before it is deleted and
/// rebuilt. Long enough to ride out a daemon hiccup or a guest that is slow to
/// bind 5432 after a restart, short enough that a sick spare doesn't sit in
/// inventory (invisible to the target, unclaimable) for the life of the pool.
const SPARE_SICK_GRACE: Duration = Duration::from_secs(300);

/// Cap on spares deleted for ill health in one pass. When *every* spare probes
/// sick the cause is usually one thing wrong with the host or the daemon, not
/// a dozen independently broken VMs — and deleting the whole pool at once then
/// rebuilding it is precisely the create burst that wedges heyvmd. Draining a
/// couple per pass heals a genuinely sick pool within a few minutes and turns
/// a host-wide fault into a slow drip instead of a stampede.
const SPARE_MAX_CULLS_PER_PASS: usize = 2;

/// Cap on the by-id confirmations one [`SparePool::take`] will spend before
/// giving up and letting the caller cold-create. Bounds the claim path's cost
/// when the published inventory has gone stale wholesale (e.g. heyvmd
/// restarted and dropped every spare).
const TAKE_MAX_ATTEMPTS: usize = 3;

/// How many spares one pass chills (stops and moves to the vehicle shelf) at
/// a time. A stop is cheap for the pooler but takes the daemon a couple of
/// seconds per VM (it SIGKILLs Firecracker and waits for the process), and a
/// pass must not sit on a wide fan of them while the warm shelf is short.
const SPARE_CHILL_CONCURRENCY: usize = 2;

/// How long a stranded-spare restart waits on a reclaim permit before giving
/// up for this pass. [`SparePool::replenish`] does not return until every build
/// in its batch has, and passes are serialized in one supervisor loop, so a
/// restart parked on a reclaim pass freezes the whole shelf behind it — the
/// surplus cull and the republish of ready spares included. The symptom is the
/// worst one available: `take` finds an empty shelf and every new schema pays a
/// cold create, for as long as the pass runs. Restarting one spare is not worth
/// that, so it is abandoned and retried on the next pass (60s away, or sooner —
/// every claim wakes the loop).
const SPARE_PERMIT_WAIT: Duration = Duration::from_secs(15);

pub struct SparePool {
    target: usize,
    /// How many spares to keep chilled — see [`Self::chilled`].
    chilled_target: usize,
    /// Sandbox ids claimed by this process (bound to a schema, or mid-claim).
    /// In-memory only: after a restart the registry's id bindings provide the
    /// same exclusion for successful claims. A claim whose bring-up *failed*
    /// downstream is released via [`Self::release_failed`], which kills the
    /// spare — its state is ambiguous after a partial claim, so it must never
    /// return to the pool.
    claimed: StdMutex<HashSet<String>>,
    /// Ids the last pass verified as running-with-Postgres-answering, ready to
    /// hand out. This — not a fresh daemon listing — is what [`Self::take`]
    /// draws from, so a claim costs one by-id lookup instead of a full
    /// inventory fetch. Rebuilt every pass, so a stale entry (a spare deleted
    /// out of band) survives at most until the next one, and `take` confirms
    /// each id before handing it over anyway.
    ///
    /// Lock order where both are held: `claimed` first, then `ready`, then
    /// `chilled`.
    ready: StdMutex<VecDeque<String>>,
    /// Spares this pool verified healthy and then deliberately **stopped**,
    /// held as image-restore vehicles.
    ///
    /// An image restore does not want a running VM. It overwrites its
    /// vehicle's data disk with the archived filesystem and boots on that, so
    /// every bit of the boot and `initdb` a warm spare paid for is thrown
    /// away — and handing it a running spare means stopping that VM first
    /// (~2.1s for the daemon to SIGKILL Firecracker and ack) and then waiting
    /// for the disk fd to be released before the swap can start. Measured on a
    /// production host, that stop-and-wait was ~4.8s of a ~6.9s thaw, against
    /// ~2.8s of actual work (decompress + one boot).
    ///
    /// So the pool keeps a few spares already in the state a restore wants.
    /// They are chilled by the replenisher, off any client's critical path,
    /// and only ever from spares whose Postgres answered while they were
    /// running — a vehicle that was never verified would also have no
    /// readable `PG_VERSION` for [`crate::imgarchive`]'s major-compatibility
    /// gate to check the archive against.
    ///
    /// Stopped VMs hold no RAM, so this shelf does not compete with `ready`
    /// for memory; it costs one thin data disk per vehicle.
    ///
    /// Ids here are untouchable by the replenish plan — a chilled spare is
    /// stopped, unbound and unclaimed, which is exactly the shape
    /// [`plan_replenish`] would otherwise restart as deficit or delete as
    /// surplus.
    chilled: StdMutex<VecDeque<String>>,
    /// Spares whose Postgres has been unreachable since the given instant.
    /// Kept out of inventory, and deleted once past [`SPARE_SICK_GRACE`] so
    /// the pool rebuilds them rather than reporting itself full of VMs no
    /// claim can use.
    sick_since: StdMutex<HashMap<String, Instant>>,
    /// Poked whenever the pool shrinks (a spare claimed, or a failed claim
    /// killed) so the replenisher rebuilds the deficit immediately instead of
    /// on its next tick — a claimed spare must not leave the pool short for
    /// up to a full tick during exactly the burst that is draining it.
    poke: Arc<Notify>,
}

impl SparePool {
    pub fn new(target: usize, chilled_target: usize) -> Self {
        Self {
            target: target.min(MAX_SPARES),
            chilled_target: chilled_target.min(MAX_SPARES),
            claimed: StdMutex::new(HashSet::new()),
            ready: StdMutex::new(VecDeque::new()),
            chilled: StdMutex::new(VecDeque::new()),
            sick_since: StdMutex::new(HashMap::new()),
            poke: Arc::new(Notify::new()),
        }
    }

    /// Handle the replenisher's supervisor selects on alongside its tick.
    /// `notify_one` stores a permit when nobody is waiting, so a poke landing
    /// mid-pass wakes the next wait instead of being lost.
    pub fn replenish_wake(&self) -> Arc<Notify> {
        self.poke.clone()
    }

    /// Snapshot of the ids this process has claimed (or is mid-claim on) —
    /// the exclusion set a purge needs so it never deletes a spare that a
    /// bring-up is loading a schema into right now.
    pub fn claimed_ids(&self) -> HashSet<String> {
        self.claimed.lock().unwrap().clone()
    }

    /// `(ready, target)`: warm spares on the shelf right now vs the
    /// configured pool size — the dashboard's pool-depth readout. Zero ready
    /// means the next cold connect pays a full create + boot + initdb.
    pub fn depth(&self) -> (usize, usize) {
        (self.ready.lock().unwrap().len(), self.target)
    }

    /// `(chilled, target)`: stopped vehicles on the shelf vs the configured
    /// count. Zero chilled means the next image restore falls back to stopping
    /// a running spare — correct, but ~4.8s slower.
    pub fn chilled_depth(&self) -> (usize, usize) {
        (self.chilled.lock().unwrap().len(), self.chilled_target)
    }

    /// Ids on the chilled shelf — the exemption set the replenish plan needs.
    /// A chilled spare is stopped, unbound and unclaimed, which is precisely
    /// what [`plan_replenish`] restarts as deficit or deletes as surplus, so
    /// without this the pool would spend every pass undoing its own vehicles.
    fn chilled_ids(&self) -> HashSet<String> {
        self.chilled.lock().unwrap().iter().cloned().collect()
    }

    /// Claim one **stopped** spare as an image-restore vehicle, excluding
    /// `bound` ids. `None` when the chilled shelf is empty, which sends the
    /// caller to the running-spare fallback.
    ///
    /// Cheap for the same reason [`Self::take`] is: it pops the shelf the
    /// replenisher published and spends one by-id `get` confirming the sandbox
    /// is still there and still stopped. A vehicle that has drifted back to
    /// running is not handed over — taking it would reintroduce exactly the
    /// stop this shelf exists to avoid — it is unclaimed and left for the next
    /// pass to re-plan.
    pub async fn take_chilled(&self, bound: &HashSet<String>) -> Option<Sandbox> {
        for _ in 0..TAKE_MAX_ATTEMPTS {
            // Pop-and-claim in one critical section, as `take` does: the id is
            // off the shelf and in `claimed` before any await.
            let popped = {
                let mut claimed = self.claimed.lock().unwrap();
                let mut chilled = self.chilled.lock().unwrap();
                loop {
                    match chilled.pop_front() {
                        Some(id) if !bound.contains(&id) && claimed.insert(id.clone()) => {
                            break Some(id);
                        }
                        Some(_) => continue,
                        None => break None,
                    }
                }
            };
            let Some(id) = popped else { break };
            let sb = match Sandbox::connect(id.clone(), vm::local_opts()) {
                Ok(sb) => sb,
                Err(e) => {
                    warn!("connecting to claimed chilled vehicle {id} failed: {e:#}");
                    self.unclaim(&id);
                    continue;
                }
            };
            match sb.get().await {
                Ok(info) if info.status == SandboxStatus::Stopped => {
                    crate::inventory::insert(&info.name, &info.id);
                    crate::events::record(crate::events::Event::SpareClaimed);
                    self.poke.notify_one();
                    return Some(sb);
                }
                Ok(info) => {
                    warn!(
                        "warm-spares: chilled vehicle {id} is {:?}, not stopped — skipping it",
                        info.status
                    );
                    self.unclaim(&id);
                }
                Err(HeyoError::NotFound(_)) => {
                    info!("warm-spares: chilled vehicle {id} no longer exists — skipping it");
                    self.unclaim(&id);
                }
                // Unlike `take`, a flaking daemon is not a reason to use it
                // anyway: the whole value of this shelf is that the vehicle is
                // already stopped, and `swap_and_boot` skips its stop on that
                // promise. Unconfirmed, the fallback path is the safe answer.
                Err(e) => {
                    warn!("warm-spares: confirming chilled vehicle {id} failed ({e:#}); \
                           falling back to a running spare");
                    self.unclaim(&id);
                }
            }
        }
        self.poke.notify_one();
        None
    }

    /// Claim one warm spare, excluding `bound` ids (schema-bound per the
    /// registry). Returns a connected handle, or `None` when no spare is
    /// available (caller falls back to a cold create).
    ///
    /// Deliberately cheap: it pops from the inventory the replenisher
    /// published and spends one by-id `get` confirming the sandbox is still
    /// running. No listing — see the module docs. Every id it takes off the
    /// shelf is either claimed or discarded, never silently put back, so two
    /// concurrent claims can't collide on one spare.
    pub async fn take(&self, bound: &HashSet<String>) -> Option<Sandbox> {
        for _ in 0..TAKE_MAX_ATTEMPTS {
            // Pop-and-claim in one critical section: the id is off the shelf
            // and in `claimed` before any await, so nothing else can take it.
            let popped = {
                let mut claimed = self.claimed.lock().unwrap();
                let mut ready = self.ready.lock().unwrap();
                loop {
                    match ready.pop_front() {
                        Some(id) if !bound.contains(&id) && claimed.insert(id.clone()) => {
                            break Some(id);
                        }
                        Some(_) => continue,
                        None => break None,
                    }
                }
            };
            let Some(id) = popped else { break };
            let sb = match Sandbox::connect(id.clone(), vm::local_opts()) {
                Ok(sb) => sb,
                Err(e) => {
                    warn!("connecting to claimed warm spare {id} failed: {e:#}");
                    self.unclaim(&id);
                    continue;
                }
            };
            // Confirm the shelf isn't stale. A spare deleted out of band (or
            // lost to a heyvmd restart) must not be handed to a schema: the
            // bring-up would fail on it and the client would pay a full cold
            // create *after* that failure rather than instead of it.
            match sb.get().await {
                Ok(info) if info.status == SandboxStatus::Running => {
                    // Id in hand — record it under whatever name the daemon
                    // reports (still its spare name until the registry binds).
                    crate::inventory::insert(&info.name, &info.id);
                    crate::events::record(crate::events::Event::SpareClaimed);
                    self.poke.notify_one();
                    return Some(sb);
                }
                Ok(info) => {
                    warn!(
                        "warm-spares: shelved spare {id} is {:?}, not running — skipping it",
                        info.status
                    );
                    self.unclaim(&id);
                }
                Err(HeyoError::NotFound(_)) => {
                    info!("warm-spares: shelved spare {id} no longer exists — skipping it");
                    self.unclaim(&id);
                }
                // The daemon is flaking, not the spare. Use it: the bring-up
                // that follows probes Postgres itself and releases the claim
                // (killing the spare) if it can't be served.
                Err(e) => {
                    warn!("warm-spares: confirming spare {id} failed ({e:#}); claiming it anyway");
                    crate::events::record(crate::events::Event::SpareClaimed);
                    self.poke.notify_one();
                    return Some(sb);
                }
            }
        }
        // Inventory is empty or wholesale stale — the caller cold-creates, and
        // the poke gets the replenisher rebuilding immediately.
        self.poke.notify_one();
        None
    }

    /// Drop a claim on an id we never actually used (nothing was done to the
    /// VM, so unlike [`Self::release_failed`] it must not be killed).
    fn unclaim(&self, id: &str) {
        self.claimed.lock().unwrap().remove(id);
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
        // Whatever happens to the kill, this id is not inventory any more —
        // on either shelf.
        self.ready.lock().unwrap().retain(|r| r != id);
        self.chilled.lock().unwrap().retain(|r| r != id);
        self.sick_since.lock().unwrap().remove(id);
        match Sandbox::connect(id.to_string(), vm::local_opts()) {
            Ok(sb) => match sb.kill().await {
                Ok(()) => {
                    self.claimed.lock().unwrap().remove(id);
                    self.poke.notify_one();
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
    /// Before all of that, every free running spare is **health-probed**, and
    /// only the ones whose Postgres answers count toward the target or reach
    /// the shelf [`Self::take`] draws from. A spare that stays unreachable for
    /// [`SPARE_SICK_GRACE`] is deleted so a replacement gets built.
    ///
    /// Returns how many sandboxes were acted on, for the supervisor's
    /// heartbeat. Builds run [`SPARE_BUILD_CONCURRENCY`]-wide: an empty pool
    /// with a target of a dozen must not take a dozen sequential boots to
    /// fill, and one failed build must not cancel the rest of the pass (the
    /// old behaviour: a single transient create error left the pool empty
    /// until the next tick, every tick).
    pub async fn replenish(&self, cfg: &Config, bound: &HashSet<String>) -> usize {
        // Retried: on a busy daemon this listing is exactly what flakes, and
        // skipping the pass leaves the pool short for another whole tick.
        let infos = match vm::list_with_retry().await {
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

        // 1. Health. Only free running spares are probed — a bound one is some
        //    schema's VM and a claimed one is mid-bring-up; neither is ours.
        let free_running: Vec<String> = {
            let claimed = self.claimed.lock().unwrap();
            spares
                .iter()
                .filter(|(id, up)| *up && !bound.contains(id) && !claimed.contains(id))
                .map(|(id, _)| id.clone())
                .collect()
        };
        let (mut healthy, sick) = self.probe_all(cfg, free_running, 1).await;
        let mut acted = self.cull_sick(&sick).await;

        // 2. Plan against *healthy* inventory: a sick spare is not something a
        //    claim can use, so it must not hold the pool at target. It stays
        //    out of the plan entirely (it is running, so it is neither a
        //    restart nor a delete candidate) until its grace expires above.
        //    Chilled vehicles are exempt for a different reason: they are
        //    stopped on purpose, so the plan would read them as deficit to
        //    restart or surplus to delete and spend every pass undoing them.
        let sick_ids: HashSet<&String> = sick.iter().collect();
        let chilled_ids = self.chilled_ids();
        let plan_input: Vec<(String, bool)> = spares
            .iter()
            .filter(|(id, _)| !sick_ids.contains(id) && !chilled_ids.contains(id))
            .cloned()
            .collect();
        let plan = {
            let claimed = self.claimed.lock().unwrap();
            plan_replenish(&plan_input, bound, &claimed, self.target)
        };

        // Nothing is built while clients are queued for bring-ups. Every spare
        // built now competes for the bring-up slots and the daemon's create
        // queue those clients are waiting on, and on a host refusing VMs for
        // memory it fails and is retried the next pass (thousands an hour in
        // the 2026-09-15 storm). Spares exist to make clients faster; they
        // must never be what keeps one waiting. The health pass above and the
        // surplus deletes below still run.
        let mut plan = plan;
        let queued = vm::bringups_waiting();
        if queued > 0 && (plan.create > 0 || !plan.start.is_empty()) {
            info!(
                "warm-spares: {queued} client bring-up(s) queued; not building {} spare(s) \
                 this pass",
                plan.create + plan.start.len()
            );
            plan.start.clear();
            plan.create = 0;
        }

        // 3. Build the deficit: restart stranded spares and create the rest,
        //    concurrently and independently of each other's failures.
        let (restarted, created) = futures::future::join(
            build_all(plan.start.iter().cloned().map(restart_spare)),
            build_all((0..plan.create).map(|_| create_spare(cfg))),
        )
        .await;
        acted += restarted.len() + created.len();

        // 4. Delete stopped surplus.
        for id in &plan.delete {
            if kill_spare(id, "surplus stopped spare (never claimed)").await {
                acted += 1;
            }
        }

        // 5. Publish the shelf: verified survivors plus whatever this pass
        //    just built and confirmed. Claimed ids are filtered under the lock
        //    so a claim that landed mid-pass can't be handed out twice.
        //    Fresh builds get a few probe attempts: the daemon calls a VM ready
        //    the moment the guest signals it, and the postmaster can take a
        //    beat longer to bind — one failed probe there would park a perfectly
        //    good spare off the shelf until the next pass.
        let (fresh, _) = self
            .probe_all(
                cfg,
                restarted.into_iter().chain(created).collect(),
                FRESH_PROBE_ATTEMPTS,
            )
            .await;
        healthy.extend(fresh);

        // 6. Chill the vehicle deficit: stop healthy running spares until the
        //    vehicle shelf is at target. Here, in the replenisher, is the
        //    whole point — a stop costs the daemon a couple of seconds, and
        //    paying it in the background is what takes it off the critical
        //    path of the image restore that would otherwise pay it with a
        //    client waiting. Chilling shrinks the warm shelf by what it takes,
        //    and the next pass's deficit rebuilds it.
        let chilled_now = self.chill_deficit(&mut healthy, bound).await;
        acted += chilled_now;

        let shelved = self.publish(healthy, bound);

        let (chilled_depth, chilled_target) = self.chilled_depth();
        if acted > 0 || shelved < self.target || chilled_depth < chilled_target {
            info!(
                "warm-spares: pass done — {shelved}/{} ready, {chilled_depth}/{chilled_target} \
                 chilled, restarted {}, created {}, chilled {chilled_now}, deleted {} surplus, \
                 {} sick",
                self.target,
                plan.start.len(),
                plan.create,
                plan.delete.len(),
                sick.len(),
            );
        }
        acted
    }

    /// Stop healthy running spares until the chilled shelf reaches its target,
    /// moving each out of `healthy` (it is no longer a warm spare) and onto
    /// the vehicle shelf. Returns how many were chilled.
    ///
    /// Held to the same politeness rule as building: nothing is chilled while
    /// clients are queued for bring-ups. Chilling takes a spare *off* the warm
    /// shelf, and doing that during the burst that is draining it would make
    /// the queued clients cold-create — spares exist to make clients faster,
    /// and that applies to the vehicle shelf as much as to the warm one.
    async fn chill_deficit(&self, healthy: &mut Vec<String>, bound: &HashSet<String>) -> usize {
        let need = self
            .chilled_target
            .saturating_sub(self.chilled.lock().unwrap().len());
        if need == 0 {
            return 0;
        }
        let queued = vm::bringups_waiting();
        if queued > 0 {
            info!(
                "warm-spares: {queued} client bring-up(s) queued; not chilling {need} \
                 vehicle(s) this pass"
            );
            return 0;
        }
        // Reserve before stopping, in one critical section: take the ids off
        // the ready shelf AND into `claimed`. Chilling is the only pass
        // operation that touches a spare a client could be claiming right now
        // — sick culls and surplus deletes only ever act on spares that were
        // never published — so without this a `take` can hand a running spare
        // to a bring-up in the instant before this pass stops it, and the
        // client's VM dies under it. `claimed` is the same exclusion a real
        // claim uses, so nothing else can reach these while the stop is in
        // flight; every path below releases it.
        let candidates: Vec<String> = {
            let mut claimed = self.claimed.lock().unwrap();
            let mut ready = self.ready.lock().unwrap();
            let picked: Vec<String> = healthy
                .iter()
                .filter(|id| !bound.contains(*id) && !claimed.contains(*id))
                .take(need)
                .cloned()
                .collect();
            for id in &picked {
                claimed.insert(id.clone());
                ready.retain(|r| r != id);
            }
            picked
        };
        if candidates.is_empty() {
            return 0;
        }
        let results: Vec<(String, bool)> =
            futures::stream::iter(candidates.into_iter().map(|id| async move {
                let ok = chill_spare(id.clone()).await.is_some();
                (id, ok)
            }))
            .buffer_unordered(SPARE_CHILL_CONCURRENCY)
            .collect()
            .await;

        let (stopped, failed): (Vec<_>, Vec<_>) = results.into_iter().partition(|(_, ok)| *ok);
        let stopped: Vec<String> = stopped.into_iter().map(|(id, _)| id).collect();
        {
            // Release every reservation. A stopped one moves to the vehicle
            // shelf; one whose stop failed simply goes back to being a warm
            // spare — it is still in `healthy`, so `publish` re-shelves it.
            let mut claimed = self.claimed.lock().unwrap();
            let mut chilled = self.chilled.lock().unwrap();
            for id in &stopped {
                claimed.remove(id);
                chilled.push_back(id.clone());
            }
            for (id, _) in &failed {
                claimed.remove(id);
            }
        }
        healthy.retain(|id| !stopped.contains(id));
        stopped.len()
    }

    /// Probe a set of spares, returning `(usable, sick)`. "Usable" folds in the
    /// unprovable case: no `guest_ip` to dial, or a daemon that errored on the
    /// lookup, means we learned nothing — and ambiguity must never shrink the
    /// pool or delete a VM. Only a spare we positively failed to reach on 5432
    /// is sick.
    async fn probe_all(
        &self,
        cfg: &Config,
        ids: Vec<String>,
        attempts: usize,
    ) -> (Vec<String>, Vec<String>) {
        if ids.is_empty() {
            return (Vec::new(), Vec::new());
        }
        // With direct connect off the pooler reaches Postgres through a tunnel
        // it opens per bring-up, so there is no address to probe here and a
        // "sick" verdict would be an artefact of the deployment shape.
        if !cfg.direct_connect {
            return (ids, Vec::new());
        }
        let results: Vec<(String, bool)> = futures::stream::iter(ids)
            .map(|id| async move {
                let Ok(sb) = Sandbox::connect(id.clone(), vm::local_opts()) else {
                    return (id, true);
                };
                let mut ok = false;
                for attempt in 0..attempts.max(1) {
                    if attempt > 0 {
                        tokio::time::sleep(PROBE_RETRY_DELAY).await;
                    }
                    ok = match vm::pg_listening(&sb).await {
                        Ok(Some(up)) => up,
                        Ok(None) | Err(_) => true,
                    };
                    if ok {
                        break;
                    }
                }
                (id, ok)
            })
            .buffer_unordered(SPARE_PROBE_CONCURRENCY)
            .collect()
            .await;
        let (usable, sick): (Vec<_>, Vec<_>) = results.into_iter().partition(|(_, ok)| *ok);
        (
            usable.into_iter().map(|(id, _)| id).collect(),
            sick.into_iter().map(|(id, _)| id).collect(),
        )
    }

    /// Record this pass's health verdicts and delete the spares that have been
    /// unreachable past [`SPARE_SICK_GRACE`]. Returns how many were deleted.
    async fn cull_sick(&self, sick: &[String]) -> usize {
        let mut doomed: Vec<String> = {
            let mut since = self.sick_since.lock().unwrap();
            // Anything not sick this pass is healthy again (or gone).
            since.retain(|id, _| sick.contains(id));
            let now = Instant::now();
            sick.iter()
                .filter(|id| {
                    now.duration_since(*since.entry((*id).to_string()).or_insert(now))
                        >= SPARE_SICK_GRACE
                })
                .cloned()
                .collect()
        };
        if doomed.len() > SPARE_MAX_CULLS_PER_PASS {
            warn!(
                "warm-spares: {} spares have been unreachable for {SPARE_SICK_GRACE:?} — \
                 deleting {SPARE_MAX_CULLS_PER_PASS} this pass and the rest over following \
                 ones. Wholesale sickness is usually the host or heyvmd, not the VMs; check \
                 the guest network and the daemon before assuming the pool is at fault",
                doomed.len(),
            );
            doomed.truncate(SPARE_MAX_CULLS_PER_PASS);
        }
        let mut killed = 0usize;
        for id in doomed {
            if kill_spare(
                &id,
                "spare whose Postgres never answered (empty by construction)",
            )
            .await
            {
                self.sick_since.lock().unwrap().remove(&id);
                killed += 1;
            }
        }
        killed
    }

    /// Replace the shelf with `ids`, minus anything now bound or claimed.
    /// Returns the resulting depth.
    /// Anything on the chilled shelf is excluded too: the two shelves must
    /// stay disjoint, or a spare restarted out of band could sit on both and
    /// be offered as a warm spare and a stopped vehicle at once.
    fn publish(&self, ids: Vec<String>, bound: &HashSet<String>) -> usize {
        let claimed = self.claimed.lock().unwrap();
        let mut ready = self.ready.lock().unwrap();
        let chilled: HashSet<String> = self.chilled.lock().unwrap().iter().cloned().collect();
        ready.clear();
        let mut seen = HashSet::new();
        for id in ids {
            if !bound.contains(&id)
                && !claimed.contains(&id)
                && !chilled.contains(&id)
                && seen.insert(id.clone())
            {
                ready.push_back(id);
            }
        }
        ready.len()
    }
}

/// Drive a batch of spare builds [`SPARE_BUILD_CONCURRENCY`]-wide, returning
/// the ids that reached a running VM. Failures are logged and dropped: each
/// build is independent, and the next pass retries whatever is still missing.
async fn build_all<F>(builds: impl Iterator<Item = F>) -> Vec<String>
where
    F: std::future::Future<Output = Option<String>>,
{
    futures::stream::iter(builds)
        .buffer_unordered(SPARE_BUILD_CONCURRENCY)
        .filter_map(|r| async move { r })
        .collect()
        .await
}

/// Boot a spare that exists but is stopped — far cheaper than a create.
/// `Some(id)` once it is genuinely up.
async fn restart_spare(id: String) -> Option<String> {
    let sb = match Sandbox::connect(id.clone(), vm::local_opts()) {
        Ok(sb) => sb,
        Err(e) => {
            warn!("warm-spares: connecting to spare {id} failed: {e:#}");
            return None;
        }
    };
    let started = {
        // Opening a stopped disk — exclusive with reclaim work on *this* disk,
        // and bounded by the bring-up gate like every other boot. Permit before
        // slot (see `vm::bring_up_existing`), and bounded so this build can
        // never stall the pass it belongs to (see `SPARE_PERMIT_WAIT`).
        let Some(_permit) = crate::reclaim::boot_permit_within(&id, SPARE_PERMIT_WAIT).await
        else {
            info!(
                "warm-spares: a disk-reclaim pass still holds spare {id}'s disk after \
                 {SPARE_PERMIT_WAIT:?} — leaving it for the next pass"
            );
            return None;
        };
        let _slot = vm::bringup_slot("warm-spares").await;
        sb.start().await
    };
    if let Err(e) = started {
        warn!("warm-spares: restarting spare {id} failed: {e:#}");
        return None;
    }
    // Wait for it here rather than declaring victory on the accepted start: an
    // unverified "restarted" spare that is still booting would count toward
    // the target and be claimed by the next cold start, which then waits out
    // the boot the spare existed to skip.
    match vm::wait_ready(&sb, vm::SPARE_READY_TIMEOUT, "warm-spare").await {
        Ok(()) => {
            info!("warm-spares: restarted stranded spare {id}");
            Some(id)
        }
        Err(e) => {
            warn!("warm-spares: restarted spare {id} never became ready: {e:#}");
            None
        }
    }
}

/// Stop a verified-healthy spare so it can serve as an image-restore vehicle.
/// `Some(id)` once the daemon has acked the stop.
///
/// Only the stop — no wait for Firecracker to release the disk file. The
/// restore that eventually claims this vehicle still runs that check before it
/// touches the disk, and by then the VM will have been stopped for a tick or
/// more, so it is free on the first scan.
async fn chill_spare(id: String) -> Option<String> {
    let sb = match Sandbox::connect(id.clone(), vm::local_opts()) {
        Ok(sb) => sb,
        Err(e) => {
            warn!("warm-spares: connecting to spare {id} to chill it failed: {e:#}");
            return None;
        }
    };
    match tokio::time::timeout(Duration::from_secs(60), sb.stop()).await {
        Ok(Ok(())) => {
            info!("warm-spares: chilled spare {id} — parked stopped as an image-restore vehicle");
            Some(id)
        }
        // Left in `healthy`, so it stays a warm spare and the next pass tries
        // again. A stop that half-landed reads as a stranded stopped spare
        // next pass and is restarted or deleted like any other.
        Ok(Err(e)) => {
            warn!("warm-spares: chilling spare {id} failed (retried next pass): {e:#}");
            None
        }
        Err(_) => {
            warn!("warm-spares: chilling spare {id} timed out (retried next pass)");
            None
        }
    }
}

/// Deploy one brand-new spare. `Some(id)` once the daemon reports it running.
async fn create_spare(cfg: &Config) -> Option<String> {
    let name = format!("{SPARE_PREFIX}{}", suffix());
    match vm::create_spare(cfg, &name).await {
        Ok(sb) => {
            let id = sb.sandbox_id().to_string();
            info!("warm-spares: created spare {name} ({id})");
            Some(id)
        }
        Err(e) => {
            warn!("warm-spares: creating {name} failed (retried next pass): {e:#}");
            None
        }
    }
}

/// Delete a spare by id, logging `why`. Returns whether it is gone.
async fn kill_spare(id: &str, why: &str) -> bool {
    match Sandbox::connect(id.to_string(), vm::local_opts()) {
        Ok(sb) => match sb.kill().await {
            Ok(()) => {
                info!("warm-spares: deleted {id} — {why}");
                true
            }
            Err(e) => {
                warn!("warm-spares: deleting {id} ({why}) failed: {e:#}");
                false
            }
        },
        Err(e) => {
            warn!("warm-spares: connecting to {id} ({why}) failed: {e:#}");
            false
        }
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
        assert_eq!(SparePool::new(100, 0).target, MAX_SPARES);
        assert_eq!(SparePool::new(2, 0).target, 2);
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
    fn publishing_the_shelf_excludes_bound_claimed_and_duplicate_ids() {
        let pool = SparePool::new(4, 0);
        pool.claimed.lock().unwrap().insert("claimed".into());
        let depth = pool.publish(
            vec![
                "free".into(),
                "bound".into(),
                "claimed".into(),
                "free".into(), // a listing that reported it twice
            ],
            &ids(&["bound"]),
        );
        assert_eq!(depth, 1);
        assert_eq!(
            pool.ready.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec!["free".to_string()],
            "only spares nobody owns are claimable inventory"
        );
    }

    #[tokio::test]
    async fn taking_from_an_empty_shelf_asks_for_no_daemon_call() {
        // The claim path must never fall back to a listing: an empty pool has
        // to answer "cold-create" immediately, not pay heyvmd's slowest call
        // to discover there is nothing to hand out.
        let pool = SparePool::new(4, 0);
        assert!(pool.take(&HashSet::new()).await.is_none());
        assert!(pool.claimed.lock().unwrap().is_empty());
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

    #[test]
    fn chilled_target_is_capped() {
        assert_eq!(SparePool::new(4, 100).chilled_target, MAX_SPARES);
        assert_eq!(SparePool::new(4, 3).chilled_target, 3);
    }

    #[test]
    fn a_chilled_vehicle_must_be_exempt_from_the_replenish_plan() {
        // A chilled vehicle is stopped, unbound and unclaimed — exactly the
        // shape the plan restarts as deficit or deletes as surplus. Both
        // halves are asserted: the hazard is real, and the exemption is what
        // stops the pool spending every pass undoing its own vehicles.
        let pool = SparePool::new(1, 1);
        pool.chilled.lock().unwrap().push_back("chilled".into());
        let spares: Vec<(String, bool)> = vec![("run1".into(), true), ("chilled".into(), false)];
        let none = HashSet::new();

        // Unfiltered: the vehicle is deleted as surplus (target met by run1).
        let hazard = plan_replenish(&spares, &none, &none, 1);
        assert_eq!(hazard.delete, vec!["chilled".to_string()]);

        // Filtered the way `replenish` filters it: untouched in every set.
        let chilled = pool.chilled_ids();
        let plan_input: Vec<(String, bool)> = spares
            .iter()
            .filter(|(id, _)| !chilled.contains(id))
            .cloned()
            .collect();
        let p = plan_replenish(&plan_input, &none, &none, 1);
        assert!(p.delete.is_empty(), "a chilled vehicle is not surplus");
        assert!(p.start.is_empty(), "a chilled vehicle is not deficit");
        assert_eq!(p.create, 0);
    }

    #[test]
    fn chilling_makes_the_warm_shelf_short_so_the_next_pass_rebuilds_it() {
        // Chilling takes a spare *off* the warm shelf. Since the vehicle is
        // then exempt from the plan, the next pass sees the warm target
        // unmet and creates a replacement — which is how the pool settles at
        // `target` running plus `chilled_target` stopped rather than
        // cannibalising itself.
        let pool = SparePool::new(2, 1);
        pool.chilled.lock().unwrap().push_back("chilled".into());
        let spares: Vec<(String, bool)> = vec![("run1".into(), true), ("chilled".into(), false)];
        let chilled = pool.chilled_ids();
        let plan_input: Vec<(String, bool)> = spares
            .iter()
            .filter(|(id, _)| !chilled.contains(id))
            .cloned()
            .collect();
        let p = plan_replenish(&plan_input, &HashSet::new(), &HashSet::new(), 2);
        assert_eq!(p.create, 1, "the chilled spare's slot is refilled");
    }

    #[test]
    fn publishing_the_shelf_excludes_chilled_ids() {
        // The two shelves must stay disjoint: a spare restarted out of band
        // could otherwise sit on both and be offered as a warm spare and a
        // stopped vehicle at once.
        let pool = SparePool::new(4, 2);
        pool.chilled.lock().unwrap().push_back("chilled".into());
        let depth = pool.publish(vec!["free".into(), "chilled".into()], &HashSet::new());
        assert_eq!(depth, 1);
        assert_eq!(
            pool.ready.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            vec!["free".to_string()]
        );
    }

    #[tokio::test]
    async fn taking_from_an_empty_chilled_shelf_asks_for_no_daemon_call() {
        // Same contract as `take`: no chilled vehicle must answer immediately
        // so the caller can fall back to a running spare, not pay a listing.
        let pool = SparePool::new(4, 2);
        assert!(pool.take_chilled(&HashSet::new()).await.is_none());
        assert!(pool.claimed.lock().unwrap().is_empty());
    }

}
