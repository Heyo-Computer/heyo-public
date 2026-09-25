//! The heyvm daemon's own routes: the contract an external controller builds
//! on.
//!
//! [`Sandbox`] is the shape a sandbox has everywhere — cloud or local. This
//! module is the *host* side of a local daemon: the listing with host-only
//! fields (`guest_ip`, `account_id`, `created_at`), the inactive (stopped,
//! persisted) sandboxes, per-VM logs, host usage, proxy binds, the storage
//! inventory and purge, uploaded mount trees, the image catalog, and the
//! full create body a controller sends (mounts, billing owner, image source,
//! workspace archive). app-lb is the reference consumer; every method here
//! is a route it calls, typed, so it no longer hand-builds JSON or reads the
//! daemon's data directory by convention.
//!
//! Reach a daemon with [`HeyoClient::local`], [`HeyoClient::local_auto`] or
//! [`HeyoClient::local_socket`], then `Daemon::new(client)`.

use std::collections::HashMap;
use std::path::Path;

use bytes::Bytes;
use futures_util::Stream;
use http::Method;
use serde::{Deserialize, Serialize};

use crate::client::{HeyoClient, RequestOptions};
use crate::errors::HeyoError;
use crate::sandbox::Sandbox;
use crate::types::{SandboxDriver, SandboxInfo, SandboxStatus};

/// One path segment, percent-encoded.
fn segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// A mount in a [`DaemonCreateRequest`]: a directory on the daemon's host,
/// or a tree uploaded with [`Daemon::upload_tree`], materialized as a block
/// device at `sandbox_path`. Order is load-bearing — the daemon letters the
/// devices in array order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonMount {
    /// A directory on the daemon's own host. For a caller that shares that
    /// host; everyone else uses `tree_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
    /// A tree uploaded with [`Daemon::upload_tree`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_id: Option<String>,
    pub sandbox_path: String,
    #[serde(default)]
    pub read_only: bool,
}

impl DaemonMount {
    /// A mount built from an uploaded tree.
    pub fn from_tree(tree_id: impl Into<String>, sandbox_path: impl Into<String>, read_only: bool) -> Self {
        Self {
            host_path: None,
            tree_id: Some(tree_id.into()),
            sandbox_path: sandbox_path.into(),
            read_only,
        }
    }
}

/// The full `POST /sandbox-deploy` body a controller sends. Everything a
/// [`crate::SandboxCreateOptions`] carries, plus what only a daemon-facing
/// caller has a use for. Unknown fields pass through `extra`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonCreateRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<SandboxDriver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_size_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hooks: Option<Vec<String>>,
    /// `micro` … `xlarge`. Absent means *no* size class on the daemon — a
    /// 1 vCPU / 128 MB guest — which is why [`Daemon::create`] fills in
    /// `small` like every other SDK create does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<DaemonMount>,
    /// Who the VM is metered to. Honoured only from a trusted caller (the
    /// daemon's internal key or a platform admin); refused otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// A public-catalog image the daemon fetches and verifies itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_sha256: Option<String>,
    /// A workspace archive the daemon unpacks at `sandbox_path` before boot.
    /// Naming one takes the from-archive create path; the answer is a 202.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_archive_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_path: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// What `POST /sandbox-deploy` answers: the id of a sandbox that is still
