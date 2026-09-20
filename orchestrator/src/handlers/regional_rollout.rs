//! Durable, explicitly drained, region-at-a-time service revision rollout.
//!
//! The state row is the work queue. Every non-idempotent create is preceded by
//! a durable `creating` marker; an uncertain create is only adopted from
//! discovery and is never blindly repeated.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::time::{interval, MissedTickBehavior};

use super::regional_plan::Plan;
use super::service_deploy::ServiceDeployRequest;
use super::{service_deploy, service_discovery};
use crate::{auth, db, AppState};

const RECONCILE_SECONDS: u64 = 5;
const MAX_BAKE_SECONDS: u64 = 86_400;
const MAX_DRAIN_SECONDS: u64 = 3_600;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionalRolloutRequest {
    pub operation_id: String,
    pub deployment: ServiceDeployRequest,
    pub minimum_serving_replicas: u16,
    pub bake_seconds: u64,
    pub drain_timeout_seconds: u64,
    #[serde(default)]
    pub runtime_by_region: HashMap<String, RegionalRuntime>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionalRuntime {
    pub driver: String,
    pub image: String,
    pub size_class: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Slot {
    index: usize,
    region: String,
    candidate_id: String,
    #[serde(default)]
    runtime: Option<RegionalRuntime>,
}

#[derive(Debug, Clone)]
struct Rollout {
    operation_id: String,
    service_id: String,
    request: ServiceDeployRequest,
    target_revision: String,
    observer_topology: String,
    baseline: service_deploy::ServiceDeploymentState,
    regions: Vec<String>,
    slots: Vec<Slot>,
    plan: Plan,
    items: serde_json::Value,
    events: serde_json::Value,
    old: HashMap<String, Vec<String>>,
    minimum: u16,
    bake_seconds: u64,
    drain_seconds: u64,
    status: String,
    phase: String,
    region_index: usize,
    slot_index: usize,
    version: Option<u64>,
    deadline: Option<DateTime<Utc>>,
    last_observed: Option<DateTime<Utc>>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RolloutStatus {
    operation_id: String,
    service_id: String,
    status: String,
    phase: String,
    regions: Vec<String>,
    region_index: usize,
    slot_index: usize,
    target_revision: String,
    discovery_version: Option<u64>,
    deadline_at: Option<DateTime<Utc>>,
    error: Option<String>,
    plan: Plan,
    slots: Vec<Slot>,
    minimum_serving_replicas: u16,
    bake_seconds: u64,
    drain_timeout_seconds: u64,
    items: serde_json::Value,
    events: serde_json::Value,
}

impl From<Rollout> for RolloutStatus {
    fn from(r: Rollout) -> Self {
        Self {
            operation_id: r.operation_id,
            service_id: r.service_id,
            status: r.status,
            phase: r.phase,
            regions: r.regions,
            region_index: r.region_index,
            slot_index: r.slot_index,
            target_revision: r.target_revision,
            discovery_version: r.version,
            deadline_at: r.deadline,
            error: r.error,
            plan: r.plan,
            slots: r.slots,
            minimum_serving_replicas: r.minimum,
            bake_seconds: r.bake_seconds,
            drain_timeout_seconds: r.drain_seconds,
            items: r.items,
            events: r.events,
        }
    }
}

type ApiResponse = (StatusCode, Json<serde_json::Value>);
fn response(status: StatusCode, value: impl Serialize) -> ApiResponse {
    (
        status,
        Json(
            serde_json::to_value(value).unwrap_or_else(|_| json!({"error":"serialization failed"})),
        ),
    )
}

pub async fn create(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(request): Json<RegionalRolloutRequest>,
) -> ApiResponse {
    if let Err(s) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return response(s, json!({"error":"Unauthorized"}));
    }
    if let Ok(db) = db::get_db() {
        if let Ok(Some(existing)) = load(Some(db), &request.operation_id).await {
            let stored = db
                .query_one(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT request_hash FROM regional_service_rollouts WHERE operation_id=$1",
                    [request.operation_id.clone().into()],
                ))
                .await;
            return match stored {
                Ok(Some(row))
                    if row.try_get::<String>("", "request_hash").ok().as_deref()
                        == payload_hash(&request).ok().as_deref() =>
                {
                    response(StatusCode::OK, RolloutStatus::from(existing))
                }
                Ok(Some(_)) => response(
                    StatusCode::CONFLICT,
                    json!({"error":"operationId was already used with a different payload"}),
                ),
                _ => response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error":"Failed to verify rollout idempotency"}),
                ),
            };
        }
    }
    if let Err((s, message)) = validate_admission(&state, &request).await {
        return response(s, json!({"error":message}));
    }
    match admit(&state, request).await {
        Ok((r, created)) => response(
            if created {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            RolloutStatus::from(r),
        ),
        Err(e) => {
            tracing::warn!("regional rollout admission failed: {e:#}");
            response(StatusCode::CONFLICT, json!({"error":e.to_string()}))
        }
    }
}

pub async fn get(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> ApiResponse {
    if let Err(s) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return response(s, json!({"error":"Unauthorized"}));
    }
    match load(db::get_db().ok(), &operation_id).await {
        Ok(Some(r)) => response(StatusCode::OK, RolloutStatus::from(r)),
        Ok(None) => response(
            StatusCode::NOT_FOUND,
            json!({"error":"Regional rollout not found"}),
        ),
        Err(e) => {
            tracing::warn!("regional rollout read failed: {e:#}");
            response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"Failed to read regional rollout"}),
            )
        }
    }
}

