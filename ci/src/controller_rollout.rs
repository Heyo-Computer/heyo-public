//! Deferred self-deployment. The requesting job finishes before replacement;
//! the replacement controller reconciles the same durable operation at startup.
use crate::{artifacts::StoredArtifact, bus::JobMessage, dispatch::Dispatcher, store::Store};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{io::Read, sync::{Arc, LazyLock}, time::Duration};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Request {
    deployment: String,
    base_url: String,
    public_url: String,
    artifact: String,
    sha: String,
    binary_sha256: String,
    previous_vm: String,
    previous_etag: String,
    desired_etag: String,
}

pub fn binary_sha256() -> Option<&'static str> {
    static HASH: LazyLock<Option<String>> = LazyLock::new(|| {
        let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash).ok()?;
        Some(hex::encode(hash.finalize()))
    });
    HASH.as_deref()
}

fn etag(spec: &Value) -> String {
    let mut spec = spec.clone();
    // Match app-lb independently of serde_json's transitive preserve_order feature.
    spec.sort_all_objects();
    format!("\"{}\"", hex::encode(Sha256::digest(serde_json::to_vec(&spec).expect("JSON serializes"))))
}

fn artifact_identity(bytes: &[u8], sha: &str) -> Result<String, String> {
    let mut revision = None;
    let mut binary = None;
    let mut checksums = None;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let name = path.to_str().ok_or("non-UTF8 artifact path")?;
        if !matches!(name, "dist/REVISION" | "dist/ci" | "dist/SHA256SUMS") { continue; }
        if !entry.header().entry_type().is_file() || entry.size() > 256 * 1024 * 1024 {
            return Err("invalid controller artifact entry".into());
        }
        if name == "dist/ci" {
            if binary.is_some() { return Err("duplicate ci executable".into()); }
            let mut hash = Sha256::new();
            std::io::copy(&mut entry, &mut hash).map_err(|e| e.to_string())?;
            binary = Some(hex::encode(hash.finalize()));
        } else {
            if entry.size() > 1024 * 1024 { return Err("oversized artifact metadata".into()); }
            let mut text = String::new();
            entry.read_to_string(&mut text).map_err(|e| e.to_string())?;
            let slot = if name == "dist/REVISION" { &mut revision } else { &mut checksums };
            if slot.replace(text).is_some() { return Err("duplicate artifact metadata".into()); }
        }
    }
    if revision.as_deref().map(str::trim) != Some(sha) { return Err("artifact REVISION differs from confirmed release".into()); }
    let binary = binary.ok_or("artifact has no ci executable")?;
    let expected = format!("{binary}  ci");
    if !checksums.ok_or("artifact has no SHA256SUMS")?.lines().any(|line| line == expected) {
        return Err("artifact executable checksum mismatch".into());
    }
    Ok(binary)
}

fn desired_spec(mut spec: Value, digest: &str, sha: &str) -> Result<Value, String> {
    let broker = spec["vm"]["env_vars"]["CI_NATS_URL"].as_str()
        .ok_or("controller requires an explicit external CI_NATS_URL before self-deployment")?;
    let url = reqwest::Url::parse(broker).map_err(|_| "invalid controller CI_NATS_URL")?;
    let host = url.host_str().ok_or("CI_NATS_URL has no host")?;
    let local = host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost")
        || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    if !matches!(url.scheme(), "nats" | "tls") || local {
        return Err("controller self-deployment requires an independently managed NATS service, not a loopback broker".into());
    }
    if spec["vm"]["driver"] != "firecracker" || !spec["vm"]["workspace"].is_object()
        || spec["scaling"]["max_replicas"] != 1 || spec["scaling"]["min_replicas"] != 1
        || spec["scaling"]["warm_pool"].as_u64().unwrap_or(0) != 0 {
        return Err("self-deployment requires one Firecracker controller with persistent workspace".into());
    }
    let mounts = spec["vm"]["mounts"].as_array_mut().ok_or("controller has no artifact mounts")?;
    let matching: Vec<_> = mounts.iter_mut().filter(|m| m["path"] == "/opt/ci-release").collect();
    if matching.len() != 1 { return Err("controller must have exactly one /opt/ci-release mount".into()); }
    let mount = matching.into_iter().next().unwrap();
    if mount["read_only"] != true || mount["strip_components"] != 1 {
        return Err("controller release mount must be read-only with strip_components=1".into());
    }
    mount["ref"] = json!(digest);
    mount["digest"] = json!(digest);
    spec["vm"]["env_vars"]["CI_EXPECTED_SHA"] = json!(sha);
    let boot = base64::engine::general_purpose::STANDARD.encode(include_str!("../deploy/start-artifact.sh"));
    spec["vm"]["start_command"] = json!(format!("echo {boot} | base64 -d > /tmp/ci-start-artifact.sh; setsid nohup bash /tmp/ci-start-artifact.sh /opt/ci-release /opt/ci /workspace/ci-state </dev/null >/workspace/ci-state.log 2>&1 &"));
    Ok(spec)
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())
}

fn success_message(request: &Request) -> String {
    format!(
        "Deployed CI controller `{}` at {} from revision {}; verified the exact executable through {}/healthz; submissions reopened.",
        request.deployment,
        request.public_url.trim_end_matches('/'),
        request.sha,
        request.public_url.trim_end_matches('/'),
    )
}

fn target(d: &Dispatcher) -> Result<(&str, &str, &str), String> {
    let id = d.config.controller_deployment.as_deref().ok_or("CI_CONTROLLER_DEPLOYMENT is not configured")?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("invalid CI_CONTROLLER_DEPLOYMENT".into());
    }
    let base = d.config.controller_app_lb_url.as_deref().ok_or("CI_CONTROLLER_APP_LB_URL is not configured")?;
    crate::cd::app_lb_endpoint(base)?;
    crate::cd::app_lb_endpoint(&d.config.public_url)?;
    let token = d.config.controller_app_lb_token.as_deref().filter(|s| !s.is_empty()).ok_or("CI_CONTROLLER_APP_LB_TOKEN is not configured")?;
    Ok((id, base, token))
}

async fn snapshot(d: &Dispatcher) -> Result<Value, String> {
    let (id, base, token) = target(d)?;
    snapshot_at(base, id, token).await
}

