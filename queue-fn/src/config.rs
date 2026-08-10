//! Function specs and process configuration.
//!
//! Two constraints shape the spec types, and both come from heyvm rather than
//! from anything queue-fn chose:
//!
//! 1. `SandboxInfo.guest_ip` is only populated for tap-networked
//!    firecracker/kvm on a local daemon, so `libvirt` is rejected at
//!    registration rather than allowed to boot and then prove unusable.
//! 2. The firecracker serial exec path wraps every command in a 30s
//!    `timeout()` that no request field can raise
//!    (mvm-ctrl/src/driver/firecracker.rs:1716). `timeout_secs` is therefore
//!    capped below that ceiling instead of being accepted and silently ignored.

use heyo_sdk::{SandboxDriver, SandboxSize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The guest-side wall-clock ceiling on a single exec.
///
/// `FirecrackerDriver::execute_via_serial` hard-codes
/// `timeout(Duration::from_secs(30), ..)`; the SDK's `CommandRunOptions.timeout`
/// bounds only the HTTP call, not the command. We cap below it so a function
/// that is going to be killed says so at registration rather than at 3am.
///
/// Raise this once the daemon plumbs a per-request timeout through
/// `ExecuteCommandRequest`.
pub const GUEST_EXEC_CEILING_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = GUEST_EXEC_CEILING_SECS - 5;

// A compile-time check rather than a test: if the ceiling is ever raised without
// the cap following it, specs would be accepted that the guest silently kills.
const _: () = assert!(MAX_TIMEOUT_SECS < GUEST_EXEC_CEILING_SECS);

/// Hard ceiling on a payload, independent of what a spec asks for.
///
/// Env values reach the guest inside a single wrapped command line written to
/// the serial console. Linux's tty line discipline buffers `N_TTY_BUF_SIZE`
/// (4096) bytes, so a payload materially larger than that risks being truncated
/// in a way that looks like a corrupt payload rather than a transport failure.
pub const PAYLOAD_CEILING_BYTES: usize = 4096;

/// The scheduler ticks at 30s, so a shorter interval could not be honoured.
pub const MIN_INTERVAL_SECS: u64 = 30;

fn default_admin_addr() -> String {
    "127.0.0.1:9494".into()
}
fn default_state_path() -> String {
    "queue-fn-state.json".into()
}
fn default_name() -> String {
    "queue-fn".into()
}
fn default_nats_url() -> String {
    "nats://127.0.0.1:4222".into()
}
fn default_subject_prefix() -> String {
    "qfn".into()
}
fn default_result_ring() -> usize {
    200
}
fn default_sync_invoke_timeout_secs() -> u64 {
    180
}
fn default_cloud_observe() -> bool {
    true
}
fn default_cloud_stream() -> String {
    crate::cloud::DEFAULT_STREAM.into()
}
fn default_cloud_subject() -> String {
    crate::cloud::DEFAULT_SUBJECT.into()
}
fn default_cloud_job_ring() -> usize {
    500
}
fn default_cloud_poll_secs() -> u64 {
    5
}

/// Process-level configuration, supplied via env rather than the admin API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QfnConfig {
    #[serde(default = "default_admin_addr")]
    pub admin_addr: String,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    /// Display name shown in the dashboard header and page title.
    #[serde(default = "default_name")]
    pub name: String,
    /// heyvm daemon base URL. Defaults to `HeyoClient::local()`'s target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_url: Option<String>,
    #[serde(default = "default_nats_url")]
    pub nats_url: String,
    /// Subject namespace. Streams derive from it: `<prefix>.invoke.<function>`,
    /// `<prefix>.result.<function>.<invocation>`, `<prefix>.dlq.<function>`.
    #[serde(default = "default_subject_prefix")]
    pub subject_prefix: String,
    /// Host ceiling on payload size. A spec may ask for less, never more.
    #[serde(default = "default_max_payload_bytes")]
    pub max_payload_bytes: usize,
    /// Recent invocations retained in memory per function, for the dashboard.
    #[serde(default = "default_result_ring")]
    pub result_ring: usize,
    /// How long a synchronous invoke waits for its result before giving up.
    /// Covers cold start *and* execution, so it must exceed both.
    #[serde(default = "default_sync_invoke_timeout_secs")]
    pub sync_invoke_timeout_secs: u64,
    /// Optional HTTP Basic Auth gate. Auth is enabled iff `password` is set;
    /// `user` defaults to `"admin"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_password: Option<String>,
    /// Extend the gate to function CRUD, not just the dashboard view.
    #[serde(default)]
    pub admin_auth: bool,
    /// Extend the gate to the invoke/enqueue endpoints. Separate from
    /// `admin_auth` because "anyone may call my functions, nobody may edit them"
    /// is a coherent posture and the reverse is not.
    #[serde(default)]
    pub invoke_auth: bool,
    /// Watch heyo cloud's queue on the same NATS and render it on the dashboard.
    ///
    /// On by default because it is read-only in both directions — a core-NATS
    /// subscription that consumes nothing, and management reads that create
    /// nothing (see `cloud`'s module doc) — and because a NATS with no
    /// `HEYO_SANDBOX` on it simply reports the stream as absent. There is
    /// nothing to break by leaving it on, and an operator who shares a bus and
    /// has to discover a flag to see half of it is being under-served.
    #[serde(default = "default_cloud_observe")]
    pub cloud_observe: bool,
    /// The cloud's JetStream stream, for the backlog gauges.
    #[serde(default = "default_cloud_stream")]
    pub cloud_stream: String,
    /// The subject the observer subscribes to. Must cover the stream's subjects.
    #[serde(default = "default_cloud_subject")]
    pub cloud_subject: String,
    /// Recent cloud jobs retained in memory for the dashboard.
    #[serde(default = "default_cloud_job_ring")]
    pub cloud_job_ring: usize,
    /// How often the cloud's stream and consumers are polled. Slower than the
    /// 2s reconcile tick on purpose: these are somebody else's API requests,
    /// they show up in *their* account's `api_requests_total`, and a queue whose
    /// jobs run for minutes does not need a 2s gauge.
    #[serde(default = "default_cloud_poll_secs")]
    pub cloud_poll_secs: u64,
}

