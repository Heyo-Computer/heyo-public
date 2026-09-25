//! Managed app-lb: namespaces and the deployments inside them. Mirrors
//! `sdk-ts/src/namespaces.ts`.
//!
//! Heyo runs one app-lb as a platform service. A *namespace* is the customer's
//! room in it — an auth-service record owned by an account — and every
//! deployment registered through this surface lives in one. Cloud fronts
//! both: `/namespaces` for the records, and `/namespaces/{ns}/lb/…` as a walled
//! door onto app-lb's admin API, pinned to that namespace and authenticated
//! with the same API key this client already holds.
//!
//! Deployment documents are app-lb's own wire format and pass through
//! verbatim: the well-known fields are typed, everything else is kept in
//! `extra` so a document round-trips without loss and a field app-lb adds
//! later does not break deserialisation.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::client::{HeyoClient, HeyoClientOptions, RequestOptions};
use crate::commands::encode_path;
use crate::errors::HeyoError;

/// The tier a caller holds in a namespace: `admin` may change it, `view` may read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamespaceScope {
    Admin,
    View,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NamespaceInfo {
    pub id: String,
    /// The owning account — what VMs in this namespace are billed to.
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The caller's tier in this namespace, when the listing carries it.
    #[serde(default)]
    pub scope: Option<NamespaceScope>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NamespaceCreateOptions {
    /// Globally unique; lowercase letters, digits and `-`, at most 63 bytes.
    /// `default` is reserved.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Create in this account rather than the key's primary one.
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

// ---------------------------------------------------------------------------
// app-lb documents — verbatim wire shapes.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_suffix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmSpec {
    /// `firecracker` or `kvm`.
    pub driver: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The guest port traffic is proxied to.
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<String>,
    /// The only sizing knob: `micro` … `xlarge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_size_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hooks: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// Secret-backed environment, resolved when a replica is created — the
    /// spec carries the reference, never the value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<SecretEnv>,
    /// Seed `/workspace` from an archive on every replica boot. Name it by
    /// `archive_id`; cloud checks ownership and fills in `s3_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_archive: Option<WorkspaceArchive>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One secret value exported to a replica as an environment variable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEnv {
    /// The secret's id, in the deployment's own namespace.
    pub secret: String,
    /// Which key inside it; app-lb defaults to `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Variable name; defaults to the key upper-cased.
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A workspace archive every replica's `/workspace` is unpacked from at boot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceArchive {
    pub archive_id: String,
    /// Resolved by cloud from the id; never set by a client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A stored secret as the API returns it: key *names*, never values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretSummary {
    pub id: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub encrypted_at_rest: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `GET /ingress` — where DNS should point a deployment's hostname. Empty
/// until the operator configures the LB's public addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressInfo {
    #[serde(default)]
    pub ipv4: Vec<String>,
    #[serde(default)]
    pub ipv6: Vec<String>,
}

/// Scaling fields. Every one is optional so the same type serves as the
/// `PATCH …/scaling` body; unset fields are left as they are.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScalingPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_pool: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_to_zero_after_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_start_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_action: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentSpec {
    pub id: String,
    /// Filled in by cloud from the path; if present it must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<VmSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaling: Option<ScalingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthCheck>,
    /// Static `host:port` upstreams, for a deployment that is not a VM pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<serde_json::Value>,
    /// Set by app-lb from the namespace's owner; what its VMs are billed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Ask the cloud for a URL that reaches the pool through the daemon —
    /// see [`IngressSpec`]. Managed deployments only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressSpec>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A URL on the Heyo cloud's domain for a managed deployment, reached through
/// the daemon rather than app-lb's own listener — so it works for VMs on a
/// machine with no public address. With `cloud` set, app-lb binds every
/// ready replica's `vm.port` on the daemon and the cloud issues one URL that
/// fans out to the current replicas; it comes back as
/// [`DeploymentStatus::url`]. Traffic on it bypasses app-lb's routes and
/// `auth` gate; `public: false` puts the cloud's account gate in front.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressSpec {
    #[serde(default)]
    pub cloud: bool,
    /// Default true.
    #[serde(default = "ingress_public_default")]
    pub public: bool,
}

fn ingress_public_default() -> bool {
    true
}

impl IngressSpec {
    /// Ask for a public cloud URL.
    pub fn cloud() -> Self {
        Self { cloud: true, public: true }
    }
}