async fn snapshot_at(base: &str, id: &str, token: &str) -> Result<Value, String> {
    let response = client()?.get(format!("{base}/deployments/{id}")).bearer_auth(token).send().await
        .map_err(|_| "could not read controller deployment")?.error_for_status().map_err(|_| "controller deployment read refused")?;
    let tag = response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok())
        .ok_or("app-lb must support conditional deployment updates before enabling self-deployment")?.to_owned();
    let body: Value = response.json().await.map_err(|_| "invalid deployment response")?;
    if tag != etag(&body["spec"]) { return Err("app-lb deployment ETag does not match its spec".into()); }
    Ok(body)
}

/// Persist intent, not credentials or a running deploy job. Publication and
/// merge must already have succeeded; the run remains running after this job.
pub async fn request(d: &Dispatcher, msg: &JobMessage, step: &str, artifact: &str, workflow: Option<&str>) -> Result<String, String> {
    let (deployment, base, _) = target(d)?;
    let (application, _, _) = application_target(d)?;
    let run = d.store.get_run(&msg.run_id).await.map_err(|e| e.to_string())?.ok_or("missing run")?;
    let repository = d.config.controller_repository.as_deref().ok_or("CI_CONTROLLER_REPOSITORY is not configured")?;
    if !crate::repos::same_repo(repository, &run.repo_url) { return Err("this repository may not replace the CI controller".into()); }
    let release = crate::release::get(&d.store, &msg.run_id).await?
        .filter(|r| r.status == "published").ok_or("controller deployment requires a confirmed merged release")?;
    let sha = release.prepared.release_sha;
    if sha != run.sha { return Err("build must match the exact merged revision; version-bump releases must rebuild first".into()); }
    let stored = if let Some(workflow) = workflow {
        crate::submission::artifact(&d.store, &msg.run_id, workflow, artifact, None).await?
    } else {
        let row = sqlx::query("SELECT a.* FROM ci_artifact a JOIN ci_job j ON j.id=a.job_id WHERE a.run_id=$1 AND a.name=$2 AND a.sink='artifacts' AND j.status='success' ORDER BY a.created_at DESC LIMIT 1")
            .bind(&msg.run_id).bind(artifact).fetch_optional(d.store.pool()).await.map_err(|e| e.to_string())?.ok_or("no successfully built controller artifact")?;
        StoredArtifact { sink: "artifacts", digest: row.get("digest"),
            size_bytes: row.get::<i64,_>("size_bytes").try_into().map_err(|_| "invalid artifact size")?,
            uri: row.get("uri"), public_url: None }
    };
    if stored.sink != "artifacts" { return Err("controller update requires the HTTP artifact sink".into()); }
    let digest = stored.digest.clone().ok_or("artifact omitted digest")?;
    let id = format!("ci-controller-{}", hex::encode(Sha256::digest(step.as_bytes())));
    if let Some(existing) = sqlx::query_scalar::<_, Value>("SELECT request FROM ci_controller_rollout WHERE id=$1")
        .bind(&id).fetch_optional(d.store.pool()).await.map_err(|e| e.to_string())? {
        let existing: Request = serde_json::from_value(existing).map_err(|e| e.to_string())?;
        if existing.sha != sha || existing.artifact != digest || existing.deployment != deployment || existing.base_url != base {
            return Err("controller rollout request changed on replay".into());
        }
        accept_application_update(d, &id).await?;
        return Ok(format!("[ci] controller deployment {id} is durably recorded\n"));
    }
    if !(1..=256 * 1024 * 1024).contains(&stored.size_bytes) { return Err("controller artifact exceeds verification budget".into()); }
    let bytes = d.artifacts.get(&stored).await.map_err(|e| e.to_string())?;
    if hex::encode(Sha256::digest(&bytes)) != digest { return Err("controller artifact digest mismatch".into()); }
    let binary_sha256 = artifact_identity(&bytes, &sha)?;
    let current = snapshot(d).await?;
    let spec = &current["spec"];
    if spec["vm"]["env_vars"]["CI_PUBLIC_URL"].as_str().map(|s| s.trim_end_matches('/')) != Some(d.config.public_url.trim_end_matches('/'))
        || spec["vm"]["env_vars"]["CI_CONTROLLER_DEPLOYMENT"] != deployment {
        return Err("registered deployment is not this controller".into());
    }
    let artifacts = d.config.artifacts.as_ref().ok_or("controller update requires the HTTP artifact sink")?;
    let mount = spec["vm"]["mounts"].as_array().and_then(|m| m.iter().find(|m| m["path"] == "/opt/ci-release"))
        .ok_or("missing controller release mount")?;
    if mount["store"].as_str().map(|s| s.trim_end_matches('/')) != Some(artifacts.url.trim_end_matches('/')) {
        return Err("controller mount uses a different artifact store".into());
    }
    let vms = current["vms"].as_array().ok_or("missing controller VM inventory")?;
    if vms.len() != 1 || vms[0]["healthy"] != true || vms[0]["draining"] != false { return Err("controller must have exactly one healthy, non-draining VM".into()); }
    let wanted = desired_spec(spec.clone(), &digest, &sha)?;
    let request = Request { deployment: deployment.into(), base_url: base.into(), public_url: d.config.public_url.clone(),
        artifact: digest, sha: sha.clone(), binary_sha256, previous_vm: vms[0]["sandbox_id"].as_str().ok_or("missing VM identity")?.into(),
        previous_etag: etag(spec), desired_etag: etag(&wanted) };
    let value = serde_json::to_value(&request).map_err(|e| e.to_string())?;
    let hash = etag(&value);
    let mut tx = d.store.pool().begin().await.map_err(|e| e.to_string())?;
    let inserted = sqlx::query("INSERT INTO ci_service_deployment(id,step_id,run_id,job_id,service_id,request_hash,status,phase,sha,git_ref) SELECT $1,s.id,r.id,j.id,$3,$4,'running','pending',$5,rel.git_ref FROM ci_step s JOIN ci_job j ON j.id=s.job_id JOIN ci_run r ON r.id=j.run_id JOIN ci_release rel ON rel.run_id=r.id AND rel.status='published' WHERE s.id=$2 AND j.status='running' AND r.status<>'cancelled'")
        .bind(&id).bind(step).bind(deployment).bind(hash).bind(&sha).execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if inserted != 1 { return Err("requesting job is no longer running".into()); }
    sqlx::query("INSERT INTO ci_controller_rollout(id,request,phase,application_id) VALUES($1,$2,'prepared',$3)")
        .bind(&id).bind(value).bind(application).execute(&mut *tx).await.map_err(|e| format!("another controller rollout is active, or intent could not be recorded: {e}"))?;
    Store::add_service_deployment_event(&mut tx, &id).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    accept_application_update(d, &id).await?;
    Ok(format!("[ci] controller deployment {id} recorded; run waits for drain, replacement, and public revision verification\n"))
}

