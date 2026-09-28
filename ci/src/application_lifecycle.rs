//! Platform-owned updates ask the addressed CI process to retire. No release
//! job is fabricated and no app-lb mutation is performed by this protocol.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;
use crate::dispatch::Dispatcher;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Instance {
    pub deployment_id: String,
    pub backend_server_id: String,
    pub backend_sandbox_id: String,
    pub region: String,
    pub boot_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Retirement {
    pub command_id: String,
    pub operation_id: String,
    pub step_id: String,
    pub service_id: String,
    pub target: Instance,
    pub survivors: Vec<Instance>,
}

impl Retirement {
    pub fn hash(&self) -> Result<String> {
        let mut canonical = serde_json::to_value(self)?;
        canonical.sort_all_objects();
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&canonical)?)))
    }

    fn validate(&self, d: &Dispatcher) -> Result<()> {
        anyhow::ensure!(d.config.application_id.as_deref() == Some(&self.service_id)
            && d.config.managed_deployment.as_deref() == Some(&self.target.deployment_id)
            && d.executor.boot_id() == self.target.boot_id, "retirement does not address this managed process boot");
        for value in [&self.command_id, &self.operation_id, &self.step_id, &self.service_id] {
            anyhow::ensure!(!value.is_empty() && value.len() <= 256, "invalid retirement identity");
        }
        let mut boots = std::collections::HashSet::new();
        for instance in std::iter::once(&self.target).chain(&self.survivors) {
            anyhow::ensure!(boots.insert(instance.boot_id) && !instance.boot_id.is_nil()
                && [&instance.deployment_id, &instance.backend_server_id, &instance.backend_sandbox_id, &instance.region]
                    .iter().all(|s| !s.is_empty() && s.len() <= 256), "invalid or duplicate instance identity");
        }
        anyhow::ensure!(self.survivors.iter().all(|s| s.region != self.target.region
            && s.deployment_id != self.target.deployment_id), "survivor is in the retiring region or deployment");
        Ok(())
    }
}

pub async fn accept(d: &Dispatcher, request: Retirement, hash: &str) -> Result<serde_json::Value> {
    request.validate(d)?;
    anyhow::ensure!(request.hash()? == hash, "retirement request hash mismatch");
    let mut tx = d.store.pool().begin().await?;
    sqlx::query("INSERT INTO ci_application_retirement(command_id,target_boot,request_hash,request,phase) VALUES($1,$2,$3,$4,'pending') ON CONFLICT(command_id) DO NOTHING")
        .bind(&request.command_id).bind(request.target.boot_id).bind(hash).bind(serde_json::to_value(&request)?)
        .execute(&mut *tx).await?;
    let stored: String = sqlx::query_scalar("SELECT request_hash FROM ci_application_retirement WHERE command_id=$1")
        .bind(&request.command_id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(stored == hash, "retirement command changed on replay");
    tx.commit().await?;
    status(d.store.pool(), &request.command_id, hash).await
}

pub async fn status(pool: &PgPool, command: &str, hash: &str) -> Result<serde_json::Value> {
    let (stored, phase, receipt): (String, String, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT request_hash,phase,receipt FROM ci_application_retirement WHERE command_id=$1",
    ).bind(command).fetch_optional(pool).await?.context("retirement command not found")?;
    anyhow::ensure!(stored == hash, "retirement request hash mismatch");
    Ok(serde_json::json!({"commandId":command,"requestHash":hash,
        "status":if phase == "safe" {"safe-to-retire"} else {"pending"},"receipt":receipt}))
}

pub fn spawn(d: Arc<Dispatcher>) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = reconcile(&d).await {
                tracing::warn!(%error, "managed application retirement remains pending");
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
}

pub async fn reconcile(d: &Dispatcher) -> Result<()> {
    let request: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT request FROM ci_application_retirement WHERE target_boot=$1 AND phase<>'safe' ORDER BY created_at LIMIT 1",
    ).bind(d.executor.boot_id()).fetch_optional(d.store.pool()).await?;
    if let Some(request) = request {
        let request: Retirement = serde_json::from_value(request)?;
        request.validate(d)?;
        d.executor.retire_application(&request).await?;
    }
    Ok(())
}
