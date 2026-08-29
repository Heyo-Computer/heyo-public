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
//! 3. `SandboxCreateOptions` cannot express guest mounts, so [`VmManager::create`]
//!    posts the create body itself rather than calling `Sandbox::create`. The
//!    body is still the SDK's own serialization of that struct — only the
//!    `mounts` array and the four defaults the SDK would have filled in are
//!    added here. See [`sdk_create_defaults`].

use crate::config::{DeploymentSpec, VmSpec};
use crate::mounts::MountStore;
use heyo_sdk::{
    CommandResult, CommandRunOptions, HeyoClient, HeyoClientOptions, RequestOptions, Sandbox,
    SandboxCreateOptions, SandboxDriver, SandboxInfo, SandboxStatus, ShellOptions, ShellSession,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
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
    /// The create body could not be serialized. Structurally impossible with the
    /// types involved — every map key is a `String` and no field is a float —
    /// but the alternative to a variant here is a panic on the autoscaler's
    /// reconcile tick.
    BadCreateBody(String),
    /// A guest mount has no tree on this host, so the VM would boot without the
    /// data its spec says it has. Refused rather than created: a replica missing
    /// a mount is a replica that answers health checks and then fails on the
    /// first request that needs the data, which is the failure mode this whole
    /// path exists to avoid.
    MountNotPulled {
        path: String,
        digest: Option<String>,
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
            Self::BadCreateBody(e) => write!(f, "could not serialize the create request: {e}"),
            Self::MountNotPulled { path, digest } => match digest {
                Some(d) => write!(
                    f,
                    "the tree for guest mount {path} ({d}) is not on this host; run \
                     `POST /deployments/<id>/mounts/pull` to fetch it, or point the mount at \
                     a digest this host still holds"
                ),
                None => write!(
                    f,
                    "guest mount {path} has never been pulled; run \
                     `POST /deployments/<id>/mounts/pull` to resolve its ref and unpack it"
                ),
            },
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

/// How long to wait on the daemon for a guest's captured output.
///
/// Short, and much shorter than the SDK's 30s: this runs inside a reconcile
/// tick, and a diagnostic is not worth stalling the autoscaler for. Missing the
/// log costs one field on one error line; blocking the tick costs every
/// deployment on the host.
const GUEST_LOG_TIMEOUT: Duration = Duration::from_secs(3);

/// Cap on the rendered tail. A boot-timeout line becomes one record at the log
/// shipper, and a guest that spent five minutes printing a stack trace a second
/// must not turn one diagnostic into a megabyte.
const GUEST_LOG_MAX_CHARS: usize = 2000;

/// `GET /sandboxes/:id/logs`, as much of it as this needs.
///
/// Deliberately loose: `source` is read as a string rather than the daemon's
/// `LogSource` enum, and every field is optional. This is a diagnostic on a
/// failure path, so a daemon that adds a variant or renames a field must
/// degrade to a thinner message, never to no message.
#[derive(Deserialize)]
struct GuestLogs {
    #[serde(default)]
    logs: Vec<GuestLogLine>,
}

#[derive(Deserialize)]
struct GuestLogLine {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    message: String,
}

/// `POST /sandbox-deploy`'s answer, which is the id of a sandbox that is still
/// provisioning. The SDK's own `CreateResponse` is private, and this is the only
/// field either of them needs.
#[derive(Deserialize)]
struct CreatedSandbox {
    id: String,
}

/// The defaults `heyo_sdk::Sandbox::create` fills in before posting, applied
/// here because this posts the body itself.
///
/// Replicated rather than skipped, and it is not cosmetic. `size_class` is the
/// one that bites: the daemon treats an absent size class as "no size class",
/// falls through to `cpus`/`memory` — which app-lb never sends — and boots a
/// Firecracker guest at **1 vCPU and 128 MB**. A spec that simply omitted
/// `size_class` would go from `small` to unusable, and the only symptom would be
/// guests that OOM under load.
///
/// `image` matters for the same reason in a quieter way: the SDK's default is
/// `ubuntu:24.04` and the daemon's own is `microsandbox/python`, so a spec with
/// no image would silently change operating systems.
///
/// Keep this in step with `augment_create_body` in the SDK.
/// Who a VM is metered to: the deployment's owner, as stamped on its spec by
/// the admin API (see `DeploymentSpec::account_id`).
///
/// Carried to the daemon as top-level `account_id` / `user_id` on the create
/// body, which a daemon that meters honours from a trusted caller and an older
/// one ignores. Both optional: a self-hosted app-lb stamps nothing and sends
/// nothing, so its create bodies are byte-for-byte what they were.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmOwner {
    pub account_id: Option<String>,
    pub user_id: Option<String>,
}

impl VmOwner {
    pub fn of(spec: &DeploymentSpec) -> Self {
        Self {
            account_id: spec.account_id.clone(),
            user_id: spec.user_id.clone(),
        }
    }
}

/// The `POST /sandbox-deploy` body for one replica: the SDK's options, the
/// defaults it would have applied, the guest mounts, and the owner. Pure, so
/// the wire shape is tested without a daemon.
fn create_body(
    options: &SandboxCreateOptions,
    mounts: Vec<Value>,
    owner: &VmOwner,
) -> Result<Value, VmError> {
    let mut body = serde_json::to_value(options).map_err(|e| VmError::BadCreateBody(e.to_string()))?;
    sdk_create_defaults(&mut body);
    if !mounts.is_empty() {
        body["mounts"] = Value::Array(mounts);
    }
    if let Some(account) = &owner.account_id {
        body["account_id"] = json!(account);
    }
    if let Some(user) = &owner.user_id {
        body["user_id"] = json!(user);
    }
    Ok(body)
}

fn sdk_create_defaults(body: &mut Value) {
    let Some(map) = body.as_object_mut() else {
        return;
    };
    for (key, value) in [
        ("region", json!("US")),
        ("image", json!("ubuntu:24.04")),
        ("size_class", json!("small")),
        ("open_ports", json!([])),
    ] {
        map.entry(key).or_insert(value);
    }
}

/// The last `lines` of guest output on one line, bounded.
///
/// One line because this becomes a tracing *field*, and a field that spans
/// twenty lines breaks every consumer that reads a record per line. The stream
/// is kept because which one a message came out of is most of the diagnosis: a
/// server announcing its port on stdout and a stack trace on stderr mean
/// opposite things about the same boot.
fn render_guest_log(logs: &[GuestLogLine], lines: usize) -> String {
    let start = logs.len().saturating_sub(lines);
    let mut out = String::new();
    for entry in &logs[start..] {
        let text = entry.message.trim();
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(" | ");
        }
        if let Some(source) = entry.source.as_deref() {
            out.push_str(source);
            out.push_str(": ");
        }
        out.push_str(text);
        // Checked as it grows rather than truncated at the end, so the cap
        // cannot be overshot by one enormous final line.
        if out.chars().count() >= GUEST_LOG_MAX_CHARS {
            let kept: String = out.chars().take(GUEST_LOG_MAX_CHARS).collect();
            return format!("{kept}… (truncated)");
        }
    }
    out
}

