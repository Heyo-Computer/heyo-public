//! Service rollout through orchestrator, never through raw VM management.
//!
//! Orchestrator's POST is not idempotent. Persist our one submission attempt
//! before sending it; after any uncertain outcome reconcile only by GET.
use crate::{bus::JobMessage, secrets::Masker, store::Store};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

pub(crate) async fn source_sha(store: &Store, msg: &JobMessage) -> Result<String, String> {
    let run = store.get_run(&msg.run_id).await.map_err(|e| e.to_string())?.ok_or("run not found")?;
    let release_workflow = store.jobs_of(&msg.run_id).await.map_err(|e| e.to_string())?.iter()
        .any(|job| job.plan["steps"].as_array().is_some_and(|steps| steps.iter().any(|s| s["uses"] == "ci/merge-release")));
    if release_workflow {
        let release = crate::release::get(store, &msg.run_id).await?
            .filter(|r| r.status == "published").ok_or("rootfs publication/deployment requires a confirmed published release")?;
        Ok(release.prepared.release_sha)
    } else {
        Ok(run.sha)
    }
}

pub(crate) async fn publication_source_sha(store: &Store, msg: &JobMessage) -> Result<String, String> {
    let sha = source_sha(store, msg).await?;
    if crate::release::get(store, &msg.run_id).await?.is_some() {
        let checked_out: Option<String> = sqlx::query_scalar("SELECT release_sha FROM ci_job WHERE id=$1")
            .bind(&msg.job_id).fetch_one(store.pool()).await.map_err(|e| e.to_string())?;
        if checked_out.as_deref() != Some(&sha) {
            return Err("rootfs must be built after ci/checkout-release at the exact release commit".into());
        }
    }
    Ok(sha)
}

fn app_lb_endpoint(base: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(base).map_err(|_| "invalid app-lb URL")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback)) || !url.username().is_empty()
        || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("app-lb URL must use HTTPS (HTTP allowed on loopback), without credentials, query or fragment".into());
    }
    Ok(base.trim_end_matches('/').into())
}

fn valid_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn clean_remote_error(masker: &Masker, token: &str, error: &str) -> String {
    masker.mask(error).replace(token, "***")
}

fn app_lb_config_fingerprint(mut spec: Value) -> String {
    if let Some(vm) = spec["vm"].as_object_mut() { vm.remove("image"); }
    spec["artifact"]["ref"] = json!("");
    hex::encode(Sha256::digest(serde_json::to_vec(&spec).expect("JSON value serializes")))
}