fn default_max_payload_bytes() -> usize {
    PAYLOAD_CEILING_BYTES
}

impl Default for QfnConfig {
    fn default() -> Self {
        Self {
            admin_addr: default_admin_addr(),
            state_path: default_state_path(),
            name: default_name(),
            daemon_url: None,
            nats_url: default_nats_url(),
            subject_prefix: default_subject_prefix(),
            max_payload_bytes: default_max_payload_bytes(),
            result_ring: default_result_ring(),
            sync_invoke_timeout_secs: default_sync_invoke_timeout_secs(),
            dashboard_user: None,
            dashboard_password: None,
            admin_auth: false,
            invoke_auth: false,
            cloud_observe: default_cloud_observe(),
            cloud_stream: default_cloud_stream(),
            cloud_subject: default_cloud_subject(),
            cloud_job_ring: default_cloud_job_ring(),
            cloud_poll_secs: default_cloud_poll_secs(),
        }
    }
}

fn default_target_concurrency() -> u32 {
    2
}
fn default_max_replicas() -> u32 {
    5
}
fn default_scale_to_zero_after_secs() -> u64 {
    300
}
fn default_cold_start_timeout_secs() -> u64 {
    120
}
fn default_drain_timeout_secs() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ScalingPolicy {
    #[serde(default)]
    pub min_replicas: u32,
    #[serde(default = "default_max_replicas")]
    pub max_replicas: u32,
    /// Idle-but-ready spares kept above what current demand requires.
    #[serde(default)]
    pub warm_pool: u32,
    /// Invocations queue-fn is willing to line up behind one VM.
    ///
    /// This is **not** parallelism. heyvm serialises exec per sandbox — it
    /// removes the handle for the duration of a call
    /// (mvm-ctrl/src/sandbox.rs:3648) and a concurrent second exec gets
    /// `SandboxNotFound` rather than queueing — so a VM runs exactly one
    /// invocation at a time and this is how deep its backlog may get.
    ///
    /// 10 queued events at a target of 2 sizes the fleet to 5 VMs, each running
    /// its two back to back.
    #[serde(default = "default_target_concurrency")]
    pub target_concurrency: u32,
    #[serde(default = "default_scale_to_zero_after_secs")]
    pub scale_to_zero_after_secs: u64,
    /// How long a synchronous invoke waits for a VM to boot before 503.
    #[serde(default = "default_cold_start_timeout_secs")]
    pub cold_start_timeout_secs: u64,
    /// How long a draining VM may keep finishing its current invocation before
    /// it is killed anyway. Must outlast one exec or a drain truncates work.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
}