/// How long a daemon call may take before it is abandoned. Unchanged from the
/// TCP-only client this replaced; a socket is faster, not more patient.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(30);

/// Talks to the heyvm daemon.
#[derive(Clone)]
pub struct VmManager {
    /// The daemon transport, built once and shared by every call here.
    ///
    /// One client rather than an options struct each call site rebuilds: a unix
    /// socket cannot be described by [`HeyoClientOptions`] — it has a URL and a
    /// socket has a path — so a client built by `local_auto_with` has to be
    /// passed through. Sharing it is also what makes the bearer uniform: the
    /// routes the SDK does not wrap used to carry their own copy of the key,
    /// which is a 401 on the diagnostics the moment the two drift apart.
    client: HeyoClient,
    /// What `client` ended up dialing, for the startup log. `base_url` cannot
    /// answer it — a socket client reports `http://localhost`.
    transport: String,
    /// Where a guest mount's tree lives on this host. Held here rather than
    /// passed in by the autoscaler so that a mount that has not been pulled
    /// fails the *create*, and therefore travels the create-failure path that
    /// already counts, reports and backs off — instead of needing a second one.
    mounts: MountStore,
}

/// `HeyoClient` is not `Debug`, and which daemon was reached is the half of
/// this struct worth printing anyway.
impl std::fmt::Debug for VmManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmManager")
            .field("transport", &self.transport)
            .field("mounts", &self.mounts)
            .finish()
    }
}

impl VmManager {
    /// Targets the local daemon, which is the only place `guest_ip` is
    /// available.
    ///
    /// With no `daemon_url`, prefers a unix socket: `HEYVM_SOCKET`, then
    /// `socket_path` from `~/.heyo/daemon.json`, each connect-verified before
    /// it is trusted — the daemon only clears that field on a graceful
    /// shutdown, so a crash leaves a path pointing at nothing. Falling back to
    /// `http://127.0.0.1:34099` costs nothing, because socket mode is additive
    /// on the daemon side: the TCP listener is there either way.
    ///
    /// An explicit `daemon_url` is an instruction, not a hint, and is honoured
    /// as given. It is how a non-default or remote daemon is named, and a
    /// local socket silently winning over it would quietly talk to the wrong
    /// machine.
    pub fn new(
        daemon_url: Option<String>,
        api_key: Option<String>,
        mounts: MountStore,
    ) -> Result<Self, VmError> {
        let explicit = daemon_url.is_some();
        let opts = HeyoClientOptions {
            base_url: daemon_url,
            api_key,
            timeout: Some(DAEMON_TIMEOUT),
        };
        let client = if explicit {
            HeyoClient::new(opts)?
        } else {
            HeyoClient::local_auto_with(opts)?
        };
        let transport = match client.socket_path() {
            Some(path) => format!("unix:{}", path.display()),
            None => client.base_url().to_string(),
        };
        Ok(Self {
            client,
            transport,
            mounts,
        })
    }

