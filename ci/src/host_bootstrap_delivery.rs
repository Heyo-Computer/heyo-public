//! One designated coordinator, immutable launcher recipes, at-most-once POST.
//! A lost response fences delivery, including a crash before the actual send.
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{fs::File, io::Write, path::Path, time::Duration};
use crate::{host_app_lb::{bundle, Target}, host_bootstrap};

const STAGE: &str = include_str!("host_bootstrap_stage.py");

fn save(path: &Path, value: &Value) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

async fn body(mut response: reqwest::Response) -> Result<Value> {
    ensure!(response.status().is_success(), "launcher API returned {}", response.status());
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(bytes.len() + chunk.len() <= 8 * 1024 * 1024, "launcher response exceeds bound");
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn same_spec(actual: &Value, expected: &Value) -> bool {
    // Older controllers omit default namespace. Defaults may be materialized,
    // but no additional behavior-bearing fields are permitted on this recipe.
    let Some(fields) = actual.as_object() else { return false };
    expected.as_object().unwrap().iter().all(|(key, value)| {
        (key == "namespace" && value == "default" && actual.get(key).is_none()) || actual.get(key) == Some(value)
    }) && fields.iter().all(|(key, value)| expected.get(key).is_some()
        || matches!(key.as_str(), "health" | "scaling")
        || value.is_null() || value == &json!([]))
}

fn result(job: &Value, id: &str) -> Result<Value> {
    ensure!(job["deployment"] == id && job["kind"] == "host-update", "launcher job identity differs");
    ensure!(job["status"] == "succeeded", "launcher job is {}; reconcile without resending", job["status"]);
    let logs = job["log"].as_array().ok_or_else(|| anyhow::anyhow!("missing launcher log"))?;
    let lines: Vec<_> = logs.iter().filter_map(Value::as_str)
        .filter_map(|s| s.strip_prefix("HEYO_BOOTSTRAP_RESULT=")).collect();
    ensure!(lines.len() == 1, "missing or duplicate native result; no redelivery");
    Ok(serde_json::from_slice(&STANDARD.decode(lines[0])?)?)
}

async fn deliver(http: &reqwest::Client, target: &Target, token: &str, path: &Path, state: &mut Value) -> Result<Value> {
    let id = state["spec"]["id"].as_str().unwrap().to_owned();
    let base = target.url.trim_end_matches('/');
    if state["delivery_armed"] != true {
        let response = http.get(format!("{base}/deployments/{id}")).bearer_auth(token).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // Never mutate an existing ID. This fresh ID is saved before registration.
            body(http.post(format!("{base}/deployments")).bearer_auth(token).json(&state["spec"]).send().await?).await?;
        } else {
            ensure!(same_spec(&body(response).await?["spec"], &state["spec"]), "launcher recipe conflict");
        }
        let actual = body(http.get(format!("{base}/deployments/{id}")).bearer_auth(token).send().await?).await?;
        ensure!(same_spec(&actual["spec"], &state["spec"]), "registered launcher differs");
        state["delivery_armed"] = json!(true);
        save(path, state)?;
        // No retries/redirects. Failure or lost response leaves delivery armed.
        let job = body(http.post(format!("{base}/deployments/{id}/update")).bearer_auth(token).send().await?).await?;
        ensure!(job["deployment"] == id, "unexpected launcher job");
        state["job_id"] = job["id"].clone();
        save(path, state)?;
    }
    let job_id = state["job_id"].as_str().filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'-'|b'_')))
        .ok_or_else(|| anyhow::anyhow!("delivery armed without a receipt; use native GET reconciliation, never resend"))?;
    let job = body(http.get(format!("{base}/jobs/{job_id}")).bearer_auth(token).send().await?).await?;
    let value = result(&job, &id)?;
    state["result"] = value.clone();
    save(path, state)?;
    Ok(value)
}