/// provisioning (202), or a created one (201).
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonCreated {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Listings, logs, usage
// ---------------------------------------------------------------------------

/// One page of `GET /sandboxes/inactive`: sandboxes the daemon has persisted
/// but is not running. The daemon lists these leniently — only `id` is
/// promised — because a stopped sandbox may predate the fields a live one
/// reports.
#[derive(Debug, Clone, Deserialize)]
pub struct InactivePage {
    #[serde(default)]
    pub sandboxes: Vec<InactiveSandbox>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InactiveSandbox {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "inactive_status")]
    pub status: SandboxStatus,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub status_changed_at: Option<String>,
    #[serde(default)]
    pub guest_ip: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Absent from the wire means the daemon did not say — not `Stopped`, which
/// a caller might act on (reap, reclaim). `Unknown` is what the status enum
/// already degrades an unrecognised value to.
fn inactive_status() -> SandboxStatus {
    SandboxStatus::Unknown
}

impl InactiveSandbox {
    /// The same sandbox as a [`SandboxInfo`], for callers that keep one map
    /// of everything the daemon knows.
    pub fn into_info(self) -> SandboxInfo {
        SandboxInfo {
            id: self.id,
            name: self.name,
            status: self.status,
            image: self.image,
            region: None,
            start_command: None,
            working_directory: None,
            size_class: None,
            disk_size_gb: None,
            env_vars: None,
            setup_hooks: None,
            uptime_secs: 0,
            ttl_seconds: self.ttl_seconds,
            is_deployed: false,
            error_message: self.error_message,
            status_changed_at: self.status_changed_at.unwrap_or_default(),
            urls: Vec::new(),
            guest_ip: self.guest_ip,
            metadata: None,
            account_id: self.account_id,
            created_at: self.created_at,
            cpus: None,
            memory: None,
            backend_type: None,
        }
    }
}

/// `GET /sandboxes/:id/logs` query.
#[derive(Debug, Clone, Default)]
pub struct LogsQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// `stdout`, `stderr` or `console`.
    pub source: Option<String>,
    /// `debug`, `info`, `warning` or `error`.
    pub level: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxLogs {
    #[serde(default)]
    pub logs: Vec<LogEntry>,
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogEntry {
    /// Unix seconds.
    #[serde(default)]
    pub timestamp: u64,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub message: String,
}

/// `GET /system/usage`. `snapshot` is `None` until the daemon's sampler has
/// run once after start.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemUsage {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub snapshot: Option<UsageSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    #[serde(default)]
    pub sampled_at_ms: u64,
    pub host: HostUsage,
    #[serde(default)]
    pub sandboxes: Vec<SandboxUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostUsage {
    #[serde(default)]
    pub cpu_percent: f64,
    #[serde(default)]
    pub cpu_count: u32,
    #[serde(default)]
    pub memory_total_bytes: u64,
    #[serde(default)]
    pub memory_used_bytes: u64,
    #[serde(default)]
    pub memory_available_bytes: u64,
    #[serde(default)]
    pub memory_reserve_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxUsage {
    #[serde(default)]
    pub sandbox_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cpu_percent: f64,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub pids: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Proxy binds
// ---------------------------------------------------------------------------

/// The app-lb deployment a bind is a member of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyDeployment {
    pub namespace: String,
    pub id: String,
}

/// `POST /sandboxes/:id/proxy` body.
#[derive(Debug, Clone, Serialize)]
pub struct BindRequest {
    pub port: u16,
    pub is_public: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<ProxyDeployment>,
}

/// A bind of a VM port to a subdomain the daemon forwards.
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyBind {
    pub subdomain: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub sandbox_id: String,
    pub port: u16,
    #[serde(default = "default_true")]
    pub is_public: bool,
    #[serde(default)]
    pub deployment: Option<ProxyDeployment>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ProxyBindList {
    #[serde(default)]
    proxies: Vec<ProxyBind>,
}

// ---------------------------------------------------------------------------
// Storage, trees, images
// ---------------------------------------------------------------------------

/// `GET /storage`: every sandbox's disks by id, whether or not the daemon
/// still knows the sandbox, and the free space behind them. Join with
/// [`Daemon::list`] and [`Daemon::list_inactive`] to tell an orphan from a
/// stopped VM.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageInventory {
    pub data_dir: String,
    pub tmp_dir: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    #[serde(default)]
    pub sandboxes: Vec<SandboxDisks>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxDisks {
    pub sandbox_id: String,
    #[serde(default)]
    pub parts: Vec<DiskPart>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiskPart {
    pub kind: DiskPartKind,
    /// Relative to `data_dir` (`run/<id>/data.ext4`), or absolute for tmp
    /// scratch.
    pub path: String,
    /// What the file occupies (sparse-aware).
    pub bytes: u64,
    pub apparent_bytes: u64,
    /// Unix seconds.
    pub modified_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskPartKind {
    Data,
    Rootfs,
    Mount,
    Snapshot,
    Tmp,
    #[serde(other)]
    Other,
}

/// What [`Daemon::purge_disks`] removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeParts {
    /// State dirs and tmp scratch: the data disk goes too.
    All,
    /// Rootfs copies only; the driver recreates them from the base image on
    /// the next start. Data disks, mount images and snapshots stay.
    Rootfs,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PurgeOutcome {
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub failed: Vec<PurgeFailure>,
    #[serde(default)]
    pub bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PurgeFailure {
    pub path: String,
    pub error: String,
}

/// An uploaded tree: a directory the daemon builds mounts from.
#[derive(Debug, Clone, Deserialize)]
pub struct TreeInfo {
    pub id: String,
    #[serde(default)]
    pub bytes: u64,
    /// Unix seconds.
    #[serde(default)]
    pub modified_at: u64,
}

/// A rootfs in the daemon's catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageInfo {
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub modified_at: u64,
}

/// Options for [`Daemon::upload_image`].
#[derive(Debug, Clone, Default)]
pub struct ImageUploadOptions {
    /// Hex SHA-256 of the body; the daemon refuses the upload on a mismatch.
    pub sha256: Option<String>,
    /// Grow the filesystem to this many GiB once it has landed.
    pub grow_gb: Option<u64>,
}

/// A body streamed to the daemon: any stream of byte chunks.
pub type UploadStream = std::pin::Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send + 'static>>;

/// A file on this host as an [`UploadStream`].
pub async fn file_stream(path: &Path) -> Result<UploadStream, HeyoError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| HeyoError::Connection(format!("open {}: {e}", path.display())))?;
    Ok(Box::pin(tokio_util::io::ReaderStream::with_capacity(file, 1 << 20)))
}

// ---------------------------------------------------------------------------
// The resource
// ---------------------------------------------------------------------------

/// A local heyvm daemon, by its own routes.
#[derive(Clone)]
pub struct Daemon {
    client: HeyoClient,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon").field("client", &self.client).finish()
    }
}

impl Daemon {
    pub fn new(client: HeyoClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &HeyoClient {
        &self.client
    }

    /// The same sandbox by the routes every SDK shares (`kill`, `stop`,
    /// `start`, `set_ttl`, `commands().run`, `shell`).
    pub fn sandbox(&self, id: &str) -> Sandbox {
        Sandbox::connect_with_client(self.client.clone(), id.to_string())
    }

    fn sandbox_path(id: &str, tail: &str) -> String {
        format!("/sandboxes/{}{}", segment(id), tail)
    }

    // -- sandboxes ---------------------------------------------------------

    /// `POST /sandbox-deploy`. Fills in the defaults every SDK create
    /// applies (`region`, `image`, `size_class`, `open_ports`) so an absent
    /// size class does not boot a 128 MB guest. Answers as soon as the daemon
    /// has accepted the create; readiness is the caller's to watch.
    pub async fn create(&self, req: &DaemonCreateRequest) -> Result<DaemonCreated, HeyoError> {
        let body = serde_json::to_value(req)
            .map_err(|e| HeyoError::InvalidArgument(format!("create body: {e}")))?;
        let body = crate::sandbox::augment_create_body(body);
        self.client
            .request(Method::POST, "/sandbox-deploy", Some(&body), RequestOptions::default())
            .await
    }

    /// `GET /deployed-sandboxes` — the live listing with the host-only
    /// fields (`guest_ip`, `account_id`, `created_at`, `cpus`, `memory`).
    pub async fn list(&self) -> Result<Vec<SandboxInfo>, HeyoError> {
        self.client
            .request(Method::GET, "/deployed-sandboxes", None::<&()>, RequestOptions::default())
            .await
    }

    /// `GET /sandboxes/inactive?count=&cursor=` — one page of the stopped,
    /// persisted sandboxes. Follow `next_cursor` for the rest.
    pub async fn list_inactive(&self, count: usize, cursor: Option<&str>) -> Result<InactivePage, HeyoError> {
        let mut query = vec![("count".to_string(), count.to_string())];
        if let Some(c) = cursor {
            query.push(("cursor".to_string(), c.to_string()));
        }
        self.client
            .request(
                Method::GET,
                "/sandboxes/inactive",
                None::<&()>,
                RequestOptions { query, ..RequestOptions::default() },
            )
            .await
    }

    /// `GET /sandboxes/:id/logs`.
    pub async fn logs(&self, id: &str, q: &LogsQuery) -> Result<SandboxLogs, HeyoError> {
        let mut query = Vec::new();
        if let Some(v) = q.limit {
            query.push(("limit".to_string(), v.to_string()));
        }
        if let Some(v) = q.offset {
            query.push(("offset".to_string(), v.to_string()));
        }
        if let Some(v) = &q.source {
            query.push(("source".to_string(), v.clone()));
        }
        if let Some(v) = &q.level {
            query.push(("level".to_string(), v.clone()));
        }
        self.client
            .request(
                Method::GET,
                &Self::sandbox_path(id, "/logs"),
                None::<&()>,
                RequestOptions { query, ..RequestOptions::default() },
            )
            .await
    }

    /// `GET /system/usage` — the daemon's own CPU/memory sample of the host
    /// and each sandbox.
    pub async fn system_usage(&self) -> Result<SystemUsage, HeyoError> {
        self.client
            .request(Method::GET, "/system/usage", None::<&()>, RequestOptions::default())
            .await
    }

    // -- proxy binds -------------------------------------------------------

    /// `GET /sandboxes/:id/proxy`.
    pub async fn list_binds(&self, id: &str) -> Result<Vec<ProxyBind>, HeyoError> {
        let list: ProxyBindList = self
            .client
            .request(Method::GET, &Self::sandbox_path(id, "/proxy"), None::<&()>, RequestOptions::default())
            .await?;
        Ok(list.proxies)
    }

    /// `POST /sandboxes/:id/proxy` — bind a guest port to a subdomain the
    /// daemon forwards (and syncs to the cloud).
    pub async fn bind(&self, id: &str, req: &BindRequest) -> Result<ProxyBind, HeyoError> {
        self.client
            .request(Method::POST, &Self::sandbox_path(id, "/proxy"), Some(req), RequestOptions::default())
            .await
    }

    /// `DELETE /sandboxes/:id/proxy?subdomain=`. A bind the daemon has
    /// already forgotten counts as withdrawn.
    pub async fn unbind(&self, id: &str, subdomain: &str) -> Result<(), HeyoError> {
        let opts = RequestOptions {
            query: vec![("subdomain".to_string(), subdomain.to_string())],
            ..RequestOptions::default()
        };
        match self
            .client
            .request::<serde_json::Value>(Method::DELETE, &Self::sandbox_path(id, "/proxy"), None::<&()>, opts)
            .await
        {
            Ok(_) => Ok(()),
            Err(HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    // -- storage -----------------------------------------------------------

    /// `GET /storage`.
    pub async fn storage(&self) -> Result<StorageInventory, HeyoError> {
        self.client
            .request(Method::GET, "/storage", None::<&()>, RequestOptions::default())
            .await
    }

    /// `DELETE /storage/sandboxes/:id?parts=` — remove a stopped sandbox's
    /// disks. Operator-only on the daemon; 409 while the sandbox is live.
    pub async fn purge_disks(&self, id: &str, parts: PurgeParts) -> Result<PurgeOutcome, HeyoError> {
        let parts = match parts {
            PurgeParts::All => "all",
            PurgeParts::Rootfs => "rootfs",
        };
        self.client
            .request(
                Method::DELETE,
                &format!("/storage/sandboxes/{}", segment(id)),
                None::<&()>,
                RequestOptions {
                    query: vec![("parts".to_string(), parts.to_string())],
                    ..RequestOptions::default()
                },
            )
            .await
    }

    /// `GET /storage/sandboxes/:id/archive` — the sandbox's state
    /// directories as a `tar.gz` stream (`response.bytes_stream()`), paths
    /// relative to the daemon's data directory.
    pub async fn archive_disks(&self, id: &str) -> Result<reqwest::Response, HeyoError> {
        self.client
            .stream_get(
                &format!("/storage/sandboxes/{}/archive", segment(id)),
                RequestOptions::default(),
            )
            .await
    }

    /// `GET /sandboxes/:id/mounts/export?sandbox_path=` — the contents of a
    /// stopped sandbox's mount as a `tar.gz` stream. The daemon runs the
    /// journal replay and extraction; the caller sees a tarball.
    pub async fn export_mount(&self, id: &str, sandbox_path: &str) -> Result<reqwest::Response, HeyoError> {
        self.client
            .stream_get(
                &Self::sandbox_path(id, "/mounts/export"),
                RequestOptions {
                    query: vec![("sandbox_path".to_string(), sandbox_path.to_string())],
                    // An export can be gigabytes; the daemon's own timeouts
                    // bound the work, not the client's.
                    timeout: Some(std::time::Duration::from_secs(6 * 3600)),
                },
            )
            .await
    }

    // -- trees -------------------------------------------------------------

    /// `PUT /trees/:id` — upload a `tar.gz` the daemon unpacks as a tree.
    /// Idempotent by id: an existing tree is answered, not replaced.
    pub async fn upload_tree(&self, id: &str, body: UploadStream) -> Result<TreeInfo, HeyoError> {
        let path = format!("/trees/{}", segment(id));
        let response = self
            .client
            .send_stream(Method::PUT, &path, "application/gzip", Vec::new(), body, upload_timeout())
            .await?;
        self.client.parse_json(response, &path).await
    }

    /// `GET /trees`.
    pub async fn list_trees(&self) -> Result<Vec<TreeInfo>, HeyoError> {
        self.client
            .request(Method::GET, "/trees", None::<&()>, RequestOptions::default())
            .await
    }

    /// `GET /trees/:id`, `None` when the daemon has no such tree.
    pub async fn tree(&self, id: &str) -> Result<Option<TreeInfo>, HeyoError> {
        match self
            .client
            .request(Method::GET, &format!("/trees/{}", segment(id)), None::<&()>, RequestOptions::default())
            .await
        {
            Ok(info) => Ok(Some(info)),
            Err(HeyoError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `DELETE /trees/:id`. Operator-only on the daemon. A tree already
    /// gone counts as deleted.
    pub async fn delete_tree(&self, id: &str) -> Result<(), HeyoError> {
        match self
            .client
            .request::<serde_json::Value>(Method::DELETE, &format!("/trees/{}", segment(id)), None::<&()>, RequestOptions::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    // -- images ------------------------------------------------------------

    /// `PUT /images/:name` — upload an ext4 rootfs into the daemon's
    /// catalog, where a create body's `image` names it.
    pub async fn upload_image(
        &self,
        name: &str,
        body: UploadStream,
        opts: &ImageUploadOptions,
    ) -> Result<ImageInfo, HeyoError> {
        let mut path = format!("/images/{}", segment(name));
        if let Some(gb) = opts.grow_gb {
            path.push_str(&format!("?grow_gb={gb}"));
        }
        let mut headers = Vec::new();
        if let Some(sha) = &opts.sha256 {
            headers.push(("x-heyo-sha256".to_string(), sha.clone()));
        }
        let response = self
            .client
            .send_stream(Method::PUT, &path, "application/octet-stream", headers, body, upload_timeout())
            .await?;
        self.client.parse_json(response, &path).await
    }

    /// `GET /images`.
    pub async fn list_images(&self) -> Result<Vec<ImageInfo>, HeyoError> {
        self.client
            .request(Method::GET, "/images", None::<&()>, RequestOptions::default())
            .await
    }

    /// `GET /images/:name`, `None` when the catalog has no such image.
    pub async fn image(&self, name: &str) -> Result<Option<ImageInfo>, HeyoError> {
        match self
            .client
            .request(Method::GET, &format!("/images/{}", segment(name)), None::<&()>, RequestOptions::default())
            .await
        {
            Ok(info) => Ok(Some(info)),
            Err(HeyoError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Uploads are as slow as the disk they land on; the daemon bounds them by
/// size, the client only by patience.
fn upload_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(6 * 3600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_body_carries_the_controller_fields_and_the_shared_defaults() {
        let req = DaemonCreateRequest {
            name: "applb-web-000000000001".into(),
            driver: Some(SandboxDriver::Firecracker),
            mounts: vec![DaemonMount::from_tree("9f2c1e7a", "/workspace", false)],
            account_id: Some("acct".into()),
            image_download_url: Some("https://cloud/public-images/x".into()),
            s3_archive_key: Some("u/a/ar-1.tar.gz".into()),
            sandbox_path: Some("/workspace".into()),
            ..Default::default()
        };
        let body = crate::sandbox::augment_create_body(serde_json::to_value(&req).unwrap());
        assert_eq!(body["driver"], "firecracker");
        assert_eq!(body["mounts"][0]["tree_id"], "9f2c1e7a");
        assert!(body["mounts"][0].get("host_path").is_none());
        assert_eq!(body["account_id"], "acct");
        assert_eq!(body["s3_archive_key"], "u/a/ar-1.tar.gz");
        // The defaults every SDK create applies.
        assert_eq!(body["region"], "US");
        assert_eq!(body["image"], "ubuntu:24.04");
        assert_eq!(body["size_class"], "small");
        assert_eq!(body["open_ports"], serde_json::json!([]));
        // And nothing that was not set.
        assert!(body.get("ttl_seconds").is_none());
        assert!(body.get("user_id").is_none());
    }

    /// Verbatim from heyvmd 2026-08-03, trimmed to two records: the inactive
    /// listing is a *native* route, so there is no `status_changed_at` and
    /// `uptime` is an object, neither of which `SandboxInfo` accepts.
    const LIVE_INACTIVE: &str = r#"{"cursor":null,"next_cursor":"sb-067cf381","sandboxes":[
        {"backend_type":"libvirt","cpu_usage":null,"id":"sb-000fcf7c",
         "image":"/home/u/.heyo/images/todo-agent-base.qcow2","memory_usage":null,
         "mounts":[{"host_path":"/home/u/.todo","read_only":false,"sandbox_path":"/data"}],
         "name":"e2e-test","remotely_accessible":false,"sandbox_type":"shell",
         "status":"stopped","ttl_seconds":3600,"uptime":{"nanos":8225,"secs":0}},
        {"backend_type":"firecracker","cpu_usage":null,"guest_ip":"172.21.94.166",
         "id":"sb-066157a9","image":"ubuntu","memory_usage":null,"name":"applb-demo-000000000001",
         "remotely_accessible":false,"sandbox_type":"shell","status":"stopped",
         "tap_device":"tap-fc-066157a9","ttl_seconds":900,"uptime":{"nanos":1379056,"secs":0}}]}"#;

    #[test]
    fn the_live_inactive_listing_parses() {
        let page: InactivePage = serde_json::from_str(LIVE_INACTIVE).unwrap();
        assert_eq!(page.sandboxes.len(), 2);
        assert_eq!(page.next_cursor.as_deref(), Some("sb-067cf381"));
        let info = page.sandboxes[1].clone().into_info();
        assert_eq!(info.name, "applb-demo-000000000001");
        assert_eq!(info.status, SandboxStatus::Stopped);
        assert_eq!(info.guest_ip.as_deref(), Some("172.21.94.166"));
        assert_eq!(info.status_changed_at, "", "absent, and that is fine");
        let odd: InactivePage =
            serde_json::from_str(r#"{"sandboxes":[{"id":"sb-1","status":"hibernating"}]}"#).unwrap();
        assert_eq!(odd.sandboxes[0].status, SandboxStatus::Unknown);
        assert!(serde_json::from_str::<InactivePage>(r#"{"sandboxes":[{"name":"nameless"}]}"#).is_err(), "only the id is load-bearing");
    }

    #[test]
    fn inactive_sandboxes_load_from_the_bare_minimum() {
        let page: InactivePage = serde_json::from_value(serde_json::json!({
            "sandboxes": [{"id": "sb-1"}, {"id": "sb-2", "status": "stopped", "guest_ip": "10.0.0.2"}],
            "next_cursor": "sb-2"
        }))
        .unwrap();
        assert_eq!(page.sandboxes.len(), 2);
        assert_eq!(page.sandboxes[0].status, SandboxStatus::Unknown);
        assert_eq!(page.next_cursor.as_deref(), Some("sb-2"));
        let info = page.sandboxes[1].clone().into_info();
        assert_eq!(info.guest_ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(info.status, SandboxStatus::Stopped);
    }

    #[test]
    fn storage_shapes_match_the_daemon() {
        let inv: StorageInventory = serde_json::from_value(serde_json::json!({
            "data_dir": "/home/u/.heyo", "tmp_dir": "/tmp", "free_bytes": 10, "total_bytes": 20,
            "sandboxes": [{"sandbox_id": "sb-1", "parts": [
                {"kind": "data", "path": "run/sb-1/data.ext4", "bytes": 4096, "apparent_bytes": 8589934592u64, "modified_at": 1},
                {"kind": "something_new", "path": "run/sb-1/x", "bytes": 1, "apparent_bytes": 1, "modified_at": 1}
            ]}]
        }))
        .unwrap();
        assert_eq!(inv.sandboxes[0].parts[0].kind, DiskPartKind::Data);
        assert_eq!(inv.sandboxes[0].parts[1].kind, DiskPartKind::Other, "an unknown kind does not break the listing");
        let out: PurgeOutcome = serde_json::from_value(serde_json::json!({"removed": ["a"], "failed": [], "bytes": 5})).unwrap();
        assert_eq!(out.bytes, 5);
        assert_eq!(serde_json::to_value(PurgeParts::Rootfs).unwrap(), "rootfs");
    }

    #[test]
    fn bind_request_and_segments() {
        let body = serde_json::to_value(BindRequest {
            port: 8080,
            is_public: false,
            deployment: Some(ProxyDeployment { namespace: "team-a".into(), id: "web".into() }),
        })
        .unwrap();
        assert_eq!(body, serde_json::json!({"port": 8080, "is_public": false, "deployment": {"namespace": "team-a", "id": "web"}}));
        let bare = serde_json::to_value(BindRequest { port: 80, is_public: true, deployment: None }).unwrap();
        assert!(bare.get("deployment").is_none());
        assert_eq!(segment("sb-1"), "sb-1");
        assert_eq!(segment("a b/c"), "a%20b%2Fc");
        assert_eq!(Daemon::sandbox_path("sb-1", "/proxy"), "/sandboxes/sb-1/proxy");
    }

    #[test]
    fn usage_and_logs_accept_the_daemon_shape() {
        let usage: SystemUsage = serde_json::from_value(serde_json::json!({
            "available": true,
            "snapshot": {"sampledAtMs": 5, "host": {"cpuPercent": 12.5, "cpuCount": 8, "memoryTotalBytes": 100, "memoryUsedBytes": 40, "memoryAvailableBytes": 60, "memoryReserveBytes": 10},
                         "sandboxes": [{"sandboxId": "sb-1", "name": "x", "cpuPercent": 1.0, "memoryBytes": 7, "pids": [1]}]}
        }))
        .unwrap();
        assert_eq!(usage.snapshot.unwrap().host.cpu_count, 8);
        let cold: SystemUsage = serde_json::from_value(serde_json::json!({"available": false, "snapshot": null})).unwrap();
        assert!(cold.snapshot.is_none());
        let logs: SandboxLogs = serde_json::from_value(serde_json::json!({
            "logs": [{"timestamp": 1700000000, "source": "stdout", "message": "hi"}], "total": 1, "limit": 100, "offset": 0
        }))
        .unwrap();
        assert_eq!(logs.logs[0].message, "hi");
        assert!(logs.logs[0].level.is_none());
    }
}
