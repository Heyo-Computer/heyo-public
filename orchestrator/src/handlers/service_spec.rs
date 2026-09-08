use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use crate::cloud_client::MountConfig;
use super::service_deploy::{ServiceDeployRequest, ServiceRevisionGuard, ServiceRouteRequest};

const APP_LB_HEALTH_TIMEOUT_SECS: u64 = 2;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpecRequest {
    pub id: String,
    pub user_id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub vm: VmSpec,
    #[serde(default)]
    pub routes: Vec<RouteSpec>,
    #[serde(default)]
    pub health: Option<HealthSpec>,
    #[serde(default)]
    pub scaling: Option<ScalingSpec>,
    pub deploy: DeploySpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSpec {
    pub driver: String,
    pub image: String,
    pub port: u16,
    #[serde(default)]
    pub open_ports: Vec<u16>,
    #[serde(default)]
    pub start_command: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub setup_hooks: Option<Vec<String>>,
    #[serde(default)]
    pub size_class: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env_from: Vec<SecretEnv>,
    #[serde(default)]
    pub mounts: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretEnv {
    pub secret: String,
    #[serde(default = "default_secret_key")]
    pub key: String,
    #[serde(default, rename = "as")]
    pub env: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub host_suffix: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub strip_prefix: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSpec {
    #[serde(default = "default_health_path")]
    pub path: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_probe_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScalingSpec {
    #[serde(default)]
    pub min_replicas: u32,
    #[serde(default = "default_max_replicas")]
    pub max_replicas: u32,
    #[serde(default)]
    pub warm_pool: Option<u32>,
    #[serde(default)]
    pub target_concurrency: Option<u32>,
    #[serde(default)]
    pub scale_to_zero_after_secs: Option<u64>,
    #[serde(default)]
    pub cold_start_timeout_secs: Option<u64>,
    #[serde(default)]
    pub drain_timeout_secs: Option<u64>,
    #[serde(default)]
    pub boot_timeout_secs: Option<u64>,
    #[serde(default)]
    pub idle_action: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploySpec {
    #[serde(default)]
    pub name: Option<String>,
    /// Host-specific bindings are rollout inputs, not app-lb artifact mounts.
    #[serde(default)]
    pub host_mounts: Vec<HostMount>,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default, rename = "async")]
    pub async_deploy: bool,
    #[serde(default)]
    pub archive_id: Option<String>,
    #[serde(default)]
    pub archive_name: Option<String>,
    #[serde(default)]
    pub archive_bytes_base64: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub placement_pool: Option<String>,
    #[serde(default)]
    pub replica_regions: Vec<String>,
    #[serde(default = "default_rollout_timeout")]
    pub health_timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub retire_previous: bool,
    #[serde(default)]
    pub retire_previous_async: bool,
    #[serde(default)]
    pub delete_previous: bool,
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub revision_guard: Option<RevisionGuard>,
    #[serde(default)]
    pub ingress: Option<IngressSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMount {
    pub host_path: String,
    pub sandbox_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressSpec {
    #[serde(default)] pub entry_points: Option<Vec<String>>,
    #[serde(default)] pub cert_resolver: Option<String>,
    #[serde(default)] pub priority: Option<u32>,
    #[serde(default)] pub pass_host_header: bool,
    #[serde(default)] pub backend_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionGuard {
    pub repository_url: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub expected_sha: String,
    #[serde(default)]
    pub force: bool,
}

impl ServiceSpecRequest {
    pub fn into_internal(self) -> Result<ServiceDeployRequest, String> {
        if self.id.trim().is_empty() || self.user_id.trim().is_empty() {
            return Err("id and user_id must not be empty".into());
        }
        if self.vm.port == 0 || self.vm.open_ports.contains(&0) {
            return Err("vm.port and vm.open_ports must be non-zero".into());
        }
        if !self.vm.mounts.is_empty() {
            return Err("vm.mounts are not supported by Orchestrator yet".into());
        }
        if self.routes.len() > 1 {
            return Err("at most one route is currently supported".into());
        }
        let route_spec = self.routes.into_iter().next();
        if let Some(route) = &route_spec {
            if route.host_suffix.is_some() {
                return Err("routes.host_suffix is not supported".into());
            }
            if route.host.as_ref().is_none_or(|host| host.trim().is_empty()) {
                return Err("routes.host is required by the Traefik renderer".into());
            }
        }
        let health = self.health.unwrap_or(HealthSpec {
            path: default_health_path(), port: None, timeout_secs: APP_LB_HEALTH_TIMEOUT_SECS,
        });
        let health_path = health.path.ok_or("TCP health checks are not supported")?;
        if health.port.is_some_and(|port| port != self.vm.port) {
            return Err("health.port must be omitted or equal vm.port".into());
        }
        if health.timeout_secs == 0 {
            return Err("health.timeout_secs must be positive".into());
        }
        let desired_replicas = match self.scaling {
            None => None,
            Some(scaling) => {
                if scaling.warm_pool.is_some() || scaling.target_concurrency.is_some()
                    || scaling.scale_to_zero_after_secs.is_some() || scaling.cold_start_timeout_secs.is_some()
                    || scaling.drain_timeout_secs.is_some() || scaling.boot_timeout_secs.is_some()
                    || scaling.idle_action.is_some() {
                    return Err("autoscaling knobs are not supported; only equal min_replicas and max_replicas are accepted".into());
                }
                if scaling.min_replicas != scaling.max_replicas {
                    return Err("scaling.min_replicas must equal scaling.max_replicas".into());
                }
                u16::try_from(scaling.min_replicas).map(Some)
                    .map_err(|_| "fixed replica count is too large".to_string())?
            }
        };
        let mut env = self.vm.env_vars.unwrap_or_default();
        for name in env.keys() {
            validate_env_name(name)?;
        }
        let mut env_refs = Vec::with_capacity(self.vm.env_from.len());
        let mut names = HashSet::new();
        for secret in self.vm.env_from {
            if secret.namespace.is_some() {
                return Err("vm.env_from.namespace is not accepted; secret references are scoped by secret/key".into());
            }
            validate_ref_part(&secret.secret, "secret")?;
            validate_ref_part(&secret.key, "key")?;
            let name = secret.env.unwrap_or_else(|| secret.key.to_ascii_uppercase());
            validate_env_name(&name)?;
            if !names.insert(name.clone()) {
                return Err(format!("duplicate environment variable {name} in env_from"));
            }
            // app-lb and the existing HeyoSecret resolver give secrets precedence
            // over literals. Do not carry the superseded literal into run state.
            env.remove(&name);
            env_refs.push(format!("{name}=heyosecret://{}/{}@active", secret.secret, secret.key));
        }
        let ingress = self.deploy.ingress;
        let route = route_spec.map(|route| ServiceRouteRequest {
            host: route.host.unwrap(), path_prefix: route.path_prefix,
            backend_url: ingress.as_ref().and_then(|v| v.backend_url.clone()),
            entry_points: ingress.as_ref().and_then(|v| v.entry_points.clone()),
            cert_resolver: ingress.as_ref().and_then(|v| v.cert_resolver.clone()),
            priority: ingress.as_ref().and_then(|v| v.priority),
            strip_prefix: route.strip_prefix,
            pass_host_header: ingress.as_ref().is_some_and(|v| v.pass_host_header),
        });
        if route.is_none() && ingress.is_some() {
            return Err("deploy.ingress requires one route".into());
        }
        let mut ports = vec![self.vm.port];
        for port in self.vm.open_ports {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
        let mounts = self.deploy.host_mounts.into_iter().map(|mount| {
            for path in [&mount.host_path, &mount.sandbox_path] {
                if !path.starts_with('/') || path.contains('\0')
                    || path.split('/').any(|component| component == "..") {
                    return Err("deploy.host_mounts paths must be absolute and contain no traversal".to_string());
                }
            }
            Ok(MountConfig {
                host_path: mount.host_path, sandbox_path: mount.sandbox_path,
                read_only: mount.read_only,
            })
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(ServiceDeployRequest {
            service_id: self.id, user_id: self.user_id, account_id: self.account_id,
            async_deploy: self.deploy.async_deploy, deployment_id: self.deploy.deployment_id,
            name: self.deploy.name, archive_id: self.deploy.archive_id,
            archive_name: self.deploy.archive_name, archive_bytes_base64: self.deploy.archive_bytes_base64,
            region: self.deploy.region, placement_pool: self.deploy.placement_pool,
            driver: self.vm.driver, image: self.vm.image, ports, port_mappings: vec![], mounts,
            env: Some(env), env_refs, start_command: self.vm.start_command,
            working_directory: self.vm.working_directory, setup_hooks: self.vm.setup_hooks,
            size_class: self.vm.size_class.unwrap_or_else(|| "small".into()), ttl_seconds: self.vm.ttl_seconds,
            health_path, health_timeout_seconds: self.deploy.health_timeout_seconds,
            health_probe_timeout_seconds: health.timeout_secs,
            desired_replicas, replica_regions: self.deploy.replica_regions,
            retire_previous: self.deploy.retire_previous,
            retire_previous_async: self.deploy.retire_previous_async,
            delete_previous: self.deploy.delete_previous, drain_seconds: self.deploy.drain_seconds,
            route, metadata: self.deploy.metadata,
            revision_guard: self.deploy.revision_guard.map(|guard| ServiceRevisionGuard {
                repository_url: guard.repository_url, git_ref: guard.git_ref,
                expected_sha: guard.expected_sha, force: guard.force,
            }),
        })
    }
}

fn validate_ref_part(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() || value == "." || value == ".." || value.contains('/')
        || !value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        return Err(format!("vm.env_from.{field} is invalid"));
    }
    Ok(())
}

fn validate_env_name(value: &str) -> Result<(), String> {
    let mut chars = value.bytes();
    if !chars.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        || !chars.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("invalid environment variable name {value}"));
    }
    Ok(())
}

fn default_secret_key() -> String { "token".into() }
fn default_health_path() -> Option<String> { Some("/".into()) }
fn default_probe_timeout() -> u64 { APP_LB_HEALTH_TIMEOUT_SECS }
fn default_max_replicas() -> u32 { 5 }
fn default_region() -> String { "local".into() }
fn default_rollout_timeout() -> u64 { 180 }
fn default_drain_seconds() -> u64 { 10 }
fn default_true() -> bool { true }

#[cfg(test)]
mod tests {
    use super::ServiceSpecRequest;
    use serde_json::json;

    fn minimal() -> serde_json::Value {
        json!({"id":"cloud","user_id":"system","vm":{"driver":"libvirt","image":"ubuntu","port":4447},"deploy":{}})
    }

    #[test]
    fn converts_canonical_shape_and_defaults() {
        let request: ServiceSpecRequest = serde_json::from_value(minimal()).unwrap();
        let internal = request.into_internal().unwrap();
        assert_eq!(internal.service_id, "cloud");
        assert_eq!(internal.ports, [4447]);
        assert_eq!(internal.health_path, "/");
        assert_eq!(internal.size_class, "small");
        assert_eq!(internal.desired_replicas, None);
        assert!(!internal.async_deploy);
        assert!(internal.retire_previous);
    }

    #[test]
    fn preserves_route_rollout_archive_and_replica_metadata() {
        let mut value = minimal();
        value["routes"] = json!([{"host":"cloud.example.com","path_prefix":"/api"}]);
        value["scaling"] = json!({"min_replicas":2,"max_replicas":2});
        value["deploy"] = json!({"archive_id":"arc","replica_regions":["EU","US"],"metadata":{"revision":"abc"},"ingress":{"entry_points":["websecure"],"priority":7}});
        let internal = serde_json::from_value::<ServiceSpecRequest>(value).unwrap().into_internal().unwrap();
        assert_eq!(internal.archive_id.as_deref(), Some("arc"));
        assert_eq!(internal.desired_replicas, Some(2));
        assert_eq!(internal.replica_regions, ["EU", "US"]);
        assert_eq!(internal.metadata.unwrap()["revision"], "abc");
        assert_eq!(internal.route.unwrap().priority, Some(7));
    }

    #[test]
    fn secret_refs_override_literals_and_duplicate_refs_fail() {
        let mut value = minimal();
        value["vm"]["env_from"] = json!([{"secret":"cicd","key":"token","as":"TOKEN"}]);
        let internal = serde_json::from_value::<ServiceSpecRequest>(value.clone()).unwrap().into_internal().unwrap();
        assert_eq!(internal.env_refs, ["TOKEN=heyosecret://cicd/token@active"]);
        assert!(internal.env.unwrap().is_empty());
        value["vm"]["env_vars"] = json!({"TOKEN":"plain"});
        let internal = serde_json::from_value::<ServiceSpecRequest>(value.clone()).unwrap().into_internal().unwrap();
        assert!(!internal.env.unwrap().contains_key("TOKEN"));
        value["vm"]["env_from"] = json!([
            {"secret":"cicd","key":"token","as":"TOKEN"},
            {"secret":"other","key":"token","as":"TOKEN"}
        ]);
        assert!(serde_json::from_value::<ServiceSpecRequest>(value).unwrap().into_internal().is_err());
    }

    #[test]
    fn rejects_old_unknown_and_unsupported_shapes() {
        assert!(serde_json::from_value::<ServiceSpecRequest>(json!({"serviceId":"cloud","userId":"system"})).is_err());
        let mut value = minimal();
        value["unexpected"] = json!(true);
        assert!(serde_json::from_value::<ServiceSpecRequest>(value).is_err());
        let mut value = minimal();
        value["vm"]["mounts"] = json!([{"path":"/data"}]);
        assert!(serde_json::from_value::<ServiceSpecRequest>(value).unwrap().into_internal().is_err());
    }

    #[test]
    fn validates_fixed_scaling_and_health_subset() {
        let mut value = minimal();
        value["scaling"] = json!({"min_replicas":1,"max_replicas":2});
        assert!(serde_json::from_value::<ServiceSpecRequest>(value).unwrap().into_internal().is_err());
        let mut value = minimal();
        value["health"] = json!({"path":null});
        assert!(serde_json::from_value::<ServiceSpecRequest>(value).unwrap().into_internal().is_err());
    }

    #[test]
    fn preserves_primary_port_probe_timeout_and_host_mounts() {
        let mut value = minimal();
        value["vm"]["open_ports"] = json!([80, 4447, 9000]);
        value["health"] = json!({"path":"/health", "timeout_secs":5});
        value["deploy"]["host_mounts"] = json!([{
            "host_path":"/var/lib/heyo-cicd/runs",
            "sandbox_path":"/var/lib/heyo-cicd/runs", "read_only":false
        }]);
        let internal = serde_json::from_value::<ServiceSpecRequest>(value.clone()).unwrap().into_internal().unwrap();
        assert_eq!(internal.ports, [4447, 80, 9000]);
        assert_eq!(internal.health_probe_timeout_seconds, 5);
        assert_eq!(internal.health_timeout_seconds, 180);
        assert_eq!(internal.mounts[0].host_path, "/var/lib/heyo-cicd/runs");
        assert!(!internal.mounts[0].read_only);
        value["deploy"]["host_mounts"][0]["host_path"] = json!("/data/../secret");
        assert!(serde_json::from_value::<ServiceSpecRequest>(value).unwrap().into_internal().is_err());
    }

    /// Integration with the offline workflow harness in both repositories.
    #[test]
    fn accepts_generated_workflow_payloads_when_supplied() {
        let Some(dir) = std::env::var_os("SERVICE_SPEC_FIXTURE_DIR") else { return };
        let mut count = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|extension| extension != "json") { continue; }
            let text = std::fs::read_to_string(&path).unwrap();
            let spec: ServiceSpecRequest = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let request = spec.into_internal()
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert!(request.async_deploy);
            assert_eq!(request.health_probe_timeout_seconds, 5);
            if request.service_id == "cicd" {
                assert_eq!(request.mounts.len(), 1);
            }
            count += 1;
        }
        assert!(count >= 7, "provide all seven service workflow payloads");
    }
}
