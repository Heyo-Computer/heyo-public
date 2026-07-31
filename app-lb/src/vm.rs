//! The heyo-sdk boundary.
//!
//! Everything that knows about sandboxes lives here so the traps are in one
//! place. Two are worth stating up front, because both fail silently:
//!
//! 1. `Sandbox::wait_for_ready` returning `Ok` does **not** mean the VM is
//!    usable — its match has a `_ => return Ok(info)` arm, so `Stopped`,
//!    `Paused`, and `ColdStored` all come back `Ok`. Against a local daemon a
//!    broken VM surfaces as `Stopped`, never `Failed`. Always check the status.
//! 2. `guest_ip` is only populated for tap-networked Firecracker/KVM on a local
//!    daemon. It is the only address we can route to, so its absence is a hard
//!    error, not something to retry.

use crate::config::VmSpec;
use heyo_sdk::{
    CommandResult, CommandRunOptions, HeyoClient, HeyoClientOptions, RequestOptions, Sandbox,
    SandboxCreateOptions, SandboxDriver, SandboxInfo, SandboxStatus, ShellOptions, ShellSession,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Marks a sandbox as ours and records which deployment owns it, so VMs can be
/// re-adopted after an LB restart instead of being orphaned or duplicated.
pub const OWNER_PREFIX: &str = "applb";

/// Sandbox name for a replica: `applb-<deployment>-<nonce>`.
///
/// The nonce is *hex* on purpose. The daemon derives a VM's tap subnet from the
/// sandbox id via `from_str_radix(hex, 16).unwrap_or(0)`, so an id that isn't
/// valid hex silently collapses to `172.16.0.2` and collides with every other
/// non-hex VM. The daemon assigns the id, not us, but keeping our names hex-safe
/// avoids feeding it anything pathological.
pub fn replica_name(deployment_id: &str, nonce: u64) -> String {
    format!("{OWNER_PREFIX}-{deployment_id}-{nonce:012x}")
}

/// Which deployment a sandbox belongs to, or `None` if it isn't ours.
pub fn owner_of(sandbox_name: &str) -> Option<&str> {
    let rest = sandbox_name.strip_prefix(OWNER_PREFIX)?.strip_prefix('-')?;
    // Trailing `-<nonce>` is ours; everything before it is the deployment id,
    // which may itself contain dashes.
    let (id, _nonce) = rest.rsplit_once('-')?;
    (!id.is_empty()).then_some(id)
}

#[derive(Debug)]
pub enum VmError {
    Sdk(heyo_sdk::HeyoError),
    /// The daemon reported a state that means this VM will never serve.
    NotRunning {
        sandbox_id: String,
        status: SandboxStatus,
        reason: Option<String>,
    },
    /// No `guest_ip`: the VM is unroutable by us. Happens for non-tap backends
    /// or a remote daemon.
    NoGuestIp {
        sandbox_id: String,
    },
    /// `guest_ip` was not parseable as an IP address.
    BadGuestIp {
        sandbox_id: String,
        value: String,
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
            Self::BadGuestIp { sandbox_id, value } => {
                write!(
                    f,
                    "sandbox {sandbox_id} reported unparseable guest_ip {value:?}"
                )
            }
        }
    }
}

impl std::error::Error for VmError {}

impl From<heyo_sdk::HeyoError> for VmError {
    fn from(e: heyo_sdk::HeyoError) -> Self {
        Self::Sdk(e)
    }
}

