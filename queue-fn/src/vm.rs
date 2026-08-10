//! The heyo-sdk boundary.
//!
//! Everything that knows about sandboxes lives here so the traps are in one
//! place. Four are worth stating up front, because every one of them fails
//! silently:
//!
//! 1. `Sandbox::wait_for_ready` returning `Ok` does **not** mean the VM is
//!    usable — its match has a `_ => return Ok(info)` arm, so `Stopped`,
//!    `Paused`, and `ColdStored` all come back `Ok`. Against a local daemon a
//!    broken VM surfaces as `Stopped`, never `Failed`. Always check the status.
//! 2. `guest_ip` is only populated for tap-networked Firecracker/KVM on a local
//!    daemon. We never dial it, but its absence is the cheapest proof that a VM
//!    was built on a backend we cannot actually drive, so it is a hard error.
//! 3. On the firecracker serial path the daemon runs `(cmd) 2>&1` and builds its
//!    response with an **empty stderr** (mvm-ctrl/src/driver/firecracker.rs:1752).
//!    Reading `stdout` alone therefore loses everything the command wrote to
//!    stderr; `normalize` folds the combined `output` field back in.
//! 4. Two concurrent execs against one sandbox do not queue — the second gets
//!    `SandboxNotFound`, because the first holds the handle out of the manager's
//!    map for its duration (mvm-ctrl/src/sandbox.rs:3648). `function::VmWorker`
//!    is what prevents that; this module only surfaces the error clearly.

use crate::config::VmSpec;
use heyo_sdk::{
    CommandRunOptions, HeyoClient, HeyoClientOptions, RequestOptions, Sandbox,
    SandboxCreateOptions, SandboxDriver, SandboxInfo, SandboxStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

/// Marks a sandbox as ours and records which function owns it, so VMs can be
/// re-adopted after a restart instead of being orphaned or duplicated.
pub const OWNER_PREFIX: &str = "qfn";

/// Sandbox name for a replica: `qfn-<function>-<nonce>`.
///
/// The nonce is *hex* on purpose. The daemon derives a VM's tap subnet from the
/// sandbox id via `from_str_radix(hex, 16).unwrap_or(0)`, so an id that isn't
/// valid hex silently collapses to `172.16.0.2` — and every such VM collides on
/// the same address. The daemon assigns the id, not us, but keeping our names
/// hex-safe avoids feeding it anything pathological.
pub fn replica_name(function_id: &str, nonce: u64) -> String {
    format!("{OWNER_PREFIX}-{function_id}-{nonce:012x}")
}

/// Which function a sandbox belongs to, or `None` if it isn't ours.
pub fn owner_of(sandbox_name: &str) -> Option<&str> {
    let rest = sandbox_name.strip_prefix(OWNER_PREFIX)?.strip_prefix('-')?;
    // Trailing `-<nonce>` is ours; everything before it is the function id,
    // which may itself contain dashes.
    let (id, _nonce) = rest.rsplit_once('-')?;
    (!id.is_empty()).then_some(id)
}

/// Whether a string is acceptable to the daemon as an `operationId`.
///
/// Mirrors `validate_exec_operation_id` (mvm-ctrl/src/api.rs:5283): 1..=160
/// bytes of `[A-Za-z0-9._-]`. Checked on our side so a bad invocation id fails
/// where it is generated rather than as an opaque 400 mid-dispatch.
pub fn valid_operation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Debug)]
pub enum VmError {
    Sdk(heyo_sdk::HeyoError),
    /// The daemon reported a state that means this VM will never run anything.
    NotRunning {
        sandbox_id: String,
        status: SandboxStatus,
        reason: Option<String>,
    },
    /// No `guest_ip`: the VM is on a backend we cannot drive. Happens for
    /// non-tap backends or a remote daemon.
    NoGuestIp {
        sandbox_id: String,
    },
    /// `guest_ip` was not parseable as an IP address.
    BadGuestIp {
        sandbox_id: String,
        value: String,
    },
    /// The daemon reported the operation failed without producing an exit code.
    ExecFailed {
        sandbox_id: String,
        reason: String,
    },
    /// The operation did not reach a terminal state within its budget.
    ExecTimeout {
        sandbox_id: String,
        secs: u64,
    },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sdk(e) => write!(f, "{e}"),
            Self::NotRunning {
                sandbox_id,
                status,
                reason,
            } => {
                write!(f, "sandbox {sandbox_id} is {status:?}, not running")?;
                if let Some(r) = reason {
                    write!(f, ": {r}")?;
                }
                Ok(())
            }
            Self::NoGuestIp { sandbox_id } => write!(
                f,
                "sandbox {sandbox_id} has no guest_ip; the daemon only exposes one for \
                 tap-networked firecracker/kvm backends running locally"
            ),
            Self::BadGuestIp { sandbox_id, value } => write!(
                f,
                "sandbox {sandbox_id} reported unparseable guest_ip {value:?}"
            ),
            Self::ExecFailed { sandbox_id, reason } => {
                write!(f, "exec on sandbox {sandbox_id} failed: {reason}")
            }
            Self::ExecTimeout { sandbox_id, secs } => write!(
                f,
                "exec on sandbox {sandbox_id} did not finish within {secs}s"
            ),
        }
    }
}

