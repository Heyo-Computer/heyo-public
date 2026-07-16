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

/// Process-level configuration, supplied via CLI/env rather than the admin API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LbConfig {
    #[serde(default = "default_proxy_addr")]
    pub proxy_addr: String,
    #[serde(default = "default_admin_addr")]
    pub admin_addr: String,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    /// heyvm daemon base URL. Defaults to `HeyoClient::local()`'s target.
    pub daemon_url: Option<String>,
}

impl Default for LbConfig {
    fn default() -> Self {
        Self {
            proxy_addr: default_proxy_addr(),
            admin_addr: default_admin_addr(),
            state_path: default_state_path(),
            daemon_url: None,
        }
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
    /// Path prefix match, e.g. `/api`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

impl RouteRule {
    pub fn is_empty(&self) -> bool {
        self.host.is_none() && self.path_prefix.is_none()
    }

    /// Longer/more-specific rules must win, so rules are ranked before matching.
    /// Host is a stronger signal than path, and a longer prefix beats a shorter.
    pub fn specificity(&self) -> usize {
        let host = if self.host.is_some() { 1_000 } else { 0 };
        host + self.path_prefix.as_ref().map_or(0, |p| p.len())
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
        if let Some(prefix) = &self.path_prefix
            && !path.starts_with(prefix.as_str())
        {
            return false;
        }
        true
    }
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
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    pub vm: VmSpec,
    #[serde(default)]
    pub scaling: ScalingPolicy,
    #[serde(default)]
    pub health: HealthCheck,
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
        }
    }
}

impl std::error::Error for SpecError {}

impl DeploymentSpec {
    /// Rejects specs the data plane could not serve.
    ///
    /// The driver check is the load-bearing one: `SandboxInfo.guest_ip` is only
    /// populated for tap-networked Firecracker/KVM on a local daemon, so a
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
        if !matches!(
            self.vm.driver,
            SandboxDriver::Firecracker | SandboxDriver::Kvm
        ) {
            return Err(SpecError::UnsupportedDriver(self.vm.driver));
        }
        if self.vm.port == 0 {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DeploymentSpec {
        DeploymentSpec {
            id: "demo".into(),
            routes: vec![RouteRule {
                host: Some("demo.local".into()),
                path_prefix: None,
            }],
            vm: VmSpec {
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
            },
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
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
        s.vm.port = 0;
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
            path_prefix: Some("/api".into()),
        };
        assert!(r.matches(Some("demo.local"), "/api/v1"));
        assert!(!r.matches(Some("demo.local"), "/web"));
        assert!(!r.matches(Some("other.local"), "/api/v1"));
    }

    #[test]
    fn host_outranks_path_and_longer_prefix_wins() {
        let host_only = RouteRule {
            host: Some("a".into()),
            path_prefix: None,
        };
        let long_path = RouteRule {
            host: None,
            path_prefix: Some("/a/very/long/prefix".into()),
        };
        let short_path = RouteRule {
            host: None,
            path_prefix: Some("/a".into()),
        };
        assert!(host_only.specificity() > long_path.specificity());
        assert!(long_path.specificity() > short_path.specificity());
    }
}