fn validate_app_lb_job(body: &Value, expected_job: Option<&str>, operation: &str, deployment: &str,
    namespace: &str, digest: &str, config_fingerprint: &str) -> Result<&'static str, String> {
    let id = body["id"].as_str().filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .ok_or("app-lb response omitted a valid job id")?;
    if expected_job.is_some_and(|expected| expected != id) {
        return Err("app-lb returned a different job than the correlated pull".into());
    }
    if body["operation_id"] != operation || body["deployment"] != deployment
        || body["target_namespace"] != namespace || body["artifact"] != digest
        || body["config_fingerprint"] != config_fingerprint {
        return Err("app-lb returned a job with the wrong operation, deployment, namespace, artifact digest, or target spec".into());
    }
    match body["status"].as_str() {
        Some("running") => Ok("running"),
        Some("failed") => Err(body["error"].as_str().unwrap_or("app-lb deployment failed").into()),
        Some("succeeded") if body["readiness_verified"] == true && matches!(body.get("reconciliation_required"), None | Some(Value::Bool(false)))
            && body["rolled_out"] == true => Ok("passed"),
        Some("succeeded") => Err("app-lb completed without an exact healthy replacement, or requires reconciliation".into()),
        _ => Err("app-lb returned an invalid deployment status".into()),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn deploy_app_lb(store: &Store, msg: &JobMessage, step: &str, base: &str, token: &str,
    deployment: &str, namespace: &str, digest: &str, store_url: &str, timeout: Duration,
    masker: &Masker) -> Result<String, String> {
    if token.trim().is_empty() { return Err("ci/deploy-app-lb needs an app-lb credential from secrets".into()); }
    if !valid_digest(digest) { return Err("manifest must be a 64-character lowercase SHA256 digest".into()); }
    if deployment.is_empty() || namespace.is_empty() || !deployment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("deployment and namespace are required, and deployment must be a safe identifier".into());
    }
    let base = app_lb_endpoint(base)?;
    let sha = source_sha(store, msg).await?;
    let authorized: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_app_lb_artifact a JOIN ci_step s ON s.id=a.step_id JOIN ci_job j ON j.id=a.job_id WHERE a.run_id=$1 AND a.sha=$2 AND a.store_url=$3 AND a.manifest_digest=$4 AND s.status='success' AND (a.job_id=$5 OR j.status='success'))")
        .bind(&msg.run_id).bind(&sha).bind(store_url.trim_end_matches('/')).bind(digest)
        .bind(&msg.job_id)
        .fetch_one(store.pool()).await.map_err(|e| e.to_string())?;
    if !authorized { return Err("deployment manifest was not published successfully by this run at the expected source SHA and artifact store".into()); }
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none())
        .build().map_err(|e| e.to_string())?;
    let spec: Value = http.get(format!("{base}/deployments/{deployment}")).bearer_auth(token).send().await
        .map_err(|_| "could not read the registered app-lb deployment")?.error_for_status()
        .map_err(|e| format!("could not read registered app-lb deployment: {e}"))?.json().await.map_err(|_| "invalid app-lb deployment response")?;
    if spec["spec"]["namespace"].as_str().unwrap_or("default") != namespace
        || spec["spec"]["artifact"]["store"].as_str().map(|s| s.trim_end_matches('/')) != Some(store_url.trim_end_matches('/')) {
        return Err("registered app-lb deployment namespace or artifact.store does not match the authorized target/store".into());
    }
    let source_spec_fingerprint = app_lb_config_fingerprint(spec["spec"].clone());
    let operation = format!("ci-app-lb-{}", hex::encode(Sha256::digest(step.as_bytes())));
    let request = json!({"operation_id":operation,"ref":digest,"force":false});
    let request_hash = hex::encode(Sha256::digest(format!("{base}\n{namespace}\n{}\n{deployment}\n{source_spec_fingerprint}\n{request}", store_url.trim_end_matches('/'))));
    let first = store.begin_service_deployment(&operation, step, deployment, &request_hash).await.map_err(|e| e.to_string())?;
    if let Some(current) = store.service_deployments_of(&msg.run_id).await.map_err(|e| e.to_string())?
        .into_iter().find(|d| d.id == operation) {
        if current.status == "passed" {
            return Ok(format!("[ci] app-lb deployment {deployment} already verified (operation {operation})\n"));
        }
        if current.status == "failed" {
            return Err(current.error.or(current.message).unwrap_or_else(|| "app-lb deployment previously failed".into()));
        }
    }
    let started = Instant::now();
    if store.is_job_cancelled(&msg.job_id).await.map_err(|e| e.to_string())? || started.elapsed() >= timeout {
        return Err(format!("CI stopped before submitting app-lb operation {operation}"));
    }
    let response = if first {
        Some(http.post(format!("{base}/deployments/{deployment}/pull")).bearer_auth(token).json(&request).send().await)
    } else { None };
    let job_id = match response {
        Some(Ok(r)) if r.status().is_success() => {
            let body: Value = r.json().await.map_err(|_| "invalid app-lb pull response")?;
            if let Err(e) = validate_app_lb_job(&body, None, &operation, deployment, namespace, digest, &source_spec_fingerprint) {
                let clean = clean_remote_error(masker, token, &e);
                store.update_service_deployment(&operation, "failed", Some("app-lb-pull"), None, Some(&clean)).await.map_err(|x| x.to_string())?;
                return Err(clean);
            }
            body["id"].as_str().ok_or("app-lb pull response omitted job id")?.to_string()
        }
        _ => {
            let jobs: Vec<Value> = http.get(format!("{base}/deployments/{deployment}/jobs")).bearer_auth(token).send().await
                .map_err(|_| "app-lb submission outcome is uncertain and reconciliation failed")?.json().await
                .map_err(|_| "invalid app-lb reconciliation response")?;
            let found = jobs.into_iter().find(|j| j["operation_id"] == operation)
                .ok_or("app-lb submission outcome is uncertain; operation was not found")?
                ;
            if let Err(e) = validate_app_lb_job(&found, None, &operation, deployment, namespace, digest, &source_spec_fingerprint) {
                let clean = clean_remote_error(masker, token, &e);
                store.update_service_deployment(&operation, "failed", Some("app-lb-pull"), None, Some(&clean)).await.map_err(|x| x.to_string())?;
                return Err(clean);
            }
            found["id"].as_str().unwrap().to_owned()
        }
    };
    store.update_service_deployment(&operation, "running", Some("app-lb-pull"), Some("Waiting for exact healthy replacement."), None).await.map_err(|e| e.to_string())?;
    loop {
        if store.is_job_cancelled(&msg.job_id).await.map_err(|e| e.to_string())? || started.elapsed() >= timeout {
            return Err(format!("CI stopped waiting; reconcile app-lb operation {operation} before retrying"));
        }
        let body: Value = http.get(format!("{base}/jobs/{job_id}")).bearer_auth(token).send().await
            .map_err(|_| "could not reconcile app-lb job")?.error_for_status().map_err(|e| e.to_string())?.json().await.map_err(|_| "invalid app-lb job response")?;
        match validate_app_lb_job(&body, Some(&job_id), &operation, deployment, namespace, digest, &source_spec_fingerprint) {
            Ok("running") => {}
            Ok("passed") => {
                if store.is_job_cancelled(&msg.job_id).await.map_err(|e| e.to_string())? || started.elapsed() >= timeout {
                    return Err(format!("CI stopped waiting; reconcile app-lb operation {operation} before retrying"));
                }
                store.update_service_deployment(&operation, "passed", Some("ready"), Some("Exact healthy replacement verified."), None).await.map_err(|e| e.to_string())?;
                return Ok(format!("[ci] app-lb deployment {deployment} installed manifest {digest} and verified readiness (operation {operation})\n"));
            }
            Ok(_) => unreachable!(),
            Err(e) => {
                let clean = clean_remote_error(masker, token, &e);
                store.update_service_deployment(&operation, "failed", Some("app-lb-pull"), None, Some(&clean)).await.map_err(|x| x.to_string())?;
                return Err(clean);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn prepare(mut spec: Value, operation: &str, repo: &str, git_ref: &str, sha: &str) -> Result<(String, Value), String> {
    if repo.is_empty() || !git_ref.starts_with("refs/heads/") ||
        !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("deployment requires a repository URL, branch ref and full commit SHA".into());
    }
    let service = spec.get("id").and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 63 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
        .ok_or("service spec requires a valid id")?.to_owned();
    let deploy = spec.get_mut("deploy").and_then(Value::as_object_mut)
        .ok_or("service spec requires a deploy object")?;
    if deploy.get("archive_id").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
        return Err("ci/deploy-service requires deploy.archive_id from a finalized orchestrator service archive".into());
    }
    if deploy.contains_key("archive_bytes_base64") {
        return Err("use a finalized archive_id, not inline archive bytes".into());
    }
    deploy.insert("deployment_id".into(), json!(operation));
    deploy.insert("async".into(), json!(true));
    // The workflow cannot turn this guard off or deploy a different revision.
    deploy.insert("revision_guard".into(), json!({
        "repository_url": repo, "ref": git_ref, "expected_sha": sha, "force": false,
    }));
    Ok((service, spec))
}

fn endpoint(base: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(base).map_err(|_| "invalid orchestrator URL")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback)) ||
        !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("orchestrator URL must use HTTPS (HTTP allowed on loopback), without credentials, query or fragment".into());
    }
    Ok(format!("{}/orchestration/services/deployments", base.trim_end_matches('/')))
}

