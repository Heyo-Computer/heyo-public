//! Deploy jobs: the two ways a deployment's code gets updated.
//!
//! Both kinds run as an async task on the app-lb host, one at a time per
//! deployment, and are polled through the same records:
//!
//! * **`image-build`** (managed deployments) — get a Dockerfile onto this host,
//!   hand it to `heyvm mvm build`, then rewrite `vm.image` to the image that
//!   produced, which recycles the pool onto it. heyvm has no build API, so this
//!   is app-lb driving child processes.
//!
//!   Two ways in, and only the first few steps differ: `git fetch` a repo and
//!   find a Dockerfile inside it, or fetch a Dockerfile manifest from an
//!   artifact store and unpack its context. Both converge on one `Prepared` —
//!   a Dockerfile, a context directory, and a version string to name the image
//!   after — which is why there is one job kind and not two.
//! * **`artifact-pull`** (managed deployments) — resolve a reference in an
//!   artifact store to a rootfs blob, materialize it as an `.ext4` heyvmd can
//!   boot, and rewrite `vm.image` the same way. The difference from a build is
//!   that nothing is produced: the digest names bytes that already exist, so the
//!   same reference gives the same rootfs on every host that can reach the
//!   store. See [`crate::artifact`].
//!
//!   Worth being precise about, now that a build can also name a store: what
//!   separates the two is not *where the bytes live* but *whether an image is
//!   made*. A `build.store` holds a recipe and every host that uses it runs its
//!   own `docker build`; an `artifact.store` holds the finished rootfs and no
//!   host builds anything.
//! * **`host-update`** (static/`proxy_pass` deployments) — run a list of
//!   commands in a working directory on this host, then re-probe the upstreams
//!   to prove the service came back. A static deployment's backend is a process
//!   somebody else runs; this is the "somebody else" being app-lb.
//!
//! Each is rejected on the wrong kind of deployment: there is no image to build
//! or pull for a `proxy_pass` upstream, and a working directory on the host has
//! nothing to do with a microVM's rootfs. The two managed kinds are exclusive
//! per deployment too — `DeploymentSpec::validate` refuses a spec holding both
//! `build` and `artifact`, because both rewrite `vm.image` and a deployment with
//! two sources for it cannot say where the running image came from.
//!
//! Things this module is careful about, all because both specs arrive over the
//! admin API rather than from a config file:
//!
//! * **Credentials never reach argv.** A URL with a token in it lands in
//!   `.git/config` and in every `ps`, so values go through `GIT_ASKPASS` and the
//!   child's environment instead.
//! * **Paths cannot leave the checkout.** Validated in the spec, then re-checked
//!   after canonicalization so a symlink committed to a repo can't redirect a
//!   build at `/etc`.
//! * **One job per deployment.** A second request while one is running is a
//!   conflict, not a queue — two `heyvm mvm build`s writing the same
//!   `<image>.ext4`, or two `cargo build`s in one directory, would race.

use crate::artifact::{Puller, human as human_bytes};
use crate::autoscale::Autoscaler;
use crate::config::{ArtifactSpec, Backend, BuildSource, BuildSpec, UpdateSpec};
use crate::deployment::{Deployment, now_secs};
use crate::health;
use crate::registry::Registry;
use crate::secrets::SecretStore;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many jobs are remembered **per deployment**. Records are in memory only:
/// a job is a transient event, and the durable outcome of a successful one is
/// either the `image` in the persisted spec or the state of the host itself.
///
/// Per-deployment rather than global, because a global cap is not a retention
/// policy at fleet scale — it is a race. One deployment pulling images in a loop
/// would evict every other deployment's history, so the one job you came to
/// investigate is the one already gone.
const HISTORY_PER_DEPLOYMENT: usize = 20;

/// Ceiling across all deployments, so the fleet as a whole cannot pin unbounded
/// memory in job records. Reached only when thousands of deployments each have
/// recent jobs; the per-deployment cap does the real work.
const HISTORY_LIMIT: usize = 2_000;
/// Log lines kept per record. Enough to hold a compiler error or a failing
/// `RUN` step, not enough for a full `docker build` transcript.
const LOG_LIMIT: usize = 400;
/// How deep to look for a Dockerfile when the spec doesn't name one.
const SEARCH_DEPTH: usize = 3;
/// Directories that never contain the Dockerfile you meant.
const SKIP_DIRS: [&str; 6] = [".git", "node_modules", "target", "vendor", "dist", ".venv"];
/// Grace before the first post-update health probe. A service that was just
/// restarted may still have its predecessor's listener up for a moment, and a
/// probe that lands there would verify the process being replaced.
const VERIFY_SETTLE: Duration = Duration::from_secs(2);
/// Gap between verification probes.
const VERIFY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobKind {
    /// Build a guest image from git + a Dockerfile (managed deployments).
    ImageBuild,
    /// Materialize content from an artifact store: a guest rootfs for a managed
    /// deployment, an unpacked bundle for a site.
    ArtifactPull,
    /// Run commands in a working directory on this host (static deployments and
    /// sites).
    HostUpdate,
}

impl JobKind {
    fn label(self) -> &'static str {
        match self {
            Self::ImageBuild => "build",
            Self::ArtifactPull => "pull",
            Self::HostUpdate => "update",
        }
    }

    /// Whether this kind of job means anything for that kind of backend.
    ///
    /// Deliberately a table rather than a pair of `is_managed()` comparisons,
    /// because the mapping stopped being one-to-one when sites learned to pull:
    /// two kinds apply to a site, and `ArtifactPull` applies to two backends. A
    /// predicate that answers "managed?" cannot express either.
    fn applies_to(self, backend: Backend) -> bool {
        match self {
            // A guest image, from a Dockerfile. Only a VM has one.
            Self::ImageBuild => backend == Backend::Vm,
            // A rootfs for a VM, a directory tree for a site. What a static
            // deployment proxies to is somebody else's process, with neither.
            Self::ArtifactPull => matches!(backend, Backend::Vm | Backend::Site),
            // Commands in a directory on this host. A VM's backend is not here.
            Self::HostUpdate => matches!(backend, Backend::Upstreams | Backend::Site),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Succeeded,
    Failed,
}

/// One job, as the admin API reports it.
///
/// The kind-specific fields are omitted rather than nulled, so a `host-update`
/// record doesn't carry six empty image fields.
#[derive(Debug, Clone, Serialize)]
pub struct JobRecord {
    pub id: String,
    pub deployment: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub started_at: u64,
    pub finished_at: Option<u64>,

    // -- image-build ------------------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// What was asked for (`main`, a tag, a sha), or `None` for the remote's
    /// default branch.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// What it resolved to. The answer to "which commit is live?".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Dockerfile path relative to the checkout, once located.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Whether `vm.image` was updated and the pool told to roll. Set by both
    /// image sources — it describes the roll-out, not how the image was made.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rolled_out: bool,

    // -- artifact-pull ----------------------------------------------------
    /// The store this pulled from, URL or path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// What was asked for: a tag or a digest.
    #[serde(rename = "artifact", skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
    /// What it resolved to. The artifact counterpart of `commit`, and the answer
    /// to "which bytes are live?" — a tag can move, a digest cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Bytes transferred or copied. `0` with `reused` set means the
    /// content-addressed image was already on disk and nothing moved; `0`
    /// without it means a local store hardlinked the blob instead of copying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Whether the fetch was skipped because the content was already present.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reused: bool,
    /// The directory a site's bundle was unpacked into. Only a site pull sets
    /// it — for a managed deployment the pull's destination is `image`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_root: Option<String>,
    /// Regular files unpacked. The site pull's answer to "did this deploy what
    /// I think it did?", which `bytes` cannot give when the blob was hardlinked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,

    // -- host-update ------------------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands_total: Option<usize>,
    /// How many commands finished successfully — which command failed, without
    /// reading the log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands_run: Option<usize>,
    /// Whether the upstreams answered a health probe afterwards. `None` when
    /// verification was switched off or the job never got that far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,

    pub error: Option<String>,
    /// Tail of the combined output of every step.
    pub log: Vec<String>,
}

impl JobRecord {
    fn new(id: String, deployment: String, kind: JobKind) -> Self {
        Self {
            id,
            deployment,
            kind,
            status: JobStatus::Running,
            started_at: now_secs(),
            finished_at: None,
            repo: None,
            git_ref: None,
            commit: None,
            dockerfile: None,
            image: None,
            rolled_out: false,
            store: None,
            artifact_ref: None,
            digest: None,
            bytes: None,
            reused: false,
            site_root: None,
            files: None,
            working_dir: None,
            commands_total: None,
            commands_run: None,
            verified: None,
            error: None,
            log: Vec::new(),
        }
    }

    fn push_log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > LOG_LIMIT {
            let overflow = self.log.len() - LOG_LIMIT;
            self.log.drain(0..overflow);
        }
    }
}