impl Default for ScalingPolicy {
    fn default() -> Self {
        Self {
            min_replicas: 0,
            max_replicas: default_max_replicas(),
            warm_pool: 0,
            target_concurrency: default_target_concurrency(),
            scale_to_zero_after_secs: default_scale_to_zero_after_secs(),
            cold_start_timeout_secs: default_cold_start_timeout_secs(),
            drain_timeout_secs: default_drain_timeout_secs(),
        }
    }
}

fn default_max_attempts() -> u32 {
    3
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RetryPolicy {
    /// Total delivery attempts, including the first. Becomes the consumer's
    /// `max_deliver`; exhausting it sends the event to the DLQ.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
        }
    }
}

/// How the event payload reaches the guest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadMode {
    /// Base64 in `QFN_PAYLOAD_B64` only. The safe default.
    #[default]
    Env,
    /// Also substituted into the command via `{{payload}}`. Sharp: after
    /// substitution the command is handed to `sh -lc`, so the payload becomes
    /// code unless quoted — see `invoke::sh_quote`.
    Template,
    Both,
}

fn default_timeout_secs() -> u64 {
    MAX_TIMEOUT_SECS
}

/// What to run, and how the event reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExecSpec {
    /// Shell command run in the guest, under `sh -lc`.
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// Extra env for every invocation, merged under the reserved `QFN_*` keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_payload_bytes")]
    pub max_payload_bytes: usize,
    #[serde(default)]
    pub payload_mode: PayloadMode,
}

/// A time-based trigger. Deliberately not cron: a parser is either ~400 lines or
/// a dependency tree (uuid + chrono + a job store) for expressiveness nothing
/// has asked for. Both variants publish to the same subject a caller would.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSpec {
    /// Fire every N seconds, counted from process start.
    Interval {
        every_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    /// Fire at fixed `HH:MM` times of day, UTC, minute resolution.
    DailyAt {
        times: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
}

impl TriggerSpec {
    pub fn payload(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Interval { payload, .. } | Self::DailyAt { payload, .. } => payload.as_ref(),
        }
    }
}

/// Parse `HH:MM` into minutes since UTC midnight.
///
/// Strict on purpose: `9:00`, `09:00:00`, and `25:00` are all rejected rather
/// than coerced, so a typo surfaces at registration instead of as a trigger that
/// silently never fires.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// The VM template. Mirrors `SandboxCreateOptions` minus what queue-fn owns:
/// `name` is generated per replica, and `wait_for_ready` is always zero because
/// the autoscaler polls readiness itself rather than blocking its reconcile loop.
///
/// No `port` (nothing is proxied to the guest) and no `setup_hooks` (code is
/// baked into the image; a hook that fetches code turns every cold start into a
/// network install).
///
/// `PartialEq` is load-bearing: an in-place spec edit keeps the running pool only
/// when the VM *template* is unchanged, so the update path compares old and new.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VmSpec {
    /// Must be `firecracker` or `kvm`; `libvirt` is rejected at registration.
    pub driver: SandboxDriver,
    /// Defaults to `ubuntu:24.04` daemon-side when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_class: Option<SandboxSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_size_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<HashMap<String, String>>,
    /// Backstop TTL so VMs die on their own if this process crashes and never
    /// reaps them. Renewed by the autoscaler while it is alive.
    #[serde(default = "default_vm_ttl_secs")]
    pub ttl_seconds: u64,
}