fn application_target(d: &Dispatcher) -> Result<(&str, &str, &str), String> {
    let application = d.config.application_id.as_deref().filter(|id| !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)))
        .ok_or("CI_APPLICATION_ID must be configured")?;
    let base = d.config.application_orchestrator_url.as_deref().ok_or("CI_APPLICATION_ORCHESTRATOR_URL must be configured")?;
    crate::cd::app_lb_endpoint(base)?;
    let token = d.config.application_lifecycle_token.as_deref().filter(|t| !t.is_empty())
        .ok_or("CI_APPLICATION_LIFECYCLE_TOKEN must be configured through HeyoSecret")?;
    Ok((application, base, token))
}

async fn accept_application_update(d: &Dispatcher, id: &str) -> Result<(), String> {
    let (application, base, token) = application_target(d)?;
    let intent = application_status(d, id).await?;
    let response = client()?.post(format!("{base}/orchestration/services/{application}/updates"))
        .bearer_auth(token).json(&json!({"operationId":id,"intentHash":intent["intentHash"]}))
        .send().await.map_err(|_| "application update acceptance is unknown; replay the same release step")?;
    if !response.status().is_success() { return Err("application authority refused the update; controller unchanged".into()); }
    let receipt: Value = response.json().await.map_err(|_| "invalid application update receipt")?;
    if receipt["operationId"] != id || receipt["intentHash"] != intent["intentHash"] {
        return Err("application authority returned a different update receipt".into());
    }
    Ok(())
}

pub async fn application_status(d: &Dispatcher, id: &str) -> Result<Value, String> {
    let row = sqlx::query("SELECT c.request,c.application_id,c.phase,s.run_id,s.status,s.message FROM ci_controller_rollout c JOIN ci_service_deployment s ON s.id=c.id WHERE c.id=$1")
        .bind(id).fetch_optional(d.store.pool()).await.map_err(|e| e.to_string())?.ok_or("unknown controller update")?;
    let request: Value = row.get("request");
    Ok(json!({"operationId":id,"applicationId":row.get::<Option<String>,_>("application_id"),
        "intentHash":etag(&request),"deploymentId":request["deployment"],"authority":request["base_url"],
        "targetRevision":request["sha"],"artifactDigest":request["artifact"],
        "runId":row.get::<String,_>("run_id"),"status":row.get::<String,_>("status"),
        "phase":row.get::<String,_>("phase"),"message":row.get::<Option<String>,_>("message")}))
}

