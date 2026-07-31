//! Deployment specs and LB configuration.

use crate::secrets::SecretRef;
use heyo_sdk::{SandboxDriver, SandboxSize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_proxy_addr() -> String {
    "0.0.0.0:6188".into()
}
fn default_admin_addr() -> String {
    "127.0.0.1:9090".into()
}
fn default_state_path() -> String {
    "app-lb-state.json".into()
}
fn default_secrets_path() -> String {
    "app-lb-secrets.json".into()
}
fn default_name() -> String {
    "app-lb".into()
}

/// Process-level configuration, supplied via CLI/env rather than the admin API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LbConfig {
    #[serde(default = "default_proxy_addr")]
    pub proxy_addr: String,
    #[serde(default = "default_admin_addr")]
    pub admin_addr: String,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    /// Where stored secrets live. A separate file from `state_path` because it
    /// has different handling: `0600`, and sealed when a key is configured.
    #[serde(default = "default_secrets_path")]
    pub secrets_path: String,
    /// Display name shown in the dashboard header and page title. Defaults to
    /// `app-lb`.
    #[serde(default = "default_name")]
    pub name: String,
    /// heyvm daemon base URL. Defaults to `HeyoClient::local()`'s target.
    pub daemon_url: Option<String>,
    /// Optional HTTP Basic Auth gate on the dashboard and its `/metrics` data
    /// source. Auth is enabled iff `password` is set; `user` defaults to
    /// `"admin"`. The rest of the admin API (deployment CRUD, healthz) is
    /// unaffected — it stays bound to `admin_addr`, localhost by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_password: Option<String>,
    /// When true, the Basic-auth gate also covers the deployment CRUD API
    /// (register/edit/scale/delete/evict and the spec-revealing reads), not just
    /// the dashboard view. It reuses the dashboard credentials, so this requires
    /// `dashboard_password` to be set. `/healthz` stays open for probes.
    #[serde(default)]
    pub admin_auth: bool,
    /// HTTPS listener for the proxy data plane, bound *in addition to* the
    /// plaintext `proxy_addr`. Enabled when ACME is on or a static cert pair is
    /// configured. Upstreams stay plaintext regardless — the guest IP is on a
    /// host-local tap network.
    #[serde(default = "default_tls_addr")]
    pub tls_addr: String,
    /// Whether `tls_addr` was configured explicitly rather than defaulted.
    /// Setting it while TLS stays disabled is a misconfiguration worth warning
    /// about — the HTTPS listener is silently skipped otherwise. Not part of the
    /// serialized form; it describes how the config was built, not what it says.
    #[serde(skip)]
    pub tls_addr_explicit: bool,
    /// A static certificate pair. Once ACME is enabled this is the *fallback*,
    /// served for any SNI with no issued certificate of its own (a `host_suffix`
    /// deployment, or a host whose issuance hasn't completed). Both or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,
    /// ACME account contact. Setting it is what enables automatic certificates;
    /// leaving it unset keeps app-lb's behaviour entirely static.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_email: Option<String>,
    /// Where the ACME account key and issued certificates are stored. Should be
    /// mode `0700` — it holds private keys.
    #[serde(default = "default_acme_dir")]
    pub acme_dir: String,
    /// ACME directory URL. Defaults to Let's Encrypt production; point it at
    /// staging for any testing, because production rate limits are per-account
    /// per-week and a failed-validation loop will lock issuance out for hours.
    #[serde(default = "default_acme_directory")]
    pub acme_directory: String,
    /// Scratch space for git checkouts driven by `build`. One directory per
    /// deployment, kept between builds so a rebuild is a fetch rather than a
    /// full clone.
    #[serde(default = "default_build_dir")]
    pub build_dir: String,
    /// The `heyvm` CLI that turns a Dockerfile into a guest rootfs. Image
    /// building has no daemon API — it needs a local `docker`, `mke2fs` and
    /// `fakeroot` — so app-lb shells out to the binary on this host.
    #[serde(default = "default_heyvm_bin")]
    pub heyvm_bin: String,
    /// The `art` CLI that materializes a rootfs out of a *local* artifact store.
    /// Only reached when a deployment's `artifact.store` is a path; a store
    /// reached by URL needs no binary here, because app-lb streams the blob
    /// itself.
    #[serde(default = "default_art_bin")]
    pub art_bin: String,
    /// Where a pulled rootfs is written so heyvmd can boot it. Unset resolves
    /// the same way mvm-ctrl does: `$MVM_DATA_DIR/images/firecracker`, else
    /// `<home>/.heyo/images/firecracker` — where `<home>` is `heyvm_home` when
    /// set, because that is the daemon's home and the daemon only finds images
    /// under its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images_dir: Option<String>,
    #[serde(default = "default_git_bin")]
    pub git_bin: String,
    /// The `aws` CLI, used for the DNS-01 challenge. Only reached when
    /// `acme_wildcards` is non-empty.
    #[serde(default = "default_aws_bin")]
    pub aws_bin: String,
    /// Domains to certify with a **wildcard** certificate, e.g.
    /// `sb.example.com` → a cert covering `sb.example.com` and
    /// `*.sb.example.com`.
    ///
    /// This is what makes a fleet of sandboxes possible. Let's Encrypt caps new
    /// certificates at 50 per registered domain per week, so issuing per
    /// hostname stops working on the first afternoon; one wildcard covers every
    /// hostname under it forever. Any exact `host` route covered by one of these
    /// is skipped by the per-host issuer for the same reason.
    ///
    /// Needs `route53_zone_id`: wildcards are only issued over DNS-01.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acme_wildcards: Vec<String>,
    /// The Route 53 hosted zone holding `acme_wildcards`, where the DNS-01
    /// challenge records are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route53_zone_id: Option<String>,
    /// Shell that a static deployment's `update.commands` run through. They are
    /// written as shell lines (`git pull && cargo build --release`), so there is
    /// one; pointing this at `bash` buys bashisms.
    #[serde(default = "default_update_shell")]
    pub update_shell: String,
    /// Ceiling on one build step (checkout, then image build), after which the
    /// child is killed. A stuck `docker build` must not hold a deployment's
    /// build slot forever.
    #[serde(default = "default_build_timeout_secs")]
    pub build_timeout_secs: u64,
    /// `HOME` for the `heyvm` child, when app-lb does not run as the same user
    /// as heyvmd. It decides where the built image lands
    /// (`$HOME/.heyo/images/firecracker/<name>.ext4`) — and the daemon only
    /// finds images under *its own* home, so a mismatch builds successfully and
    /// then boots nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heyvm_home: Option<String>,
}

fn default_tls_addr() -> String {
    "0.0.0.0:6189".into()
}
fn default_build_dir() -> String {
    "/var/lib/app-lb/builds".into()
}
fn default_heyvm_bin() -> String {
    "heyvm".into()
}
fn default_art_bin() -> String {
    "art".into()
}
fn default_git_bin() -> String {
    "git".into()
}
fn default_aws_bin() -> String {
    "aws".into()
}
fn default_update_shell() -> String {
    "/bin/sh".into()
}
fn default_build_timeout_secs() -> u64 {
    1800
}
fn default_acme_dir() -> String {
    "/var/lib/app-lb/acme".into()
}
fn default_acme_directory() -> String {
    crate::acme::AcmeConfig::production_directory()
}

impl Default for LbConfig {
    fn default() -> Self {
        Self {
            proxy_addr: default_proxy_addr(),
            admin_addr: default_admin_addr(),
            state_path: default_state_path(),
            secrets_path: default_secrets_path(),
            name: default_name(),
            daemon_url: None,
            dashboard_user: None,
            dashboard_password: None,
            admin_auth: false,
            tls_addr: default_tls_addr(),
            tls_addr_explicit: false,
            tls_cert_path: None,
            tls_key_path: None,
            acme_email: None,
            acme_dir: default_acme_dir(),
            acme_directory: default_acme_directory(),
            build_dir: default_build_dir(),
            heyvm_bin: default_heyvm_bin(),
            art_bin: default_art_bin(),
            images_dir: None,
            git_bin: default_git_bin(),
            aws_bin: default_aws_bin(),
            acme_wildcards: Vec::new(),
            route53_zone_id: None,
            update_shell: default_update_shell(),
            build_timeout_secs: default_build_timeout_secs(),
            heyvm_home: None,
        }
    }
}

impl LbConfig {
    /// ACME is on iff a contact address was configured.
    pub fn acme_enabled(&self) -> bool {
        self.acme_email.is_some()
    }

    /// Whether to bind the HTTPS listener at all. ACME alone is enough: it will
    /// have certificates shortly even if none exist at startup.
    pub fn tls_enabled(&self) -> bool {
        self.acme_enabled() || self.tls_cert_path.is_some()
    }
}

/// How a request is matched to a deployment.
///
/// A rule matches when *every* populated field matches. An empty rule matches
/// nothing (rejected at registration) rather than everything, so a typo can't
/// silently swallow all traffic.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RouteRule {
    /// Exact hostname match, case-insensitive, port stripped. For HTTP/2 this
    /// is matched against `:authority`, which carries no `Host` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Subdomain (wildcard) host match: a domain whose apex *and* any subdomain
    /// match — `host_suffix: "apps.example.com"` routes `apps.example.com`,
    /// `a.apps.example.com`, and `x.y.apps.example.com`, but not
    /// `notapps.example.com` (the match is anchored at a label boundary). A
    /// leading dot is accepted and ignored, so `.apps.example.com` is equivalent.
    /// An exact `host` always outranks a `host_suffix`, and a longer suffix
    /// outranks a shorter one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_suffix: Option<String>,
    /// Path prefix match, e.g. `/api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

impl RouteRule {
    pub fn is_empty(&self) -> bool {
        self.host.is_none() && self.host_suffix.is_none() && self.path_prefix.is_none()
    }

    /// Longer/more-specific rules must win, so rules are ranked before matching.
    /// The tiers don't overlap: an exact host beats *any* subdomain rule, which
    /// beats any path-only rule; within a tier a longer suffix or path wins.
    pub fn specificity(&self) -> usize {
        let mut score = 0usize;
        if self.host.is_some() {
            score += 1_000_000;
        }
        if let Some(s) = &self.host_suffix {
            score += 100_000 + s.trim_start_matches('.').len();
        }
        score + self.path_prefix.as_ref().map_or(0, |p| p.len())
    }

    pub fn matches(&self, host: Option<&str>, path: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        if let Some(want) = &self.host {
            match host {
                Some(got) if got.eq_ignore_ascii_case(want) => {}
                _ => return false,
            }
        }
        if let Some(suffix) = &self.host_suffix {
            match host {
                Some(got) if host_in_domain(got, suffix) => {}
                _ => return false,
            }
        }
        if let Some(prefix) = &self.path_prefix
            && !path.starts_with(prefix.as_str())
        {
            return false;
        }
        true
    }
}