fn default_vm_ttl_secs() -> u64 {
    3600
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FunctionSpec {
    pub id: String,
    pub vm: VmSpec,
    pub exec: ExecSpec,
    #[serde(default)]
    pub scaling: ScalingPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<TriggerSpec>,
    #[serde(default)]
    pub retry: RetryPolicy,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    EmptyId,
    /// Ids become NATS subject tokens and durable consumer names, so the
    /// alphabet is restricted rather than escaped.
    BadId(String),
    EmptyCommand,
    UnsupportedDriver(SandboxDriver),
    BadReplicaRange { min: u32, max: u32 },
    ZeroTargetConcurrency,
    ZeroMaxAttempts,
    ZeroTimeout,
    TimeoutExceedsGuestCeiling { secs: u64 },
    PayloadLimitTooLarge { bytes: usize },
    ReservedEnvKey(String),
    InvalidEnvKey(String),
    BadTriggerTime(String),
    NoTriggerTimes,
    IntervalTooShort { secs: u64 },
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "function id must not be empty"),
            Self::BadId(id) => write!(
                f,
                "function id {id:?} must match [a-zA-Z0-9_-]+: it becomes a NATS \
                 subject token and a durable consumer name, neither of which is escaped"
            ),
            Self::EmptyCommand => write!(f, "exec.command must not be empty"),
            Self::UnsupportedDriver(d) => write!(
                f,
                "driver {d:?} is not supported: queue-fn requires a guest IP to \
                 confirm a VM is reachable, which the daemon only exposes for \
                 tap-networked firecracker/kvm backends"
            ),
            Self::BadReplicaRange { min, max } => {
                write!(f, "min_replicas ({min}) exceeds max_replicas ({max})")
            }
            Self::ZeroTargetConcurrency => write!(f, "target_concurrency must be greater than 0"),
            Self::ZeroMaxAttempts => write!(f, "retry.max_attempts must be greater than 0"),
            Self::ZeroTimeout => write!(f, "exec.timeout_secs must be greater than 0"),
            Self::TimeoutExceedsGuestCeiling { secs } => write!(
                f,
                "exec.timeout_secs ({secs}) exceeds the maximum of {MAX_TIMEOUT_SECS}: the \
                 firecracker serial exec path hard-codes a {GUEST_EXEC_CEILING_SECS}s timeout \
                 that no request field can raise, so a longer timeout would be \
                 accepted here and ignored by the guest"
            ),
            Self::PayloadLimitTooLarge { bytes } => write!(
                f,
                "exec.max_payload_bytes ({bytes}) exceeds the host ceiling of \
                 {PAYLOAD_CEILING_BYTES}: the payload crosses a serial console as one \
                 command line, and the tty line discipline buffers 4096 bytes"
            ),
            Self::ReservedEnvKey(k) => write!(
                f,
                "env key {k:?} is reserved: QFN_* is set per invocation and a spec \
                 value would be overwritten"
            ),
            Self::InvalidEnvKey(k) => write!(
                f,
                "env key {k:?} must match [A-Za-z_][A-Za-z0-9_]*: the daemon quotes env \
                 *values* but not keys (mvm-ctrl/src/driver/driver.rs:563), so a key \
                 with a space or a `;` reaches the guest as shell metacharacters"
            ),
            Self::BadTriggerTime(t) => {
                write!(f, "trigger time {t:?} must be zero-padded HH:MM, 24-hour UTC")
            }
            Self::NoTriggerTimes => write!(f, "a daily_at trigger must list at least one time"),
            Self::IntervalTooShort { secs } => write!(
                f,
                "trigger interval ({secs}s) is below the minimum of {MIN_INTERVAL_SECS}s: \
                 the scheduler ticks at {MIN_INTERVAL_SECS}s and could not honour it"
            ),
        }
    }
}

impl std::error::Error for SpecError {}

/// Env keys queue-fn sets on every invocation. A spec may not shadow them.
pub const RESERVED_ENV_PREFIX: &str = "QFN_";