/// Why a job could not be *started*. A job that starts and then fails is a
/// record with `status: failed`, not one of these.
#[derive(Debug)]
pub enum StartError {
    NoDeployment(String),
    /// The deployment is the wrong kind for this job. Carries the backend it
    /// actually has, because "wrong kind" is only useful with what it is
    /// instead — and with three backends and three job kinds, the message
    /// cannot be inferred from the job kind alone.
    WrongKind {
        id: String,
        kind: JobKind,
        backend: Backend,
    },
    NoSpec {
        id: String,
        kind: JobKind,
    },
    AlreadyRunning(String),
    BadRef(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDeployment(id) => write!(f, "no deployment {id:?}"),
            Self::WrongKind {
                id,
                kind: JobKind::ImageBuild,
                backend,
            } => write!(
                f,
                "deployment {id:?} {}, so it has no guest image to build. {}",
                describe_backend(*backend),
                match backend {
                    Backend::Site => "Use `pull` to unpack a bundle from an artifact store, \
                                      or `update` to build on this host",
                    _ => "Use `update` to run commands on the host instead",
                }
            ),
            Self::WrongKind {
                id,
                kind: JobKind::ArtifactPull,
                backend,
            } => write!(
                f,
                "deployment {id:?} {}, so there is nothing for a pull to land in — it \
                 forwards to upstreams somebody else runs. Use `update` to run commands on \
                 the host instead",
                describe_backend(*backend),
            ),
            Self::WrongKind {
                id,
                kind: JobKind::HostUpdate,
                backend,
            } => write!(
                f,
                "deployment {id:?} {}; its backends are microVMs, not processes on this \
                 host. Use `build` or `pull` to change its image",
                describe_backend(*backend),
            ),
            Self::NoSpec {
                id,
                kind: JobKind::ImageBuild,
            } => write!(
                f,
                "deployment {id:?} has no `build` block — set `build.repo` (and optionally \
                 `build.dockerfile`) to build from a git checkout, or `build.store` and \
                 `build.ref` to build a Dockerfile manifest out of an artifact store"
            ),
            Self::NoSpec {
                id,
                kind: JobKind::ArtifactPull,
            } => write!(
                f,
                "deployment {id:?} has no `artifact` block — set `artifact.store` (an \
                 `art serve` URL or a store root on this host) and `artifact.ref` on the \
                 spec first"
            ),
            Self::NoSpec {
                id,
                kind: JobKind::HostUpdate,
            } => write!(
                f,
                "deployment {id:?} has no `update` block — set `update.working_dir` and \
                 `update.commands` on the spec first"
            ),
            Self::AlreadyRunning(id) => write!(
                f,
                "a job for deployment {id:?} is already running; wait for it to finish"
            ),
            Self::BadRef(r) => write!(f, "{r}"),
        }
    }
}

/// A digest, short enough for a one-line job outcome.
fn short(digest: &str) -> String {
    digest.chars().take(12).collect()
}

/// What a build source produced, which is all `heyvm mvm build` needs.
///
/// The narrow waist between the two sources: whatever a git checkout and an
/// artifact store had to do to get here, from this point on a build is the same
/// build. Adding a field is the test of whether a difference between the sources
/// is real — `size_mb` is, because only a manifest can carry a default; a
/// `commit` field would not be, because the image name is all it was ever for.
struct Prepared {
    dockerfile: PathBuf,
    context: PathBuf,
    /// What the image is named after: a commit, or a manifest digest.
    version: String,
    /// A `--size-mb` default carried by the source itself. Overridden by
    /// `build.image_size_mb` when the spec sets one.
    size_mb: Option<u64>,
}

/// A backend as it appears mid-sentence in a "wrong kind of deployment" error.
fn describe_backend(backend: Backend) -> &'static str {
    match backend {
        Backend::Vm => "is a managed VM pool",
        Backend::Upstreams => "is static (proxy_pass)",
        Backend::Site => "is a site: it serves files off disk",
    }
}

impl std::error::Error for StartError {}

pub struct JobConfig {
    /// Parent of the per-deployment checkouts.
    pub work_dir: PathBuf,
    pub heyvm_bin: String,
    /// The `art` CLI, for pulling from a store on this host.
    pub art_bin: String,
    /// Where a pulled rootfs is written, which must be the directory heyvmd
    /// resolves image names in. `Err` when it could not be worked out at
    /// startup; see [`crate::artifact::Puller`] for why that is not fatal.
    pub images_dir: Result<PathBuf, String>,
    pub git_bin: String,
    /// Shell that host-update commands are run through.
    pub shell: String,
    pub timeout: Duration,
    /// `HOME` for child processes, when app-lb and heyvmd run as different users.
    pub home: Option<String>,
}

pub struct Jobs {
    cfg: JobConfig,
    /// Built once and shared: it holds an HTTP client whose connection pool is
    /// worth keeping between pulls of the same store.
    puller: Puller,
    registry: Arc<Registry>,
    autoscaler: Arc<Autoscaler>,
    secrets: Arc<SecretStore>,
    history: Mutex<VecDeque<JobRecord>>,
    /// Deployment ids with a job in flight.
    running: Mutex<HashSet<String>>,
    /// Mirrors step output into app-obs, so a transcript outlives this process's
    /// bounded in-memory history. `None` when log shipping is off.
    obs: Option<crate::obs::LogSink>,
}

impl Jobs {
    pub fn new(
        cfg: JobConfig,
        registry: Arc<Registry>,
        autoscaler: Arc<Autoscaler>,
        secrets: Arc<SecretStore>,
        obs: Option<crate::obs::LogSink>,
    ) -> Self {
        Self {
            puller: Puller::new(
                cfg.art_bin.clone(),
                cfg.images_dir.clone(),
                cfg.home.clone(),
            ),
            cfg,
            registry,
            autoscaler,
            secrets,
            history: Mutex::new(VecDeque::new()),
            running: Mutex::new(HashSet::new()),
            obs,
        }
    }

    /// Jobs newest-first, optionally for one deployment.
    /// Return the newest record first.
    pub fn records(&self, deployment: Option<&str>) -> Vec<JobRecord> {
        self.history
            .lock()
            .expect("job history mutex poisoned")
            .iter()
            .rev()
            .filter(|r| deployment.is_none_or(|d| r.deployment == d))
            .cloned()
            .collect()
    }

    pub fn record(&self, job_id: &str) -> Option<JobRecord> {
        self.history
            .lock()
            .expect("job history mutex poisoned")
            .iter()
            .find(|r| r.id == job_id)
            .cloned()
    }

    /// Build a managed deployment's guest image, then roll the pool onto it.
    ///
    /// Asynchronous by necessity: a `docker build` takes minutes, and an admin
    /// API that blocks that long would time out in every client. Progress is
    /// polled through `GET /jobs/:id`.
    pub fn start_build(
        self: &Arc<Self>,
        deployment_id: &str,
        ref_override: Option<String>,
    ) -> Result<JobRecord, StartError> {
        let deployment = self.claimable(deployment_id, JobKind::ImageBuild)?;
        let Some(mut spec) = deployment.spec.build.clone() else {
            return Err(StartError::NoSpec {
                id: deployment_id.to_string(),
                kind: JobKind::ImageBuild,
            });
        };
        if let Some(r) = ref_override {
            // A one-off ref does not touch the stored spec — `POST …/build
            // {"ref": "v2.1"}` builds that tag without making it the default.
            spec.source_ref = Some(r);
            // The stored spec was validated on registration; an override was not.
            // This is also what refuses a git ref on a store source and vice
            // versa: the two have different rules and the same field.
            let probe = crate::config::DeploymentSpec {
                build: Some(spec.clone()),
                ..deployment.spec.clone()
            };
            probe.validate().map_err(|e| StartError::BadRef(e.to_string()))?;
        }

        // The record says up front which source this build is using, so a job
        // list distinguishes the two without waiting for the first log line.
        let source_ref = spec.source_ref.clone();
        let repo = spec.repo.clone();
        let store = spec.store.clone();
        self.spawn(deployment_id, JobKind::ImageBuild, move |r| {
            match (repo, store) {
                (Some(repo), _) => {
                    r.repo = Some(repo);
                    r.git_ref = source_ref;
                }
                (None, store) => {
                    r.store = store;
                    r.artifact_ref = source_ref;
                }
            }
        }, move |jobs, job_id, deployment_id| async move {
            jobs.run_build(&job_id, &deployment_id, &spec).await
        })
    }