#[allow(clippy::too_many_arguments)]
pub async fn deploy(
    store: &Store, msg: &JobMessage, step: &str, spec: Value, base: &str,
    token: &str, timeout: Duration, masker: &Masker,
) -> Result<String, String> {
    if token.trim().is_empty() { return Err("ci/deploy-service needs an orchestrator credential from secrets".into()); }
    let endpoint = endpoint(base)?;
    let run = store.get_run(&msg.run_id).await.map_err(|e| e.to_string())?.ok_or("run not found")?;
    let release = crate::release::get(store, &msg.run_id).await?;
    let release_workflow = store.jobs_of(&msg.run_id).await.map_err(|e| e.to_string())?.iter()
        .any(|job| job.plan["steps"].as_array().is_some_and(|steps| steps.iter().any(|s| s["uses"] == "ci/merge-release")));
    let (sha, git_ref) = if release_workflow || release.is_some() {
        let release = release.as_ref().filter(|r| r.status == "published")
            .ok_or("deployment requires a confirmed published release")?;
        let archive = spec["deploy"]["archive_id"].as_str().unwrap_or("");
        let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_service_archive WHERE run_id=$1 AND archive_id=$2 AND sha=$3 AND orchestrator_url=$4)")
            .bind(&msg.run_id).bind(archive).bind(&release.prepared.release_sha).bind(base.trim_end_matches('/'))
            .fetch_one(store.pool()).await.map_err(|e| e.to_string())?;
        if !matches { return Err("deployment archive was not published from this run's release checkout to this orchestrator".into()); }
        (&release.prepared.release_sha, &release.prepared.git_ref)
    } else { (&run.sha, &run.git_ref) };
    let id = format!("ci-{}", hex::encode(Sha256::digest(step.as_bytes())));
    let (service, request) = prepare(spec, &id, &run.repo_url, git_ref, sha)?;
    let request_hash = hex::encode(Sha256::digest(format!("{endpoint}\n{request}")));
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())?;
    let started = Instant::now();
    let first = store.begin_service_deployment(&id, step, &service, &request_hash).await.map_err(|e| e.to_string())?;
    if first {
        // A non-2xx can still be an uncertain outcome (e.g. a proxy timeout
        // after acceptance). GET is the only safe follow-up to any response.
        let result = http.post(&endpoint).bearer_auth(token).json(&request).send().await;
        let accepted = result.as_ref().is_ok_and(|r| r.status().is_success());
        let note = if accepted { "Submission acknowledged; waiting for rollout status." }
            else { "Submission outcome unknown; reconciling by operation ID. No automatic resubmission." };
        store.update_service_deployment(&id, if accepted { "running" } else { "submission_unknown" },
            None, Some(note), None).await.map_err(|e| e.to_string())?;
    }
    loop {
        // Persisted terminal results win over stale/concurrent polling.
        let current = store.service_deployments_of(&msg.run_id).await.map_err(|e| e.to_string())?
            .into_iter().find(|d| d.id == id).ok_or("deployment record disappeared")?;
        match current.status.as_str() {
            "passed" => return Ok(format!("[ci] service {service} deployed at {sha} (operation {id})\n")),
            "failed" => return Err(current.error.or(current.message).unwrap_or_else(|| format!("deployment {id} failed"))),
            _ => {}
        }
        if store.is_job_cancelled(&msg.job_id).await.map_err(|e| e.to_string())? || started.elapsed() >= timeout {
            let note = "CI stopped waiting (cancelled or timed out). The remote rollout may continue; do not resubmit without reconciling its operation ID.";
            let status = if current.status == "submitting" { "submission_unknown" } else { &current.status };
            store.update_service_deployment(&id, status, current.phase.as_deref(), Some(note), current.error.as_deref())
                .await.map_err(|e| e.to_string())?;
            return Err(format!("{note} Operation: {id}"));
        }
        let mut poll_error = Some("orchestrator returned an invalid deployment status or identity".to_string());
        match http.get(format!("{endpoint}/{id}")).bearer_auth(token).send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(body) if body["deploymentId"] == id && body["serviceId"] == service => {
                        if let Some(status @ ("running" | "passed" | "failed")) = body["status"].as_str() {
                            let clean = |key: &str| body[key].as_str().map(|s| masker.mask(s).replace(token, "***"));
                            store.update_service_deployment(&id, status, clean("phase").as_deref(),
                                clean("message").as_deref(), clean("errorMessage").as_deref()).await.map_err(|e| e.to_string())?;
                            if status != "running" { continue; }
                            poll_error = None;
                        }
                    }
                    _ => {}
                }
            }
            Ok(response) => poll_error = Some(format!("status lookup returned HTTP {}; retrying GET, not deployment submission", response.status().as_u16())),
            Err(_) => poll_error = Some("could not reach orchestrator for status; retrying GET, not deployment submission".into()),
        }
        if let Some(error) = poll_error {
            let status = if current.status == "submitting" { "submission_unknown" } else { &current.status };
            store.update_service_deployment(&id, status, current.phase.as_deref(), current.message.as_deref(), Some(&error))
                .await.map_err(|e| e.to_string())?;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_lb_requires_exact_identity_and_complete_readiness() {
        let digest = "a".repeat(64);
        let base = json!({"id":"job","deployment":"api","operation_id":"ci-app-lb-1",
            "target_namespace":"team","artifact":digest,"status":"succeeded",
            "config_fingerprint":"spec","readiness_verified":true,"rolled_out":true});
        assert_eq!(validate_app_lb_job(&base, None, "ci-app-lb-1", "api", "team", &digest, "spec").unwrap(), "passed");
        for (key, value) in [("target_namespace", json!("other")), ("artifact", json!("b".repeat(64))), ("id", json!("../other"))] {
            let mut bad = base.clone(); bad[key] = value;
            assert!(validate_app_lb_job(&bad, None, "ci-app-lb-1", "api", "team", &digest, "spec").is_err());
        }
        for (key, value) in [("readiness_verified", json!(false)), ("reconciliation_required", json!(true)), ("reconciliation_required", json!("false")), ("rolled_out", json!(false))] {
            let mut incomplete = base.clone(); incomplete[key] = value;
            assert!(validate_app_lb_job(&incomplete, None, "ci-app-lb-1", "api", "team", &digest, "spec").is_err());
        }
        assert!(validate_app_lb_job(&base, Some("different-job"), "ci-app-lb-1", "api", "team", &digest, "spec").is_err());
        let mut wrong_spec = base.clone(); wrong_spec["config_fingerprint"] = json!("changed");
        assert!(validate_app_lb_job(&wrong_spec, Some("job"), "ci-app-lb-1", "api", "team", &digest, "spec").is_err());
        let mut spec = json!({"vm":{"image":"old","port":8080},"artifact":{"store":"https://art.test","ref":"old"}});
        let before = app_lb_config_fingerprint(spec.clone());
        spec["vm"]["image"] = json!("new"); spec["artifact"]["ref"] = json!("new");
        assert_eq!(app_lb_config_fingerprint(spec.clone()), before);
        spec["vm"]["port"] = json!(9090);
        assert_ne!(app_lb_config_fingerprint(spec), before);
        let masker = Masker::new(["remote-secret"].into_iter());
        assert_eq!(clean_remote_error(&masker, "bearer-secret", "remote-secret bearer-secret failed"), "*** *** failed");
        assert!(!valid_digest(&"A".repeat(64)));
        assert!(!valid_digest("abc"));
    }

    #[test]
    fn revision_and_operation_identity_cannot_be_overridden() {
        let (_, request) = prepare(json!({"id":"api","deploy":{
            "archive_id":"archive-a", "deployment_id":"other", "async":false,
            "revision_guard":{"force":true,"expected_sha":"other"}
        }}), "ci-1", "https://example.test/repo.git", "refs/heads/main", &"a".repeat(40)).unwrap();
        assert_eq!(request["deploy"]["deployment_id"], "ci-1");
        assert_eq!(request["deploy"]["async"], true);
        assert_eq!(request["deploy"]["revision_guard"]["force"], false);
        assert_eq!(request["deploy"]["revision_guard"]["expected_sha"], "a".repeat(40));
        assert_eq!(request["deploy"]["archive_id"], "archive-a");
        assert!(prepare(json!({"id":"api","deploy":{}}), "ci-1", "repo", "refs/heads/main", &"a".repeat(40)).is_err());
        assert!(prepare(request, "ci-1", "repo", "refs/heads/main", "abc123").is_err());
    }

    #[test]
    fn credentials_are_not_sent_to_plaintext_or_url_auth_destinations() {
        assert!(endpoint("https://orch.example/prefix").is_ok());
        assert!(endpoint("http://127.0.0.1:1234").is_ok());
        for url in ["http://orch.example", "https://user:pass@orch.example", "https://orch.example?q=1", "file:///tmp"] {
            assert!(endpoint(url).is_err(), "{url}");
        }
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL; uses a local fake app-lb"]
    async fn app_lb_deploy_requires_provenance_and_exact_durable_result() {
        use axum::{Json, Router, extract::{Path, State}, routing::{get, post}};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Copy)]
        enum Reply { Success, WrongDigest, WrongNamespace, MaskedFailure }
        struct Remote { posts: usize, reply: Reply, operation: String, artifact: String }
        let remote = Arc::new(Mutex::new(Remote {
            posts: 0, reply: Reply::Success, operation: String::new(), artifact: String::new(),
        }));
        let spec = json!({
            "id":"api", "namespace":"team", "routes":[],
            "vm":{"image":"old-image","port":8080},
            "artifact":{"store":"https://artifacts.test/","ref":"old-ref"}
        });
        let normalized = json!({"id":"api","namespace":"team","routes":[],
            "vm":{"port":8080},"artifact":{"store":"https://artifacts.test/","ref":""}});
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&normalized).unwrap()));
        let response_fingerprint = fingerprint.clone();
        let job_response = move |remote: &Remote| {
            let (status, namespace, artifact, error) = match remote.reply {
                Reply::Success => ("succeeded", "team", remote.artifact.clone(), Value::Null),
                Reply::WrongDigest => ("succeeded", "team", "f".repeat(64), Value::Null),
                Reply::WrongNamespace => ("succeeded", "other", remote.artifact.clone(), Value::Null),
                Reply::MaskedFailure => ("failed", "team", remote.artifact.clone(), json!("test-secret remote-secret failed")),
            };
            json!({"id":"job_1", "deployment":"api", "operation_id":remote.operation,
                "target_namespace":namespace, "artifact":artifact, "config_fingerprint":response_fingerprint,
                "status":status, "readiness_verified":true, "rolled_out":true, "error":error})
        };
        let app = Router::new()
            .route("/deployments/{deployment}", get({
                let spec = spec.clone();
                move |Path(deployment): Path<String>| { let spec = spec.clone(); async move {
                    assert_eq!(deployment, "api"); Json(json!({"spec":spec}))
                }}
            }))
            .route("/deployments/{deployment}/pull", post(
                |State(remote): State<Arc<Mutex<Remote>>>, Path(deployment): Path<String>,
                 headers: axum::http::HeaderMap, Json(request): Json<Value>| async move {
                    assert_eq!(deployment, "api");
                    assert_eq!(headers["authorization"], "Bearer test-secret");
                    assert_eq!(request["force"], false);
                    let mut remote = remote.lock().unwrap();
                    remote.posts += 1;
                    remote.operation = request["operation_id"].as_str().unwrap().into();
                    remote.artifact = request["ref"].as_str().unwrap().into();
                    Json(json!({"id":"job_1", "deployment":"api", "operation_id":remote.operation.clone(),
                        "target_namespace":"team", "artifact":remote.artifact.clone(),
                        "config_fingerprint":fingerprint, "status":"running"}))
                }
            ))
            .route("/jobs/{id}", get(move |State(remote): State<Arc<Mutex<Remote>>>, Path(id): Path<String>| async move {
                assert_eq!(id, "job_1");
                Json(job_response(&remote.lock().unwrap()))
            }))
            .with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

        let store = Store::connect(&std::env::var("CI_TEST_DATABASE_URL").unwrap(),
            std::env::temp_dir().join(crate::vm::new_id()), Duration::from_secs(30)).await.unwrap();
        store.migrate().await.unwrap();
        let workflow = crate::workflow::Workflow::parse("deploy.yml", "jobs:\n  deploy:\n    vm: { driver: firecracker }\n    steps: [{ uses: ci/publish-rootfs }, { uses: ci/deploy-app-lb }]\n  producer:\n    vm: { driver: firecracker }\n    steps: [{ uses: ci/publish-rootfs }]\n").unwrap();
        let plan = crate::plan::Plan::build(&workflow).unwrap();
        let run = crate::vm::new_id();
        let sha = "a".repeat(40);
        store.create_run(&run, &crate::store::RunRequest {
            repo_url: "https://example.test/repo.git".into(), git_ref: "refs/heads/main".into(), sha: sha.clone(),
            ..Default::default()
        }, &plan).await.unwrap();
        let jobs = store.jobs_of(&run).await.unwrap();
        let deploy_job = jobs.iter().find(|j| j.job_key == "deploy").unwrap();
        let producer_job = jobs.iter().find(|j| j.job_key == "producer").unwrap();
        store.set_job_status(&deploy_job.id, crate::store::JobStatus::Running, None).await.unwrap();
        let published = crate::store::step_id(&deploy_job.id, 0);
        store.create_step(&published, &deploy_job.id, 0, "Publish", Some("ci/publish-rootfs")).await.unwrap();
        store.finish_step(&published, crate::store::StepStatus::Success, Some(0), None).await.unwrap();
        let digest = "b".repeat(64);
        sqlx::query("INSERT INTO ci_app_lb_artifact(step_id,run_id,job_id,sha,store_url,manifest_digest,blob_digest,size_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,1)")
            .bind(&published).bind(&run).bind(&deploy_job.id).bind(&sha).bind("https://artifacts.test").bind(&digest).bind("c".repeat(64))
            .execute(store.pool()).await.unwrap();
        let cross_step = crate::store::step_id(&producer_job.id, 0);
        store.create_step(&cross_step, &producer_job.id, 0, "Publish from unfinished job", Some("ci/publish-rootfs")).await.unwrap();
        store.finish_step(&cross_step, crate::store::StepStatus::Success, Some(0), None).await.unwrap();
        let cross_digest = "d".repeat(64);
        sqlx::query("INSERT INTO ci_app_lb_artifact(step_id,run_id,job_id,sha,store_url,manifest_digest,blob_digest,size_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,1)")
            .bind(&cross_step).bind(&run).bind(&producer_job.id).bind(&sha).bind("https://artifacts.test").bind(&cross_digest).bind("e".repeat(64))
            .execute(store.pool()).await.unwrap();
        let msg = JobMessage { run_id: run.clone(), job_id: deploy_job.id.clone(), job_key: deploy_job.job_key.clone() };
        let masker = Masker::new(["test-secret", "remote-secret"].into_iter());

        let rejected_step = crate::store::step_id(&deploy_job.id, 1);
        store.create_step(&rejected_step, &deploy_job.id, 1, "Reject", Some("ci/deploy-app-lb")).await.unwrap();
        assert!(deploy_app_lb(&store, &msg, &rejected_step, &base, "test-secret", "api", "team", &cross_digest,
            "https://artifacts.test", Duration::from_secs(5), &masker).await.unwrap_err().contains("not published successfully"));
        assert_eq!(remote.lock().unwrap().posts, 0);

        let success_step = crate::store::step_id(&deploy_job.id, 2);
        store.create_step(&success_step, &deploy_job.id, 2, "Deploy", Some("ci/deploy-app-lb")).await.unwrap();
        deploy_app_lb(&store, &msg, &success_step, &base, "test-secret", "api", "team", &digest,
            "https://artifacts.test/", Duration::from_secs(5), &masker).await.unwrap();
        deploy_app_lb(&store, &msg, &success_step, &base, "test-secret", "api", "team", &digest,
            "https://artifacts.test/", Duration::from_secs(5), &masker).await.unwrap();
        assert_eq!(remote.lock().unwrap().posts, 1, "a verified operation must not POST twice");

        for (idx, reply) in [(3, Reply::WrongDigest), (4, Reply::WrongNamespace), (5, Reply::MaskedFailure)] {
            remote.lock().unwrap().reply = reply;
            let sid = crate::store::step_id(&deploy_job.id, idx);
            store.create_step(&sid, &deploy_job.id, idx as i32, "Bad deploy", Some("ci/deploy-app-lb")).await.unwrap();
            let error = deploy_app_lb(&store, &msg, &sid, &base, "test-secret", "api", "team", &digest,
                "https://artifacts.test", Duration::from_secs(5), &masker).await.unwrap_err();
            assert!(!error.contains("test-secret") && !error.contains("remote-secret"));
            let row = store.service_deployments_of(&run).await.unwrap().into_iter().find(|d| d.step_id == sid).unwrap();
            assert_eq!(row.status, "failed");
            assert_eq!(row.error.as_deref(), Some(error.as_str()));
        }
        assert!(remote.lock().unwrap().posts >= 4);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL; uses a local fake orchestrator"]
    async fn uncertain_acceptance_and_concurrent_replay_never_post_twice() {
        use axum::{Router, Json, extract::State, http::StatusCode, response::IntoResponse, routing::post};
        use std::sync::{Arc, Mutex};
        #[derive(Default)]
        struct Remote { posts: usize, reads: usize, request: Option<Value>, status: String }
        let remote = Arc::new(Mutex::new(Remote { status: "passed".into(), ..Default::default() }));
        let app = Router::new().route("/orchestration/services/deployments", post(
            |State(remote): State<Arc<Mutex<Remote>>>, headers: axum::http::HeaderMap, Json(request): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer test-secret");
                let mut r = remote.lock().unwrap();
                r.posts += 1;
                r.request = Some(request);
                // Simulates an intermediary losing the acceptance response.
                StatusCode::BAD_GATEWAY
            }
        )).route("/orchestration/services/deployments/{id}", axum::routing::get(
            |State(remote): State<Arc<Mutex<Remote>>>| async move {
                let mut r = remote.lock().unwrap();
                r.reads += 1;
                if r.reads == 1 || r.request.is_none() { return StatusCode::NOT_FOUND.into_response(); }
                let request = r.request.as_ref().unwrap();
                Json(json!({"deploymentId":request["deploy"]["deployment_id"], "serviceId":request["id"],
                    "status":r.status, "phase":"route-cutover", "message":"test-secret rollout result"})).into_response()
            }
        )).with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let store = Store::connect(&std::env::var("CI_TEST_DATABASE_URL").unwrap(),
            std::env::temp_dir().join(crate::vm::new_id()), Duration::from_secs(30)).await.unwrap();
        store.migrate().await.unwrap();
        let workflow = crate::workflow::Workflow::parse("deploy.yml", "jobs:\n  deploy:\n    vm: { driver: firecracker }\n    steps: [{ uses: ci/deploy-service }]\n").unwrap();
        let plan = crate::plan::Plan::build(&workflow).unwrap();
        let run = crate::vm::new_id();
        store.create_run(&run, &crate::store::RunRequest {
            repo_url: "https://example.test/repo.git".into(), git_ref: "refs/heads/main".into(), sha: "a".repeat(40),
            ..Default::default()
        }, &plan).await.unwrap();
        let job = store.jobs_of(&run).await.unwrap().remove(0);
        store.set_job_status(&job.id, crate::store::JobStatus::Running, None).await.unwrap();
        let sid = crate::store::step_id(&job.id, 0);
        store.create_step(&sid, &job.id, 0, "Deploy", Some("ci/deploy-service")).await.unwrap();
        let msg = JobMessage { run_id: run.clone(), job_id: job.id, job_key: job.job_key };
        let spec = json!({"id":"api","deploy":{"archive_id":"archive-one"}});
        let masker = Masker::new(std::iter::once("test-secret"));
        let constraint = format!("reject_cd_{run}");
        sqlx::query(&format!("ALTER TABLE ci_event_outbox ADD CONSTRAINT \"{constraint}\" CHECK (run_id <> '{run}' OR event_type <> 'ci.deployment.status.v1')"))
            .execute(store.pool()).await.unwrap();
        let rejected = deploy(&store, &msg, &sid, spec.clone(), &base, "test-secret", Duration::from_secs(10), &masker).await;
        sqlx::query(&format!("ALTER TABLE ci_event_outbox DROP CONSTRAINT \"{constraint}\""))
            .execute(store.pool()).await.unwrap();
        assert!(rejected.is_err());
        assert_eq!(remote.lock().unwrap().posts, 0, "no remote effect before the ledger and event commit");
        assert!(store.service_deployments_of(&run).await.unwrap().is_empty());
        let (a,b) = tokio::join!(
            deploy(&store, &msg, &sid, spec.clone(), &base, "test-secret", Duration::from_secs(10), &masker),
            deploy(&store, &msg, &sid, spec.clone(), &base, "test-secret", Duration::from_secs(10), &masker),
        );
        a.unwrap(); b.unwrap();
        assert_eq!(remote.lock().unwrap().posts, 1);
        let rows = store.service_deployments_of(&run).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "passed");
        assert_eq!(rows[0].sha, "a".repeat(40));
        assert_eq!(rows[0].message.as_deref(), Some("*** rollout result"));
        let changed = json!({"id":"api","deploy":{"archive_id":"different-archive"}});
        assert!(deploy(&store, &msg, &sid, changed, &base, "test-secret", Duration::from_secs(1), &masker).await.is_err());
        assert_eq!(remote.lock().unwrap().posts, 1);
        // Terminal state cannot be erased by a stale poll or POST response.
        store.update_service_deployment(&rows[0].id, "running", None, None, None).await.unwrap();
        assert_eq!(store.service_deployments_of(&run).await.unwrap()[0].status, "passed");

        remote.lock().unwrap().status = "failed".into();
        let failed_sid = crate::store::step_id(&msg.job_id, 1);
        store.create_step(&failed_sid, &msg.job_id, 1, "Failing deploy", Some("ci/deploy-service")).await.unwrap();
        let failed = deploy(&store, &msg, &failed_sid, spec.clone(), &base, "test-secret", Duration::from_secs(10), &masker).await;
        assert!(failed.unwrap_err().contains("*** rollout result"));
        assert_eq!(store.service_deployments_of(&run).await.unwrap().iter().find(|d| d.step_id == failed_sid).unwrap().status, "failed");

        remote.lock().unwrap().status = "running".into();
        let waiting_sid = crate::store::step_id(&msg.job_id, 2);
        store.create_step(&waiting_sid, &msg.job_id, 2, "Slow deploy", Some("ci/deploy-service")).await.unwrap();
        assert!(deploy(&store, &msg, &waiting_sid, spec.clone(), &base, "test-secret", Duration::from_millis(100), &masker).await.is_err());
        assert_eq!(store.service_deployments_of(&run).await.unwrap().iter().find(|d| d.step_id == waiting_sid).unwrap().status, "running",
            "CI timing out must not claim the remote deployment failed");
        let posts = remote.lock().unwrap().posts;
        remote.lock().unwrap().status = "passed".into();
        drop(store);
        let restarted = Store::connect(&std::env::var("CI_TEST_DATABASE_URL").unwrap(),
            std::env::temp_dir().join(crate::vm::new_id()), Duration::from_secs(30)).await.unwrap();
        deploy(&restarted, &msg, &waiting_sid, spec, &base, "test-secret", Duration::from_secs(10), &masker).await.unwrap();
        assert_eq!(remote.lock().unwrap().posts, posts, "reconnecting must reconcile the existing deployment, not POST again");

        let release_sha = "b".repeat(40);
        let prepared = json!({"source_sha":"a".repeat(40),"release_sha":release_sha,
            "git_ref":"refs/heads/main","versions":{"package.json":"1.2.3"}});
        sqlx::query("INSERT INTO ci_release(run_id,request_hash,source_sha,base_sha,git_ref,versions,candidate_sha,prepared,status) VALUES($1,'test',$2,$2,'refs/heads/main','{}',$3,$4,'published')")
            .bind(&run).bind("a".repeat(40)).bind(&release_sha).bind(prepared)
            .execute(restarted.pool()).await.unwrap();
        let release_sid = crate::store::step_id(&msg.job_id, 3);
        restarted.create_step(&release_sid, &msg.job_id, 3, "Release deploy", Some("ci/deploy-service")).await.unwrap();
        let release_spec = json!({"id":"api","deploy":{"archive_id":"release-archive"}});
        sqlx::query("INSERT INTO ci_service_archive(step_id,run_id,job_id,archive_id,sha,orchestrator_url) VALUES($1,$2,$3,'release-archive',$4,$5)")
            .bind(&release_sid).bind(&run).bind(&msg.job_id).bind("a".repeat(40)).bind(&base)
            .execute(restarted.pool()).await.unwrap();
        assert!(deploy(&restarted, &msg, &release_sid, release_spec.clone(), &base, "test-secret", Duration::from_secs(10), &masker)
            .await.unwrap_err().contains("release checkout"));
        assert_eq!(remote.lock().unwrap().posts, posts, "pre-bump archive must never deploy");
        sqlx::query("UPDATE ci_service_archive SET sha=$2 WHERE step_id=$1").bind(&release_sid).bind(&release_sha)
            .execute(restarted.pool()).await.unwrap();
        deploy(&restarted, &msg, &release_sid, release_spec, &base, "test-secret", Duration::from_secs(10), &masker).await.unwrap();
        assert_eq!(remote.lock().unwrap().request.as_ref().unwrap()["deploy"]["revision_guard"]["expected_sha"], release_sha);
        assert_eq!(restarted.service_deployments_of(&run).await.unwrap().iter().find(|d| d.step_id == release_sid).unwrap().sha, release_sha);
        server.abort();
    }
}
