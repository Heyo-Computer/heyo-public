//! The shared registry: the one piece of state the proxy, autoscaler, and admin
//! API all touch.
//!
//! Pingora fixes its service set at startup (`Server::run_forever(self)`
//! consumes the server), so "dynamic deployment registration" cannot mean adding
//! services at runtime. Instead every deployment lives in this registry behind
//! `ArcSwap`, and a single proxy service routes across whatever is currently in
//! it. Readers are lock-free; writers copy-on-write.

use crate::config::DeploymentSpec;
use crate::deployment::{Deployment, DeploymentState};
use arc_swap::ArcSwap;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Route rules pre-sorted most-specific-first, so the first match wins.
///
/// Split in two rather than held as one sorted list, because a fleet of agent
/// sandboxes is thousands of deployments each contributing one exact-host rule,
/// and a linear scan of that per request is the whole routing cost. The split is
/// sound because [`RouteRule::specificity`] tiers do not overlap: any rule with
/// a `host` scores at least `1_000_000`, and any rule without one scores at most
/// `100_000` plus the length of a hostname and a path. So if *any* exact-host
/// rule matches, it outranks everything in `rest` outright, and the two can be
/// consulted in order instead of merged.
///
/// [`RouteRule::specificity`]: crate::config::RouteRule::specificity
#[derive(Debug, Default)]
pub struct RouteTable {
    /// Rules that name an exact `host`, bucketed by that host (lowercased) so
    /// the common case is one hash lookup. Each bucket is sorted the same way
    /// the flat list was, so a `host`+`/api` rule still beats a bare `host` one.
    ///
    /// Buckets are `Arc`d so a copy-on-write update can share every bucket it
    /// does not touch. Registering one deployment then costs a table allocation
    /// and a refcount bump per bucket, instead of deep-copying and re-sorting
    /// every rule in the fleet.
    exact: HashMap<Arc<str>, Arc<Vec<RouteEntry>>>,
    /// Everything else — `host_suffix` and path-only rules — still scanned
    /// linearly. It stays short: these are the platform-level catch-alls, not
    /// the per-deployment routes. `Arc`d for the same reason, so a deployment
    /// with only exact hosts (which is every sandbox) never copies it at all.
    rest: Arc<Vec<RouteEntry>>,
}

/// One rule and the id of the deployment that declared it. The id is shared
/// rather than copied per rule — a fleet has one per deployment, and they are
/// cloned on every index update that touches the bucket.
type RouteEntry = (crate::config::RouteRule, Arc<str>);

/// Most specific first: a host+path rule must beat a bare path rule, and
/// `/api/v2` must beat `/api`. Ties broken by id for determinism.
fn by_specificity(
    (a, ai): &RouteEntry,
    (b, bi): &RouteEntry,
) -> std::cmp::Ordering {
    b.specificity()
        .cmp(&a.specificity())
        .then_with(|| ai.cmp(bi))
}

/// The bucket key for a rule's host: lowercased, since matching is
/// case-insensitive.
fn host_key(rule: &crate::config::RouteRule) -> Option<String> {
    rule.host.as_ref().map(|h| h.to_ascii_lowercase())
}