    /// Pull a managed deployment's guest rootfs from an artifact store, then
    /// roll the pool onto it.
    ///
    /// The same shape as [`start_build`](Self::start_build), including the
    /// one-off reference override: `POST …/pull {"ref": "web-v2"}` pulls that
    /// tag without making it the deployment's default, which is what a rollback
    /// to a known digest looks like.
    ///
    /// `force` re-fetches even when the content-addressed image is already on
    /// disk. There is normally no reason to — the filename *is* the digest — so
    /// it exists for the one case the name cannot describe: a file that was
    /// damaged after it was written.
    pub fn start_pull(
        self: &Arc<Self>,
        deployment_id: &str,
        ref_override: Option<String>,
        force: bool,
    ) -> Result<JobRecord, StartError> {
        let deployment = self.claimable(deployment_id, JobKind::ArtifactPull)?;
        let Some(mut spec) = deployment.spec.artifact.clone() else {
            return Err(StartError::NoSpec {
                id: deployment_id.to_string(),
                kind: JobKind::ArtifactPull,
            });
        };
        if let Some(r) = ref_override {
            spec.artifact_ref = r;
            // The stored spec was validated on registration; an override was not.
            let probe = crate::config::DeploymentSpec {
                artifact: Some(spec.clone()),
                ..deployment.spec.clone()
            };
            probe.validate().map_err(|e| StartError::BadRef(e.to_string()))?;
        }

        let store = spec.store.clone();
        let reference = spec.artifact_ref.clone();
        self.spawn(
            deployment_id,
            JobKind::ArtifactPull,
            move |r| {
                r.store = Some(store);
                r.artifact_ref = Some(reference);
            },
            move |jobs, job_id, deployment_id| async move {
                jobs.run_pull(&job_id, &deployment_id, &spec, force).await
            },
        )
    }

    /// Run a static deployment's update commands on this host.
    pub fn start_update(
        self: &Arc<Self>,
        deployment_id: &str,
    ) -> Result<JobRecord, StartError> {
        let deployment = self.claimable(deployment_id, JobKind::HostUpdate)?;
        let Some(spec) = deployment.spec.update.clone() else {
            return Err(StartError::NoSpec {
                id: deployment_id.to_string(),
                kind: JobKind::HostUpdate,
            });
        };

        let working_dir = spec.working_dir.clone();
        let total = spec.commands.len();
        self.spawn(deployment_id, JobKind::HostUpdate, move |r| {
            r.working_dir = Some(working_dir);
            r.commands_total = Some(total);
            r.commands_run = Some(0);
        }, move |jobs, job_id, deployment_id| async move {
            jobs.run_update(&job_id, &deployment_id, &spec).await
        })
    }

    /// The shared checks every job start makes: the deployment exists and is the
    /// right kind for this job.
    fn claimable(
        &self,
        deployment_id: &str,
        kind: JobKind,
    ) -> Result<Arc<Deployment>, StartError> {
        let Some(deployment) = self.registry.get(deployment_id) else {
            return Err(StartError::NoDeployment(deployment_id.to_string()));
        };
        let backend = deployment.spec.backend();
        if !kind.applies_to(backend) {
            return Err(StartError::WrongKind {
                id: deployment_id.to_string(),
                kind,
                backend,
            });
        }
        Ok(deployment)
    }