    /// Which daemon endpoint this manager reached — a socket path or a URL.
    pub fn transport(&self) -> &str {
        &self.transport
    }

    /// The tail of what a guest itself printed, as the daemon captured it.
    ///
    /// heyvmd starts vsock forwarders inside the guest before it runs the start
    /// command, piping the workload's `/var/log/heyvm-start.log` and
    /// `heyvm-start.err.log` into a per-sandbox ring buffer it serves at
    /// `GET /sandboxes/:id/logs`. `heyo_sdk` wraps exec, shell and the lifecycle
    /// but not that route, so this goes through the client's raw request helper
    /// — same transport and same bearer as everything else here.
    ///
    /// **Best-effort by construction**: an unreachable daemon, a guest with no
    /// `socat` or `/dev/vsock` to run the forwarders, a response shape we do not
    /// recognise — all return `None`. Every caller is already reporting a
    /// failure, and a diagnostic that can turn into a second failure is worse
    /// than no diagnostic at all.
    ///
    /// Call it while the sandbox still exists: the ring buffer belongs to the
    /// sandbox and goes when it does.
    pub async fn guest_log_tail(&self, sandbox_id: &str, lines: usize) -> Option<String> {
        // The id reaches the daemon inside a URL path. It comes from the daemon
        // in the first place, but it is validated rather than escaped here for
        // the same reason the disk routes validate it: refusing an id that
        // cannot be one is a smaller surface than getting the escaping right.
        if !crate::disks::valid_sandbox_id(sandbox_id) {
            return None;
        }
        let response = self
            .client
            .raw_request(
                http::Method::GET,
                &format!("/sandboxes/{sandbox_id}/logs"),
                None::<&serde_json::Value>,
                RequestOptions {
                    timeout: Some(GUEST_LOG_TIMEOUT),
                    query: vec![("limit".to_string(), lines.to_string())],
                },
            )
            .await;

        let body: GuestLogs = match response {
            Ok(r) if r.status().is_success() => r.json().await.ok()?,
            // Both are ordinary here — an older daemon has no such route, and a
            // sandbox killed between the decision and this call is gone. Debug,
            // not warn: the caller's own error line is the event.
            Ok(r) => {
                tracing::debug!(sandbox = %sandbox_id, status = %r.status(), "no guest logs");
                return None;
            }
            Err(e) => {
                tracing::debug!(sandbox = %sandbox_id, error = %e, "could not fetch guest logs");
                return None;
            }
        };

        Some(render_guest_log(&body.logs, lines)).filter(|s| !s.is_empty())
    }

    /// Every sandbox the daemon knows about.
    ///
    /// Callers should hit this **once per reconcile tick** and index the result:
    /// `Sandbox::info()` fetches this same full list and filters client-side, so
    /// per-VM polling is quadratic.
    pub async fn list(&self) -> Result<Vec<SandboxInfo>, VmError> {
        Ok(self.list_detailed().await?.sandboxes)
    }

    /// [`Self::list`], plus what the SDK's [`SandboxInfo`] does not carry.
    ///
    /// The daemon's listing says whose sandbox each one is (`account_id`) and
    /// when it was created; the SDK struct predates both fields and drops them
    /// on the floor. Rather than pin a newer SDK for two strings, the raw JSON
    /// is read once, the SDK's own type is deserialized from it — so what the
    /// autoscaler sees is exactly what it always saw — and the extras are
    /// picked off beside it. One request, not two.
    pub async fn list_detailed(&self) -> Result<Listing, VmError> {
        let raw: Vec<Value> = self
            .client
            .request(
                http::Method::GET,
                "/deployed-sandboxes",
                None::<&Value>,
                RequestOptions::default(),
            )
            .await?;
        Listing::from_raw(raw)
    }

    /// Create a VM and return immediately, without waiting for boot.
    ///
    /// Returning immediately is deliberate: the autoscaler must not block its
    /// reconcile loop for the ~minutes a VM can take. Readiness is tracked
    /// across subsequent ticks instead.
    ///
    /// Posts the create body directly rather than going through
    /// `Sandbox::create`, because `SandboxCreateOptions` has no `mounts` field
    /// and the daemon's `CreateSandboxRequest` does. The body is still that
    /// struct's own serialization — see [`sdk_create_defaults`] for the four
    /// fields the SDK would have added, and why leaving them out would quietly
    /// change what boots.
    ///
    /// Fails before touching the daemon if any of the spec's mounts has no tree
    /// on this host; see [`VmError::MountNotPulled`].
    ///
    /// `workspace` is the deployment's workspace tree, when it has one — see
    /// [`crate::workspace`]. It goes **last** in the mount list, writable, so
    /// its image index on the daemon is `spec.mounts.len()`; the capture path
    /// relies on that to find the image again.
    ///
    /// `owner` is who the VM is metered to — see [`VmOwner`].
    pub async fn create(
        &self,
        spec: &VmSpec,
        name: String,
        workspace: Option<&Path>,
        owner: &VmOwner,
    ) -> Result<Sandbox, VmError> {
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
            // Unused on this path — `Sandbox::create` is what reads it, and the
            // wait it describes (none) is what this method does anyway.
            wait_for_ready: Some(Duration::ZERO),
            region: None,
            archive_id: None,
        };

