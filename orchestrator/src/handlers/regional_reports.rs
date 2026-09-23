//! Durable evidence from individually authenticated, operation-pinned gateways.
//! Callers must authenticate the observer before recording its response.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::regional_policy::RegionalPolicy;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Participant {
    pub gateway_id: String,
    pub region: String,
    pub boot_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Report {
    pub gateway_id: String,
    pub boot_id: String,
    pub sequence: u64,
    pub generation: i64,
    pub prepared: bool,
    pub adopted: bool,
    /// Totals include all retained generations, not only the current snapshot.
    pub outgoing_target: u64,
    pub local_target: u64,
    pub peer_admission_closed: bool,
}

pub(super) fn validate_participants(
    participants: &[Participant], policy: &RegionalPolicy, predecessor: Option<&RegionalPolicy>,
) -> Result<()> {
    let mut expected = HashMap::new();
    for policy in std::iter::once(policy).chain(predecessor) {
        for region in &policy.regions {
            for gateway in &region.gateways {
                if let Some(previous) = expected.insert(gateway.id.as_str(), region.region.as_str()) {
                    anyhow::ensure!(previous == region.region, "gateway identity cannot move regions during a transition");
                }
            }
        }
    }
    anyhow::ensure!(participants.len() == expected.len(), "pin every source and destination gateway");
    for participant in participants {
        anyhow::ensure!(expected.remove(participant.gateway_id.as_str()) == Some(participant.region.as_str()),
            "missing, duplicate or incorrectly placed participant");
        anyhow::ensure!(uuid::Uuid::parse_str(&participant.boot_id).is_ok(), "gateway boot ID must be a UUID");
    }
    Ok(())
}

/// Called with a fresh authenticated poll response. The caller's elapsed poll
/// time is included in age_ms; a cached response must include its sample age too.
/// This is not an unauthenticated push endpoint.
pub(super) async fn record(
    db: &sea_orm::DatabaseConnection, service: &str, operation: &str, report: &Report, age_ms: u32,
) -> Result<bool> {
    anyhow::ensure!(age_ms <= 5_000, "gateway observation is too old");
    anyhow::ensure!(uuid::Uuid::parse_str(&report.boot_id).is_ok(), "gateway boot ID must be a UUID");
    anyhow::ensure!(report.sequence > 0 && report.sequence <= i64::MAX as u64,
        "gateway sequence is outside database range");
    let tx = super::service_deploy::try_service_lifecycle_lock(db, service).await?
        .context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.observer_topology FROM regional_service_rollouts r
         JOIN regional_policy_proposals p USING(service_id,operation_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND p.generation=$3
         AND (r.status IN ('running','blocked') OR (r.status IN ('passed','rolled_back')
             AND EXISTS (SELECT 1 FROM service_active_regional_policies a WHERE a.service_id=p.service_id AND a.generation=p.generation)
             AND NOT EXISTS (SELECT 1 FROM regional_policy_proposals newer WHERE newer.service_id=p.service_id AND newer.generation>p.generation)))",
        vec![service.into(), operation.into(), report.generation.into()])).await?.context("report has no owning proposal")?;
    let participants: Vec<Participant> = serde_json::from_str(&row.try_get::<String>("", "observer_topology")?)?;
    let pinned = participants.iter().find(|p| p.gateway_id == report.gateway_id)
        .context("gateway identity is not an operation participant")?;
    if pinned.boot_id != report.boot_id {
        // Retain the sequence high-water mark but permanently invalidate this
        // boot's evidence, even if an older in-flight poll completes afterward.
        tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_gateway_reports(service_id,gateway_id,boot_id,sequence,invalidated,operation_id,generation,report,observed_at)
             VALUES($1,$2,$3,0,TRUE,$4,$5,'{}',clock_timestamp())
             ON CONFLICT(service_id,gateway_id,boot_id) DO UPDATE SET invalidated=TRUE",
            vec![service.into(), pinned.gateway_id.clone().into(), pinned.boot_id.clone().into(),
                operation.into(), report.generation.into()])).await?;
        tx.commit().await?;
        anyhow::bail!("gateway boot changed; restarted counters cannot prove predecessor drain");
    }
    let changed = tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_gateway_reports(service_id,gateway_id,boot_id,sequence,operation_id,generation,report,observed_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()-$8::bigint * interval '1 millisecond')
         ON CONFLICT(service_id,gateway_id,boot_id) DO UPDATE SET sequence=EXCLUDED.sequence,
             operation_id=EXCLUDED.operation_id,generation=EXCLUDED.generation,report=EXCLUDED.report,
             received_at=clock_timestamp(),observed_at=EXCLUDED.observed_at
         WHERE NOT regional_gateway_reports.invalidated AND regional_gateway_reports.sequence < EXCLUDED.sequence",
        vec![service.into(), report.gateway_id.clone().into(), report.boot_id.clone().into(),
            (report.sequence as i64).into(), operation.into(), report.generation.into(),
            serde_json::to_value(report)?.into(), i64::from(age_ms).into()])).await?.rows_affected() == 1;
    tx.commit().await?;
    Ok(changed)
}

