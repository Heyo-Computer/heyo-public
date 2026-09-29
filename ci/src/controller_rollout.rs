//! Read-only compatibility surface for legacy controller self-replacement.
//!
//! CI instances no longer coordinate a singleton executor or replace themselves.
//! Platform-owned per-instance lifecycle must perform controller updates. Existing
//! unfinished records are deliberately retained for operator inspection; this
//! module neither migrates nor settles them automatically.
use crate::{bus::JobMessage, dispatch::Dispatcher};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::sync::LazyLock;

const RETIRED: &str = "legacy CI self-replacement is unsupported; use the platform-owned per-instance managed lifecycle API";

pub fn binary_sha256() -> Option<&'static str> {
    static HASH: LazyLock<Option<String>> = LazyLock::new(|| {
        let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash).ok()?;
        Some(hex::encode(hash.finalize()))
    });
    HASH.as_deref()
}

fn etag(value: &Value) -> String {
    let mut value = value.clone();
    value.sort_all_objects();
    format!("\"{}\"", hex::encode(Sha256::digest(serde_json::to_vec(&value).expect("JSON serializes"))))
}

/// Retired before recording intent: callers must use the platform lifecycle.
pub async fn request(
    _d: &Dispatcher,
    _msg: &JobMessage,
    _step: &str,
    _artifact: &str,
    _workflow: Option<&str>,
) -> Result<String, String> {
    Err(RETIRED.into())
}

/// Preserve the legacy read API so unresolved records remain observable.
pub async fn application_status(d: &Dispatcher, id: &str) -> Result<Value, String> {
    let row = sqlx::query("SELECT c.request,c.application_id,c.phase,s.run_id,s.status,s.message FROM ci_controller_rollout c JOIN ci_service_deployment s ON s.id=c.id WHERE c.id=$1")
        .bind(id).fetch_optional(d.store.pool()).await.map_err(|e| e.to_string())?
        .ok_or("unknown controller update")?;
    let request: Value = row.get("request");
    Ok(json!({"operationId":id,"applicationId":row.get::<Option<String>,_>("application_id"),
        "intentHash":etag(&request),"deploymentId":request["deployment"],"authority":request["base_url"],
        "targetRevision":request["sha"],"artifactDigest":request["artifact"],
        "runId":row.get::<String,_>("run_id"),"status":row.get::<String,_>("status"),
        "phase":row.get::<String,_>("phase"),"message":row.get::<Option<String>,_>("message")}))
}

/// Legacy activation is intentionally non-mutating. Pending records require an
/// explicit operator/platform disposition rather than an automatic migration.
pub async fn activate_application_update(_d: &Dispatcher, _id: &str, _hash: &str) -> Result<(), String> {
    Err(RETIRED.into())
}