pub async fn resume(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> ApiResponse {
    if let Err(s) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return response(s, json!({"error":"Unauthorized"}));
    }
    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            return response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":e.to_string()}),
            )
        }
    };
    let result = db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET status='running', error_message=NULL,
         last_observed_at=NULL,
         deadline_at=CASE WHEN phase IN ('wait_drained','wait_restored','rollback_wait')
             THEN NOW() + drain_timeout_seconds * INTERVAL '1 second' ELSE deadline_at END,
         updated_at=NOW() WHERE operation_id=$1 AND status='blocked'",
        [operation_id.clone().into()])).await;
    match result {
        Ok(v) if v.rows_affected() == 1 => match load(Some(db), &operation_id).await {
            Ok(Some(r)) => response(StatusCode::ACCEPTED, RolloutStatus::from(r)),
            Err(e) => response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":e.to_string()}),
            ),
            _ => unreachable!(),
        },
        Ok(_) => response(
            StatusCode::CONFLICT,
            json!({"error":"Only a blocked rollout can be resumed"}),
        ),
        Err(e) => response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":e.to_string()}),
        ),
    }
}

pub async fn rollback(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> ApiResponse {
    if let Err(status) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return response(status, json!({"error":"Unauthorized"}));
    }
    let result = async {
        let db = db::get_db()?;
        let r = load(Some(db), &operation_id).await?.context("rollout not found")?;
        let _lock = service_deploy::try_service_lifecycle_lock(db, &r.service_id)
            .await?.context("service lifecycle is busy")?;
        let changed = db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET status='running', phase='rollback_restore',
             region_index=0, slot_index=0, deadline_at=NULL, last_observed_at=NULL,
             error_message=NULL, updated_at=NOW()
             WHERE operation_id=$1 AND status IN ('running','blocked')",
            [operation_id.clone().into()])).await?;
        if changed.rows_affected() != 1 { bail!("only an active or blocked rollout can be rolled back"); }
        Ok::<_, anyhow::Error>(())
    }.await;
    match result {
        Ok(()) => response(StatusCode::ACCEPTED, json!({"operationId":operation_id,"phase":"rollback_restore"})),
        Err(error) => response(StatusCode::CONFLICT, json!({"error":error.to_string()})),
    }
}

async fn validate_admission(
    state: &AppState,
    r: &RegionalRolloutRequest,
) -> std::result::Result<(), (StatusCode, String)> {
    if r.operation_id.trim().is_empty() || r.operation_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "operationId must contain 1-128 characters".into(),
        ));
    }
    service_deploy::validate_service_deployment_request(state, &r.deployment).await?;
    validate_policy(r).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let regions = ordered_regions(&r.deployment.replica_regions);
    service_deploy::validate_regional_observers(state, &r.deployment.service_id, &regions)
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("regional observers are not ready: {e:#}"),
            )
        })?;
    Ok(())
}

fn validate_policy(r: &RegionalRolloutRequest) -> std::result::Result<(), String> {
    let d = &r.deployment;
    // Admission, lifecycle locks, and candidate creation must use the same key.
    // Ordinary deployment trims service IDs; durable regional plans reject them.
    if d.service_id.is_empty() || !d.service_id.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
    }) {
        return Err("serviceId must contain only lowercase letters, digits, and '-' characters".into());
    }
    if d.archive_id
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        return Err("archiveId must identify an immutable archive".into());
    }
    if d.archive_bytes_base64.is_some() {
        return Err("archiveBytesBase64 is not accepted".into());
    }
    if d.env.as_ref().is_some_and(|v| !v.is_empty()) {
        return Err("plaintext env is not accepted; use envRefs".into());
    }
    if d.delete_previous {
        return Err("deletePrevious must be false so rollback capacity is retained".into());
    }
    let desired = d
        .desired_replicas
        .ok_or_else(|| "desiredReplicas is required".to_string())? as usize;
    if desired != d.replica_regions.len() {
        return Err("desiredReplicas must equal replicaRegions slots".into());
    }
    if ordered_regions(&d.replica_regions).len() < 2 {
        return Err("at least two distinct regions are required".into());
    }
    if d.replica_regions.iter().any(|v| v.trim().is_empty()) {
        return Err("replicaRegions cannot contain an empty region".into());
    }
    if r.runtime_by_region.iter().any(|(region, runtime)| {
        !d.replica_regions.contains(region) || runtime.driver.trim().is_empty()
            || runtime.image.trim().is_empty() || runtime.size_class.trim().is_empty()
    }) {
        return Err("runtimeByRegion requires a target region and nonempty driver, image, sizeClass".into());
    }
    if r.minimum_serving_replicas == 0 || r.minimum_serving_replicas as usize >= desired {
        return Err("minimumServingReplicas must be between 1 and desiredReplicas-1".into());
    }
    for region in ordered_regions(&d.replica_regions) {
        let outside = d.replica_regions.iter().filter(|r| **r != region).count();
        if outside < usize::from(r.minimum_serving_replicas) {
            return Err(format!("planned capacity outside {region} cannot satisfy minimumServingReplicas"));
        }
    }
    if r.bake_seconds == 0 || r.bake_seconds > MAX_BAKE_SECONDS {
        return Err(format!("bakeSeconds must be between 1 and {MAX_BAKE_SECONDS}"));
    }
    if r.drain_timeout_seconds == 0 || r.drain_timeout_seconds > MAX_DRAIN_SECONDS {
        return Err(format!(
            "drainTimeoutSeconds must be between 1 and {MAX_DRAIN_SECONDS}"
        ));
    }
    Ok(())
}

