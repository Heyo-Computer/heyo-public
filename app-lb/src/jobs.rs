//! Deploy jobs: the two ways a deployment's code gets updated.
//!
//! Both kinds run as an async task on the app-lb host, one at a time per
//! deployment, and are polled through the same records:
//!
//! * **`image-build`** (managed deployments) — `git fetch` a repo, hand its
//!   Dockerfile to `heyvm mvm build`, then rewrite `vm.image` to the image that
//!   produced, which recycles the pool onto it. heyvm has no build API, so this
//!   is app-lb driving child processes.
//! * **`host-update`** (static/`proxy_pass` deployments) — run a list of
//!   commands in a working directory on this host, then re-probe the upstreams
//!   to prove the service came back. A static deployment's backend is a process
//!   somebody else runs; this is the "somebody else" being app-lb.
//!
//! The two are deliberately exclusive, and each is rejected on the other kind of
//! deployment: there is no image to build for a `proxy_pass` upstream, and a
//! working directory on the host has nothing to do with a microVM's rootfs.
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

use crate::autoscale::Autoscaler;
use crate::config::{BuildSpec, UpdateSpec};
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

/// How many jobs are remembered. Records are in memory only: a job is a
/// transient event, and the durable outcome of a successful one is either the
/// `image` in the persisted spec or the state of the host itself.
const HISTORY_LIMIT: usize = 50;
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
    /// Run commands in a working directory on this host (static deployments).
    HostUpdate,
}