/// Live host + per-sandbox resource usage from the daemon's `GET /system/usage`.
///
/// The daemon serves this from a background poller (~5s) sampling the host
/// processes that back each VM, so it is cheap to fetch — a cache read, not a
/// per-VM probe — and safe to poll on the reconcile tick. Only backends with a
/// local host process are reported, which for app-lb (Firecracker/KVM on a local
/// daemon) is all of them. Disk and network throughput are not exposed.
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
    /// its reconcile loop for the ~minutes a VM can take. Readiness is tracked
    /// across subsequent ticks instead.
    pub async fn create(&self, spec: &VmSpec, name: String) -> Result<Sandbox, VmError> {
        debug_assert!(
            matches!(spec.driver, SandboxDriver::Firecracker | SandboxDriver::Kvm),
            "DeploymentSpec::validate must reject other drivers before reaching here",
        );

        // The proxied port must be open, plus whatever else the spec asks for.
        let mut open_ports = spec.open_ports.clone();
        if !open_ports.contains(&spec.port) {
            open_ports.push(spec.port);
        }

        let options = SandboxCreateOptions {
            name: Some(name),
            driver: Some(spec.driver),
            image: spec.image.clone(),
            start_command: spec.start_command.clone(),
            size_class: spec.size_class,
            disk_size_gb: spec.disk_size_gb,
            working_directory: spec.working_directory.clone(),
            env_vars: spec.env_vars.clone(),
            setup_hooks: spec.setup_hooks.clone(),
            open_ports,
            // Backstop: if this LB dies without reaping, the VM still expires.
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

    /// Fetch the daemon's cached host + per-sandbox usage snapshot.
    ///
    /// Like `list`, this is a single daemon round-trip meant for once-per-tick
    /// use — the daemon already samples in the background, so this just reads
    /// its cache. There is no typed SDK method for it, so we go through the
    /// client's generic request helper.
    pub async fn system_usage(&self) -> Result<SystemUsage, VmError> {
        let client = HeyoClient::new(self.opts.clone())?;
        let usage = client
            .request::<SystemUsage>(
                http::Method::GET,
                "/system/usage",
                None::<&serde_json::Value>,
                RequestOptions::default(),
            )
            .await?;
        Ok(usage)
    }

    /// Destroy a VM. A VM the daemon has already forgotten counts as killed.
    pub async fn kill(&self, sandbox_id: &str) -> Result<(), VmError> {
        match self.connect(sandbox_id.to_string()) {
            Ok(sandbox) => match sandbox.kill().await {
                Ok(()) => Ok(()),
                Err(heyo_sdk::HeyoError::NotFound(_)) => Ok(()),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e),
        }
    }

    /// Keep a VM's TTL backstop from expiring under a long-lived deployment.
    pub async fn renew_ttl(&self, sandbox_id: &str, ttl_seconds: u64) -> Result<(), VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        match sandbox.set_ttl(ttl_seconds).await {
            Ok(()) => Ok(()),
            Err(heyo_sdk::HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Run one command in a VM and wait for it to finish.
    ///
    /// `POST /sandbox/:id/exec` daemon-side, which runs the string through
    /// `sh -c`. There is no streaming and no way to cancel: whatever the guest
    /// prints is buffered until it exits, so `options.timeout` is the only thing
    /// bounding a command that never returns.
    pub async fn exec(
        &self,
        sandbox_id: &str,
        command: &str,
        options: CommandRunOptions,
    ) -> Result<CommandResult, VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        Ok(sandbox.commands().run(command, options).await?)
    }

    /// Open an interactive PTY on a VM.
    ///
    /// Returns once the daemon has answered the session handshake, so a failure
    /// to attach surfaces here rather than as a socket that closes immediately.
    /// The session carries its own sequence/ack and reconnect handling.
    pub async fn shell(
        &self,
        sandbox_id: &str,
        options: ShellOptions,
    ) -> Result<ShellSession, VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        Ok(sandbox.shell(options).await?)
    }

    /// Stop a VM without destroying it — `scaling.idle_action: retain`.
    ///
    /// The sandbox record and its `/workspace` data disk survive; memory and any
    /// rootfs writes do not, because mvm-ctrl recopies the rootfs from the base
    /// image on the next boot.
    ///
    /// **A stopped sandbox disappears from [`list`](Self::list).** mvm-ctrl's
    /// `stop` removes it from the in-memory map that backs `GET /sandboxes`, so
    /// after this returns the daemon will not mention this sandbox again until
    /// it is resumed. Whoever calls this owns remembering the id — see
    /// [`list_inactive`](Self::list_inactive) — or the VM is leaked, still
    /// holding its disk, with nothing left to reap it.
    pub async fn suspend(&self, sandbox_id: &str) -> Result<(), VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        match sandbox.stop().await {
            Ok(()) => Ok(()),
            // Already gone: the caller wanted it not running, and it isn't.
            Err(heyo_sdk::HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Restart a suspended VM and wait for the daemon to call it `Running`.
    ///
    /// The status check is not optional. `Sandbox::start` answers as soon as the
    /// daemon accepts the request, and `wait_for_ready` cannot be used to
    /// confirm the outcome — it has a `_ => return Ok(info)` arm, so a sandbox
    /// that failed to come back reports `Stopped` *and* `Ok`. Reading the status
    /// ourselves is what turns that into an error.
    ///
    /// Returns the info for the resumed VM; the caller still has to health-probe
    /// the guest, exactly as it would after a cold boot.
    pub async fn resume(&self, sandbox_id: &str) -> Result<SandboxInfo, VmError> {
        let sandbox = self.connect(sandbox_id.to_string())?;
        sandbox.start().await?;
        let info = sandbox.info().await?;
        if info.status != SandboxStatus::Running {
            return Err(VmError::NotRunning {
                sandbox_id: info.id.clone(),
                status: info.status.clone(),
                reason: info.error_message.clone(),
            });
        }
        Ok(info)
    }

    /// Every sandbox the daemon has *stopped but not deleted*.
    ///
    /// The counterpart to [`list`](Self::list), which only reports running ones.
    /// There is no SDK method for this, so it goes through the client's generic
    /// request helper against `GET /sandboxes/inactive`.
    ///
    /// **Expensive.** The daemon answers it by walking its persistence directory
    /// and loading metadata per sandbox — every sandbox ever created, not just
    /// the stopped ones — and it is cursor-paginated with a default page of 10.
    /// This pages to the end, so it belongs on a slow sweep and never on the
    /// reconcile tick. `max_pages` bounds a cursor that fails to advance.
    pub async fn list_inactive(&self) -> Result<Vec<SandboxInfo>, VmError> {
        const PAGE: usize = 200;
        const MAX_PAGES: usize = 200;

        let client = HeyoClient::new(self.opts.clone())?;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let path = match &cursor {
                Some(c) => format!("/sandboxes/inactive?count={PAGE}&cursor={c}"),
                None => format!("/sandboxes/inactive?count={PAGE}"),
            };
            let page = client
                .request::<InactivePage>(
                    http::Method::GET,
                    &path,
                    None::<&serde_json::Value>,
                    RequestOptions::default(),
                )
                .await?;
            let empty = page.sandboxes.is_empty();
            out.extend(page.sandboxes);
            match page.next_cursor {
                // A cursor that repeats itself would loop forever.
                Some(next) if !empty && Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
        tracing::warn!(
            pages = MAX_PAGES,
            "inactive sandbox listing did not terminate; treating it as complete",
        );
        Ok(out)
    }
}

/// One page of `GET /sandboxes/inactive`.
#[derive(Debug, Deserialize)]
struct InactivePage {
    #[serde(default)]
    sandboxes: Vec<SandboxInfo>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Extract the routable address, rejecting anything we could not proxy to.
///
/// This is where the two SDK traps are enforced together: a VM is only routable
/// if it is *actually* `Running` and *actually* has a `guest_ip`.
pub fn routable_addr(info: &SandboxInfo, port: u16) -> Result<SocketAddr, VmError> {
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
    let ip: IpAddr = raw.parse().map_err(|_| VmError::BadGuestIp {
        sandbox_id: info.id.clone(),
        value: raw.to_string(),
    })?;
    Ok(SocketAddr::new(ip, port))
}

/// Whether a status means the VM will never serve and should be reaped, as
/// opposed to still being on its way up.
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
    fn routable_addr_accepts_running_vm_with_guest_ip() {
        let i = info("sb-1", SandboxStatus::Running, Some("172.16.0.2"));
        assert_eq!(
            routable_addr(&i, 8080).unwrap(),
            "172.16.0.2:8080".parse::<SocketAddr>().unwrap()
        );
    }

    /// The trap: wait_for_ready returns Ok for a Stopped VM, so this check is
    /// the only thing standing between a dead VM and the pool.
    #[test]
    fn routable_addr_rejects_non_running_even_with_an_ip() {
        for status in [
            SandboxStatus::Stopped,
            SandboxStatus::Paused,
            SandboxStatus::Provisioning,
            SandboxStatus::Failed,
            SandboxStatus::ColdStored,
        ] {
            let i = info("sb-1", status, Some("172.16.0.2"));
            assert!(
                matches!(routable_addr(&i, 8080), Err(VmError::NotRunning { .. })),
                "expected NotRunning",
            );
        }
    }

    #[test]
    fn routable_addr_rejects_missing_or_empty_guest_ip() {
        let i = info("sb-1", SandboxStatus::Running, None);
        assert!(matches!(
            routable_addr(&i, 8080),
            Err(VmError::NoGuestIp { .. })
        ));

        let i = info("sb-1", SandboxStatus::Running, Some(""));
        assert!(matches!(
            routable_addr(&i, 8080),
            Err(VmError::NoGuestIp { .. })
        ));
    }

    #[test]
    fn routable_addr_rejects_garbage_guest_ip() {
        let i = info("sb-1", SandboxStatus::Running, Some("not-an-ip"));
        assert!(matches!(
            routable_addr(&i, 8080),
            Err(VmError::BadGuestIp { .. })
        ));
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
        // Deployment ids containing dashes survive.
        let n = replica_name("my-app-v2", 7);
        assert_eq!(owner_of(&n), Some("my-app-v2"));
    }

    #[test]
    fn replica_nonce_is_hex_to_stay_clear_of_subnet_derivation() {
        let n = replica_name("demo", 0xdead_beef);
        assert!(n.ends_with("0000deadbeef"), "got {n}");
    }

    #[test]
    fn owner_of_ignores_foreign_sandboxes() {
        assert_eq!(owner_of("some-other-vm"), None);
        assert_eq!(owner_of("applb"), None);
        assert_eq!(owner_of("applb-"), None);
        // No nonce segment => not one of ours.
        assert_eq!(owner_of("applb-demo"), None);
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
}
