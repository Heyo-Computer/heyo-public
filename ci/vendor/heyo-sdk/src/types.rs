//! Public types. Field names mirror the cloud HTTP API (snake_case both on
//! the wire and in Rust), so the SDK stays a thin wrapper rather than a
//! parallel domain model.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize a value that may be explicitly `null` on the wire into `T`'s
/// `Default`. `#[serde(default)]` only covers a *missing* key — an explicit
/// `null` still reaches the target type's deserializer and fails for types
/// like `Vec<_>` ("invalid type: null, expected a sequence"). The heyvm daemon
/// emits `"urls": null` for sandboxes with no bound URLs, so any bare
/// (non-`Option`) collection field needs this.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// `US` or `EU`. The wire field is uppercase; we preserve that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxRegion {
    US,
    EU,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxDriver {
    Libvirt,
    Firecracker,
    Kvm,
    /// Firecracker with a containerd-managed OCI rootfs: `image` is a
    /// standard registry reference (`node:22`, `ghcr.io/org/image:tag`).
    #[serde(rename = "firecracker_containerd")]
    FirecrackerContainerd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxSize {
    Micro,
    Mini,
    Small,
    Medium,
    Large,
    Xlarge,
}

impl SandboxSize {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxSize::Micro => "micro",
            SandboxSize::Mini => "mini",
            SandboxSize::Small => "small",
            SandboxSize::Medium => "medium",
            SandboxSize::Large => "large",
            SandboxSize::Xlarge => "xlarge",
        }
    }
}

/// Lifecycle states the cloud reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxStatus {
    Provisioning,
    Running,
    Stopped,
    Paused,
    Failed,
    #[serde(rename = "cold-stored")]
    ColdStored,
    #[serde(other)]
    Unknown,
}

/// Options accepted by `Sandbox::create`. All fields except `image`/`region`
/// may be left unset; the server applies defaults.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SandboxCreateOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "archive_id")]
    pub archive_id: Option<String>,
    /// Defaults to `US` if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<SandboxRegion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<SandboxDriver>,
    /// Image identifier (e.g. `ubuntu:24.04`, `bun`, `pi-…`). Defaults to
    /// `ubuntu:24.04` if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "start_command")]
    pub start_command: Option<String>,
    #[serde(default, rename = "open_ports", skip_serializing_if = "Vec::is_empty")]
    pub open_ports: Vec<u16>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "ttl_seconds")]
    pub ttl_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "disk_size_gb")]
    pub disk_size_gb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "working_directory")]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "env_vars")]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "setup_hooks")]
    pub setup_hooks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "size_class")]
    pub size_class: Option<SandboxSize>,
    /// Maximum time `create()` will wait for the sandbox to leave the
    /// `provisioning` state. `None` ⇒ default 5 minutes. `Some(Duration::ZERO)`
    /// ⇒ return immediately while it's still provisioning.
    #[serde(skip_serializing)]
    pub wait_for_ready: Option<std::time::Duration>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub status: SandboxStatus,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub start_command: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub size_class: Option<String>,
    #[serde(default)]
    pub disk_size_gb: Option<u64>,
    #[serde(default)]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(default)]
    pub setup_hooks: Option<Vec<String>>,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub is_deployed: bool,
    #[serde(default)]
    pub error_message: Option<String>,
    pub status_changed_at: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub urls: Vec<BoundUrl>,
    /// Direct host-reachable guest IP for tap-networked backends (Firecracker/
    /// KVM on the local daemon). Present when a client shares the host with the
    /// VM and can dial it directly (e.g. `<guest_ip>:5432`) instead of opening a
    /// P2P tunnel. `None` for cloud/remote sandboxes or non-tap backends.
    #[serde(default)]
    pub guest_ip: Option<String>,
    /// Free-form JSON tags set by the server (e.g. `{"project_id": "..."}`).
    /// Mirrors the `metadata` column on `deployed_sandbox`.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// The account the sandbox is metered to. Reported by a local daemon's
    /// listing; absent from a cloud listing, which is already per account.
    #[serde(default)]
    pub account_id: Option<String>,
    /// RFC 3339, from a local daemon's listing.
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub cpus: Option<u32>,
    /// Bytes.
    #[serde(default)]
    pub memory: Option<u64>,
    /// `firecracker`, `kvm`, … as the daemon names its driver.
    #[serde(default)]
    pub backend_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicImage {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub backend_type: Option<String>,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BoundUrl {
    pub subdomain: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub url: String,
    pub port: u16,
    #[serde(default, rename = "is_public")]
    pub is_public: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CommandRunOptions {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Combined stdout+stderr as the backend reports it.
    pub output: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The local heyvm daemon emits `"urls": null` (not `[]`, not absent) for a
    /// sandbox with no bound URLs. Regression: `#[serde(default)]` alone failed
    /// with "invalid type: null, expected a sequence".
    #[test]
    fn sandbox_info_tolerates_null_urls() {
        let json = r#"{
            "id":"sb-e8502fbe","name":"factory","image":"agents-v1",
            "status":"stopped","region":null,"setup_hooks":null,
            "env_vars":null,"start_command":null,"working_directory":null,
            "ttl_seconds":3600,"uptime_secs":7369,"is_deployed":false,
            "status_changed_at":"2026-06-30T21:38:42Z","urls":null
        }"#;
        let info: SandboxInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "factory");
        assert_eq!(info.status, SandboxStatus::Stopped);
        assert!(info.urls.is_empty());
    }

    /// The cloud validates the driver string exactly; `rename_all =
    /// "lowercase"` would have produced `firecrackercontainerd`.
    #[test]
    fn driver_wire_names_match_the_cloud() {
        for (driver, wire) in [
            (SandboxDriver::Libvirt, "\"libvirt\""),
            (SandboxDriver::Firecracker, "\"firecracker\""),
            (SandboxDriver::Kvm, "\"kvm\""),
            (SandboxDriver::FirecrackerContainerd, "\"firecracker_containerd\""),
        ] {
            assert_eq!(serde_json::to_string(&driver).unwrap(), wire);
            assert_eq!(serde_json::from_str::<SandboxDriver>(wire).unwrap(), driver);
        }
    }
}