    /// Claim the deployment's job slot, record the job, and spawn it.
    ///
    /// The slot is taken before spawning, so two concurrent requests cannot both
    /// see "not running", and released by `JobSlot`'s drop however the task
    /// ends — panic included.
    fn spawn<F, Fut>(
        self: &Arc<Self>,
        deployment_id: &str,
        kind: JobKind,
        describe: impl FnOnce(&mut JobRecord),
        run: F,
    ) -> Result<JobRecord, StartError>
    where
        F: FnOnce(Arc<Self>, String, String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send,
    {
        {
            let mut running = self.running.lock().expect("job slot mutex poisoned");
            if !running.insert(deployment_id.to_string()) {
                return Err(StartError::AlreadyRunning(deployment_id.to_string()));
            }
        }

        let mut record = JobRecord::new(new_job_id(), deployment_id.to_string(), kind);
        describe(&mut record);
        {
            let mut history = self.history.lock().expect("job history mutex poisoned");
            history.push_back(record.clone());
            trim_history(&mut history, deployment_id);
        }

        let jobs = self.clone();
        let job_id = record.id.clone();
        let deployment_id = deployment_id.to_string();
        tokio::spawn(async move {
            let _slot = JobSlot {
                jobs: jobs.clone(),
                deployment: deployment_id.clone(),
            };
            let started = std::time::Instant::now();
            match run(jobs.clone(), job_id.clone(), deployment_id.clone()).await {
                Ok(outcome) => {
                    tracing::info!(
                        deployment = %deployment_id,
                        job = %job_id,
                        kind = kind.label(),
                        outcome = %outcome,
                        elapsed_s = started.elapsed().as_secs(),
                        "job succeeded",
                    );
                    jobs.finish(&job_id, JobStatus::Succeeded, None);
                }
                Err(e) => {
                    tracing::error!(
                        deployment = %deployment_id,
                        job = %job_id,
                        kind = kind.label(),
                        error = %e,
                        elapsed_s = started.elapsed().as_secs(),
                        "job failed",
                    );
                    jobs.finish(&job_id, JobStatus::Failed, Some(e));
                }
            }
        });

        Ok(record)
    }

    fn update_record(&self, job_id: &str, f: impl FnOnce(&mut JobRecord)) {
        let mut history = self.history.lock().expect("job history mutex poisoned");
        if let Some(r) = history.iter_mut().find(|r| r.id == job_id) {
            f(r);
        }
    }

    /// Record one line of a job's output — in the record, and in app-obs.
    ///
    /// The deployment id is read off the record rather than passed in, so a line
    /// can only be attributed to a job that is still in the history; one whose
    /// record has aged out has nowhere honest to go and is dropped with it.
    fn log(&self, job_id: &str, line: impl Into<String>) {
        let line = line.into();
        // Only pay for the copy when there is somewhere to send it.
        let shipped = self.obs.is_some().then(|| line.clone());
        let mut deployment = None;
        self.update_record(job_id, |r| {
            deployment = Some(r.deployment.clone());
            r.push_log(line);
        });

        if let (Some(sink), Some(line), Some(deployment)) = (&self.obs, shipped, deployment) {
            sink.send(crate::obs::job_line(&deployment, job_id, line));
        }
    }

    fn finish(&self, job_id: &str, status: JobStatus, error: Option<String>) {
        self.update_record(job_id, |r| {
            r.status = status;
            r.finished_at = Some(now_secs());
            r.error = error;
        });
    }

    // -- image builds ------------------------------------------------------

    /// The build itself. Every error is a string because it goes straight into
    /// the record for a human to read.
    ///
    /// Two sources produce the same three things — a Dockerfile path, a context
    /// directory, and a version string to name the image after — and everything
    /// downstream of that is identical. So the sources diverge only for as long
    /// as they must, and `heyvm mvm build` is driven from one place: a second
    /// copy of the invocation is a second place for `--local-only` to be
    /// forgotten.
    async fn run_build(
        &self,
        job_id: &str,
        deployment_id: &str,
        spec: &BuildSpec,
    ) -> Result<String, String> {
        let prepared = match spec.source().ok_or_else(|| {
            // Unreachable through the admin API, which validates on registration
            // and on a ref override. Reachable by hand-editing the state file.
            format!(
                "deployment {deployment_id:?} has a `build` block with neither `repo` nor \
                 `store` set, so there is no Dockerfile to build"
            )
        })? {
            BuildSource::Git { .. } => self.prepare_git_build(job_id, deployment_id, spec).await?,
            BuildSource::Dockerfile { store, reference } => {
                self.prepare_store_build(job_id, deployment_id, spec, store, reference)
                    .await?
            }
        };

        let image = spec.image_for(deployment_id, &prepared.version);
        self.update_record(job_id, |r| r.image = Some(image.clone()));

        let mut cmd = tokio::process::Command::new(&self.cfg.heyvm_bin);
        cmd.arg("mvm")
            .arg("build")
            .arg("-f")
            .arg(&prepared.dockerfile)
            .arg("-c")
            .arg(&prepared.context)
            .arg("-n")
            .arg(&image)
            // Never upload: the image is consumed by the daemon on this host,
            // and a cloud push would need credentials app-lb does not have.
            .arg("--local-only")
            .current_dir(&prepared.context);
        // The spec wins over the manifest's annotation, which is only a default
        // recorded by whoever pushed the recipe; `build.image_size_mb` is the
        // operator of *this* deployment saying what its guest needs.
        if let Some(mb) = spec.image_size_mb.or(prepared.size_mb) {
            cmd.arg("--size-mb").arg(mb.to_string());
        }
        if let Some(home) = &self.cfg.home {
            cmd.env("HOME", home);
        }
        self.step(job_id, "heyvm", cmd, self.cfg.timeout).await?;

        // -- roll out --------------------------------------------------------
        self.roll_out(job_id, deployment_id, &image).await?;
        Ok(image)
    }

    /// Fetch a git checkout and find the Dockerfile in it.
    async fn prepare_git_build(
        &self,
        job_id: &str,
        deployment_id: &str,
        spec: &BuildSpec,
    ) -> Result<Prepared, String> {
        let repo = spec.repo.as_deref().expect("a git source has a repo");
        let checkout = self.cfg.work_dir.join(sanitize_dir(deployment_id));
        crate::tls::create_dir_private(&checkout)
            .map_err(|e| format!("could not create {}: {e}", checkout.display()))?;

        let token = self.git_token(spec.auth.as_ref())?;
        // Said once per build rather than once per git command: an ssh remote
        // authenticates with the host's key material, so a secret here is a
        // credential somebody thinks is in use and isn't.
        if token.is_some() && is_ssh_remote(repo) {
            let note = "build.auth is set on an ssh remote; git authenticates with the host's \
                        key material and the secret is ignored";
            tracing::warn!(repo = %repo, "{note}");
            self.log(job_id, note);
        }

        let commit = self.checkout(job_id, &checkout, spec, token.as_ref()).await?;
        self.update_record(job_id, |r| r.commit = Some(commit.clone()));

        let (dockerfile, context) = locate_dockerfile(&checkout, spec)?;
        let shown = dockerfile
            .strip_prefix(&checkout)
            .unwrap_or(&dockerfile)
            .display()
            .to_string();
        self.update_record(job_id, |r| r.dockerfile = Some(shown.clone()));
        self.log(job_id, format!("using {shown} (context {})", context.display()));

        Ok(Prepared {
            dockerfile,
            context,
            version: commit,
            size_mb: None,
        })
    }

    /// Fetch a Dockerfile manifest from an artifact store and lay it out on disk.
    ///
    /// The image is named after the *manifest* digest rather than the recipe's,
    /// because the manifest is what covers the whole build input — the recipe,
    /// the context and the annotations together. Naming it after the Dockerfile
    /// alone would give two builds with different contexts the same image name.
    async fn prepare_store_build(
        &self,
        job_id: &str,
        deployment_id: &str,
        spec: &BuildSpec,
        store: &str,
        reference: &str,
    ) -> Result<Prepared, String> {
        // Inside the deployment's own work directory, not beside it: a sibling
        // called `<id>-recipe` would collide with a deployment actually named
        // that, and nesting cannot, because the parent is already unique per
        // deployment.
        //
        // A subdirectory of the checkout rather than the checkout itself, so a
        // deployment that switches sources never unpacks a context over a git
        // tree — a `COPY` satisfied by a file the current recipe never shipped
        // is exactly what `git clean -xffdq` exists to prevent on the other
        // path. Switching the other way needs nothing: `git clean` removes this
        // directory along with everything else it did not put there.
        let deployment_dir = self.cfg.work_dir.join(sanitize_dir(deployment_id));
        // `0700` here rather than in the puller: a build context holds whatever
        // the person who pushed it put in it, and the mode is a property of
        // where app-lb stages work, not of how the bytes were fetched.
        crate::tls::create_dir_private(&deployment_dir)
            .map_err(|e| format!("could not create {}: {e}", deployment_dir.display()))?;
        let dir = deployment_dir.join(".recipe");

        let api_key = self.store_key(spec.auth.as_ref())?;
        if api_key.is_some() && !spec.store_is_remote() {
            let note = "build.auth is set on a local store; a store root is protected by file \
                        permissions, not by an API key, and the secret is unused";
            tracing::warn!(store = %store, "{note}");
            self.log(job_id, note);
        }

        let mut log = |line: String| self.log(job_id, line);
        let fetched = self
            .puller
            .fetch_dockerfile(store, reference, api_key.as_deref(), &dir, &mut log)
            .await?;

        self.update_record(job_id, |r| {
            r.digest = Some(fetched.manifest.clone());
            r.dockerfile = Some(crate::artifact::DOCKERFILE_ENTRY.to_string());
            r.bytes = Some(fetched.bytes_written);
            // The same question a site pull's `files` answers — "did this deploy
            // what I think it did?" — asked of the build's inputs. `bytes` cannot
            // answer it, because a local store hardlinks and transfers nothing.
            r.files = fetched.context_files;
        });

        Ok(Prepared {
            dockerfile: fetched.dockerfile,
            context: fetched.context,
            version: fetched.manifest,
            size_mb: fetched.size_mb,
        })
    }

    /// Fetch and check out, returning the resolved commit.
    async fn checkout(
        &self,
        job_id: &str,
        dir: &Path,
        spec: &BuildSpec,
        token: Option<&(String, String)>,
    ) -> Result<String, String> {
        let repo = spec
            .repo
            .as_deref()
            .ok_or("this build has no `repo`; a git checkout was asked for anyway")?;

        // `git init` on an existing repo just reinitializes it, so the checkout
        // directory survives between builds and a rebuild is a shallow fetch
        // rather than a fresh clone.
        let mut init = self.git(token);
        init.arg("init").arg("-q").arg(dir);
        self.step(job_id, "git", init, self.cfg.timeout).await?;

        let refspec = spec.source_ref.clone().unwrap_or_else(|| "HEAD".into());
        let fetch = |depth: Option<&str>| {
            let mut cmd = self.git(token);
            cmd.arg("-C").arg(dir).arg("fetch").arg("--no-tags").arg("--force");
            if let Some(d) = depth {
                cmd.arg("--depth").arg(d);
            }
            cmd.arg(repo).arg(&refspec);
            cmd
        };

        if let Err(shallow_err) = self.step(job_id, "git", fetch(Some("1")), self.cfg.timeout).await
        {
            // A raw commit sha can only be fetched directly if the server allows
            // it (`uploadpack.allowReachableSHA1InWant`); plenty don't. Falling
            // back to a full fetch turns "cannot deploy this commit" into
            // "deploying this commit is slower".
            if !looks_like_sha(&refspec) {
                return Err(shallow_err);
            }
            self.log(
                job_id,
                format!("shallow fetch of {refspec} was refused; retrying with full history"),
            );
            let mut full = self.git(token);
            full.arg("-C").arg(dir).arg("fetch").arg("--no-tags").arg("--force").arg(repo);
            self.step(job_id, "git", full, self.cfg.timeout).await?;
        }

        let mut checkout = self.git(token);
        checkout
            .arg("-C")
            .arg(dir)
            .arg("checkout")
            .arg("-q")
            .arg("--detach")
            .arg("--force")
            .arg(if spec.source_ref.is_some() && looks_like_sha(&refspec) {
                refspec.clone()
            } else {
                "FETCH_HEAD".into()
            });
        self.step(job_id, "git", checkout, self.cfg.timeout).await?;

        // Artefacts from a previous build of this checkout would otherwise land
        // in the docker context and, worse, could satisfy a `COPY` that the
        // current commit no longer produces.
        let mut clean = self.git(token);
        clean.arg("-C").arg(dir).arg("clean").arg("-xffdq");
        self.step(job_id, "git", clean, self.cfg.timeout).await?;

        let mut rev = self.git(token);
        rev.arg("-C").arg(dir).arg("rev-parse").arg("HEAD");
        let out = self.step(job_id, "git", rev, self.cfg.timeout).await?;
        let commit = out.trim().to_string();
        if commit.is_empty() {
            return Err("git rev-parse HEAD produced nothing".into());
        }
        Ok(commit)
    }

    /// Point the deployment at the new image and recycle its pool.
    ///
    /// Deliberately unconditional: rebuilding the same commit overwrites the same
    /// `<image>.ext4`, and running VMs hold a copy of the *old* rootfs, so
    /// "nothing changed in the spec" is not the same as "nothing changed". Same
    /// swap-then-teardown order as the admin API's update path — while the old
    /// deployment is still live the autoscaler would boot VMs into it.
    async fn roll_out(
        &self,
        job_id: &str,
        deployment_id: &str,
        image: &str,
    ) -> Result<(), String> {
        let Some(old) = self.registry.get(deployment_id) else {
            return Err(format!(
                "deployment {deployment_id:?} was removed while its image was building; \
                 the image {image:?} was built but nothing is using it"
            ));
        };
        let mut spec = old.spec.clone();
        let Some(vm) = spec.vm.as_mut() else {
            return Err(format!(
                "deployment {deployment_id:?} is no longer a managed VM deployment"
            ));
        };
        let previous = vm.image.clone();
        vm.image = Some(image.to_string());

        let deployment = self.registry.upsert(spec);
        self.autoscaler.teardown(&old).await;
        if let Err(e) = self.registry.persist_one(&deployment.spec.id) {
            tracing::error!(error = %e, "failed to persist state after a build");
        }
        deployment.scale_signal.notify_one();

        self.update_record(job_id, |r| {
            r.rolled_out = true;
            r.push_log(format!(
                "vm.image {} -> {image}; pool recycling",
                previous.as_deref().unwrap_or("(daemon default)")
            ));
        });
        tracing::info!(
            deployment = %deployment_id,
            image = %image,
            previous = previous.as_deref().unwrap_or("(none)"),
            "rolled deployment onto its new image",
        );
        Ok(())
    }

    // -- artifact pulls ----------------------------------------------------

    /// Resolve the reference, materialize the rootfs, roll the pool onto it.
    ///
    /// Shorter than a build because there is no source to fetch and nothing to
    /// compile — the work is entirely in [`crate::artifact`], and what is left
    /// here is the same "rewrite `vm.image` and recycle" ending a build has.
    async fn run_pull(
        &self,
        job_id: &str,
        deployment_id: &str,
        spec: &ArtifactSpec,
        force: bool,
    ) -> Result<String, String> {
        // Resolved here rather than inside the puller, so the one place that
        // reads secrets is the one place that already does for a build.
        let api_key = self.store_key(spec.auth.as_ref())?;

        // A site's artifact is a directory tree rather than a guest rootfs, and
        // everything after the resolve differs: where the bytes land, what
        // proves they landed, and whether there is a pool to roll afterwards.
        if let Some(site) = self.registry.get(deployment_id).and_then(|d| d.spec.site.clone()) {
            return self.run_site_pull(job_id, spec, &site, api_key.as_deref(), force).await;
        }

        let mut log = |line: String| self.log(job_id, line);
        let pulled = self
            .puller
            .pull(deployment_id, spec, api_key.as_deref(), force, &mut log)
            .await?;

        self.update_record(job_id, |r| {
            r.digest = Some(pulled.digest.clone());
            r.image = Some(pulled.image.clone());
            r.bytes = Some(pulled.bytes_written);
            r.reused = pulled.reused;
        });
        self.log(
            job_id,
            format!(
                "{} is {} ({})",
                pulled.path.display(),
                pulled.digest,
                human_bytes(pulled.size)
            ),
        );

        // Unconditional, exactly as a build's is. A pull that reused an image
        // already on disk still has to roll: the running VMs hold a copy of
        // whatever rootfs they booted from, which is not necessarily this one.
        self.roll_out(job_id, deployment_id, &pulled.image).await?;
        Ok(pulled.image)
    }

    /// The site reading of a pull: a bundle unpacked into `site.root`.
    ///
    /// Where the managed path ends in a pool roll, this ends in nothing at all —
    /// and that is the whole appeal. There is no image to name, no VM to
    /// recycle, and no window in which capacity is short: the files are simply
    /// the files, and the next request reads the new ones.
    ///
    /// It also ends without the verification step the *other* two deploy paths
    /// need, because that step has already happened. `pull_tree` checks the
    /// unpacked tree for the site's index before it swaps anything in, so by
    /// the time this returns there is nothing left to confirm — unlike an
    /// update, which can only look at the wreckage afterwards.
    async fn run_site_pull(
        &self,
        job_id: &str,
        spec: &ArtifactSpec,
        site: &crate::config::SiteSpec,
        api_key: Option<&str>,
        force: bool,
    ) -> Result<String, String> {
        let mut log = |line: String| self.log(job_id, line);
        let pulled = self
            .puller
            .pull_tree(spec, site, api_key, force, &mut log)
            .await?;

        let root = pulled.root.display().to_string();
        self.update_record(job_id, |r| {
            r.digest = Some(pulled.digest.clone());
            r.bytes = Some(pulled.bytes_written);
            r.reused = pulled.reused;
            r.site_root = Some(root.clone());
            // Both are about the tree that is now live, so a reused pull reports
            // what is serving rather than the zero it did not write.
            if !pulled.reused {
                r.files = Some(pulled.files);
            }
            // The index was checked before the swap; saying so is what makes a
            // succeeded site pull mean the same thing as a succeeded update.
            r.verified = Some(true);
        });

        if pulled.reused {
            return Ok(format!("{} already serving {}", root, short(&pulled.digest)));
        }
        Ok(format!(
            "{} file{} ({}) in {root}",
            pulled.files,
            if pulled.files == 1 { "" } else { "s" },
            crate::artifact::human(pulled.unpacked),
        ))
    }

    // -- host updates ------------------------------------------------------

    /// Run the update commands, then prove the upstreams came back.
    ///
    /// Nothing in the spec changes: a static deployment's backends are fixed
    /// addresses, and what moved is the code behind them. That is exactly why
    /// the verification step exists — without it a successful job would only
    /// mean "the commands exited 0", which is not the same as "the service is
    /// serving".
    async fn run_update(
        &self,
        job_id: &str,
        deployment_id: &str,
        spec: &UpdateSpec,
    ) -> Result<String, String> {
        let dir = Path::new(&spec.working_dir);
        if !dir.is_dir() {
            return Err(format!(
                "update.working_dir {} does not exist on this host (app-lb runs as {}), \
                 so there is nothing to update",
                spec.working_dir,
                whoami()
            ));
        }

        // Resolve every secret before running anything: discovering a missing
        // credential after `git pull` has already moved the working directory is
        // strictly worse than discovering it now.
        let env = self.update_env(spec)?;
        let token = self.git_token(spec.auth.as_ref())?;
        let timeout = spec
            .timeout_secs
            .map_or(self.cfg.timeout, Duration::from_secs);

        for (i, command) in spec.commands.iter().enumerate() {
            let mut cmd = tokio::process::Command::new(&self.cfg.shell);
            // Through a shell because that is what the spec's strings are:
            // `git pull --ff-only && cargo build --release` is one command to
            // whoever wrote it. The string is never interpolated into a larger
            // shell line, so it means exactly what it says.
            cmd.arg("-c").arg(command).current_dir(dir);
            for (k, v) in &env {
                cmd.env(k, v);
            }
            self.apply_git_auth(&mut cmd, token.as_ref());
            if let Some(home) = &self.cfg.home {
                cmd.env("HOME", home);
            }

            self.step(job_id, &format!("command {}", i + 1), cmd, timeout)
                .await
                .map_err(|e| format!("{e} (command {} of {})", i + 1, spec.commands.len()))?;
            self.update_record(job_id, |r| r.commands_run = Some(i + 1));
        }

        // -- verify ----------------------------------------------------------
        let wait = spec.verify_timeout();
        if wait.is_zero() {
            self.log(job_id, "verification is disabled (verify_timeout_secs: 0)");
            return Ok(format!("{} command(s)", spec.commands.len()));
        }

        // A site has no upstreams to probe, so "did it come back" is a different
        // question: did the build leave something servable in the root. Without
        // this the job would report failure after a perfectly good build, purely
        // because there was nothing to send a request to.
        if let Some(site) = self.registry.get(deployment_id).and_then(|d| d.spec.site.clone()) {
            return match verify_site(&site) {
                Ok(what) => {
                    self.update_record(job_id, |r| {
                        r.verified = Some(true);
                        r.push_log(what.clone());
                    });
                    Ok(format!("{} command(s), {what}", spec.commands.len()))
                }
                Err(e) => {
                    self.update_record(job_id, |r| r.verified = Some(false));
                    Err(format!(
                        "the commands succeeded but the site is not servable: {e}"
                    ))
                }
            };
        }

        self.log(
            job_id,
            format!(
                "commands finished; waiting up to {}s for the upstreams to answer",
                wait.as_secs()
            ),
        );
        match self.verify(deployment_id, wait).await {
            Ok(peers) => {
                self.update_record(job_id, |r| {
                    r.verified = Some(true);
                    r.push_log(format!("{peers} upstream(s) healthy"));
                });
                Ok(format!("{} command(s), upstreams healthy", spec.commands.len()))
            }
            Err(e) => {
                self.update_record(job_id, |r| r.verified = Some(false));
                // The commands succeeded, so this is not "the update failed to
                // run" — it is "the update ran and the service is not back".
                // Both are failures; only the message can tell them apart.
                Err(format!(
                    "every command succeeded, but {e}. The host has already been changed — \
                     check the service and its logs"
                ))
            }
        }
    }

    /// Poll the deployment's upstreams until they all answer, or give up.
    ///
    /// Probes directly rather than reading the autoscaler's `healthy` flags: a
    /// flag set two seconds ago describes the process that was just replaced.
    async fn verify(&self, deployment_id: &str, wait: Duration) -> Result<usize, String> {
        tokio::time::sleep(VERIFY_SETTLE.min(wait)).await;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let Some(d) = self.registry.get(deployment_id) else {
                return Err(format!("deployment {deployment_id:?} was removed mid-update"));
            };
            let backends = d.backends();
            if backends.is_empty() {
                return Err("the deployment has no upstreams to probe".to_string());
            }

            let mut unhealthy = Vec::new();
            for b in backends.iter() {
                if !probe_peer(&b.peer, &d.spec.health).await {
                    unhealthy.push(b.peer.clone());
                }
            }
            if unhealthy.is_empty() {
                return Ok(backends.len());
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "{} of {} upstream(s) did not answer within {}s ({})",
                    unhealthy.len(),
                    backends.len(),
                    wait.as_secs(),
                    unhealthy.join(", ")
                ));
            }
            tokio::time::sleep(VERIFY_INTERVAL).await;
        }
    }

    /// Literal env plus values pulled from the secret store.
    ///
    /// Ordered, so the log line naming which variables were set reads the same
    /// way twice — and so a secret always wins over a literal of the same name
    /// rather than winning at random.
    fn update_env(&self, spec: &UpdateSpec) -> Result<BTreeMap<String, String>, String> {
        let mut env: BTreeMap<String, String> =
            spec.env.clone().unwrap_or_default().into_iter().collect();
        for from in &spec.env_from {
            let value = self.secrets.resolve(&from.secret_ref()).map_err(|e| {
                format!("{e} — `serverctl get secrets` lists what this LB holds")
            })?;
            env.insert(from.env_name(), value);
        }
        Ok(env)
    }

    // -- shared child-process plumbing -------------------------------------

    /// Resolve a git credential reference into `(value, username)`.
    /// An artifact store's API key, out of the secret store.
    ///
    /// The counterpart of [`git_token`](Self::git_token), and the only other
    /// thing `build.auth` / `artifact.auth` can mean. Kept beside it so the
    /// places that read secrets stay countable.
    fn store_key(
        &self,
        auth: Option<&crate::secrets::SecretRef>,
    ) -> Result<Option<String>, String> {
        match auth {
            None => Ok(None),
            Some(r) => Ok(Some(self.secrets.resolve(r).map_err(|e| {
                format!("{e} — `serverctl get secrets` lists what this LB holds")
            })?)),
        }
    }

    fn git_token(
        &self,
        auth: Option<&crate::secrets::SecretRef>,
    ) -> Result<Option<(String, String)>, String> {
        match auth {
            None => Ok(None),
            Some(r) => Ok(Some((
                self.secrets.resolve(r).map_err(|e| {
                    format!("{e} — `serverctl get secrets` lists what this LB holds")
                })?,
                r.username.clone().unwrap_or_else(|| "x-access-token".into()),
            ))),
        }
    }

    /// A `git` invocation with the environment set so it can never block on a
    /// prompt, and so a token (when there is one) travels out of band.
    fn git(&self, token: Option<&(String, String)>) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.cfg.git_bin);
        if let Some(home) = &self.cfg.home {
            cmd.env("HOME", home);
        }
        self.apply_git_auth(&mut cmd, token);
        cmd
    }

    /// Make a child able to authenticate to git without the token appearing in
    /// its arguments. Applied to `git` itself for a build, and to the shell for
    /// a host update — whose first command is very often `git pull`.
    fn apply_git_auth(
        &self,
        cmd: &mut tokio::process::Command,
        token: Option<&(String, String)>,
    ) {
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        let Some((value, username)) = token else {
            return;
        };
        match self.askpass_script() {
            Ok(script) => {
                cmd.env("GIT_ASKPASS", script)
                    .env("APP_LB_GIT_TOKEN", value)
                    .env("APP_LB_GIT_USERNAME", username)
                    // Clear any credential helper the host has configured, so the
                    // answer comes from our askpass and not from a cached
                    // credential for a different account. `-c` for git itself;
                    // the env form also reaches a `git` run inside a shell command.
                    .env("GIT_CONFIG_COUNT", "1")
                    .env("GIT_CONFIG_KEY_0", "credential.helper")
                    .env("GIT_CONFIG_VALUE_0", "");
            }
            Err(e) => {
                tracing::error!(error = %e, "could not write the git askpass helper");
            }
        }
    }

    /// Write (once) the helper that answers git's credential prompts from the
    /// environment. On disk because `GIT_ASKPASS` takes a program, not a value;
    /// `0700` because it is executable, and it holds no secret itself.
    fn askpass_script(&self) -> std::io::Result<PathBuf> {
        let path = self.cfg.work_dir.join("git-askpass.sh");
        if path.exists() {
            return Ok(path);
        }
        crate::tls::create_dir_private(&self.cfg.work_dir)?;
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             # Written by app-lb: answers git's credential prompts from the environment,\n\
             # so a token is never visible in argv.\n\
             case \"$1\" in\n\
             \x20 Username*) printf %s \"${APP_LB_GIT_USERNAME:-x-access-token}\" ;;\n\
             \x20 *)         printf %s \"${APP_LB_GIT_TOKEN}\" ;;\n\
             esac\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(path)
    }

    /// Run one child to completion, logging its output and failing on a non-zero
    /// exit or a timeout.
    async fn step(
        &self,
        job_id: &str,
        label: &str,
        mut cmd: tokio::process::Command,
        timeout: Duration,
    ) -> Result<String, String> {
        let shown = describe(&cmd);
        self.log(job_id, format!("$ {shown}"));

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Without this a timed-out `docker build` keeps running after the
            // future is dropped, holding the daemon and the disk.
            .kill_on_drop(true);

        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "{label} is not installed or not on app-lb's PATH ({shown}): {e}"
                ));
            }
            Ok(Err(e)) => return Err(format!("could not run {label}: {e}")),
            Err(_) => {
                return Err(format!(
                    "{label} timed out after {}s",
                    timeout.as_secs()
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        for line in stdout.lines().chain(String::from_utf8_lossy(&output.stderr).lines()) {
            self.log(job_id, line.to_string());
        }

        if output.status.success() {
            Ok(stdout)
        } else {
            // The tail of stderr is what actually says why, so put it in the
            // error rather than making the caller go read the log.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            Err(format!(
                "{label} exited with {}: {}",
                output.status.code().map_or("a signal".into(), |c| c.to_string()),
                if tail.is_empty() {
                    "no output".to_string()
                } else {
                    tail.join(" / ")
                }
            ))
        }
    }
}

