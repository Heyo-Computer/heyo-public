//! Candidate-first updates of existing app-lb services from validated bundles.
//! Credentials and live environment values are never stored in rollout intent.
use crate::{bus::JobMessage, dispatch::Dispatcher, secrets::Masker, store::Store};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::time::Duration;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Target {
    pub url: String,
    pub deployment: String,
    pub namespace: String,
    pub mount_path: String,
    pub revision_env: String,
    pub start_command: String,
    pub working_directory: String,
}

#[derive(Deserialize, Serialize, PartialEq)]
struct Intent {
    target: Target,
    store: String,
    artifact: String,
    sha: String,
    source_revision: String,
    source_spec_sha256: String,
    target_spec_sha256: String,
}

fn digest(value: &Value) -> String {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    hex::encode(Sha256::digest(serde_json::to_vec(&canonical).expect("JSON serializes")))
}

fn desired_spec(mut spec: Value, target: &Target, store: &str, artifact: &str, sha: &str) -> Result<Value> {
    use std::path::{Component, Path};
    ensure!(spec["id"] == target.deployment && spec["namespace"].as_str().unwrap_or("default") == target.namespace,
        "registered deployment identity differs from target");
    ensure!(spec["vm"]["driver"] == "firecracker" && spec["vm"]["workspace"].is_null(),
        "service rollout requires a stateless Firecracker deployment");
    ensure!(target.mount_path.starts_with("/opt/") && Path::new(&target.mount_path).components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_))), "release mount must be an absolute /opt path without traversal");
    ensure!(target.working_directory == target.mount_path && target.start_command == format!("{}/start.sh", target.mount_path),
        "service startup must execute the validated release mount start.sh");
    ensure!(!target.revision_env.is_empty() && target.revision_env.bytes().enumerate()
        .all(|(i,b)| b == b'_' || b.is_ascii_uppercase() || (i > 0 && b.is_ascii_digit())), "invalid revision environment name");
    let vm = spec["vm"].as_object_mut().ok_or_else(|| anyhow::anyhow!("missing VM template"))?;
    let mounts = vm.entry("mounts").or_insert_with(|| json!([])).as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid artifact mounts"))?;
    let selected: Vec<_> = mounts.iter().enumerate().filter(|(_,m)| m["path"] == target.mount_path).map(|(i,_)| i).collect();
    ensure!(selected.len() <= 1, "ambiguous release mount");
    let mut mount = if let Some(index) = selected.first() {
        ensure!(mounts[*index]["store"].as_str().map(|s| s.trim_end_matches('/')) == Some(store),
            "existing release mount uses a different artifact store");
        ensure!(mounts[*index]["read_only"] == true, "release mount is writable");
        mounts[*index].clone()
    } else { json!({"path":target.mount_path,"store":store,"read_only":true}) };
    mount["ref"] = json!(artifact);
    mount["digest"] = json!(artifact);
    mount["strip_components"] = json!(1);
    if let Some(index) = selected.first() { mounts[*index] = mount; } else { mounts.push(mount); }
    vm.insert("working_directory".into(), json!(target.working_directory));
    vm.insert("start_command".into(), json!(target.start_command));
    let env = vm.entry("env_vars").or_insert_with(|| json!({})).as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid VM environment"))?;
    env.insert(target.revision_env.clone(), json!(sha));
    ensure!(!vm.get("env_from").and_then(Value::as_array).is_some_and(|refs| refs.iter()
        .any(|r| r["as"] == target.revision_env)), "secret reference would override deployment revision");
    Ok(spec)
}

fn verify(body: &Value, id: &str, intent: &Intent) -> Result<bool> {
    ensure!(body["operation_id"] == id && body["deployment"] == intent.target.deployment
        && body["source_revision"] == intent.source_revision && body["target_spec_sha256"] == intent.target_spec_sha256,
        "app-lb rollout identity or target configuration mismatch");
    match body["status"].as_str() {
        Some("running") => Ok(false),
        Some("succeeded") => {
            ensure!(body["readiness_verified"] == true && body["previous_stopped"] == true,
                "rollout completed without verified readiness and previous-generation retirement");
            Ok(true)
        }
        _ => anyhow::bail!("app-lb rollout failed or requires reconciliation"),
    }
}

