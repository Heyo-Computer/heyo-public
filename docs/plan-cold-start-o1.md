# Plan: kill the O(fleet) cold start — daemon name index, by-name lookup, pooler cache, heyvmd threading

Cross-repo work: `pg-fc` (this repo) and the heyo monorepo (`mvm-ctrl` = heyvmd daemon,
`sdk-rs` = heyo-sdk source, 0.1.6 in-tree; pg-fc pins published 0.1.5).

> **HANDOFF STATE (2026-08-19).** Phase 1 is DONE, Phase 2a was just starting.
> Phase 1 lives as **uncommitted changes in the heyo repo** — commit/push them before
> switching machines or redo them from the spec below:
> `mvm-ctrl/src/{persistence,sandbox,api}.rs` modified, `mvm-ctrl/tests/by_name_lookup_test.rs` new,
> `mvm-ctrl/Cargo.lock` incidentally touched. pg-fc is clean (harness et al. committed at `def4d61`).

## Why

The load harness (`src/loadtest.rs`, committed) proved cold starts are O(fleet): a never-seen
schema pulls the ENTIRE `GET /deployed-sandboxes` inventory to find its VM by name — ~1MB/op at
fleet 5000 vs 249B at fleet 0 (`cold_start_cost_versus_fleet_size`). Modeled behind a realistic
daemon (800ms/listing, serialized when its workers are parked on blocking work), a 32-burst shows
p95 ≈ 25s — the production 30s+ at <20% CPU. Acceptance criterion:
`one_cold_start_must_not_cost_the_whole_inventory` (loadtest.rs:648, `#[ignore]`d, fails today)
must pass un-ignored.

## Locked decisions

- **Endpoint shape:** `GET /deployed-sandboxes?name=<exact>` query param. Old daemon ignores it
  (its handler took only `State`) → full list → pooler's client-side `.find()` still works.
  New daemon + old pooler: no param sent → unchanged. Zero version negotiation, no flag day.
  Ship heyvmd first, pg-fc second (but either order is safe).