        // Resolved before the request, so a deployment whose mounts have not
        // been pulled fails without asking the daemon for anything.
        let mut mounts = self.guest_mounts(spec)?;
        if let (Some(tree), Some(ws)) = (workspace, spec.workspace.as_ref()) {
            mounts.push(json!({
                "host_path": tree.to_string_lossy(),
                "sandbox_path": ws.guest_path(),
                "read_only": false,
            }));
        }

        let body = create_body(&options, mounts, owner)?;

        // `POST /sandbox-deploy`, which is what `Sandbox::create` posts. It
        // answers `202` with the id of a sandbox that is still provisioning;
        // readiness is tracked across reconcile ticks either way.
        let client = &self.client;
        let created: CreatedSandbox = client
            .request(
                http::Method::POST,
                "/sandbox-deploy",
                Some(&body),
                RequestOptions::default(),
            )
            .await?;

        Ok(Sandbox::connect_with_client(self.client.clone(), created.id))
    }

    /// The daemon's `mounts` array for a spec: one host directory per guest
    /// mount, in spec order.
    ///
    /// Order is load-bearing. heyvmd letters the devices `/dev/vdb`, `/dev/vdc`
    /// … in exactly this order and mounts them in it, so two mounts swapping
    /// places between one create and the next would hand a restarted replica a
    /// different filesystem under the same path.
    fn guest_mounts(&self, spec: &VmSpec) -> Result<Vec<Value>, VmError> {
        spec.mounts
            .iter()
            .map(|mount| {
                let tree =
                    self.mounts
                        .resolve(mount)
                        .ok_or_else(|| VmError::MountNotPulled {
                            path: mount.guest_path().to_string(),
                            digest: mount.digest.clone(),
                        })?;
                Ok(json!({
                    "host_path": tree.to_string_lossy(),
                    "sandbox_path": mount.guest_path(),
                    "read_only": mount.read_only,
                }))
            })
            .collect()
    }

    pub fn connect(&self, sandbox_id: String) -> Result<Sandbox, VmError> {
        Ok(Sandbox::connect_with_client(self.client.clone(), sandbox_id))
    }

    /// Fetch the daemon's cached host + per-sandbox usage snapshot.
    ///
    /// Like `list`, this is a single daemon round-trip meant for once-per-tick
    /// use — the daemon already samples in the background, so this just reads
    /// its cache. There is no typed SDK method for it, so we go through the
    /// client's generic request helper.
    pub async fn system_usage(&self) -> Result<SystemUsage, VmError> {
        let client = &self.client;
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
    /// The sandbox record and its `/workspace` data disk survive; memory does
    /// not. Rootfs writes do not survive either, but the two drivers get there
    /// differently: Firecracker's per-boot copy lives in `/tmp` and is recopied
    /// from the base image on every boot, while the KVM driver *would* reuse a
    /// persisted `kvm/<id>/rootfs.ext4` if one still existed — which is why the
    /// autoscaler discards that copy right after a successful suspend (see
    /// [`crate::disks::discard_rootfs`]), so a replica behaves the same on both
    /// drivers and a scaled-to-zero pool is not parking a gigabyte per VM.
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

        let client = &self.client;
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
            out.extend(page.sandboxes.into_iter().map(InactiveSandbox::into_info));
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

/// What a `/deployed-sandboxes` listing said, in both shapes app-lb needs.
#[derive(Debug, Default)]
pub struct Listing {
    /// The SDK's view, exactly as [`VmManager::list`] returns it.
    pub sandboxes: Vec<SandboxInfo>,
    /// Sandbox id → the fields the SDK's struct does not declare.
    pub details: HashMap<String, SandboxDetail>,
}

/// The parts of a daemon listing entry that identify a sandbox to a person
/// rather than to the autoscaler.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SandboxDetail {
    /// The heyo account billed for the sandbox. Absent when the daemon runs
    /// with auth off, or predates the field.
    #[serde(default)]
    pub account_id: Option<String>,
    /// RFC 3339, from the daemon's own record — unlike `uptime_secs` it does
    /// not restart with the VM.
    #[serde(default)]
    pub created_at: Option<String>,
}