#[derive(Clone, Copy)]
pub(super) enum Gate { Prepared, Adopted, AssignmentsDrained, AdmissionDrained }

/// One hierarchical executor tick. Network observations never hold a lifecycle
/// transaction; each state-changing primitive rechecks ownership independently.
pub(super) async fn reconcile(state: &crate::AppState, db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<()> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT p.generation,r.phase,r.observer_topology FROM regional_service_rollouts r
         JOIN regional_policy_proposals p USING(service_id,operation_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running'
         AND p.step_id=r.region_index || ':publish_policy:' || r.slot_index",
        [service.into(), operation.into()])).await?.context("no running published policy operation")?;
    let generation: i64 = row.try_get("", "generation")?;
    let participants: Vec<Participant> = serde_json::from_str(&row.try_get::<String>("", "observer_topology")?)?;
    super::regional_observers::observe_policy(state, db, service, operation, generation, &participants).await?;
    if row.try_get::<String>("", "phase")? == "activate_policy" {
        super::regional_policy::activate_proposal(db, service, operation, generation).await?;
    } else {
        advance(db, service, operation, generation).await?;
    }
    Ok(())
}

/// Re-evaluate within the same ownership transaction as the cursor transition.
/// Missing, stale or restarted participants block; none are silently dropped.
pub(super) async fn ready(
    db: &impl ConnectionTrait, service: &str, operation: &str, generation: i64, gate: Gate,
) -> Result<bool> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.observer_topology,r.plan,p.step_id FROM regional_service_rollouts r
         JOIN regional_policy_proposals p USING(service_id,operation_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND p.generation=$3",
        vec![service.into(), operation.into(), generation.into()])).await?.context("operation proposal disappeared")?;
    let participants: Vec<Participant> = serde_json::from_str(&row.try_get::<String>("", "observer_topology")?)?;
    let plan: super::regional_plan::Plan = serde_json::from_value(row.try_get("", "plan")?)?;
    let publication = plan.publication(&row.try_get::<String>("", "step_id")?)?;
    let target = publication.region.as_deref();
    let close_id = format!("{}:close_peer_admission:{}", publication.region_index, publication.slot_index);
    let activate_id = plan.step("activate_policy", publication.region_index, publication.slot_index)?.id.clone();
    if matches!(gate, Gate::AssignmentsDrained | Gate::AdmissionDrained) {
        anyhow::ensure!(target.is_some(), "drain gate has no pinned target");
    }
    anyhow::ensure!(!participants.is_empty(), "policy transition has no pinned participants");
    let rows = db.query_all(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT report,COALESCE(observed_at >= (SELECT started_at FROM regional_rollout_items
             WHERE operation_id=$2 AND step_id=$4),FALSE) AS after_close,
             COALESCE(observed_at >= (SELECT completed_at FROM regional_rollout_items
             WHERE operation_id=$2 AND step_id=$5),FALSE) AS after_activate
         FROM regional_gateway_reports WHERE service_id=$1 AND operation_id=$2 AND generation=$3
         AND NOT invalidated AND observed_at <= clock_timestamp() AND observed_at >= clock_timestamp()-interval '5 seconds'
         AND received_at >= clock_timestamp()-interval '5 seconds'",
        vec![service.into(), operation.into(), generation.into(), close_id.into(), activate_id.into()])).await?;
    let reports = rows.into_iter().map(|r| Ok((serde_json::from_value::<Report>(r.try_get("", "report")?)?,
        r.try_get::<bool>("", "after_close")?, r.try_get::<bool>("", "after_activate")?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(participants.iter().all(|p| reports.iter().any(|(r, after_close, after_activate)| {
        r.gateway_id == p.gateway_id && r.boot_id == p.boot_id && r.prepared && match gate {
            Gate::Prepared => true,
            Gate::Adopted => *after_activate && r.adopted,
            Gate::AssignmentsDrained => *after_activate && r.adopted && r.outgoing_target == 0,
            Gate::AdmissionDrained => *after_activate && r.adopted && r.outgoing_target == 0
                && (Some(p.region.as_str()) != target || (*after_close && r.peer_admission_closed && r.local_target == 0)),
        }
    })))
}

/// Advance one evidence gate, never infer a later gate from an earlier ACK.
pub(super) async fn advance(
    db: &sea_orm::DatabaseConnection, service: &str, operation: &str, generation: i64,
) -> Result<bool> {
    let tx = super::service_deploy::try_service_lifecycle_lock(db, service).await?
        .context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.phase,r.plan,r.region_index,r.slot_index FROM regional_service_rollouts r JOIN regional_policy_proposals p USING(service_id,operation_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND p.generation=$3 AND r.status='running'
         AND p.step_id=r.region_index || ':publish_policy:' || r.slot_index FOR UPDATE OF r",
        vec![service.into(), operation.into(), generation.into()])).await?;
    let Some(row) = row else {
        tx.rollback().await?;
        anyhow::bail!("operation does not own policy gate");
    };
    let phase: String = row.try_get("", "phase")?;
    let plan: super::regional_plan::Plan = serde_json::from_value(row.try_get("", "plan")?)?;
    anyhow::ensure!(matches!(plan.version, 2 | 3), "policy gates require a hierarchical plan");
    let item = plan.step(&phase, row.try_get::<i32>("", "region_index")?.try_into()?,
        row.try_get::<i32>("", "slot_index")?.try_into()?)?;
    let gate = match phase.as_str() {
        "wait_policy_prepared" => Gate::Prepared,
        "wait_policy_adopted" => Gate::Adopted,
        "wait_assignments_drained" | "close_peer_admission" => Gate::AssignmentsDrained,
        "wait_admission_drained" => Gate::AdmissionDrained,
        _ => anyhow::bail!("phase is not a policy evidence gate"),
    };
    if phase != "wait_policy_prepared" {
        let active = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM service_active_regional_policies WHERE service_id=$1 AND generation=$2",
            vec![service.into(), generation.into()])).await?.is_some();
        anyhow::ensure!(active, "policy gate no longer owns the active generation");
    }
    if !ready(&tx, service, operation, generation, gate).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    let next = plan.successor(&item.id)?;
    if phase == "close_peer_admission" {
        tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO service_regional_admission_fences(service_id,region,closed_through_generation) VALUES($1,$2,$3)
             ON CONFLICT(service_id,region) DO UPDATE SET closed_through_generation=
             GREATEST(service_regional_admission_fences.closed_through_generation,EXCLUDED.closed_through_generation)",
            vec![service.into(), item.region.clone().context("close admission has no target")?.into(), generation.into()])).await?;
    }
    let revision = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_sets SET version=version+1,updated_at=clock_timestamp() WHERE service_id=$1 RETURNING version",
        [service.into()])).await?.context("discovery set disappeared")?.try_get::<i64>("", "version")?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase=$2,discovery_version=$3,status=CASE WHEN $2='passed' THEN 'passed' ELSE 'running' END,
         completed_at=CASE WHEN $2='passed' THEN clock_timestamp() ELSE NULL END,updated_at=clock_timestamp(),region_index=$4,slot_index=$5
         WHERE operation_id=$1", vec![operation.into(), next.phase.clone().into(), revision.into(),
            i32::try_from(next.region_index)?.into(),i32::try_from(next.slot_index)?.into()])).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::regional_policy::{Gateway, Region};

    #[test]
    fn participant_set_includes_removed_sources_and_rejects_duplicate_or_moved_identities() {
        let old = RegionalPolicy { version: 1, regions: vec![Region {
            region: "eu1".into(), weight: 1, gateways: vec![Gateway {
                id: "old".into(), backend_server_id: "eu-host".into(), url: "https://old.example".into(),
            }],
        }] };
        let mut new = old.clone();
        new.regions[0].gateways[0].id = "new".into();
        let mut participants = vec![Participant {
            gateway_id: "new".into(), region: "eu1".into(), boot_id: uuid::Uuid::new_v4().to_string(),
        }];
        assert!(validate_participants(&participants, &new, Some(&old)).is_err());
        participants.push(Participant { gateway_id: "old".into(), ..participants[0].clone() });
        validate_participants(&participants, &new, Some(&old)).unwrap();
        participants[1].gateway_id = "new".into();
        assert!(validate_participants(&participants, &new, Some(&old)).is_err());
        participants[1].gateway_id = "old".into();
        participants[1].region = "us3".into();
        assert!(validate_participants(&participants, &new, Some(&old)).is_err());
        participants[1].region = "eu1".into();
        participants[1].boot_id = String::new();
        assert!(validate_participants(&participants, &new, Some(&old)).is_err());
    }
}