impl std::error::Error for VmError {}

impl From<heyo_sdk::HeyoError> for VmError {
    fn from(e: heyo_sdk::HeyoError) -> Self {
        Self::Sdk(e)
    }
}

impl VmError {
    /// Whether this error means the *sandbox* is gone, as opposed to the command
    /// having failed.
    ///
    /// The dispatcher uses this to mark a worker unhealthy and retry elsewhere
    /// immediately. It is also the canary for the concurrent-exec bug: a second
    /// exec against a busy sandbox comes back as `NotFound` even though the VM
    /// is perfectly alive.
    pub fn is_sandbox_gone(&self) -> bool {
        matches!(self, Self::Sdk(heyo_sdk::HeyoError::NotFound(_)))
    }
}

/// The result of one command, normalized across the daemon's exec paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl ExecOutput {
    /// Fold the daemon's three output fields into two.
    ///
    /// The serial path returns everything in `output` with `stdout`/`stderr`
    /// empty; the SSH path fills `stdout`/`stderr` properly. Preferring the
    /// explicit streams and falling back to `output` covers both without the
    /// caller needing to know which backend ran the command.
    pub fn normalize(stdout: String, stderr: String, output: String, exit_code: i32) -> Self {
        let stdout = if stdout.is_empty() && stderr.is_empty() {
            output
        } else {
            stdout
        };
        Self {
            stdout,
            stderr,
            exit_code,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// Live host + per-sandbox resource usage from the daemon's `GET /system/usage`.
///
/// The daemon serves this from a background poller (~5s) sampling the host
/// processes that back each VM, so it is cheap to fetch — a cache read, not a
/// per-VM probe — and safe to poll on the reconcile tick.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemUsage {
    /// False while the poller is still warming up; `snapshot` is then absent.
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub snapshot: Option<UsageSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub host: HostUsage,
    #[serde(default)]
    pub sampled_at_ms: u64,
    #[serde(default)]
    pub sandboxes: Vec<SandboxUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostUsage {
    #[serde(default)]
    pub cpu_count: u32,
    /// Whole-host CPU utilisation, 0–100.
    #[serde(default)]
    pub cpu_percent: f64,
    #[serde(default)]
    pub memory_total_bytes: u64,
    #[serde(default)]
    pub memory_used_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxUsage {
    #[serde(default)]
    pub sandbox_id: String,
    /// `top`-style CPU: percent of a *single* core, so a busy multi-vCPU VM can
    /// exceed 100.
    #[serde(default)]
    pub cpu_percent: f64,
    /// Resident set size of the backing host process(es).
    #[serde(default)]
    pub memory_bytes: u64,
}

/// `POST /sandboxes/:id/exec-operations`. No SDK wrapper exists, so this goes
/// through the generic request helper — and note the wire format is **camelCase**
/// here, unlike the rest of the daemon's API.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartExecOperation<'a> {
    operation_id: &'a str,
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecOperationRecord {
    pub operation_id: String,
    #[serde(default)]
    pub sandbox_id: String,
    /// `queued` | `running` | `completed` | `failed`
    /// (mvm-ctrl/src/api.rs:5113, 5204, 5226, 5231).
    pub status: String,
    #[serde(default)]
    pub result: Option<ExecOperationResult>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecOperationResult {
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
}

/// The two non-terminal exec-operation states (mvm-ctrl/src/api.rs:5113, 5204).
const PENDING_STATUSES: [&str; 2] = ["queued", "running"];

impl ExecOperationRecord {
    /// Terminal iff the daemon is no longer working on it.
    ///
    /// Deliberately a check for *not pending* rather than a list of terminal
    /// statuses. Matching terminal states positively is how this broke: the
    /// list said `succeeded`, the daemon says `completed`, and every invocation
    /// polled until it hit its timeout while a finished result sat there. Under
    /// this form an unrecognised status is treated as terminal — the caller
    /// sees a real error instead of hanging for the full budget.
    pub fn is_finished(&self) -> bool {
        !PENDING_STATUSES.contains(&self.status.as_str())
    }
}

/// Talks to the heyvm daemon.
#[derive(Debug, Clone)]
pub struct VmManager {
    opts: HeyoClientOptions,
}

impl VmManager {
    /// Targets the local daemon (`http://127.0.0.1:34099` by default), which is
    /// the only place `guest_ip` is available.
    pub fn new(daemon_url: Option<String>) -> Self {
        Self {
            opts: HeyoClientOptions {
                base_url: Some(
                    daemon_url.unwrap_or_else(|| heyo_sdk::DEFAULT_LOCAL_BASE_URL.to_string()),
                ),
                // A same-machine daemon runs without JWT_SECRET and skips auth.
                api_key: None,
                timeout: Some(Duration::from_secs(30)),
            },
        }
    }

    /// Every sandbox the daemon knows about.
    ///
    /// Callers should hit this **once per reconcile tick** and index the result:
    /// `Sandbox::info()` fetches this same full list and filters client-side, so
    /// per-VM polling is quadratic.
    pub async fn list(&self) -> Result<Vec<SandboxInfo>, VmError> {
        Ok(Sandbox::list(self.opts.clone()).await?)
    }

    /// Create a VM and return immediately, without waiting for boot.
    ///
    /// `wait_for_ready: Some(ZERO)` is deliberate: the autoscaler must not block
    /// its reconcile loop for the minutes a VM can take. Readiness is tracked
    /// across subsequent ticks instead.
    pub async fn create(&self, spec: &VmSpec, name: String) -> Result<Sandbox, VmError> {
        debug_assert!(
            matches!(spec.driver, SandboxDriver::Firecracker | SandboxDriver::Kvm),
            "FunctionSpec::validate must reject other drivers before reaching here",
        );

        let options = SandboxCreateOptions {
            name: Some(name),
            driver: Some(spec.driver),
            image: spec.image.clone(),
            start_command: spec.start_command.clone(),
            size_class: spec.size_class,
            disk_size_gb: spec.disk_size_gb,
            working_directory: spec.working_directory.clone(),
            env_vars: spec.env_vars.clone(),
            // Deliberately none: function code is baked into the image, so a
            // hook here would turn every cold start into a network install.
            setup_hooks: None,
            open_ports: vec![],
            // Backstop: if this process dies without reaping, the VM expires.
            ttl_seconds: Some(spec.ttl_seconds),
            wait_for_ready: Some(Duration::ZERO),
            region: None,
            archive_id: None,
        };

        Ok(Sandbox::create(options, self.opts.clone()).await?)
    }

    pub fn connect(&self, sandbox_id: String) -> Result<Sandbox, VmError> {
        Ok(Sandbox::connect(sandbox_id, self.opts.clone())?)
    }

    /// Run a command and wait for it, in one round trip.
    ///
    /// Used for the readiness probe and anywhere the caller is already blocked
    /// and has nothing to be idempotent about. Note `timeout` bounds only the
    /// HTTP call — the guest-side ceiling is the daemon's, not ours.
    pub async fn exec(
        &self,
        sandbox_id: &str,
        command: &str,
        env: Option<&HashMap<String, String>>,
        cwd: Option<&str>,
        timeout: Duration,
    ) -> Result<ExecOutput, VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        let result = sandbox
            .commands()
            .run(
                command,
                CommandRunOptions {
                    cwd: cwd.map(Into::into),
                    env: env.cloned(),
                    timeout: Some(timeout),
                },
            )
            .await?;
        Ok(ExecOutput::normalize(
            result.stdout,
            result.stderr,
            result.output,
            result.exit_code,
        ))
    }

    /// Start an idempotent async exec, keyed by `operation_id`.
    ///
    /// The daemon stores a record per operation id and, on a repeat call with
    /// the same id and command, returns the *existing* record rather than
    /// running again (mvm-ctrl/src/api.rs:5098-5106). That is what makes a
    /// JetStream redelivery safe: a crash between "exec finished" and "message
    /// acked" replays into a result lookup, not a second execution.
    pub async fn start_exec_operation(
        &self,
        sandbox_id: &str,
        operation_id: &str,
        command: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOperationRecord, VmError> {
        let client = HeyoClient::new(self.opts.clone())?;
        let body = StartExecOperation {
            operation_id,
            command,
            env,
        };
        Ok(client
            .request::<ExecOperationRecord>(
                http::Method::POST,
                &format!("/sandboxes/{}/exec-operations", encode_segment(sandbox_id)),
                Some(&body),
                RequestOptions::default(),
            )
            .await?)
    }

    /// `GET /sandboxes/:id/exec-operations/:operation_id`.
    pub async fn poll_exec_operation(
        &self,
        sandbox_id: &str,
        operation_id: &str,
    ) -> Result<ExecOperationRecord, VmError> {
        let client = HeyoClient::new(self.opts.clone())?;
        Ok(client
            .request::<ExecOperationRecord>(
                http::Method::GET,
                &format!(
                    "/sandboxes/{}/exec-operations/{}",
                    encode_segment(sandbox_id),
                    encode_segment(operation_id)
                ),
                None::<&serde_json::Value>,
                RequestOptions::default(),
            )
            .await?)
    }

    /// Fetch the daemon's cached host + per-sandbox usage snapshot.
    ///
    /// Like `list`, a single round trip meant for once-per-tick use — the daemon
    /// already samples in the background, so this reads its cache. There is no
    /// typed SDK method, so it goes through the generic request helper.
    pub async fn system_usage(&self) -> Result<SystemUsage, VmError> {
        let client = HeyoClient::new(self.opts.clone())?;
        Ok(client
            .request::<SystemUsage>(
                http::Method::GET,
                "/system/usage",
                None::<&serde_json::Value>,
                RequestOptions::default(),
            )
            .await?)
    }

    /// Destroy a VM. A VM the daemon has already forgotten counts as killed.
    pub async fn kill(&self, sandbox_id: &str) -> Result<(), VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        match sandbox.kill().await {
            Ok(()) => Ok(()),
            Err(heyo_sdk::HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Keep a VM's TTL backstop from expiring under a long-lived function.
    pub async fn renew_ttl(&self, sandbox_id: &str, ttl_seconds: u64) -> Result<(), VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        match sandbox.set_ttl(ttl_seconds).await {
            Ok(()) => Ok(()),
            Err(heyo_sdk::HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Percent-encode a path segment. Sandbox ids and our invocation ids are already
/// in the unreserved set, so this is normally a no-op — it exists so a
/// pathological id produces a 404 rather than a traversal.
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

/// Confirm a VM is one we can actually drive, and return its guest IP.
///
/// queue-fn never dials this address — it execs through the daemon. The check
/// exists because it enforces both SDK traps in one place: the status must be
/// *actually* `Running` (not merely `Ok` from `wait_for_ready`), and a
/// `guest_ip` must be present and parseable, which is only true on the
/// tap-networked local backends whose exec path we depend on. A VM that fails
/// this is misconfigured in a way that would otherwise surface much later, as a
/// mysterious exec failure.
pub fn routable_ip(info: &SandboxInfo) -> Result<IpAddr, VmError> {
    if info.status != SandboxStatus::Running {
        return Err(VmError::NotRunning {
            sandbox_id: info.id.clone(),
            status: info.status.clone(),
            reason: info.error_message.clone(),
        });
    }
    let Some(raw) = info.guest_ip.as_deref().filter(|s| !s.is_empty()) else {
        return Err(VmError::NoGuestIp {
            sandbox_id: info.id.clone(),
        });
    };
    raw.parse().map_err(|_| VmError::BadGuestIp {
        sandbox_id: info.id.clone(),
        value: raw.to_string(),
    })
}

/// Whether a status means the VM will never run anything and should be reaped,
/// as opposed to still being on its way up.
pub fn is_terminal(status: &SandboxStatus) -> bool {
    match status {
        // Still booting, or the daemon hasn't classified it yet.
        SandboxStatus::Provisioning | SandboxStatus::Unknown | SandboxStatus::Running => false,
        // Against a local daemon a broken VM shows up as Stopped, not Failed,
        // so Stopped has to count as terminal for a VM we just asked to start.
        SandboxStatus::Stopped
        | SandboxStatus::Paused
        | SandboxStatus::Failed
        | SandboxStatus::ColdStored => true,
    }
}

/// Index a sandbox list by id for O(1) lookup during reconcile.
pub fn index_by_id(infos: Vec<SandboxInfo>) -> HashMap<String, SandboxInfo> {
    infos.into_iter().map(|i| (i.id.clone(), i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, status: SandboxStatus, guest_ip: Option<&str>) -> SandboxInfo {
        SandboxInfo {
            id: id.into(),
            name: id.into(),
            status,
            image: "ubuntu:24.04".into(),
            region: None,
            start_command: None,
            working_directory: None,
            size_class: None,
            env_vars: None,
            setup_hooks: None,
            uptime_secs: 0,
            ttl_seconds: None,
            is_deployed: true,
            error_message: None,
            status_changed_at: String::new(),
            urls: vec![],
            guest_ip: guest_ip.map(Into::into),
            metadata: None,
        }
    }

    #[test]
    fn routable_ip_accepts_a_running_vm_with_a_guest_ip() {
        let i = info("sb-1", SandboxStatus::Running, Some("172.16.0.2"));
        assert_eq!(routable_ip(&i).unwrap(), "172.16.0.2".parse::<IpAddr>().unwrap());
    }

    /// The trap: `wait_for_ready` returns Ok for a Stopped VM, so this check is
    /// the only thing standing between a dead VM and the pool.
    #[test]
    fn routable_ip_rejects_non_running_even_with_an_ip() {
        for status in [
            SandboxStatus::Stopped,
            SandboxStatus::Paused,
            SandboxStatus::Provisioning,
            SandboxStatus::Failed,
            SandboxStatus::ColdStored,
        ] {
            let i = info("sb-1", status, Some("172.16.0.2"));
            assert!(
                matches!(routable_ip(&i), Err(VmError::NotRunning { .. })),
                "expected NotRunning",
            );
        }
    }

    #[test]
    fn routable_ip_rejects_missing_or_empty_guest_ip() {
        let i = info("sb-1", SandboxStatus::Running, None);
        assert!(matches!(routable_ip(&i), Err(VmError::NoGuestIp { .. })));

        // The daemon reports `""` rather than omitting the field for backends
        // that have no tap address, so an empty string is "absent", not a value.
        let i = info("sb-1", SandboxStatus::Running, Some(""));
        assert!(matches!(routable_ip(&i), Err(VmError::NoGuestIp { .. })));
    }

    #[test]
    fn routable_ip_rejects_garbage_guest_ip() {
        let i = info("sb-1", SandboxStatus::Running, Some("not-an-ip"));
        assert!(matches!(routable_ip(&i), Err(VmError::BadGuestIp { .. })));
    }

    #[test]
    fn terminal_states_are_reaped_and_transient_ones_are_not() {
        assert!(!is_terminal(&SandboxStatus::Provisioning));
        assert!(!is_terminal(&SandboxStatus::Running));
        assert!(!is_terminal(&SandboxStatus::Unknown));
        assert!(is_terminal(&SandboxStatus::Stopped));
        assert!(is_terminal(&SandboxStatus::Failed));
        assert!(is_terminal(&SandboxStatus::Paused));
        assert!(is_terminal(&SandboxStatus::ColdStored));
    }

    #[test]
    fn replica_names_round_trip_through_owner_of() {
        let n = replica_name("demo", 42);
        assert_eq!(owner_of(&n), Some("demo"));
        // Function ids containing dashes survive.
        let n = replica_name("my-fn-v2", 7);
        assert_eq!(owner_of(&n), Some("my-fn-v2"));
    }

    /// Regression: the daemon derives a VM's tap subnet by parsing the trailing
    /// name segment as hex. A decimal nonce parsed as 0 for anything containing
    /// a non-hex digit, collapsing every such VM onto 172.16.0.2 — where they
    /// collided and only one had working networking.
    #[test]
    fn replica_nonce_is_hex_to_stay_clear_of_subnet_derivation() {
        let n = replica_name("demo", 0xdead_beef);
        assert!(n.ends_with("0000deadbeef"), "got {n}");
        let nonce = n.rsplit('-').next().unwrap();
        assert!(
            u64::from_str_radix(nonce, 16).is_ok(),
            "the nonce must parse as hex; the daemon's subnet derivation depends on it"
        );
    }

    #[test]
    fn owner_of_ignores_foreign_sandboxes() {
        assert_eq!(owner_of("some-other-vm"), None);
        assert_eq!(owner_of("qfn"), None);
        assert_eq!(owner_of("qfn-"), None);
        // No nonce segment => not one of ours.
        assert_eq!(owner_of("qfn-demo"), None);
        // A different tool's prefix that happens to start the same way.
        assert_eq!(owner_of("qfnx-demo-000000000001"), None);
    }

    #[test]
    fn index_by_id_builds_a_lookup() {
        let idx = index_by_id(vec![
            info("sb-1", SandboxStatus::Running, Some("172.16.0.2")),
            info("sb-2", SandboxStatus::Stopped, None),
        ]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx["sb-1"].status, SandboxStatus::Running);
    }

    /// Regression: the firecracker serial path returns everything in `output`
    /// and leaves `stdout`/`stderr` empty. Reading `stdout` alone made every
    /// function on that backend look like it had produced no output at all.
    #[test]
    fn normalize_recovers_output_when_the_serial_path_leaves_stdout_empty() {
        let out = ExecOutput::normalize(
            String::new(),
            String::new(),
            "hello from the guest".into(),
            0,
        );
        assert_eq!(out.stdout, "hello from the guest");
        assert_eq!(out.stderr, "");
        assert!(out.succeeded());
    }

    #[test]
    fn normalize_prefers_the_explicit_streams_when_the_ssh_path_fills_them() {
        let out = ExecOutput::normalize(
            "real stdout".into(),
            "real stderr".into(),
            "real stdout\nreal stderr".into(),
            3,
        );
        assert_eq!(out.stdout, "real stdout");
        assert_eq!(out.stderr, "real stderr");
        assert_eq!(out.exit_code, 3);
        assert!(!out.succeeded());
    }

    /// A command that legitimately wrote only to stderr must not have that text
    /// duplicated into stdout by the fallback.
    #[test]
    fn normalize_does_not_duplicate_a_stderr_only_result() {
        let out = ExecOutput::normalize(String::new(), "warning".into(), "warning".into(), 0);
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, "warning");
    }

    #[test]
    fn operation_ids_match_what_the_daemon_will_accept() {
        assert!(valid_operation_id("0198f2a1b3c4-0000002a"));
        assert!(valid_operation_id("a.b_c-1"));
        assert!(!valid_operation_id(""));
        assert!(!valid_operation_id("has space"));
        assert!(!valid_operation_id("has/slash"));
        assert!(!valid_operation_id(&"x".repeat(161)));
        assert!(valid_operation_id(&"x".repeat(160)));
    }

    /// Regression: `is_finished` matched a positive list of terminal statuses
    /// containing `succeeded`. The daemon actually emits `completed`, so every
    /// invocation polled a finished operation until it hit its own timeout and
    /// was reported as a timeout — while its result sat there, complete.
    #[test]
    fn an_operation_is_finished_in_the_status_the_daemon_actually_emits() {
        let rec = |s: &str| ExecOperationRecord {
            operation_id: "op".into(),
            sandbox_id: "sb".into(),
            status: s.into(),
            result: None,
            error: None,
        };
        assert!(!rec("queued").is_finished());
        assert!(!rec("running").is_finished());
        assert!(rec("completed").is_finished(), "the real success status");
        assert!(rec("failed").is_finished());
    }

    /// An unrecognised status must read as terminal, not pending. Getting this
    /// backwards is what made the bug above cost a full timeout per invocation
    /// instead of surfacing immediately.
    #[test]
    fn an_unknown_status_is_terminal_rather_than_polled_forever() {
        let rec = ExecOperationRecord {
            operation_id: "op".into(),
            sandbox_id: "sb".into(),
            status: "some-future-status".into(),
            result: None,
            error: None,
        };
        assert!(rec.is_finished());
    }

    #[test]
    fn an_exec_operation_record_deserializes_from_the_daemons_camel_case() {
        let raw = r#"{
            "operationId": "inv-1",
            "sandboxId": "sb-1",
            "status": "succeeded",
            "command": "echo hi",
            "result": {"output":"hi\n","stdout":"","stderr":"","exit_code":0},
            "createdAt": "2026-07-19T00:00:00Z",
            "updatedAt": "2026-07-19T00:00:01Z"
        }"#;
        let rec: ExecOperationRecord = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(rec.operation_id, "inv-1");
        assert_eq!(rec.sandbox_id, "sb-1");
        assert!(rec.is_finished());
        let result = rec.result.expect("result present");
        assert_eq!(result.output, "hi\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn a_missing_sandbox_is_distinguishable_from_a_failed_command() {
        let gone = VmError::Sdk(heyo_sdk::HeyoError::NotFound("sb-1".into()));
        assert!(gone.is_sandbox_gone());

        let failed = VmError::ExecFailed {
            sandbox_id: "sb-1".into(),
            reason: "boom".into(),
        };
        assert!(!failed.is_sandbox_gone());
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(encode_segment("sb-1_a.b~c"), "sb-1_a.b~c");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("a b"), "a%20b");
    }

    /// Every VmManager method must fail cleanly against an unreachable daemon
    /// rather than panicking or hanging — this is what the reconcile loop sees
    /// when heyvmd is down, and it has to survive it.
    #[tokio::test]
    async fn a_dead_daemon_produces_errors_not_panics() {
        let vms = VmManager::new(Some("http://127.0.0.1:1".into()));
        assert!(vms.list().await.is_err());
        assert!(vms.system_usage().await.is_err());
        assert!(vms.kill("sb-1").await.is_err());
        assert!(vms.renew_ttl("sb-1", 60).await.is_err());
        assert!(
            vms.exec("sb-1", "true", None, None, Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(
            vms.start_exec_operation("sb-1", "op-1", "true", None)
                .await
                .is_err()
        );
        assert!(vms.poll_exec_operation("sb-1", "op-1").await.is_err());
    }
}
