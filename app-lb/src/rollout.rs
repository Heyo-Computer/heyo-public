//! Candidate-first replacement. The deployment record is the transaction log.
//! An uncertain create is looked up by its durable name, never blindly repeated.
use crate::{config::{DeploymentSpec, Driver}, deployment::{Deployment, DeploymentState, VmBackend, now_secs}, registry::Registry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub fn revision() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{:x}-{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

pub fn fingerprint(spec: &DeploymentSpec) -> String {
    let mut value = serde_json::to_value(spec).expect("serializable spec");
    value.sort_all_objects();
    format!("{:x}", Sha256::digest(serde_json::to_vec(&value).expect("JSON spec")))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub operation_id: String,
    pub expected_revision: String,
    pub spec: DeploymentSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Allocation {
    pub name: String,
    pub sandbox_id: Option<String>,
    pub attempted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub operation_id: String,
    pub deployment: String,
    pub source_revision: String,
    pub target_spec_sha256: String,
    pub status: String,
    pub phase: String,
    pub readiness_verified: bool,
    pub previous_stopped: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub preparation_stage: Option<String>,
    pub spec: DeploymentSpec,
    pub prepared: Option<DeploymentSpec>,
    pub prefix: String,
    pub allocations: Vec<Allocation>,
    pub previous: Vec<String>,
    pub stopped: Vec<String>,
    pub deadline: u64,
    pub drain_deadline: Option<u64>,
}

impl Operation {
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({"operation_id":self.operation_id,"deployment":self.deployment,
            "source_revision":self.source_revision,"target_spec_sha256":self.target_spec_sha256,
            "status":self.status,"phase":self.phase,"readiness_verified":self.readiness_verified,
            "previous_stopped":self.previous_stopped,"error":self.error,"preparation_stage":self.preparation_stage})
    }
}

pub fn reserved(d: &Deployment) -> bool {
    let state = d.state();
    state.retirement.is_some() || state.rollouts.iter().any(|o| matches!(o.status.as_str(), "running" | "reconciliation_required"))
        || state.route_handoff.as_ref().is_some_and(|h| h.phase != crate::registry::RouteHandoffPhase::Committed)
}

pub fn protected_ids(state: &DeploymentState) -> impl Iterator<Item = &String> {
    state.rollouts.iter().flat_map(|o| o.previous.iter().chain(o.allocations.iter().filter_map(|a| a.sandbox_id.as_ref())))
}

/// Both startup and continuous adoption must apply this before health or cleanup.
pub fn adoptable(d: &Deployment, name: &str, id: &str) -> bool {
    let state = d.state();
    if state.retirement.is_some() {return false;}
    if let Some(prefix) = &state.active_prefix {
        return name.starts_with(prefix);
    }
    !state.rollouts.iter().any(|o| name.starts_with(&o.prefix) || o.previous.contains(&id.to_string()) && o.readiness_verified)
}

fn digest(s: &str) -> bool { s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) }

pub fn validate(old: &DeploymentSpec, next: &DeploymentSpec) -> Result<(), String> {
    next.validate().map_err(|e| e.to_string())?;
    if old.id != next.id || old.namespace != next.namespace || old.routes != next.routes || old.auth != next.auth
        || old.account_id != next.account_id || old.user_id != next.user_id || old.maintenance != next.maintenance {
        return Err("rollout cannot change identity, namespace, routes, maintenance or request authentication".into());
    }
    for spec in [old, next] {
        let vm = spec.vm.as_ref().ok_or("rollout requires a managed Firecracker service")?;
        if vm.driver != Driver::Firecracker || vm.workspace.is_some() || vm.workspace_archive.is_some()
            || vm.mounts.iter().any(|m| !m.read_only) || spec.ingress.as_ref().is_some_and(|i| i.cloud)
            || vm.open_ports.iter().any(|port| *port != vm.port) || spec.routes.is_empty() {
            return Err("rollout supports stateless Firecracker services with read-only mounts and app-lb ingress only".into());
        }
    }
    let vm = next.vm_spec();
    if !next.artifact.as_ref().is_some_and(|a| digest(&a.artifact_ref) && a.store.starts_with("https://")) {
        return Err("pinned HTTPS rootfs artifact required; catalog name/size cannot attest preinstalled images".into());
    }
    if vm.mounts.iter().any(|m| !digest(&m.artifact_ref) || m.digest.as_deref() != Some(m.artifact_ref.as_str())) {
        return Err("every candidate mount must pin ref and digest to the same lowercase SHA256".into());
    }
    if next.health.path.is_none() || next.scaling.max_replicas == 0 {
        return Err("candidate requires HTTP health check and nonzero capacity".into());
    }
    if !next.health.expected_header.as_ref().is_some_and(|h| h.name.eq_ignore_ascii_case("x-heyo-revision")
        && matches!(h.value.len(), 40 | 64) && h.value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())) {
        return Err("rollout requires health.expected_header x-heyo-revision with exact lowercase Git SHA from the immutable service build".into());
    }
    Ok(())
}