fn ordered_regions(slots: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    slots
        .iter()
        .filter(|r| seen.insert((*r).clone()))
        .cloned()
        .collect()
}

async fn admit(state: &AppState, request: RegionalRolloutRequest) -> Result<(Rollout, bool)> {
    let hash = payload_hash(&request)?;
    let db = db::get_db()?;
    if let Some(existing) = load(Some(db), &request.operation_id).await? {
        let stored = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT request_hash FROM regional_service_rollouts WHERE operation_id=$1",
                [request.operation_id.clone().into()],
            ))
            .await?
            .unwrap();
        let existing_hash: String = stored.try_get("", "request_hash")?;
        if hash != existing_hash {
            bail!("operationId was already used with a different payload");
        }
        return Ok((existing, false));
    }
    let lock = service_deploy::try_service_lifecycle_lock(db, &request.deployment.service_id)
        .await?
        .context("service lifecycle is busy")?;
    // Close the admission race after taking the same lock used by ordinary
    // deploy/retire operations.
    if let Some(existing) = load(Some(db), &request.operation_id).await? {
        let stored = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT request_hash FROM regional_service_rollouts WHERE operation_id=$1",
                [request.operation_id.clone().into()],
            ))
            .await?
            .context("concurrent rollout disappeared")?;
        if stored.try_get::<String>("", "request_hash")? != hash {
            bail!("operationId was already used with a different payload");
        }
        lock.commit().await?;
        return Ok((existing, false));
    }
    ensure_no_regional_rollout(db, &request.deployment.service_id).await?;
    if db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM service_rollouts WHERE service_id=$1 AND status='running'",
        [request.deployment.service_id.clone().into()])).await?.is_some() {
        bail!("ordinary rollout must finish or be reconciled before a regional rollout");
    }
    let baseline = service_deploy::regional_baseline(state, &request.deployment).await?;
    let target = service_deploy::regional_revision(state, &request.deployment).await?;
    let regions = ordered_regions(&request.deployment.replica_regions);
    let slots: Vec<Slot> = request
        .deployment
        .replica_regions
        .iter()
        .enumerate()
        .map(|(index, region)| Slot {
            index,
            region: region.clone(),
            candidate_id: candidate_id(&request.operation_id, index),
            runtime: request.runtime_by_region.get(region).cloned(),
        })
        .collect();
    let plan = Plan::compile(&regions, &slots.iter()
        .map(|s| (s.region.clone(), s.candidate_id.clone())).collect::<Vec<_>>());
    let tx = db.begin().await?;
    let inserted = tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_service_rollouts (operation_id,service_id,request_hash,deployment_request,target_revision,regions,slots,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,observer_topology,baseline_state,plan,status,phase) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,'running','preflight') ON CONFLICT (operation_id) DO NOTHING",
        vec![request.operation_id.clone().into(), request.deployment.service_id.clone().into(), hash.clone().into(), serde_json::to_value(&request.deployment)?.into(), target.into(), serde_json::to_value(regions)?.into(), serde_json::to_value(slots)?.into(), (request.minimum_serving_replicas as i32).into(), (request.bake_seconds as i64).into(), (request.drain_timeout_seconds as i64).into(), super::regional_observers::topology(state, &request.deployment.service_id)?.into(), serde_json::to_value(baseline)?.into(), serde_json::to_value(plan)?.into()])).await;
    let created = match inserted {
        Ok(v) if v.rows_affected() == 1 => {
            tx.commit().await?;
            true
        }
        Ok(_) => {
            tx.rollback().await?;
            false
        }
        Err(e) => {
            tx.rollback().await?;
            return Err(e.into());
        }
    };
    let row = load(Some(db), &request.operation_id)
        .await?
        .context("admitted rollout disappeared")?;
    if !created {
        let stored = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT request_hash FROM regional_service_rollouts WHERE operation_id=$1",
                [request.operation_id.into()],
            ))
            .await?
            .context("conflicting rollout disappeared")?;
        if stored.try_get::<String>("", "request_hash")? != hash {
            bail!("operationId was already used with a different payload");
        }
    }
    lock.commit().await?;
    Ok((row, created))
}