/// Frees a deployment's job slot however the task ends, panic included.
struct JobSlot {
    jobs: Arc<Jobs>,
    deployment: String,
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        if let Ok(mut running) = self.jobs.running.lock() {
            running.remove(&self.deployment);
        }
    }
}

/// Resolve an upstream and probe it with the deployment's health check — the
/// same two steps the autoscaler's static re-probe takes each tick.
async fn probe_peer(peer: &str, check: &crate::config::HealthCheck) -> bool {
    match tokio::net::lookup_host(peer).await {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => health::probe(addr, check).await,
            None => false,
        },
        Err(_) => false,
    }
}

/// Who app-lb is running as, for the "that directory isn't there" message —
/// which is very often a permissions or wrong-user problem, not a typo.
/// Enforce the retention caps after pushing a record for `deployment`.
///
/// Two passes, in this order and not the other: the deployment that just gained
/// a record is trimmed to [`HISTORY_PER_DEPLOYMENT`] first, so a busy deployment
/// evicts *its own* oldest job rather than somebody else's. Only then does the
/// global [`HISTORY_LIMIT`] apply, and by construction it almost never bites.
///
/// Oldest-first order is preserved, so `records` still reads newest-first.
fn trim_history(history: &mut VecDeque<JobRecord>, deployment: &str) {
    let mut mine = history.iter().filter(|r| r.deployment == deployment).count();
    if mine > HISTORY_PER_DEPLOYMENT {
        history.retain(|r| {
            if r.deployment != deployment || mine <= HISTORY_PER_DEPLOYMENT {
                return true;
            }
            mine -= 1;
            false
        });
    }
    while history.len() > HISTORY_LIMIT {
        history.pop_front();
    }
}

