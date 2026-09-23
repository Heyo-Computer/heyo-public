//! Wire types for pg-vm-pool's JSON admin API.
//!
//! pg-vm-pool serves these from its dashboard listener (behind the dashboard's
//! Basic auth); app-lb's pg-fc plugin reads them. Both sides depend on this
//! crate, so a field added here is a field both sides see — which is the point
//! of it existing rather than each side re-declaring the shapes.
//!
//! Deliberately serde-only. Nothing here knows about axum, tokio or the
//! pooler's internals, so a client pays for exactly these types.
//!
//! Every response struct derives both `Serialize` and `Deserialize`, and every
//! optional field is `#[serde(default)]`, so an older client keeps reading a
//! newer server and the reverse.

use serde::{Deserialize, Serialize};

/// Error body for every non-2xx JSON response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// ---- health --------------------------------------------------------------

/// `GET /api/health` — cheap, and safe to poll: no daemon walk, no guest access.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// `pg-vm-pool` package version.
    pub version: String,
    pub uptime_secs: u64,
    /// The Postgres wire listener clients connect to (`PG_VM_POOL_LISTEN`).
    pub listen: String,
    /// Schemas with a warm (checked-out-able) VM right now.
    pub warm_schemas: usize,
    /// Every schema the pooler has a durable record for.
    pub known_schemas: usize,
    /// Which offload tiers are configured, cheapest first.
    #[serde(default)]
    pub tiers: Vec<String>,
    pub tls: bool,
    pub replication: bool,
}

// ---- schemas -------------------------------------------------------------

/// One schema (a client-facing database name) the pooler knows about.
///
/// A schema is live on a VM, or offloaded to one of the cheaper tiers; the
/// warm-entry fields are present only while a VM is checked into the pooler.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchemaInfo {
    pub schema: String,
    /// `live`, `compacted`, `frozen` or `archived`; `pending` for a
    /// dedicated database whose VM has never been brought up.
    pub tier: String,
    /// The VM that backs it, or last backed it. After an offload that VM no
    /// longer exists.
    #[serde(default)]
    pub sandbox_id: Option<String>,
    /// Unix seconds of the last client checkout.
    pub last_active: u64,
    /// Size of the data device in GiB, when known.
    #[serde(default)]
    pub disk_gb: Option<u32>,
    /// A warm VM is checked into the pooler for it right now.
    pub warm: bool,
    /// Live client sessions (warm only).
    #[serde(default)]
    pub sessions: Option<usize>,
    /// Client slots `(free, total)` on its Postgres (warm only).
    #[serde(default)]
    pub free_slots: Option<usize>,
    #[serde(default)]
    pub slot_limit: Option<usize>,
    #[serde(default)]
    pub idle_secs: Option<u64>,
    /// The idle timeout that applies to this VM (warm only; `null` when idle
    /// reaping is off).
    #[serde(default)]
    pub idle_budget_secs: Option<u64>,
    #[serde(default)]
    pub bringup_ms: Option<u64>,
    pub keepalive: bool,
    /// A dedicated database: bound to its own role and password.
    pub dedicated: bool,
    /// Why this schema must not be stopped or offloaded (a replication
    /// pairing depends on it), if it is pinned.
    #[serde(default)]
    pub pinned: Option<String>,
    /// An offload of this schema is in flight.
    pub offloading: bool,
}

/// `GET /api/schemas/{schema}` — one schema, plus live stats read over the
/// pooler's warm Postgres pool when it is warm.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchemaDetail {
    #[serde(flatten)]
    pub info: SchemaInfo,
    #[serde(default)]
    pub db_size_bytes: Option<i64>,
    #[serde(default)]
    pub backends: Option<i32>,
}

/// What `POST /api/schemas/{schema}/{action}` can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaAction {
    Start,
    Stop,
    Reboot,
    /// Needs a `size_class` body.
    Resize,
    /// Dump to S3 and delete the VM.
    Reap,
    /// Bring an offloaded schema back onto a VM.
    Restore,
    /// Archive the raw disk image to S3, no boot.
    ArchiveImage,
}

impl SchemaAction {
    pub fn as_str(self) -> &'static str {
        match self {
            SchemaAction::Start => "start",
            SchemaAction::Stop => "stop",
            SchemaAction::Reboot => "reboot",
            SchemaAction::Resize => "resize",
            SchemaAction::Reap => "reap",
            SchemaAction::Restore => "restore",
            SchemaAction::ArchiveImage => "archive-image",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "start" => SchemaAction::Start,
            "stop" => SchemaAction::Stop,
            "reboot" => SchemaAction::Reboot,
            "resize" => SchemaAction::Resize,
            "reap" => SchemaAction::Reap,
            "restore" => SchemaAction::Restore,
            "archive-image" => SchemaAction::ArchiveImage,
            _ => return None,
        })
    }
}

/// Body for `POST /api/schemas/{schema}/resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeRequest {
    /// `micro`, `mini`, `small`, `medium` or `large`.
    pub size_class: String,
}

/// Response to an action or maintenance request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    /// True when the work finished before the response; false when it was
    /// started in the background (its outcome lands in `/api/events`).
    pub done: bool,
    pub message: String,
}