impl Listing {
    fn from_raw(raw: Vec<Value>) -> Result<Self, VmError> {
        // The whole array at once, so a malformed entry fails the listing the
        // way it always did rather than silently thinning the fleet: a VM that
        // vanishes from the list is a VM the autoscaler replaces.
        let sandboxes: Vec<SandboxInfo> =
            serde_json::from_value(Value::Array(raw.clone())).map_err(|e| {
                VmError::Sdk(heyo_sdk::HeyoError::Api {
                    status: 0,
                    message: format!("malformed sandbox listing: {e}"),
                    body: None,
                })
            })?;
        let details = raw
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_string();
                // Lenient on purpose: these fields are decoration, and a daemon
                // that sends them in a shape this build does not expect should
                // cost the row its decoration, not the fleet its listing.
                let detail = serde_json::from_value(entry.clone()).unwrap_or_default();
                Some((id, detail))
            })
            .collect();
        Ok(Self {
            sandboxes,
            details,
        })
    }
}

/// One page of `GET /sandboxes/inactive`.
#[derive(Debug, Deserialize)]
struct InactivePage {
    #[serde(default)]
    sandboxes: Vec<InactiveSandbox>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// One sandbox as the *inactive* listing reports it.
///
/// Parsed here rather than straight into the SDK's [`SandboxInfo`], because the
/// two are not the same wire format and never were. mvm-ctrl serves two families
/// of route: the **compat** ones the SDK is built for (`/deployed-sandboxes`),
/// which go through its `compat_sandbox_json` shim and synthesize the
/// `status_changed_at` the SDK requires, and the **native** ones
/// (`/sandboxes/inactive`), which serialize the daemon's own model — a model with
/// no such field. `SandboxInfo` declares `status_changed_at` as the single field
/// without `#[serde(default)]`, so pointing it at the native route fails the
/// *entire page* on a string that was never promised.
///
/// That is app-lb's error and not the daemon's: the SDK has no `list_inactive`,
/// so this call was written here against a route the SDK does not cover. It cost
/// real damage before it was understood — a failed page takes down both callers
/// (the suspended-VM backstop in `autoscale` and disk reclamation in `disks`),
/// and both then decline to act, silently.
///
/// So: `id` is required, because a record without one is not addressable and
/// acting on a page containing it would be guesswork. Everything else defaults.
/// That keeps a genuinely malformed page an error — which is what makes the
/// inventory report `complete: false` and hold every disk — while a merely
/// *sparse* one, which is what the daemon actually sends, goes through.
#[derive(Debug, Deserialize)]
struct InactiveSandbox {
    id: String,
    #[serde(default)]
    name: String,
    /// `SandboxStatus` has a `#[serde(other)]` fallback, so an unrecognised
    /// state degrades to `Unknown` rather than failing the page.
    #[serde(default = "unknown_status")]
    status: SandboxStatus,
    #[serde(default)]
    image: String,
    #[serde(default)]
    ttl_seconds: Option<u64>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    status_changed_at: String,
    #[serde(default)]
    guest_ip: Option<String>,
}

fn unknown_status() -> SandboxStatus {
    SandboxStatus::Unknown
}

impl InactiveSandbox {
    /// Widen to the shape the rest of app-lb reads.
    ///
    /// Only `id`, `name` and `status` are ever consulted for a sandbox from this
    /// listing — it exists to answer "does the daemon still know about this?" —
    /// so the remaining fields are filled with the same defaults `SandboxInfo`
    /// would have used. Nothing routes to an inactive sandbox, by definition.
    fn into_info(self) -> SandboxInfo {
        SandboxInfo {
            id: self.id,
            name: self.name,
            status: self.status,
            image: self.image,
            region: None,
            start_command: None,
            working_directory: None,
            size_class: None,
            // Added to `SandboxInfo` after 0.1.6; unset for the same
            // reason the rest of these are — nothing here reads it.
            disk_size_gb: None,
            env_vars: None,
            setup_hooks: None,
            uptime_secs: 0,
            ttl_seconds: self.ttl_seconds,
            is_deployed: false,
            error_message: self.error_message,
            status_changed_at: self.status_changed_at,
            urls: vec![],
            guest_ip: self.guest_ip,
            metadata: None,
        }
    }
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
mod guest_log_tests {
    use super::*;

    fn line(source: Option<&str>, message: &str) -> GuestLogLine {
        GuestLogLine {
            source: source.map(str::to_string),
            message: message.into(),
        }
    }

    /// The tail, and the stream each line came from — a server announcing its
    /// port and a stack trace mean opposite things about the same boot.
    #[test]
    fn keeps_the_last_lines_and_says_which_stream_they_came_from() {
        let logs = vec![
            line(Some("stdout"), "starting"),
            line(Some("stdout"), "migrations ok"),
            line(Some("stderr"), "connection refused"),
        ];
        assert_eq!(
            render_guest_log(&logs, 2),
            "stdout: migrations ok | stderr: connection refused",
        );
    }

    /// A daemon that stops sending `source` still has to produce a message.
    #[test]
    fn a_line_with_no_stream_still_renders() {
        assert_eq!(render_guest_log(&[line(None, "boom")], 5), "boom");
    }