/// Whether a site's root still holds something worth serving.
///
/// The counterpart to probing a static deployment's upstreams: a build that
/// exits 0 but writes its output somewhere else leaves a directory that answers
/// every request with a 404, and "the commands succeeded" would call that a
/// successful deploy.
fn verify_site(spec: &crate::config::SiteSpec) -> Result<String, String> {
    let root = Path::new(&spec.root);
    if !root.is_dir() {
        return Err(format!(
            "site.root {} is not a directory on this host (app-lb runs as {})",
            spec.root,
            whoami()
        ));
    }
    let index = spec.index.trim();
    if index.is_empty() {
        // No index configured, so the site is a bag of files; the directory
        // existing is all there is to check.
        return Ok(format!("{} exists", spec.root));
    }
    if !root.join(index).is_file() {
        return Err(format!(
            "{index} is missing from {} — did the build write its output somewhere else?",
            spec.root
        ));
    }
    Ok(format!("{index} is in place"))
}

/// The user app-lb is running as, for the errors that are almost always a
/// permission problem wearing a different hat.
pub fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| format!("uid {}", unsafe { libc_getuid() }))
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(not(unix))]
unsafe fn libc_getuid() -> u32 {
    0
}

fn is_ssh_remote(repo: &str) -> bool {
    repo.starts_with("git@") || repo.starts_with("ssh://")
}