impl JobKind {
    fn label(self) -> &'static str {
        match self {
            Self::ImageBuild => "build",
            Self::HostUpdate => "update",
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
    /// Whether `vm.image` was updated and the pool told to roll.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rolled_out: bool,

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
    /// The deployment is the wrong kind for this job.
    WrongKind {
        id: String,
        kind: JobKind,
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
            } => write!(
                f,
                "deployment {id:?} is static (proxy_pass); it has no guest image to build. \
                 Use `update` to run commands on the host instead"
            ),
            Self::WrongKind {
                id,
                kind: JobKind::HostUpdate,
            } => write!(
                f,
                "deployment {id:?} is a managed VM pool, not a static (proxy_pass) one; its \
                 backends are microVMs, not host processes. Use `build` to rebuild its image"
            ),
            Self::NoSpec {
                id,
                kind: JobKind::ImageBuild,
            } => write!(
                f,
                "deployment {id:?} has no `build` block — set `build.repo` (and optionally \
                 `build.dockerfile`) on the spec first"
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

impl std::error::Error for StartError {}

pub struct JobConfig {
    /// Parent of the per-deployment checkouts.
    pub work_dir: PathBuf,
    pub heyvm_bin: String,
    pub git_bin: String,
    /// Shell that host-update commands are run through.
    pub shell: String,
    pub timeout: Duration,
    /// `HOME` for child processes, when app-lb and heyvmd run as different users.
    pub home: Option<String>,
}

pub struct Jobs {
    cfg: JobConfig,
    registry: Arc<Registry>,
    autoscaler: Arc<Autoscaler>,
    secrets: Arc<SecretStore>,
    history: Mutex<VecDeque<JobRecord>>,
    /// Deployment ids with a job in flight.
    running: Mutex<HashSet<String>>,
}

impl Jobs {
    pub fn new(
        cfg: JobConfig,
        registry: Arc<Registry>,
        autoscaler: Arc<Autoscaler>,
        secrets: Arc<SecretStore>,
    ) -> Self {
        Self {
            cfg,
            registry,
            autoscaler,
            secrets,
            history: Mutex::new(VecDeque::new()),
            running: Mutex::new(HashSet::new()),
        }
    }

    /// Jobs newest-first, optionally for one deployment.
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
            spec.git_ref = Some(r);
            // The stored spec was validated on registration; an override was not.
            let probe = crate::config::DeploymentSpec {
                build: Some(spec.clone()),
                ..deployment.spec.clone()
            };
            probe.validate().map_err(|e| StartError::BadRef(e.to_string()))?;
        }

        let git_ref = spec.git_ref.clone();
        let repo = spec.repo.clone();
        self.spawn(deployment_id, JobKind::ImageBuild, move |r| {
            r.repo = Some(repo);
            r.git_ref = git_ref;
        }, move |jobs, job_id, deployment_id| async move {
            jobs.run_build(&job_id, &deployment_id, &spec).await
        })
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
        let wants_static = kind == JobKind::HostUpdate;
        if deployment.spec.is_static() != wants_static {
            return Err(StartError::WrongKind {
                id: deployment_id.to_string(),
                kind,
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
            while history.len() > HISTORY_LIMIT {
                history.pop_front();
            }
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

    fn log(&self, job_id: &str, line: impl Into<String>) {
        self.update_record(job_id, |r| r.push_log(line));
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
    async fn run_build(
        &self,
        job_id: &str,
        deployment_id: &str,
        spec: &BuildSpec,
    ) -> Result<String, String> {
        let checkout = self.cfg.work_dir.join(sanitize_dir(deployment_id));
        crate::tls::create_dir_private(&checkout)
            .map_err(|e| format!("could not create {}: {e}", checkout.display()))?;

        // -- checkout ------------------------------------------------------
        let token = self.git_token(spec.auth.as_ref())?;
        // Said once per build rather than once per git command: an ssh remote
        // authenticates with the host's key material, so a secret here is a
        // credential somebody thinks is in use and isn't.
        if token.is_some() && is_ssh_remote(&spec.repo) {
            let note = "build.auth is set on an ssh remote; git authenticates with the host's \
                        key material and the secret is ignored";
            tracing::warn!(repo = %spec.repo, "{note}");
            self.log(job_id, note);
        }

        let commit = self.checkout(job_id, &checkout, spec, token.as_ref()).await?;
        self.update_record(job_id, |r| r.commit = Some(commit.clone()));

        // -- locate the Dockerfile -----------------------------------------
        let (dockerfile, context) = locate_dockerfile(&checkout, spec)?;
        let shown = dockerfile
            .strip_prefix(&checkout)
            .unwrap_or(&dockerfile)
            .display()
            .to_string();
        self.update_record(job_id, |r| r.dockerfile = Some(shown.clone()));
        self.log(job_id, format!("using {shown} (context {})", context.display()));

        // -- build ----------------------------------------------------------
        let image = spec.image_for(deployment_id, &commit);
        self.update_record(job_id, |r| r.image = Some(image.clone()));

        let mut cmd = tokio::process::Command::new(&self.cfg.heyvm_bin);
        cmd.arg("mvm")
            .arg("build")
            .arg("-f")
            .arg(&dockerfile)
            .arg("-c")
            .arg(&context)
            .arg("-n")
            .arg(&image)
            // Never upload: the image is consumed by the daemon on this host,
            // and a cloud push would need credentials app-lb does not have.
            .arg("--local-only")
            .current_dir(&context);
        if let Some(mb) = spec.image_size_mb {
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

    /// Fetch and check out, returning the resolved commit.
    async fn checkout(
        &self,
        job_id: &str,
        dir: &Path,
        spec: &BuildSpec,
        token: Option<&(String, String)>,
    ) -> Result<String, String> {
        // `git init` on an existing repo just reinitializes it, so the checkout
        // directory survives between builds and a rebuild is a shallow fetch
        // rather than a fresh clone.
        let mut init = self.git(token);
        init.arg("init").arg("-q").arg(dir);
        self.step(job_id, "git", init, self.cfg.timeout).await?;

        let refspec = spec.git_ref.clone().unwrap_or_else(|| "HEAD".into());
        let fetch = |depth: Option<&str>| {
            let mut cmd = self.git(token);
            cmd.arg("-C").arg(dir).arg("fetch").arg("--no-tags").arg("--force");
            if let Some(d) = depth {
                cmd.arg("--depth").arg(d);
            }
            cmd.arg(&spec.repo).arg(&refspec);
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
            full.arg("-C").arg(dir).arg("fetch").arg("--no-tags").arg("--force").arg(&spec.repo);
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
            .arg(if spec.git_ref.is_some() && looks_like_sha(&refspec) {
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
        if let Err(e) = self.registry.persist() {
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
fn whoami() -> String {
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
mod tests {
    use super::*;

    fn build_spec() -> BuildSpec {
        BuildSpec {
            repo: "https://example.com/acme/web.git".into(),
            git_ref: None,
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
    }

    #[test]
    fn each_job_kind_is_refused_on_the_other_kind_of_deployment() {
        // The message has to say what to use instead: somebody who ran `build`
        // on a static deployment wants `update`, and vice versa.
        let build_on_static = StartError::WrongKind {
            id: "obs".into(),
            kind: JobKind::ImageBuild,
        }
        .to_string();
        assert!(build_on_static.contains("static"), "{build_on_static}");
        assert!(build_on_static.contains("update"), "{build_on_static}");

        let update_on_managed = StartError::WrongKind {
            id: "web".into(),
            kind: JobKind::HostUpdate,
        }
        .to_string();
        assert!(update_on_managed.contains("managed"), "{update_on_managed}");
        assert!(update_on_managed.contains("build"), "{update_on_managed}");
    }
}