// ---- host ----------------------------------------------------------------

/// `GET /api/host` — whole-machine health, each part best-effort.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    #[serde(default)]
    pub cpu_percent: Option<f32>,
    #[serde(default)]
    pub cpu_count: Option<u32>,
    #[serde(default)]
    pub memory_total_bytes: Option<u64>,
    #[serde(default)]
    pub memory_used_bytes: Option<u64>,
    #[serde(default)]
    pub disks: Vec<HostDisk>,
    /// Warm-spare shelf `(ready, target)`, when the spare pool is on.
    #[serde(default)]
    pub spares_ready: Option<usize>,
    #[serde(default)]
    pub spares_target: Option<usize>,
    /// Schema counts by tier.
    #[serde(default)]
    pub tiers: TierCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierCounts {
    pub live: usize,
    pub compacted: usize,
    pub frozen: usize,
    pub archived: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDisk {
    pub source: String,
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

// ---- events and logs ------------------------------------------------------

/// One `GET /api/events` row, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEntry {
    pub t: u64,
    /// `info` or `error`.
    pub level: String,
    pub kind: String,
    pub msg: String,
}

/// `GET /api/logs/{source}` — the tail of a log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogTail {
    /// Where the lines came from (a path, or `<vm>:<path>`).
    pub source: String,
    pub lines: Vec<String>,
}

// ---- runtime configuration ------------------------------------------------

/// The configuration knobs that can change without a restart.
///
/// Every field is optional in a `PUT`: absent means "leave it". In a `GET` the
/// effective value is always present for a knob whose subsystem is on; `null`
/// means the subsystem is off (and was off at boot, so it cannot be switched
/// on from here).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeKnobs {
    /// Stop a VM this long after its last connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// The shorter timeout for VMs that were cheap to bring back. `0` turns
    /// the two-speed behaviour off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_fast_secs: Option<u64>,
    /// Pre-booted spare VMs to keep on the shelf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_spares: Option<usize>,
    /// Offload-ladder thresholds: idle this long before compacting, freezing
    /// to a local dump, or archiving to S3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_after_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze_after_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_after_secs: Option<u64>,
}

/// Where a knob's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KnobSource {
    /// Set through `PUT /api/config` and persisted beside the registry.
    Override,
    /// From the `PG_VM_POOL_*` environment at boot.
    Env,
    Default,
}

/// One row of `GET /api/config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnobInfo {
    /// The `RuntimeKnobs` field name, or the env var for a fixed setting.
    pub key: String,
    /// The environment variable that sets it at boot.
    pub env: String,
    /// Rendered value; `null` when the subsystem is off.
    #[serde(default)]
    pub value: Option<String>,
    pub source: KnobSource,
    /// Whether `PUT /api/config` may change it.
    pub mutable: bool,
    /// Why it cannot be changed here, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `GET /api/config` (and the response to `PUT`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigView {
    pub effective: RuntimeKnobs,
    pub knobs: Vec<KnobInfo>,
}

// ---- dedicated databases --------------------------------------------------

/// Body for `POST /api/databases`. Only `database` is required: `username`
/// defaults to the database name and `password` to a generated one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDatabase {
    pub database: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Response to a successful provision — the only place the password is ever
/// returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedDatabase {
    pub database: String,
    pub username: String,
    pub password: String,
    /// `provisioning`: the credential is live now; the VM comes up in the
    /// background and the first connection waits for it.
    pub status: String,
    pub created_at: u64,
}

/// One record in `GET /api/databases`. No password, by construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseInfo {
    pub database: String,
    pub username: String,
    pub created_at: u64,
    #[serde(default)]
    pub sandbox_id: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip_through_their_path_segment() {
        for a in [
            SchemaAction::Start,
            SchemaAction::Stop,
            SchemaAction::Reboot,
            SchemaAction::Resize,
            SchemaAction::Reap,
            SchemaAction::Restore,
            SchemaAction::ArchiveImage,
        ] {
            assert_eq!(SchemaAction::parse(a.as_str()), Some(a));
            assert_eq!(serde_json::to_value(a).unwrap(), a.as_str());
        }
        assert_eq!(SchemaAction::parse("delete"), None);
    }

    #[test]
    fn a_partial_knob_patch_leaves_the_rest_absent() {
        let k: RuntimeKnobs = serde_json::from_str(r#"{"idle_timeout_secs":600}"#).unwrap();
        assert_eq!(k.idle_timeout_secs, Some(600));
        assert_eq!(k.warm_spares, None);
        assert_eq!(
            serde_json::to_string(&k).unwrap(),
            r#"{"idle_timeout_secs":600}"#
        );
    }

    #[test]
    fn a_detail_flattens_its_info() {
        let d = SchemaDetail {
            info: SchemaInfo {
                schema: "acme".into(),
                tier: "live".into(),
                ..Default::default()
            },
            db_size_bytes: Some(42),
            backends: None,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["schema"], "acme");
        assert_eq!(v["db_size_bytes"], 42);
        let back: SchemaDetail = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }
}