fn candidate_id(operation: &str, index: usize) -> String {
    let digest = Sha256::digest(operation.as_bytes());
    format!("regional-{}-{index}", hex_prefix(&digest, 20))
}
fn hex_prefix(bytes: &[u8], count: usize) -> String {
    bytes
        .iter()
        .flat_map(|b| {
            [
                char::from_digit((b >> 4) as u32, 16).unwrap(),
                char::from_digit((b & 15) as u32, 16).unwrap(),
            ]
        })
        .take(count)
        .collect()
}
fn payload_hash(r: &RegionalRolloutRequest) -> Result<String> {
    // Canonical JSON object ordering also covers maps in nested metadata.
    Ok(hex_prefix(&Sha256::digest(serde_json::to_vec(&serde_json::to_value(r)?)?), 64))
}

pub async fn ensure_no_regional_rollout(
    connection: &impl ConnectionTrait,
    service_id: &str,
) -> Result<()> {
    if connection.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM regional_service_rollouts WHERE service_id=$1 AND status IN ('running','blocked')", [service_id.into()])).await?.is_some() {
        bail!("service has an active or blocked regional rollout");
    }
    Ok(())
}

pub async fn run_reconciler(state: AppState) {
    let mut ticker = interval(Duration::from_secs(RECONCILE_SECONDS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if let Err(e) = reconcile(&state).await {
            tracing::warn!("regional rollout reconciliation failed: {e:#}");
        }
    }
}

async fn reconcile(state: &AppState) -> Result<()> {
    let db = db::get_db()?;
    let rows = db.query_all(Statement::from_string(DbBackend::Postgres, "SELECT operation_id FROM regional_service_rollouts WHERE status='running' ORDER BY created_at")).await?;
    for row in rows {
        let id: String = row.try_get("", "operation_id")?;
        if let Err(e) = tick(state, &id).await {
            tracing::warn!(operation_id = id, "regional rollout tick failed: {e:#}");
        }
    }
    Ok(())
}

async fn tick(state: &AppState, operation: &str) -> Result<()> {
    let db = db::get_db()?;
    let Some(r) = load(Some(db), operation).await? else {
        return Ok(());
    };
    if r.status != "running" {
        return Ok(());
    }
    let Some(lock) = service_deploy::try_service_lifecycle_lock(db, &r.service_id).await? else {
        return Ok(());
    };
    let r = load(Some(db), operation)
        .await?
        .context("rollout disappeared")?;
    if r.status != "running" {
        return Ok(());
    }
    // State transitions use a separate autocommit connection while the advisory
    // transaction remains open. Thus the intent is durable before an external
    // side effect, without releasing the shared service lifecycle lock.
    let result = async {
        service_deploy::validate_regional_observers(state, &r.service_id, &r.regions).await?;
        if super::regional_observers::topology(state, &r.service_id)? != r.observer_topology {
            bail!("ingress observer topology changed; restore the admitted observer set before resuming");
        }
        step(state, db, r).await
    }.await;
    // Block while still holding ownership. Otherwise another controller could
    // advance the same operation between lock release and the error write.
    if let Err(error) = result {
        block(db, operation, &format!("{error:#}")).await?;
    }
    lock.commit().await?;
    Ok(())
}

async fn step(state: &AppState, db: &impl ConnectionTrait, mut r: Rollout) -> Result<()> {
    let item = r.plan.step(&r.phase, r.region_index, r.slot_index)?;
    if r.phase.starts_with("rollback_") {
        return rollback_step(state, db, &r).await;
    }
    if r.phase == "verify" {
        return verify_complete(state, db, &r).await;
    }
    let region = item.region.as_ref()
        .context("regional plan item has no region")?
        .clone();
    match r.phase.as_str() {
        "preflight" => {
            let snapshot = snapshot(&r.service_id).await?;
            probe_survivors(state, &r, &snapshot, &region).await?;
            let old: Vec<String> = snapshot
                .endpoints
                .iter()
                .filter(|e| {
                    e.region.as_deref() == Some(&region)
                        && !e.draining
                })
                .map(|e| e.deployment_id.clone())
                .collect();
            if old.is_empty() { bail!("region {region} has no serving rollback capacity"); }
            r.old.insert(region, old);
            update(db, &r, "exclude_region", 0, None, None, None).await?;
        }
        "exclude_region" => {
            let before = snapshot(&r.service_id).await?;
            probe_survivors(state, &r, &before, &region).await?;
            service_discovery::set_region_draining(&r.service_id, &region, true).await?;
            let s = snapshot(&r.service_id).await?;
            update(
                db,
                &r,
                "wait_drained",
                0,
                Some(s.version),
                Some(Utc::now() + chrono::Duration::seconds(r.drain_seconds as i64)),
                None,
            )
            .await?;
        }
        "wait_drained" => {
            let s = snapshot(&r.service_id).await?;
            probe_survivors(state, &r, &s, &region).await?;
            let withdrawn: Vec<_> = s.endpoints.iter()
                .filter(|e| e.region.as_deref() == Some(&region))
                .map(|e| e.deployment_id.clone()).collect();
            if service_deploy::regional_observe(
                state,
                &r.service_id,
                &s,
                &withdrawn,
            )
            .await?
            {
                update(db, &r, "create_slot", 0, Some(s.version), None, None).await?;
            } else if r.deadline.is_some_and(|d| Utc::now() >= d) {
                bail!("drain timed out in region {region}");
            }
        }
        "create_slot" => {
            let regional: Vec<&Slot> = r.slots.iter().filter(|s| s.region == region).collect();
            if r.slot_index >= regional.len() {
                update(db, &r, "probe_candidates", r.slot_index, None, None, None).await?;
            } else {
                let s = snapshot(&r.service_id).await?;
                probe_survivors(state, &r, &s, &region).await?;
                update(db, &r, "creating", r.slot_index, None, None, None).await?;
                let candidate_request = request_for_slot(&r.request, regional[r.slot_index]);
                let candidate_id = item.candidate_id.as_deref().context("plan item has no candidate")?;
                if candidate_id != regional[r.slot_index].candidate_id {
                    bail!("plan candidate does not match its pinned runtime slot");
                }
                service_deploy::regional_create_candidate(
                    state,
                    &candidate_request,
                    candidate_id,
                    &region,
                )
                .await?;
                update(db, &r, "create_slot", r.slot_index + 1, None, None, None).await?;
            }
        }
        "creating" => {
            let regional: Vec<&Slot> = r.slots.iter().filter(|s| s.region == region).collect();
            let slot = regional
                .get(r.slot_index)
                .context("creating cursor out of bounds")?;
            let s = snapshot(&r.service_id).await?;
            let adopted = s
                .endpoints
                .iter()
                .any(|e| candidate_matches(e, slot, &r.target_revision));
            if !adopted {
                bail!(
                    "uncertain candidate creation cannot be reconciled safely: {}",
                    slot.candidate_id
                );
            }
            update(db, &r, "create_slot", r.slot_index + 1, None, None, None).await?;
        }
        "probe_candidates" => {
            let s = snapshot(&r.service_id).await?;
            probe_targets(state, &r, &s, &region).await?;
            update(db, &r, "mark_old_draining", r.slot_index, None, None, None).await?;
        }
        "mark_old_draining" => {
            for id in r.old.get(&region).into_iter().flatten() {
                service_discovery::mark_endpoint_draining(&r.service_id, id).await?;
            }
            update(db, &r, "restore_region", r.slot_index, None, None, None).await?;
        }
        "restore_region" => {
            let before = snapshot(&r.service_id).await?;
            probe_targets(state, &r, &before, &region).await?;
            service_discovery::set_region_draining(&r.service_id, &region, false).await?;
            let s = snapshot(&r.service_id).await?;
            update(
                db,
                &r,
                "wait_restored",
                r.slot_index,
                Some(s.version),
                Some(Utc::now() + chrono::Duration::seconds(r.drain_seconds as i64)),
                None,
            )
            .await?;
        }
        "wait_restored" => {
            let s = snapshot(&r.service_id).await?;
            probe_targets(state, &r, &s, &region).await?;
            if service_deploy::regional_observe(state, &r.service_id, &s, &[]).await? {
                update(
                    db,
                    &r,
                    "bake",
                    r.slot_index,
                    Some(s.version),
                    Some(Utc::now() + chrono::Duration::seconds(r.bake_seconds as i64)),
                    Some(Utc::now()),
                )
                .await?;
            } else if r.deadline.is_some_and(|d| Utc::now() >= d) {
                bail!("routing restoration timed out in region {region}");
            }
        }
        "bake" => {
            let s = snapshot(&r.service_id).await?;
            probe_targets(state, &r, &s, &region).await?;
            let now = Utc::now();
            let interrupted = r.last_observed.is_none_or(|last| {
                now - last > chrono::Duration::seconds((RECONCILE_SECONDS * 3) as i64)
            });
            if interrupted {
                update(
                    db,
                    &r,
                    "bake",
                    r.slot_index,
                    r.version,
                    Some(now + chrono::Duration::seconds(r.bake_seconds as i64)),
                    Some(now),
                )
                .await?;
            } else if r.deadline.is_some_and(|d| now >= d) {
                if r.region_index + 1 == r.regions.len() {
                    update_region(db, &r, "verify", r.region_index + 1, 0, None, None).await?;
                } else {
                    update_region(db, &r, "preflight", r.region_index + 1, 0, None, None).await?;
                }
            } else {
                update(
                    db,
                    &r,
                    "bake",
                    r.slot_index,
                    r.version,
                    r.deadline,
                    Some(now),
                )
                .await?;
            }
        }
        other => bail!("unknown regional rollout phase {other}"),
    }
    Ok(())
}

async fn probe_survivors(
    state: &AppState,
    r: &Rollout,
    s: &service_discovery::ServiceDiscoverySnapshot,
    region: &str,
) -> Result<()> {
    let endpoints: Vec<_> = s
        .endpoints
        .iter()
        .filter(|e| {
            e.region.as_deref().is_some_and(|value| value != region)
                && !e.draining && e.health_status == "healthy"
        })
        .collect();
    if endpoints.len() < r.minimum as usize {
        bail!(
            "only {} healthy serving replicas remain outside {region}; minimum is {}",
            endpoints.len(),
            r.minimum
        );
    }
    for e in endpoints {
        service_deploy::regional_probe(state, &r.request, e).await?;
    }
    Ok(())
}
async fn probe_targets(
    state: &AppState,
    r: &Rollout,
    s: &service_discovery::ServiceDiscoverySnapshot,
    region: &str,
) -> Result<()> {
    let expected: Vec<_> = r.slots.iter().filter(|x| x.region == region).collect();
    for slot in expected {
        let e = s
            .endpoints
            .iter()
            .find(|e| candidate_matches(e, slot, &r.target_revision))
            .with_context(|| {
                format!(
                    "target candidate {} is not healthy in discovery",
                    slot.candidate_id
                )
            })?;
        service_deploy::regional_probe(state, &r.request, e).await?;
    }
    Ok(())
}
fn request_for_slot(request: &ServiceDeployRequest, slot: &Slot) -> ServiceDeployRequest {
    let mut request = request.clone();
    if let Some(runtime) = &slot.runtime {
        request.driver = runtime.driver.clone();
        request.image = runtime.image.clone();
        request.size_class = runtime.size_class.clone();
    }
    request.region = slot.region.clone();
    request
}

fn candidate_matches(
    endpoint: &service_discovery::ServiceDiscoveryEndpoint,
    slot: &Slot,
    revision: &str,
) -> bool {
    endpoint.deployment_id == slot.candidate_id
        && endpoint.region.as_deref() == Some(slot.region.as_str())
        && endpoint.revision.as_deref() == Some(revision)
        && endpoint.health_status == "healthy"
}
async fn snapshot(service: &str) -> Result<service_discovery::ServiceDiscoverySnapshot> {
    service_discovery::read_snapshot(service)
        .await?
        .context("service must already have discovery-routed endpoints")
}

async fn verify_complete(state: &AppState, db: &impl ConnectionTrait, r: &Rollout) -> Result<()> {
    let s = snapshot(&r.service_id).await?;
    for region in &r.regions {
        probe_targets(state, r, &s, region).await?;
        let expected = r.slots.iter().filter(|x| &x.region == region).count();
        let actual = s
            .endpoints
            .iter()
            .filter(|e| {
                e.region.as_ref() == Some(region)
                    && e.revision.as_deref() == Some(&r.target_revision)
                    && e.health_status == "healthy"
                    && !e.draining
            })
            .count();
        if actual != expected {
            bail!("region {region} has {actual} target replicas; expected {expected}");
        }
    }
    if !service_deploy::regional_observe(state, &r.service_id, &s, &[]).await? {
        bail!("final routing state is not adopted by every ingress");
    }
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,"UPDATE regional_service_rollouts SET status='passed',phase='passed',completed_at=NOW(),updated_at=NOW(),error_message=NULL WHERE operation_id=$1 AND status='running'",[r.operation_id.clone().into()])).await?;
    Ok(())
}

