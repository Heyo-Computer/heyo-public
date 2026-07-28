//! Deployment specs and LB configuration.

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
}

fn default_tls_addr() -> String {
    "0.0.0.0:6189".into()
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
            name: default_name(),
            daemon_url: None,
            dashboard_user: None,
            dashboard_password: None,
            admin_auth: false,
            tls_addr: default_tls_addr(),
            tls_cert_path: None,
            tls_key_path: None,
            acme_email: None,
            acme_dir: default_acme_dir(),
            acme_directory: default_acme_directory(),
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
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    EmptyId,
    NoRoutes,
    EmptyRoute,
    UnsupportedDriver(SandboxDriver),
    BadReplicaRange { min: u32, max: u32 },
    ZeroTargetConcurrency,
    ZeroPort,
    /// Both a `vm` template and a static `upstreams` list were set.
    BothBackendKinds,
    /// Neither a `vm` template nor a static `upstreams` list was set.
    NoBackendKind,
    /// A static upstream address is not a valid `host:port`.
    BadUpstream(String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => write!(f, "deployment id must not be empty"),
            Self::NoRoutes => write!(f, "deployment must declare at least one route"),
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
        }
    }
}

impl std::error::Error for SpecError {}

impl DeploymentSpec {
    /// A static (proxy_pass) deployment forwards to fixed upstreams instead of a
    /// managed VM pool. Determined by which backend field is populated; a valid
    /// spec sets exactly one (enforced by [`validate`](Self::validate)).
    pub fn is_static(&self) -> bool {
        !self.upstreams.is_empty()
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
        if self.routes.is_empty() {
            return Err(SpecError::NoRoutes);
        }
        if self.routes.iter().any(RouteRule::is_empty) {
            return Err(SpecError::EmptyRoute);
        }

        // Exactly one backend kind.
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
        } else {
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
        }
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
        let mut s = spec();
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
