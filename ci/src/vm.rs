//! The heyvm boundary: creating a VM on a runner and running steps in it.
//!
//! Everything that knows about sandboxes lives here so the traps are in one
//! place. Four are worth stating up front, because every one of them fails
//! silently. The first three are queue-fn's findings
//! (`queue-fn/src/vm.rs`), re-verified against this SDK version; the fourth is
//! ours and is the reason this module does not use the SDK's exec at all.
//!
//! 1. **`wait_for_ready` returning `Ok` does not mean the VM is usable.** Its
//!    match has a `_ => return Ok(info)` arm (`sdk-rs/src/sandbox.rs:182`), so
//!    `Stopped`, `Paused` and `ColdStored` all come back `Ok`. Only `Running`
//!    can run anything. [`Vm::ensure_running`] is the check.
//!
//! 2. **The firecracker serial path returns an empty `stderr`.** The daemon runs
//!    `(cmd) 2>&1` and builds its response with stderr empty
//!    (`mvm-ctrl/src/driver/firecracker.rs:1752`). Reading `stdout` alone loses
//!    everything the command wrote to stderr, so [`ExecOutput::combined`] folds
//!    the `output` field back in.
//!
//! 3. **Two concurrent execs against one sandbox do not queue.** The second gets
//!    `SandboxNotFound`, because the first holds the handle out of the manager's
//!    map for its duration (`mvm-ctrl/src/sandbox.rs:3648`). [`Vms`] keeps a
//!    per-sandbox lock so that is impossible from this process.
//!
//! 4. **The SDK's `Commands::run` cannot raise the guest timeout.** It posts to
//!    the cloud-compat route `POST /sandbox/{id}/exec` with a body of only
//!    `{command, cwd, env}` (`sdk-rs/src/commands.rs:42`) — `CommandRunOptions::timeout`
//!    sets the *HTTP client's* timeout and never reaches the guest. The
//!    firecracker serial path then caps the command at 30 seconds
//!    (`mvm-ctrl/src/driver/firecracker.rs:2178`). No CI step survives that, so
//!    this module drives the daemon's own async route instead:
//!    `POST /sandboxes/{id}/exec-operations`, which does take `timeoutSecs`.
//!
//! ## Why the async exec route is the right primitive anyway
//!
//! It is **idempotent by `operationId` and persisted to disk**, and a replay
//! with a different command for the same id is rejected
//! (`mvm-ctrl/src/api.rs:5164`). That makes a JetStream redelivery safe: re-posting
//! a step reattaches to the operation already running instead of starting the
//! build twice. A `queued` record that has not moved in 30s is respawned by the
//! daemon on re-POST, so a daemon restart mid-step self-heals.