impl DeploymentSpec {
    /// A spec with only the required fields; fill in `vm`/`upstreams` and
    /// `scaling` on the result.
    pub fn new(id: impl Into<String>, routes: Vec<RouteRule>) -> Self {
        Self {
            id: id.into(),
            namespace: None,
            routes,
            vm: None,
            scaling: None,
            health: None,
            upstreams: Vec::new(),
            build: None,
            account_id: None,
            ingress: None,
            extra: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentVm {
    pub sandbox_id: String,
    pub addr: String,
    pub in_flight: usize,
    pub healthy: bool,
    pub draining: bool,
    pub uptime_secs: u64,
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One row of `GET /deployments`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentSummary {
    pub id: String,
    #[serde(default)]
    pub namespace: Option<String>,
    /// `vm`, `static` or `site`.
    pub kind: String,
    #[serde(default)]
    pub upstreams: Vec<String>,
    #[serde(default)]
    pub routed: bool,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub vms: Vec<DeploymentVm>,
    /// The cloud URL, when the spec asked for one (`ingress.cloud`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `GET /deployments/{id}`, and what create/replace return.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentStatus {
    pub spec: DeploymentSpec,
    /// `vm`, `static` or `site`.
    pub kind: String,
    pub desired_replicas: u32,
    pub ready: usize,
    pub pending: usize,
    pub total_in_flight: usize,
    #[serde(default)]
    pub vms: Vec<serde_json::Value>,
    /// The cloud URL, when the spec asked for one (`ingress.cloud`). Added
    /// by cloud on the way out, not by app-lb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentExecOptions {
    /// Run through `sh -c` in the guest.
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Boot or resume a VM if none is running (default true); `false` asks
    /// for a 409 instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake: Option<bool>,
}

impl DeploymentExecOptions {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            env: None,
            timeout_secs: None,
            wake: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentExecResult {
    /// Which VM ran it — after a resume or rebuild it is a different sandbox
    /// than last time.
    pub sandbox_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// stdout and stderr interleaved as the guest wrote them.
    pub output: String,
}

#[derive(Deserialize)]
struct NamespacesEnvelope {
    #[serde(default)]
    namespaces: Vec<NamespaceInfo>,
}

#[derive(Deserialize)]
struct NamespaceEnvelope {
    namespace: NamespaceInfo,
}

fn namespace_path(name: &str) -> String {
    format!("/namespaces/{}", encode_path(name))
}

/// The deployments of one namespace — app-lb's admin API through cloud's
/// walled door.
#[derive(Clone)]
pub struct Deployments {
    client: HeyoClient,
    namespace: String,
    base: String,
}

impl Deployments {
    fn new(client: HeyoClient, namespace: &str) -> Self {
        Self {
            client,
            namespace: namespace.to_string(),
            base: format!("{}/lb", namespace_path(namespace)),
        }
    }

    fn path(&self, segments: &[&str]) -> String {
        let mut out = self.base.clone();
        for s in segments {
            out.push('/');
            out.push_str(&encode_path(s));
        }
        out
    }

    /// `GET /deployments` — every deployment in the namespace.
    pub async fn list(&self) -> Result<Vec<DeploymentSummary>, HeyoError> {
        let rows: Option<Vec<DeploymentSummary>> = self
            .client
            .request(
                Method::GET,
                &self.path(&["deployments"]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(rows.unwrap_or_default())
    }

    /// `GET /deployments/{id}`.
    pub async fn get(&self, id: &str) -> Result<DeploymentStatus, HeyoError> {
        self.client
            .request(
                Method::GET,
                &self.path(&["deployments", id]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await
    }

    /// `POST /deployments` — register a deployment. A `spec.id` that already
    /// exists is *replaced*, which tears down its pool; use [`Self::scale`]
    /// for a scaling-only change.
    pub async fn create(&self, spec: &DeploymentSpec) -> Result<DeploymentStatus, HeyoError> {
        self.client
            .request(
                Method::POST,
                &self.path(&["deployments"]),
                Some(spec),
                RequestOptions::default(),
            )
            .await
    }

    /// `PUT /deployments/{id}` — replace the spec in place; the pool is kept
    /// unless `vm`/`upstreams` changed.
    pub async fn replace(
        &self,
        id: &str,
        spec: &DeploymentSpec,
    ) -> Result<DeploymentStatus, HeyoError> {
        self.client
            .request(
                Method::PUT,
                &self.path(&["deployments", id]),
                Some(spec),
                RequestOptions::default(),
            )
            .await
    }

    /// `DELETE /deployments/{id}` — deregister and tear down its backends.
    pub async fn delete(&self, id: &str) -> Result<(), HeyoError> {
        let _: serde_json::Value = self
            .client
            .request(
                Method::DELETE,
                &self.path(&["deployments", id]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(())
    }

    /// `PATCH /deployments/{id}/scaling` — change only the given scaling fields.
    pub async fn scale(
        &self,
        id: &str,
        scaling: &ScalingPolicy,
    ) -> Result<DeploymentStatus, HeyoError> {
        self.client
            .request(
                Method::PATCH,
                &self.path(&["deployments", id, "scaling"]),
                Some(scaling),
                RequestOptions::default(),
            )
            .await
    }

    /// `DELETE /deployments/{id}/vms/{sandbox_id}` — evict one VM; the
    /// autoscaler replaces it.
    pub async fn evict_vm(&self, id: &str, sandbox_id: &str) -> Result<(), HeyoError> {
        let _: serde_json::Value = self
            .client
            .request(
                Method::DELETE,
                &self.path(&["deployments", id, "vms", sandbox_id]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(())
    }

    /// `POST /deployments/{id}/build` — start a build job. Poll [`Self::jobs`].
    pub async fn build(
        &self,
        id: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, HeyoError> {
        let empty = serde_json::json!({});
        self.client
            .request(
                Method::POST,
                &self.path(&["deployments", id, "build"]),
                Some(body.unwrap_or(&empty)),
                RequestOptions::default(),
            )
            .await
    }

    /// `POST /deployments/{id}/pull` — start an artifact pull job.
    pub async fn pull(
        &self,
        id: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, HeyoError> {
        let empty = serde_json::json!({});
        self.client
            .request(
                Method::POST,
                &self.path(&["deployments", id, "pull"]),
                Some(body.unwrap_or(&empty)),
                RequestOptions::default(),
            )
            .await
    }

    /// `GET /ingress` — the load balancer's public addresses: what the A (and
    /// AAAA) record for a `routes[].host` should point at.
    pub async fn ingress(&self) -> Result<IngressInfo, HeyoError> {
        let info: Option<IngressInfo> = self
            .client
            .request(
                Method::GET,
                &self.path(&["ingress"]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(info.unwrap_or_default())
    }

    /// `POST /deployments/{id}/update` — start an update job; this rolls the VMs.
    pub async fn update(
        &self,
        id: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, HeyoError> {
        let empty = serde_json::json!({});
        self.client
            .request(
                Method::POST,
                &self.path(&["deployments", id, "update"]),
                Some(body.unwrap_or(&empty)),
                RequestOptions::default(),
            )
            .await
    }

    /// `GET /deployments/{id}/jobs` — recent build/pull/update jobs.
    pub async fn jobs(&self, id: &str) -> Result<Vec<serde_json::Value>, HeyoError> {
        let rows: Option<Vec<serde_json::Value>> = self
            .client
            .request(
                Method::GET,
                &self.path(&["deployments", id, "jobs"]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(rows.unwrap_or_default())
    }

    /// `POST /deployments/{id}/exec` — run a command in one of the
    /// deployment's VMs.
    pub async fn exec(
        &self,
        id: &str,
        options: &DeploymentExecOptions,
    ) -> Result<DeploymentExecResult, HeyoError> {
        let opts = RequestOptions {
            timeout: options
                .timeout_secs
                .map(|s| std::time::Duration::from_secs(s + 30)),
            ..RequestOptions::default()
        };
        self.client
            .request(
                Method::POST,
                &self.path(&["deployments", id, "exec"]),
                Some(options),
                opts,
            )
            .await
    }

    /// `GET /deployments/{id}/shell` — an interactive PTY in one of the
    /// deployment's VMs, over a WebSocket. Returns once app-lb has picked
    /// (or woken) a VM and the shell is up; `sandbox_id()` says which.
    ///
    /// ```no_run
    /// # use heyo_sdk::{Namespace, DeploymentShellOptions};
    /// # use futures_util::StreamExt;
    /// # async fn run() -> Result<(), heyo_sdk::HeyoError> {
    /// let ns = Namespace::connect("team-a", Default::default())?;
    /// let shell = ns.deployments().shell("web", DeploymentShellOptions::default()).await?;
    /// shell.write(b"ls -la\n")?;
    /// let mut out = shell.output();
    /// while let Some(chunk) = out.next().await { print!("{}", String::from_utf8_lossy(&chunk)); }
    /// # Ok(()) }
    /// ```
    pub async fn shell(
        &self,
        id: &str,
        options: crate::DeploymentShellOptions,
    ) -> Result<crate::DeploymentShell, HeyoError> {
        crate::DeploymentShell::open(&self.client, &self.namespace, id, options).await
    }

    /// `GET /metrics` — pool counters and request stats, scoped to the namespace.
    pub async fn metrics(&self) -> Result<serde_json::Value, HeyoError> {
        self.client
            .request(
                Method::GET,
                &self.path(&["metrics"]),
                None::<&()>,
                RequestOptions::default(),
            )
            .await
    }
}

#[derive(Clone)]
pub struct Namespace {
    name: String,
    info: Option<NamespaceInfo>,
    client: HeyoClient,
}

impl Namespace {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The record as last fetched, if this handle has fetched it.
    pub fn cached_info(&self) -> Option<&NamespaceInfo> {
        self.info.as_ref()
    }

    pub fn client(&self) -> &HeyoClient {
        &self.client
    }

    /// `POST /namespaces`.
    pub async fn create(
        options: NamespaceCreateOptions,
        client_options: HeyoClientOptions,
    ) -> Result<Self, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let env: NamespaceEnvelope = client
            .request(
                Method::POST,
                "/namespaces",
                Some(&options),
                RequestOptions::default(),
            )
            .await?;
        Ok(Self {
            name: env.namespace.name.clone(),
            info: Some(env.namespace),
            client,
        })
    }

    /// `GET /namespaces` — every namespace the key reaches, with the caller's
    /// tier on each.
    pub async fn list(client_options: HeyoClientOptions) -> Result<Vec<NamespaceInfo>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let env: NamespacesEnvelope = client
            .request(
                Method::GET,
                "/namespaces",
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(env.namespaces)
    }

    /// `GET /namespaces/{name}`.
    pub async fn get(name: &str, client_options: HeyoClientOptions) -> Result<Self, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let env: NamespaceEnvelope = client
            .request(
                Method::GET,
                &namespace_path(name),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(Self {
            name: env.namespace.name.clone(),
            info: Some(env.namespace),
            client,
        })
    }

    /// A handle by name with no request made; [`Self::info`] fetches on first use.
    pub fn connect(name: &str, client_options: HeyoClientOptions) -> Result<Self, HeyoError> {
        Ok(Self {
            name: name.to_string(),
            info: None,
            client: HeyoClient::new(client_options)?,
        })
    }

    /// The record, fetched once and cached; [`Self::refresh`] re-reads it.
    pub async fn info(&mut self) -> Result<&NamespaceInfo, HeyoError> {
        if self.info.is_none() {
            self.refresh().await?;
        }
        Ok(self.info.as_ref().expect("refresh populates info"))
    }

    pub async fn refresh(&mut self) -> Result<&NamespaceInfo, HeyoError> {
        let env: NamespaceEnvelope = self
            .client
            .request(
                Method::GET,
                &namespace_path(&self.name),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        self.info = Some(env.namespace);
        Ok(self.info.as_ref().expect("just set"))
    }

    /// `DELETE /namespaces/{name}` — the record only. Deployments still
    /// registered in app-lb are untouched and unreachable until an operator
    /// deregisters them.
    pub async fn delete(&self) -> Result<(), HeyoError> {
        let _: serde_json::Value = self
            .client
            .request(
                Method::DELETE,
                &namespace_path(&self.name),
                None::<&()>,
                RequestOptions::default(),
            )
            .await?;
        Ok(())
    }

    /// The deployments inside this namespace.
    pub fn deployments(&self) -> Deployments {
        Deployments::new(self.client.clone(), &self.name)
    }

    /// The namespace's secrets — see [`Secrets`].
    pub fn secrets(&self) -> Secrets {
        Secrets::new(self.client.clone(), &self.name)
    }
}

/// The secrets of one namespace: write-only values a deployment in the same
/// namespace refers to by id (`vm.env_from`, `build.auth`, …). Walled exactly
/// as deployments are.
#[derive(Clone)]
pub struct Secrets {
    client: HeyoClient,
    base: String,
}

/// `POST /secrets` body.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SecretSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub data: BTreeMap<String, String>,
}

/// `PATCH /secrets/{id}` body: `Some` sets a key, `None` removes it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SecretPatch {
    pub data: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Secrets {
    fn new(client: HeyoClient, namespace: &str) -> Self {
        Self {
            client,
            base: format!("{}/lb/secrets", namespace_path(namespace)),
        }
    }

    fn path(&self, id: Option<&str>) -> String {
        match id {
            Some(id) => format!("{}/{}", self.base, encode_path(id)),
            None => self.base.clone(),
        }
    }

    /// `GET /secrets` — every secret in the namespace, key names only.
    pub async fn list(&self) -> Result<Vec<SecretSummary>, HeyoError> {
        let rows: Option<Vec<SecretSummary>> = self
            .client
            .request(Method::GET, &self.path(None), None::<&()>, RequestOptions::default())
            .await?;
        Ok(rows.unwrap_or_default())
    }

    /// `GET /secrets/{id}`.
    pub async fn get(&self, id: &str) -> Result<SecretSummary, HeyoError> {
        self.client
            .request(Method::GET, &self.path(Some(id)), None::<&()>, RequestOptions::default())
            .await
    }

    /// `POST /secrets` — create or **replace** a secret wholesale. Values go
    /// in and are never readable back out; use [`patch`](Self::patch) to
    /// rotate one key.
    pub async fn put(&self, spec: &SecretSpec) -> Result<SecretSummary, HeyoError> {
        self.client
            .request(Method::POST, &self.path(None), Some(spec), RequestOptions::default())
            .await
    }

    /// `PATCH /secrets/{id}` — set or remove keys; absent keys are left alone.
    pub async fn patch(&self, id: &str, patch: &SecretPatch) -> Result<SecretSummary, HeyoError> {
        self.client
            .request(Method::PATCH, &self.path(Some(id)), Some(patch), RequestOptions::default())
            .await
    }

    /// `DELETE /secrets/{id}` — refused (409) while a deployment still refers
    /// to the secret, unless `force`.
    pub async fn delete(&self, id: &str, force: bool) -> Result<(), HeyoError> {
        let path = if force {
            format!("{}?force=true", self.path(Some(id)))
        } else {
            self.path(Some(id))
        };
        let _: Option<serde_json::Value> = self
            .client
            .request(Method::DELETE, &path, None::<&()>, RequestOptions::default())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployments() -> Deployments {
        let client = HeyoClient::local_at("http://127.0.0.1:1").expect("client");
        Deployments::new(client, "team a")
    }

    #[test]
    fn every_path_goes_through_the_namespace_door() {
        let d = deployments();
        assert_eq!(d.path(&["deployments"]), "/namespaces/team%20a/lb/deployments");
        assert_eq!(
            d.path(&["deployments", "web/1", "vms", "sb-1"]),
            "/namespaces/team%20a/lb/deployments/web%2F1/vms/sb-1"
        );
        assert_eq!(d.path(&["metrics"]), "/namespaces/team%20a/lb/metrics");
        assert_eq!(namespace_path("team-a"), "/namespaces/team-a");
    }

    #[test]
    fn secrets_and_ingress_use_the_namespace_door() {
        let client = HeyoClient::local_at("http://127.0.0.1:1").expect("client");
        let s = Secrets::new(client, "team a");
        assert_eq!(s.path(None), "/namespaces/team%20a/lb/secrets");
        assert_eq!(s.path(Some("db/1")), "/namespaces/team%20a/lb/secrets/db%2F1");
        assert_eq!(deployments().path(&["ingress"]), "/namespaces/team%20a/lb/ingress");
        let patch = SecretPatch {
            data: BTreeMap::from([("url".to_string(), Some("x".to_string())), ("old".to_string(), None)]),
            description: None,
        };
        assert_eq!(
            serde_json::to_value(&patch).unwrap(),
            serde_json::json!({"data": {"old": null, "url": "x"}})
        );
        let summary: SecretSummary = serde_json::from_value(serde_json::json!({
            "id": "db", "namespace": "team-a", "description": null, "keys": ["url"],
            "updated_at": 1, "encrypted_at_rest": false
        }))
        .unwrap();
        assert_eq!(summary.namespace, "team-a");
        let ingress: IngressInfo = serde_json::from_value(serde_json::json!({"ipv4": ["203.0.113.10"]})).unwrap();
        assert_eq!(ingress.ipv4, vec!["203.0.113.10"]);
        assert!(ingress.ipv6.is_empty());
    }

    #[test]
    fn env_from_and_workspace_archive_round_trip() {
        let doc = serde_json::json!({
            "id": "web", "routes": [],
            "vm": {"driver": "firecracker", "port": 3000,
                   "env_from": [{"secret": "db", "key": "url", "as": "DATABASE_URL"}],
                   "workspace_archive": {"archive_id": "ar-1"}}
        });
        let spec: DeploymentSpec = serde_json::from_value(doc.clone()).expect("parse");
        let vm = spec.vm.as_ref().unwrap();
        assert_eq!(vm.env_from[0].env.as_deref(), Some("DATABASE_URL"));
        assert_eq!(vm.workspace_archive.as_ref().unwrap().archive_id, "ar-1");
        assert_eq!(serde_json::to_value(&spec).expect("serialise"), doc);
    }

    #[test]
    fn a_spec_round_trips_without_losing_unknown_fields() {
        let doc = serde_json::json!({
            "id": "web",
            "routes": [{"host": "web.example.com", "weight": 3}],
            "vm": {"driver": "firecracker", "image": "my-app", "port": 3000,
                   "size_class": "small", "mounts": [{"path": "/data"}]},
            "scaling": {"min_replicas": 1, "max_replicas": 4},
            "feed": {"title": "Web"},
            "account_id": "acc-1"
        });
        let spec: DeploymentSpec = serde_json::from_value(doc.clone()).expect("parse");
        assert_eq!(spec.vm.as_ref().unwrap().size_class.as_deref(), Some("small"));
        assert_eq!(spec.scaling.as_ref().unwrap().max_replicas, Some(4));
        assert_eq!(spec.account_id.as_deref(), Some("acc-1"));
        assert_eq!(serde_json::to_value(&spec).expect("serialise"), doc);
    }

    #[test]
    fn a_minimal_spec_serialises_only_what_was_set() {
        let mut spec = DeploymentSpec::new("web", vec![RouteRule { host: Some("w.example.com".into()), ..Default::default() }]);
        spec.upstreams = vec!["127.0.0.1:9000".into()];
        assert_eq!(
            serde_json::to_value(&spec).expect("serialise"),
            serde_json::json!({"id": "web", "routes": [{"host": "w.example.com"}], "upstreams": ["127.0.0.1:9000"]})
        );
        let patch = ScalingPolicy { max_replicas: Some(8), ..Default::default() };
        assert_eq!(serde_json::to_value(&patch).unwrap(), serde_json::json!({"max_replicas": 8}));
        let exec = DeploymentExecOptions { wake: Some(false), ..DeploymentExecOptions::new("uname -a") };
        assert_eq!(serde_json::to_value(&exec).unwrap(), serde_json::json!({"command": "uname -a", "wake": false}));
    }

    #[test]
    fn the_listing_and_status_shapes_parse() {
        let rows: Vec<DeploymentSummary> = serde_json::from_value(serde_json::json!([
            {"id": "web", "kind": "vm", "upstreams": [], "routed": true, "hosts": ["w"],
             "pool": {"ready": 1}, "vms": [{"sandbox_id": "sb-1", "addr": "10.0.0.2:3000",
             "in_flight": 0, "healthy": true, "draining": false, "uptime_secs": 5}]}
        ])).expect("parse");
        assert_eq!(rows[0].vms[0].sandbox_id, "sb-1");
        assert!(rows[0].extra.contains_key("pool"));
        let status: DeploymentStatus = serde_json::from_value(serde_json::json!({
            "spec": {"id": "web", "routes": []}, "kind": "vm", "desired_replicas": 1,
            "ready": 0, "pending": 1, "total_in_flight": 0, "vms": [], "workspace": {"x": 1}
        })).expect("parse");
        assert_eq!(status.spec.id, "web");
        assert!(status.extra.contains_key("workspace"));
    }
}
