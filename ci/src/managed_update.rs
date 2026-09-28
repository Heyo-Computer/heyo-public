//! Managed self-release records intent, lets its job/effect permit finish, and
//! observes the normal regional platform operation. It never replaces a VM.
use anyhow::{Context,Result};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use sqlx::Row;
use std::sync::Arc;
use crate::{bus::JobMessage,dispatch::Dispatcher,store::Store};

fn target(d:&Dispatcher)->Result<(&str,String,&str)> {
    anyhow::ensure!(d.config.managed_deployment.is_some(),"managed deployment identity missing");
    let service=d.config.application_id.as_deref().context("managed application identity missing")?;
    anyhow::ensure!(!service.is_empty() && service.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b)),"invalid application identity");
    let base=crate::cd::app_lb_endpoint(d.config.application_orchestrator_url.as_deref().context("application authority missing")?)
        .map_err(anyhow::Error::msg)?;
    let token=d.config.application_lifecycle_token.as_deref().context("application credential missing")?;
    Ok((service,format!("{base}/orchestration/services/{service}/managed-updates"),token))
}

pub async fn request(d:&Dispatcher,msg:&JobMessage,step:&str,archive:&str)->Result<String> {
    let (service,_,_)=target(d)?;
    let run=d.store.get_run(&msg.run_id).await?.context("run missing")?;
    anyhow::ensure!(crate::repos::same_repo(d.config.controller_repository.as_deref().context("controller repository missing")?,&run.repo_url),
        "repository cannot replace this managed CI app");
    let release=crate::release::get(&d.store,&msg.run_id).await.map_err(anyhow::Error::msg)?
        .filter(|r|r.status=="published").context("managed self-release requires a confirmed published release")?;
    let sha=release.prepared.release_sha;
    let digest:String=sqlx::query_scalar("SELECT a.archive_sha256 FROM ci_service_archive a JOIN ci_step s ON s.id=a.step_id JOIN ci_job j ON j.id=a.job_id
        WHERE a.run_id=$1 AND a.archive_id=$2 AND a.sha=$3 AND a.orchestrator_url=$4 AND s.status='success'
        AND (j.status='success' OR j.id=$5) AND a.archive_sha256 IS NOT NULL ORDER BY a.created_at DESC LIMIT 1")
        .bind(&msg.run_id).bind(archive).bind(&sha).bind(d.config.application_orchestrator_url.as_deref().unwrap().trim_end_matches('/'))
        .bind(&msg.job_id).fetch_optional(d.store.pool()).await?.context("archive was not finalized by this release at this authority")?;
    let id=format!("ci-managed-{}",hex::encode(Sha256::digest(step.as_bytes())));
    let command=json!({"operationId":id,"archiveId":archive,"archiveSha256":digest,"runtimeRevision":sha});
    let mut tx=d.store.pool().begin().await?;
    let running:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE j.id=$1 AND j.status='running' AND r.status<>'cancelled')")
        .bind(&msg.job_id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(running,"release job is no longer running");
    sqlx::query("INSERT INTO ci_managed_update(operation_id,step_id,run_id,job_id,service_id,request) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(operation_id) DO NOTHING")
        .bind(&id).bind(step).bind(&msg.run_id).bind(&msg.job_id).bind(service).bind(&command).execute(&mut *tx).await?;
    let saved:Value=sqlx::query_scalar("SELECT request FROM ci_managed_update WHERE operation_id=$1").bind(&id).fetch_one(&mut *tx).await?;
    anyhow::ensure!(saved==command,"managed self-release intent changed on replay");
    tx.commit().await?;
    Ok(format!("[ci] managed update {id} recorded; release result waits for both regional identities and platform bake\n"))
}

fn targets(observation:&Value,command:&Value,service:&str)->Result<Value> {
    anyhow::ensure!(observation["operationId"]==command["operationId"] && observation["serviceId"]==service
        && observation["request"]==*command,"managed update receipt identity mismatch");
    let values=observation["targets"].as_array().context("managed target set missing")?;
    let mut ids=std::collections::HashSet::new(); let mut regions=std::collections::HashSet::new();
    let mut result=Vec::new();
    for value in values {
        let id=value["deploymentId"].as_str().filter(|s|!s.is_empty()).context("candidate ID missing")?;
        let region=value["region"].as_str().filter(|s|!s.is_empty()).context("candidate region missing")?;
        anyhow::ensure!(ids.insert(id) && value["revision"]==command["runtimeRevision"],"candidate identity mismatch");
        regions.insert(region);
        result.push(json!({"deploymentId":id,"region":region,"revision":value["revision"]}));
    }
    anyhow::ensure!(regions.len()>=2,"managed CI self-release requires at least two regional targets");
    Ok(json!(result))
}

pub(crate) async fn reconcile(d:&Dispatcher)->Result<()> {
    if d.config.managed_deployment.is_none() || !d.executor.is_owner().await.map_err(anyhow::Error::msg)? {return Ok(())}
    let row=sqlx::query("SELECT u.*,j.status AS job_status,r.status AS run_status FROM ci_managed_update u
        JOIN ci_job j ON j.id=u.job_id JOIN ci_run r ON r.id=u.run_id WHERE u.result IS NULL ORDER BY u.created_at LIMIT 1")
        .fetch_optional(d.store.pool()).await?;
    let Some(row)=row else {return Ok(())};
    let job:String=row.get("job_status");
    if !matches!(job.as_str(),"success"|"failure"|"cancelled"|"skipped") {return Ok(())}
    let _permit=d.executor.effect_permit().await.map_err(anyhow::Error::msg)?;
    let (service,url,token)=target(d)?;
    anyhow::ensure!(row.get::<String,_>("service_id")==service,"managed service changed");
    let id:String=row.get("operation_id"); let command:Value=row.get("request");
    let saved:Option<Value>=row.get("targets");
    if !row.get::<bool,_>("attempted") {
        let mut tx=d.store.pool().begin().await?;
        let run:String=sqlx::query_scalar("SELECT status FROM ci_run WHERE id=$1 FOR UPDATE")
            .bind(row.get::<String,_>("run_id")).fetch_one(&mut *tx).await?;
        if job!="success" || matches!(run.as_str(),"failure"|"cancelled") {
            sqlx::query("UPDATE ci_managed_update SET result='failed' WHERE operation_id=$1 AND NOT attempted")
                .bind(&id).execute(&mut *tx).await?;
            Store::roll_up_run_in(&mut tx,&row.get::<String,_>("run_id")).await?;
            tx.commit().await?; return Ok(());
        }
        sqlx::query("UPDATE ci_managed_update SET attempted=TRUE WHERE operation_id=$1").bind(&id).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never())
        .timeout(std::time::Duration::from_secs(15)).build()?;
    // The operation ID is durable before any POST. An uncertain result may only
    // replay this exact command; Orchestrator deduplicates it transactionally.
    let response=if saved.is_none() {
        client.post(&url).bearer_auth(token).json(&command).send().await?
    } else {client.get(format!("{url}/{id}")).bearer_auth(token).send().await?};
    anyhow::ensure!(response.status().is_success(),"managed platform operation remains unresolved");
    let observation:Value=response.json().await?;
    let pinned=targets(&observation,&command,service)?;
    if let Some(saved)=saved {anyhow::ensure!(saved==pinned,"managed target set changed");}
    let result=match observation["status"].as_str() {
        Some("passed") if observation["verified"]==true => {
            anyhow::ensure!(observation["targets"].as_array().unwrap().iter().all(|t|t["bootId"].as_str()
                .and_then(|s|s.parse::<uuid::Uuid>().ok()).is_some_and(|id|!id.is_nil())
                && ["backendServerId","backendSandboxId"].iter().all(|k|t[*k].as_str().is_some_and(|s|!s.is_empty()))),
                "completed regional runtime/boot attestation missing");
            Some("passed")
        }
        Some("rolled_back")=>Some("failed"),
        _=>None,
    };
    let mut tx=d.store.pool().begin().await?;
    // The run stays pending until this same transaction records verified
    // completion. Cancellation remains sticky in Store::roll_up_run_in.
    sqlx::query("SELECT id FROM ci_run WHERE id=$1 FOR UPDATE").bind(row.get::<String,_>("run_id")).execute(&mut *tx).await?;
    sqlx::query("UPDATE ci_managed_update SET targets=$2,observation=$3,result=$4 WHERE operation_id=$1 AND result IS NULL")
        .bind(&id).bind(pinned).bind(observation).bind(result).execute(&mut *tx).await?;
    Store::roll_up_run_in(&mut tx,&row.get::<String,_>("run_id")).await?;
    tx.commit().await?;
    Ok(())
}

pub fn spawn(d:Arc<Dispatcher>) {
    tokio::spawn(async move {loop {
        if let Err(error)=reconcile(&d).await {tracing::warn!(%error,"managed CI update remains unresolved");}
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }});
}