/// Whether `host` is the domain `suffix` itself or a subdomain of it, matched at
/// a label boundary so `apps.example.com` covers `a.apps.example.com` but never
/// `notapps.example.com`. Case-insensitive; a leading dot on `suffix` is ignored.
fn host_in_domain(host: &str, suffix: &str) -> bool {
    let suffix = suffix.trim_start_matches('.');
    if suffix.is_empty() {
        return false;
    }
    if host.eq_ignore_ascii_case(suffix) {
        return true; // apex
    }
    // A proper subdomain: the char just before the suffix must be a dot, so
    // `notapps.example.com` doesn't match suffix `apps.example.com`.
    host.len() > suffix.len()
        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        && host[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn default_target_concurrency() -> u32 {
    10
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
    30
}
/// Deliberately several times `default_cold_start_timeout_secs`. The two bound
/// different things: a request gives up long before the VM does, because a boot
/// that overran one caller's patience may still be the boot that serves the next
/// one. Five minutes is past the point where a Firecracker guest that is going to
/// come up has come up, so what is left is a guest whose server never started.
fn default_boot_timeout_secs() -> u64 {
    300
}

/// What becomes of a VM the autoscaler no longer needs.
///
/// The distinction only exists because a *sandbox* is not a replica. Retiring
/// one of four interchangeable web VMs should reclaim everything it held;
/// retiring the single VM that is somebody's working directory should not.
///
/// Note what `Retain` can and cannot keep, because the daemon decides this and
/// not app-lb: a stopped sandbox keeps its record and its **`/workspace` data
/// disk** (`vm.disk_size_gb`), and loses its memory and any writes to the
/// rootfs — mvm-ctrl recopies the rootfs from the base image on every cold boot.
/// A `Retain` deployment with no data disk therefore saves boot time and nothing
/// else. Persistent state has to live under `/workspace`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleAction {
    /// Kill it: the sandbox, its data disk and its rootfs all go. The default,
    /// and right for a pool of interchangeable replicas.
    #[default]
    Destroy,
    /// Stop it: the sandbox stays, keeping its data disk, and a later request or
    /// `exec` resumes it instead of booting a fresh one.
    Retain,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScalingPolicy {
    #[serde(default)]
    pub min_replicas: u32,
    #[serde(default = "default_max_replicas")]
    pub max_replicas: u32,
    /// Idle-but-ready spares kept above what current load requires.
    #[serde(default)]
    pub warm_pool: u32,
    /// In-flight requests per VM the autoscaler aims for.
    #[serde(default = "default_target_concurrency")]
    pub target_concurrency: u32,
    #[serde(default = "default_scale_to_zero_after_secs")]
    pub scale_to_zero_after_secs: u64,
    /// How long a request will wait for a VM to boot before giving up with 503.
    #[serde(default = "default_cold_start_timeout_secs")]
    pub cold_start_timeout_secs: u64,
    /// How long a draining VM may keep serving in-flight requests before it is
    /// killed anyway.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
    /// How long a booting VM has to pass its health check before the autoscaler
    /// gives up on it, kills it and lets the next tick create a replacement.
    ///
    /// Without a deadline here a VM that boots but never serves — the daemon says
    /// `Running`, the guest's process died or never started — is re-queued every
    /// tick indefinitely, so the deployment sits at zero replicas with no error
    /// anywhere. The pool's own `min_replicas` can never be met and nothing says
    /// why. `0` restores that unbounded wait for a deployment whose boots are
    /// genuinely open-ended.
    #[serde(default = "default_boot_timeout_secs")]
    pub boot_timeout_secs: u64,
    /// Whether a VM the autoscaler retires is destroyed or merely stopped. See
    /// [`IdleAction`]; defaults to `destroy`, which is the historical behaviour.
    #[serde(default)]
    pub idle_action: IdleAction,
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
            boot_timeout_secs: default_boot_timeout_secs(),
            idle_action: IdleAction::default(),
        }
    }
}

fn default_health_path() -> Option<String> {
    Some("/".into())
}
fn default_health_timeout_secs() -> u64 {
    2
}

/// How a freshly-booted VM is proven ready before it joins the pool.
///
/// This exists because the SDK's readiness signal is not trustworthy on its own
/// (see `vm::wait_until_running`), so we always probe the guest ourselves.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthCheck {
    /// `None` means a bare TCP connect is enough.
    #[serde(default = "default_health_path")]
    pub path: Option<String>,
    /// Health port, if the guest serves health somewhere other than `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default = "default_health_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for HealthCheck {
    fn default() -> Self {
        Self {
            path: default_health_path(),
            port: None,
            timeout_secs: default_health_timeout_secs(),
        }
    }
}

/// The VM template. Mirrors `SandboxCreateOptions`, minus the fields the LB owns
/// (`name` is generated per-replica; `wait_for_ready` is always zero because the
/// autoscaler polls readiness itself rather than blocking its reconcile loop).
///
/// Note the SDK cannot express vcpu/memory/mounts directly — `size_class` is the
/// only resource knob, and the daemon resolves it host-side.
/// `PartialEq` is load-bearing: an in-place edit keeps the running pool only
/// when the VM *template* is unchanged, so the update path compares old and new
/// `VmSpec`s to decide whether the VMs must be rebuilt.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct VmSpec {
    /// Must be `firecracker` or `kvm`; `libvirt` is rejected at registration.
    pub driver: SandboxDriver,
    /// Defaults to `ubuntu:24.04` daemon-side when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The guest port traffic is proxied to.
    pub port: u16,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hooks: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_ports: Vec<u16>,
    /// Backstop TTL so VMs die on their own if this LB crashes and never reaps
    /// them. Renewed by the autoscaler while it is alive.
    #[serde(default = "default_vm_ttl_secs")]
    pub ttl_seconds: u64,
}

fn default_vm_ttl_secs() -> u64 {
    3600
}

/// Where a deployment's guest image comes from: a git checkout plus a Dockerfile.
///
/// This is the *source*, not the running image. A build clones (or fetches)
/// `repo` at `ref`, hands the Dockerfile to `heyvm mvm build`, and only then
/// writes the resulting image name into [`VmSpec::image`] — so the spec always
/// says which image is actually booting, and this block says where the next one
/// will come from. Editing it never disturbs running VMs; running a build does.
///
/// Note what is deliberately absent: build arguments and a registry. The image
/// is an ext4 rootfs on this host, built from a Dockerfile the daemon never
/// sees, and `heyvm mvm build` exposes neither `--build-arg` nor a push target
/// for the local-only path.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct BuildSpec {
    /// Git remote: `https://…`, `ssh://…`, `git@host:path`, or a local path.
    pub repo: String,
    /// Branch, tag or commit to build. `None` follows the remote's default
    /// branch, which is what makes `POST …/build` mean "ship what is on main".
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Dockerfile path *within the checkout*. `None` looks for one: `Dockerfile`
    /// at the context root, else a unique `Dockerfile` within three directories
    /// of it. Ambiguity is an error, never a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    /// Build context within the checkout. Defaults to the Dockerfile's directory,
    /// matching `heyvm mvm build`'s own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Base name for built images; the commit is appended, so one deployment's
    /// builds are `<name>-<short sha>`. Defaults to the deployment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_name: Option<String>,
    /// Rootfs size passed to `heyvm mvm build --size-mb`. Unset lets heyvm size
    /// it from the exported tar (×1.2 + 64 MB), which is right until the guest
    /// writes to its own rootfs at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_size_mb: Option<u64>,
    /// Credential for a private repo, as a reference into the secret store. Only
    /// meaningful for HTTP(S) remotes — an `ssh://` or `git@` remote authenticates
    /// with the host's own key material and should leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SecretRef>,
}

impl BuildSpec {
    /// The image name for a given commit. Lowercased and stripped to what both
    /// `docker build -t` and heyvm's `<name>.ext4` filename accept, because the
    /// deployment id it defaults to is only constrained by the route table.
    pub fn image_for(&self, deployment_id: &str, commit: &str) -> String {
        let base = self.image_name.as_deref().unwrap_or(deployment_id);
        let base = sanitize_image_name(base);
        let short: String = commit.chars().take(12).collect();
        if short.is_empty() {
            base
        } else {
            format!("{base}-{short}")
        }
    }

    fn validate(&self) -> Result<(), SpecError> {
        if self.repo.trim().is_empty() {
            return Err(SpecError::EmptyRepo);
        }
        if !is_supported_repo_url(&self.repo) {
            return Err(SpecError::UnsupportedRepoUrl(self.repo.clone()));
        }
        if let Some(r) = &self.git_ref
            && !is_safe_git_ref(r)
        {
            return Err(SpecError::BadBuildRef(r.clone()));
        }
        for path in [&self.dockerfile, &self.context].into_iter().flatten() {
            if !is_safe_relative_path(path) {
                return Err(SpecError::BadBuildPath(path.clone()));
            }
        }
        if let Some(name) = &self.image_name
            && sanitize_image_name(name).is_empty()
        {
            return Err(SpecError::BadImageName(name.clone()));
        }
        if let Some(auth) = &self.auth {
            auth.validate().map_err(|e| SpecError::BadSecretRef {
                field: "build.auth",
                detail: e.to_string(),
            })?;
        }
        Ok(())
    }
}

/// Where a deployment's guest image comes from: a rootfs already in an artifact
/// store, addressed by content.
///
/// The counterpart of [`BuildSpec`], and the same shape of thing: the *source*
/// of the next image, not the running one. A pull resolves `reference` to a
/// blob digest, materializes that blob as an ext4 rootfs heyvmd can boot, and
/// only then rewrites [`VmSpec::image`] — so the spec still says which image is
/// actually booting and this block says where the next one comes from.
///
/// What makes this different from a build is that nothing is *produced*. The
/// digest names bytes that already exist, so the same `artifact` block resolves
/// to the same rootfs on every host that can reach the store, which is the whole
/// reason to prefer it over rebuilding a Dockerfile per machine.
///
/// See <https://github.com/sarocu/artifacts> — `art heyvm import` puts heyvm's
/// base images in, `serverctl artifact push` puts a locally-built one in, and
/// either is pullable here.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ArtifactSpec {
    /// The store to pull from, in one of two forms:
    ///
    /// * `http://host:port` — a remote `art serve`. app-lb resolves and streams
    ///   the blob itself, verifying the digest as the bytes land.
    /// * `/abs/path` — a store root (`ART_ROOT`) on this host. app-lb shells out
    ///   to `art heyvm materialize`, which skips the blob's holes instead of
    ///   copying its zeros.
    ///
    /// A local store is by far the faster of the two and is what a host running
    /// its own store should use; the URL form is what makes one store serve a
    /// fleet.
    pub store: String,
    /// A tag (`debian-hermes`) or a 64-hex digest. A tag is resolved at pull
    /// time, so a deployment pinned to one follows whatever the tag moves to; a
    /// digest is immutable and is what a rollback should name.
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    /// API key for a store started with `ART_API_KEY`, as a reference into the
    /// secret store. Only meaningful for the URL form — a local store is
    /// protected by file permissions, not a header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SecretRef>,
    /// Extend the materialized rootfs to this many gigabytes. Sparse, so it
    /// costs no disk until the guest writes; heyvm still runs the `resize2fs`
    /// that lets the guest filesystem use the room. Set it when the image was
    /// built small and the workload needs space on `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grow_gb: Option<u64>,
    /// Base name for the materialized image; the digest is appended, so one
    /// deployment's pulls are `<name>-<short digest>`. Defaults to the
    /// deployment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_name: Option<String>,
}

impl ArtifactSpec {
    /// The image name for a given blob digest, in the same shape
    /// [`BuildSpec::image_for`] gives a commit.
    ///
    /// Content-addressed on purpose: the same digest always materializes to the
    /// same filename, so a re-pull of bytes already on disk is a no-op the job
    /// can detect by name alone, and two deployments pulling one image share it.
    pub fn image_for(&self, deployment_id: &str, digest: &str) -> String {
        let base = self.image_name.as_deref().unwrap_or(deployment_id);
        let base = sanitize_image_name(base);
        let short: String = digest.chars().take(12).collect();
        if short.is_empty() {
            base
        } else {
            format!("{base}-{short}")
        }
    }

    /// Whether `store` names a remote `art serve` rather than a path on this
    /// host. The two are told apart by the scheme, which is also what decides
    /// whether `auth` means anything.
    pub fn is_remote(&self) -> bool {
        let s = self.store.trim();
        s.starts_with("http://") || s.starts_with("https://")
    }

    fn validate(&self) -> Result<(), SpecError> {
        let store = self.store.trim();
        if store.is_empty() {
            return Err(SpecError::EmptyArtifactStore);
        }
        let store_ok = if let Some(rest) = store
            .strip_prefix("http://")
            .or_else(|| store.strip_prefix("https://"))
        {
            !rest.is_empty() && !rest.contains(char::is_whitespace)
        } else {
            // A store root, and only an absolute one: app-lb's working
            // directory is not something a spec author can see, so a relative
            // path would name a different store depending on how the LB was
            // started.
            std::path::Path::new(store).is_absolute()
                && !store.contains("..")
                && !store.contains('\0')
        };
        if !store_ok {
            return Err(SpecError::UnsupportedArtifactStore(self.store.clone()));
        }

        if !is_valid_artifact_ref(&self.artifact_ref) {
            return Err(SpecError::BadArtifactRef(self.artifact_ref.clone()));
        }
        if let Some(name) = &self.image_name
            && sanitize_image_name(name).is_empty()
        {
            return Err(SpecError::BadImageName(name.clone()));
        }
        if self.grow_gb == Some(0) {
            return Err(SpecError::ZeroGrow);
        }
        if let Some(auth) = &self.auth {
            auth.validate().map_err(|e| SpecError::BadSecretRef {
                field: "artifact.auth",
                detail: e.to_string(),
            })?;
        }
        Ok(())
    }
}