/// Every replay uses the same persisted source revision and desired fingerprint.
/// A missing operation can be POSTed only while that exact source still exists.
pub async fn deploy(d: &Dispatcher, msg: &JobMessage, step: &str, target: Target,
    token: &str, workflow: &str, artifact: &str, timeout: Duration, masker: &Masker) -> Result<String> {
    crate::submission::authorize_publication(&d.store, &msg.run_id).await.map_err(anyhow::Error::msg)?;
    ensure!(!token.trim().is_empty(), "app-lb rollout requires a secret credential");
    ensure!(Some(target.deployment.as_str()) != d.config.controller_deployment.as_deref(),
        "use deferred controller deployment for the running CI service");
    ensure!(!target.deployment.is_empty() && target.deployment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid deployment identifier");
    let base = crate::cd::app_lb_endpoint(&target.url).map_err(anyhow::Error::msg)?;
    let release = crate::release::get(&d.store, &msg.run_id).await.map_err(anyhow::Error::msg)?
        .filter(|r| r.status == "published").ok_or_else(|| anyhow::anyhow!("rollout requires confirmed release"))?;
    let run = d.store.get_run(&msg.run_id).await?.ok_or_else(|| anyhow::anyhow!("missing run"))?;
    ensure!(release.prepared.release_sha == run.sha, "validated bundle must match the exact merged revision");
    let stored = crate::submission::artifact(&d.store, &msg.run_id, workflow, artifact, None).await.map_err(anyhow::Error::msg)?;
    ensure!(stored.sink == "artifacts", "bundle rollout requires the HTTP artifact store");
    let blob = stored.digest.clone().ok_or_else(|| anyhow::anyhow!("missing artifact digest"))?;
    let store = d.config.artifacts.as_ref().ok_or_else(|| anyhow::anyhow!("missing artifact store"))?.url.trim_end_matches('/').to_string();
    let id = format!("ci-service-{}", hex::encode(Sha256::digest(step.as_bytes())));
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none()).build()?;
    let endpoint = format!("{base}/deployments/{}", target.deployment);
    let existing: Option<Value> = sqlx::query_scalar("SELECT intent FROM ci_service_rollout WHERE id=$1")
        .bind(&id).fetch_optional(d.store.pool()).await?;
    let intent: Intent = if let Some(existing) = existing {
        let saved: Intent = serde_json::from_value(existing)?;
        ensure!(saved.target == target && saved.artifact == blob && saved.store == store && saved.sha == run.sha,
            "service rollout inputs changed on replay");
        saved
    } else {
        ensure!(stored.size_bytes <= 512 * 1024 * 1024, "service bundle exceeds verification budget");
        let bytes = d.artifacts.get(&stored).await?;
        ensure!(hex::encode(Sha256::digest(&bytes)) == blob, "validated artifact digest mismatch");
        // Every runtime executes this same archive-owned entry point.
        let start = crate::service_archive::validated_archive(&bytes, "dist/start.sh").map_err(anyhow::Error::msg)?;
        ensure!(start.starts_with(b"#!"), "service bundle lacks an executable startup script");
        let snapshot: Value = http.get(&endpoint).bearer_auth(token).send().await?.error_for_status()?.json().await?;
        let revision = snapshot["rollout_revision"].as_str().filter(|r| !r.is_empty())
            .ok_or_else(|| anyhow::anyhow!("app-lb lacks candidate-first rollout support"))?.to_string();
        let wanted = desired_spec(snapshot["spec"].clone(), &target, &store, &blob, &run.sha)?;
        let intent = Intent { target, store, artifact: blob, sha: run.sha.clone(), source_revision: revision,
            source_spec_sha256: digest(&snapshot["spec"]), target_spec_sha256: digest(&wanted) };
        let value = serde_json::to_value(&intent)?;
        let mut tx = d.store.pool().begin().await?;
        // Serialize cancellation and concurrent delivery admission.
        let status: String = sqlx::query_scalar("SELECT status FROM ci_run WHERE id=$1 FOR UPDATE")
            .bind(&msg.run_id).fetch_one(&mut *tx).await?;
        ensure!(!matches!(status.as_str(), "cancelled" | "failure"), "run stopped before rollout");
        sqlx::query("INSERT INTO ci_service_deployment(id,step_id,run_id,job_id,service_id,request_hash,status,sha,git_ref) SELECT $1,$2,$3,$4,$5,$6,'submitting',$7,$8 WHERE EXISTS(SELECT 1 FROM ci_job WHERE id=$4 AND status='running') ON CONFLICT(step_id) DO NOTHING")
            .bind(&id).bind(step).bind(&msg.run_id).bind(&msg.job_id).bind(&intent.target.deployment)
            .bind(digest(&value)).bind(&run.sha).bind(&release.prepared.git_ref).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO ci_service_rollout(id,intent,deadline) VALUES($1,$2,now()+make_interval(secs=>$3)) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(&value).bind(timeout.min(d.config.max_job_duration).as_secs() as f64).execute(&mut *tx).await?;
        let saved: Value = sqlx::query_scalar("SELECT intent FROM ci_service_rollout WHERE id=$1").bind(&id).fetch_one(&mut *tx).await?;
        ensure!(saved == value, "concurrent rollout intent differs");
        Store::add_service_deployment_event(&mut tx, &id).await?;
        tx.commit().await?;
        intent
    };
    reconcile(&d.store, msg, &id, &intent, token, masker).await
}