/// Find the Dockerfile and the context, as absolute paths inside `root`.
///
/// The canonicalized result is re-checked against the checkout: everything here
/// came from a repo, and a committed symlink is the one way a validated relative
/// path can still end up pointing at `/etc`.
fn locate_dockerfile(root: &Path, spec: &BuildSpec) -> Result<(PathBuf, PathBuf), String> {
    let root_real = root
        .canonicalize()
        .map_err(|e| format!("checkout {} is unreadable: {e}", root.display()))?;

    let context_root = match &spec.context {
        Some(c) => contained(&root_real, &root_real.join(c), "build.context")?,
        None => root_real.clone(),
    };

    let dockerfile = match &spec.dockerfile {
        Some(f) => {
            let candidate = root_real.join(f);
            // Existence first: a dangling or absent path should say what is
            // missing, not that it escaped the checkout.
            if !candidate.is_file() {
                return Err(format!(
                    "no Dockerfile at {f:?} in this checkout — the repo may have moved it, \
                     or `build.dockerfile` may be stale"
                ));
            }
            contained(&root_real, &candidate, "build.dockerfile")?
        }
        // The search matches on filename and `is_file()`, both of which follow
        // symlinks, so its result needs the same containment check as a path the
        // spec named.
        None => contained(
            &root_real,
            &find_dockerfile(&context_root)?,
            "the Dockerfile found in the checkout",
        )?,
    };

    // heyvm's own default, made explicit: the context is the Dockerfile's
    // directory unless the spec says otherwise.
    let context = match &spec.context {
        Some(_) => context_root,
        None => dockerfile
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root_real.clone()),
    };
    Ok((dockerfile, context))
}

/// Canonicalize and refuse anything that escaped `root`.
fn contained(root: &Path, path: &Path, what: &str) -> Result<PathBuf, String> {
    let real = path
        .canonicalize()
        .map_err(|e| format!("{what} {} is not in the checkout: {e}", path.display()))?;
    if real.starts_with(root) {
        Ok(real)
    } else {
        Err(format!(
            "{what} resolves to {} which is outside the checkout; a symlink in the repo \
             cannot be used to reach the host filesystem",
            real.display()
        ))
    }
}

/// Look for a Dockerfile: the context root first, then a bounded walk. Several
/// candidates is an error — picking one would make the deployed image depend on
/// directory iteration order.
fn find_dockerfile(context_root: &Path) -> Result<PathBuf, String> {
    let obvious = context_root.join("Dockerfile");
    if obvious.is_file() {
        return Ok(obvious);
    }

    let mut found = Vec::new();
    let mut frontier = vec![(context_root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = frontier.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if depth < SEARCH_DEPTH && !SKIP_DIRS.contains(&name.as_ref()) {
                    frontier.push((path, depth + 1));
                }
            } else if name == "Dockerfile" {
                found.push(path);
            }
        }
    }
    found.sort();

    match found.len() {
        0 => Err(format!(
            "no Dockerfile found within {SEARCH_DEPTH} directories of {}; set \
             `build.dockerfile` to its path in the repo",
            context_root.display()
        )),
        1 => Ok(found.into_iter().next().expect("len == 1")),
        _ => {
            let names: Vec<String> = found
                .iter()
                .map(|p| {
                    p.strip_prefix(context_root)
                        .unwrap_or(p)
                        .display()
                        .to_string()
                })
                .take(8)
                .collect();
            Err(format!(
                "found {} Dockerfiles ({}); set `build.dockerfile` to say which one builds \
                 this deployment",
                found.len(),
                names.join(", ")
            ))
        }
    }
}

/// A ref that is a full or abbreviated commit sha. Such a ref may need a full
/// fetch, and can be checked out by name once fetched.
fn looks_like_sha(r: &str) -> bool {
    r.len() >= 7 && r.len() <= 40 && r.chars().all(|c| c.is_ascii_hexdigit())
}

/// `<work_dir>/<deployment>` — the id is only constrained by the route table, so
/// it cannot be trusted as a path component.
fn sanitize_dir(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "_".into()
    } else {
        cleaned
    }
}

/// A command as a log line. Safe to print in full: credentials travel in the
/// environment precisely so that this never has to be redacted.
fn describe(cmd: &tokio::process::Command) -> String {
    let std_cmd = cmd.as_std();
    let mut out = std_cmd.get_program().to_string_lossy().into_owned();
    for arg in std_cmd.get_args() {
        out.push(' ');
        out.push_str(&arg.to_string_lossy());
    }
    out
}

