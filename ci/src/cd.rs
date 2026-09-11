//! Service rollout through orchestrator, never through raw VM management.
//!
//! Orchestrator's POST is not idempotent. Persist our one submission attempt
//! before sending it; after any uncertain outcome reconcile only by GET.
use crate::{bus::JobMessage, secrets::Masker, store::Store};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

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