- **heyvmd threading: everything now** — including the handles-map restructure (user's call).
- **SDK: decoupled** — pg-fc consumes via 0.1.5's `HeyoClient::request` + `RequestOptions.query`
  (public; existing usage precedent `src/dashboard/model.rs:487`). sdk-rs 0.1.6 gets
  `find_by_name` + `info()` fix for whenever it next publishes; nothing waits on that.
- **Duplicate-VM hazard:** heyvmd does NOT enforce name uniqueness. Pooler cache is therefore
  **positive-only** (name→id); a cache miss ALWAYS goes to the authoritative by-name daemon call
  before any create; daemon-unreachable fails the bring-up (as today), never creates.

---

## Phase 1 — heyvmd name index + `?name=` endpoint — ✅ DONE (uncommitted in heyo)

What was implemented (all tests pass: `cargo test --lib persistence::` = 7,
`cargo test --test by_name_lookup_test`, `cargo test --test compat_routes_test`):

- `mvm-ctrl/src/persistence.rs`:
  - `pub(crate) fn created_key(SystemTime) -> Duration` (since-epoch, pre-epoch→0).
  - `struct NameIndex { by_name, by_slug: HashMap<String, BTreeSet<(Duration, String)>>, by_id: HashMap<String,(name,slug,key)>, seeded }`
    — multimaps; winner = `set.last()` = newest created_at, greater-id tiebreak (identical to the
    old scan's rule); deleting the winner reveals the runner-up.
  - `PersistenceManager` gained `index: std::sync::RwLock<NameIndex>`; constructor split into
    `new()` + `fn at(data_dir)` (tests use `at`).
  - `save_sandbox_metadata` upserts AFTER the temp+rename lands (upsert removes the id's old rows
    first → rename moves, not duplicates). `delete_sandbox_directory` unindexes even if dir absent.
  - `ensure_index_seeded()`: lazy, once-per-process full scan under the write lock (double-checked);
    merge-upsert; tolerates corrupt YAML (log + continue).
  - `find_sandbox_by_slug` REWRITTEN onto the index (same signature/semantics).
  - New: `find_sandbox_by_name`, `find_sandbox_by_name_keyed` (returns `(Duration, id)` for
    cross-source merging), `unindex_sandbox(id)` (post-lookup heal; meaningful only post-seed).
  - 6 new unit tests + updated old one (helper `test_manager`, `persisted_named` uses
    `crate::models::slugify`).
- `mvm-ctrl/src/sandbox.rs`:
  - `resolve_lifecycle_id`'s disk fallback moved into `spawn_blocking` (JoinError →
    `AppError::InternalError`).
  - New `SandboxManager::find_sandbox_by_name(&self, name) -> Result<Option<SandboxInfo>, AppError>`
    (placed after `resolve_lifecycle_id`): (1) in-memory metadata scan for exact `meta.name == name`,
    keyed by `persistence::created_key(meta.created_at)`; (2) `spawn_blocking` →
    `find_sandbox_by_name_keyed`; (3) `max` across both; (4) `get_sandbox(id)`;
    `SandboxNotFound|SandboxNotFoundInFilesystem` → `unindex_sandbox` + `Ok(None)`.
- `mvm-ctrl/src/api.rs`:
  - `#[derive(Deserialize)] struct CompatListQuery { name: Option<String> }`;
    `compat_list_sandboxes` takes `Query<CompatListQuery>`; `Some(name)` → single
    `find_sandbox_by_name` → `Json(vec![...])`/`Json(vec![])`, NO reconcile, NO fleet probe, NO
    provisioning-entry merge (those carry `"name": ""`). `None` → untouched full path.
- `mvm-ctrl/tests/by_name_lookup_test.rs` (new): single test fn (env is process-wide;
  `MVM_DATA_DIR` overrides the data dir — seed ALL sandboxes before the daemon's first lookup,
  because the daemon's index seeds once and the test's seeder is a different
  `PersistenceManager` instance). Covers: duplicate names → newest id; mixed-case exact-name
  (slug can't reach it); unknown → `[]`; no-param full list regression.
  **Learning:** don't assert status of persisted-but-not-loaded sandboxes — Wasix reports
  "running" via reattach; assert on id only.
- Two `unused import` warnings in mvm-ctrl (`validate_virtual_network_name`, `VirtualNetworkMode`)
  pre-exist this work.

## Phase 2a — heyvmd spawn_blocking wraps — ⏸ IN PROGRESS (nothing written yet)

Next concrete step: add async wrappers on `PersistenceManager` (it's already
`Arc<PersistenceManager>` on `SandboxManager.persistence`), then convert sandbox.rs call sites.

Wrapper set (each: clone the Arc, `tokio::task::spawn_blocking`, JoinError → `AppError::InternalError`):
```rust
pub async fn save_sandbox_metadata_async(self: &Arc<Self>, p: &PersistedSandbox) -> Result<(), AppError>   // clones p internally; PersistedSandbox is Clone
pub async fn load_sandbox_metadata_async(self: &Arc<Self>, id: &str) -> Result<PersistedSandbox, AppError>
pub async fn list_sandbox_directories_async(self: &Arc<Self>) -> Result<Vec<String>, AppError>
pub async fn delete_sandbox_directory_async(self: &Arc<Self>, id: &str) -> Result<(), AppError>            // remove_dir_all can be huge
pub async fn find_sandbox_by_slug_async(self: &Arc<Self>, slug: &str) -> Result<Option<String>, AppError>
pub async fn find_sandbox_by_name_keyed_async(...)  // optional; SandboxManager::find_sandbox_by_name already spawn_blockings inline
```

Call-site conversion in `mvm-ctrl/src/sandbox.rs` (catalogued; mechanical `X(...)` →
`X_async(...).await`, compile errors will surface any sync-context stragglers):
save: 1258, 1557, 2773, 5208, 5260(+5258 load); load: 2094, 2097, 2753, 2878, 2965, 3876, 4256,
4261, 5191, 5250; slug: 2096, 4260, 5169; delete: 3072; reconcile scans (list+load pairs):
4673/4684, 4715/4726, 4756/4764, 4803/4819, 4874/4890, 4943/4956, 5001/5012, 5052/5060, 5092/5094.
(Keep the per-candidate filtering — do NOT bulk-load all YAMLs; most reconcilers load only ids in
their `running` set.) Leave `apple_virt*.rs` / `cli.rs` persistence callers alone (macOS driver /
separate CLI process).

Also in 2a:
- `mvm-ctrl/src/driver/firecracker.rs`: `used_firecracker_virtual_subnets` (~:590 — read_dir + a
  read PER VM, on the create/start path, itself O(fleet) sync I/O) and
  `persist_virtual_network_allocation` (~:1838, sync writes inside `start_vm` :3269 and
  `start_vm_from_snapshot` :3093) → `spawn_blocking` (precedent: debugfs wrap at :3244).
- `mvm-ctrl/src/linux_vm_image.rs:187`: GB-scale `tar -xzf` via `std::process::Command` →
  `tokio::process::Command`.

## Phase 2b — heyvmd handles-map restructure — ⬜ PENDING

`VmHandle` has `&mut self` methods (start/stop/execute — driver/driver.rs:379+), so
`Arc<dyn VmHandle>` does NOT compile. Target shape:
```rust
handles: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<Box<dyn VmHandle>>>>>>
```
- ~90 lock sites, ALL contained in `mvm-ctrl/src/sandbox.rs` (verified; the api.rs
  `local_proxy_handles` hit is a different field). Drivers untouched — constructors still return
  `Box<dyn VmHandle>`; wrap at map insert.
- Pattern per site: brief map guard → clone per-VM Arc → drop map guard → lock per-VM mutex.
- `snapshot_sandbox_infos` (~:4543): clone ALL per-VM Arcs under one short map read, drop the map
  guard BEFORE probing; per-VM probe = `tokio::time::timeout(LIST_STATUS_PROBE_TIMEOUT, lock+is_running())`,
  timeout → default `running: true` (same default as today, now per-VM instead of stalling writers
  globally). NOTE: `is_running`/`get_metrics` take `&self` today — through a per-VM mutex they'll be
  called on the guard; fine.
- Lock order everywhere: `sandbox_lifecycle_locks` (outer, :526) → map guard (brief) → per-handle
  mutex. NEVER hold the map guard across an await.
- Tests: concurrent list + start/stop no deadlock; one wedged handle's probe timeout doesn't delay
  the rest of a listing.

## Phase 3 — sdk-rs 0.1.6 — ⬜ PENDING

`sdk-rs/src/sandbox.rs`: `info()` (:162 — currently full list + client-side filter!) delegates to
`get()` (per-id GET; `wait_for_ready` inherits the fix — it polls `info()` per tick). Add
`Sandbox::find_by_name(name, client_options) -> Result<Option<SandboxInfo>, HeyoError>` =
GET `/deployed-sandboxes` with query `[("name", name)]` + client-side `.find()` (correct against
old daemons). Tests: stub that honors the filter + stub that ignores it. Changelog: `info()` now
returns persisted metadata for stopped sandboxes where it previously NotFound'd.

## Phase 4 — pg-fc inventory cache + by-name resolve — ⬜ PENDING

- New `src/inventory.rs` (pending.rs idiom — `OnceLock` + std `Mutex`, await-free sections):
  `lookup(name) -> Option<String>`, `insert(name, id)`, `remove_id(id)` (retain-based),
  `absorb(&[SandboxInfo])` (merge-upsert, never replace-all), `#[cfg(test)] reset()`.
  Positive-only name→id. Register `mod inventory;` in `src/main.rs`.
- `src/vm.rs`:
  - `find_by_name_with_retry(name) -> Result<Option<SandboxInfo>, HeyoError>` beside
    `list_with_retry` (:354): `HeyoClient::new(local_opts())` +
    `request::<Vec<SandboxInfo>>(GET, "/deployed-sandboxes", opts.query = [("name", name)])`;
    factor the transient-retry loop (4 attempts, exp backoff, `transient()`) into a helper shared
    with `list_with_retry`; then client-side `.find(|s| s.name == name)` + `inventory::absorb` of
    the whole response (old daemon's full list warms the cache free).
  - `list_with_retry`: absorb on success.
  - `resolve_sandbox` (~:2101): step 2 (the full listing at ~:2119) becomes:
    cache `lookup(name)` hit → `bring_up_existing(id)` (Ok(None) → `remove_id`, continue);
    then `find_by_name_with_retry` (authoritative; found → insert + bring_up_existing; error →
    bail as today). Steps 1 (known_id), 3 (spare take), 4 (create_vm) unchanged.
  - Write-through: `create_vm_within` (~:2363) insert after deploy; failed-bring-up kill (~:2380)
    remove; `bring_up_existing` insert on success; `stop_after_failed_bringup` (~:2223) fallback
    listing → `find_by_name_with_retry`.
- `src/registry.rs`: `kill_by_id` (~:2904) → `remove_id`; `purge_pass` (~:2069) → absorb.
- `src/spares.rs`: replenisher absorbs via list_with_retry already; insert at claim where id in hand.
- README: document cache + by-name in the cold-start section.

## Phase 5 — pg-fc loadtest — ⬜ PENDING

`src/loadtest.rs`:
- Stub `Daemon`: add `supports_name_filter: StdMutex<bool>` (default true). `list` handler (:167):
  `Query<HashMap<String,String>>`; filter-on + `name` present → 0/1-entry array, metric label
  `"GET /deployed-sandboxes?name"`; filter-off ignores the param (real old daemon).
- `config_for` (:336): `inventory::reset()` + reset the flag. (Cells already serialize via
  `exclusive()`.)
- Tests: un-`#[ignore]` `one_cold_start_must_not_cost_the_whole_inventory` (:648);
  `by_name_lookup_finds_without_pulling_inventory` (zero full-list hits, one ?name hit, zero
  deploys); `existing_vm_absent_from_cache_is_reattached_not_recreated` (duplicate-VM guard: daemon
  has pg-X, cache reset, no registry row → resolve → no deploy);
  `old_daemon_full_list_fallback_still_resolves` (+ second resolve hits cache → no second list).
- Re-run sweeps, record before/after (before, committed at def4d61: new-schema p50 61ms/1.0MB/op
  at fleet 5000 vs known-schema 1ms/213B flat; serialized-daemon model p95 25s at conc 32).

## Verification (end state)

1. mvm-ctrl: `cargo test` (+ the two integration tests above); manual
   `curl 'localhost:34099/deployed-sandboxes?name=pg-foo'` fast on a big fleet.
2. sdk-rs: `cargo test`.
3. pg-fc: `cargo test` (acceptance test now in the normal suite);
   `cargo test --release loadtest -- --ignored --nocapture --test-threads=1` — new-schema bytes/op
   flat across fleet sizes.
4. e2e vs real daemon: `cargo run --release --example e2e`; on the deployed host a new schema logs
   `creating VM pg-X` within ~1s of `client requested schema X` regardless of fleet size.

## Standing risks

- Index-vs-disk drift: nothing in-process writes sandbox.yaml outside PersistenceManager
  (verified); a stale miss costs one authoritative round-trip, never a blind create.
- 2b is independently shippable; if it stalls, 1/2a/4/5 stand alone.
- Name uniqueness on create remains unenforced daemon-side (deliberate non-goal).