use heyo_sdk::{
    HeyoClientOptions, HeyoError, RequestOptions, Sandbox, SandboxCreateOptions, SandboxDriver,
    SandboxInfo, SandboxSize, SandboxStatus,
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// How long the daemon may sit in `queued` before a re-POST respawns it
/// (`EXEC_OPERATION_QUEUED_RESTART_AFTER_SECS`, mvm-ctrl/src/api.rs:21). We poll
/// well under this so a stuck operation is noticed rather than waited out.
const POLL_MIN: Duration = Duration::from_millis(250);
const POLL_MAX: Duration = Duration::from_secs(2);

/// Slack between the timeout we hand the guest and the deadline we stop polling
/// at. The daemon's own timeout must fire first, so that a slow step comes back
/// as a `failed` record naming the timeout rather than as our own give-up, which
/// would leave the command running with nobody reading it.
const POLL_SLACK: Duration = Duration::from_secs(30);

/// Base64 bytes per upload exec. See [`Vm::upload_bytes`] for where this number
/// comes from — it is measured against a real guest, not chosen.
const UPLOAD_CHUNK: usize = 24 * 1024;

/// One line of a VM's own log, as `GET /sandboxes/{id}/logs` returns it.
///
/// Re-declared rather than imported: the daemon's `LogEntry` lives in
/// `mvm-ctrl`, which this crate deliberately does not depend on. Every field is
/// defaulted so a daemon that adds or drops one does not break the parse of the
/// rest — a VM log is diagnostic, and half of it beats an error.
#[derive(Debug, Clone, Deserialize)]
struct LogsResponse {
    #[serde(default)]
    logs: Vec<LogLine>,
    #[serde(default)]
    total: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct LogLine {
    /// `stdout`, `stderr` or `console`.
    #[serde(default = "unknown_source")]
    source: String,
    #[serde(default)]
    message: String,
}

fn unknown_source() -> String {
    "?".to_string()
}

/// `vm.build` — the Dockerfile a job's image is made from.
///
/// Both paths are repository-relative and validated like `cache_key_files`: the
/// build runs against a tree somebody submitted, so a path that escapes it is
/// refused when the workflow is parsed rather than when a build reads a file.
///
/// The resulting image name is *derived*, never written by the author — it is
/// the content hash of the Dockerfile and its context (see
/// [`crate::image::Dockerfile::fingerprint`]). That is what makes "reused until
/// cache busted" fall out: identical inputs name an image the host already has,
/// and any change names one it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuild {
    pub dockerfile: String,
    /// Defaults to the Dockerfile's own directory, as `docker build` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Rootfs size in MB. Absent means the daemon auto-sizes from the exported
    /// tree (× 1.2 + 64 MB) — right for an image that only ever reads itself,
    /// too tight for one whose workload writes into the rootfs at runtime, the
    /// way a crate registry cache under `CARGO_HOME` does. Part of the image
    /// fingerprint: a different ext4 size is a different image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mb: Option<u64>,
}

impl ImageBuild {
    /// The context directory, defaulted the way `docker build -f` does.
    pub fn context_dir(&self) -> &str {
        match self.context.as_deref() {
            Some(c) => c,
            None => match self.dockerfile.rsplit_once('/') {
                Some((dir, _)) => dir,
                None => ".",
            },
        }
    }
}

/// The `vm:` block of a workflow job.
///
/// Mirrors the subset of `SandboxCreateOptions` that makes sense to declare in a
/// workflow. `cache_key_files` is not passed to the daemon at all — it feeds the
/// pool fingerprint (see `pool`), and lives here because it is part of what the
/// author writes under `vm:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmSpec {
    pub driver: SandboxDriver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Build the image from a Dockerfile in the submitted tree instead of
    /// naming one the host already has.
    ///
    /// Mutually exclusive with `image`, and refused at parse time when both are
    /// set: the name is derived from the Dockerfile, so an author-supplied one
    /// could only disagree with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<ImageBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_class: Option<SandboxSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_size_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// Baked into the fingerprint, so changing one busts the pool.
    ///
    /// `BTreeMap` rather than `HashMap` because the fingerprint is a hash of the
    /// serialized spec, and a `HashMap`'s iteration order would make it differ
    /// between processes — every restart would rebuild every VM.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_hooks: Vec<String>,
    /// Repo-relative files whose contents bust the pool when they change.
    /// Consumed by the fingerprint, never sent to the daemon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_key_files: Vec<String>,
    /// Whether a finished job's VM may be handed to the next job with the same
    /// fingerprint.
    #[serde(default = "default_reuse")]
    pub reuse: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

fn default_reuse() -> bool {
    true
}

impl VmSpec {
    pub fn validate(&self) -> Result<(), VmSpecError> {
        // Libvirt is refused for the same reason queue-fn refuses it: the pool
        // depends on per-VM rootfs cloning and on the exec paths that only the
        // tap-networked backends implement well here. Refusing at parse time
        // means the author sees it when they write the workflow, not when a job
        // has already been queued against a runner.
        if self.driver == SandboxDriver::Libvirt {
            return Err(VmSpecError::UnsupportedDriver);
        }
        // "Both are set" is an authoring mistake with its own fix, so it is
        // stated separately from everything else.
        if let Some(build) = &self.build {
            if self.image.is_some() {
                return Err(VmSpecError::ImageAndBuild);
            }
            for path in [build.dockerfile.as_str(), build.context_dir()] {
                if path.starts_with('/') || path.split('/').any(|s| s == "..") {
                    return Err(VmSpecError::EscapingBuildPath(path.to_string()));
                }
            }
            if build.dockerfile.trim().is_empty() {
                return Err(VmSpecError::EmptyDockerfilePath);
            }
            if build.size_mb == Some(0) {
                return Err(VmSpecError::ZeroImageSize);
            }
        }
        if let Some(d) = self.disk_size_gb
            && d == 0
        {
            return Err(VmSpecError::ZeroDisk);
        }
        if let Some(t) = self.ttl_seconds
            && t == 0
        {
            return Err(VmSpecError::ZeroTtl);
        }
        for key in self.env_vars.keys() {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(VmSpecError::InvalidEnvKey(key.clone()));
            }
        }
        for path in &self.cache_key_files {
            if path.starts_with('/') || path.split('/').any(|s| s == "..") {
                return Err(VmSpecError::EscapingCacheKeyFile(path.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum VmSpecError {
    UnsupportedDriver,
    ZeroDisk,
    ZeroTtl,
    InvalidEnvKey(String),
    EscapingCacheKeyFile(String),
    ImageAndBuild,
    EscapingBuildPath(String),
    EmptyDockerfilePath,
    ZeroImageSize,
}

impl fmt::Display for VmSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDriver => write!(
                f,
                "vm.driver must be `firecracker` or `kvm`; `libvirt` is not supported \
                 for CI VMs"
            ),
            Self::ZeroDisk => write!(f, "vm.disk_size_gb must be greater than zero"),
            Self::ZeroTtl => write!(f, "vm.ttl_seconds must be greater than zero"),
            Self::InvalidEnvKey(k) => write!(
                f,
                "vm.env_vars key {k:?} is not a shell-safe name; use only letters, \
                 digits and underscores"
            ),
            Self::EscapingCacheKeyFile(p) => write!(
                f,
                "vm.cache_key_files entry {p:?} must be a relative path inside the \
                 repository, with no `..` segments"
            ),
            Self::ImageAndBuild => write!(
                f,
                "vm.image and vm.build cannot both be set: a built image's name is \
                 the hash of its Dockerfile and context, so naming one as well \
                 could only disagree with it. Drop `image:` to build, or drop \
                 `build:` to use an image the host already has."
            ),
            Self::EscapingBuildPath(p) => write!(
                f,
                "vm.build path {p:?} must be a relative path inside the repository, \
                 with no `..` segments"
            ),
            Self::EmptyDockerfilePath => {
                write!(f, "vm.build.dockerfile must name a file in the repository")
            }
            Self::ZeroImageSize => write!(f, "vm.build.size_mb must be greater than zero"),
        }
    }
}

impl std::error::Error for VmSpecError {}

/// What a step produced.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// The daemon's combined stream. On the firecracker serial path this is the
    /// only field that carries everything.
    pub output: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl ExecOutput {
    /// The text to show and store for this step.
    ///
    /// Prefers `output`, because on the firecracker serial path `stderr` is
    /// always empty and `stdout` alone silently loses every diagnostic a build
    /// tool wrote — which is most of what anyone reads a failed build for. Falls
    /// back to concatenation for backends that populate the split streams and
    /// leave `output` empty.
    pub fn combined(&self) -> String {
        if !self.output.is_empty() {
            return self.output.clone();
        }
        let mut s = self.stdout.clone();
        if !self.stderr.is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s
    }

    /// A non-zero exit is a failed step but a successful exec — the command ran.
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// `POST /sandboxes/{id}/exec-operations` request body.
///
/// camelCase, matching `StartExecOperationRequest` (mvm-ctrl/src/api.rs:5074).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartExecOperation<'a> {
    operation_id: &'a str,
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

/// The daemon's operation record.
///
/// **The casing is mixed and that is not a mistake in this struct.** The
/// envelope is camelCase (`ExecOperationRecord` carries `rename_all`), but the
/// nested `result` is `ExecuteCommandResponse`, which carries no rename at all
/// (mvm-ctrl/src/models.rs:1058) and so serializes `exit_code` in snake_case.
/// Verified against records the daemon wrote to `~/.heyo/exec-operations`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecOperationRecord {
    status: String,
    #[serde(default)]
    result: Option<ExecResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExecResult {
    #[serde(default)]
    output: String,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    exit_code: i32,
}

impl ExecOperationRecord {
    /// `queued` and `running` are in-flight; `completed` and `failed` are
    /// terminal. `completed` means the command *ran* — its exit code may still
    /// be non-zero. `failed` means the daemon could not run it at all.
    fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed")
    }
}

/// Mints operation ids, and owns the per-sandbox exec lock.
#[derive(Default)]
pub struct Vms {
    /// One lock per sandbox id, so trap 3 is impossible from this process
    /// regardless of which task holds a [`Vm`].
    ///
    /// The pool is the outer guarantee — it hands a sandbox to one job at a
    /// time — but that guarantee lives in a different module and covers only
    /// jobs. TTL renewal, teardown and a step run from the same process too.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Vms {
    pub fn new() -> Self {
        Self::default()
    }

    async fn lock_for(&self, sandbox_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(sandbox_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Adopt an existing sandbox on a runner.
    ///
    /// `options` come from [`crate::runners::Runners::options_for`] and point at
    /// that runner's tunnel. Issues no network call.
    pub async fn open(
        &self,
        options: HeyoClientOptions,
        sandbox_id: String,
    ) -> Result<Vm, VmError> {
        let sandbox =
            Sandbox::connect(sandbox_id.clone(), options).map_err(|e| VmError::Daemon {
                sandbox: sandbox_id.clone(),
                what: "connecting to the sandbox",
                source: e,
            })?;
        let lock = self.lock_for(&sandbox_id).await;
        Ok(Vm {
            sandbox,
            id: sandbox_id,
            lock,
        })
    }

    /// Create a VM on a runner and wait for it to be genuinely runnable.
    pub async fn create(
        &self,
        options: HeyoClientOptions,
        name: &str,
        spec: &VmSpec,
        ttl: Duration,
        boot_timeout: Duration,
    ) -> Result<Vm, VmError> {
        let opts = SandboxCreateOptions {
            name: Some(name.to_string()),
            driver: Some(spec.driver),
            image: spec.image.clone(),
            size_class: spec.size_class,
            disk_size_gb: spec.disk_size_gb,
            working_directory: spec.working_directory.clone(),
            env_vars: (!spec.env_vars.is_empty()).then(|| {
                spec.env_vars
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            }),
            setup_hooks: (!spec.setup_hooks.is_empty()).then(|| spec.setup_hooks.clone()),
            ttl_seconds: Some(spec.ttl_seconds.unwrap_or(ttl.as_secs())),
            wait_for_ready: Some(boot_timeout),
            ..Default::default()
        };

        // Goes to the daemon's cloud-compat `POST /sandbox-deploy`, which renames
        // `driver` to the native `backend_type` and passes `size_class`,
        // `setup_hooks`, `disk_size_gb`, `working_directory`, `env_vars` and
        // `ttl_seconds` straight through (mvm-ctrl/src/api.rs:2449). So the whole
        // `vm:` block lands, and none of it is silently defaulted.
        let sandbox = Sandbox::create(opts, options)
            .await
            .map_err(|e| VmError::Create {
                name: name.to_string(),
                source: e,
            })?;

        let id = sandbox.sandbox_id().to_string();
        let lock = self.lock_for(&id).await;
        let vm = Vm { sandbox, id, lock };
        // `create` already waited, but waiting is not the same as running — see
        // trap 1. Assert before handing the VM to a job.
        vm.ensure_running(boot_timeout).await?;
        Ok(vm)
    }
}

/// One VM on one runner.
pub struct Vm {
    sandbox: Sandbox,
    id: String,
    lock: Arc<Mutex<()>>,
}

impl Vm {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn info(&self) -> Result<SandboxInfo, VmError> {
        self.sandbox.info().await.map_err(|e| VmError::Daemon {
            sandbox: self.id.clone(),
            what: "reading sandbox info",
            source: e,
        })
    }

    /// Make sure the VM is `Running`, starting it if it is merely stopped.
    ///
    /// This is the check trap 1 requires. A pooled VM that was stopped between
    /// jobs is a normal, recoverable state; `Failed` and `ColdStored` are not,
    /// and are reported rather than retried into a timeout.
    pub async fn ensure_running(&self, timeout: Duration) -> Result<(), VmError> {
        let info = self.info().await?;
        match info.status {
            SandboxStatus::Running => return Ok(()),
            SandboxStatus::Stopped | SandboxStatus::Paused => {}
            other => {
                return Err(VmError::NotRunnable {
                    sandbox: self.id.clone(),
                    status: format!("{other:?}"),
                    reason: info.error_message.clone(),
                });
            }
        }

        // Serialized: `start` takes the handle out of the manager's map exactly
        // as `execute` does.
        {
            let _guard = self.lock.lock().await;
            self.sandbox.start().await.map_err(|e| VmError::Daemon {
                sandbox: self.id.clone(),
                what: "starting the sandbox",
                source: e,
            })?;
        }

        let info = self
            .sandbox
            .wait_for_ready(timeout)
            .await
            .map_err(|e| VmError::Daemon {
                sandbox: self.id.clone(),
                what: "waiting for the sandbox to start",
                source: e,
            })?;
        if info.status != SandboxStatus::Running {
            return Err(VmError::NotRunnable {
                sandbox: self.id.clone(),
                status: format!("{:?}", info.status),
                reason: info.error_message,
            });
        }
        Ok(())
    }

    /// Run one step and wait for it to finish.
    ///
    /// `operation_id` makes this idempotent: re-calling with the same id and the
    /// same command reattaches to the operation already in flight rather than
    /// running the step twice. That is what makes a queue redelivery safe.
    pub async fn exec(
        &self,
        operation_id: &str,
        command: &str,
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<ExecOutput, VmError> {
        if !valid_operation_id(operation_id) {
            return Err(VmError::BadOperationId(operation_id.to_string()));
        }

        // Held for the whole exec, including the poll loop: the daemon holds the
        // handle for the command's duration, so a second exec started while this
        // one polls would fail with `SandboxNotFound` (trap 3).
        let _guard = self.lock.lock().await;

        let body = StartExecOperation {
            operation_id,
            command,
            env: (!env.is_empty()).then_some(env),
            timeout_secs: Some(timeout.as_secs().max(1)),
        };
        let path = format!("/sandboxes/{}/exec-operations", encode_segment(&self.id));
        // A short HTTP timeout, not the step's: this call only enqueues.
        let start: ExecOperationRecord = self
            .sandbox
            .client()
            .request(
                Method::POST,
                &path,
                Some(&body),
                RequestOptions {
                    timeout: Some(Duration::from_secs(30)),
                    query: Vec::new(),
                },
            )
            .await
            .map_err(|e| VmError::Daemon {
                sandbox: self.id.clone(),
                what: "starting an exec operation",
                source: e,
            })?;

        if start.is_terminal() {
            return self.finish(operation_id, start);
        }

        let deadline = Instant::now() + timeout + POLL_SLACK;
        let mut interval = POLL_MIN;
        let get_path = format!(
            "/sandboxes/{}/exec-operations/{}",
            encode_segment(&self.id),
            encode_segment(operation_id)
        );
        loop {
            tokio::time::sleep(interval).await;
            // Exponential up to a ceiling: a `cargo build` polled every 250ms
            // for ten minutes is 2400 pointless round trips over an iroh tunnel.
            interval = (interval * 2).min(POLL_MAX);

            let record: ExecOperationRecord = self
                .sandbox
                .client()
                .request(
                    Method::GET,
                    &get_path,
                    None::<&()>,
                    RequestOptions {
                        timeout: Some(Duration::from_secs(30)),
                        query: Vec::new(),
                    },
                )
                .await
                .map_err(|e| VmError::Daemon {
                    sandbox: self.id.clone(),
                    what: "polling an exec operation",
                    source: e,
                })?;

            if record.is_terminal() {
                return self.finish(operation_id, record);
            }
            if Instant::now() >= deadline {
                return Err(VmError::ExecTimeout {
                    sandbox: self.id.clone(),
                    operation: operation_id.to_string(),
                    // Naming the guest timeout matters: exceeding *our* deadline
                    // when the guest's should have fired first means the daemon
                    // stopped answering, which is a different fault.
                    after: timeout + POLL_SLACK,
                });
            }
        }
    }

    fn finish(
        &self,
        operation_id: &str,
        record: ExecOperationRecord,
    ) -> Result<ExecOutput, VmError> {
        match record.status.as_str() {
            "completed" => {
                let r = record.result.ok_or_else(|| VmError::MalformedRecord {
                    sandbox: self.id.clone(),
                    operation: operation_id.to_string(),
                    detail: "status was `completed` but no result was attached".to_string(),
                })?;
                Ok(ExecOutput {
                    output: r.output,
                    stdout: r.stdout,
                    stderr: r.stderr,
                    exit_code: r.exit_code,
                })
            }
            // `failed` means the daemon could not run the command — a missing
            // sandbox, a guest timeout, a driver fault. It is not a non-zero
            // exit code, which arrives as `completed`.
            "failed" => Err(VmError::ExecFailed {
                sandbox: self.id.clone(),
                operation: operation_id.to_string(),
                reason: record
                    .error
                    .unwrap_or_else(|| "no reason reported".to_string()),
            }),
            other => Err(VmError::MalformedRecord {
                sandbox: self.id.clone(),
                operation: operation_id.to_string(),
                detail: format!("unexpected terminal status {other:?}"),
            }),
        }
    }

    /// Write bytes into the guest filesystem, in chunks, through exec.
    ///
    /// **Not the SDK's `Files::write`, and not the daemon's upload route.** Both
    /// go to `SandboxManager::upload_file`, which writes into a host-side
    /// *mount* directory and only syncs that into the guest on Hyper-V
    /// (`mvm-ctrl/src/sandbox.rs:8183`). On Firecracker a sandbox has no mounts,
    /// so the call fails with `Mount not found: /workspace (available mounts:
    /// [])` — and if it had one, the bytes would land on the host rather than in
    /// the VM. It also caps at 10 MB. Exec is the only transport that actually
    /// reaches the guest on every backend.
    ///
    /// The chunk size is set by measurement, not guesswork. The daemon renders a
    /// command as `env … sh -lc '<script>'`, so the script is one argv entry and
    /// Linux's `MAX_ARG_STRLEN` (128 KiB) bounds it. Probed against a real
    /// Firecracker guest: 32 KiB succeeds, 128 KiB returns `bash:
    /// /usr/bin/env: Argument list too long`. [`UPLOAD_CHUNK`] leaves room for
    /// the wrapper on top of the payload.
    ///
    /// Base64's alphabet is `A-Za-z0-9+/=`, none of which is special inside
    /// single quotes, so a chunk needs no escaping — which is also why it cannot
    /// break out of them.
    pub async fn upload_bytes(
        &self,
        operation_prefix: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), VmError> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let quoted = shell_single_quote(path);

        // An empty payload still has to create the file.
        let chunks: Vec<&str> = if encoded.is_empty() {
            vec![""]
        } else {
            encoded
                .as_bytes()
                .chunks(UPLOAD_CHUNK)
                .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
                .collect()
        };

        for (i, chunk) in chunks.iter().enumerate() {
            // The first chunk truncates, the rest append — so a retried upload
            // does not concatenate onto a half-written file.
            let redirect = if i == 0 { ">" } else { ">>" };
            let script = format!(
                "mkdir -p \"$(dirname {quoted})\" && printf %s '{chunk}' | base64 -d {redirect} {quoted}"
            );
            let out = self
                .exec(
                    &format!("{operation_prefix}.u{i}"),
                    &script,
                    &HashMap::new(),
                    Duration::from_secs(120),
                )
                .await?;
            if !out.succeeded() {
                return Err(VmError::UploadFailed {
                    sandbox: self.id.clone(),
                    path: path.to_string(),
                    chunk: i,
                    of: chunks.len(),
                    detail: out.combined(),
                });
            }
        }

        // The loop above proves every exec exited 0, which is a claim about
        // the shell, not about the bytes: a guest whose exec channel or
        // filesystem is corrupting data acknowledges every write and hands
        // back garbage — seen in the wild as a source tarball that uploads
        // cleanly and then fails `tar` with "not in gzip format". Hashing
        // both ends turns that silent corruption into a named error at the
        // layer that caused it, before anything downstream consumes the file.
        use sha2::{Digest, Sha256};
        let expected = format!("{:x}", Sha256::digest(bytes));
        let out = self
            .exec(
                &format!("{operation_prefix}.uv"),
                &format!("sha256sum {quoted}"),
                &HashMap::new(),
                Duration::from_secs(120),
            )
            .await?;
        let combined = out.combined();
        if !out.succeeded() {
            // 127 is an image without coreutils — a gap in the image, not in
            // the upload, so the upload stands unverified. Any other failure
            // is the guest unable to read back a file it just acknowledged,
            // which is exactly what this check exists to catch.
            if out.exit_code == 127 {
                tracing::warn!(
                    sandbox = %self.id,
                    path,
                    "upload not verified: the guest has no sha256sum"
                );
                return Ok(());
            }
            return Err(VmError::UploadCorrupted {
                sandbox: self.id.clone(),
                path: path.to_string(),
                expected,
                actual: format!("no hash; sha256sum failed with: {}", combined.trim()),
            });
        }
        let reported = combined
            .split_whitespace()
            .find(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()));
        match reported {
            Some(hash) if hash == expected => Ok(()),
            Some(hash) => Err(VmError::UploadCorrupted {
                sandbox: self.id.clone(),
                path: path.to_string(),
                expected,
                actual: hash.to_string(),
            }),
            // Exit 0 with no hash in the output is a backend quirk, not
            // evidence about the bytes; log it rather than fail on it.
            None => {
                tracing::warn!(
                    sandbox = %self.id,
                    path,
                    "upload not verified: sha256sum exited 0 without printing a hash"
                );
                Ok(())
            }
        }
    }

    /// Push the TTL out so a pooled VM does not expire while it is still wanted.
    /// The VM's own logs — console, stdout and stderr as the daemon collected
    /// them — rendered as text.
    ///
    /// Not in the SDK, so it goes through the raw client like the exec routes
    /// above. Bounded by `limit` because this is a whole VM's console since
    /// boot and the point is to attach it to a run, not to stream it.
    ///
    /// Read *before* the VM is released: a job's VM may be destroyed the moment
    /// it finishes, and a log nobody captured is a log nobody can read.
    pub async fn logs(&self, limit: usize) -> Result<String, VmError> {
        let path = format!("/sandboxes/{}/logs", encode_segment(&self.id));
        let response: LogsResponse = self
            .sandbox
            .client()
            .request(
                Method::GET,
                &path,
                None::<&()>,
                RequestOptions {
                    timeout: Some(Duration::from_secs(30)),
                    query: vec![("limit".to_string(), limit.to_string())],
                },
            )
            .await
            .map_err(|e| VmError::Daemon {
                sandbox: self.id.clone(),
                what: "reading the VM's logs",
                source: e,
            })?;

        let mut out = String::new();
        if response.total > response.logs.len() {
            // Said once at the top rather than left for someone to infer from a
            // log that starts mid-boot.
            out.push_str(&format!(
                "[ci] showing the last {} of {} lines\n",
                response.logs.len(),
                response.total
            ));
        }
        for entry in &response.logs {
            out.push_str(&format!("{:<7} {}\n", entry.source, entry.message));
        }
        Ok(out)
    }

    pub async fn renew_ttl(&self, ttl: Duration) -> Result<(), VmError> {
        self.sandbox
            .set_ttl(ttl.as_secs())
            .await
            .map_err(|e| VmError::Daemon {
                sandbox: self.id.clone(),
                what: "renewing the TTL",
                source: e,
            })
    }

    /// Delete the VM. A 404 is success — the SDK already treats it that way.
    pub async fn destroy(&self) -> Result<(), VmError> {
        let _guard = self.lock.lock().await;
        self.sandbox.kill().await.map_err(|e| VmError::Daemon {
            sandbox: self.id.clone(),
            what: "destroying the sandbox",
            source: e,
        })
    }
}

/// Whether the daemon will accept this as an `operationId`.
///
/// Mirrors `validate_exec_operation_id` (mvm-ctrl/src/api.rs:5306): 1..=160
/// bytes of `[A-Za-z0-9._-]`. Checked here so a bad id fails where it is
/// generated rather than as an opaque 400 in the middle of a job.
pub fn valid_operation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Mint an id usable as both an `operationId` and a NATS subject token:
/// `<epoch_ms:012x>-<seq:08x>`.
///
/// Hex and dash only, and time-ordered so a sorted listing reads
/// chronologically. Same scheme queue-fn uses for invocation ids.
pub fn new_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:012x}-{:08x}", ms, seq & 0xffff_ffff)
}

/// Sandbox name for a pooled VM: `ci-<workflow>-<fingerprint>-<nonce>`.
///
/// **The nonce is hex on purpose.** The daemon derives a VM's tap subnet from
/// its id with `from_str_radix(hex, 16).unwrap_or(0)`, so an id that is not
/// valid hex silently collapses to `172.16.0.2` — and every such VM collides on
/// one address. The daemon assigns the id rather than us, but keeping our names
/// hex-safe avoids feeding it anything pathological.
pub fn sandbox_name(workflow: &str, fingerprint: &str, nonce: u64) -> String {
    let workflow: String = workflow
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("ci-{workflow}-{fingerprint}-{nonce:012x}")
}

/// Single-quote a value for `sh`.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Percent-encode one path segment. Sandbox ids are `sb-…`/`dep-…` so this is
/// almost always a no-op, but an id is server-assigned and interpolated into a
/// URL.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Whether an SDK error is the transport, not the request.
///
/// The SDK folds every failure to *send* a request into `Api { status: 0 }`
/// ("network error calling …"); a real HTTP answer always carries its status.
/// `Connection` is the same condition on the shell WebSocket. Either one over a
/// cached iroh tunnel means the tunnel is dead — the daemon restarted, the
/// relay dropped it — and retrying through it can only fail the same way.
pub(crate) fn is_transport(e: &HeyoError) -> bool {
    matches!(e, HeyoError::Api { status: 0, .. } | HeyoError::Connection(_))
}

#[derive(Debug)]
pub enum VmError {
    Create {
        name: String,
        source: HeyoError,
    },
    Daemon {
        sandbox: String,
        what: &'static str,
        source: HeyoError,
    },
    NotRunnable {
        sandbox: String,
        status: String,
        reason: Option<String>,
    },
    BadOperationId(String),
    ExecFailed {
        sandbox: String,
        operation: String,
        reason: String,
    },
    ExecTimeout {
        sandbox: String,
        operation: String,
        after: Duration,
    },
    MalformedRecord {
        sandbox: String,
        operation: String,
        detail: String,
    },
    UploadFailed {
        sandbox: String,
        path: String,
        chunk: usize,
        of: usize,
        detail: String,
    },
    /// Every chunk exec exited 0 and the assembled file still hashes to
    /// something other than what was sent. There is no innocent reading of
    /// that: either the exec channel garbled the payload in flight or the
    /// guest's filesystem is returning different bytes than it acknowledged —
    /// and in both cases nothing else this VM reports can be trusted either,
    /// which is why [`crate::dispatch::DispatchError::indicates_guest_corruption`]
    /// treats it as grounds to destroy the VM rather than repool it.
    UploadCorrupted {
        sandbox: String,
        path: String,
        expected: String,
        actual: String,
    },
}

impl VmError {
    /// True when the failure was reaching the daemon at all, not anything the
    /// daemon said. See [`is_transport`] — the caller's right move is to drop
    /// the tunnel this rode in on so the retry redials.
    pub fn is_transport(&self) -> bool {
        match self {
            Self::Create { source, .. } | Self::Daemon { source, .. } => is_transport(source),
            _ => false,
        }
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create { name, source } => {
                write!(f, "could not create VM {name:?}: {source}")
            }
            Self::Daemon {
                sandbox,
                what,
                source,
            } => write!(f, "{what} for {sandbox}: {source}"),
            Self::NotRunnable {
                sandbox,
                status,
                reason,
            } => {
                write!(f, "VM {sandbox} is {status} and cannot run steps")?;
                if let Some(r) = reason {
                    write!(f, ": {r}")?;
                }
                Ok(())
            }
            Self::BadOperationId(id) => write!(
                f,
                "operation id {id:?} is not acceptable to the daemon; it must be \
                 1-160 bytes of letters, digits, '.', '_' or '-'"
            ),
            Self::ExecFailed {
                sandbox,
                operation,
                reason,
            } => write!(
                f,
                "the daemon could not run operation {operation} on {sandbox}: {reason}"
            ),
            Self::ExecTimeout {
                sandbox,
                operation,
                after,
            } => write!(
                f,
                "operation {operation} on {sandbox} did not finish within {after:?}. \
                 The guest timeout should have fired first, so the daemon most \
                 likely stopped responding."
            ),
            Self::MalformedRecord {
                sandbox,
                operation,
                detail,
            } => write!(
                f,
                "the daemon returned an exec record for {operation} on {sandbox} that \
                 could not be interpreted: {detail}"
            ),
            Self::UploadFailed {
                sandbox,
                path,
                chunk,
                of,
                detail,
            } => write!(
                f,
                "writing {path} into {sandbox} failed on chunk {} of {of}: {detail}",
                chunk + 1
            ),
            Self::UploadCorrupted {
                sandbox,
                path,
                expected,
                actual,
            } => write!(
                f,
                "writing {path} into {sandbox} completed, but the guest does not \
                 hold the bytes that were sent (sha256 mismatch: sent {expected}, \
                 guest reports {actual}) — the exec channel or the guest \
                 filesystem corrupted the data"
            ),
        }
    }
}

impl std::error::Error for VmError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> VmSpec {
        VmSpec {
            driver: SandboxDriver::Firecracker,
            image: Some("ubuntu:24.04".into()),
            build: None,
            size_class: Some(SandboxSize::Medium),
            disk_size_gb: Some(20),
            working_directory: Some("/workspace".into()),
            env_vars: BTreeMap::new(),
            setup_hooks: vec![],
            cache_key_files: vec![],
            reuse: true,
            ttl_seconds: None,
        }
    }

    #[test]
    fn a_reasonable_spec_validates() {
        assert_eq!(spec().validate(), Ok(()));
    }

    #[test]
    fn libvirt_is_refused_at_parse_time() {
        let mut s = spec();
        s.driver = SandboxDriver::Libvirt;
        assert_eq!(s.validate(), Err(VmSpecError::UnsupportedDriver));
        assert!(
            VmSpecError::UnsupportedDriver
                .to_string()
                .contains("firecracker"),
            "the error must name what to use instead"
        );
    }

    /// A `..` in a cache-key path would let a workflow hash files outside its
    /// own checkout, which is both a traversal and a fingerprint that no other
    /// runner could reproduce.
    #[test]
    fn cache_key_files_may_not_escape_the_checkout() {
        for bad in ["../secrets", "/etc/passwd", "a/../../b"] {
            let mut s = spec();
            s.cache_key_files = vec![bad.to_string()];
            assert_eq!(
                s.validate(),
                Err(VmSpecError::EscapingCacheKeyFile(bad.to_string())),
                "{bad} must be rejected"
            );
        }
        let mut s = spec();
        s.cache_key_files = vec!["Cargo.lock".into(), "a/b/rust-toolchain.toml".into()];
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn env_keys_must_be_shell_safe() {
        let mut s = spec();
        s.env_vars.insert("PATH; rm -rf /".into(), "x".into());
        assert!(matches!(s.validate(), Err(VmSpecError::InvalidEnvKey(_))));

        let mut s = spec();
        s.env_vars.insert("CARGO_TERM_COLOR".into(), "never".into());
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn zero_sizes_are_rejected() {
        let mut s = spec();
        s.disk_size_gb = Some(0);
        assert_eq!(s.validate(), Err(VmSpecError::ZeroDisk));
        let mut s = spec();
        s.ttl_seconds = Some(0);
        assert_eq!(s.validate(), Err(VmSpecError::ZeroTtl));
    }

    /// The SDK marks a failure to *send* a request as `Api { status: 0 }`; a
    /// real answer always carries its status. Only the former means the cached
    /// tunnel is dead and worth evicting — treating a daemon's 500 as a tunnel
    /// failure would redial on every genuine error.
    #[test]
    fn only_a_transport_failure_reads_as_one() {
        let api = |status: u16| HeyoError::Api {
            status,
            message: "network error calling /sandbox-deploy".into(),
            body: None,
        };
        let dead = VmError::Create {
            name: "vm".into(),
            source: api(0),
        };
        assert!(dead.is_transport());
        let refused = VmError::Create {
            name: "vm".into(),
            source: api(500),
        };
        assert!(!refused.is_transport(), "a 500 arrived; the tunnel works");
        let exec = VmError::ExecFailed {
            sandbox: "vm".into(),
            operation: "op".into(),
            reason: "exit 1".into(),
        };
        assert!(!exec.is_transport(), "a failed step is not a dead link");
    }

    /// The fingerprint hashes the serialized spec, so map ordering has to be
    /// stable across processes or every restart rebuilds every VM.
    #[test]
    fn env_vars_serialize_in_a_stable_order() {
        let mut a = spec();
        let mut b = spec();
        for (k, v) in [("Z", "1"), ("A", "2"), ("M", "3")] {
            a.env_vars.insert(k.into(), v.into());
        }
        for (k, v) in [("M", "3"), ("Z", "1"), ("A", "2")] {
            b.env_vars.insert(k.into(), v.into());
        }
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn reuse_defaults_to_true() {
        let s: VmSpec = serde_yaml::from_str("driver: firecracker\nimage: ubuntu:24.04\n").unwrap();
        assert!(s.reuse);
        assert_eq!(s.driver, SandboxDriver::Firecracker);
    }

    #[test]
    fn operation_ids_match_the_daemons_rule() {
        assert!(valid_operation_id("019f7c7ef325-00000000"));
        assert!(valid_operation_id("a.b_c-1"));
        assert!(!valid_operation_id(""));
        assert!(!valid_operation_id("has space"));
        assert!(!valid_operation_id("has/slash"));
        assert!(!valid_operation_id(&"a".repeat(161)));
        assert!(valid_operation_id(&"a".repeat(160)));
    }

    #[test]
    fn minted_ids_are_valid_operation_ids_and_subject_tokens() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b, "two calls in the same millisecond must still differ");
        for id in [&a, &b] {
            assert!(valid_operation_id(id), "{id}");
            // Also usable as a NATS subject token — no dots.
            assert!(crate::config::is_subject_token(id), "{id}");
        }
        assert!(a < b, "ids must sort chronologically");
    }

    /// Non-hex characters in a VM name feed an id the daemon parses with
    /// `from_str_radix(_, 16).unwrap_or(0)`, collapsing every VM onto one tap
    /// address.
    #[test]
    fn sandbox_names_are_hex_safe_and_sanitized() {
        let n = sandbox_name("my/weird workflow", "a1b2c3d4e5f6", 1);
        assert_eq!(n, "ci-my-weird-workflow-a1b2c3d4e5f6-000000000001");
        assert!(
            n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "{n}"
        );
    }

    /// Trap 2. On the firecracker serial path the daemon returns an empty
    /// `stderr` and puts everything in `output`; reading `stdout` alone loses
    /// every diagnostic a failing build wrote.
    #[test]
    fn combined_output_prefers_the_daemons_combined_stream() {
        let o = ExecOutput {
            output: "compiling\nerror: no such file\n".into(),
            stdout: "compiling\n".into(),
            stderr: String::new(),
            exit_code: 1,
        };
        assert_eq!(o.combined(), "compiling\nerror: no such file\n");
        assert!(!o.succeeded());
    }

    /// Backends that populate the split streams and leave `output` empty must
    /// still produce everything.
    #[test]
    fn combined_output_falls_back_to_concatenating_the_split_streams() {
        let o = ExecOutput {
            output: String::new(),
            stdout: "line one".into(),
            stderr: "a warning".into(),
            exit_code: 0,
        };
        assert_eq!(o.combined(), "line one\na warning");
        assert!(o.succeeded());
    }

    /// Pinned to a record the daemon actually wrote
    /// (`~/.heyo/exec-operations/sb-3aac9a30/019f7c7ef325-00000000.json`). The
    /// envelope is camelCase while the nested result is snake_case, and getting
    /// that wrong silently yields `exit_code: 0` for every step.
    #[test]
    fn a_real_daemon_record_deserializes_with_its_mixed_casing() {
        let json = r#"{
          "operationId": "019f7c7ef325-00000000",
          "sandboxId": "sb-3aac9a30",
          "status": "completed",
          "command": "echo hi",
          "result": { "output": "hi\n", "stdout": "hi\n", "stderr": "", "exit_code": 7 },
          "createdAt": "2026-07-19T22:30:00.081149324+00:00",
          "updatedAt": "2026-07-19T22:30:00.136611374+00:00",
          "completedAt": "2026-07-19T22:30:00.136611374+00:00"
        }"#;
        let rec: ExecOperationRecord = serde_json::from_str(json).unwrap();
        assert!(rec.is_terminal());
        let r = rec.result.expect("result");
        assert_eq!(r.exit_code, 7, "snake_case exit_code must be read");
        assert_eq!(r.output, "hi\n");
    }

    #[test]
    fn queued_and_running_are_not_terminal() {
        for s in ["queued", "running"] {
            let rec: ExecOperationRecord =
                serde_json::from_str(&format!(r#"{{"status":"{s}"}}"#)).unwrap();
            assert!(!rec.is_terminal(), "{s}");
        }
        for s in ["completed", "failed"] {
            let rec: ExecOperationRecord =
                serde_json::from_str(&format!(r#"{{"status":"{s}"}}"#)).unwrap();
            assert!(rec.is_terminal(), "{s}");
        }
    }

    /// The request body has to be camelCase or the daemon defaults every field,
    /// most damagingly `timeoutSecs` — which is the whole reason this module
    /// bypasses the SDK's exec.
    #[test]
    fn the_start_request_is_camel_case() {
        let env: HashMap<String, String> = [("A".to_string(), "b".to_string())].into();
        let body = StartExecOperation {
            operation_id: "op-1",
            command: "true",
            env: Some(&env),
            timeout_secs: Some(1800),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["operationId"], "op-1");
        assert_eq!(json["timeoutSecs"], 1800);
        assert!(json.get("operation_id").is_none());
        assert!(json.get("timeout_secs").is_none());
    }

    #[test]
    fn an_absent_env_is_omitted_rather_than_sent_as_null() {
        let body = StartExecOperation {
            operation_id: "op-1",
            command: "true",
            env: None,
            timeout_secs: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("env").is_none());
        assert!(json.get("timeoutSecs").is_none());
    }

    #[test]
    fn the_upload_chunk_stays_under_the_measured_guest_limit() {
        // The chunk size is a measured limit, not a guess: 32 KiB commands
        // succeed against a real Firecracker guest and 128 KiB returns
        // `bash: /usr/bin/env: Argument list too long`, because the daemon
        // renders the script as one argv entry and Linux bounds that at
        // MAX_ARG_STRLEN. This pins the margin the wrapper needs.
        // Both bounds are compile-time constants; the point of asserting them
        // is that changing UPLOAD_CHUNK past what a guest accepts fails here
        // rather than as a hung build.
        const _: () = assert!(UPLOAD_CHUNK <= 32 * 1024 - 4096);
        const _: () = assert!(UPLOAD_CHUNK >= 8 * 1024);
    }

    /// Base64 is `A-Za-z0-9+/=` — nothing special inside single quotes — which
    /// is why a chunk is interpolated without escaping. If that ever stopped
    /// being true, the upload would become a shell injection.
    #[test]
    fn base64_output_contains_nothing_that_needs_shell_escaping() {
        use base64::Engine;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode((0u8..=255).collect::<Vec<u8>>());
        assert!(
            encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
            "{encoded}"
        );
        assert!(!encoded.contains('\''));
    }

    #[test]
    fn a_path_with_a_quote_is_escaped_for_the_shell() {
        assert_eq!(shell_single_quote("/workspace"), "'/workspace'");
        let escaped = shell_single_quote("/tmp/a'b");
        assert!(
            escaped.starts_with('\'') && escaped.ends_with('\''),
            "{escaped}"
        );
        assert!(escaped.contains(r"'\''"), "{escaped}");
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(encode_segment("sb-3aac9a30"), "sb-3aac9a30");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("a b"), "a%20b");
    }

    /// Boots a real VM on the local `heyvmd` and runs steps in it. Run with:
    /// `cargo test -- --ignored --nocapture local_daemon`
    ///
    /// Env overrides: `CI_TEST_IMAGE` (default `debian`), `CI_TEST_DRIVER`
    /// (`firecracker` or `kvm`, default `firecracker`), `CI_TEST_DAEMON`
    /// (default `http://127.0.0.1:34099`).
    ///
    /// `firecracker` is the default because `kvm` re-execs the daemon's own
    /// binary as `kvm-run <id>` (mvm-ctrl/src/lib.rs), so it fails with
    /// "No such file or directory" whenever `heyvmd` is running from a path that
    /// has since been rebuilt or moved — which is the normal state of a
    /// development machine.
    ///
    /// The middle assertion is the point of this whole module: a command that
    /// runs for longer than 30 seconds. The SDK's own exec cannot express that
    /// — its timeout never reaches the guest, so the firecracker serial path
    /// caps at 30s — and no CI step of any interest fits under that.
    #[tokio::test]
    #[ignore = "boots a real VM on the local heyvmd"]
    async fn local_daemon_runs_steps_past_the_thirty_second_ceiling() {
        let base = std::env::var("CI_TEST_DAEMON")
            .unwrap_or_else(|_| "http://127.0.0.1:34099".to_string());
        let image = std::env::var("CI_TEST_IMAGE").unwrap_or_else(|_| "debian".to_string());
        let driver = match std::env::var("CI_TEST_DRIVER").as_deref() {
            Ok("kvm") => SandboxDriver::Kvm,
            _ => SandboxDriver::Firecracker,
        };
        let options = HeyoClientOptions {
            // The local daemon runs without JWT_SECRET, so no bearer is sent.
            api_key: None,
            base_url: Some(base),
            timeout: None,
        };

        let spec = VmSpec {
            driver,
            image: Some(image),
            build: None,
            size_class: Some(SandboxSize::Micro),
            disk_size_gb: None,
            working_directory: None,
            env_vars: BTreeMap::new(),
            setup_hooks: vec![],
            cache_key_files: vec![],
            reuse: true,
            ttl_seconds: Some(900),
        };
        spec.validate().expect("spec validates");

        let vms = Vms::new();
        let vm = vms
            .create(
                options,
                &sandbox_name("selftest", "000000000000", 1),
                &spec,
                Duration::from_secs(900),
                Duration::from_secs(240),
            )
            .await
            .expect("VM boots");
        println!("booted {}", vm.id());

        // Everything after this point must run even on failure, or a panicking
        // assertion strands a VM on the developer's machine.
        let outcome = run_local_daemon_checks(&vm).await;
        if let Err(e) = vm.destroy().await {
            eprintln!("warning: could not destroy {}: {e}", vm.id());
        }
        outcome.expect("all checks pass");
    }

    async fn run_local_daemon_checks(vm: &Vm) -> Result<(), String> {
        let env = HashMap::new();

        let out = vm
            .exec(&new_id(), "echo hello", &env, Duration::from_secs(60))
            .await
            .map_err(|e| format!("basic exec: {e}"))?;
        if !out.combined().contains("hello") {
            return Err(format!("expected 'hello', got {:?}", out.combined()));
        }
        if !out.succeeded() {
            return Err(format!("expected exit 0, got {}", out.exit_code));
        }

        // The whole reason this module bypasses the SDK's exec.
        let started = Instant::now();
        let out = vm
            .exec(
                &new_id(),
                "sleep 40; echo survived",
                &env,
                Duration::from_secs(180),
            )
            .await
            .map_err(|e| format!("long exec: {e}"))?;
        let elapsed = started.elapsed();
        if !out.combined().contains("survived") {
            return Err(format!(
                "a 40s command did not survive the 30s guest ceiling; got {:?} after {elapsed:?}",
                out.combined()
            ));
        }
        if elapsed < Duration::from_secs(35) {
            return Err(format!(
                "the long step returned suspiciously fast: {elapsed:?}"
            ));
        }
        println!("40s step completed in {elapsed:?}");

        // A non-zero exit is a failed step but a successful exec, and stderr has
        // to survive — on the serial path it arrives only via `output`.
        let out = vm
            .exec(
                &new_id(),
                "echo to-stdout; echo to-stderr >&2; exit 3",
                &env,
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| format!("failing exec: {e}"))?;
        if out.exit_code != 3 {
            return Err(format!("expected exit 3, got {}", out.exit_code));
        }
        let combined = out.combined();
        for want in ["to-stdout", "to-stderr"] {
            if !combined.contains(want) {
                return Err(format!("combined output lost {want:?}: {combined:?}"));
            }
        }

        // Idempotency: the same operation id and command must reattach rather
        // than run twice. This is what makes a queue redelivery safe.
        let op = new_id();
        let cmd = "date +%s%N";
        let first = vm
            .exec(&op, cmd, &env, Duration::from_secs(60))
            .await
            .map_err(|e| format!("idempotent exec (first): {e}"))?;
        let second = vm
            .exec(&op, cmd, &env, Duration::from_secs(60))
            .await
            .map_err(|e| format!("idempotent exec (replay): {e}"))?;
        if first.combined() != second.combined() {
            return Err(format!(
                "replaying an operation id re-ran the command: {:?} vs {:?}",
                first.combined(),
                second.combined()
            ));
        }

        // Env vars must reach the guest.
        let mut env2 = HashMap::new();
        env2.insert("CI_SELFTEST".to_string(), "marker-value".to_string());
        let out = vm
            .exec(
                &new_id(),
                "echo $CI_SELFTEST",
                &env2,
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| format!("env exec: {e}"))?;
        if !out.combined().contains("marker-value") {
            return Err(format!(
                "env var did not reach the guest: {:?}",
                out.combined()
            ));
        }

        Ok(())
    }
}