impl RouteTable {
    /// Build the whole index from scratch.
    ///
    /// Nothing in the running LB calls this any more — every write goes through
    /// [`updated`](Self::updated). It is kept as the reference implementation
    /// the incremental path is checked against: an index that drifts from a full
    /// rebuild is the failure mode of incremental updating, and it would show up
    /// as traffic routed to the wrong deployment rather than as a crash.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn build(deployments: &HashMap<Arc<str>, Arc<Deployment>>) -> Self {
        let mut exact: HashMap<Arc<str>, Vec<RouteEntry>> = HashMap::new();
        let mut rest = Vec::new();
        for d in deployments.values() {
            let id: Arc<str> = Arc::from(d.spec.id.as_str());
            for rule in &d.spec.routes {
                let entry = (rule.clone(), id.clone());
                match host_key(rule) {
                    Some(h) => exact.entry(Arc::from(h.as_str())).or_default().push(entry),
                    None => rest.push(entry),
                }
            }
        }
        for bucket in exact.values_mut() {
            bucket.sort_by(by_specificity);
        }
        rest.sort_by(by_specificity);
        Self {
            exact: exact.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(),
            rest: Arc::new(rest),
        }
    }

    /// This table with one deployment's rules replaced.
    ///
    /// `previous` is what it declared before (`None` when registering),
    /// `next` what it declares now (`None` when deregistering). Only the buckets
    /// named by either are rebuilt; every other bucket is shared with `self`.
    ///
    /// This is what keeps registering the thousandth sandbox as cheap as the
    /// first. Rebuilding the whole index per write made a create storm quadratic
    /// — measurably so: the marginal cost of a create tracked the size of the
    /// fleet, which is exactly the thing a fleet cannot afford.
    fn updated(
        &self,
        id: &str,
        previous: Option<&[crate::config::RouteRule]>,
        next: Option<&[crate::config::RouteRule]>,
    ) -> Self {
        // The union of the hosts this deployment used to name and the ones it
        // names now. Anything outside it cannot have changed.
        let mut touched: HashSet<String> = HashSet::new();
        let mut touches_rest = false;
        for rules in [previous, next].into_iter().flatten() {
            for rule in rules {
                match host_key(rule) {
                    Some(h) => {
                        touched.insert(h);
                    }
                    None => touches_rest = true,
                }
            }
        }

        if touched.is_empty() && !touches_rest {
            // An unrouted deployment — the sandbox default — contributes no
            // rules either way, so the index is unchanged.
            return Self {
                exact: self.exact.clone(),
                rest: self.rest.clone(),
            };
        }

        let id: Arc<str> = Arc::from(id);
        let mut exact = self.exact.clone();
        for host in &touched {
            // Drop this deployment's old entries for the bucket, keeping every
            // other deployment's; then add whatever it declares now.
            let mut bucket: Vec<RouteEntry> = exact
                .get(host.as_str())
                .map(|b| b.iter().filter(|(_, i)| **i != *id).cloned().collect())
                .unwrap_or_default();
            if let Some(rules) = next {
                for rule in rules {
                    if host_key(rule).as_deref() == Some(host.as_str()) {
                        bucket.push((rule.clone(), id.clone()));
                    }
                }
            }
            // An empty bucket is removed rather than kept: `resolve` treats a
            // present-but-empty bucket as a miss anyway, and leaving them would
            // grow the map by one entry per hostname ever used.
            if bucket.is_empty() {
                exact.remove(host.as_str());
            } else {
                bucket.sort_by(by_specificity);
                exact.insert(Arc::from(host.as_str()), Arc::new(bucket));
            }
        }

        let rest = if touches_rest {
            let mut v: Vec<RouteEntry> = self
                .rest
                .iter()
                .filter(|(_, i)| **i != *id)
                .cloned()
                .collect();
            if let Some(rules) = next {
                for rule in rules.iter().filter(|r| r.host.is_none()) {
                    v.push((rule.clone(), id.clone()));
                }
            }
            v.sort_by(by_specificity);
            Arc::new(v)
        } else {
            self.rest.clone()
        };

        Self { exact, rest }
    }

    pub fn resolve(&self, host: Option<&str>, path: &str) -> Option<&str> {
        if let Some(host) = host {
            // `matches` compares hostnames case-insensitively, so the key has to
            // be folded too. Borrowed unless the request actually carries upper
            // case — the proxy already lowercases (`proxy::request_host`), so in
            // practice this is a scan and no allocation.
            let key = if host.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Owned(host.to_ascii_lowercase())
            } else {
                Cow::Borrowed(host)
            };
            if let Some(bucket) = self.exact.get(key.as_ref())
                && let Some((_, id)) = bucket.iter().find(|(rule, _)| rule.matches(Some(host), path))
            {
                return Some(id);
            }
        }
        self.rest
            .iter()
            .find(|(rule, _)| rule.matches(host, path))
            .map(|(_, id)| &**id)
    }
}

#[derive(Debug)]
pub struct Registry {
    deployments: ArcSwap<HashMap<Arc<str>, Arc<Deployment>>>,
    routes: ArcSwap<RouteTable>,
    persist_path: PathBuf,
    /// Whether the last [`load`](Registry::load) left a file on disk that it
    /// could not turn into a deployment. Gates
    /// [`sweep_orphan_state`](Registry::sweep_orphan_state), which must not
    /// delete what it cannot account for.
    load_skipped: std::sync::atomic::AtomicBool,
}