/// `[A-Za-z_][A-Za-z0-9_]*`.
///
/// heyvm builds `env K='v' .. sh -lc '..'` and single-quotes only the value
/// (mvm-ctrl/src/driver/driver.rs:559-566), so an unquoted key is injection.
pub fn valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Ids are interpolated into NATS subjects and durable consumer names unescaped.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl FunctionSpec {
    /// Rejects specs the runtime could not honour.
    ///
    /// Everything here fails at registration rather than at invocation time,
    /// because every one of these mistakes is otherwise silent: a libvirt VM
    /// boots and is unreachable, an over-long timeout is truncated by the guest,
    /// an oversized payload is truncated by a tty, and a bad env key is a shell
    /// injection that works until it doesn't.
    pub fn validate(&self) -> Result<(), SpecError> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err(SpecError::EmptyId);
        }
        if !valid_id(id) {
            return Err(SpecError::BadId(self.id.clone()));
        }
        if self.exec.command.trim().is_empty() {
            return Err(SpecError::EmptyCommand);
        }
        if !matches!(
            self.vm.driver,
            SandboxDriver::Firecracker | SandboxDriver::Kvm
        ) {
            return Err(SpecError::UnsupportedDriver(self.vm.driver));
        }
        if self.scaling.min_replicas > self.scaling.max_replicas {
            return Err(SpecError::BadReplicaRange {
                min: self.scaling.min_replicas,
                max: self.scaling.max_replicas,
            });
        }
        if self.scaling.target_concurrency == 0 {
            return Err(SpecError::ZeroTargetConcurrency);
        }
        if self.retry.max_attempts == 0 {
            return Err(SpecError::ZeroMaxAttempts);
        }
        if self.exec.timeout_secs == 0 {
            return Err(SpecError::ZeroTimeout);
        }
        if self.exec.timeout_secs > MAX_TIMEOUT_SECS {
            return Err(SpecError::TimeoutExceedsGuestCeiling {
                secs: self.exec.timeout_secs,
            });
        }
        if self.exec.max_payload_bytes > PAYLOAD_CEILING_BYTES {
            return Err(SpecError::PayloadLimitTooLarge {
                bytes: self.exec.max_payload_bytes,
            });
        }
        if let Some(env) = &self.exec.env {
            for k in env.keys() {
                if k.starts_with(RESERVED_ENV_PREFIX) {
                    return Err(SpecError::ReservedEnvKey(k.clone()));
                }
                if !valid_env_key(k) {
                    return Err(SpecError::InvalidEnvKey(k.clone()));
                }
            }
        }
        for t in &self.triggers {
            match t {
                TriggerSpec::Interval { every_secs, .. } => {
                    if *every_secs < MIN_INTERVAL_SECS {
                        return Err(SpecError::IntervalTooShort { secs: *every_secs });
                    }
                }
                TriggerSpec::DailyAt { times, .. } => {
                    if times.is_empty() {
                        return Err(SpecError::NoTriggerTimes);
                    }
                    for t in times {
                        if parse_hhmm(t).is_none() {
                            return Err(SpecError::BadTriggerTime(t.clone()));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> FunctionSpec {
        FunctionSpec {
            id: "demo".into(),
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
                command: "echo hi".into(),
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

    #[test]
    fn accepts_supported_drivers() {
        let mut s = spec();
        s.vm.driver = SandboxDriver::Firecracker;
        assert!(s.validate().is_ok());
        s.vm.driver = SandboxDriver::Kvm;
        assert!(s.validate().is_ok());
    }

    #[test]
    fn rejects_libvirt_because_it_has_no_guest_ip() {
        let mut s = spec();
        s.vm.driver = SandboxDriver::Libvirt;
        assert_eq!(
            s.validate(),
            Err(SpecError::UnsupportedDriver(SandboxDriver::Libvirt))
        );
    }

    #[test]
    fn rejects_a_timeout_past_the_guest_exec_ceiling() {
        let mut s = spec();
        s.exec.timeout_secs = MAX_TIMEOUT_SECS + 1;
        assert_eq!(
            s.validate(),
            Err(SpecError::TimeoutExceedsGuestCeiling {
                secs: MAX_TIMEOUT_SECS + 1
            })
        );
        s.exec.timeout_secs = MAX_TIMEOUT_SECS;
        assert!(s.validate().is_ok(), "the ceiling itself is allowed");
    }

    #[test]
    fn rejects_a_payload_limit_above_the_host_ceiling() {
        let mut s = spec();
        s.exec.max_payload_bytes = PAYLOAD_CEILING_BYTES + 1;
        assert_eq!(
            s.validate(),
            Err(SpecError::PayloadLimitTooLarge {
                bytes: PAYLOAD_CEILING_BYTES + 1
            })
        );
    }

    #[test]
    fn rejects_degenerate_specs() {
        let mut s = spec();
        s.id = "  ".into();
        assert_eq!(s.validate(), Err(SpecError::EmptyId));

        let mut s = spec();
        s.exec.command = "   ".into();
        assert_eq!(s.validate(), Err(SpecError::EmptyCommand));

        let mut s = spec();
        s.scaling.min_replicas = 3;
        s.scaling.max_replicas = 2;
        assert_eq!(
            s.validate(),
            Err(SpecError::BadReplicaRange { min: 3, max: 2 })
        );

        let mut s = spec();
        s.scaling.target_concurrency = 0;
        assert_eq!(s.validate(), Err(SpecError::ZeroTargetConcurrency));

        let mut s = spec();
        s.retry.max_attempts = 0;
        assert_eq!(s.validate(), Err(SpecError::ZeroMaxAttempts));

        let mut s = spec();
        s.exec.timeout_secs = 0;
        assert_eq!(s.validate(), Err(SpecError::ZeroTimeout));
    }

    /// Regression: the id is interpolated into `qfn.invoke.<id>` and into the
    /// durable consumer name with no escaping. An id containing a dot would
    /// silently widen the filter subject; one containing a space would produce a
    /// consumer name the server rejects at create time, long after registration
    /// reported success.
    #[test]
    fn rejects_ids_that_would_corrupt_a_subject_or_consumer_name() {
        for bad in ["a.b", "a b", "a*", "a>", "a/b", ""] {
            let mut s = spec();
            s.id = bad.into();
            assert!(
                s.validate().is_err(),
                "id {bad:?} should be rejected but was accepted"
            );
        }
        for good in ["a", "my-fn", "my_fn", "fn123"] {
            let mut s = spec();
            s.id = good.into();
            assert!(s.validate().is_ok(), "id {good:?} should be accepted");
        }
    }

    /// Regression: heyvm single-quotes env *values* but writes keys verbatim
    /// into `env K=v sh -lc ..`, so a key like `X;rm -rf /` is not a bad
    /// variable name — it is a second command that runs as root in the guest.
    #[test]
    fn rejects_env_keys_that_are_shell_metacharacters() {
        for bad in ["X;rm -rf /", "A B", "1LEADING_DIGIT", "", "a-b", "$X"] {
            let mut s = spec();
            s.exec.env = Some(HashMap::from([(bad.to_string(), "v".to_string())]));
            assert!(
                matches!(s.validate(), Err(SpecError::InvalidEnvKey(_))),
                "env key {bad:?} should be rejected but was accepted"
            );
        }
        for good in ["X", "_x", "MY_VAR2"] {
            let mut s = spec();
            s.exec.env = Some(HashMap::from([(good.to_string(), "v".to_string())]));
            assert!(s.validate().is_ok(), "env key {good:?} should be accepted");
        }
    }

    #[test]
    fn rejects_specs_that_shadow_the_reserved_env_namespace() {
        let mut s = spec();
        s.exec.env = Some(HashMap::from([(
            "QFN_PAYLOAD_B64".to_string(),
            "spoofed".to_string(),
        )]));
        assert_eq!(
            s.validate(),
            Err(SpecError::ReservedEnvKey("QFN_PAYLOAD_B64".into()))
        );
    }

    #[test]
    fn parses_only_zero_padded_24_hour_times() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("09:30"), Some(570));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        for bad in ["9:00", "09:00:00", "24:00", "23:60", "0900", "", "ab:cd"] {
            assert_eq!(parse_hhmm(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn rejects_triggers_the_scheduler_could_not_honour() {
        let mut s = spec();
        s.triggers = vec![TriggerSpec::Interval {
            every_secs: 5,
            payload: None,
        }];
        assert_eq!(s.validate(), Err(SpecError::IntervalTooShort { secs: 5 }));

        let mut s = spec();
        s.triggers = vec![TriggerSpec::DailyAt {
            times: vec![],
            payload: None,
        }];
        assert_eq!(s.validate(), Err(SpecError::NoTriggerTimes));

        let mut s = spec();
        s.triggers = vec![TriggerSpec::DailyAt {
            times: vec!["9:00".into()],
            payload: None,
        }];
        assert_eq!(s.validate(), Err(SpecError::BadTriggerTime("9:00".into())));
    }

    #[test]
    fn a_spec_round_trips_through_json() {
        let s = spec();
        let json = serde_json::to_string(&s).expect("serialize");
        let back: FunctionSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn defaults_fill_in_a_minimal_spec() {
        let s: FunctionSpec = serde_json::from_str(
            r#"{"id":"m","vm":{"driver":"firecracker"},"exec":{"command":"true"}}"#,
        )
        .expect("deserialize");
        assert!(s.validate().is_ok());
        assert_eq!(s.scaling.target_concurrency, default_target_concurrency());
        assert_eq!(s.exec.timeout_secs, MAX_TIMEOUT_SECS);
        assert_eq!(s.exec.payload_mode, PayloadMode::Env);
        assert_eq!(s.retry.max_attempts, default_max_attempts());
        assert_eq!(s.vm.ttl_seconds, default_vm_ttl_secs());
    }

    /// The default timeout must itself be legal, or every minimal spec is
    /// rejected by the very check that produced its default.
    #[test]
    fn the_default_timeout_is_within_the_guest_ceiling() {
        assert!(default_timeout_secs() <= MAX_TIMEOUT_SECS);
    }
}