fn new_job_id() -> String {
    let mut bytes = [0u8; 6];
    if openssl::rand::rand_bytes(&mut bytes).is_err() {
        // Only used for uniqueness within a 50-entry history.
        let n = now_secs();
        bytes.copy_from_slice(&n.to_le_bytes()[..6]);
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("job-{hex}")
}

#[cfg(test)]
mod retention {
    use super::*;

    fn push(history: &mut VecDeque<JobRecord>, deployment: &str, n: usize) {
        for i in 0..n {
            history.push_back(JobRecord::new(
                format!("{deployment}-{i}"),
                deployment.to_string(),
                JobKind::ImageBuild,
            ));
            trim_history(history, deployment);
        }
    }

    fn ids_for(history: &VecDeque<JobRecord>, deployment: &str) -> Vec<String> {
        history
            .iter()
            .filter(|r| r.deployment == deployment)
            .map(|r| r.id.clone())
            .collect()
    }

    #[test]
    fn a_deployment_keeps_its_most_recent_jobs_and_no_more() {
        let mut history = VecDeque::new();
        push(&mut history, "a", HISTORY_PER_DEPLOYMENT + 5);

        let ids = ids_for(&history, "a");
        assert_eq!(ids.len(), HISTORY_PER_DEPLOYMENT);
        assert_eq!(ids.last().unwrap(), &format!("a-{}", HISTORY_PER_DEPLOYMENT + 4));
        assert_eq!(ids.first().unwrap(), &"a-5".to_string(), "the oldest go first");
    }

    /// The reason the cap is per-deployment. Under a global cap, one deployment
    /// churning jobs would evict every other deployment's history — so the job
    /// you came to investigate is the one already gone.
    #[test]
    fn a_busy_deployment_evicts_only_its_own_history() {
        let mut history = VecDeque::new();
        push(&mut history, "quiet", 1);
        push(&mut history, "busy", HISTORY_PER_DEPLOYMENT * 3);

        assert_eq!(ids_for(&history, "quiet"), vec!["quiet-0"]);
        assert_eq!(ids_for(&history, "busy").len(), HISTORY_PER_DEPLOYMENT);
    }

    /// The fleet-wide ceiling still applies once enough deployments each hold
    /// recent jobs, and it evicts oldest-first across the whole history.
    #[test]
    fn the_global_ceiling_bounds_the_whole_fleet() {
        let mut history = VecDeque::new();
        // One job each from more deployments than the ceiling allows.
        for i in 0..HISTORY_LIMIT + 10 {
            let id = format!("d{i}");
            history.push_back(JobRecord::new(format!("{id}-0"), id.clone(), JobKind::ImageBuild));
            trim_history(&mut history, &id);
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.front().unwrap().deployment, "d10", "oldest evicted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_spec() -> BuildSpec {
        BuildSpec {
            repo: Some("https://example.com/acme/web.git".into()),
            store: None,
            source_ref: None,
            dockerfile: None,
            context: None,
            image_name: None,
            image_size_mb: None,
            auth: None,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("app-lb-jobs-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "FROM scratch\n").unwrap();
    }

    #[test]
    fn the_obvious_dockerfile_wins_over_a_search() {
        let dir = scratch("obvious");
        touch(&dir.join("Dockerfile"));
        touch(&dir.join("deploy/Dockerfile"));

        let (df, ctx) = locate_dockerfile(&dir, &build_spec()).unwrap();
        assert!(df.ends_with("Dockerfile"));
        assert_eq!(df.parent().unwrap(), ctx, "context defaults to its directory");
        assert_eq!(df, dir.canonicalize().unwrap().join("Dockerfile"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_single_nested_dockerfile_is_found() {
        let dir = scratch("nested");
        touch(&dir.join("docker/app/Dockerfile"));

        let (df, ctx) = locate_dockerfile(&dir, &build_spec()).unwrap();
        assert!(df.ends_with("docker/app/Dockerfile"), "{}", df.display());
        assert!(ctx.ends_with("docker/app"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ambiguity_is_reported_rather_than_guessed() {
        let dir = scratch("ambiguous");
        touch(&dir.join("api/Dockerfile"));
        touch(&dir.join("web/Dockerfile"));

        let err = locate_dockerfile(&dir, &build_spec()).unwrap_err();
        assert!(err.contains("found 2 Dockerfiles"), "{err}");
        assert!(err.contains("build.dockerfile"), "{err}");

        // Naming one resolves it.
        let spec = BuildSpec {
            dockerfile: Some("web/Dockerfile".into()),
            ..build_spec()
        };
        let (df, _) = locate_dockerfile(&dir, &spec).unwrap();
        assert!(df.ends_with("web/Dockerfile"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn vendored_trees_are_not_searched() {
        let dir = scratch("skips");
        touch(&dir.join("node_modules/pkg/Dockerfile"));
        touch(&dir.join(".git/Dockerfile"));
        touch(&dir.join("svc/Dockerfile"));

        let (df, _) = locate_dockerfile(&dir, &build_spec()).unwrap();
        assert!(df.ends_with("svc/Dockerfile"), "{}", df.display());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_dockerfile_says_which_knob_to_set() {
        let dir = scratch("missing");
        let err = locate_dockerfile(&dir, &build_spec()).unwrap_err();
        assert!(err.contains("no Dockerfile found"), "{err}");

        let named = BuildSpec {
            dockerfile: Some("deploy/Dockerfile".into()),
            ..build_spec()
        };
        let err = locate_dockerfile(&dir, &named).unwrap_err();
        assert!(err.contains("no Dockerfile at"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: `build.dockerfile` is validated as a relative path, but a
    /// symlink committed to the repo can still resolve outside the checkout.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_checkout_is_refused() {
        let dir = scratch("symlink");
        let outside = scratch("symlink-target");
        touch(&outside.join("Dockerfile"));
        std::os::unix::fs::symlink(outside.join("Dockerfile"), dir.join("Dockerfile")).unwrap();

        match locate_dockerfile(&dir, &build_spec()) {
            Err(e) => assert!(
                e.contains("outside the checkout") || e.contains("not in the checkout"),
                "{e}"
            ),
            Ok((df, _)) => panic!("accepted a symlink to {}", df.display()),
        }

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn a_context_scopes_the_search() {
        let dir = scratch("context");
        touch(&dir.join("services/api/Dockerfile"));
        touch(&dir.join("services/web/Dockerfile"));

        // Two candidates at the root...
        assert!(locate_dockerfile(&dir, &build_spec()).is_err());
        // ...one within the context.
        let spec = BuildSpec {
            context: Some("services/api".into()),
            ..build_spec()
        };
        let (df, ctx) = locate_dockerfile(&dir, &spec).unwrap();
        assert!(df.ends_with("services/api/Dockerfile"));
        assert!(ctx.ends_with("services/api"), "an explicit context is kept");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shas_are_told_apart_from_branch_names() {
        assert!(looks_like_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(looks_like_sha("0123456"));
        assert!(!looks_like_sha("main"), "a branch, even if short");
        assert!(!looks_like_sha("012345"), "too short to be a useful sha");
        assert!(!looks_like_sha("release-1"));
    }

    #[test]
    fn a_deployment_id_cannot_pick_the_checkout_directory() {
        assert_eq!(sanitize_dir("web"), "web");
        assert_eq!(sanitize_dir("../../etc"), "_.._etc");
        assert_eq!(sanitize_dir("a/b"), "a_b");
        assert_eq!(sanitize_dir(".."), "_");
    }

    #[test]
    fn a_log_keeps_the_tail_not_the_head() {
        let mut r = JobRecord::new("job-1".into(), "web".into(), JobKind::ImageBuild);
        for i in 0..(LOG_LIMIT + 10) {
            r.push_log(format!("line {i}"));
        }
        assert_eq!(r.log.len(), LOG_LIMIT);
        assert_eq!(r.log.last().unwrap(), &format!("line {}", LOG_LIMIT + 9));
        assert_eq!(r.log.first().unwrap(), "line 10");
    }

    #[test]
    fn job_ids_are_distinct() {
        let a = new_job_id();
        assert!(a.starts_with("job-"), "{a}");
        assert_ne!(a, new_job_id());
    }

    #[test]
    fn a_record_only_serializes_the_fields_its_kind_uses() {
        let build = JobRecord::new("job-1".into(), "web".into(), JobKind::ImageBuild);
        let json = serde_json::to_string(&build).unwrap();
        assert!(json.contains(r#""kind":"image-build""#), "{json}");
        assert!(!json.contains("working_dir"), "no host-update fields: {json}");
        assert!(!json.contains("commands_total"), "{json}");

        let mut update = JobRecord::new("job-2".into(), "obs".into(), JobKind::HostUpdate);
        update.working_dir = Some("/srv/app".into());
        update.commands_total = Some(3);
        update.commands_run = Some(1);
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains(r#""kind":"host-update""#), "{json}");
        assert!(json.contains(r#""working_dir":"/srv/app""#), "{json}");
        assert!(!json.contains("dockerfile"), "no image fields: {json}");
        assert!(!json.contains("rolled_out"), "{json}");

        let mut pull = JobRecord::new("job-3".into(), "web".into(), JobKind::ArtifactPull);
        pull.store = Some("http://127.0.0.1:8080".into());
        pull.artifact_ref = Some("debian-hermes".into());
        pull.digest = Some("c74abee2ce84".into());
        pull.bytes = Some(609_222_656);
        let json = serde_json::to_string(&pull).unwrap();
        assert!(json.contains(r#""kind":"artifact-pull""#), "{json}");
        assert!(json.contains(r#""artifact":"debian-hermes""#), "{json}");
        assert!(json.contains(r#""digest":"c74abee2ce84""#), "{json}");
        // A pull has no repo and no commit; those belong to the other source.
        assert!(!json.contains("commit"), "no build fields: {json}");
        assert!(!json.contains("working_dir"), "no host-update fields: {json}");
        // And a pull that fetched nothing does not claim to have reused
        // anything until it did.
        assert!(!json.contains("reused"), "{json}");
    }

    #[test]
    fn a_reused_image_is_reported_as_zero_bytes_rather_than_omitted() {
        // `bytes: 0` and `reused: true` together are the difference between
        // "the fetch was skipped" and "the fetch never ran", which is exactly
        // what somebody looking at a suspiciously fast pull wants to know.
        let mut pull = JobRecord::new("job-4".into(), "web".into(), JobKind::ArtifactPull);
        pull.bytes = Some(0);
        pull.reused = true;
        let json = serde_json::to_string(&pull).unwrap();
        assert!(json.contains(r#""bytes":0"#), "{json}");
        assert!(json.contains(r#""reused":true"#), "{json}");
    }

    #[test]
    fn each_job_kind_is_refused_on_the_backends_it_does_not_describe() {
        // The message has to say what to use instead: somebody who ran `build`
        // on a static deployment wants `update`, and vice versa.
        let build_on_static = StartError::WrongKind {
            id: "obs".into(),
            kind: JobKind::ImageBuild,
            backend: Backend::Upstreams,
        }
        .to_string();
        assert!(build_on_static.contains("static"), "{build_on_static}");
        assert!(build_on_static.contains("update"), "{build_on_static}");

        let pull_on_static = StartError::WrongKind {
            id: "obs".into(),
            kind: JobKind::ArtifactPull,
            backend: Backend::Upstreams,
        }
        .to_string();
        assert!(pull_on_static.contains("static"), "{pull_on_static}");
        assert!(pull_on_static.contains("update"), "{pull_on_static}");

        let update_on_managed = StartError::WrongKind {
            id: "web".into(),
            kind: JobKind::HostUpdate,
            backend: Backend::Vm,
        }
        .to_string();
        assert!(update_on_managed.contains("managed"), "{update_on_managed}");
        assert!(update_on_managed.contains("build"), "{update_on_managed}");

        // A site is neither of the other two, and the message that used to be
        // reached here called it "static (proxy_pass)" — which is wrong, and
        // sends somebody looking for upstreams they do not have.
        let build_on_site = StartError::WrongKind {
            id: "docs".into(),
            kind: JobKind::ImageBuild,
            backend: Backend::Site,
        }
        .to_string();
        assert!(build_on_site.contains("serves files off disk"), "{build_on_site}");
        assert!(!build_on_site.contains("proxy_pass"), "{build_on_site}");
        // Both of a site's deploy paths, since either could be what was meant.
        assert!(build_on_site.contains("pull"), "{build_on_site}");
        assert!(build_on_site.contains("update"), "{build_on_site}");
    }

    /// The table that replaced "is this deployment managed?", which could not
    /// express a job kind applying to two backends or a backend accepting two
    /// job kinds — and both are now true.
    #[test]
    fn a_pull_applies_to_a_vm_and_a_site_but_never_to_upstreams() {
        for (kind, expected) in [
            (JobKind::ImageBuild, [true, false, false]),
            (JobKind::ArtifactPull, [true, false, true]),
            (JobKind::HostUpdate, [false, true, true]),
        ] {
            for (backend, want) in
                [Backend::Vm, Backend::Upstreams, Backend::Site].into_iter().zip(expected)
            {
                assert_eq!(
                    kind.applies_to(backend),
                    want,
                    "{kind:?} on {backend:?}",
                );
            }
        }
    }

    /// A site pull records what it unpacked; a rootfs pull has no such thing and
    /// must not carry the fields as nulls.
    #[test]
    fn a_site_pull_reports_its_root_and_file_count() {
        let mut pull = JobRecord::new("job-5".into(), "docs".into(), JobKind::ArtifactPull);
        pull.site_root = Some("/srv/docs/public".into());
        pull.files = Some(412);
        let json = serde_json::to_string(&pull).unwrap();
        assert!(json.contains(r#""site_root":"/srv/docs/public""#), "{json}");
        assert!(json.contains(r#""files":412"#), "{json}");

        let rootfs = JobRecord::new("job-6".into(), "web".into(), JobKind::ArtifactPull);
        let json = serde_json::to_string(&rootfs).unwrap();
        assert!(!json.contains("site_root"), "no site fields on a rootfs pull: {json}");
        assert!(!json.contains("files"), "{json}");
    }
}
