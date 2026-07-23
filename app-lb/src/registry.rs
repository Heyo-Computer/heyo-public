//! The shared registry: the one piece of state the proxy, autoscaler, and admin
//! API all touch.
//!
//! Pingora fixes its service set at startup (`Server::run_forever(self)`
//! consumes the server), so "dynamic deployment registration" cannot mean adding
//! services at runtime. Instead every deployment lives in this registry behind
//! `ArcSwap`, and a single proxy service routes across whatever is currently in
//! it. Readers are lock-free; writers copy-on-write.

use crate::config::DeploymentSpec;
use crate::deployment::Deployment;
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Route rules pre-sorted most-specific-first, so the first match wins.
#[derive(Debug, Default)]
pub struct RouteTable {
    entries: Vec<(crate::config::RouteRule, String)>,
}

impl RouteTable {
    pub fn build(deployments: &HashMap<String, Arc<Deployment>>) -> Self {
        let mut entries: Vec<_> = deployments
            .values()
            .flat_map(|d| {
                d.spec
                    .routes
                    .iter()
                    .cloned()
                    .map(|r| (r, d.spec.id.clone()))
            })
            .collect();
        // Most specific first: a host+path rule must beat a bare path rule, and
        // `/api/v2` must beat `/api`. Ties broken by id for determinism.
        entries.sort_by(|(a, ai), (b, bi)| {
            b.specificity()
                .cmp(&a.specificity())
                .then_with(|| ai.cmp(bi))
        });
        Self { entries }
    }

    pub fn resolve(&self, host: Option<&str>, path: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(rule, _)| rule.matches(host, path))
            .map(|(_, id)| id.as_str())
    }
}

#[derive(Debug)]
pub struct Registry {
    deployments: ArcSwap<HashMap<String, Arc<Deployment>>>,
    routes: ArcSwap<RouteTable>,
    persist_path: PathBuf,
}

impl Registry {
    pub fn new(persist_path: impl Into<PathBuf>) -> Self {
        Self {
            deployments: ArcSwap::from_pointee(HashMap::new()),
            routes: ArcSwap::from_pointee(RouteTable::default()),
            persist_path: persist_path.into(),
        }
    }

    pub fn deployments(&self) -> Arc<HashMap<String, Arc<Deployment>>> {
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
        let id = deployment.spec.id.clone();
        self.mutate(|map| {
            map.insert(id.clone(), deployment.clone());
        });
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
        let id = new.spec.id.clone();
        self.mutate(|map| {
            map.insert(id.clone(), new.clone());
        });
        Some(new)
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Deployment>> {
        let mut removed = None;
        self.mutate(|map| {
            removed = map.remove(id);
        });
        removed
    }

    /// Copy-on-write mutation plus route-table rebuild.
    ///
    /// Not atomic against a concurrent writer, which is fine: the admin API is
    /// the only writer of the deployment set, and it is a single service.
    fn mutate(&self, f: impl FnOnce(&mut HashMap<String, Arc<Deployment>>)) {
        let mut next = (**self.deployments.load()).clone();
        f(&mut next);
        let routes = RouteTable::build(&next);
        self.deployments.store(Arc::new(next));
        self.routes.store(Arc::new(routes));
    }

    pub fn specs(&self) -> Vec<DeploymentSpec> {
        let mut specs: Vec<_> = self
            .deployments()
            .values()
            .map(|d| d.spec.clone())
            .collect();
        specs.sort_by(|a, b| a.id.cmp(&b.id));
        specs
    }

    pub fn persist_path(&self) -> &Path {
        &self.persist_path
    }

    /// Write specs (not live VM state) so deployments survive a restart.
    pub fn persist(&self) -> std::io::Result<()> {
        let specs = self.specs();
        let json = serde_json::to_vec_pretty(&specs)?;
        // Write-then-rename so a crash mid-write can't truncate existing state.
        let tmp = self.persist_path.with_extension("json.tmp");
        if let Some(parent) = tmp.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.persist_path)
    }

    /// Load persisted specs. A missing file is a normal first run.
    pub fn load(&self) -> std::io::Result<usize> {
        let bytes = match std::fs::read(&self.persist_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let specs: Vec<DeploymentSpec> = serde_json::from_slice(&bytes)?;
        let mut loaded = 0;
        for spec in specs {
            // Persisted state predates any validation change; skip bad entries
            // rather than refusing to start.
            if let Err(e) = spec.validate() {
                tracing::warn!(id = %spec.id, error = %e, "skipping invalid persisted deployment");
                continue;
            }
            self.upsert(spec);
            loaded += 1;
        }
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HealthCheck, RouteRule, ScalingPolicy, VmSpec};
    use heyo_sdk::SandboxDriver;

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
        r.persist().unwrap();

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
        r.persist().unwrap();

        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 1);
        let d = r2.route(None, "/legacy").unwrap();
        assert!(d.spec.is_static());
        assert_eq!(d.backends().len(), 1, "backends rebuilt from upstreams on load");

        std::fs::remove_dir_all(&dir).ok();
    }
}