/// A tag or a digest, as the artifact store itself defines them.
///
/// Mirrors `TagName::parse` and `Digest::parse` in the `artifacts` crate rather
/// than deferring to the store, because a reference that store would reject
/// should be a registration error here and not a job that fails minutes later.
/// A digest is 64 lowercase hex characters; a tag is `[A-Za-z0-9._-]`, not
/// starting with `-` or `.` — which also means it can never contain a path
/// separator or a `..`, and so can never travel outside the store when it is
/// pasted into a URL path.
fn is_valid_artifact_ref(r: &str) -> bool {
    if r.is_empty() || r.len() > 128 {
        return false;
    }
    let first = r.as_bytes()[0];
    if first == b'-' || first == b'.' {
        return false;
    }
    r.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Keep to what a docker tag component and an ext4 filename both allow.
fn sanitize_image_name(name: &str) -> String {
    let lowered: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A docker tag must start with an alphanumeric, and a filename starting with
    // a dot would be hidden from `heyvm mvm images`.
    lowered
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .trim_end_matches(['-', '.', '_'])
        .chars()
        .take(64)
        .collect()
}

/// Remotes app-lb will hand to `git fetch`.
///
/// The list is short on purpose: `git` treats an argument beginning with `-` as
/// a flag, and `ext::`/`--upload-pack=` style remotes run a command of the
/// remote's choosing on this host. A spec comes in over the admin API, so the
/// URL is input, not configuration.
fn is_supported_repo_url(repo: &str) -> bool {
    let repo = repo.trim();
    if repo.starts_with('-') || repo.contains(char::is_whitespace) {
        return false;
    }
    for scheme in ["https://", "http://", "ssh://", "file://"] {
        if let Some(rest) = repo.strip_prefix(scheme) {
            return !rest.is_empty();
        }
    }
    if repo.starts_with('/') {
        return true; // a path on this host, for testing and vendored sources
    }
    // scp-like: user@host:path, which git accepts and which has no scheme.
    matches!(repo.split_once('@'), Some((user, rest))
        if !user.is_empty() && rest.contains(':') && !rest.starts_with(':'))
}

/// A ref is passed to `git fetch` as an argument and appears in an image name,
/// so it may not look like a flag or carry anything a shell or a path would
/// reinterpret. Git's own rules already forbid most of this in a real ref.
fn is_safe_git_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.starts_with('/')
        && r.chars().all(|c| {
            !c.is_whitespace()
                && !c.is_control()
                && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\'' | '"')
        })
}

/// A path inside the checkout: relative, and unable to climb out of it.
fn is_safe_relative_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('-')
        && !p.contains('\0')
        && std::path::Path::new(p)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

/// How a *static* (proxy_pass) deployment's backend is updated: a working
/// directory on the app-lb host, and commands to run in it.
///
/// The managed counterpart of this is [`BuildSpec`], and the asymmetry is the
/// point. A managed deployment's backend is a microVM app-lb owns, so updating
/// it means producing a new image. A static deployment's backend is a process
/// somebody else runs — usually on this same host, under supervisord or systemd
/// — so updating it means doing on the host what a person would otherwise ssh in
/// and do: pull, build, restart.
///
/// Nothing in the spec changes when this runs. The upstreams are the same
/// addresses; what moved is the code answering on them. That is why the job
/// re-probes those addresses afterwards: "the commands exited 0" is not the same
/// claim as "the service is serving".
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct UpdateSpec {
    /// Absolute path on the app-lb host. Must exist when the job runs — app-lb
    /// never creates it, because a typo that silently created an empty directory
    /// and ran `git pull` in it would be worse than an error.
    pub working_dir: String,
    /// Commands, run in order, each through `sh -c` in `working_dir`. The first
    /// non-zero exit stops the job.
    pub commands: Vec<String>,
    /// Extra environment for every command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// Environment pulled from the secret store, so a deploy key or registry
    /// token reaches the commands without being written into this spec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<SecretEnv>,
    /// Git credential for commands that fetch (`git pull`), supplied through
    /// `GIT_ASKPASS`. Only meaningful for HTTP(S) remotes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SecretRef>,
    /// Ceiling on a single command. Defaults to `APP_LB_BUILD_TIMEOUT_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// How long to wait, after the commands, for every upstream to answer its
    /// health check. `0` skips verification — appropriate when the commands do
    /// not restart anything, and wrong otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_timeout_secs: Option<u64>,
}

fn default_verify_timeout_secs() -> u64 {
    60
}

impl UpdateSpec {
    pub fn verify_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.verify_timeout_secs
                .unwrap_or_else(default_verify_timeout_secs),
        )
    }

    fn validate(&self) -> Result<(), SpecError> {
        let dir = self.working_dir.trim();
        if dir.is_empty() {
            return Err(SpecError::EmptyWorkingDir);
        }
        // Absolute, because the job's CWD is app-lb's own and a relative path
        // would resolve somewhere nobody intended.
        if !dir.starts_with('/')
            || dir.contains('\0')
            || std::path::Path::new(dir)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(SpecError::BadWorkingDir(self.working_dir.clone()));
        }
        if self.commands.is_empty() {
            return Err(SpecError::NoCommands);
        }
        for c in &self.commands {
            if c.trim().is_empty() || c.contains('\0') {
                return Err(SpecError::BadCommand(c.clone()));
            }
        }
        for from in &self.env_from {
            from.validate()?;
        }
        if let Some(auth) = &self.auth {
            auth.validate().map_err(|e| SpecError::BadSecretRef {
                field: "update.auth",
                detail: e.to_string(),
            })?;
        }
        if self.timeout_secs == Some(0) {
            return Err(SpecError::ZeroTimeout);
        }
        Ok(())
    }
}

/// One secret value, exported to the update commands as an environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SecretEnv {
    pub secret: String,
    #[serde(default = "default_env_secret_key")]
    pub key: String,
    /// Variable name. Defaults to the key, upper-cased — `{"secret": "obs",
    /// "key": "ingest_token"}` arrives as `INGEST_TOKEN`.
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

fn default_env_secret_key() -> String {
    "token".into()
}

impl SecretEnv {
    pub fn secret_ref(&self) -> SecretRef {
        SecretRef {
            secret: self.secret.clone(),
            key: self.key.clone(),
            username: None,
        }
    }

    pub fn env_name(&self) -> String {
        self.env
            .clone()
            .unwrap_or_else(|| self.key.to_ascii_uppercase())
    }

    fn validate(&self) -> Result<(), SpecError> {
        self.secret_ref()
            .validate()
            .map_err(|e| SpecError::BadSecretRef {
                field: "update.env_from",
                detail: e.to_string(),
            })?;
        let name = self.env_name();
        // Not a hard requirement of execve, but a variable name that needs
        // quoting is one the shell cannot read back — so it would be set and
        // then invisible to the command that wanted it.
        let usable = !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if usable {
            Ok(())
        } else {
            Err(SpecError::BadEnvName(name))
        }
    }
}

/// An optional sign-in gate in front of a deployment.
///
/// Orthogonal to the backend kind on purpose: a managed VM pool and a static
/// `proxy_pass` target are gated identically, because this happens in the proxy
/// before either is reached. The application behind it needs to know nothing
/// about OAuth — it sees only requests that got past the gate, optionally with
/// the caller's identity in headers.
///
/// The client *secret* is a [`SecretRef`], not a value, for the same reason a
/// build's git token is: the admin API echoes specs back and the state file
/// holds them in the clear.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AuthGate {
    /// Which identity provider. Only Google today; the field exists so a spec
    /// written now still parses when there is a second one.
    #[serde(default)]
    pub provider: AuthProvider,
    /// OAuth client id from the provider's console.
    pub client_id: String,
    /// Where the client secret is stored.
    pub client_secret: SecretRef,
    /// Google Workspace domains whose accounts may enter, matched against the
    /// `hd` claim (not the email's suffix — see `AuthGate::allows`). `["*"]`
    /// means *any* Google account, which is a real choice and has to be spelled
    /// out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
    /// Individual addresses allowed regardless of domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_emails: Vec<String>,
    /// Path prefixes served without the gate: health endpoints, webhook
    /// receivers, anything with its own authentication.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_paths: Vec<String>,
    /// Where app-lb's own endpoints live under this deployment's hostname:
    /// `<base_path>/callback`, `/login` and `/logout`. The callback is the URL
    /// that must be registered with the provider.
    #[serde(default = "default_auth_base_path")]
    pub base_path: String,
    /// How long a session lasts before the user is sent back to the provider.
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// The session cookie's name. Worth changing only if it collides with one
    /// the application already sets.
    #[serde(default = "default_auth_cookie_name")]
    pub cookie_name: String,
    /// Redirect URI to send the provider, when app-lb cannot derive it from the
    /// request — something in front rewriting the host or terminating TLS
    /// elsewhere. Normally unset: `https://<request host><base_path>/callback`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// Pass the identity upstream as `x-auth-request-{email,user,name}`
    /// (oauth2-proxy's spelling, so apps that already read those work unchanged).
    /// Those headers are stripped from the incoming request either way, so a
    /// client cannot forge them.
    #[serde(default = "default_true")]
    pub forward_identity: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthProvider {
    #[default]
    Google,
}

fn default_auth_base_path() -> String {
    "/__applb/auth".into()
}
fn default_session_ttl_secs() -> u64 {
    43_200 // 12h — a working day plus slack, short enough that a revoked
           // account loses access the same day.
}
fn default_auth_cookie_name() -> String {
    "applb_session".into()
}
fn default_true() -> bool {
    true
}

impl AuthGate {
    /// `<base_path>/callback` — the URL the provider redirects back to, and the
    /// one that has to be registered with it.
    pub fn callback_path(&self) -> String {
        format!("{}/callback", self.base_path.trim_end_matches('/'))
    }

    pub fn login_path(&self) -> String {
        format!("{}/login", self.base_path.trim_end_matches('/'))
    }

    pub fn logout_path(&self) -> String {
        format!("{}/logout", self.base_path.trim_end_matches('/'))
    }

    /// Whether `path` is served without the gate.
    pub fn is_public(&self, path: &str) -> bool {
        self.public_paths.iter().any(|p| path.starts_with(p.as_str()))
    }

    /// Whether this identity may enter.
    ///
    /// The domain is matched on the provider's `hd` claim rather than the part
    /// after `@`, because only `hd` says the account is *governed by* that
    /// Workspace domain. A personal Gmail account can carry any email address a
    /// Workspace admin has not claimed, so trusting the suffix would let
    /// `someone@yourcompany.com` in from an account you do not control.
    pub fn allows(&self, email: &str, hosted_domain: Option<&str>) -> bool {
        let email = email.to_ascii_lowercase();
        if self.allowed_emails.iter().any(|e| e.eq_ignore_ascii_case(&email)) {
            return true;
        }
        self.allowed_domains.iter().any(|d| {
            d == "*"
                || hosted_domain.is_some_and(|hd| hd.eq_ignore_ascii_case(d.trim_start_matches('@')))
        })
    }