    /// Fewer lines than asked for is the normal case for a guest that died early.
    #[test]
    fn asking_for_more_than_there_is_returns_what_there_is() {
        assert_eq!(
            render_guest_log(&[line(Some("stderr"), "boom")], 20),
            "stderr: boom"
        );
    }

    /// Blank lines are padding in a terminal and noise in a log field.
    #[test]
    fn empty_lines_are_dropped_rather_than_joined_as_gaps() {
        let logs = vec![
            line(Some("stdout"), "a"),
            line(Some("stdout"), "   "),
            line(Some("stdout"), "b"),
        ];
        assert_eq!(render_guest_log(&logs, 10), "stdout: a | stdout: b");
    }

    /// Nothing captured is not an empty string dressed up as output —
    /// `guest_log_tail` turns this into `None` and the caller says why.
    #[test]
    fn nothing_captured_renders_to_nothing() {
        assert!(render_guest_log(&[], 20).is_empty());
        assert!(render_guest_log(&[line(Some("stdout"), "")], 20).is_empty());
    }

    /// One enormous final line must not overshoot the cap — the check runs as
    /// the string grows, not once at the end.
    #[test]
    fn a_single_huge_line_is_still_bounded() {
        let huge = "x".repeat(GUEST_LOG_MAX_CHARS * 3);
        let out = render_guest_log(&[line(Some("stderr"), &huge)], 20);
        assert!(out.ends_with("… (truncated)"), "{}", &out[out.len() - 40..]);
        assert!(
            out.chars().count() <= GUEST_LOG_MAX_CHARS + "… (truncated)".chars().count(),
            "{} chars",
            out.chars().count(),
        );
    }