async fn rollback_step(state: &AppState, db: &impl ConnectionTrait, r: &Rollout) -> Result<()> {
    let Some(region) = r.regions.get(r.region_index) else {
        service_deploy::restore_regional_baseline(state, &r.baseline).await?;
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET status='rolled_back',phase='rolled_back',
             completed_at=NOW(),updated_at=NOW() WHERE operation_id=$1 AND status='running'",
            [r.operation_id.clone().into()])).await?;
        return Ok(());
    };
    let Some(old) = r.old.get(region) else {
        return update_region(db, r, "rollback_restore", r.region_index + 1, 0, None, None).await;
    };
    let s = snapshot(&r.service_id).await?;
    for id in old {
        let endpoint = s.endpoints.iter().find(|e| &e.deployment_id == id)
            .context("retained rollback endpoint is missing")?;
        service_deploy::regional_probe(state, &r.request, endpoint).await?;
    }
    let candidates: Vec<_> = r.slots.iter().filter(|slot| &slot.region == region)
        .filter(|slot| s.endpoints.iter().any(|e| e.deployment_id == slot.candidate_id))
        .map(|slot| slot.candidate_id.clone()).collect();
    if r.phase == "rollback_restore" {
        for id in old { service_discovery::mark_endpoint_active(&r.service_id, id).await?; }
        for id in &candidates { service_discovery::mark_endpoint_draining(&r.service_id, id).await?; }
        service_discovery::set_region_draining(&r.service_id, region, false).await?;
        update(db, r, "rollback_wait", 0, None,
            Some(Utc::now() + chrono::Duration::seconds(r.drain_seconds as i64)), None).await?;
    } else if service_deploy::regional_observe(state, &r.service_id, &s, &candidates).await? {
        update_region(db, r, "rollback_restore", r.region_index + 1, 0, None, None).await?;
    } else if r.deadline.is_some_and(|d| Utc::now() >= d) {
        bail!("rollback drain timed out in region {region}");
    }
    Ok(())
}
async fn block(db: &impl ConnectionTrait, id: &str, error: &str) -> Result<()> {
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,"UPDATE regional_service_rollouts SET status='blocked',error_message=$2,updated_at=NOW() WHERE operation_id=$1 AND status='running'",[id.into(),error.into()])).await?;
    Ok(())
}
async fn update(
    db: &impl ConnectionTrait,
    r: &Rollout,
    phase: &str,
    slot: usize,
    version: Option<u64>,
    deadline: Option<DateTime<Utc>>,
    observed: Option<DateTime<Utc>>,
) -> Result<()> {
    update_full(
        db,
        r,
        phase,
        r.region_index,
        slot,
        version,
        deadline,
        observed,
    )
    .await
}
async fn update_region(
    db: &impl ConnectionTrait,
    r: &Rollout,
    phase: &str,
    region: usize,
    slot: usize,
    deadline: Option<DateTime<Utc>>,
    observed: Option<DateTime<Utc>>,
) -> Result<()> {
    update_full(db, r, phase, region, slot, None, deadline, observed).await
}
async fn update_full(
    db: &impl ConnectionTrait,
    r: &Rollout,
    phase: &str,
    region: usize,
    slot: usize,
    version: Option<u64>,
    deadline: Option<DateTime<Utc>>,
    observed: Option<DateTime<Utc>>,
) -> Result<()> {
    r.plan.step(phase, region, slot)?;
    db.execute(Statement::from_sql_and_values(DbBackend::Postgres,"UPDATE regional_service_rollouts SET phase=$2,region_index=$3,slot_index=$4,discovery_version=$5,deadline_at=$6,last_observed_at=$7,old_endpoints=$8,updated_at=NOW() WHERE operation_id=$1 AND status='running'",vec![r.operation_id.clone().into(),phase.into(),(region as i32).into(),(slot as i32).into(),version.map(|x|x as i64).into(),deadline.into(),observed.into(),serde_json::to_value(&r.old)?.into()])).await?;
    Ok(())
}