pub async fn run(alias: &str, phase: &str, input: &Path, archive: &Path, journal: &Path, targets: Option<&str>, token: &str) -> Result<Value> {
    ensure!(!token.trim().is_empty() && matches!(phase,"inspect"|"admit"), "credential and inspect/admit phase required");
    let target = crate::host_app_lb::mapping(targets, alias)?;
    let original = host_bootstrap::read(input, 4 * 1024 * 1024)?;
    let value: Value = serde_json::from_slice(&original)?;
    let id = value["operation_id"].as_str().filter(|s| !s.is_empty() && s.len() <= 128
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'-'|b'_')))
        .ok_or_else(|| anyhow::anyhow!("invalid operation ID"))?;
    let config = &value["config"];
    ensure!(config["deployment"] == target.deployment && config["namespace"] == target.namespace && config["health_url"] == target.health_url,
        "bootstrap input differs from trusted target");
    let revision = if phase == "inspect" { &value["revision"] } else { &value["target"]["revision"] };
    let revision = revision.as_str().ok_or_else(|| anyhow::anyhow!("missing revision"))?;
    let archive = host_bootstrap::read(archive, bundle::LIMIT as usize)?;
    let binary = bundle::executable(&archive, revision).map_err(anyhow::Error::msg)?;
    if phase == "admit" {
        ensure!(value["target"]["artifact_sha256"] == bundle::sha(&archive) && value["target"]["binary_sha256"] == bundle::sha(&binary)
            && value["helper_sha256"] == bundle::sha(&binary), "manifest artifact differs");
    }
    let store = config["artifact_store"].as_str().ok_or_else(|| anyhow::anyhow!("missing artifact store"))?;
    crate::cd::app_lb_endpoint(store).map_err(anyhow::Error::msg)?;
    ensure!(store.starts_with("https://"), "bootstrap artifacts require public HTTPS");
    let data = if phase == "inspect" { serde_json::to_vec(config)? } else { original.clone() };
    let request = json!({"phase":phase,"operation_id":id,"input_base64":STANDARD.encode(&data),"input_sha256":bundle::sha(&data),
        "binary_sha256":bundle::sha(&binary),"artifact_sha256":bundle::sha(&archive),"artifact_size":archive.len(),
        "artifact_url":format!("{}/blobs/{}",store.trim_end_matches('/'),bundle::sha(&archive))});
    let command = format!("python3 -c \"import base64;exec(base64.b64decode('{}'))\" '{}'", STANDARD.encode(STAGE), STANDARD.encode(serde_json::to_vec(&request)?));
    ensure!(command.len() <= 100_000, "bootstrap recipe exceeds safe shell argument budget");
    let path = std::path::absolute(journal)?;
    // Exclusive lock directory is durable: a crashed coordinator needs explicit
    // local reconciliation rather than a second issuer stealing its lock.
    let lock = path.with_extension("delivery-lock");
    std::fs::create_dir(&lock).map_err(|_| anyhow::anyhow!("delivery coordinator lock exists; reconcile before continuing"))?;
    struct Guard(std::path::PathBuf);
    impl Drop for Guard { fn drop(&mut self) { let _ = std::fs::remove_dir(&self.0); } }
    let _guard = Guard(lock);
    let identity = json!({"target":target,"phase":phase,"input_sha256":bundle::sha(&original),"request":request});
    let mut state = if path.exists() {
        let state: Value = serde_json::from_slice(&host_bootstrap::read(&path, 16 * 1024 * 1024)?)?;
        ensure!(state["identity"] == identity && state["spec"]["update"]["commands"] == json!([command]), "delivery inputs changed");
        state
    } else {
        let launcher = format!("bootstrap-{}", uuid::Uuid::new_v4().simple());
        let state = json!({"identity":identity,"delivery_armed":false,"spec":{"id":launcher,"namespace":target.namespace,
            "routes":[{"host":format!("{launcher}.invalid")}],"maintenance":true,"upstreams":["bootstrap-unreachable.invalid:1"],
            "health":{"path":null,"timeout_secs":2},
            "update":{"working_dir":"/","commands":[command],"timeout_secs":180,"verify_timeout_secs":0}}});
        save(&path,&state)?;
        state
    };
    if let Some(result) = state.get("result") { return Ok(result.clone()); }
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20)).build()?;
    deliver(&http, &target, token, &path, &mut state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::{get, post}, extract::State, Json, http::StatusCode};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Server { spec: Option<Value>, registrations: usize, starts: usize, lost: bool }
    async fn lookup(State(s): State<Arc<Mutex<Server>>>) -> (StatusCode, Json<Value>) {
        match &s.lock().unwrap().spec {
            Some(spec) => (StatusCode::OK, Json(json!({"spec":spec}))),
            None => (StatusCode::NOT_FOUND, Json(json!({}))),
        }
    }
    async fn register(State(s): State<Arc<Mutex<Server>>>, Json(spec): Json<Value>) -> Json<Value> {
        let mut s = s.lock().unwrap(); s.registrations += 1; s.spec = Some(spec.clone());
        Json(json!({"spec":spec}))
    }
    async fn start(State(s): State<Arc<Mutex<Server>>>) -> (StatusCode, Json<Value>) {
        let mut s = s.lock().unwrap(); s.starts += 1;
        (if s.lost { StatusCode::BAD_GATEWAY } else { StatusCode::ACCEPTED }, Json(json!({"id":"job-1","deployment":"launcher"})))
    }
    async fn job() -> Json<Value> {
        Json(json!({"id":"job-1","deployment":"launcher","kind":"host-update","status":"succeeded",
            "log":[format!("HEYO_BOOTSTRAP_RESULT={}",STANDARD.encode(b"{\"protocol\":\"host-app-lb-bootstrap-v1\"}"))]}))
    }

    #[tokio::test]
    async fn immutable_launcher_delivery_replay_and_lost_response() {
        for lost in [false,true] {
            let server = Arc::new(Mutex::new(Server { lost, ..Default::default() }));
            let app = Router::new().route("/deployments", post(register)).route("/deployments/launcher", get(lookup))
                .route("/deployments/launcher/update",post(start)).route("/jobs/job-1",get(job)).with_state(server.clone());
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}",socket.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(socket,app).await.unwrap() });
            let target = Target { repository:"repo".into(),url,deployment:"host".into(),namespace:"default".into(),health_url:"https://health.invalid".into() };
            let temp = tempfile::tempdir().unwrap(); let path = temp.path().join("intent.json");
            let mut state = json!({"delivery_armed":false,"spec":{"id":"launcher","namespace":"default","maintenance":true}});
            save(&path,&state).unwrap();
            let http = reqwest::Client::new();
            assert_eq!(deliver(&http,&target,"token",&path,&mut state).await.is_err(),lost);
            let mut replay: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(deliver(&http,&target,"token",&path,&mut replay).await.is_err(),lost);
            assert_eq!(server.lock().unwrap().starts,1);
            assert_eq!(server.lock().unwrap().registrations,1);
            // Restart between arming and sending: absence is not permission to resend.
            replay["job_id"] = Value::Null;
            assert!(deliver(&http,&target,"token",&path,&mut replay).await.is_err());
            assert_eq!(server.lock().unwrap().starts,1);
            // Existing mismatched recipe must not be replaced or executed.
            replay["delivery_armed"] = json!(false);
            replay["spec"]["maintenance"] = json!(false);
            assert!(deliver(&http,&target,"token",&path,&mut replay).await.is_err());
            assert_eq!(server.lock().unwrap().starts,1);
            assert_eq!(server.lock().unwrap().registrations,1);
            task.abort();
        }
    }

    #[test]
    fn staging_preserves_inputs_and_rejects_conflicts() {
        // Exercise the embedded transport with disposable paths. Only ownership
        // checks are replaced because the developer test process is not root.
        let test = r#"
import tempfile
with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp)
    original_trusted = trusted
    linked = root / 'link'
    linked.symlink_to(root)
    try:
        original_trusted(linked)
        raise AssertionError('symlink accepted')
    except ValueError:
        pass
    trusted = lambda path: None
    destination = root / 'immutable'
    publish(destination, b'first', 0o600)
    publish(destination, b'first', 0o600)
    assert destination.read_bytes() == b'first'
    for data, mode in [(b'different', 0o600), (b'first', 0o700)]:
        try:
            publish(destination, data, mode)
            raise AssertionError('conflict accepted')
        except ValueError:
            pass
    assert destination.read_bytes() == b'first'
    assert NoRedirect().redirect_request(None,None,None,None,None,None) is None
"#;
        let output = std::process::Command::new("python3").arg("-c")
            .arg(format!("__name__ = 'transport_test'\n{STAGE}\n{test}")).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}