impl Registry {
    pub fn new(persist_path: impl Into<PathBuf>) -> Self {
        Self {
            deployments: ArcSwap::from_pointee(HashMap::new()),
            routes: ArcSwap::from_pointee(RouteTable::default()),
            persist_path: persist_path.into(),
            load_skipped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn deployments(&self) -> Arc<HashMap<Arc<str>, Arc<Deployment>>> {
        self.deployments.load_full()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Deployment>> {
        self.deployments().get(id).cloned()
    }

    /// Resolve a request to a deployment.
    pub fn route(&self, host: Option<&str>, path: &str) -> Option<Arc<Deployment>> {
        let id = {
            let routes = self.routes.load();
            routes.resolve(host, path)?.to_string()
        };
        self.get(&id)
    }

    /// Register or replace a deployment.
    ///
    /// Replacing builds a fresh `Deployment`, so the old pool's VMs are dropped
    /// from routing immediately; the autoscaler reaps the orphaned sandboxes on
    /// its next tick by diffing against the daemon's list.
    pub fn upsert(&self, spec: DeploymentSpec) -> Arc<Deployment> {
        let deployment = Arc::new(Deployment::new(spec));
        self.install(Some(deployment.clone()), &deployment.spec.id.clone());
        deployment
    }

    /// Update a deployment's spec in place, **preserving its live VM pool**.
    ///
    /// Unlike `upsert` (which abandons the old pool for the autoscaler to reap),
    /// this carries the existing backends and pending VMs onto a fresh
    /// `Deployment` built from the new spec. The pool is a vec of shared
    /// `Arc<VmBackend>`, so the moved VMs keep their in-flight counters and
    /// drain flags, and requests in flight during the edit are unaffected.
    /// Returns `None` if the id is unknown.
    ///
    /// Only valid when the VM *template* is unchanged; a template change means
    /// the running VMs were built from a different spec and must be rebuilt,
    /// which is `upsert` plus a teardown of the old pool. The admin layer makes
    /// that decision.
    pub fn update(&self, spec: DeploymentSpec) -> Option<Arc<Deployment>> {
        let old = self.get(&spec.id)?;
        let new = Arc::new(Deployment::new(spec));
        new.set_backends((*old.backends()).clone());
        new.set_pending((*old.pending()).clone());
        // Runtime state is carried for the same reason the pool is: an edit is
        // not a reset. Dropping it here would strand every sandbox this
        // deployment had suspended — they are absent from the daemon's fleet
        // list, so nothing else remembers them.
        new.set_state((*old.state()).clone());
        self.install(Some(new.clone()), &new.spec.id.clone());
        Some(new)
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Deployment>> {
        let removed = self.get(id)?;
        self.install(None, id);
        Some(removed)
    }

    /// Install or remove one deployment, updating the route index with it.
    ///
    /// Copy-on-write, so readers stay lock-free. The cost is one map allocation
    /// and a refcount bump per entry — *not* a deep copy of every deployment's
    /// routes and a re-sort of the whole index, which is what made the marginal
    /// cost of a create grow with the size of the fleet.
    ///
    /// Not atomic against a concurrent writer, which is fine: the admin API is
    /// the only writer of the deployment set, and it is a single service. The
    /// two `store`s are ordered deployments-then-routes so a request that
    /// resolves an id always finds it — the reverse order has a window where the
    /// index names a deployment the map does not yet hold.
    fn install(&self, deployment: Option<Arc<Deployment>>, id: &str) {
        let current = self.deployments.load();
        let previous = current.get(id).cloned();

        let mut next = (**current).clone();
        match &deployment {
            Some(d) => next.insert(Arc::from(id), d.clone()),
            None => next.remove(id),
        };

        let routes = self.routes.load().updated(
            id,
            previous.as_ref().map(|d| d.spec.routes.as_slice()),
            deployment.as_ref().map(|d| d.spec.routes.as_slice()),
        );

        self.deployments.store(Arc::new(next));
        self.routes.store(Arc::new(routes));
    }

    /// Where per-deployment state files live: `app-lb-state.json` →
    /// `app-lb-state.d/`. Derived rather than separately configured so there is
    /// still one knob to point at a data directory.
    pub fn state_dir(&self) -> PathBuf {
        let stem = self
            .persist_path
            .file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("app-lb-state"));
        let mut name = stem.to_os_string();
        name.push(".d");
        match self.persist_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join(name),
            _ => PathBuf::from(name),
        }
    }

    /// Write one deployment's file. O(1) in fleet size, which is the point: a
    /// fleet of agent sandboxes registers and deregisters constantly, and
    /// rewriting every spec on each change made a create storm quadratic.
    pub fn persist_one(&self, id: &str) -> std::io::Result<()> {
        let Some(d) = self.get(id) else {
            return self.forget(id);
        };
        let dir = self.state_dir();
        std::fs::create_dir_all(&dir)?;
        let record = StoredDeployment {
            spec: d.spec.clone(),
            state: (*d.state()).clone(),
        };
        let json = serde_json::to_vec_pretty(&record)?;
        // Write-then-rename so a crash mid-write can't truncate existing state.
        let path = dir.join(state_file_name(id));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    /// Drop one deployment's file. A missing file is success — deregistering
    /// something that was never persisted is not an error.
    pub fn forget(&self, id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.state_dir().join(state_file_name(id))) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Delete state files that no live deployment claims, and report how many.
    ///
    /// The leak this closes is narrow: a deregistration whose [`forget`] failed
    /// leaves a file behind, and the next start reads it and resurrects the
    /// deployment. Sweeping at startup catches that.
    ///
    /// **Refuses to run if the preceding [`load`] skipped anything.** A file is
    /// skipped when its spec no longer validates — which a validation change can
    /// cause for a spec that was fine when written — and that file is the
    /// operator's only copy. Deleting it would turn a warning into data loss, so
    /// the sweep stands down and says so instead.
    ///
    /// [`forget`]: Self::forget
    /// [`load`]: Self::load
    pub fn sweep_orphan_state(&self) -> std::io::Result<usize> {
        if self.load_skipped.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "not sweeping orphaned state files: the last load skipped at least one, \
                 and an unreadable file is still somebody's only copy of a spec",
            );
            return Ok(0);
        }
        let dir = self.state_dir();
        let wanted: std::collections::HashSet<String> = self
            .deployments()
            .keys()
            .map(|id| state_file_name(id))
            .collect();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".json") && !wanted.contains(name) {
                tracing::info!(file = %name, "removing orphaned deployment state file");
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Load persisted deployments. Nothing on disk is a normal first run.
    ///
    /// Reads the state directory, importing the legacy single file first if one
    /// is present. The filename is only a storage key — the id comes from the
    /// spec inside — so a file whose name was mangled by encoding still loads
    /// under the right id.
    pub fn load(&self) -> std::io::Result<usize> {
        self.migrate_legacy_file()?;

        let dir = self.state_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };

        let mut loaded = 0;
        let mut skipped = false;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue; // a stray `.json.tmp` from an interrupted write
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "unreadable state file");
                    skipped = true;
                    continue;
                }
            };
            let record: StoredDeployment = match serde_json::from_slice(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "unparseable state file");
                    skipped = true;
                    continue;
                }
            };
            // Persisted state predates any validation change; skip bad entries
            // rather than refusing to start.
            if let Err(e) = record.spec.validate() {
                tracing::warn!(id = %record.spec.id, error = %e, "skipping invalid persisted deployment");
                skipped = true;
                continue;
            }
            let d = self.upsert(record.spec);
            d.set_state(record.state);
            loaded += 1;
        }
        self.load_skipped
            .store(skipped, std::sync::atomic::Ordering::Relaxed);
        Ok(loaded)
    }

    /// Import a pre-directory `app-lb-state.json`, then move it aside.
    ///
    /// Renamed rather than deleted so a downgrade still has the data, and so a
    /// second start cannot re-import specs that have since been edited or
    /// deregistered — which would resurrect deleted deployments.
    fn migrate_legacy_file(&self) -> std::io::Result<()> {
        let bytes = match std::fs::read(&self.persist_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let specs: Vec<DeploymentSpec> = serde_json::from_slice(&bytes)?;
        let dir = self.state_dir();
        std::fs::create_dir_all(&dir)?;
        for spec in &specs {
            let record = StoredDeployment {
                spec: spec.clone(),
                state: DeploymentState::default(),
            };
            let path = dir.join(state_file_name(&spec.id));
            // Never clobber: if the directory already has an entry it is newer
            // than this file, which is only here because an earlier rename
            // failed.
            if path.exists() {
                continue;
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&record)?)?;
        }
        let aside = self.persist_path.with_extension("json.migrated");
        std::fs::rename(&self.persist_path, &aside)?;
        tracing::info!(
            count = specs.len(),
            from = %self.persist_path.display(),
            to = %dir.display(),
            "migrated deployment state to per-deployment files",
        );
        Ok(())
    }
}

