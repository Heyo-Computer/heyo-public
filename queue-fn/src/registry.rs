//! The shared registry: the one piece of state the dispatcher, autoscaler,
//! scheduler, and admin API all touch.
//!
//! Every function lives here behind `ArcSwap`. Readers are lock-free; writers
//! copy-on-write. The admin API is the only writer of the *set*, and the
//! autoscaler the only writer of each function's VM pool, so no lock is needed
//! to keep the two from colliding — they write different things.
//!
//! Only specs are persisted, never live VM state. Running VMs are recovered
//! after a restart by adopting them back from the daemon by name prefix, which
//! is authoritative in a way a state file written before a crash is not.

use crate::config::FunctionSpec;
use crate::function::Function;
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub struct Registry {
    functions: ArcSwap<HashMap<String, Arc<Function>>>,
    persist_path: PathBuf,
}

impl Registry {
    pub fn new(persist_path: impl Into<PathBuf>) -> Self {
        Self {
            functions: ArcSwap::from_pointee(HashMap::new()),
            persist_path: persist_path.into(),
        }
    }

    pub fn functions(&self) -> Arc<HashMap<String, Arc<Function>>> {
        self.functions.load_full()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Function>> {
        self.functions().get(id).cloned()
    }

    /// Register or replace a function.
    ///
    /// Replacing builds a fresh `Function`, so the old pool's VMs are dropped
    /// from dispatch immediately; the autoscaler reaps the orphaned sandboxes on
    /// its next tick by diffing against the daemon's list.
    pub fn upsert(&self, spec: FunctionSpec) -> Arc<Function> {
        let function = Arc::new(Function::new(spec));
        let id = function.spec.id.clone();
        self.mutate(|map| {
            map.insert(id.clone(), function.clone());
        });
        function
    }

    /// Update a function's spec in place, **preserving its live VM pool**.
    ///
    /// Unlike `upsert` (which abandons the old pool for the autoscaler to reap),
    /// this carries the existing workers and pending VMs onto a fresh `Function`
    /// built from the new spec. The pool is a vec of shared `Arc<VmWorker>`, so
    /// the moved VMs keep their slot state — an invocation running during the
    /// edit still holds its claim, and its `SlotGuard` still releases the same
    /// worker afterwards.
    ///
    /// Only valid when the VM *template* is unchanged; a template change means
    /// the running VMs were built from a different spec and must be rebuilt.
    /// The admin layer makes that decision.
    pub fn update(&self, spec: FunctionSpec) -> Option<Arc<Function>> {
        let old = self.get(&spec.id)?;
        let new = Arc::new(Function::new(spec));
        new.set_workers((*old.workers()).clone());
        new.set_pending((*old.pending()).clone());
        new.set_queue_depth(old.queue_pending(), old.queue_ack_pending());
        new.set_paused(old.is_paused());
        let id = new.spec.id.clone();
        self.mutate(|map| {
            map.insert(id.clone(), new.clone());
        });
        Some(new)
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Function>> {
        let mut removed = None;
        self.mutate(|map| {
            removed = map.remove(id);
        });
        removed
    }

    /// Copy-on-write mutation.
    ///
    /// Not atomic against a concurrent writer, which is fine: the admin API is
    /// the only writer of the function set, and it is a single service.
    fn mutate(&self, f: impl FnOnce(&mut HashMap<String, Arc<Function>>)) {
        let mut next = (**self.functions.load()).clone();
        f(&mut next);
        self.functions.store(Arc::new(next));
    }

    pub fn specs(&self) -> Vec<FunctionSpec> {
        let mut specs: Vec<_> = self.functions().values().map(|f| f.spec.clone()).collect();
        specs.sort_by(|a, b| a.id.cmp(&b.id));
        specs
    }

    pub fn persist_path(&self) -> &Path {
        &self.persist_path
    }

    /// Write specs (not live VM state) so functions survive a restart.
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
        let specs: Vec<FunctionSpec> = serde_json::from_slice(&bytes)?;
        let mut loaded = 0;
        for spec in specs {
            // Persisted state predates any validation change; skip bad entries
            // rather than refusing to start. One function that no longer
            // validates must not take the whole process down with it.
            if let Err(e) = spec.validate() {
                tracing::warn!(id = %spec.id, error = %e, "skipping invalid persisted function");
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
    use crate::config::{ExecSpec, PayloadMode, RetryPolicy, ScalingPolicy, VmSpec};
    use crate::function::VmWorker;
    use heyo_sdk::SandboxDriver;

    fn spec(id: &str) -> FunctionSpec {
        FunctionSpec {
            id: id.into(),
            vm: VmSpec {
                driver: SandboxDriver::Firecracker,
                image: None,
                start_command: None,
                size_class: None,
                disk_size_gb: None,
                working_directory: None,
                env_vars: None,
                ttl_seconds: 3600,
            },
            exec: ExecSpec {
                command: "true".into(),
                working_directory: None,
                env: None,
                timeout_secs: 20,
                max_payload_bytes: 4096,
                payload_mode: PayloadMode::Env,
            },
            scaling: ScalingPolicy::default(),
            triggers: vec![],
            retry: RetryPolicy::default(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("queue-fn-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn upsert_replaces_by_id() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a"));
        let mut edited = spec("a");
        edited.exec.command = "echo changed".into();
        r.upsert(edited);
        assert_eq!(r.functions().len(), 1);
        assert_eq!(r.get("a").unwrap().spec.exec.command, "echo changed");
    }

    #[test]
    fn remove_drops_the_function() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a"));
        assert!(r.remove("a").is_some());
        assert!(r.get("a").is_none());
        assert!(r.remove("a").is_none(), "removing twice is not an error");
    }

    /// Regression: an in-place spec edit built a fresh `Function` and left the
    /// pool empty, so every VM the autoscaler had booted was orphaned on an edit
    /// as trivial as a timeout change — and any invocation running at that
    /// moment released its slot onto a worker nothing referenced any more.
    #[test]
    fn update_preserves_the_live_pool_and_its_slot_state() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a"));
        let before = r.get("a").unwrap();
        let worker = Arc::new(VmWorker::new("sb-1".into()));
        let slot = worker.try_claim().expect("claim");
        before.set_workers(vec![worker.clone()]);

        let mut edited = spec("a");
        edited.exec.timeout_secs = 10;
        let updated = r.update(edited).unwrap();

        let pool = updated.workers();
        assert_eq!(pool.len(), 1);
        assert!(
            Arc::ptr_eq(&pool[0], &worker),
            "the running VM must be carried over, not rebuilt"
        );
        assert!(pool[0].is_busy(), "the in-flight claim survives the edit");
        assert_eq!(updated.spec.exec.timeout_secs, 10);

        drop(slot);
        assert!(!pool[0].is_busy(), "and the guard still releases the same worker");
    }

    #[test]
    fn update_carries_queue_depth_and_pause_state() {
        let r = Registry::new("unused.json");
        r.upsert(spec("a"));
        let before = r.get("a").unwrap();
        before.set_queue_depth(9, 2);
        before.set_paused(true);

        let updated = r.update(spec("a")).unwrap();
        assert_eq!(updated.queue_pending(), 9);
        assert_eq!(updated.queue_ack_pending(), 2);
        assert!(
            updated.is_paused(),
            "an edit must not silently resume a function an operator paused"
        );
    }

    #[test]
    fn update_of_an_unknown_function_is_none() {
        let r = Registry::new("unused.json");
        assert!(r.update(spec("ghost")).is_none());
    }

    #[test]
    fn persist_round_trips() {
        let dir = temp_dir("persist");
        let state_file = dir.join("state.json");

        let r = Registry::new(&state_file);
        r.upsert(spec("a"));
        r.upsert(spec("b"));
        r.persist().unwrap();

        let r2 = Registry::new(&state_file);
        assert_eq!(r2.load().unwrap(), 2);
        assert!(r2.get("a").is_some());
        assert!(r2.get("b").is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_a_clean_first_run() {
        let r = Registry::new("/nonexistent/definitely/not/here.json");
        assert_eq!(r.load().unwrap(), 0);
    }

    /// Regression: a spec that no longer validates (written by an older build,
    /// or hand-edited) used to abort the whole load, so one bad entry meant the
    /// process came up with *no* functions rather than the rest of them.
    #[test]
    fn an_invalid_persisted_entry_is_skipped_not_fatal() {
        let dir = temp_dir("skip-invalid");
        let state_file = dir.join("state.json");

        let good = spec("good");
        let mut bad = spec("bad");
        // Past the guest exec ceiling: accepted by serde, rejected by validate.
        bad.exec.timeout_secs = 9_999;
        std::fs::write(
            &state_file,
            serde_json::to_vec_pretty(&vec![bad, good]).unwrap(),
        )
        .unwrap();

        let r = Registry::new(&state_file);
        assert_eq!(r.load().unwrap(), 1);
        assert!(r.get("good").is_some());
        assert!(r.get("bad").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: `persist` wrote in place, so a crash partway through left a
    /// truncated file that failed to parse on the next boot — losing every
    /// function rather than the one being written.
    #[test]
    fn persist_leaves_no_temp_file_behind() {
        let dir = temp_dir("no-tmp");
        let state_file = dir.join("state.json");

        let r = Registry::new(&state_file);
        r.upsert(spec("a"));
        r.persist().unwrap();

        assert!(state_file.exists());
        assert!(
            !state_file.with_extension("json.tmp").exists(),
            "the temp file must be renamed onto the target, not left alongside it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn specs_are_sorted_for_a_stable_state_file() {
        let r = Registry::new("unused.json");
        r.upsert(spec("c"));
        r.upsert(spec("a"));
        r.upsert(spec("b"));
        let ids: Vec<_> = r.specs().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["a", "b", "c"], "stable order keeps diffs readable");
    }
}