#[async_trait::async_trait]
pub trait Runtime: Send + Sync {
    async fn prepare(&self, spec: &DeploymentSpec, generation: &str, progress: tokio::sync::watch::Sender<String>) -> Result<DeploymentSpec, String>;
    async fn list(&self) -> Result<Vec<heyo_sdk::SandboxInfo>, String>;
    async fn create(&self, spec: &DeploymentSpec, name: &str) -> Result<String, String>;
    async fn healthy(&self, info: &heyo_sdk::SandboxInfo, spec: &DeploymentSpec) -> Option<std::net::SocketAddr>;
    async fn stop(&self, id: &str) -> Result<(), String>;
}

struct Live { scaler: Arc<crate::autoscale::Autoscaler>, jobs: Arc<crate::jobs::Jobs> }
#[async_trait::async_trait]
impl Runtime for Live {
    async fn prepare(&self, spec: &DeploymentSpec, generation: &str, progress: tokio::sync::watch::Sender<String>) -> Result<DeploymentSpec, String> { self.jobs.prepare_candidate(spec, generation, progress).await }
    async fn list(&self) -> Result<Vec<heyo_sdk::SandboxInfo>, String> { self.scaler.vms().list().await.map_err(|e| e.to_string()) }
    async fn create(&self, spec: &DeploymentSpec, name: &str) -> Result<String, String> { self.scaler.create_candidate(spec, name).await }
    async fn healthy(&self, info: &heyo_sdk::SandboxInfo, spec: &DeploymentSpec) -> Option<std::net::SocketAddr> {
        let addr = crate::vm::routable_addr(info, spec.vm_spec().port).ok()?;
        crate::health::probe(addr, &spec.health).await.then_some(addr)
    }
    async fn stop(&self, id: &str) -> Result<(), String> { self.scaler.vms().suspend(id).await.map_err(|e| e.to_string()) }
}

pub struct Rollouts {
    registry: Arc<Registry>,
    runtime: Arc<dyn Runtime>,
    scaler: Option<Arc<crate::autoscale::Autoscaler>>,
    // One worker per process; admission writes only intents. The registry's
    // per-deployment files require a single owning app-lb process, as before.
    worker: tokio::sync::Mutex<HashMap<String, Vec<Arc<VmBackend>>>>,
}

impl Rollouts {
    pub fn new(registry: Arc<Registry>, scaler: Arc<crate::autoscale::Autoscaler>, jobs: Arc<crate::jobs::Jobs>) -> Self {
        Self { registry, runtime: Arc::new(Live { scaler: scaler.clone(), jobs }), scaler: Some(scaler), worker: tokio::sync::Mutex::new(HashMap::new()) }
    }