async fn load(connection: Option<&impl ConnectionTrait>, id: &str) -> Result<Option<Rollout>> {
    let Some(db) = connection else {
        bail!("database unavailable")
    };
    let Some(x)=db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.*,
         COALESCE((SELECT jsonb_agg(jsonb_build_object('stepId',i.step_id,'status',i.status,
             'attempts',i.attempts,'startedAt',i.started_at,'completedAt',i.completed_at,
             'error',i.error_message) ORDER BY i.step_id)
             FROM regional_rollout_items i WHERE i.operation_id=r.operation_id),'[]'::jsonb) AS items,
         COALESCE((SELECT jsonb_agg(jsonb_build_object('id',e.id,'stepId',e.step_id,'status',e.status,
             'discoveryVersion',e.discovery_version,'deadlineAt',e.deadline_at,
             'error',e.error_message,'createdAt',e.created_at) ORDER BY e.id)
             FROM (SELECT * FROM regional_rollout_events WHERE operation_id=r.operation_id ORDER BY id DESC LIMIT 100) e),
             '[]'::jsonb) AS events
         FROM regional_service_rollouts r WHERE r.operation_id=$1",[id.into()])).await? else{return Ok(None)};
    let request_json: serde_json::Value = x.try_get("", "deployment_request")?;
    let regions_json: serde_json::Value = x.try_get("", "regions")?;
    let slots_json: serde_json::Value = x.try_get("", "slots")?;
    let old_json: serde_json::Value = x.try_get("", "old_endpoints")?;
    let version: Option<i64> = x.try_get("", "discovery_version")?;
    Ok(Some(Rollout {
        operation_id: x.try_get("", "operation_id")?,
        service_id: x.try_get("", "service_id")?,
        request: serde_json::from_value(request_json)?,
        target_revision: x.try_get("", "target_revision")?,
        observer_topology: x.try_get("", "observer_topology")?,
        baseline: serde_json::from_value(x.try_get("", "baseline_state")?)?,
        regions: serde_json::from_value(regions_json)?,
        slots: serde_json::from_value(slots_json)?,
        plan: serde_json::from_value(x.try_get("", "plan")?)?,
        items: x.try_get("", "items")?,
        events: x.try_get("", "events")?,
        old: serde_json::from_value(old_json)?,
        minimum: x.try_get::<i32>("", "minimum_serving_replicas")? as u16,
        bake_seconds: x.try_get::<i64>("", "bake_seconds")? as u64,
        drain_seconds: x.try_get::<i64>("", "drain_timeout_seconds")? as u64,
        status: x.try_get("", "status")?,
        phase: x.try_get("", "phase")?,
        region_index: x.try_get::<i32>("", "region_index")? as usize,
        slot_index: x.try_get::<i32>("", "slot_index")? as usize,
        version: version.map(|v| v as u64),
        deadline: x.try_get("", "deadline_at")?,
        last_observed: x.try_get("", "last_observed_at")?,
        error: x.try_get("", "error_message")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(regions: &[&str]) -> RegionalRolloutRequest {
        RegionalRolloutRequest { operation_id:"op-a".into(),deployment:serde_json::from_value(json!({"serviceId":"svc","userId":"u","archiveId":"immutable-a","desiredReplicas":regions.len(),"replicaRegions":regions,"deletePrevious":false})).unwrap(),minimum_serving_replicas:1,bake_seconds:1,drain_timeout_seconds:1,runtime_by_region:HashMap::new() }
    }
    #[test]
    fn regions_preserve_first_occurrence_and_asymmetry() {
        let r = request(&["eu", "us", "eu"]);
        assert_eq!(
            ordered_regions(&r.deployment.replica_regions),
            vec!["eu", "us"]
        );
        assert!(validate_policy(&r).is_ok());
    }
    #[test]
    fn policy_boundaries_are_enforced() {
        let mut r = request(&["eu", "us"]);
        r.minimum_serving_replicas = 2;
        assert!(validate_policy(&r).is_err());
        r.minimum_serving_replicas = 1;
        r.drain_timeout_seconds = 0;
        assert!(validate_policy(&r).is_err());
        r.drain_timeout_seconds = 1;
        r.bake_seconds = MAX_BAKE_SECONDS;
        assert!(validate_policy(&r).is_ok());
        r.bake_seconds = 0;
        assert!(validate_policy(&r).is_err());
        let mut asymmetric = request(&["eu", "eu", "us"]);
        asymmetric.minimum_serving_replicas = 2;
        assert!(validate_policy(&asymmetric).is_err());
    }
    #[test]
    fn service_identity_cannot_change_during_candidate_creation() {
        let mut r = request(&["eu", "us"]);
        for id in [" svc", "svc ", "", "Svc", "../svc"] {
            r.deployment.service_id = id.into();
            assert!(validate_policy(&r).is_err(), "accepted {id:?}");
        }
        r.deployment.service_id = "svc-2".into();
        assert!(validate_policy(&r).is_ok());
    }
    #[test]
    fn candidate_ids_are_stable_per_slot() {
        assert_eq!(candidate_id("x", 2), candidate_id("x", 2));
        assert_ne!(candidate_id("x", 1), candidate_id("x", 2));
    }
    #[test]
    fn runtime_plan_survives_reload_without_changing_other_regions() {
        let mut r = request(&["eu","us"]);
        r.deployment.driver = "firecracker".into();
        r.deployment.image = "us-base".into();
        let eu = Slot { index:0,region:"eu".into(),candidate_id:"eu-candidate".into(),
            runtime:Some(RegionalRuntime { driver:"libvirt".into(),image:"eu-base".into(),size_class:"large".into() }) };
        let persisted: Slot = serde_json::from_value(serde_json::to_value(eu).unwrap()).unwrap();
        let candidate = request_for_slot(&r.deployment,&persisted);
        assert_eq!((candidate.driver.as_str(),candidate.image.as_str(),candidate.region.as_str()),("libvirt","eu-base","eu"));
        let us = Slot { index:1,region:"us".into(),candidate_id:"us-candidate".into(),runtime:None };
        let candidate = request_for_slot(&r.deployment,&us);
        assert_eq!((candidate.driver.as_str(),candidate.image.as_str()),("firecracker","us-base"));
        assert_eq!(r.deployment.driver,"firecracker");
        r.runtime_by_region.insert("unknown".into(),persisted.runtime.unwrap());
        assert!(validate_policy(&r).is_err());
    }
    #[test]
    fn uncertain_create_adopts_only_an_exact_healthy_match() {
        let slot = Slot {
            index: 0,
            region: "eu".into(),
            candidate_id: "candidate".into(),
            runtime: None,
        };
        let mut endpoint = service_discovery::ServiceDiscoveryEndpoint {
            deployment_id: "candidate".into(),
            backend_server_id: None,
            region: Some("eu".into()),
            revision: Some("rev".into()),
            url: "http://example".into(),
            health_status: "healthy".into(),
            draining: true,
        };
        assert!(candidate_matches(&endpoint, &slot, "rev"));
        endpoint.revision = Some("old".into());
        assert!(!candidate_matches(&endpoint, &slot, "rev"));
        endpoint.revision = Some("rev".into());
        endpoint.health_status = "unhealthy".into();
        assert!(!candidate_matches(&endpoint, &slot, "rev"));
    }
    #[test]
    fn a_failed_candidate_gate_cannot_match_or_advance() {
        let slot = Slot {
            index: 0,
            region: "eu".into(),
            candidate_id: "candidate".into(),
            runtime: None,
        };
        let endpoint = service_discovery::ServiceDiscoveryEndpoint {
            deployment_id: "other".into(),
            backend_server_id: None,
            region: Some("eu".into()),
            revision: Some("rev".into()),
            url: "http://example".into(),
            health_status: "healthy".into(),
            draining: false,
        };
        assert!(!candidate_matches(&endpoint, &slot, "rev"));
    }
}

#[cfg(test)]
#[path = "regional_rollout_tests.rs"]
mod integration_tests;