pub async fn activate_application_update(d: &Dispatcher, id: &str, hash: &str) -> Result<(), String> {
    let (application, _, _) = application_target(d)?;
    let mut tx = d.store.pool().begin().await.map_err(|e| e.to_string())?;
    // Use the same run-before-operation lock order as cancellation/submission.
    let run: String = sqlx::query_scalar("SELECT run_id FROM ci_service_deployment WHERE id=$1")
        .bind(id).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    let run_status: String = sqlx::query_scalar("SELECT status FROM ci_run WHERE id=$1 FOR UPDATE")
        .bind(run).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    let row = sqlx::query("SELECT request,application_id,phase,activation_hash FROM ci_controller_rollout WHERE id=$1 FOR UPDATE")
        .bind(id).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    if row.get::<Option<String>,_>("application_id").as_deref() != Some(application)
        || etag(&row.get::<Value,_>("request")) != hash { return Err("application intent identity mismatch".into()); }
    if let Some(previous) = row.get::<Option<String>,_>("activation_hash") {
        if previous != hash { return Err("application activation changed on replay".into()); }
        return Ok(());
    }
    if row.get::<String,_>("phase") != "prepared" || matches!(run_status.as_str(), "cancelled" | "failure") {
        return Err("application update is no longer eligible for activation".into());
    }
    sqlx::query("UPDATE ci_controller_rollout SET phase='pending',activation_hash=$2,updated_at=now() WHERE id=$1")
        .bind(id).bind(hash).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

async fn finish(d: &Dispatcher, id: &str, run: &str, passed: bool, message: &str) -> Result<(), String> {
    let mut tx = d.store.pool().begin().await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE ci_service_deployment SET status=$2,phase='complete',message=$3,updated_at=now() WHERE id=$1")
        .bind(id).bind(if passed { "passed" } else { "failed" }).bind(message).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE ci_controller_rollout SET phase='complete',updated_at=now() WHERE id=$1")
        .bind(id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    Store::add_service_deployment_event(&mut tx, id).await.map_err(|e| e.to_string())?;
    Store::roll_up_run_in(&mut tx, run).await.map_err(|e| e.to_string())?;
    // Completion and opening normal execution are one durable outcome. A crash
    // between separate commits would leave an owner restricted to a finished
    // operation that the reconciler no longer selects.
    sqlx::query("UPDATE ci_executor_owner SET continuation_operation_id=NULL WHERE singleton=TRUE AND boot_id=$1 AND continuation_operation_id=$2")
        .bind(d.executor.boot_id()).bind(id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn mark_submitting(store: &Store, id: &str, run: &str) -> Result<bool, String> {
    let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
    // Serialize the first externally visible attempt against cancellation.
    // Cancellation after this commit cannot retract a remote PUT.
    let status: String = sqlx::query_scalar("SELECT status FROM ci_run WHERE id=$1 FOR UPDATE")
        .bind(run).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    let phase: String = sqlx::query_scalar("SELECT phase FROM ci_controller_rollout WHERE id=$1 FOR UPDATE")
        .bind(id).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    if phase == "quiesced" && matches!(status.as_str(), "cancelled" | "failure") { return Ok(false); }
    if !matches!(phase.as_str(), "quiesced" | "submitting") { return Err("rollout attempt is not quiesced".into()); }
    sqlx::query("UPDATE ci_controller_rollout SET phase='submitting',updated_at=now() WHERE id=$1")
        .bind(id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(true)
}

async fn reconcile(d: &Dispatcher) -> Result<(), String> {
    // A standby is not a rollout failure. In particular it must not overwrite
    // the owner's progress with submission_unknown on every polling tick.
    if !d.executor.is_owner().await? { return Ok(()); }
    let row = sqlx::query("SELECT c.*,s.run_id,r.status AS run_status FROM ci_controller_rollout c JOIN ci_service_deployment s ON s.id=c.id JOIN ci_run r ON r.id=s.run_id WHERE c.phase<>'complete'")
        .fetch_optional(d.store.pool()).await.map_err(|e| e.to_string())?;
    let Some(row) = row else { return Ok(()) };
    let id: String = row.get("id");
    let phase: String = row.get("phase");
    let run: String = row.get("run_id");
    let request: Request = serde_json::from_value(row.get("request")).map_err(|e| e.to_string())?;
    let continuation = matches!(phase.as_str(), "quiesced" | "submitting" | "verifying");
    let permit = d.executor.effect_permit_for(continuation.then_some(id.as_str())).await?;
    let (local_deployment, local_base, token) = target(d)?;
    let deployment = request.deployment.as_str();
    let base = request.base_url.as_str();
    let attempted = matches!(phase.as_str(), "submitting" | "verifying");
    if !attempted && matches!(row.get::<String,_>("run_status").as_str(), "cancelled" | "failure") {
        return finish(d, &id, &run, false, "Release cancelled or failed before deployment; controller unchanged.").await;
    }
    let created: chrono::DateTime<chrono::Utc> = row.get("created_at");
    if !attempted && (chrono::Utc::now() - created).to_std().unwrap_or_default() > d.config.max_job_duration {
        return finish(d, &id, &run, false, "Controller drain exceeded CI_MAX_JOB_SECONDS; no update submitted and submissions reopened.").await;
    }
    match phase.as_str() {
        "prepared" => return Ok(()),
        "pending" => {
            d.lifecycle.close_admission(&d.store, &id).await?;
            d.store.update_service_deployment(&id, "running", Some("draining"), Some("Submissions closed; waiting for jobs and leases to finish."), None).await.map_err(|e| e.to_string())?;
            return Ok(());
        }
        "draining" => { d.lifecycle.quiesce(&d.store, &id).await?; return Ok(()); }
        "quiesced" | "submitting" | "verifying" => {}
        _ => return Err("invalid controller rollout phase".into()),
    }
    let current = snapshot_at(base, deployment, token).await?;
    let tag = etag(&current["spec"]);
    if tag == request.previous_etag && tag != request.desired_etag && phase != "verifying" {
        if permit.continuation_operation_id().is_none()
            && local_deployment == deployment && local_base == base {
            // Do not queue the exclusive fence behind our own pass permit.
            drop(permit);
            let successor = d.executor.ready_successor().await?
                .ok_or("no ready surviving executor replica; self replacement remains safely paused")?;
            let fence = d.executor.handoff_fence().await?;
            d.lifecycle.verify_handoff_quiesced(&d.store, &id).await?;
            fence.transfer_to(successor, crate::executor::VerifiedDurableContinuation::ContinueExactOperation(&id)).await?;
            return Ok(());
        }
        // CAS makes retry after a lost response safe: only a writer observing
        // the original spec may change it. Also reject a rolled-back/new VM.
        let original = current["vms"].as_array().is_some_and(|v| v.len() == 1 && v[0]["sandbox_id"] == request.previous_vm && v[0]["healthy"] == true);
        if !original { return Err("original controller identity changed; refusing a blind deployment retry".into()); }
        let wanted = desired_spec(current["spec"].clone(), &request.artifact, &request.sha)?;
        if etag(&wanted) != request.desired_etag { return Err("controller update no longer matches recorded intent".into()); }
        if !mark_submitting(&d.store, &id, &run).await? {
            return finish(d, &id, &run, false, "Release cancelled before the deployment attempt; controller unchanged.").await;
        }
        d.store.update_service_deployment(&id, "submitting", Some("replacing"), Some("All work drained; app-lb is replacing the controller and preserving its workspace."), None).await.map_err(|e| e.to_string())?;
        let result = client()?.put(format!("{base}/deployments/{deployment}")).bearer_auth(token)
            .header(reqwest::header::IF_MATCH, &request.previous_etag).json(&wanted).send().await;
        if let Ok(response) = result {
            if response.status() == reqwest::StatusCode::PRECONDITION_FAILED {
                return Err("controller spec changed concurrently; reconciling without overwriting it".into());
            }
        }
        // Never infer failure or success from PUT transport: this process may
        // disappear here. The next pass (including after restart) observes GET.
        return Ok(());
    }
    if tag != request.desired_etag {
        if !attempted { return finish(d, &id, &run, false, "Controller configuration changed before deployment; no update submitted.").await; }
        return Err("controller specification diverged after submission; reconciliation required".into());
    }
    sqlx::query("UPDATE ci_controller_rollout SET phase='verifying',updated_at=now() WHERE id=$1 AND phase<>'verifying'")
        .bind(&id).execute(d.store.pool()).await.map_err(|e| e.to_string())?;
    let ready = current["vms"].as_array().is_some_and(|v| v.len() == 1
        && (v[0]["sandbox_id"] != request.previous_vm || request.previous_etag == request.desired_etag)
        && v[0]["healthy"] == true && v[0]["draining"] == false);
    if !ready || current["workspace"]["phase"] != "idle" || current["workspace"]["push_pending"] != false { return Ok(()); }
    let response = client()?.get(format!("{}/healthz", request.public_url.trim_end_matches('/'))).send().await
        .map_err(|_| "replacement public health is unreachable")?;
    let headers = response.headers();
    if !response.status().is_success()
        || headers.get("x-ci-revision").and_then(|h| h.to_str().ok()) != Some(request.sha.as_str())
        || headers.get("x-ci-binary-sha256").and_then(|h| h.to_str().ok()) != Some(request.binary_sha256.as_str()) {
        return Err("public health does not identify the exact replacement binary".into());
    }
    finish(d, &id, &run, true, &success_message(&request)).await
}

pub fn spawn(d: Arc<Dispatcher>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(error) = reconcile(&d).await {
                tracing::warn!(%error, "controller deployment reconciliation is blocked");
                if let Ok(Some(id)) = sqlx::query_scalar::<_, String>("SELECT id FROM ci_controller_rollout WHERE phase<>'complete'")
                    .fetch_optional(d.store.pool()).await {
                    if let Err(e) = d.store.update_service_deployment(&id, "submission_unknown", Some("reconciling"), Some("Deployment is unresolved; the run is not marked successful. See the reconciliation error."), Some(&error)).await {
                        tracing::warn!(error = %e, "could not persist controller reconciliation status");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lifecycle::Lifecycle, store::{JobStatus, RunStatus}};

    #[test]
    fn etag_sorts_nested_objects_but_preserves_array_order() {
        let input: Value = serde_json::from_str(r#"{"z":[{"z":2,"a":1},3],"a":{"z":4,"a":5}}"#).unwrap();
        let canonical = br#"{"a":{"a":5,"z":4},"z":[{"a":1,"z":2},3]}"#;
        let expected = format!("\"{:x}\"", Sha256::digest(canonical));
        assert_eq!(etag(&input), expected);
        let mut reordered = input;
        reordered["z"].as_array_mut().unwrap().reverse();
        assert_ne!(etag(&reordered), expected);
    }

    fn package(revision: &str, sum: &str, duplicate: bool) -> Vec<u8> {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let mut files = vec![("dist/REVISION", revision.as_bytes()), ("dist/ci", b"abc".as_slice()), ("dist/SHA256SUMS", sum.as_bytes())];
        if duplicate { files.push(("dist/ci", b"different")); }
        for (path, bytes) in files {
            let mut h = tar::Header::new_gnu(); h.set_size(bytes.len() as u64); h.set_mode(0o755); h.set_cksum();
            tar.append_data(&mut h, path, bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn deployment_artifact_proves_revision_and_actual_executable() {
        // Independent published SHA-256 test vector, not generated by the
        // verifier or extracted from its output.
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let sums = format!("{hash}  ci\n");
        assert_eq!(artifact_identity(&package("source\n", &sums, false), "source").unwrap(), hash);
        assert!(artifact_identity(&package("different\n", &sums, false), "source").is_err());
        assert!(artifact_identity(&package("source\n", "bad  ci\n", false), "source").is_err());
        assert!(artifact_identity(&package("source\n", &sums, true), "source").is_err());
    }

    #[test]
    fn completed_deployment_message_identifies_what_and_where() {
        let request = Request {
            deployment: "ci-eu1".into(), base_url: "https://admin.eu1.example".into(),
            public_url: "https://ci.eu1.example/".into(), artifact: "a".repeat(64),
            sha: "8f3dc6d".into(), binary_sha256: "b".repeat(64), previous_vm: "old".into(),
            previous_etag: "before".into(), desired_etag: "after".into(),
        };
        assert_eq!(success_message(&request),
            "Deployed CI controller `ci-eu1` at https://ci.eu1.example from revision 8f3dc6d; verified the exact executable through https://ci.eu1.example/healthz; submissions reopened.");
    }

    fn spec() -> Value {
        json!({"id":"ci-test","routes":[{"host":"ci.example.test"}],
            "scaling":{"min_replicas":1,"max_replicas":1,"warm_pool":0},
            "vm":{"driver":"firecracker","workspace":{"ref":"preserve-me"},
                "env_vars":{"OTHER":"unchanged","CI_EXPECTED_SHA":"old","CI_NATS_URL":"nats://broker.internal:4222"},
                "mounts":[{"path":"/opt/data","ref":"leave-me"},
                    {"path":"/opt/ci-release","ref":"old","digest":"old","read_only":true,"strip_components":1}]}})
    }

    #[test]
    fn promotion_preserves_broker_and_state_and_replaces_legacy_launcher() {
        let original = spec();
        let mut expected = original.clone();
        expected["vm"]["mounts"][1]["ref"] = json!("blob");
        expected["vm"]["mounts"][1]["digest"] = json!("blob");
        expected["vm"]["env_vars"]["CI_EXPECTED_SHA"] = json!("revision");
        let mut actual = desired_spec(original.clone(), "blob", "revision").unwrap();
        let command = actual["vm"].as_object_mut().unwrap().remove("start_command").unwrap();
        let encoded = command.as_str().unwrap().split_whitespace().nth(1).unwrap();
        let boot = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded).unwrap()).unwrap();
        assert!(boot.contains("exec ./ci"));
        assert!(!boot.contains("exec bash \"$runtime/start.sh\""));
        assert_eq!(actual, expected);
        for broker in [Value::Null, json!("nats://localhost:4222"), json!("nats://127.0.0.2:4222"), json!("nats://[::1]:4222"), json!("http://broker.internal:4222")] {
            let mut invalid = original.clone(); invalid["vm"]["env_vars"]["CI_NATS_URL"] = broker;
            assert!(desired_spec(invalid, "blob", "revision").is_err());
        }
        for pointer in ["/scaling/max_replicas", "/scaling/min_replicas", "/scaling/warm_pool"] {
            let mut invalid = original.clone(); *invalid.pointer_mut(pointer).unwrap() = json!(2);
            assert!(desired_spec(invalid, "blob", "revision").is_err());
        }
        let mut invalid = original; invalid["vm"]["workspace"] = Value::Null;
        assert!(desired_spec(invalid, "blob", "revision").is_err());
    }

    struct Fixture { store: Store, _dir: tempfile::TempDir }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL and CI_TEST_NATS_URL"]
    async fn application_activation_is_required_durable_and_cancellation_fenced() {
        let f = fixture().await;
        let d = dispatcher(&f, "http://127.0.0.1:1").await;
        sqlx::query("UPDATE ci_controller_rollout SET phase='prepared',application_id='ci' WHERE id='op'")
            .execute(f.store.pool()).await.unwrap();
        let hash = "\"44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a\"";
        assert!(Lifecycle::default().admission(&f.store).await.is_ok());
        assert!(Lifecycle::default().work(&f.store).await.is_ok());
        assert!(activate_application_update(&d, "op", "different").await.is_err());
        let phase: String = sqlx::query_scalar("SELECT phase FROM ci_controller_rollout WHERE id='op'")
            .fetch_one(f.store.pool()).await.unwrap();
        assert_eq!(phase, "prepared");
        activate_application_update(&d, "op", hash).await.unwrap();
        let restarted = dispatcher(&f, "http://127.0.0.1:2").await;
        activate_application_update(&restarted, "op", hash).await.unwrap();
        let phase: String = sqlx::query_scalar("SELECT phase FROM ci_controller_rollout WHERE id='op'")
            .fetch_one(f.store.pool()).await.unwrap();
        assert_eq!(phase, "pending");
        assert!(activate_application_update(&restarted, "op", "different").await.is_err());
        sqlx::query("UPDATE ci_controller_rollout SET phase='prepared',activation_hash=NULL WHERE id='op'")
            .execute(f.store.pool()).await.unwrap();
        f.store.cancel_run("run").await.unwrap();
        assert!(activate_application_update(&d, "op", hash).await.is_err());
        let phase: String = sqlx::query_scalar("SELECT phase FROM ci_controller_rollout WHERE id='op'")
            .fetch_one(f.store.pool()).await.unwrap();
        assert_eq!(phase, "prepared");
    }

    async fn fixture() -> Fixture {
        let base = std::env::var("CI_TEST_DATABASE_URL").expect("disposable CI_TEST_DATABASE_URL");
        let admin = sqlx::PgPool::connect(&base).await.unwrap();
        let schema = format!("rollout_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
        admin.close().await;
        let mut url = reqwest::Url::parse(&base).unwrap();
        url.query_pairs_mut().append_pair("options", &format!("-c search_path={schema}"));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(url.as_str(), dir.path().join("logs"), Duration::from_secs(10)).await.unwrap();
        store.migrate().await.unwrap();
        sqlx::raw_sql("INSERT INTO ci_run(id,workflow_id,workflow_path,status) VALUES('run','tests','ci.yml','running');
            INSERT INTO ci_job(id,run_id,job_key,base_id,display,status) VALUES('job','run','deploy','deploy','Deploy','success');
            INSERT INTO ci_step(id,job_id,idx,name,uses,status) VALUES('step','job',0,'Request','ci/deploy-controller','success');
            INSERT INTO ci_service_deployment(id,step_id,run_id,job_id,service_id,request_hash,status,sha,git_ref) VALUES('op','step','run','job','ci-test','hash','running','source','refs/heads/main');
            INSERT INTO ci_controller_rollout(id,request) VALUES('op','{}');")
            .execute(store.pool()).await.unwrap();
        Fixture { store, _dir: dir }
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn regional_admission_and_native_grants_serialize_with_drain() {
        let f = fixture().await;
        let first = Lifecycle::default();
        let peer = Lifecycle::default();
        // This peer passed the early process-local check before source preparation.
        let _early = peer.admission(&f.store).await.unwrap();
        let mut admitted = f.store.pool().begin().await.unwrap();
        Lifecycle::admit_in(&mut admitted).await.unwrap();
        let closing_store = f.store.clone();
        let mut closing = tokio::spawn(async move { first.close_admission(&closing_store, "op").await });
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut closing).await.is_err(),
            "another process must wait for an already-admitted transaction");
        admitted.commit().await.unwrap();
        closing.await.unwrap().unwrap();
        let mut late = f.store.pool().begin().await.unwrap();
        assert!(Lifecycle::admit_in(&mut late).await.unwrap_err().contains("submissions are closed"),
            "the early peer permit cannot authorize a late submission commit");
        late.rollback().await.unwrap();

        let mut grant = f.store.pool().begin().await.unwrap();
        Lifecycle::grant_in(&mut grant).await.unwrap();
        assert!(!peer.quiesce(&f.store, "op").await.unwrap(), "native grant is still in flight on a peer");
        grant.commit().await.unwrap();
        assert!(peer.quiesce(&f.store, "op").await.unwrap());
        let mut late_grant = f.store.pool().begin().await.unwrap();
        assert!(Lifecycle::grant_in(&mut late_grant).await.unwrap_err().contains("new work is paused"));
        late_grant.rollback().await.unwrap();
        let result = crate::native::poll(&f.store,
            crate::native::Poll { runner_id: "unused".into(), protocol_version: 1 },
            "http://localhost", &crate::secrets::Secrets::unconfigured()).await;
        assert!(matches!(result, Err(crate::native::PollError::Internal(message)) if message.contains("new work is paused")),
            "poll must enforce the transactional gate itself");
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn durable_admission_and_job_lease_barriers_survive_restart() {
        let f = fixture().await; let s = &f.store;
        let gate = Arc::new(Lifecycle::default());
        let admitted = gate.admission(s).await.unwrap();
        let other_gate = gate.clone(); let other_store = s.clone();
        let closing = tokio::spawn(async move { other_gate.close_admission(&other_store, "op").await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!closing.is_finished(), "cannot close admission while a submit still writes its source");
        drop(admitted); closing.await.unwrap().unwrap();
        assert!(gate.admission(s).await.is_err());
        assert!(Lifecycle::default().admission(s).await.is_err(), "a restart must read the durable gate");
        let work = gate.work(s).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "in-flight delivery/advance/upload is a blocker");
        drop(work);
        s.set_run_status("run", RunStatus::Failure, None).await.unwrap();
        s.set_job_status("job", JobStatus::Running, None).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "running siblings of a failed run still count");
        s.set_job_status("job", JobStatus::Success, None).await.unwrap();
        sqlx::query("INSERT INTO ci_vm_pool(sandbox_id,runner_hd_id,fingerprint,status) VALUES('vm','host','fingerprint','building')").execute(s.pool()).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "VM creation is work");
        sqlx::query("UPDATE ci_vm_pool SET status='idle'").execute(s.pool()).await.unwrap();
        sqlx::query("INSERT INTO ci_native_job(job_id,run_id,required_labels,state,lease_expires_at) VALUES('job','run','{}','leased',now()+interval '1 minute')").execute(s.pool()).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "native lease remains authoritative even with a terminal job row");
        s.set_job_status("job", JobStatus::Running, None).await.unwrap();
        sqlx::query("UPDATE ci_native_job SET lease_expires_at=now()-interval '1 minute'").execute(s.pool()).await.unwrap();
        s.set_run_status("run", RunStatus::Running, None).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "lease expiry does not prove the native process stopped");
        s.set_run_status("run", RunStatus::Failure, None).await.unwrap();
        s.set_job_status("job", JobStatus::Cancelled, None).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "terminal rows cannot discharge native execution");
        // Model an explicit native completion, not a timer-based release.
        sqlx::query("UPDATE ci_native_job SET state='completed'").execute(s.pool()).await.unwrap();
        sqlx::query("INSERT INTO ci_host_work(job_id,runner_hd_id,attempt) VALUES('job','host',1)").execute(s.pool()).await.unwrap();
        assert!(!gate.quiesce(s, "op").await.unwrap(), "host work can exist before its VM is recorded");
        s.end_host_work("job", "host", 1).await.unwrap();
        assert!(gate.quiesce(s, "op").await.unwrap());
        assert!(Lifecycle::default().work(s).await.is_err());
        sqlx::query("UPDATE ci_controller_rollout SET phase='complete'").execute(s.pool()).await.unwrap();
        assert!(gate.admission(s).await.is_ok()); assert!(gate.work(s).await.is_ok());
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn completed_jobs_do_not_publish_success_before_deployment_and_cancel_fences_submission() {
        let f = fixture().await; let s = &f.store;
        assert_eq!(s.roll_up_run("run").await.unwrap(), RunStatus::Running);
        assert!(s.get_run("run").await.unwrap().unwrap().finished_at.is_none());
        sqlx::query("UPDATE ci_controller_rollout SET phase='quiesced'").execute(s.pool()).await.unwrap();
        s.cancel_run("run").await.unwrap();
        assert!(!mark_submitting(s, "op", "run").await.unwrap());
        sqlx::query("UPDATE ci_controller_rollout SET phase='submitting'").execute(s.pool()).await.unwrap();
        assert!(mark_submitting(s, "op", "run").await.unwrap(), "a committed attempt must reconcile even after cancellation");
        sqlx::query("UPDATE ci_service_deployment SET status='passed'").execute(s.pool()).await.unwrap();
        assert_eq!(s.roll_up_run("run").await.unwrap(), RunStatus::Cancelled);
        sqlx::query("UPDATE ci_run SET status='running'").execute(s.pool()).await.unwrap();
        assert_eq!(s.roll_up_run("run").await.unwrap(), RunStatus::Success);
        sqlx::query("UPDATE ci_service_deployment SET status='failed'").execute(s.pool()).await.unwrap();
        assert_eq!(s.roll_up_run("run").await.unwrap(), RunStatus::Failure);
    }

    async fn dispatcher(f: &Fixture, base: &str) -> Dispatcher {
        unsafe {
            std::env::set_var("CI_HEYO_API_KEY", "local-test-only");
            std::env::set_var("CI_NETWORK", "local-test-only");
            std::env::set_var("CI_DATABASE_URL", std::env::var("CI_TEST_DATABASE_URL").unwrap());
            std::env::set_var("CI_WEBHOOK_SECRET", "0123456789abcdef");
            std::env::set_var("CI_NATS_URL", std::env::var("CI_TEST_NATS_URL").expect("disposable CI_TEST_NATS_URL"));
        }
        let mut config = crate::config::Config::from_env().unwrap();
        config.application_id = Some("ci".into());
        config.application_orchestrator_url = Some(base.into());
        config.application_lifecycle_token = Some("test-lifecycle".into());
        config.controller_deployment = Some("ci-test".into());
        config.controller_repository = Some("https://github.com/example/ci.git".into());
        config.app_lb_url = None; config.app_lb_token = None;
        config.controller_app_lb_url = Some(base.into()); config.controller_app_lb_token = Some("test-admin".into());
        assert!(!crate::objects::Workflows::new(&config).is_configured(), "self-deployment must not enable workflow-object discovery");
        config.public_url = base.into();
        config.nats_prefix = format!("rollout{}", uuid::Uuid::new_v4().simple());
        config.artifact_sink = crate::config::ArtifactSinkKind::Disk;
        config.artifact_dir = f._dir.path().join("artifacts");
        let config = Arc::new(config);
        Dispatcher {
            config: config.clone(), store: f.store.clone(), lifecycle: Arc::new(Lifecycle::default()),
            executor: Arc::new(crate::executor::ExecutorOwner::register(f.store.pool().clone(), &format!("{base}/deployments/ci-test")).await.unwrap()),
            pool: crate::pool::Pool::new(f.store.pool().clone()), images: crate::image::Catalog::new(f.store.pool().clone()),
            bus: Arc::new(crate::bus::Bus::connect(&config.nats, &config.nats_prefix).await.unwrap()),
            runners: Arc::new(crate::runners::Runners::new(config.clone())),
            vms: Arc::new(crate::vm::Vms::new()), secrets: crate::secrets::Secrets::unconfigured(),
            artifacts: Arc::from(crate::artifacts::sink_for(&config).unwrap()),
            objects: Arc::new(crate::objects::Workflows::new(&config)),
        }
    }

    #[derive(Clone)]
    struct Remote {
        snapshot: Arc<std::sync::Mutex<Value>>,
        puts: Arc<std::sync::atomic::AtomicUsize>,
        wrong_binary: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn remote() -> (String, Remote, tokio::task::JoinHandle<()>) {
        use axum::{Router, Json, extract::State, http::{HeaderMap, StatusCode}, response::IntoResponse, routing::get};
        use std::sync::atomic::Ordering::SeqCst;
        let remote = Remote {
            snapshot: Arc::new(std::sync::Mutex::new(json!({"spec":spec(),"vms":[{"sandbox_id":"old-vm","healthy":true,"draining":false}],"workspace":{"phase":"idle","push_pending":false}}))),
            puts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            wrong_binary: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = Router::new().route("/deployments/ci-test", get(|State(r): State<Remote>, headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer test-admin");
            let body = r.snapshot.lock().unwrap().clone();
            ([("etag", etag(&body["spec"]))], Json(body))
        }).put(|State(r): State<Remote>, headers: HeaderMap, Json(spec): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer test-admin");
            let mut current = r.snapshot.lock().unwrap();
            if headers.get("if-match").and_then(|h| h.to_str().ok()) != Some(etag(&current["spec"]).as_str()) {
                return StatusCode::PRECONDITION_FAILED.into_response();
            }
            r.puts.fetch_add(1, SeqCst);
            current["spec"] = spec;
            current["vms"] = json!([{"sandbox_id":"replacement-vm","healthy":true,"draining":false}]);
            // The mutation happened but the caller did not receive success.
            StatusCode::BAD_GATEWAY.into_response()
        })).route("/healthz", get(|State(r): State<Remote>| async move {
            let binary = if r.wrong_binary.load(SeqCst) { "wrong-binary" } else { "verified-binary" };
            ([("x-ci-revision", "source"), ("x-ci-binary-sha256", binary)], "ok\n")
        })).with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        (url, remote, task)
    }

    async fn seed_request(f: &Fixture, base: &str) {
        let request = Request { deployment: "ci-test".into(), base_url: base.into(), public_url: base.into(),
            artifact: "blob".into(), sha: "source".into(), binary_sha256: "verified-binary".into(),
            previous_vm: "old-vm".into(), previous_etag: etag(&spec()),
            desired_etag: etag(&desired_spec(spec(), "blob", "source").unwrap()) };
        sqlx::query("UPDATE ci_controller_rollout SET request=$1").bind(serde_json::to_value(request).unwrap())
            .execute(f.store.pool()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL and CI_TEST_NATS_URL"]
    async fn regional_handoff_and_lost_update_response_reconcile_once_without_false_success() {
        use std::sync::atomic::Ordering::SeqCst;
        let f = fixture().await;
        let (base, remote, server) = remote().await;
        seed_request(&f, &base).await;
        let d = dispatcher(&f, &base).await;
        // Same deployment name, different authority: one CI app, two replicas.
        // This authority has no mock route, so accidentally using the peer's
        // local target instead of the persisted operation must fail the test.
        let restarted = dispatcher(&f, &format!("{base}/peer")).await;
        restarted.executor.mark_ready().await.unwrap();
        sqlx::query("UPDATE ci_controller_rollout SET phase='prepared',application_id='ci' WHERE id='op'")
            .execute(f.store.pool()).await.unwrap();
        reconcile(&d).await.unwrap();
        assert_eq!(remote.puts.load(SeqCst),0,"prepared intent cannot replace the controller");
        assert!(d.lifecycle.admission(&f.store).await.is_ok());
        let intent = application_status(&d,"op").await.unwrap();
        activate_application_update(&d,"op",intent["intentHash"].as_str().unwrap()).await.unwrap();
        reconcile(&d).await.unwrap(); // pending -> draining
        assert!(d.lifecycle.admission(&f.store).await.is_err());
        reconcile(&d).await.unwrap(); // draining -> quiesced
        assert!(d.lifecycle.work(&f.store).await.is_err());
        reconcile(&d).await.unwrap(); // transfer before any replacement request
        assert_eq!(remote.puts.load(SeqCst), 0);
        assert!(d.executor.effect_permit().await.is_err());
        assert!(restarted.executor.effect_permit().await.is_err(), "only the exact continuation is admitted");
        reconcile(&restarted).await.unwrap(); // remote changed; response lost
        assert_eq!(remote.puts.load(SeqCst), 1);
        drop(d);
        assert!(reconcile(&restarted).await.unwrap_err().contains("exact replacement"));
        assert_eq!(remote.puts.load(SeqCst), 1, "must not replace twice after a lost response");
        assert_eq!(f.store.get_run("run").await.unwrap().unwrap().status, "running");
        assert!(restarted.lifecycle.admission(&f.store).await.is_err());
        remote.wrong_binary.store(false, SeqCst);
        sqlx::query("ALTER TABLE ci_event_outbox ADD CONSTRAINT reject_success CHECK (status <> 'passed')").execute(f.store.pool()).await.unwrap();
        assert!(reconcile(&restarted).await.is_err(), "simulate failure at the final durable outcome commit");
        assert!(restarted.lifecycle.admission(&f.store).await.is_err(), "gate and result must roll back together");
        assert!(restarted.executor.effect_permit().await.is_err(), "continuation must roll back with the result");
        assert_eq!(f.store.get_run("run").await.unwrap().unwrap().status, "running");
        sqlx::query("ALTER TABLE ci_event_outbox DROP CONSTRAINT reject_success").execute(f.store.pool()).await.unwrap();
        reconcile(&restarted).await.unwrap();
        assert_eq!(f.store.get_run("run").await.unwrap().unwrap().status, "success");
        assert_eq!(f.store.service_deployments_of("run").await.unwrap()[0].status, "passed");
        assert!(restarted.lifecycle.admission(&f.store).await.is_ok());
        assert!(restarted.executor.effect_permit().await.is_ok(), "verified completion opens normal execution atomically");
        reconcile(&restarted).await.unwrap();
        assert_eq!(remote.puts.load(SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL and CI_TEST_NATS_URL"]
    async fn concurrent_configuration_edit_is_not_overwritten() {
        use std::sync::atomic::Ordering::SeqCst;
        let f = fixture().await;
        let (base, remote, server) = remote().await;
        seed_request(&f, &base).await;
        let d = dispatcher(&f, &base).await;
        reconcile(&d).await.unwrap(); reconcile(&d).await.unwrap();
        remote.snapshot.lock().unwrap()["spec"]["vm"]["env_vars"]["OTHER"] = json!("concurrent-edit");
        reconcile(&d).await.unwrap();
        assert_eq!(remote.puts.load(SeqCst), 0);
        assert_eq!(f.store.get_run("run").await.unwrap().unwrap().status, "failure");
        assert!(d.lifecycle.admission(&f.store).await.is_ok());
        assert_eq!(remote.snapshot.lock().unwrap()["spec"]["vm"]["env_vars"]["OTHER"], "concurrent-edit");
        server.abort();
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn failed_remote_operation_still_blocks_handoff() {
        let f = fixture().await;
        sqlx::raw_sql("UPDATE ci_controller_rollout SET phase='quiesced' WHERE id='op';
            INSERT INTO ci_step(id,job_id,idx,name,uses,status) VALUES('maintenance-step','job',1,'Maintenance','ci/host-heyvm-maintenance','failure');
            INSERT INTO ci_service_deployment(id,step_id,run_id,job_id,service_id,request_hash,status,sha,git_ref) VALUES('maintenance','maintenance-step','run','job','host','hash','failed','source','refs/heads/main');
            INSERT INTO ci_host_maintenance(id,runner_hd_id,request,phase,deadline) VALUES('maintenance','runner','{}','failed',now());")
            .execute(f.store.pool()).await.unwrap();
        let lifecycle = Lifecycle::default();
        assert!(lifecycle.verify_handoff_quiesced(&f.store, "op").await.unwrap_err().contains("obligations remain"));
        // Only positive settlement of the remote operation releases this fence.
        sqlx::query("UPDATE ci_host_maintenance SET phase='passed' WHERE id='maintenance'")
            .execute(f.store.pool()).await.unwrap();
        lifecycle.verify_handoff_quiesced(&f.store, "op").await.unwrap();
    }
}