    /// Truncation counts characters, not bytes: slicing a multi-byte guest
    /// message on a byte boundary would panic on the failure path.
    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        let huge = "é".repeat(GUEST_LOG_MAX_CHARS * 2);
        let out = render_guest_log(&[line(Some("stderr"), &huge)], 20);
        assert!(out.ends_with("… (truncated)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MountSpec;

    /// A mount store over a directory that does not exist, which is all the
    /// tests that never resolve a mount need.
    fn test_mounts() -> MountStore {
        MountStore::new(std::path::PathBuf::from("/nonexistent/app-lb-mounts"), 0)
    }

    fn mount(path: &str, digest: Option<&str>) -> MountSpec {
        MountSpec {
            path: path.into(),
            store: "/srv/art".into(),
            artifact_ref: "corpus".into(),
            auth: None,
            strip_components: None,
            read_only: true,
            digest: digest.map(Into::into),
        }
    }

    fn spec_with(mounts: Vec<MountSpec>) -> VmSpec {
        VmSpec {
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
            mounts,
            workspace: None,
            ttl_seconds: 3600,
        }
    }

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
            // Added to `SandboxInfo` after 0.1.6; unset for the same
            // reason the rest of these are — nothing here reads it.
            disk_size_gb: None,
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
    fn daemon_api_key_reaches_every_daemon_request() {
        let manager = VmManager::new(
            Some("http://host.internal:3000".into()),
            Some("service-token".into()),
            test_mounts(),
        )
        .unwrap();

        // One client serves the lifecycle calls and the guest-log diagnostics
        // alike, so the bearer cannot reach one and miss the other. The
        // separate request builder this replaced had to be kept in step with
        // `opts.api_key` by hand, and a drift there was a silent 401 on
        // exactly the path you reach for when something is already wrong.
        assert_eq!(manager.client.api_key(), Some("service-token"));
        assert_eq!(manager.transport(), "http://host.internal:3000");
    }

    #[test]
    fn an_explicit_daemon_url_is_never_traded_for_a_socket() {
        // Holds even on a host serving a live socket: a named address is how a
        // non-default or remote daemon is reached, so preferring a local
        // socket over it would quietly talk to the wrong machine.
        let manager =
            VmManager::new(Some("http://127.0.0.1:34099".into()), None, test_mounts()).unwrap();

        assert_eq!(manager.transport(), "http://127.0.0.1:34099");
        assert!(manager.client.socket_path().is_none());
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

    /// The daemon's two listings do not agree on their own schema, and the
    /// SDK's `SandboxInfo` cannot parse the inactive one.
    ///
    /// This payload is copied verbatim from a live `GET /sandboxes/inactive`:
    /// note the complete absence of `status_changed_at`, plus an `uptime` object
    /// where the SDK expects `uptime_secs`. Deserializing it as `SandboxInfo`
    /// fails the whole page, which took disk reclamation and the suspended-VM
    /// backstop down with it.
    mod inactive_listing {
        use super::*;

        /// Verbatim from heyvmd 2026-08-03, trimmed to two records.
        const LIVE: &str = r#"{"cursor":null,"next_cursor":"sb-067cf381","sandboxes":[
            {"backend_type":"libvirt","cpu_usage":null,"id":"sb-000fcf7c",
             "image":"/home/u/.heyo/images/todo-agent-base.qcow2","memory_usage":null,
             "mounts":[{"host_path":"/home/u/.todo","read_only":false,"sandbox_path":"/data"}],
             "name":"e2e-test","remotely_accessible":false,"sandbox_type":"shell",
             "status":"stopped","ttl_seconds":3600,"uptime":{"nanos":8225,"secs":0}},
            {"backend_type":"firecracker","cpu_usage":null,"guest_ip":"172.21.94.166",
             "id":"sb-066157a9","image":"ubuntu","memory_usage":null,"name":"applb-demo-000000000001",
             "remotely_accessible":false,"sandbox_type":"shell","status":"stopped",
             "tap_device":"tap-fc-066157a9","ttl_seconds":900,"uptime":{"nanos":1379056,"secs":0}}]}"#;

        /// Not a daemon bug: `/sandboxes/inactive` is a *native* route and
        /// `SandboxInfo` is the *compat* shape. This pins the mismatch so that a
        /// future SDK that does cover this route makes the local parser
        /// obviously redundant rather than quietly redundant.
        #[test]
        fn the_sdk_type_cannot_parse_what_the_daemon_sends() {
            #[derive(Debug, Deserialize)]
            struct StrictPage {
                #[allow(dead_code)]
                sandboxes: Vec<SandboxInfo>,
            }
            let e = serde_json::from_str::<StrictPage>(LIVE).unwrap_err().to_string();
            assert!(
                e.contains("status_changed_at"),
                "if the SDK ever defaults this field, our own parser can go: {e}",
            );
        }

        #[test]
        fn our_parser_reads_it() {
            let page: InactivePage = serde_json::from_str(LIVE).unwrap();
            assert_eq!(page.sandboxes.len(), 2);
            assert_eq!(page.next_cursor.as_deref(), Some("sb-067cf381"));

            let infos: Vec<SandboxInfo> = page
                .sandboxes
                .into_iter()
                .map(InactiveSandbox::into_info)
                .collect();
            assert_eq!(infos[0].id, "sb-000fcf7c");
            assert_eq!(infos[0].status, SandboxStatus::Stopped);
            assert_eq!(infos[0].status_changed_at, "", "absent, and that is fine");
            // The name is what maps a sandbox back to its deployment, so it is
            // the one field besides the id that must survive.
            assert_eq!(owner_of(&infos[1].name), Some("demo"));
            assert_eq!(infos[1].guest_ip.as_deref(), Some("172.21.94.166"));
        }

        /// Only the id is load-bearing. A record without one cannot be acted on,
        /// and failing the page is what makes the disk inventory report
        /// `complete: false` and hold every disk rather than guess.
        #[test]
        fn a_record_with_no_id_still_fails_the_page() {
            let bad = r#"{"sandboxes":[{"name":"nameless","status":"stopped"}]}"#;
            assert!(serde_json::from_str::<InactivePage>(bad).is_err());
        }

        /// Everything else is optional, including the status.
        #[test]
        fn an_almost_empty_record_is_accepted() {
            let sparse = r#"{"sandboxes":[{"id":"sb-1"}]}"#;
            let page: InactivePage = serde_json::from_str(sparse).unwrap();
            let info = page.sandboxes.into_iter().next().unwrap().into_info();
            assert_eq!(info.id, "sb-1");
            assert_eq!(info.status, SandboxStatus::Unknown);
        }

        /// An unrecognised status must not fail the page either — the SDK enum
        /// has a `#[serde(other)]` arm and this pins that we rely on it.
        #[test]
        fn an_unknown_status_degrades_rather_than_failing() {
            let odd = r#"{"sandboxes":[{"id":"sb-1","status":"hibernating"}]}"#;
            let page: InactivePage = serde_json::from_str(odd).unwrap();
            assert_eq!(page.sandboxes[0].status, SandboxStatus::Unknown);
        }
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

    // -- guest mounts ------------------------------------------------------

    fn store_holding(digests: &[(&str, usize)]) -> (std::path::PathBuf, MountStore) {
        let root = std::env::temp_dir().join(format!(
            "app-lb-vm-mounts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = MountStore::new(root.clone(), 0);
        for (digest, strip) in digests {
            std::fs::create_dir_all(store.tree_path(digest, *strip)).unwrap();
        }
        (root, store)
    }

    const D1: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const D2: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f809";

    /// A VM whose data is not on the host is refused before the daemon is asked
    /// for anything. The alternative — creating it anyway — is a replica that
    /// passes its health check and then fails on the first request that reads
    /// the mount.
    #[test]
    fn a_mount_with_no_tree_refuses_the_create() {
        let (root, store) = store_holding(&[]);
        let manager = VmManager::new(None, None, store).unwrap();

        let never_pulled = manager
            .guest_mounts(&spec_with(vec![mount("/data", None)]))
            .unwrap_err();
        assert!(
            matches!(&never_pulled, VmError::MountNotPulled { path, digest }
                if path == "/data" && digest.is_none()),
            "{never_pulled}",
        );
        assert!(
            never_pulled.to_string().contains("mounts/pull"),
            "the error must say what fixes it: {never_pulled}",
        );

        // Pinned to bytes this host does not hold — a tree reclaimed by the
        // sweep, or a spec copied from another host.
        let elsewhere = manager
            .guest_mounts(&spec_with(vec![mount("/data", Some(D1))]))
            .unwrap_err();
        assert!(
            matches!(&elsewhere, VmError::MountNotPulled { digest, .. } if digest.is_some()),
            "{elsewhere}",
        );
        assert!(elsewhere.to_string().contains(D1));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The daemon's `MountConfig` spelling, and the order it letters devices in.
    #[test]
    fn resolved_mounts_carry_the_daemons_field_names_in_spec_order() {
        let (root, store) = store_holding(&[(D1, 0), (D2, 0)]);
        let tree_one = store.tree_path(D1, 0);
        let tree_two = store.tree_path(D2, 0);
        let manager = VmManager::new(None, None, store).unwrap();

        let writable = MountSpec {
            read_only: false,
            ..mount("/scratch", Some(D2))
        };
        let mounts = manager
            .guest_mounts(&spec_with(vec![mount("/data", Some(D1)), writable]))
            .unwrap();

        assert_eq!(
            mounts,
            vec![
                json!({
                    "host_path": tree_one.to_string_lossy(),
                    "sandbox_path": "/data",
                    "read_only": true,
                }),
                json!({
                    "host_path": tree_two.to_string_lossy(),
                    "sandbox_path": "/scratch",
                    "read_only": false,
                }),
            ],
            "order is load-bearing: heyvmd letters /dev/vdb, /dev/vdc in this order",
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A deployment with no mounts must produce exactly the body it always did,
    /// so the array is absent rather than empty.
    #[test]
    fn a_spec_with_no_mounts_sends_none() {
        let (root, store) = store_holding(&[]);
        let manager = VmManager::new(None, None, store).unwrap();
        assert!(manager.guest_mounts(&spec_with(vec![])).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Posting the create body ourselves means the SDK's defaults are ours to
    /// apply. `size_class` is the one that matters: without it the daemon boots
    /// a Firecracker guest at 1 vCPU and 128 MB.
    #[test]
    fn the_sdk_defaults_are_applied_to_a_hand_built_body() {
        let mut body = json!({ "name": "applb-demo-01" });
        sdk_create_defaults(&mut body);
        assert_eq!(body["size_class"], "small");
        assert_eq!(body["image"], "ubuntu:24.04");
        assert_eq!(body["region"], "US");
        assert_eq!(body["open_ports"], json!([]));

        // ...and never over a value the spec set.
        let mut explicit = json!({
            "image": "agent-base",
            "size_class": "large",
            "open_ports": [8080],
        });
        sdk_create_defaults(&mut explicit);
        assert_eq!(explicit["image"], "agent-base");
        assert_eq!(explicit["size_class"], "large");
        assert_eq!(explicit["open_ports"], json!([8080]));
    }

    /// The owner rides on the create body as top-level fields, and only when
    /// there is one: a self-hosted app-lb's body has no such keys at all, so
    /// nothing it sends changes.
    #[test]
    fn the_create_body_carries_the_owner_only_when_there_is_one() {
        let options = SandboxCreateOptions {
            name: Some("applb-demo-01".into()),
            ..Default::default()
        };
        let unowned = create_body(&options, vec![], &VmOwner::default()).unwrap();
        assert!(unowned.get("account_id").is_none());
        assert!(unowned.get("user_id").is_none());
        assert_eq!(unowned["size_class"], "small", "the SDK defaults still apply");

        let owner = VmOwner {
            account_id: Some("acc-1".into()),
            user_id: Some("u-1".into()),
        };
        let owned = create_body(&options, vec![json!({"host_path": "/x"})], &owner).unwrap();
        assert_eq!(owned["account_id"], "acc-1");
        assert_eq!(owned["user_id"], "u-1");
        assert_eq!(owned["mounts"], json!([{"host_path": "/x"}]));

        // An account with no user — the spec was stamped by hand — is fine.
        let half = VmOwner {
            account_id: Some("acc-1".into()),
            user_id: None,
        };
        let body = create_body(&options, vec![], &half).unwrap();
        assert_eq!(body["account_id"], "acc-1");
        assert!(body.get("user_id").is_none());

        // ...and the owner rides straight off a spec.
        let mut spec: DeploymentSpec = serde_json::from_value(json!({
            "id": "web", "routes": [], "upstreams": ["127.0.0.1:1"],
        }))
        .unwrap();
        assert_eq!(VmOwner::of(&spec), VmOwner::default());
        spec.account_id = Some("acc-2".into());
        assert_eq!(VmOwner::of(&spec).account_id.as_deref(), Some("acc-2"));
    }
}