async fn reconcile(store: &Store, msg: &JobMessage, id: &str, intent: &Intent,
    token: &str, masker: &Masker) -> Result<String> {
    let base = crate::cd::app_lb_endpoint(&intent.target.url).map_err(anyhow::Error::msg)?;
    let endpoint = format!("{base}/deployments/{}", intent.target.deployment);
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none()).build()?;
    loop {
        let row = sqlx::query("SELECT r.deadline,d.status FROM ci_service_rollout r JOIN ci_service_deployment d ON d.id=r.id WHERE r.id=$1")
            .bind(id).fetch_one(store.pool()).await?;
        match row.get::<String,_>("status").as_str() {
            "passed" => return Ok(format!("[ci] {} candidate verified and previous generation stopped at {}\n", intent.target.deployment, intent.sha)),
            "failed" => anyhow::bail!("service rollout failed; reconcile operation {id}"),
            _ => {}
        }
        let expired = row.get::<chrono::DateTime<chrono::Utc>,_>("deadline") <= chrono::Utc::now();
        if expired || store.is_job_cancelled(&msg.job_id).await? {
            anyhow::bail!("CI stopped waiting; remote rollout may continue. Reconcile operation {id} before another deployment");
        }
        let attempt: Result<Option<bool>> = async {
            let response = http.get(format!("{endpoint}/rollouts/{id}")).bearer_auth(token).send().await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                let current: Value = http.get(&endpoint).bearer_auth(token).send().await?.error_for_status()?.json().await?;
                ensure!(current["rollout_revision"] == intent.source_revision && digest(&current["spec"]) == intent.source_spec_sha256,
                    "source changed before rollout acceptance; refusing replacement");
                let wanted = desired_spec(current["spec"].clone(), &intent.target, &intent.store, &intent.artifact, &intent.sha)?;
                ensure!(digest(&wanted) == intent.target_spec_sha256, "desired configuration changed");
                let response = http.post(format!("{endpoint}/rollouts")).bearer_auth(token)
                    .json(&json!({"operation_id":id,"expected_revision":intent.source_revision,"spec":wanted})).send().await?;
                ensure!(!response.status().is_client_error() && !response.status().is_redirection(), "app-lb refused candidate-first rollout");
                if !response.status().is_success() { return Ok(None); }
                // Admission is never deployment success; only the next exact GET counts.
                return Ok(Some(false));
            }
            ensure!(!response.status().is_client_error() && !response.status().is_redirection(), "app-lb refused rollout lookup");
            if !response.status().is_success() { return Ok(None); }
            Ok(Some(verify(&response.json::<Value>().await?, id, intent)?))
        }.await;
        // Cancellation and the durable deadline also fence a slow successful GET.
        if store.is_job_cancelled(&msg.job_id).await? || row.get::<chrono::DateTime<chrono::Utc>,_>("deadline") <= chrono::Utc::now() {
            anyhow::bail!("CI stopped waiting; reconcile operation {id}");
        }
        match attempt {
            Ok(Some(true)) => {
                // Same run -> job lock order as cancellation. A late health
                // response cannot mark a cancelled or timed-out rollout passed.
                let mut tx = store.pool().begin().await?;
                let run_status: String = sqlx::query_scalar("SELECT status FROM ci_run WHERE id=$1 FOR UPDATE")
                    .bind(&msg.run_id).fetch_one(&mut *tx).await?;
                let job_status: String = sqlx::query_scalar("SELECT status FROM ci_job WHERE id=$1 FOR UPDATE")
                    .bind(&msg.job_id).fetch_one(&mut *tx).await?;
                ensure!(!matches!(run_status.as_str(), "cancelled" | "failure") && job_status == "running",
                    "CI stopped waiting; reconcile operation {id}");
                ensure!(row.get::<chrono::DateTime<chrono::Utc>,_>("deadline") > chrono::Utc::now(),
                    "CI deadline elapsed; reconcile operation {id}");
                let changed = sqlx::query("UPDATE ci_service_deployment SET status='passed',phase='complete',message='Exact candidate healthy; previous generation stopped.',error=NULL,updated_at=now() WHERE id=$1 AND status NOT IN ('passed','failed')")
                    .bind(id).execute(&mut *tx).await?.rows_affected();
                if changed == 1 { Store::add_service_deployment_event(&mut tx, id).await?; }
                tx.commit().await?;
            }
            Ok(Some(false)) => store.update_service_deployment(id, "running", Some("rollout"), Some("Waiting for candidate readiness, cutover and drain."), None).await?,
            Ok(None) => {}
            Err(error) if error.is::<reqwest::Error>() => {}
            Err(error) => {
                let note = masker.mask(&error.to_string()).replace(token, "***");
                store.update_service_deployment(id, "failed", Some("reconciliation"), None, Some(&note)).await?;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target { url: "https://admin.test".into(), deployment: "cloud-eu1".into(), namespace: "default".into(),
            mount_path: "/opt/cloud-release".into(), revision_env: "HEYO_CLOUD_DEPLOYMENT_GIT_SHA".into(),
            start_command: "/opt/cloud-release/start.sh".into(), working_directory: "/opt/cloud-release".into() }
    }

    fn current() -> Value {
        json!({"id":"cloud-eu1","routes":[{"host":"cloud.test"}],"health":{"path":"/health","timeout_secs":5},
            "vm":{"driver":"firecracker","image":"pinned-runtime","port":8080,"env_vars":{"PORT":"8080"},
                "env_from":[{"secret":"shared","key":"database-url","as":"DATABASE_URL"}],
                "mounts":[{"path":"/opt/cloud-release","store":"https://art.test","ref":"old","digest":"old","read_only":true,
                    "auth":{"secret":"art","key":"token"}},
                    {"path":"/opt/other","store":"https://other.test","ref":"unrelated","read_only":true}]}})
    }

    #[test]
    fn rollout_preserves_routes_secrets_runtime_and_unrelated_mounts() {
        let before = current();
        let after = desired_spec(before.clone(), &target(), "https://art.test", "new-bundle", "new-sha").unwrap();
        for key in ["routes", "health"] { assert_eq!(before[key], after[key]); }
        for key in ["driver", "image", "port", "env_from"] { assert_eq!(before["vm"][key], after["vm"][key]); }
        assert_eq!(after["vm"]["env_vars"]["PORT"], "8080");
        assert_eq!(after["vm"]["env_vars"]["HEYO_CLOUD_DEPLOYMENT_GIT_SHA"], "new-sha");
        assert_eq!(after["vm"]["mounts"][0]["ref"], "new-bundle");
        assert_eq!(after["vm"]["mounts"][0]["digest"], "new-bundle");
        assert_eq!(after["vm"]["mounts"][0]["auth"], before["vm"]["mounts"][0]["auth"]);
        assert_eq!(after["vm"]["mounts"][1], before["vm"]["mounts"][1]);
        assert_eq!(after["vm"]["working_directory"], "/opt/cloud-release");
        assert_eq!(after["vm"]["start_command"], "/opt/cloud-release/start.sh");
        let mut without = before.clone(); without["vm"].as_object_mut().unwrap().remove("mounts");
        let added = desired_spec(without, &target(), "https://art.test", "new-bundle", "new-sha").unwrap();
        assert_eq!(added["vm"]["mounts"].as_array().unwrap().len(), 1);
        assert_eq!(added["vm"]["mounts"][0]["read_only"], true);
        assert_ne!(digest(&before), digest(&after));
        let mut shuffled = after.clone(); shuffled.sort_all_objects();
        assert_eq!(digest(&after), digest(&shuffled));
    }

    #[test]
    fn rollout_rejects_wrong_identity_stateful_and_ambiguous_templates() {
        for (key, value) in [("id",json!("other")),("namespace",json!("other"))] {
            let mut spec = current(); spec[key] = value;
            assert!(desired_spec(spec, &target(), "https://art.test", "bundle", "sha").is_err());
        }
        for (key, value) in [("workspace",json!({"store":"data"})),("driver",json!("libvirt"))] {
            let mut spec = current(); spec["vm"][key] = value;
            assert!(desired_spec(spec, &target(), "https://art.test", "bundle", "sha").is_err());
        }
        let mut spec = current(); spec["vm"]["mounts"][0]["read_only"] = json!(false);
        assert!(desired_spec(spec, &target(), "https://art.test", "bundle", "sha").is_err());
        assert!(desired_spec(current(), &target(), "https://different-store.test", "bundle", "sha").is_err());
        let mut spec = current(); let duplicate = spec["vm"]["mounts"][0].clone();
        spec["vm"]["mounts"].as_array_mut().unwrap().push(duplicate);
        assert!(desired_spec(spec, &target(), "https://art.test", "bundle", "sha").is_err());
        let mut spec = current(); spec["vm"]["env_from"][0]["as"] = json!(target().revision_env);
        assert!(desired_spec(spec, &target(), "https://art.test", "bundle", "sha").is_err());
        for path in ["/opt/../data", "relative/path", "/data"] {
            let mut bad = target(); bad.mount_path = path.into();
            assert!(desired_spec(current(), &bad, "https://art.test", "bundle", "sha").is_err());
        }
    }

    #[test]
    fn admission_and_partial_completion_are_not_success() {
        let intent = Intent { target: target(), store: "store".into(), artifact: "artifact".into(), sha: "sha".into(),
            source_revision: "generation-3".into(), source_spec_sha256: "source".into(), target_spec_sha256: "desired".into() };
        let response = json!({"operation_id":"op","deployment":"cloud-eu1","source_revision":"generation-3",
            "target_spec_sha256":"desired","status":"succeeded","readiness_verified":true,"previous_stopped":true});
        assert!(verify(&response, "op", &intent).unwrap());
        for key in ["operation_id","deployment","source_revision","target_spec_sha256"] {
            let mut wrong = response.clone(); wrong[key] = json!("other");
            assert!(verify(&wrong, "op", &intent).is_err(), "{key}");
        }
        for key in ["readiness_verified","previous_stopped"] {
            let mut incomplete = response.clone(); incomplete[key] = json!(false);
            assert!(verify(&incomplete, "op", &intent).is_err());
        }
        let mut pending = response.clone(); pending["status"] = json!("running");
        assert!(!verify(&pending, "op", &intent).unwrap());
        for status in ["accepted","failed","reconciliation_required","success"] {
            let mut wrong = response.clone(); wrong["status"] = json!(status);
            assert!(verify(&wrong, "op", &intent).is_err());
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable CI_TEST_DATABASE_URL; fake app-lb HTTP"]
    async fn reconciliation_survives_response_loss_and_refuses_uncertain_success() {
        use axum::{Json, Router, extract::{Path, State}, http::StatusCode, response::IntoResponse, routing::{get, post}};
        use std::sync::{Arc, Mutex};
        struct Remote { posts: Vec<Value>, result: Option<Value>, hidden: bool, mode: &'static str }
        let remote = Arc::new(Mutex::new(Remote { posts: vec![], result: None, hidden: false, mode: "success" }));
        let app = Router::new()
            .route("/deployments/cloud-eu1", get(|State(state): State<Arc<Mutex<Remote>>>| async move {
                let mode = state.lock().unwrap().mode;
                Json(json!({"rollout_revision":if mode == "stale" { "other" } else { "generation-3" },"spec":current()}))
            }))
            .route("/deployments/cloud-eu1/rollouts", post(|State(state): State<Arc<Mutex<Remote>>>, Json(body): Json<Value>| async move {
                assert_eq!(body["expected_revision"], "generation-3");
                assert_eq!(body["spec"]["vm"]["mounts"][0]["digest"], "bundle");
                assert_eq!(body["spec"]["routes"], json!([{"host":"cloud.test"}]));
                let mut state = state.lock().unwrap();
                state.posts.push(body.clone());
                state.result = Some(json!({"operation_id":body["operation_id"],"deployment":"cloud-eu1",
                    "source_revision":"generation-3","target_spec_sha256":digest(&body["spec"]),"status":"succeeded",
                    "readiness_verified":true,"previous_stopped":true}));
                // The server accepted the operation but the client cannot know it.
                (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"lost response"})))
            }))
            .route("/deployments/cloud-eu1/rollouts/{id}", get(|State(state): State<Arc<Mutex<Remote>>>, Path(_): Path<String>| async move {
                let mut state = state.lock().unwrap();
                if state.mode == "redirect" { return StatusCode::TEMPORARY_REDIRECT.into_response(); }
                if state.hidden { state.hidden = false; return StatusCode::NOT_FOUND.into_response(); }
                match state.result.clone() {
                    Some(mut result) => {
                        if state.mode == "identity" { result["target_spec_sha256"] = json!("wrong"); }
                        if state.mode == "partial" { result["previous_stopped"] = json!(false); }
                        if state.mode == "failed" { result["status"] = json!("failed"); }
                        Json(result).into_response()
                    }
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            })).with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let workspace = tempfile::tempdir().unwrap();
        let db = std::env::var("CI_TEST_DATABASE_URL").unwrap();
        let store = Store::connect(&db, workspace.path().into(), Duration::from_secs(30)).await.unwrap();
        store.migrate().await.unwrap();
        let plan = crate::plan::Plan::build(&crate::workflow::Workflow::parse("test.yml",
            "jobs:\n  deploy:\n    steps: [{uses: ci/rollout-service}]\n").unwrap()).unwrap();
        let masker = Masker::new(["test-token"].into_iter());
        for mode in ["success", "lost", "identity", "partial", "failed", "stale", "redirect", "cancelled", "expired"] {
            *remote.lock().unwrap() = Remote { posts: vec![], result: None, hidden: false, mode };
            let run = crate::vm::new_id();
            store.create_run(&run, &crate::store::RunRequest { repo_url:"https://repo.test/source.git".into(),
                git_ref:"refs/heads/main".into(),sha:"a".repeat(40),..Default::default() }, &plan).await.unwrap();
            let job = store.jobs_of(&run).await.unwrap().remove(0);
            store.set_job_status(&job.id, crate::store::JobStatus::Running, None).await.unwrap();
            let step = crate::store::step_id(&job.id, 0);
            store.create_step(&step, &job.id, 0, "Rollout", Some("ci/rollout-service")).await.unwrap();
            let msg = JobMessage { run_id:run.clone(),job_id:job.id.clone(),job_key:job.job_key.clone() };
            let id = format!("op-{run}");
            let mut target = target(); target.url = base.clone();
            let desired = desired_spec(current(), &target, "https://art.test", "bundle", "sha").unwrap();
            let intent = Intent { target,store:"https://art.test".into(),artifact:"bundle".into(),sha:"sha".into(),
                source_revision:"generation-3".into(),source_spec_sha256:digest(&current()),target_spec_sha256:digest(&desired) };
            store.begin_service_deployment(&id, &step, "cloud-eu1", "request").await.unwrap();
            sqlx::query("INSERT INTO ci_service_rollout(id,intent,deadline) VALUES($1,$2,now()+interval '30 seconds')")
                .bind(&id).bind(serde_json::to_value(&intent).unwrap()).execute(store.pool()).await.unwrap();
            if mode == "cancelled" { store.cancel_run(&run).await.unwrap(); }
            if mode == "expired" { sqlx::query("UPDATE ci_service_rollout SET deadline=now()-interval '1 second' WHERE id=$1")
                .bind(&id).execute(store.pool()).await.unwrap(); }
            if mode == "lost" {
                assert!(tokio::time::timeout(Duration::from_millis(500), reconcile(&store, &msg, &id, &intent, "test-token", &masker)).await.is_err());
                assert_eq!(remote.lock().unwrap().posts.len(), 1);
                remote.lock().unwrap().hidden = true;
            }
            let restarted = Store::connect(&db, workspace.path().into(), Duration::from_secs(30)).await.unwrap();
            let result = reconcile(&restarted, &msg, &id, &intent, "test-token", &masker).await;
            let success = matches!(mode, "success" | "lost");
            assert_eq!(result.is_ok(), success, "{mode}: {result:?}");
            let status = store.service_deployments_of(&run).await.unwrap().remove(0).status;
            assert_eq!(status == "passed", success, "{mode}");
            if matches!(mode, "stale" | "redirect" | "cancelled" | "expired") {
                assert!(remote.lock().unwrap().posts.is_empty(), "{mode}");
            }
            if mode == "lost" {
                let state = remote.lock().unwrap();
                assert_eq!(state.posts.len(), 2);
                assert_eq!(state.posts[0], state.posts[1], "retry must reuse exact persisted identity and payload");
            }
            if success {
                let count = remote.lock().unwrap().posts.len();
                reconcile(&restarted, &msg, &id, &intent, "test-token", &masker).await.unwrap();
                assert_eq!(remote.lock().unwrap().posts.len(), count);
            }
        }
        server.abort();
    }
}