    /// A fingerprint of everything that decides *who* may enter. It goes into
    /// each session, so tightening the allow-list or rotating the client id
    /// invalidates the sessions issued under the old policy instead of leaving
    /// a removed user signed in until their cookie expires.
    pub fn policy_fingerprint(&self) -> String {
        let mut domains: Vec<String> = self
            .allowed_domains
            .iter()
            .map(|d| d.to_ascii_lowercase())
            .collect();
        let mut emails: Vec<String> = self
            .allowed_emails
            .iter()
            .map(|e| e.to_ascii_lowercase())
            .collect();
        domains.sort();
        emails.sort();
        let material = format!(
            "{:?}|{}|{}|{}",
            self.provider,
            self.client_id,
            domains.join(","),
            emails.join(",")
        );
        let digest = openssl::hash::hash(
            openssl::hash::MessageDigest::sha256(),
            material.as_bytes(),
        )
        .expect("sha256 of a byte string cannot fail");
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    fn validate(&self, routes: &[RouteRule]) -> Result<(), SpecError> {
        if self.client_id.trim().is_empty() {
            return Err(SpecError::EmptyClientId);
        }
        self.client_secret
            .validate()
            .map_err(|e| SpecError::BadSecretRef {
                field: "auth.client_secret",
                detail: e.to_string(),
            })?;

        // An empty allow-list would gate the deployment behind "has a Google
        // account", which is nearly everyone. That is a legitimate thing to
        // want, so it can be asked for — but only in writing.
        if self.allowed_domains.is_empty() && self.allowed_emails.is_empty() {
            return Err(SpecError::EmptyAllowList);
        }
        for d in &self.allowed_domains {
            // Surrounding whitespace is rejected rather than trimmed: the match
            // is exact, so a stored " example.com " would let nobody in while
            // looking exactly like a rule that does.
            if d.trim().len() != d.len() || d.is_empty() || (d != "*" && !d.contains('.')) {
                return Err(SpecError::BadAllowedDomain(d.clone()));
            }
        }
        for e in &self.allowed_emails {
            if !e.contains('@') || e.trim().len() != e.len() {
                return Err(SpecError::BadAllowedEmail(e.clone()));
            }
        }

        let base = self.base_path.trim_end_matches('/');
        if !base.starts_with('/') || base.len() < 2 || base.contains(char::is_whitespace) {
            return Err(SpecError::BadAuthBasePath(self.base_path.clone()));
        }
        for p in &self.public_paths {
            if !p.starts_with('/') {
                return Err(SpecError::BadPublicPath(p.clone()));
            }
        }
        if self.session_ttl_secs == 0 {
            return Err(SpecError::ZeroSessionTtl);
        }
        if self.cookie_name.is_empty()
            || !self
                .cookie_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        {
            return Err(SpecError::BadCookieName(self.cookie_name.clone()));
        }

        // The provider redirects the browser to `<host><callback>`, and that
        // request has to route back to *this* deployment or the login can never
        // complete. A rule with a path prefix only matches paths under it, so a
        // deployment routed at `/app` needs its base_path there too. Caught here
        // rather than as a 404 halfway through someone's first sign-in.
        let callback = self.callback_path();
        let routable = routes
            .iter()
            .any(|r| r.path_prefix.as_ref().is_none_or(|p| callback.starts_with(p.as_str())));
        if !routable {
            return Err(SpecError::AuthCallbackUnroutable(callback));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeploymentSpec {
    pub id: String,
    pub routes: Vec<RouteRule>,
    /// The VM template for a *managed* deployment: app-lb boots and autoscales a
    /// pool of microVMs. Mutually exclusive with `upstreams`; exactly one of the
    /// two must be set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<VmSpec>,
    #[serde(default)]
    pub scaling: ScalingPolicy,
    #[serde(default)]
    pub health: HealthCheck,
    /// A *static* (proxy_pass) deployment: forward matched requests to a fixed
    /// set of upstream addresses (`host:port` or `ip:port`) with no VM lifecycle
    /// and no autoscaling. Load-balanced least-in-flight with failover, and
    /// health-re-probed by the autoscaler so a recovered upstream rejoins.
    /// Mutually exclusive with `vm`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    /// Where `vm.image` is built from: a git repo and a Dockerfile. Optional —
    /// a deployment can go on naming a prebuilt image — and only valid on a
    /// managed deployment, since a static one has no image to build.
    ///
    /// Not part of `VmSpec`, so editing it is not a template change and does not
    /// recycle the pool. The pool moves when a build finishes and rewrites
    /// `vm.image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildSpec>,
    /// Where `vm.image` is pulled from: a rootfs already in an artifact store.
    /// The alternative to `build` and mutually exclusive with it — both rewrite
    /// `vm.image`, and a deployment with two sources for it would have no
    /// answer to "where did this image come from".
    ///
    /// Like `build`, editing it disturbs nothing; the pool moves when a pull
    /// finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactSpec>,
    /// Serve files from a directory on this host, with no backend at all — the
    /// third kind of deployment, alongside a managed VM pool and a `proxy_pass`
    /// upstream list. See [`SiteSpec`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<SiteSpec>,
    /// How a *static* deployment's backend is updated: a working directory on
    /// this host and commands to run in it. The static counterpart of `build`,
    /// and mutually exclusive with it for the same reason the backend kinds are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdateSpec>,
    /// An optional sign-in gate in front of everything this deployment serves.
    /// Applies to either backend kind — it runs in the proxy, before a backend
    /// is chosen — so the application behind it needs to know nothing about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthGate>,
}

/// What answers a request routed to a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// An autoscaled pool of microVMs.
    Vm,
    /// A fixed list of `proxy_pass` addresses.
    Upstreams,
    /// Files on this host, served by app-lb itself.
    Site,
}

/// A static *site*: a directory on this host, served straight off disk.
///
/// The third backend kind, and the one with no backend — app-lb answers the
/// request itself instead of proxying it. What nginx's `root` or a CloudFront
/// origin bucket does: files, an index, a 404, and cache headers. There is no
/// pool, nothing to scale, and nothing to health check.
///
/// Deliberately not configurable: rewrites, redirects, per-location blocks. A
/// site that needs those wants a real server behind a `proxy_pass` deployment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SiteSpec {
    /// Absolute path to the directory to serve. Nothing outside it is ever
    /// served, symlinks included — see `site::resolve`.
    pub root: String,
    /// Served for a request that names a directory. Set to `""` to answer those
    /// with a 404 instead of looking for an index.
    #[serde(default = "default_site_index")]
    pub index: String,
    /// Body for a 404, relative to `root`. Absent means a plain-text 404.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_found: Option<String>,
    /// Serve `index` (with a 200) for any path that matches no file, so a
    /// client-side router owns the URL space. The single-page-app switch; off
    /// by default because it turns every typo into a 200.
    #[serde(default)]
    pub spa: bool,
    /// `Cache-Control` for served files. The default is deliberately short:
    /// a wrong long max-age is not something you can take back, since the
    /// client will not ask again until it expires.
    #[serde(default = "default_site_cache_control")]
    pub cache_control: String,
}

fn default_site_index() -> String {
    "index.html".into()
}

/// Five minutes, and public. Long enough that a page's assets survive a reload,
/// short enough that a bad deploy is not pinned in caches for a day. Sites that
/// fingerprint their asset filenames should raise it.
fn default_site_cache_control() -> String {
    "public, max-age=300".into()
}

impl SiteSpec {
    fn validate(&self) -> Result<(), SpecError> {
        let root = self.root.trim();
        if root.is_empty() {
            return Err(SpecError::EmptySiteRoot);
        }
        // Absolute, because app-lb's working directory is not something a spec
        // author can see — a relative root would resolve differently depending
        // on how the process was started.
        if !std::path::Path::new(root).is_absolute() {
            return Err(SpecError::RelativeSiteRoot(root.to_string()));
        }
        for (field, value) in [("index", Some(&self.index)), ("not_found", self.not_found.as_ref())]
        {
            let Some(value) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) else {
                continue;
            };
            // These are joined onto the root, so a rooted or climbing path here
            // would read a file the site was never meant to expose.
            if value.starts_with('/') || value.split('/').any(|p| p == "..") {
                return Err(SpecError::BadSitePath {
                    field,
                    value: value.to_string(),
                });
            }
        }
        if self.spa && self.index.trim().is_empty() {
            return Err(SpecError::SpaWithoutIndex);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    EmptyId,
    /// A *static* deployment declared no routes. Managed deployments may:
    /// see [`DeploymentSpec::validate`].
    NoRoutes,
    EmptyRoute,
    /// A sign-in gate on a deployment with no routes. The gate only ever runs
    /// on a proxied request, and an unrouted deployment receives none.
    AuthWithoutRoutes,
    UnsupportedDriver(SandboxDriver),
    BadReplicaRange { min: u32, max: u32 },
    ZeroTargetConcurrency,
    ZeroPort,
    /// A `site` with no `root` to serve.
    EmptySiteRoot,
    /// A `site.root` that is not an absolute path.
    RelativeSiteRoot(String),
    /// `index` or `not_found` escapes the site root, or is rooted.
    BadSitePath {
        field: &'static str,
        value: String,
    },
    /// `spa` needs an `index` to fall back *to*.
    SpaWithoutIndex,
    /// A `site` alongside a `vm` or `upstreams`: a deployment serves from one
    /// place, and three ways to answer a request has no defined precedence.
    SiteWithOtherBackend,
    /// A `build`, `artifact` or `vm`-only block on a site.
    NotForSites(&'static str),
    /// Both a `vm` template and a static `upstreams` list were set.
    BothBackendKinds,
    /// Neither a `vm` template nor a static `upstreams` list was set.
    NoBackendKind,
    /// A static upstream address is not a valid `host:port`.
    BadUpstream(String),
    /// A static deployment declared a `build` block; there is no image to build.
    BuildOnStaticDeployment,
    /// A static deployment declared an `artifact` block; there is no image to
    /// pull one into.
    ArtifactOnStaticDeployment,
    /// Both `build` and `artifact` were set; both claim to produce `vm.image`.
    BothImageSources,
    /// A managed deployment declared an `update` block; its backend is a VM, not
    /// a process on this host.
    UpdateOnManagedDeployment,
    EmptyWorkingDir,
    BadWorkingDir(String),
    NoCommands,
    BadCommand(String),
    BadEnvName(String),
    ZeroTimeout,
    EmptyClientId,
    /// Neither an allowed domain nor an allowed address was given.
    EmptyAllowList,
    BadAllowedDomain(String),
    BadAllowedEmail(String),
    BadAuthBasePath(String),
    BadPublicPath(String),
    ZeroSessionTtl,
    BadCookieName(String),
    /// The provider's redirect would not route back to this deployment.
    AuthCallbackUnroutable(String),
    EmptyRepo,
    UnsupportedRepoUrl(String),
    BadBuildRef(String),
    BadBuildPath(String),
    BadImageName(String),
    EmptyArtifactStore,
    UnsupportedArtifactStore(String),
    BadArtifactRef(String),
    ZeroGrow,
    /// A secret reference somewhere in the spec is malformed. Carries the field
    /// it came from: four blocks can hold one, and "a secret ref is unusable"
    /// with no idea which is not an error message anyone can act on.
    BadSecretRef {
        field: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "deployment id must not be empty"),
            Self::EmptySiteRoot => write!(f, "site.root must name a directory to serve"),
            Self::RelativeSiteRoot(p) => write!(
                f,
                "site.root must be an absolute path, got {p:?} — app-lb's working \
                 directory is not something a spec can rely on"
            ),
            Self::BadSitePath { field, value } => write!(
                f,
                "site.{field} must be a path inside the site root, got {value:?}"
            ),
            Self::SpaWithoutIndex => {
                write!(f, "site.spa needs a site.index to fall back to")
            }
            Self::SiteWithOtherBackend => write!(
                f,
                "a deployment serves from exactly one place: `site` cannot be combined \
                 with `vm` or `upstreams`"
            ),
            Self::NotForSites(what) => write!(
                f,
                "`{what}` does not apply to a site — it serves files off disk, with no \
                 image and no pool"
            ),
            Self::NoRoutes => write!(
                f,
                "this deployment must declare at least one route; only a managed (vm) \
                 deployment may have none, and is then reachable only by exec/shell — \
                 a static deployment and a site are both reachable only through the proxy"
            ),
            Self::AuthWithoutRoutes => write!(
                f,
                "a deployment with no routes takes no HTTP traffic, so there is nothing \
                 for `auth` to gate"
            ),
            Self::EmptyRoute => {
                write!(
                    f,
                    "a route must set at least one of `host` or `path_prefix`"
                )
            }
            Self::UnsupportedDriver(d) => write!(
                f,
                "driver {d:?} is not supported: app-lb routes directly to the guest IP, \
                 which the daemon only exposes for tap-networked firecracker/kvm backends"
            ),
            Self::BadReplicaRange { min, max } => {
                write!(f, "min_replicas ({min}) exceeds max_replicas ({max})")
            }
            Self::ZeroTargetConcurrency => write!(f, "target_concurrency must be greater than 0"),
            Self::ZeroPort => write!(f, "vm.port must be greater than 0"),
            Self::BothBackendKinds => write!(
                f,
                "a deployment sets both `vm` and `upstreams`: pick one — a managed VM \
                 pool or a static proxy_pass upstream list, not both"
            ),
            Self::NoBackendKind => write!(
                f,
                "a deployment must set exactly one of `vm` (managed VM pool) or \
                 `upstreams` (static proxy_pass)"
            ),
            Self::BadUpstream(a) => write!(
                f,
                "static upstream {a:?} is not a valid `host:port` address"
            ),
            Self::BuildOnStaticDeployment => write!(
                f,
                "a static (proxy_pass) deployment cannot declare `build`: it has no guest \
                 image, it forwards to upstreams somebody else runs. Use `update` to run \
                 commands on the host instead"
            ),
            Self::ArtifactOnStaticDeployment => write!(
                f,
                "a static (proxy_pass) deployment cannot declare `artifact`: it has no guest \
                 image to pull a rootfs into, it forwards to upstreams somebody else runs"
            ),
            Self::BothImageSources => write!(
                f,
                "a deployment sets both `build` and `artifact`: pick one — both rewrite \
                 `vm.image` when they run, so with both there is no answer to where the \
                 running image came from. Build from git, or pull a rootfs somebody already \
                 built; to do both, build on one host and `serverctl artifact push` the result"
            ),
            Self::UpdateOnManagedDeployment => write!(
                f,
                "a managed (vm) deployment cannot declare `update`: its backends are microVMs, \
                 not processes on this host, so a working directory here would update nothing. \
                 Use `build` to rebuild its image instead"
            ),
            Self::EmptyWorkingDir => write!(f, "update.working_dir must not be empty"),
            Self::BadWorkingDir(d) => write!(
                f,
                "update.working_dir {d:?} must be an absolute path on the app-lb host, with \
                 no `..` components"
            ),
            Self::NoCommands => write!(
                f,
                "update.commands must list at least one command — a working directory with \
                 nothing to run in it updates nothing"
            ),
            Self::BadCommand(c) => write!(f, "update command {c:?} is empty or unusable"),
            Self::BadEnvName(n) => write!(
                f,
                "{n:?} is not a usable environment variable name: use [A-Za-z_][A-Za-z0-9_]*, \
                 or set `as` on the env_from entry"
            ),
            Self::ZeroTimeout => write!(
                f,
                "update.timeout_secs must be greater than 0; omit it to use the server default"
            ),
            Self::EmptyClientId => write!(f, "auth.client_id must not be empty"),
            Self::EmptyAllowList => write!(
                f,
                "auth sets neither `allowed_domains` nor `allowed_emails`, which would let \
                 in anyone with a Google account. Name the Workspace domain(s) or the \
                 address(es) — or set `\"allowed_domains\": [\"*\"]` if any Google account \
                 really is the intent"
            ),
            Self::BadAllowedDomain(d) => write!(
                f,
                "auth.allowed_domains entry {d:?} is not a domain: use a Workspace domain \
                 like \"example.com\", or \"*\" for any Google account"
            ),
            Self::BadAllowedEmail(e) => write!(
                f,
                "auth.allowed_emails entry {e:?} is not an email address"
            ),
            Self::BadAuthBasePath(p) => write!(
                f,
                "auth.base_path {p:?} must be an absolute path with at least one segment, \
                 e.g. \"/__applb/auth\""
            ),
            Self::BadPublicPath(p) => {
                write!(f, "auth.public_paths entry {p:?} must start with `/`")
            }
            Self::ZeroSessionTtl => write!(f, "auth.session_ttl_secs must be greater than 0"),
            Self::BadCookieName(c) => write!(
                f,
                "auth.cookie_name {c:?} is not a usable cookie name"
            ),
            Self::AuthCallbackUnroutable(c) => write!(
                f,
                "no route would match the sign-in callback {c:?}, so the provider's redirect \
                 would 404 and the login could never finish. Set `auth.base_path` under a \
                 path prefix this deployment serves"
            ),
            Self::EmptyRepo => write!(f, "build.repo must not be empty"),
            Self::UnsupportedRepoUrl(r) => write!(
                f,
                "build.repo {r:?} is not a supported remote: use https://, ssh://, \
                 user@host:path or an absolute path on this host"
            ),
            Self::BadBuildRef(r) => write!(
                f,
                "build.ref {r:?} is not a usable git ref: no whitespace, no leading `-`, \
                 and none of ~^:?*[\\'\""
            ),
            Self::BadBuildPath(p) => write!(
                f,
                "build path {p:?} must be relative to the checkout and must not climb out \
                 of it with `..`"
            ),
            Self::BadImageName(n) => write!(
                f,
                "build.image_name {n:?} has no usable characters: image names are \
                 [a-z0-9._-] and must start with a letter or digit"
            ),
            Self::EmptyArtifactStore => write!(f, "artifact.store must not be empty"),
            Self::UnsupportedArtifactStore(s) => write!(
                f,
                "artifact.store {s:?} is not a usable store: give either an `art serve` URL \
                 (http://host:port) or an absolute path to a store root on this host"
            ),
            Self::BadArtifactRef(r) => write!(
                f,
                "artifact.ref {r:?} is neither a tag nor a digest: a tag is [A-Za-z0-9._-] \
                 and may not start with `-` or `.`, and a digest is 64 lowercase hex characters"
            ),
            Self::ZeroGrow => write!(
                f,
                "artifact.grow_gb must be greater than 0; omit it to keep the image at its \
                 stored size"
            ),
            Self::BadSecretRef { field, detail } => {
                write!(f, "{field} is unusable: {detail}")
            }
        }
    }
}

impl std::error::Error for SpecError {}

impl DeploymentSpec {
    /// Which of the three backend kinds this deployment is.
    ///
    /// A valid spec sets exactly one of `vm`, `upstreams` and `site` (enforced
    /// by [`validate`](Self::validate)). Prefer matching on this over asking
    /// two questions: the code used to assume "static or managed" throughout,
    /// and a third kind added to that shape is a bug in every place the
    /// question was really "does this have a VM pool?".
    pub fn backend(&self) -> Backend {
        if self.site.is_some() {
            Backend::Site
        } else if !self.upstreams.is_empty() {
            Backend::Upstreams
        } else {
            Backend::Vm
        }
    }

    /// A static (proxy_pass) deployment forwards to fixed upstreams instead of a
    /// managed VM pool. **A site is not static in this sense** — it has no
    /// upstreams either — so this is not the predicate for "has no VM pool".
    pub fn is_static(&self) -> bool {
        self.backend() == Backend::Upstreams
    }

    /// Serves files off disk rather than proxying anywhere.
    pub fn is_site(&self) -> bool {
        self.backend() == Backend::Site
    }

    /// Owns an autoscaled VM pool — the only kind the autoscaler, the job
    /// system and the eviction API have anything to do with.
    pub fn is_managed(&self) -> bool {
        self.backend() == Backend::Vm
    }

    /// The VM template of a *managed* deployment. Panics if called on a static
    /// deployment — the VM-lifecycle code (autoscaler) only reaches this after
    /// confirming the deployment is managed, and `validate` guarantees a managed
    /// spec has a `vm`.
    pub fn vm_spec(&self) -> &VmSpec {
        self.vm
            .as_ref()
            .expect("vm_spec() on a static deployment; guard on is_static() first")
    }

    /// Rejects specs the data plane could not serve.
    ///
    /// A deployment is either *managed* (a `vm` template, autoscaled) or *static*
    /// (a fixed `upstreams` list, proxy_pass); exactly one must be set. For the
    /// managed kind the driver check is load-bearing: `SandboxInfo.guest_ip` is
    /// only populated for tap-networked Firecracker/KVM on a local daemon, so a
    /// Libvirt VM would boot fine and then be unroutable.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.id.trim().is_empty() {
            return Err(SpecError::EmptyId);
        }
        if self.routes.iter().any(RouteRule::is_empty) {
            return Err(SpecError::EmptyRoute);
        }
        // No routes means no ingress. For a *managed* deployment that is a
        // legitimate shape — an agent sandbox reached only through `exec` and
        // `shell`, never over HTTP — and it is the common one at fleet scale,
        // where exposure is the exception. Neither other kind has such a door:
        // a static deployment's backend is a fixed upstream list and a site is
        // files on disk, and for both the proxy is the only way in, so an
        // unrouted one is simply dead weight.
        if self.routes.is_empty() {
            if self.vm.is_none() {
                return Err(SpecError::NoRoutes);
            }
            // Nothing arrives over HTTP, so there is no request for a sign-in
            // gate to intercept and no browser to redirect. Rejected here
            // because `AuthGate::validate` would otherwise fail this on
            // callback routability, reporting a path the author never wrote.
            if self.auth.is_some() {
                return Err(SpecError::AuthWithoutRoutes);
            }
        }

        // Checked for either backend kind: the gate runs in the proxy, ahead of
        // whichever backend the deployment has.
        if let Some(auth) = &self.auth {
            auth.validate(&self.routes)?;
        }

        // Exactly one backend kind.
        if let Some(site) = &self.site {
            if self.vm.is_some() || !self.upstreams.is_empty() {
                return Err(SpecError::SiteWithOtherBackend);
            }
            site.validate()?;
            // A site has no image and no pool, so the blocks that produce or
            // scale one are meaningless rather than merely unused. Rejected so
            // a spec cannot claim something app-lb will silently ignore.
            for (present, what) in [
                (self.build.is_some(), "build"),
                (self.artifact.is_some(), "artifact"),
            ] {
                if present {
                    return Err(SpecError::NotForSites(what));
                }
            }
            // `update` *is* allowed: running `git pull && npm run build` in a
            // directory on this host is exactly how a site is redeployed.
            if let Some(update) = &self.update {
                update.validate()?;
            }
            return Ok(());
        }

        match (&self.vm, self.upstreams.is_empty()) {
            (Some(_), false) => return Err(SpecError::BothBackendKinds),
            (None, true) => return Err(SpecError::NoBackendKind),
            _ => {}
        }

        if let Some(vm) = &self.vm {
            // Managed: validate the VM template and scaling policy.
            if !matches!(vm.driver, SandboxDriver::Firecracker | SandboxDriver::Kvm) {
                return Err(SpecError::UnsupportedDriver(vm.driver));
            }
            if vm.port == 0 {
                return Err(SpecError::ZeroPort);
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
            // `build` and `artifact` are the two ways `vm.image` gets rewritten,
            // and each has its own explicit trigger. Two of them on one
            // deployment would make the running image depend on which job ran
            // last, which is not something the spec says anywhere.
            if self.build.is_some() && self.artifact.is_some() {
                return Err(SpecError::BothImageSources);
            }
            if let Some(build) = &self.build {
                build.validate()?;
            }
            if let Some(artifact) = &self.artifact {
                artifact.validate()?;
            }
            // An `update` runs commands in a directory on this host; a managed
            // deployment's backend is a microVM, so there is nothing here for
            // those commands to act on.
            if self.update.is_some() {
                return Err(SpecError::UpdateOnManagedDeployment);
            }
        } else {
            // A `build` produces a guest image, and a static deployment has no
            // guest. Rejecting it here beats accepting a spec whose build block
            // could never run.
            if self.build.is_some() {
                return Err(SpecError::BuildOnStaticDeployment);
            }
            // Same reasoning for a pull: there is no `vm.image` to point at the
            // rootfs it would materialize.
            if self.artifact.is_some() {
                return Err(SpecError::ArtifactOnStaticDeployment);
            }
            if let Some(update) = &self.update {
                update.validate()?;
            }
            // Static: every upstream must be a well-formed `host:port`. Actual
            // name resolution happens at request time (pingora) and per tick (the
            // health re-probe), so a temporarily-unresolvable name is not a
            // registration error — only a malformed address is.
            for addr in &self.upstreams {
                if !is_valid_host_port(addr) {
                    return Err(SpecError::BadUpstream(addr.clone()));
                }
            }
        }
        Ok(())
    }
}

/// Whether `s` is a syntactically valid `host:port` (or `ip:port`) upstream: a
/// non-empty host and a numeric port in `1..=65535`. Splits on the last colon so
/// IPv6 literals like `[::1]:8080` are handled; the host is not otherwise
/// resolved here.
fn is_valid_host_port(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    // Strip brackets from an IPv6 literal for the emptiness check.
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if host.is_empty() {
        return false;
    }
    matches!(port.parse::<u16>(), Ok(p) if p > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DeploymentSpec {
        DeploymentSpec {
            id: "demo".into(),
            routes: vec![RouteRule {
                host: Some("demo.local".into()),
                host_suffix: None,
                path_prefix: None,
            }],
            vm: Some(VmSpec {
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
                ttl_seconds: 3600,
            }),
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
            upstreams: vec![],
            build: None,
            artifact: None,
            site: None,
            update: None,
            auth: None,
        }
    }

    fn build_spec() -> BuildSpec {
        BuildSpec {
            repo: "https://github.com/acme/web.git".into(),
            git_ref: Some("main".into()),
            dockerfile: None,
            context: None,
            image_name: None,
            image_size_mb: None,
            auth: None,
        }
    }

    /// A static (proxy_pass) deployment: `upstreams` set, no `vm`.
    fn static_spec(upstreams: &[&str]) -> DeploymentSpec {
        DeploymentSpec {
            id: "proxy".into(),
            routes: vec![RouteRule {
                host: None,
                host_suffix: None,
                path_prefix: Some("/legacy".into()),
            }],
            vm: None,
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
            upstreams: upstreams.iter().map(|s| s.to_string()).collect(),
            build: None,
            artifact: None,
            site: None,
            update: None,
            auth: None,
        }
    }

    #[test]
    fn accepts_a_git_and_dockerfile_build_source() {
        let mut s = spec();
        s.build = Some(build_spec());
        assert_eq!(s.validate(), Ok(()));

        // scp-like and local remotes are accepted too.
        for repo in [
            "git@github.com:acme/web.git",
            "ssh://git@github.com/acme/web.git",
            "/srv/src/web",
        ] {
            let mut s = spec();
            s.build = Some(BuildSpec {
                repo: repo.into(),
                ..build_spec()
            });
            assert_eq!(s.validate(), Ok(()), "{repo} should be accepted");
        }
    }

    #[test]
    fn rejects_remotes_and_refs_that_git_would_read_as_flags_or_commands() {
        for repo in [
            "--upload-pack=/bin/sh",
            "ext::sh -c whoami",
            "https://",
            "https://host/a b",
            "",
        ] {
            let mut s = spec();
            s.build = Some(BuildSpec {
                repo: repo.into(),
                ..build_spec()
            });
            assert!(s.validate().is_err(), "{repo:?} should be rejected");
        }

        for git_ref in ["--upload-pack=x", "a b", "a..b", "/refs/heads/main", "v1^"] {
            let mut s = spec();
            s.build = Some(BuildSpec {
                git_ref: Some(git_ref.into()),
                ..build_spec()
            });
            assert_eq!(
                s.validate(),
                Err(SpecError::BadBuildRef(git_ref.to_string())),
                "{git_ref:?} should be rejected",
            );
        }
    }

    #[test]
    fn build_paths_cannot_escape_the_checkout() {
        for path in ["../../etc/passwd", "/etc/passwd", "a/../../b", "-f"] {
            let mut s = spec();
            s.build = Some(BuildSpec {
                dockerfile: Some(path.into()),
                ..build_spec()
            });
            assert_eq!(
                s.validate(),
                Err(SpecError::BadBuildPath(path.to_string())),
                "{path:?} should be rejected",
            );
        }
        // Ordinary in-tree paths are fine.
        let mut s = spec();
        s.build = Some(BuildSpec {
            dockerfile: Some("deploy/Dockerfile".into()),
            context: Some("./".into()),
            ..build_spec()
        });
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_static_deployment_cannot_declare_a_build() {
        let mut s = static_spec(&["10.0.0.9:8080"]);
        s.build = Some(build_spec());
        assert_eq!(s.validate(), Err(SpecError::BuildOnStaticDeployment));
    }

    fn update_spec() -> UpdateSpec {
        UpdateSpec {
            working_dir: "/srv/app-obs".into(),
            commands: vec![
                "git pull --ff-only".into(),
                "cargo build --release".into(),
                "supervisorctl restart app-obs".into(),
            ],
            env: None,
            env_from: vec![],
            auth: None,
            timeout_secs: None,
            verify_timeout_secs: None,
        }
    }

    #[test]
    fn accepts_a_working_directory_and_commands_on_a_static_deployment() {
        let mut s = static_spec(&["127.0.0.1:9600"]);
        s.update = Some(update_spec());
        assert_eq!(s.validate(), Ok(()));
    }

    /// The two update paths are exclusive, and each error has to point at the
    /// other one — landing on the wrong verb is the obvious mistake to make.
    #[test]
    fn each_kind_of_deployment_gets_exactly_one_update_path() {
        let mut managed = spec();
        managed.update = Some(update_spec());
        assert_eq!(managed.validate(), Err(SpecError::UpdateOnManagedDeployment));
        assert!(
            SpecError::UpdateOnManagedDeployment.to_string().contains("build"),
            "the error should name the path that does apply"
        );

        let mut static_dep = static_spec(&["10.0.0.9:8080"]);
        static_dep.build = Some(build_spec());
        assert!(
            SpecError::BuildOnStaticDeployment.to_string().contains("update"),
            "and likewise in the other direction"
        );
        assert_eq!(static_dep.validate(), Err(SpecError::BuildOnStaticDeployment));
    }

    #[test]
    fn a_working_directory_must_be_absolute_and_cannot_climb() {
        for dir in ["", "   "] {
            let mut s = static_spec(&["10.0.0.9:8080"]);
            s.update = Some(UpdateSpec {
                working_dir: dir.into(),
                ..update_spec()
            });
            assert_eq!(s.validate(), Err(SpecError::EmptyWorkingDir));
        }
        for dir in ["srv/app", "./app", "/srv/../../etc", "~/app"] {
            let mut s = static_spec(&["10.0.0.9:8080"]);
            s.update = Some(UpdateSpec {
                working_dir: dir.into(),
                ..update_spec()
            });
            assert_eq!(
                s.validate(),
                Err(SpecError::BadWorkingDir(dir.to_string())),
                "{dir:?} should be rejected",
            );
        }
    }

    #[test]
    fn an_update_needs_something_to_run() {
        let mut s = static_spec(&["10.0.0.9:8080"]);
        s.update = Some(UpdateSpec {
            commands: vec![],
            ..update_spec()
        });
        assert_eq!(s.validate(), Err(SpecError::NoCommands));

        let mut s = static_spec(&["10.0.0.9:8080"]);
        s.update = Some(UpdateSpec {
            commands: vec!["  ".into()],
            ..update_spec()
        });
        assert_eq!(s.validate(), Err(SpecError::BadCommand("  ".to_string())));
    }

    #[test]
    fn secret_env_defaults_to_the_upper_cased_key_and_must_be_a_usable_name() {
        let from = SecretEnv {
            secret: "obs".into(),
            key: "ingest_token".into(),
            env: None,
        };
        assert_eq!(from.env_name(), "INGEST_TOKEN");
        assert_eq!(from.secret_ref().key, "ingest_token");

        let renamed = SecretEnv {
            env: Some("APP_OBS_INGEST_TOKEN".into()),
            ..from.clone()
        };
        assert_eq!(renamed.env_name(), "APP_OBS_INGEST_TOKEN");

        // A name the shell cannot read back would be set and then invisible.
        let mut s = static_spec(&["10.0.0.9:8080"]);
        s.update = Some(UpdateSpec {
            env_from: vec![SecretEnv {
                env: Some("2FA-TOKEN".into()),
                ..from
            }],
            ..update_spec()
        });
        assert_eq!(
            s.validate(),
            Err(SpecError::BadEnvName("2FA-TOKEN".to_string()))
        );
    }

    #[test]
    fn verification_defaults_to_a_minute_and_can_be_switched_off() {
        assert_eq!(update_spec().verify_timeout().as_secs(), 60);
        let off = UpdateSpec {
            verify_timeout_secs: Some(0),
            ..update_spec()
        };
        assert!(off.verify_timeout().is_zero());

        // A zero *command* timeout is a mistake, not a switch.
        let mut s = static_spec(&["10.0.0.9:8080"]);
        s.update = Some(UpdateSpec {
            timeout_secs: Some(0),
            ..update_spec()
        });
        assert_eq!(s.validate(), Err(SpecError::ZeroTimeout));
    }

    #[test]
    fn an_update_block_round_trips_and_is_absent_when_unset() {
        let mut s = static_spec(&["127.0.0.1:9600"]);
        s.update = Some(UpdateSpec {
            env_from: vec![SecretEnv {
                secret: "obs".into(),
                key: "token".into(),
                env: Some("APP_OBS_TOKEN".into()),
            }],
            ..update_spec()
        });
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""working_dir":"/srv/app-obs""#), "{json}");
        assert!(json.contains(r#""as":"APP_OBS_TOKEN""#), "{json}");
        let parsed: DeploymentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.update, s.update);

        // An older persisted spec has no `update` key at all.
        let plain: DeploymentSpec =
            serde_json::from_str(&serde_json::to_string(&static_spec(&["10.0.0.9:80"])).unwrap())
                .unwrap();
        assert!(plain.update.is_none());
    }

    #[test]
    fn image_names_are_derived_from_the_commit_and_kept_docker_safe() {
        let b = build_spec();
        assert_eq!(
            b.image_for("web", "0123456789abcdef0123"),
            "web-0123456789ab",
            "the short sha is what makes each build a distinct image"
        );
        // The deployment id is only constrained by routing, so it is sanitized.
        assert_eq!(b.image_for("Web.API_v2", "abc"), "web.api_v2-abc");
        assert_eq!(b.image_for("--weird--", "abc"), "weird-abc");

        let named = BuildSpec {
            image_name: Some("Custom Name".into()),
            ..build_spec()
        };
        assert_eq!(named.image_for("web", "deadbeef"), "custom-name-deadbeef");

        let mut s = spec();
        s.build = Some(BuildSpec {
            image_name: Some("///".into()),
            ..build_spec()
        });
        assert_eq!(
            s.validate(),
            Err(SpecError::BadImageName("///".to_string()))
        );
    }

    #[test]
    fn a_build_credential_is_a_reference_not_a_value() {
        let mut s = spec();
        s.build = Some(BuildSpec {
            auth: Some(crate::secrets::SecretRef {
                secret: "github".into(),
                key: "token".into(),
                username: None,
            }),
            ..build_spec()
        });
        assert_eq!(s.validate(), Ok(()));
        // Round-tripping a spec must not require the value to exist.
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""auth":{"secret":"github","key":"token"}"#), "{json}");

        s.build = Some(BuildSpec {
            auth: Some(crate::secrets::SecretRef {
                secret: "../etc".into(),
                key: "token".into(),
                username: None,
            }),
            ..build_spec()
        });
        assert!(matches!(
            s.validate(),
            Err(SpecError::BadSecretRef { field: "build.auth", .. })
        ));
    }

    #[test]
    fn a_build_block_is_optional_and_absent_from_a_spec_that_has_none() {
        let json = serde_json::to_string(&spec()).unwrap();
        assert!(!json.contains("build"), "{json}");
        // And an old persisted spec (no `build` key) still parses.
        let parsed: DeploymentSpec = serde_json::from_str(&json).unwrap();
        assert!(parsed.build.is_none());
    }

    fn artifact_spec() -> ArtifactSpec {
        ArtifactSpec {
            store: "http://127.0.0.1:8080".into(),
            artifact_ref: "debian-hermes".into(),
            auth: None,
            grow_gb: None,
            image_name: None,
        }
    }

    #[test]
    fn an_artifact_store_is_a_url_or_an_absolute_path_and_nothing_else() {
        let ok = ["http://127.0.0.1:8080", "https://art.example.com", "/srv/artifacts"];
        for store in ok {
            let s = DeploymentSpec {
                artifact: Some(ArtifactSpec { store: store.into(), ..artifact_spec() }),
                ..spec()
            };
            assert!(s.validate().is_ok(), "{store} should be accepted");
        }

        // A relative root would resolve against app-lb's working directory,
        // which nobody writing a spec can see; the rest are not stores at all.
        for store in ["", ".artifacts", "art.example.com", "/srv/../etc", "file:///srv/art"] {
            let s = DeploymentSpec {
                artifact: Some(ArtifactSpec { store: store.into(), ..artifact_spec() }),
                ..spec()
            };
            assert!(s.validate().is_err(), "{store:?} should be refused");
        }
    }

    #[test]
    fn only_the_url_form_is_remote() {
        assert!(artifact_spec().is_remote());
        assert!(
            !ArtifactSpec { store: "/srv/artifacts".into(), ..artifact_spec() }.is_remote(),
            "a store root is materialized locally, not fetched"
        );
    }

    #[test]
    fn an_artifact_ref_is_a_tag_or_a_digest() {
        let digest = "c74abee2ce84".repeat(5) + "abcd";
        assert_eq!(digest.len(), 64);
        for r in ["debian-hermes", "ubuntu-24.04", "web_v2", digest.as_str()] {
            let s = DeploymentSpec {
                artifact: Some(ArtifactSpec { artifact_ref: r.into(), ..artifact_spec() }),
                ..spec()
            };
            assert!(s.validate().is_ok(), "{r} should be accepted");
        }
        // A leading `-` reads as a flag to anything that shells out, and a
        // slash or a `..` would leave the store when pasted into a URL path.
        for r in ["", "-flag", ".hidden", "a/b", "../etc/passwd", "has space"] {
            let s = DeploymentSpec {
                artifact: Some(ArtifactSpec { artifact_ref: r.into(), ..artifact_spec() }),
                ..spec()
            };
            assert!(
                matches!(s.validate(), Err(SpecError::BadArtifactRef(_))),
                "{r:?} should be refused"
            );
        }
    }

    #[test]
    fn a_pulled_image_is_named_after_the_digest_it_came_from() {
        let a = artifact_spec();
        assert_eq!(
            a.image_for("web", "c74abee2ce8409f1"),
            "web-c74abee2ce84",
            "the short digest is what makes a re-pull of the same bytes detectable by name",
        );
        let named = ArtifactSpec { image_name: Some("Custom Name".into()), ..artifact_spec() };
        assert_eq!(named.image_for("web", "deadbeefcafe0"), "custom-name-deadbeefcafe");
    }

    #[test]
    fn a_deployment_cannot_both_build_and_pull_its_image() {
        let s = DeploymentSpec {
            build: Some(build_spec()),
            artifact: Some(artifact_spec()),
            ..spec()
        };
        assert_eq!(s.validate(), Err(SpecError::BothImageSources));
    }

    #[test]
    fn a_static_deployment_has_no_image_to_pull_into() {
        let s = DeploymentSpec {
            artifact: Some(artifact_spec()),
            ..static_spec(&["127.0.0.1:9000"])
        };
        assert_eq!(s.validate(), Err(SpecError::ArtifactOnStaticDeployment));
    }

    #[test]
    fn growing_to_zero_is_refused_rather_than_silently_shrinking_nothing() {
        let s = DeploymentSpec {
            artifact: Some(ArtifactSpec { grow_gb: Some(0), ..artifact_spec() }),
            ..spec()
        };
        assert_eq!(s.validate(), Err(SpecError::ZeroGrow));
    }

    #[test]
    fn an_artifact_block_is_optional_and_absent_from_a_spec_that_has_none() {
        let json = serde_json::to_string(&spec()).unwrap();
        assert!(!json.contains("artifact"), "{json}");
        let parsed: DeploymentSpec = serde_json::from_str(&json).unwrap();
        assert!(parsed.artifact.is_none());
    }

    #[test]
    fn an_artifact_block_round_trips_with_ref_spelled_the_way_a_spec_spells_it() {
        let s = DeploymentSpec { artifact: Some(artifact_spec()), ..spec() };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""ref":"debian-hermes""#), "{json}");
        let parsed: DeploymentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.artifact, Some(artifact_spec()));
    }

    fn auth_gate() -> AuthGate {
        AuthGate {
            provider: AuthProvider::Google,
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: crate::secrets::SecretRef {
                secret: "google".into(),
                key: "client_secret".into(),
                username: None,
            },
            allowed_domains: vec!["example.com".into()],
            allowed_emails: vec![],
            public_paths: vec![],
            base_path: default_auth_base_path(),
            session_ttl_secs: default_session_ttl_secs(),
            cookie_name: default_auth_cookie_name(),
            redirect_url: None,
            forward_identity: true,
        }
    }

    #[test]
    fn a_gate_applies_to_either_backend_kind() {
        // The gate runs in the proxy, ahead of whichever backend the deployment
        // has, so both shapes accept one.
        let mut managed = spec();
        managed.auth = Some(auth_gate());
        assert_eq!(managed.validate(), Ok(()));

        // `static_spec` routes on a path prefix, so its gate has to put its
        // endpoints under that prefix for the callback to route back — which is
        // the check below, and the reason this is spelled out rather than
        // defaulted.
        let mut static_dep = static_spec(&["127.0.0.1:9600"]);
        static_dep.auth = Some(AuthGate {
            base_path: "/legacy/__auth".into(),
            ..auth_gate()
        });
        assert_eq!(static_dep.validate(), Ok(()));
    }

    /// Gating behind "has a Google account" is a real choice, and a very
    /// different one from what an empty list looks like it means.
    #[test]
    fn an_empty_allow_list_is_refused_and_the_escape_hatch_is_explicit() {
        let mut s = spec();
        s.auth = Some(AuthGate {
            allowed_domains: vec![],
            allowed_emails: vec![],
            ..auth_gate()
        });
        assert_eq!(s.validate(), Err(SpecError::EmptyAllowList));
        assert!(
            SpecError::EmptyAllowList.to_string().contains("\"*\""),
            "the error has to name the way to say what was meant"
        );

        s.auth = Some(AuthGate {
            allowed_domains: vec!["*".into()],
            allowed_emails: vec![],
            ..auth_gate()
        });
        assert_eq!(s.validate(), Ok(()));

        // An address alone is enough.
        s.auth = Some(AuthGate {
            allowed_domains: vec![],
            allowed_emails: vec!["someone@gmail.com".into()],
            ..auth_gate()
        });
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_gate_rejects_unusable_allow_list_entries() {
        for domain in ["", "not-a-domain", " example.com "] {
            let mut s = spec();
            s.auth = Some(AuthGate {
                allowed_domains: vec![domain.into()],
                ..auth_gate()
            });
            assert!(
                matches!(s.validate(), Err(SpecError::BadAllowedDomain(_))),
                "{domain:?} should be rejected",
            );
        }
        let mut s = spec();
        s.auth = Some(AuthGate {
            allowed_emails: vec!["not-an-address".into()],
            ..auth_gate()
        });
        assert!(matches!(s.validate(), Err(SpecError::BadAllowedEmail(_))));
    }

    /// Regression: a deployment routed only at `/app` would 404 the provider's
    /// redirect to `/__applb/auth/callback`, and the sign-in could never finish.
    #[test]
    fn the_callback_must_route_back_to_this_deployment() {
        let mut s = spec();
        s.routes = vec![RouteRule {
            host: None,
            host_suffix: None,
            path_prefix: Some("/app".into()),
        }];
        s.auth = Some(auth_gate());
        assert_eq!(
            s.validate(),
            Err(SpecError::AuthCallbackUnroutable(
                "/__applb/auth/callback".to_string()
            ))
        );

        // Putting the gate's endpoints inside the prefix fixes it.
        s.auth = Some(AuthGate {
            base_path: "/app/__auth".into(),
            ..auth_gate()
        });
        assert_eq!(s.validate(), Ok(()));

        // A host route matches any path, so the default is fine there.
        let mut hosted = spec();
        hosted.auth = Some(auth_gate());
        assert_eq!(hosted.validate(), Ok(()));
    }

    #[test]
    fn a_gate_rejects_degenerate_settings() {
        let cases: Vec<(AuthGate, SpecError)> = vec![
            (
                AuthGate { client_id: "  ".into(), ..auth_gate() },
                SpecError::EmptyClientId,
            ),
            (
                AuthGate { base_path: "relative".into(), ..auth_gate() },
                SpecError::BadAuthBasePath("relative".into()),
            ),
            (
                AuthGate { base_path: "/".into(), ..auth_gate() },
                SpecError::BadAuthBasePath("/".into()),
            ),
            (
                AuthGate { public_paths: vec!["healthz".into()], ..auth_gate() },
                SpecError::BadPublicPath("healthz".into()),
            ),
            (
                AuthGate { session_ttl_secs: 0, ..auth_gate() },
                SpecError::ZeroSessionTtl,
            ),
            (
                AuthGate { cookie_name: "bad name".into(), ..auth_gate() },
                SpecError::BadCookieName("bad name".into()),
            ),
        ];
        for (gate, want) in cases {
            let mut s = spec();
            s.auth = Some(gate);
            assert_eq!(s.validate(), Err(want));
        }
    }

    #[test]
    fn the_gates_endpoints_hang_off_its_base_path() {
        let g = AuthGate {
            base_path: "/app/__auth/".into(),
            ..auth_gate()
        };
        assert_eq!(g.callback_path(), "/app/__auth/callback");
        assert_eq!(g.login_path(), "/app/__auth/login");
        assert_eq!(g.logout_path(), "/app/__auth/logout");
    }

    #[test]
    fn public_paths_are_prefixes() {
        let g = AuthGate {
            public_paths: vec!["/healthz".into(), "/hooks/".into()],
            ..auth_gate()
        };
        assert!(g.is_public("/healthz"));
        assert!(g.is_public("/hooks/github"));
        assert!(!g.is_public("/hooks"), "the trailing slash was deliberate");
        assert!(!g.is_public("/"));
    }

    /// The fingerprint is what makes a session stop working when the policy
    /// changes, so it has to move for every input that decides who gets in —
    /// and stay put for everything else.
    #[test]
    fn the_policy_fingerprint_tracks_who_may_enter() {
        let base = auth_gate();
        let same_order = AuthGate {
            allowed_emails: vec!["b@x.com".into(), "a@x.com".into()],
            ..base.clone()
        };
        let other_order = AuthGate {
            allowed_emails: vec!["a@x.com".into(), "B@x.com".into()],
            ..base.clone()
        };
        assert_eq!(
            same_order.policy_fingerprint(),
            other_order.policy_fingerprint(),
            "order and case are not policy",
        );

        for changed in [
            AuthGate { client_id: "other".into(), ..base.clone() },
            AuthGate { allowed_domains: vec!["other.example".into()], ..base.clone() },
            AuthGate { allowed_emails: vec!["extra@x.com".into()], ..base.clone() },
        ] {
            assert_ne!(changed.policy_fingerprint(), base.policy_fingerprint());
        }

        // Cosmetic settings must not sign everybody out.
        let cosmetic = AuthGate {
            session_ttl_secs: 999,
            public_paths: vec!["/healthz".into()],
            forward_identity: false,
            ..base.clone()
        };
        assert_eq!(cosmetic.policy_fingerprint(), base.policy_fingerprint());
    }

    #[test]
    fn a_gate_round_trips_and_keeps_the_secret_a_reference() {
        let mut s = spec();
        s.auth = Some(auth_gate());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""provider":"google""#), "{json}");
        assert!(
            json.contains(r#""client_secret":{"secret":"google","key":"client_secret"}"#),
            "{json}"
        );
        let parsed: DeploymentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.auth, s.auth);

        // A spec written before the gate existed still parses.
        let plain: DeploymentSpec =
            serde_json::from_str(&serde_json::to_string(&spec()).unwrap()).unwrap();
        assert!(plain.auth.is_none());
    }

    /// The minimum a spec has to say: everything else defaults.
    #[test]
    fn a_gate_parses_from_just_a_client_id_secret_and_allow_list() {
        let gate: AuthGate = serde_json::from_str(
            r#"{"client_id":"cid","client_secret":{"secret":"google"},
                "allowed_domains":["example.com"]}"#,
        )
        .unwrap();
        assert_eq!(gate.provider, AuthProvider::Google);
        assert_eq!(gate.client_secret.key, "token", "the SecretRef default");
        assert_eq!(gate.base_path, "/__applb/auth");
        assert_eq!(gate.cookie_name, "applb_session");
        assert!(gate.forward_identity);
        assert_eq!(gate.session_ttl_secs, 43_200);
    }

    #[test]
    fn accepts_supported_drivers() {
        let mut s = spec();
        s.vm.as_mut().unwrap().driver = SandboxDriver::Firecracker;
        assert!(s.validate().is_ok());
        s.vm.as_mut().unwrap().driver = SandboxDriver::Kvm;
        assert!(s.validate().is_ok());
    }

    #[test]
    fn rejects_libvirt_because_it_has_no_guest_ip() {
        let mut s = spec();
        s.vm.as_mut().unwrap().driver = SandboxDriver::Libvirt;
        assert_eq!(
            s.validate(),
            Err(SpecError::UnsupportedDriver(SandboxDriver::Libvirt))
        );
    }

    #[test]
    fn accepts_a_static_upstream_spec() {
        assert!(!spec().is_static());
        let s = static_spec(&["10.0.0.9:8080", "backend.internal:8080", "[::1]:9000"]);
        assert!(s.is_static());
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn rejects_both_or_neither_backend_kind() {
        // Both `vm` and `upstreams`.
        let mut both = spec();
        both.upstreams = vec!["10.0.0.9:8080".into()];
        assert_eq!(both.validate(), Err(SpecError::BothBackendKinds));

        // Neither.
        let mut neither = spec();
        neither.vm = None;
        assert_eq!(neither.validate(), Err(SpecError::NoBackendKind));
    }

    #[test]
    fn rejects_malformed_upstreams() {
        for bad in ["no-port", "host:", ":8080", "host:0", "host:notaport"] {
            let s = static_spec(&[bad]);
            assert_eq!(
                s.validate(),
                Err(SpecError::BadUpstream(bad.to_string())),
                "{bad:?} should be rejected",
            );
        }
    }

    #[test]
    fn rejects_degenerate_specs() {
        let mut s = static_spec(&["127.0.0.1:9000"]);
        s.routes.clear();
        assert_eq!(s.validate(), Err(SpecError::NoRoutes));

        let mut s = spec();
        s.routes = vec![RouteRule::default()];
        assert_eq!(s.validate(), Err(SpecError::EmptyRoute));

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
        s.vm.as_mut().unwrap().port = 0;
        assert_eq!(s.validate(), Err(SpecError::ZeroPort));
    }

    /// `destroy` is the historical behaviour, so a spec written before
    /// `idle_action` existed must keep meaning what it meant.
    #[test]
    fn idle_action_defaults_to_destroy_and_round_trips() {
        let s: ScalingPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(s.idle_action, IdleAction::Destroy);

        let s: ScalingPolicy = serde_json::from_str(r#"{"idle_action":"retain"}"#).unwrap();
        assert_eq!(s.idle_action, IdleAction::Retain);

        let round = serde_json::to_string(&s).unwrap();
        assert!(round.contains(r#""idle_action":"retain""#), "got {round}");
    }

    mod sites {
        use super::*;

        fn site_spec() -> DeploymentSpec {
            let mut s = static_spec(&["127.0.0.1:9000"]);
            s.upstreams.clear();
            s.site = Some(SiteSpec {
                root: "/var/www/docs".into(),
                index: "index.html".into(),
                not_found: None,
                spa: false,
                cache_control: "public, max-age=300".into(),
            });
            s
        }

        #[test]
        fn a_site_is_its_own_backend_kind() {
            let s = site_spec();
            assert_eq!(s.validate(), Ok(()));
            assert_eq!(s.backend(), Backend::Site);
            assert!(s.is_site());
            // The distinction that matters everywhere else: a site is *not*
            // static, and code asking "does this have a VM pool?" must not get
            // its answer from `is_static`.
            assert!(!s.is_static());
            assert!(!s.is_managed());
        }

        #[test]
        fn a_site_cannot_also_be_something_else() {
            let mut s = site_spec();
            s.upstreams = vec!["127.0.0.1:9000".into()];
            assert_eq!(s.validate(), Err(SpecError::SiteWithOtherBackend));

            let mut s = site_spec();
            s.vm = spec().vm;
            assert_eq!(s.validate(), Err(SpecError::SiteWithOtherBackend));
        }

        /// A site has no image and no pool, so a block that produces one is a
        /// misunderstanding rather than a harmless extra.
        #[test]
        fn image_sources_are_refused_on_a_site() {
            let mut s = site_spec();
            s.build = Some(
                serde_json::from_str(r#"{"repo":"https://github.com/acme/site.git"}"#).unwrap(),
            );
            assert_eq!(s.validate(), Err(SpecError::NotForSites("build")));
        }

        /// `update` *is* allowed: `git pull && npm run build` in a directory on
        /// this host is exactly how a site is redeployed.
        #[test]
        fn an_update_block_is_how_a_site_is_deployed() {
            let mut s = site_spec();
            s.update = Some(
                serde_json::from_str(
                    r#"{"working_dir":"/srv/docs","commands":["git pull","npm run build"]}"#,
                )
                .unwrap(),
            );
            assert_eq!(s.validate(), Ok(()));
        }

        #[test]
        fn the_root_must_be_absolute_and_present() {
            let mut s = site_spec();
            s.site.as_mut().unwrap().root = "  ".into();
            assert_eq!(s.validate(), Err(SpecError::EmptySiteRoot));

            let mut s = site_spec();
            s.site.as_mut().unwrap().root = "www".into();
            assert_eq!(
                s.validate(),
                Err(SpecError::RelativeSiteRoot("www".into())),
                "a relative root would resolve against app-lb's working directory",
            );
        }

        /// `index` and `not_found` are joined onto the root, so a climbing or
        /// rooted value would read a file the site never meant to expose.
        #[test]
        fn the_index_and_404_page_cannot_escape_the_root() {
            for (field, value) in [("index", "../../etc/passwd"), ("not_found", "/etc/passwd")] {
                let mut s = site_spec();
                match field {
                    "index" => s.site.as_mut().unwrap().index = value.into(),
                    _ => s.site.as_mut().unwrap().not_found = Some(value.into()),
                }
                assert_eq!(
                    s.validate(),
                    Err(SpecError::BadSitePath {
                        field,
                        value: value.into()
                    }),
                );
            }
        }

        #[test]
        fn spa_mode_needs_an_index_to_fall_back_to() {
            let mut s = site_spec();
            s.site.as_mut().unwrap().spa = true;
            s.site.as_mut().unwrap().index = "".into();
            assert_eq!(s.validate(), Err(SpecError::SpaWithoutIndex));
        }

        /// A site is only reachable through the proxy, so an unrouted one is
        /// dead weight — the same reasoning as a static deployment.
        #[test]
        fn a_site_needs_a_route() {
            let mut s = site_spec();
            s.routes.clear();
            assert_eq!(s.validate(), Err(SpecError::NoRoutes));
        }

        #[test]
        fn the_defaults_are_what_a_build_output_wants() {
            let s: SiteSpec = serde_json::from_str(r#"{"root":"/var/www"}"#).unwrap();
            assert_eq!(s.index, "index.html");
            assert_eq!(s.cache_control, "public, max-age=300");
            assert!(!s.spa, "SPA mode turns every typo into a 200; opt in");
            assert_eq!(s.not_found, None);
        }
    }

    /// The agent-sandbox shape: a managed VM with no ingress at all, reached
    /// only through exec/shell. It is the *common* case at fleet scale, so it
    /// has to validate rather than being a special case someone works around.
    #[test]
    fn a_managed_deployment_may_have_no_routes() {
        let mut s = spec();
        s.routes.clear();
        assert_eq!(s.validate(), Ok(()));
    }

    /// The same shape is meaningless for a static deployment: the proxy is the
    /// only way in, so no route means nothing can ever reach it.
    #[test]
    fn a_static_deployment_may_not() {
        let mut s = static_spec(&["127.0.0.1:9000"]);
        s.routes.clear();
        assert_eq!(s.validate(), Err(SpecError::NoRoutes));
    }

    /// A gate runs on a proxied request. With no routes there are none, and the
    /// callback-routability check would otherwise reject this while talking
    /// about a path the author never wrote.
    #[test]
    fn a_gate_needs_something_to_gate() {
        let mut s = spec();
        s.routes.clear();
        s.auth = Some(auth_gate());
        assert_eq!(s.validate(), Err(SpecError::AuthWithoutRoutes));
    }

    #[test]
    fn empty_rule_matches_nothing() {
        assert!(!RouteRule::default().matches(Some("a.local"), "/"));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let r = RouteRule {
            host: Some("Demo.Local".into()),
            host_suffix: None,
            path_prefix: None,
        };
        assert!(r.matches(Some("demo.local"), "/"));
        assert!(r.matches(Some("DEMO.LOCAL"), "/"));
        assert!(!r.matches(Some("other.local"), "/"));
        assert!(!r.matches(None, "/"));
    }

    #[test]
    fn host_and_path_must_both_match() {
        let r = RouteRule {
            host: Some("demo.local".into()),
            host_suffix: None,
            path_prefix: Some("/api".into()),
        };
        assert!(r.matches(Some("demo.local"), "/api/v1"));
        assert!(!r.matches(Some("demo.local"), "/web"));
        assert!(!r.matches(Some("other.local"), "/api/v1"));
    }

    #[test]
    fn host_suffix_matches_apex_and_subdomains_at_a_label_boundary() {
        let r = RouteRule {
            host: None,
            host_suffix: Some("apps.example.com".into()),
            path_prefix: None,
        };
        // Apex and any depth of subdomain.
        assert!(r.matches(Some("apps.example.com"), "/"));
        assert!(r.matches(Some("a.apps.example.com"), "/"));
        assert!(r.matches(Some("x.y.apps.example.com"), "/"));
        // Case-insensitive.
        assert!(r.matches(Some("A.Apps.Example.Com"), "/"));
        // Boundary is anchored: a label that merely ends with the string is not
        // a subdomain of it.
        assert!(!r.matches(Some("notapps.example.com"), "/"));
        assert!(!r.matches(Some("example.com"), "/"));
        assert!(!r.matches(None, "/"));
    }

    #[test]
    fn host_suffix_accepts_a_leading_dot() {
        let r = RouteRule {
            host: None,
            host_suffix: Some(".example.com".into()),
            path_prefix: None,
        };
        assert!(r.matches(Some("a.example.com"), "/"));
        assert!(r.matches(Some("example.com"), "/"));
    }

    #[test]
    fn host_suffix_and_path_must_both_match() {
        let r = RouteRule {
            host: None,
            host_suffix: Some("example.com".into()),
            path_prefix: Some("/api".into()),
        };
        assert!(r.matches(Some("a.example.com"), "/api/v1"));
        assert!(!r.matches(Some("a.example.com"), "/web"));
        assert!(!r.matches(Some("a.other.com"), "/api/v1"));
    }

    #[test]
    fn exact_host_outranks_subdomain_outranks_path() {
        let exact = RouteRule {
            host: Some("a.example.com".into()),
            host_suffix: None,
            path_prefix: None,
        };
        let wild = RouteRule {
            host: None,
            host_suffix: Some("example.com".into()),
            path_prefix: None,
        };
        let long_wild = RouteRule {
            host: None,
            host_suffix: Some("apps.example.com".into()),
            path_prefix: None,
        };
        let path_only = RouteRule {
            host: None,
            host_suffix: None,
            path_prefix: Some("/some/long/path".into()),
        };
        assert!(exact.specificity() > long_wild.specificity());
        assert!(long_wild.specificity() > wild.specificity(), "longer suffix wins");
        assert!(wild.specificity() > path_only.specificity(), "subdomain beats path");
    }

    #[test]
    fn empty_and_dot_only_suffix_match_nothing() {
        assert!(RouteRule::default().is_empty());
        let dot = RouteRule {
            host: None,
            host_suffix: Some(".".into()),
            path_prefix: None,
        };
        assert!(!dot.is_empty(), "a suffix field is set");
        assert!(!dot.matches(Some("example.com"), "/"), "but it matches nothing");
    }

    #[test]
    fn host_outranks_path_and_longer_prefix_wins() {
        let host_only = RouteRule {
            host: Some("a".into()),
            host_suffix: None,
            path_prefix: None,
        };
        let long_path = RouteRule {
            host: None,
            host_suffix: None,
            path_prefix: Some("/a/very/long/prefix".into()),
        };
        let short_path = RouteRule {
            host: None,
            host_suffix: None,
            path_prefix: Some("/a".into()),
        };
        assert!(host_only.specificity() > long_path.specificity());
        assert!(long_path.specificity() > short_path.specificity());
    }
}