/// One deployment's on-disk record: its spec, plus the runtime state that has
/// to outlive a restart but is not something the operator writes.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct StoredDeployment {
    spec: DeploymentSpec,
    #[serde(default)]
    state: DeploymentState,
}

/// A filename for a deployment id.
///
/// Ids are only required to be non-empty, so one may contain `/`, `..`, or a
/// leading dot — all of which would escape or hide the state directory.
/// Everything outside `[A-Za-z0-9_-]` is percent-escaped, including `.` and `%`
/// itself, which keeps the encoding injective: two different ids can never
/// collide on one file. It is deliberately not reversible — the id is read back
/// from the spec inside the file, so the name is a key and nothing more.
fn state_file_name(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + 5);
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out.push_str(".json");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HealthCheck, RouteRule, ScalingPolicy, VmSpec};
    use heyo_sdk::SandboxDriver;
    use std::path::Path;

    fn spec(id: &str, routes: Vec<RouteRule>) -> DeploymentSpec {
        DeploymentSpec {
            id: id.into(),
            routes,
            vm: Some(VmSpec {
                driver: SandboxDriver::Firecracker,
                image: None,
                port: 8080,
                start_command: None,
                size_class: None,
                disk_size_gb: None,
                working_directory: None,
                env_vars: None,
                setup_hooks: None,
                open_ports: vec![],
                ttl_seconds: 3600,
            }),
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
            upstreams: vec![],
            build: None,
            artifact: None,
            update: None,
            auth: None,
        }
    }

    fn static_spec(id: &str, routes: Vec<RouteRule>, upstreams: &[&str]) -> DeploymentSpec {
        DeploymentSpec {
            id: id.into(),
            routes,
            vm: None,
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
            upstreams: upstreams.iter().map(|s| s.to_string()).collect(),
            build: None,
            artifact: None,
            update: None,
            auth: None,
        }
    }

    fn host(h: &str) -> RouteRule {
        RouteRule {
            host: Some(h.into()),
            host_suffix: None,
            path_prefix: None,
        }
    }

    fn path(p: &str) -> RouteRule {
        RouteRule {
            host: None,
            host_suffix: None,
            path_prefix: Some(p.into()),
        }
    }

    fn suffix(s: &str) -> RouteRule {
        RouteRule {
            host: None,
            host_suffix: Some(s.into()),
            path_prefix: None,
        }
    }

    #[test]
    fn routes_by_subdomain() {
        let r = Registry::new("unused.json");
        r.upsert(spec("apps", vec![suffix("apps.example.com")]));
        assert_eq!(
            r.route(Some("foo.apps.example.com"), "/").unwrap().spec.id,
            "apps"
        );
        assert_eq!(
            r.route(Some("apps.example.com"), "/").unwrap().spec.id,
            "apps"
        );
        assert!(r.route(Some("foo.other.com"), "/").is_none());
    }

    #[test]
    fn exact_host_beats_subdomain_wildcard() {
        let r = Registry::new("unused.json");
        r.upsert(spec("wild", vec![suffix("example.com")]));
        r.upsert(spec("exact", vec![host("special.example.com")]));

        // A generic subdomain falls to the wildcard...
        assert_eq!(
            r.route(Some("other.example.com"), "/").unwrap().spec.id,
            "wild"
        );
        // ...but the exact host wins for its own name.
        assert_eq!(
            r.route(Some("special.example.com"), "/").unwrap().spec.id,
            "exact"
        );
    }

    #[test]
    fn routes_by_host() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a", vec![host("a.local")]));
        r.upsert(spec("b", vec![host("b.local")]));

        assert_eq!(r.route(Some("a.local"), "/").unwrap().spec.id, "a");
        assert_eq!(r.route(Some("b.local"), "/").unwrap().spec.id, "b");
        assert!(r.route(Some("nope.local"), "/").is_none());
        assert!(r.route(None, "/").is_none());
    }

    #[test]
    fn routes_by_path_prefix() {
        let r = Registry::new("unused.json");
        r.upsert(spec("api", vec![path("/api")]));
        assert_eq!(r.route(None, "/api/v1").unwrap().spec.id, "api");
        assert!(r.route(None, "/web").is_none());
    }

    #[test]
    fn most_specific_route_wins() {
        let r = Registry::new("unused.json");
        r.upsert(spec("broad", vec![path("/api")]));
        r.upsert(spec("narrow", vec![path("/api/v2")]));
        r.upsert(spec("hosted", vec![host("a.local")]));

        assert_eq!(r.route(None, "/api/v1").unwrap().spec.id, "broad");
        assert_eq!(r.route(None, "/api/v2/x").unwrap().spec.id, "narrow");
        // Host beats a bare path prefix.
        assert_eq!(
            r.route(Some("a.local"), "/api/v1").unwrap().spec.id,
            "hosted"
        );
    }

    #[test]
    fn host_plus_path_beats_host_alone() {
        let r = Registry::new("unused.json");
        r.upsert(spec("site", vec![host("a.local")]));
        r.upsert(spec(
            "site-api",
            vec![RouteRule {
                host: Some("a.local".into()),
                host_suffix: None,
                path_prefix: Some("/api".into()),
            }],
        ));
        assert_eq!(r.route(Some("a.local"), "/").unwrap().spec.id, "site");
        assert_eq!(
            r.route(Some("a.local"), "/api/x").unwrap().spec.id,
            "site-api"
        );
    }

    /// The exact-host bucket is keyed by a lowercased hostname, so a request
    /// carrying upper case has to be folded before the lookup or it misses the
    /// bucket entirely — a miss the flat scan could never have.
    #[test]
    fn exact_host_lookup_is_case_insensitive_in_both_directions() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a", vec![host("MiXeD.Local")]));
        assert_eq!(r.route(Some("mixed.local"), "/").unwrap().spec.id, "a");
        assert_eq!(r.route(Some("MIXED.LOCAL"), "/").unwrap().spec.id, "a");
    }

    /// A populated host bucket whose rules all fail must fall through to the
    /// residual list. Returning `None` at the first bucket miss would be the
    /// natural bug in a split index, and it would silently 404 traffic that a
    /// wildcard rule is there to catch.
    #[test]
    fn a_host_bucket_that_does_not_match_falls_through_to_the_wildcard() {
        let r = Registry::new("unused.json");
        r.upsert(spec("wild", vec![suffix("local")]));
        r.upsert(spec(
            "api",
            vec![RouteRule {
                host: Some("a.local".into()),
                host_suffix: None,
                path_prefix: Some("/api".into()),
            }],
        ));

        // The bucket for `a.local` exists and is consulted first...
        assert_eq!(r.route(Some("a.local"), "/api/x").unwrap().spec.id, "api");
        // ...but a path it doesn't cover still reaches the suffix rule.
        assert_eq!(r.route(Some("a.local"), "/other").unwrap().spec.id, "wild");
    }

    /// A rule may set `host` *and* `host_suffix`. It buckets by the exact host,
    /// and both conditions must still hold.
    #[test]
    fn a_rule_with_both_host_and_suffix_is_bucketed_but_still_fully_matched() {
        let r = Registry::new("unused.json");
        r.upsert(spec(
            "both",
            vec![RouteRule {
                host: Some("a.example.com".into()),
                host_suffix: Some("example.com".into()),
                path_prefix: None,
            }],
        ));
        r.upsert(spec(
            "mismatch",
            vec![RouteRule {
                host: Some("a.other.com".into()),
                host_suffix: Some("example.com".into()),
                path_prefix: None,
            }],
        ));

        assert_eq!(r.route(Some("a.example.com"), "/").unwrap().spec.id, "both");
        // The suffix contradicts the host, so nothing can ever satisfy it.
        assert!(r.route(Some("a.other.com"), "/").is_none());
    }

    /// The incremental index must agree with a full rebuild after *every*
    /// mutation, not just at the end.
    ///
    /// This is the test that matters for `RouteTable::updated`. Its failure mode
    /// is not a crash — it is a stale or missing bucket, which routes somebody's
    /// traffic to the wrong deployment or 404s a hostname that is registered.
    /// Comparing against `build` after each step is the only way to see that.
    #[test]
    fn the_incremental_index_never_drifts_from_a_full_rebuild() {
        // Route shapes chosen to cover every branch of `updated`: exact host,
        // host+path (two rules sharing one bucket), a suffix and a bare path
        // (both land in `rest`), several rules at once, and none at all.
        let shapes: Vec<Vec<RouteRule>> = vec![
            vec![host("a.local")],
            vec![host("b.local")],
            vec![RouteRule {
                host: Some("a.local".into()),
                host_suffix: None,
                path_prefix: Some("/api".into()),
            }],
            vec![suffix("apps.example.com")],
            vec![path("/legacy")],
            vec![host("a.local"), suffix("example.com"), path("/x")],
            vec![host("A.LOCAL")], // same bucket, different case
            vec![],                // unrouted
        ];
        let probes: Vec<(Option<&str>, &str)> = vec![
            (Some("a.local"), "/"),
            (Some("a.local"), "/api/v1"),
            (Some("A.Local"), "/api"),
            (Some("b.local"), "/"),
            (Some("x.apps.example.com"), "/"),
            (Some("apps.example.com"), "/x"),
            (Some("other.example.com"), "/"),
            (None, "/legacy/thing"),
            (None, "/x"),
            (None, "/"),
        ];

        let r = Registry::new("unused.json");
        // A fixed LCG rather than a random source: a failure has to be
        // reproducible, and this is a correctness check, not a fuzz run.
        let mut seed: u64 = 0x5eed;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };

        for step in 0..400 {
            let id = format!("d{}", next() % 12);
            match next() % 4 {
                0 => {
                    r.remove(&id);
                }
                _ => {
                    let routes = shapes[next() % shapes.len()].clone();
                    // Exercise both write paths: `update` preserves the pool,
                    // `upsert` replaces the deployment outright.
                    if next() % 2 == 0 && r.get(&id).is_some() {
                        r.update(spec(&id, routes));
                    } else {
                        r.upsert(spec(&id, routes));
                    }
                }
            }

            let reference = RouteTable::build(&r.deployments());
            let live = r.routes.load();
            for (h, p) in &probes {
                assert_eq!(
                    live.resolve(*h, p),
                    reference.resolve(*h, p),
                    "step {step}: incremental index disagrees for host={h:?} path={p}",
                );
            }
        }
    }

    /// An unrouted deployment — the agent-sandbox shape — contributes nothing to
    /// either half of the index and is reachable only by id.
    #[test]
    fn an_unrouted_deployment_is_registered_but_never_routed() {
        let r = Registry::new("unused.json");
        r.upsert(spec("sandbox", vec![]));
        r.upsert(spec("web", vec![host("web.local")]));

        assert!(r.get("sandbox").is_some());
        assert!(r.route(Some("sandbox"), "/").is_none());
        assert!(r.route(None, "/").is_none());
        assert_eq!(r.route(Some("web.local"), "/").unwrap().spec.id, "web");
    }

    #[test]
    fn update_preserves_the_live_pool_and_swaps_routes() {
        use crate::deployment::VmBackend;
        use std::sync::Arc;

        let r = Registry::new("unused.json");
        r.upsert(spec("a", vec![host("old.local")]));
        let before = r.get("a").unwrap();
        let backend = Arc::new(VmBackend::new("sb-1".into(), "10.0.0.1:80".parse().unwrap()));
        backend.acquire(); // 1 in-flight, to prove the counter survives the edit
        before.set_backends(vec![backend.clone()]);

        // Edit the routes (not the VM template).
        let updated = r.update(spec("a", vec![host("new.local")])).unwrap();

        // The same backend Arc moved across, in-flight intact.
        let pool = updated.backends();
        assert_eq!(pool.len(), 1);
        assert!(Arc::ptr_eq(&pool[0], &backend), "the running VM must be carried over");
        assert_eq!(pool[0].in_flight(), 1, "in-flight counter preserved across the edit");

        // Routing follows the new spec.
        assert!(r.route(Some("old.local"), "/").is_none());
        assert_eq!(r.route(Some("new.local"), "/").unwrap().spec.id, "a");
    }

    #[test]
    fn update_of_unknown_deployment_is_none() {
        let r = Registry::new("unused.json");
        assert!(r.update(spec("ghost", vec![host("x.local")])).is_none());
    }

    #[test]
    fn upsert_replaces_and_reroutes() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a", vec![host("old.local")]));
        r.upsert(spec("a", vec![host("new.local")]));
        assert!(r.route(Some("old.local"), "/").is_none());
        assert_eq!(r.route(Some("new.local"), "/").unwrap().spec.id, "a");
        assert_eq!(r.deployments().len(), 1);
    }

    #[test]
    fn remove_drops_routes() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a", vec![host("a.local")]));
        assert!(r.remove("a").is_some());
        assert!(r.route(Some("a.local"), "/").is_none());
        assert!(r.remove("a").is_none());
    }

    #[test]
    fn persist_round_trips() {
        let dir = std::env::temp_dir().join(format!("app-lb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("state.json");

        let r = Registry::new(&state_file);
        r.upsert(spec("a", vec![host("a.local")]));
        r.upsert(spec("b", vec![path("/b")]));
        persist_all(&r);

        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 2);
        assert_eq!(r2.route(Some("a.local"), "/").unwrap().spec.id, "a");
        assert_eq!(r2.route(None, "/b/x").unwrap().spec.id, "b");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_a_clean_first_run() {
        let r = Registry::new("/nonexistent/definitely/not/here.json");
        assert_eq!(r.load().unwrap(), 0);
    }

    /// A scratch state file under a name unique to this test, so the cases below
    /// can run in parallel without sharing a directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("app-lb-test-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.json")
    }

    /// Write every deployment's file. Production writes one at a time
    /// (`persist_one`); this is only for setting up a populated directory.
    fn persist_all(r: &Registry) {
        for id in r.deployments().keys() {
            r.persist_one(id).unwrap();
        }
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The whole point of the directory layout: registering one deployment
    /// writes one file, not the whole fleet. A create storm was quadratic before.
    #[test]
    fn persist_one_touches_exactly_one_file() {
        let state_file = scratch("persist-one");
        let r = Registry::new(&state_file);
        r.upsert(spec("a", vec![host("a.local")]));
        r.upsert(spec("b", vec![host("b.local")]));

        r.persist_one("a").unwrap();
        assert_eq!(files_in(&r.state_dir()), vec!["a.json"]);

        r.persist_one("b").unwrap();
        assert_eq!(files_in(&r.state_dir()), vec!["a.json", "b.json"]);

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    /// Runtime state rides along with the spec. Losing it across a restart would
    /// strand suspended sandboxes, which are invisible to the daemon's fleet
    /// list and so have no other record anywhere.
    #[test]
    fn runtime_state_round_trips_with_the_spec() {
        let state_file = scratch("runtime-state");
        let r = Registry::new(&state_file);
        let d = r.upsert(spec("a", vec![host("a.local")]));
        d.set_state(DeploymentState {
            suspended: vec!["sb-1".into()],
        });
        r.persist_one("a").unwrap();

        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 1);
        assert_eq!(r2.get("a").unwrap().state().suspended, vec!["sb-1"]);

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    /// An edit is not a reset: `update` preserves the pool, and must preserve
    /// the runtime state for the same reason.
    #[test]
    fn update_carries_runtime_state_across_the_edit() {
        let r = Registry::new("unused.json");
        let d = r.upsert(spec("a", vec![host("old.local")]));
        d.set_state(DeploymentState {
            suspended: vec!["sb-1".into()],
        });

        let updated = r.update(spec("a", vec![host("new.local")])).unwrap();
        assert_eq!(updated.state().suspended, vec!["sb-1"]);
    }

    #[test]
    fn forget_drops_one_file_and_tolerates_a_missing_one() {
        let state_file = scratch("forget");
        let r = Registry::new(&state_file);
        r.upsert(spec("a", vec![host("a.local")]));
        r.upsert(spec("b", vec![host("b.local")]));
        persist_all(&r);

        r.forget("a").unwrap();
        assert_eq!(files_in(&r.state_dir()), vec!["b.json"]);
        // Deregistering something never persisted is not an error.
        r.forget("never-existed").unwrap();

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    /// A file left behind by a deregistration whose `forget` failed must not
    /// resurrect the deployment on the next start.
    #[test]
    fn the_sweep_removes_files_no_deployment_claims() {
        let state_file = scratch("sweep");
        let r = Registry::new(&state_file);
        r.upsert(spec("a", vec![host("a.local")]));
        r.upsert(spec("b", vec![host("b.local")]));
        persist_all(&r);

        // Deregister without forgetting — the failed-unlink case.
        r.remove("b");
        assert_eq!(r.sweep_orphan_state().unwrap(), 1);
        assert_eq!(files_in(&r.state_dir()), vec!["a.json"]);

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    /// The sweep must stand down when the load it follows could not account for
    /// every file. A spec that stopped validating is still the operator's only
    /// copy, and deleting it would turn a warning into data loss.
    #[test]
    fn the_sweep_refuses_to_run_after_a_load_that_skipped_a_file() {
        let state_file = scratch("sweep-guard");
        let r = Registry::new(&state_file);
        r.upsert(spec("good", vec![host("a.local")]));
        persist_all(&r);
        // A spec app-lb cannot parse — the shape of a forward/backward
        // incompatibility, not of a file that may be thrown away.
        std::fs::write(r.state_dir().join("mystery.json"), b"{\"spec\":{\"nope\":1}}").unwrap();

        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 1, "the good one still loads");
        assert_eq!(r2.sweep_orphan_state().unwrap(), 0, "and nothing is deleted");
        assert!(r2.state_dir().join("mystery.json").is_file());

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    /// An id is only required to be non-empty, so it can contain `/` or `..`.
    /// Those must neither escape the state directory nor collide with each
    /// other — the encoding is escaped, not stripped.
    #[test]
    fn ids_that_are_not_filename_safe_stay_inside_and_stay_distinct() {
        let state_file = scratch("unsafe-ids");
        let r = Registry::new(&state_file);
        r.upsert(spec("../escape", vec![host("a.local")]));
        r.upsert(spec("..%2Fescape", vec![host("b.local")]));
        r.upsert(spec("a/b", vec![host("c.local")]));
        persist_all(&r);

        let dir = r.state_dir();
        // Three distinct files, all of them directly inside the state directory.
        assert_eq!(files_in(&dir).len(), 3, "no two ids may share a file");
        assert!(dir.join(state_file_name("../escape")).is_file());

        // And they come back under their original ids.
        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 3);
        assert!(r2.get("../escape").is_some());
        assert!(r2.get("..%2Fescape").is_some());
        assert!(r2.get("a/b").is_some());

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    /// Upgrading from the single-file format imports it and moves it aside, so a
    /// second start cannot resurrect deployments deregistered in between.
    #[test]
    fn a_legacy_state_file_is_imported_once_and_moved_aside() {
        let state_file = scratch("legacy");
        let legacy = vec![spec("a", vec![host("a.local")]), spec("b", vec![path("/b")])];
        std::fs::write(&state_file, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let r = Registry::new(&state_file);
        assert_eq!(r.load().unwrap(), 2);
        assert_eq!(r.route(Some("a.local"), "/").unwrap().spec.id, "a");
        assert!(!state_file.exists(), "the legacy file must be moved aside");
        assert!(state_file.with_extension("json.migrated").is_file());

        // Deregister one, restart: the import must not bring it back.
        r.remove("b");
        r.forget("b").unwrap();
        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 1);
        assert!(r2.get("b").is_none(), "a migrated file must not re-import");

        std::fs::remove_dir_all(state_file.parent().unwrap()).ok();
    }

    #[test]
    fn static_deployment_routes_and_is_prepopulated() {
        let r = Registry::new("unused.json");
        r.upsert(static_spec(
            "proxy",
            vec![path("/legacy")],
            &["10.0.0.9:8080", "backend.internal:8080"],
        ));
        let d = r.route(None, "/legacy/x").unwrap();
        assert_eq!(d.spec.id, "proxy");
        // Backends come straight from the spec — no autoscaler needed.
        assert_eq!(d.backends().len(), 2);
    }

    #[test]
    fn static_deployment_survives_persist_round_trip() {
        let dir = std::env::temp_dir().join(format!("app-lb-static-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state_file = dir.join("state.json");

        let r = Registry::new(&state_file);
        r.upsert(static_spec("proxy", vec![path("/legacy")], &["10.0.0.9:8080"]));
        persist_all(&r);

        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 1);
        let d = r2.route(None, "/legacy").unwrap();
        assert!(d.spec.is_static());
        assert_eq!(d.backends().len(), 1, "backends rebuilt from upstreams on load");

        std::fs::remove_dir_all(&dir).ok();
    }
}