    /// Caller quiesces autoscale before acquiring the registry writer lock.
    pub fn admit(&self, d: &Arc<Deployment>, request: Request) -> Result<Operation, String> {
        let hash = fingerprint(&request.spec);
        if let Some(o) = d.state().rollouts.iter().find(|o| o.operation_id == request.operation_id) {
            return if o.source_revision == request.expected_revision && o.target_spec_sha256 == hash { Ok(o.clone()) }
                else { Err("operation_id reused with different payload".into()) };
        }
        if request.operation_id.is_empty() || request.operation_id.len() > 128
            || !request.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err("operation_id requires 1-128 ASCII letters, digits, hyphen or underscore".into());
        }
        if reserved(d) || d.state().rollout_revision != request.expected_revision { return Err("deployment reserved or revision changed".into()); }
        validate(&d.spec, &request.spec)?;
        if !d.pending().is_empty() || d.backends().is_empty() || d.backends().iter().any(|b| b.is_draining()) {
            return Err("source must have a stable serving pool without pending or draining replicas".into());
        }
        let prefix = format!("applb-{}-r{:x}", d.spec.id, Sha256::digest(format!("{}:{}", request.expected_revision, request.operation_id)));
        let count = request.spec.scaling.min_replicas.max(request.spec.scaling.warm_pool).max(1);
        let o = Operation { operation_id: request.operation_id, deployment: d.spec.id.clone(), source_revision: request.expected_revision,
            target_spec_sha256: hash, status: "running".into(), phase: "preparing".into(), readiness_verified: false, previous_stopped: false,
            error: None, preparation_stage: None, prefix: prefix.clone(), allocations: (0..count).map(|i| Allocation { name: format!("{prefix}{i:08x}"), sandbox_id: None, attempted: false }).collect(),
            previous: d.backends().iter().map(|b| b.sandbox_id.clone()).chain(d.state().suspended.iter().cloned()).collect(), stopped: vec![],
            deadline: now_secs() + request.spec.scaling.boot_timeout_secs.max(30).min(1800), drain_deadline: None, spec: request.spec, prepared: None };
        let mut state = (*d.state()).clone(); state.rollouts.push(o.clone());
        self.save(d, state)?;
        Ok(o)
    }

    fn save(&self, d: &Arc<Deployment>, mut state: DeploymentState) -> Result<(), String> {
        if let Err(e) = self.registry.persist_snapshot(d, &state) {
            // Rename may already have happened. Freeze in memory, preserve all
            // allocation evidence, and let restart read the durable authority.
            if let Some(o) = state.rollouts.last_mut() {
                o.status = "reconciliation_required".into(); o.error = Some(format!("persistence outcome uncertain: {e}"));
            }
            d.set_state(state);
            return Err(e.to_string());
        }
        d.set_state(state); Ok(())
    }

    pub async fn tick(&self) {
        let _retirement=self.registry.retirement_gate.read().await;
        let mut retiring = self.worker.lock().await;
        for d in self.registry.deployments().values() {
            if d.state().retirement.is_some() {continue;}
            if let Some(index) = d.state().rollouts.iter().position(|o| o.status == "running") {
                if let Err(e) = self.advance(d.clone(), index, &mut retiring).await {
                    tracing::warn!(deployment=%d.spec.id, error=%e, "candidate rollout paused");
                }
            }
        }
    }

    async fn record(&self, d: &Arc<Deployment>, index: usize, o: Operation) -> Result<(), String> {
        let _guard = self.registry.change_guard().await;
        if !self.registry.get(&d.spec.id).is_some_and(|live| Arc::ptr_eq(&live, d)) { return Err("deployment changed".into()); }
        let mut state = (*d.state()).clone(); state.rollouts[index] = o;
        self.save(d, state)
    }

    async fn advance(&self, d: Arc<Deployment>, index: usize, retiring: &mut HashMap<String, Vec<Arc<VmBackend>>>) -> Result<(), String> {
        let mut o = d.state().rollouts[index].clone();
        if o.phase != "draining" && now_secs() >= o.deadline {
            o.status = "failed".into();
            o.error = Some(if o.phase == "preparing" {
                format!("candidate artifact preparation deadline expired during {}; source retained",
                    o.preparation_stage.as_deref().unwrap_or("initializing"))
            } else { "candidate readiness deadline elapsed; source retained".into() });
            return self.record(&d, index, o).await;
        }
        if o.phase == "preparing" {
            if self.runtime.list().await?.iter().any(|info| crate::vm::owner_of(&info.name) == Some(&d.spec.id)
                && adoptable(&d, &info.name, &info.id) && !o.previous.contains(&info.id)) {
                o.status = "reconciliation_required".into(); o.error = Some("untracked source allocations require reconciliation before rollout".into());
                return self.record(&d, index, o).await;
            }
            let (progress, mut stages) = tokio::sync::watch::channel("initializing".to_string());
            // Rootfs transfers can legitimately exceed two minutes. Preparation
            // shares the persisted rollout deadline; restart never resets it.
            let result = {
                let preparation = tokio::time::timeout(Duration::from_secs(o.deadline.saturating_sub(now_secs())),
                    self.runtime.prepare(&o.spec, &o.prefix, progress));
                tokio::pin!(preparation);
                let mut progress_open = true;
                loop {
                    tokio::select! {
                        result = &mut preparation => break result,
                        changed = stages.changed(), if progress_open => {
                            if changed.is_err() { progress_open = false; continue; }
                            let mut snapshot = o.clone();
                            snapshot.preparation_stage = Some(stages.borrow_and_update().clone());
                            self.record(&d, index, snapshot).await?;
                        }
                    }
                }
            };
            o.preparation_stage = Some(stages.borrow().clone());
            let stage = o.preparation_stage.as_deref().unwrap_or("initializing");
            match result {
                Ok(Ok(spec)) if now_secs() < o.deadline => { o.prepared = Some(spec); o.phase = "creating".into(); }
                Ok(Err(_)) => {
                    // Remote errors can echo credentials or response bodies.
                    // Persist our bounded stage/status codes, never that text.
                    o.status = "failed".into();
                    o.error = Some(format!("candidate artifact preparation failed during {stage}; source retained"));
                }
                _ => {
                    o.status = "failed".into();
                    o.error = Some(format!("candidate artifact preparation deadline expired during {stage}; source retained"));
                }
            }
            return self.record(&d, index, o).await;
        }
        if o.phase == "creating" {
            for i in 0..o.allocations.len() {
                if o.allocations[i].sandbox_id.is_some() { continue; }
                if o.allocations[i].attempted {
                    let fleet = self.runtime.list().await?;
                    let matches: Vec<_> = fleet.iter().filter(|info| info.name == o.allocations[i].name).collect();
                    if matches.len() == 1 { o.allocations[i].sandbox_id = Some(matches[0].id.clone()); }
                    else { o.status = "reconciliation_required".into(); o.error = Some("allocation outcome unknown; create will not be repeated".into()); }
                    return self.record(&d, index, o).await;
                }
                o.allocations[i].attempted = true;
                self.record(&d, index, o.clone()).await?; // durable BEFORE create
                match tokio::time::timeout(Duration::from_secs(120), self.runtime.create(o.prepared.as_ref().ok_or("missing prepared spec")?, &o.allocations[i].name)).await {
                    Ok(Ok(id)) => o.allocations[i].sandbox_id = Some(id),
                    _ => { /* recover by exact allocation name on the next pass */ }
                }
                return self.record(&d, index, o).await;
            }
            o.phase = "verifying".into();
            return self.record(&d, index, o).await;
        }
        if o.phase == "verifying" {
            let fleet = self.runtime.list().await?;
            let mut ready = Vec::new();
            for a in &o.allocations {
                let Some(info) = fleet.iter().find(|v| Some(&v.id) == a.sandbox_id.as_ref() && v.name == a.name) else { return Ok(()); };
                if Some(&info.image) != o.prepared.as_ref().and_then(|s| s.vm_spec().image.as_ref()) {
                    o.status = "reconciliation_required".into(); o.error = Some("candidate image identity mismatch".into());
                    return self.record(&d, index, o).await;
                }
                let Some(addr) = self.runtime.healthy(info, &o.spec).await else { return Ok(()); };
                ready.push(Arc::new(VmBackend::new(info.id.clone(), addr)));
            }
            let _lifecycle = match &self.scaler { Some(s) => Some(s.rollout_guard().await), None => None };
            let _guard = self.registry.change_guard().await;
            if d.state().rollout_revision != o.source_revision || !self.registry.get(&d.spec.id).is_some_and(|live| Arc::ptr_eq(&live, &d)) {
                return Err("source revision changed before cutover".into());
            }
            let next = Arc::new(Deployment::new(o.prepared.clone().ok_or("missing prepared spec")?));
            let mut state = (*d.state()).clone();
            state.rollout_revision = revision(); state.active_prefix = Some(o.prefix.clone());
            state.suspended.clear(); // previous-generation retained VMs must never resume into the new pool
            o.readiness_verified = true; o.phase = "draining".into();
            o.drain_deadline = Some(now_secs() + d.spec.scaling.drain_timeout_secs);
            state.rollouts[index] = o.clone();
            // Active generation and exact retiring IDs land atomically, before
            // routing changes. On ambiguous persistence preserve BOTH pools.
            if let Err(e) = self.registry.persist_snapshot(&next, &state) {
                let mut frozen = (*d.state()).clone();
                frozen.rollouts[index].status = "reconciliation_required".into();
                frozen.rollouts[index].error = Some(format!("cutover persistence uncertain: {e}"));
                d.set_state(frozen); return Err(e.to_string());
            }
            next.set_state(state); next.set_backends(ready);
            retiring.insert(o.prefix.clone(), (*d.backends()).clone());
            // Fence admission on old Arcs before publication. Already-acquired
            // requests retain their Arc/counter and finish normally.
            for b in d.backends().iter() { b.set_draining(true); }
            self.registry.publish(next);
            d.ready_signal.notify_waiters();
            return Ok(());
        }
        if o.phase == "draining" {
            if now_secs() > o.drain_deadline.unwrap_or(0).saturating_add(300) {
                o.status = "reconciliation_required".into(); o.error = Some("post-cutover verification/stop deadline elapsed; generations retained".into());
                return self.record(&d, index, o).await;
            }
            // Restart may precede startup adoption. Do not stop the old service
            // until the committed active generation is healthy AND routable.
            let fleet = self.runtime.list().await?;
            let mut active = Vec::new();
            for allocation in &o.allocations {
                let Some(info) = fleet.iter().find(|v| Some(&v.id) == allocation.sandbox_id.as_ref() && v.name == allocation.name) else { return Ok(()); };
                if Some(&info.image) != d.spec.vm_spec().image.as_ref() { return Err("active generation image identity mismatch".into()); }
                let Some(addr) = self.runtime.healthy(info, &d.spec).await else { return Ok(()); };
                let backend = d.backends().iter().find(|b| b.sandbox_id == info.id).cloned()
                    .unwrap_or_else(|| Arc::new(VmBackend::new(info.id.clone(), addr)));
                active.push(backend);
            }
            {
                let _lifecycle = match &self.scaler { Some(s) => Some(s.rollout_guard().await), None => None };
                d.set_backends(active);
                d.ready_signal.notify_waiters();
            }
            let old = retiring.get(&o.prefix);
            if old.is_some_and(|pool| pool.iter().any(|b| b.in_flight() != 0)) && now_secs() < o.drain_deadline.unwrap_or(u64::MAX) { return Ok(()); }
            for id in &o.previous {
                if o.stopped.contains(id) { continue; }
                tokio::time::timeout(Duration::from_secs(30), self.runtime.stop(id)).await.map_err(|e| e.to_string())??;
                o.stopped.push(id.clone());
                self.record(&d, index, o.clone()).await?;
            }
            o.previous_stopped = true; o.status = "succeeded".into(); o.phase = "complete".into();
            retiring.remove(&o.prefix);
            return self.record(&d, index, o).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, atomic::{AtomicBool, Ordering}};

    #[derive(Default)]
    struct Fake {
        created: Mutex<Vec<(String, String)>>,
        stopped: Mutex<Vec<String>>,
        healthy: AtomicBool,
        lose_response: AtomicBool,
        hide: AtomicBool,
        stop_failure: AtomicBool,
        prepare_delay: Mutex<Duration>,
        prepare_failure: AtomicBool,
    }
    fn info(id: &str, name: &str) -> heyo_sdk::SandboxInfo {
        serde_json::from_value(serde_json::json!({"id":id,"name":name,"status":"Running","image":"verified-rootfs",
            "uptime_secs":1,"is_deployed":true,"status_changed_at":"","urls":[],"guest_ip":"127.0.0.1"})).unwrap()
    }
    #[async_trait::async_trait]
    impl Runtime for Fake {
        async fn prepare(&self, spec: &DeploymentSpec, _: &str, progress: tokio::sync::watch::Sender<String>) -> Result<DeploymentSpec, String> {
            progress.send_replace("blob_download".into());
            let delay = *self.prepare_delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            if self.prepare_failure.load(Ordering::SeqCst) {
                progress.send_replace("blob_http_403".into());
                return Err("remote response containing credential-super-secret".into());
            }
            let mut prepared = spec.clone(); prepared.vm.as_mut().unwrap().image = Some("verified-rootfs".into()); Ok(prepared)
        }
        async fn list(&self) -> Result<Vec<heyo_sdk::SandboxInfo>, String> {
            if self.hide.load(Ordering::SeqCst) { return Ok(vec![]); }
            Ok(self.created.lock().unwrap().iter().map(|(id,name)| info(id,name)).collect())
        }
        async fn create(&self, spec: &DeploymentSpec, name: &str) -> Result<String, String> {
            assert_eq!(spec.vm_spec().image.as_deref(), Some("verified-rootfs"));
            let mut created = self.created.lock().unwrap();
            let id = format!("candidate-{}", created.len()); created.push((id.clone(), name.into()));
            if self.lose_response.swap(false, Ordering::SeqCst) { Err("lost response".into()) } else { Ok(id) }
        }
        async fn healthy(&self, _: &heyo_sdk::SandboxInfo, _: &DeploymentSpec) -> Option<std::net::SocketAddr> {
            self.healthy.load(Ordering::SeqCst).then(|| "127.0.0.1:4321".parse().unwrap())
        }
        async fn stop(&self, id: &str) -> Result<(), String> {
            if self.stop_failure.load(Ordering::SeqCst) { return Err("stop failed".into()); }
            self.stopped.lock().unwrap().push(id.into()); Ok(())
        }
    }
    fn spec() -> DeploymentSpec {
        let mut spec: DeploymentSpec = serde_json::from_value(serde_json::json!({"id":"svc", "vm":{"driver":"firecracker","image":"base","port":4321},
            "routes":[{"host":"svc.test"}],"health":{"path":"/ready","expected_header":{"name":"x-heyo-revision","value":"c".repeat(40)}},"scaling":{"min_replicas":1,"max_replicas":2},
            "artifact":{"store":"https://artifacts.test","ref":"a".repeat(64)}})).unwrap();
        spec.normalize(); spec
    }
    fn engine(registry: Arc<Registry>, runtime: Arc<Fake>) -> Rollouts {
        Rollouts { registry, runtime, scaler: None, worker: tokio::sync::Mutex::new(HashMap::new()) }
    }
    fn setup() -> (tempfile::TempDir, Arc<Registry>, Arc<Deployment>, Arc<Fake>, Rollouts, Request) {
        let dir = tempfile::tempdir().unwrap(); let registry = Arc::new(Registry::new(dir.path().join("state.json")));
        let d = registry.upsert(spec());
        d.set_backends(vec![Arc::new(VmBackend::new("old-exact-id".into(), "127.0.0.1:1234".parse().unwrap()))]);
        registry.persist_one("svc").unwrap();
        let runtime = Arc::new(Fake::default()); let e = engine(registry.clone(), runtime.clone());
        let mut desired = d.spec.clone(); desired.vm.as_mut().unwrap().start_command = Some("new-service".into());
        let request = Request { operation_id: "release_123".into(), expected_revision: d.state().rollout_revision.clone(), spec: desired };
        (dir, registry, d, runtime, e, request)
    }
    async fn to_verifying(e: &Rollouts) { for _ in 0..3 { e.tick().await; } }

    #[tokio::test(start_paused = true)]
    async fn preparation_over_two_minutes_uses_remaining_durable_deadline() {
        let (_dir, _registry, d, runtime, e, request) = setup();
        *runtime.prepare_delay.lock().unwrap() = Duration::from_secs(150);
        e.admit(&d, request).unwrap();
        e.tick().await;
        let state = d.state();
        assert_eq!(state.rollouts[0].phase, "creating");
        assert_eq!(state.rollouts[0].status, "running");
        assert!(runtime.created.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn preparation_failure_and_deadline_preserve_source_and_durable_diagnostics() {
        for timeout in [false, true] {
            let (dir, _registry, d, runtime, e, request) = setup();
            *runtime.prepare_delay.lock().unwrap() = Duration::from_secs(if timeout { 301 } else { 1 });
            runtime.prepare_failure.store(!timeout, Ordering::SeqCst);
            e.admit(&d, request).unwrap();
            // A resumed operation gets only its remaining budget, not a fresh one.
            if timeout { d.mutate_state(|s| s.rollouts[0].deadline = now_secs() + 3); }
            let started = tokio::time::Instant::now();
            e.tick().await;
            assert!(started.elapsed() <= Duration::from_secs(3));
            let reloaded = Registry::new(dir.path().join("state.json"));
            reloaded.load().unwrap();
            let state = reloaded.get("svc").unwrap().state();
            let op = &state.rollouts[0];
            assert_eq!(op.status, "failed");
            assert_eq!(op.preparation_stage.as_deref(), Some(if timeout { "blob_download" } else { "blob_http_403" }));
            assert_eq!(op.error.as_deref().unwrap().contains("deadline expired"), timeout);
            assert!(!serde_json::to_string(op).unwrap().contains("credential-super-secret"));
            assert!(op.allocations.iter().all(|a| !a.attempted));
            assert!(d.select(&[]).is_some());
            assert!(runtime.created.lock().unwrap().is_empty());
            assert!(runtime.stopped.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn operation_ids_accept_ci_prefix_and_enforce_length_and_path_boundaries() {
        for (id, accepted) in [(format!("ci-service-{}", "a".repeat(64)), true),
            ("b".repeat(128), true), ("c".repeat(129), false), (String::new(), false), ("a/b".into(), false)] {
            let (_dir, _registry, d, _runtime, e, mut request) = setup();
            request.operation_id = id;
            assert_eq!(e.admit(&d, request).is_ok(), accepted);
        }
    }

    #[tokio::test]
    async fn candidate_first_cutover_drains_acquired_requests_and_retains_exact_vms() {
        let (_dir, registry, d, runtime, e, request) = setup();
        let backend = d.backends()[0].clone(); assert!(backend.try_acquire());
        let requested_hash = fingerprint(&request.spec);
        e.admit(&d, request.clone()).unwrap();
        let ((),()) = tokio::join!(e.tick(), e.tick());
        e.tick().await;
        assert_eq!(runtime.created.lock().unwrap().len(), 1);
        e.tick().await; // unhealthy candidate cannot replace the source
        assert!(Arc::ptr_eq(&registry.get("svc").unwrap(), &d));
        assert!(runtime.stopped.lock().unwrap().is_empty());
        assert!(d.select(&[]).is_some());
        runtime.healthy.store(true, Ordering::SeqCst); e.tick().await;
        let active = registry.get("svc").unwrap();
        assert!(!Arc::ptr_eq(&active, &d)); assert!(!backend.try_acquire());
        assert_eq!(backend.in_flight(), 1); assert_eq!(active.select(&[]).unwrap().sandbox_id, "candidate-0");
        assert_eq!(active.state().rollouts[0].target_spec_sha256, requested_hash);
        assert_ne!(fingerprint(&active.spec), requested_hash, "materialized image is separate from requested hash");
        e.tick().await; assert!(runtime.stopped.lock().unwrap().is_empty());
        backend.release(); e.tick().await;
        assert_eq!(*runtime.stopped.lock().unwrap(), vec!["old-exact-id"]);
        let state = active.state(); let op = &state.rollouts[0];
        assert_eq!(op.status, "succeeded"); assert!(op.readiness_verified && op.previous_stopped);
        assert!(protected_ids(&state).any(|id| id == "old-exact-id"));
        assert_eq!(e.admit(&active, request).unwrap().status, "succeeded", "completed exact replay");
    }

    #[tokio::test]
    async fn restart_after_lost_create_uses_exact_name_without_duplicate_allocation() {
        let (dir, registry, d, runtime, e, request) = setup();
        runtime.lose_response.store(true, Ordering::SeqCst);
        e.admit(&d, request).unwrap(); e.tick().await; e.tick().await;
        assert!(d.state().rollouts[0].allocations[0].sandbox_id.is_none());
        let restarted = Arc::new(Registry::new(dir.path().join("state.json"))); assert_eq!(restarted.load().unwrap(), 1);
        let e = engine(restarted.clone(), runtime.clone()); e.tick().await;
        let current = restarted.get("svc").unwrap(); let state = current.state();
        let a = &state.rollouts[0].allocations[0]; assert_eq!(a.sandbox_id.as_deref(), Some("candidate-0"));
        assert!(!adoptable(&current, &a.name, "candidate-0"));
        assert!(adoptable(&current, "applb-svc-old", "old-exact-id"));
        assert_eq!(runtime.created.lock().unwrap().len(), 1);
        assert_eq!(registry.get("svc").unwrap().spec.vm_spec().image.as_deref(), Some("base"));
    }

    #[tokio::test]
    async fn unknown_create_and_failed_candidate_never_replace_source() {
        let (_dir, registry, d, runtime, e, request) = setup();
        runtime.lose_response.store(true, Ordering::SeqCst); runtime.hide.store(true, Ordering::SeqCst);
        e.admit(&d, request).unwrap(); for _ in 0..6 { e.tick().await; }
        assert_eq!(d.state().rollouts[0].status, "reconciliation_required");
        assert!(reserved(&d)); assert_eq!(runtime.created.lock().unwrap().len(), 1);
        assert!(Arc::ptr_eq(&registry.get("svc").unwrap(), &d));
        assert!(runtime.stopped.lock().unwrap().is_empty());

        let (_dir, registry, d, runtime, e, request) = setup();
        e.admit(&d, request).unwrap(); to_verifying(&e).await;
        d.mutate_state(|s| s.rollouts[0].deadline = 0); e.tick().await;
        assert_eq!(d.state().rollouts[0].status, "failed"); assert!(d.select(&[]).is_some());
        assert!(Arc::ptr_eq(&registry.get("svc").unwrap(), &d));
        assert!(runtime.stopped.lock().unwrap().is_empty());
        let a = d.state().rollouts[0].allocations[0].clone(); assert!(!adoptable(&d, &a.name, a.sandbox_id.as_ref().unwrap()));
    }

    #[tokio::test]
    async fn ambiguous_commit_retains_both_and_restart_uses_durable_generation() {
        let (dir, registry, d, runtime, e, request) = setup();
        e.admit(&d, request).unwrap(); to_verifying(&e).await;
        runtime.healthy.store(true, Ordering::SeqCst);
        registry.fail_after_rename.store(true, Ordering::SeqCst); e.tick().await;
        assert!(Arc::ptr_eq(&registry.get("svc").unwrap(), &d));
        assert!(d.select(&[]).is_some()); assert_eq!(d.state().rollouts[0].status, "reconciliation_required");
        assert!(runtime.stopped.lock().unwrap().is_empty());
        let restarted = Arc::new(Registry::new(dir.path().join("state.json"))); assert_eq!(restarted.load().unwrap(), 1);
        let active = restarted.get("svc").unwrap();
        assert_eq!(active.spec.vm_spec().image.as_deref(), Some("verified-rootfs"));
        assert!(!adoptable(&active, "applb-svc-old", "old-exact-id"));
        let op = active.state().rollouts[0].clone(); assert!(adoptable(&active, &op.allocations[0].name, "candidate-0"));
        let e = engine(restarted.clone(), runtime.clone());
        runtime.healthy.store(false, Ordering::SeqCst); e.tick().await;
        assert!(runtime.stopped.lock().unwrap().is_empty(), "restart cannot stop source before active readiness");
        runtime.healthy.store(true, Ordering::SeqCst); e.tick().await;
        assert_eq!(*runtime.stopped.lock().unwrap(), vec!["old-exact-id"]);
        assert_eq!(active.state().rollouts[0].status, "succeeded");
        assert_eq!(runtime.created.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn admission_persistence_failure_conflicting_replay_and_stale_revision_are_fenced() {
        let (_dir, registry, d, runtime, e, request) = setup();
        let mut stale = request.clone(); stale.expected_revision = "old".into(); assert!(e.admit(&d, stale).is_err());
        registry.fail_after_rename.store(true, Ordering::SeqCst); assert!(e.admit(&d, request.clone()).is_err());
        assert!(reserved(&d)); e.tick().await; assert!(runtime.created.lock().unwrap().is_empty());
        let mut changed = request.clone(); changed.spec.vm.as_mut().unwrap().port = 1111;
        assert!(e.admit(&d, changed).is_err());
        assert_eq!(e.admit(&d, request).unwrap().status, "reconciliation_required");
    }

    #[tokio::test]
    async fn stop_failure_and_restart_do_not_report_success_or_repeat_create() {
        let (dir, registry, d, runtime, e, request) = setup();
        e.admit(&d, request).unwrap(); to_verifying(&e).await;
        runtime.healthy.store(true, Ordering::SeqCst); e.tick().await;
        runtime.stop_failure.store(true, Ordering::SeqCst); e.tick().await;
        let current = registry.get("svc").unwrap(); assert!(!current.state().rollouts[0].previous_stopped);
        let restarted = Arc::new(Registry::new(dir.path().join("state.json"))); restarted.load().unwrap();
        let e = engine(restarted.clone(), runtime.clone()); runtime.stop_failure.store(false, Ordering::SeqCst); e.tick().await;
        assert_eq!(restarted.get("svc").unwrap().state().rollouts[0].status, "succeeded");
        e.tick().await; assert_eq!(runtime.created.lock().unwrap().len(), 1);
        assert_eq!(*runtime.stopped.lock().unwrap(), vec!["old-exact-id"]);
    }

    #[test]
    fn pinned_artifacts_and_stateless_ingress_are_required() {
        let old = spec(); let mut next = old.clone(); assert!(validate(&old, &next).is_ok());
        next.health.expected_header = None; assert!(validate(&old, &next).is_err()); next = old.clone();
        next.artifact = None; assert!(validate(&old, &next).unwrap_err().contains("catalog"));
        next = old.clone(); next.artifact.as_mut().unwrap().artifact_ref = "latest".into(); assert!(validate(&old, &next).is_err());
        next = old.clone(); next.vm.as_mut().unwrap().open_ports = vec![9999]; assert!(validate(&old, &next).is_err());
        next = old.clone(); next.vm.as_mut().unwrap().mounts = serde_json::from_value(serde_json::json!([{"path":"/opt/code","store":"https://art.test","ref":"b".repeat(64),"digest":"c".repeat(64)}])).unwrap();
        assert!(validate(&old, &next).is_err());
        next.vm.as_mut().unwrap().mounts[0].digest = Some("b".repeat(64)); assert!(validate(&old, &next).is_ok());
        next.vm.as_mut().unwrap().mounts[0].read_only = false; assert!(validate(&old, &next).is_err());
    }

    #[tokio::test]
    async fn concurrent_admission_and_source_cas_cannot_publish_two_generations() {
        let (_dir, registry, d, runtime, e, request) = setup();
        let mut competing = request.clone(); competing.operation_id = "other".into();
        let (one, two) = tokio::join!(
            async { let _writer = registry.change_guard().await; e.admit(&d, request) },
            async { let _writer = registry.change_guard().await; e.admit(&d, competing) });
        assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
        assert_eq!(d.state().rollouts.len(), 1);
        to_verifying(&e).await;
        // A source revision changing after admission must still fail the commit CAS.
        d.mutate_state(|s| s.rollout_revision = revision());
        runtime.healthy.store(true, Ordering::SeqCst); e.tick().await;
        assert!(Arc::ptr_eq(&registry.get("svc").unwrap(), &d));
        assert!(!d.state().rollouts[0].readiness_verified);
        assert!(runtime.stopped.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn retained_old_vms_are_stopped_but_cannot_resume_in_new_generation() {
        let (_dir, registry, d, runtime, e, request) = setup();
        d.mutate_state(|s| s.suspended.push("retained-old".into()));
        e.admit(&d, request).unwrap(); to_verifying(&e).await;
        runtime.healthy.store(true, Ordering::SeqCst); e.tick().await; e.tick().await;
        let active = registry.get("svc").unwrap(); let state = active.state();
        assert!(state.suspended.is_empty());
        assert!(protected_ids(&state).any(|id| id == "retained-old"));
        assert_eq!(*runtime.stopped.lock().unwrap(), vec!["old-exact-id", "retained-old"]);
        assert!(!adoptable(&active, "applb-svc-retained", "retained-old"));
    }

    #[tokio::test]
    async fn failed_allocation_intent_write_prevents_create_even_after_restart() {
        let (dir, registry, d, runtime, e, request) = setup();
        e.admit(&d, request).unwrap(); e.tick().await;
        registry.fail_after_rename.store(true, Ordering::SeqCst); e.tick().await;
        assert!(runtime.created.lock().unwrap().is_empty());
        let restarted = Arc::new(Registry::new(dir.path().join("state.json"))); restarted.load().unwrap();
        let e = engine(restarted.clone(), runtime.clone()); e.tick().await;
        assert_eq!(restarted.get("svc").unwrap().state().rollouts[0].status, "reconciliation_required");
        assert!(runtime.created.lock().unwrap().is_empty(), "unknown pre-create crash cannot be retried safely");
    }
}
