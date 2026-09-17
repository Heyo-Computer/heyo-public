//! Offline preparation and authenticated completion checks. Native admission
//! owns installation; neither command delivers or retries a legacy update POST.
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, fs::File, io::{Read, Write}, path::Path};
use crate::host_app_lb::bundle;

const LIMIT: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    operation_id: String,
    revision: String,
    config: Value,
    mapping_path: String,
    files: Vec<Change>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    path: String,
    mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preserve: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supervisor_environment: Option<bool>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Generation { boot_id: String, pid: u32, start_time: u64 }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source { disk_sha256: String, running_sha256: String, generation: Generation }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Original { path: String, sha256: Option<String>, mode: Option<u32> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspection { protocol: String, source: Source, files: Vec<Original> }

fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    ensure!(file.metadata()?.is_file() && file.metadata()?.len() <= limit as u64, "input is not a bounded regular file");
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "input exceeds byte limit");
    Ok(bytes)
}

fn absolute(path: &str) -> bool {
    Path::new(path).is_absolute() && Path::new(path).components().all(|c|
        matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_)))
}

fn prepare(plan: Plan, inspection: Inspection, archive: &[u8]) -> Result<Vec<u8>> {
    ensure!(!plan.operation_id.is_empty() && plan.operation_id.len() <= 128
        && plan.operation_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "invalid bootstrap operation ID");
    ensure!(bundle::valid_sha(&plan.revision, 40), "invalid build revision");
    ensure!(inspection.protocol == "host-app-lb-bootstrap-v1"
        && bundle::valid_sha(&inspection.source.disk_sha256, 64)
        && inspection.source.disk_sha256 == inspection.source.running_sha256
        && !inspection.source.generation.boot_id.is_empty() && inspection.source.generation.pid > 0
        && inspection.source.generation.start_time > 0, "invalid or divergent inspected source identity");
    ensure!(plan.files.len() > 0 && plan.files.len() <= 32 && plan.files.len() == inspection.files.len(), "file inspection coverage differs");
    let config = plan.config.as_object().ok_or_else(|| anyhow::anyhow!("invalid host config"))?;
    let keys = ["deployment", "namespace", "executable", "process", "state_dir", "artifact_store", "health_url", "config_files"];
    ensure!(config.len() == keys.len() && keys.iter().all(|k| config.contains_key(*k)), "host config fields differ from native protocol");
    let paths: Vec<_> = plan.files.iter().map(|f| f.path.as_str()).collect();
    ensure!(plan.config["config_files"] == json!(paths) && paths.iter().collect::<HashSet<_>>().len() == paths.len()
        && paths.contains(&plan.mapping_path.as_str()), "config_files must exactly list unique changes including mapping");
    let executable = plan.config["executable"].as_str().ok_or_else(|| anyhow::anyhow!("missing executable"))?;
    let state = plan.config["state_dir"].as_str().ok_or_else(|| anyhow::anyhow!("missing state directory"))?;
    ensure!(absolute(executable) && absolute(state) && !Path::new(executable).starts_with(state), "invalid executable/state paths");
    let mut changes = Vec::new();
    let mut decoded_total = 0;
    let mut native_edits = 0;
    for (change, original) in plan.files.iter().zip(&inspection.files) {
        ensure!(change.path == original.path && absolute(&change.path)
            && change.path != executable && !Path::new(&change.path).starts_with(state), "file path differs from inspected mapping");
        ensure!(original.sha256.as_ref().is_none_or(|s| bundle::valid_sha(s, 64))
            && original.sha256.is_some() == original.mode.is_some(), "invalid inspected file identity");
        ensure!(usize::from(change.after_base64.is_some()) + usize::from(change.preserve.is_some())
            + usize::from(change.supervisor_environment.is_some()) == 1
            && change.preserve != Some(false) && change.supervisor_environment != Some(false), "select exactly one file edit");
        if let Some(encoded) = &change.after_base64 {
            ensure!(matches!(change.mode, 0o600 | 0o644), "literal config mode must be 0600 or 0644");
            let bytes = STANDARD.decode(encoded).map_err(|_| anyhow::anyhow!("invalid encoded config"))?;
            decoded_total += bytes.len();
            ensure!(decoded_total <= LIMIT, "decoded config exceeds byte limit");
            if change.path == plan.mapping_path {
                let mapping: Value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid mapping JSON"))?;
                ensure!(mapping == plan.config, "mapping AFTER differs from requested config");
            }
        } else {
            ensure!(change.path != plan.mapping_path && original.sha256.is_some() && original.mode == Some(change.mode)
                && change.mode <= 0o777 && change.mode & 0o022 == 0, "native edits must preserve an existing file mode");
        }
        if change.supervisor_environment.is_some() {
            native_edits += 1;
            ensure!(plan.config["process"]["kind"] == "supervisor" && native_edits == 1, "native environment edit requires one Supervisor file");
        }
        let mut value = serde_json::to_value(change)?;
        value["before_sha256"] = json!(original.sha256);
        changes.push(value);
    }
    // Derive both artifact and executable identity from the actual verified
    // bundle, never from digests supplied alongside the operator plan.
    let binary = bundle::executable(archive, &plan.revision).map_err(anyhow::Error::msg)?;
    let binary_sha256 = bundle::sha(&binary);
    let mut manifest = json!({"operation_id":plan.operation_id,"helper_sha256":binary_sha256,
        "source":inspection.source,"config":plan.config,"mapping_path":plan.mapping_path,"files":changes,
        "target":{"artifact_sha256":bundle::sha(archive),"binary_sha256":binary_sha256,"revision":plan.revision}});
    manifest.sort_all_objects();
    let bytes = serde_json::to_vec(&manifest)?;
    ensure!(bytes.len() <= LIMIT, "native manifest exceeds byte limit");
    Ok(bytes)
}

pub fn run(plan: &Path, inspection: &Path, archive: &Path, output: &Path) -> Result<Value> {
    let plan: Plan = serde_json::from_slice(&read(plan, LIMIT)?).map_err(|_| anyhow::anyhow!("invalid bootstrap plan JSON"))?;
    let inspection: Inspection = serde_json::from_slice(&read(inspection, LIMIT)?).map_err(|_| anyhow::anyhow!("invalid native inspection JSON"))?;
    let manifest = prepare(plan, inspection, &read(archive, bundle::LIMIT as usize)?)?;
    let output = std::path::absolute(output)?;
    let parent = output.parent().ok_or_else(|| anyhow::anyhow!("manifest output has no parent"))?;
    // tempfile is private (0600); publish atomically without replacing a prior
    // intent, and sync the containing directory before reporting prepared.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&manifest)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(&output)?;
    File::open(parent)?.sync_all()?;
    Ok(json!({"status":"prepared","intent_sha256":bundle::sha(&manifest),"manifest_path":output}))
}

pub async fn check(path: &Path, intent: &str, alias: &str, targets: Option<&str>, token: &str) -> Result<Value> {
    ensure!(!token.trim().is_empty(), "CI_HOST_APP_LB_TOKEN is required");
    let target = crate::host_app_lb::mapping(targets, alias)?;
    let bytes = read(path, LIMIT)?;
    ensure!(bundle::valid_sha(intent, 64) && bundle::sha(&bytes) == intent, "manifest differs from recorded intent");
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid manifest JSON"))?;
    let id = manifest["operation_id"].as_str().filter(|s| !s.is_empty() && s.len() <= 128
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
        .ok_or_else(|| anyhow::anyhow!("invalid operation ID"))?;
    let revision = manifest["target"]["revision"].as_str().filter(|s| bundle::valid_sha(s, 40))
        .ok_or_else(|| anyhow::anyhow!("invalid target revision"))?;
    ensure!(manifest["config"]["deployment"] == target.deployment && manifest["config"]["namespace"] == target.namespace
        && manifest["config"]["health_url"] == target.health_url, "manifest differs from trusted target mapping");
    ensure!(manifest["source"].is_object() && manifest["target"].is_object(), "missing manifest identities");
    let state = manifest["config"]["state_dir"].as_str().filter(|s| absolute(s))
        .ok_or_else(|| anyhow::anyhow!("invalid manifest state directory"))?;
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5)).timeout(std::time::Duration::from_secs(20)).build()?;
    // A missing operation, old controller, timeout or busy helper never causes
    // an admission POST. The native GET alone may persist verified completion.
    let mut response = http.get(format!("{}/deployments/{}/update/bootstrap/{id}", target.url.trim_end_matches('/'), target.deployment))
        .bearer_auth(token).send().await.map_err(|_| anyhow::anyhow!("bootstrap lookup unavailable; no launch attempted"))?;
    ensure!(response.status().is_success(), "bootstrap lookup returned {}; no launch attempted", response.status());
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| anyhow::anyhow!("bootstrap lookup interrupted"))? {
        ensure!(body.len() + chunk.len() <= 64 * 1024, "bootstrap response exceeds byte limit");
        body.extend_from_slice(&chunk);
    }
    let receipt: Value = serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("invalid bootstrap response"))?;
    ensure!(receipt["protocol"] == "host-app-lb-bootstrap-v1" && receipt["operation_id"] == id
        && receipt["intent_sha256"] == intent && receipt["deployment"] == target.deployment && receipt["namespace"] == target.namespace
        && receipt["source"] == manifest["source"] && receipt["target"] == manifest["target"]
        && receipt["journal_path"] == json!(Path::new(state).join("bootstrap.json"))
        && receipt["unit_name"] == format!("app-lb-bootstrap-{intent}"), "bootstrap receipt identity differs");
    ensure!(receipt["status"] == "succeeded" && receipt["phase"] == "complete" && receipt["readiness_verified"] == true
        && receipt.get("error") == Some(&Value::Null), "bootstrap remains unverified; reconcile the same operation");
    let health = http.get(&target.health_url).send().await.map_err(|_| anyhow::anyhow!("public bootstrap health unavailable"))?;
    ensure!(health.status().is_success() && health.headers().get_all("x-heyo-revision").iter().count() == 1
        && health.headers().get("x-heyo-revision").and_then(|h| h.to_str().ok()) == Some(revision), "public build differs from bootstrap target");
    Ok(json!({"status":"succeeded","operation_id":id,"intent_sha256":intent,"revision":revision}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (Value, Value, Vec<u8>) {
        let config = json!({"deployment":"app-lb-host","namespace":"default","executable":"/usr/local/bin/app-lb",
            "process":{"kind":"supervisor","program":"app-lb"},"state_dir":"/var/lib/app-lb-host-update",
            "artifact_store":"https://artifacts.test","health_url":"https://admin.test/healthz",
            "config_files":["/etc/app-lb-host.json","/etc/supervisor.conf","/etc/service.env"]});
        let plan = json!({"operation_id":"bootstrap-us3-1","revision":"a".repeat(40),"config":config,
            "mapping_path":"/etc/app-lb-host.json","files":[
                {"path":"/etc/app-lb-host.json","mode":384,"after_base64":STANDARD.encode(serde_json::to_vec(&config).unwrap())},
                {"path":"/etc/supervisor.conf","mode":420,"supervisor_environment":true},
                {"path":"/etc/service.env","mode":384,"preserve":true}]});
        let inspection = json!({"protocol":"host-app-lb-bootstrap-v1","source":{
            "disk_sha256":"b".repeat(64),"running_sha256":"b".repeat(64),
            "generation":{"boot_id":"boot-1","pid":481,"start_time":913}},"files":[
                {"path":"/etc/app-lb-host.json","sha256":null,"mode":null},
                {"path":"/etc/supervisor.conf","sha256":"c".repeat(64),"mode":420},
                {"path":"/etc/service.env","sha256":"d".repeat(64),"mode":384}]});
        (plan, inspection, bundle::tests::bundle(&"a".repeat(40), false, false))
    }

    fn prepared(plan: Value, inspection: Value, archive: &[u8]) -> Result<Vec<u8>> {
        prepare(serde_json::from_value(plan)?, serde_json::from_value(inspection)?, archive)
    }

    #[test]
    fn manifest_binds_inspected_source_and_each_distinct_file_without_secret_bytes() {
        let (plan, inspection, archive) = fixture();
        let bytes = prepared(plan.clone(), inspection.clone(), &archive).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["source"], inspection["source"]);
        assert_eq!(value["config"], plan["config"]);
        assert_eq!(value["files"][0]["before_sha256"], Value::Null);
        assert_eq!(value["files"][1], json!({"path":"/etc/supervisor.conf","mode":420,
            "before_sha256":"c".repeat(64),"supervisor_environment":true}));
        assert_eq!(value["files"][2], json!({"path":"/etc/service.env","mode":384,
            "before_sha256":"d".repeat(64),"preserve":true}));
        assert_eq!(value["helper_sha256"], value["target"]["binary_sha256"]);
        assert_ne!(value["helper_sha256"], value["source"]["disk_sha256"]);
        assert_eq!(value["target"]["revision"], "a".repeat(40));
        fn sorted(v: &Value) {
            match v {
                Value::Object(map) => {
                    let keys: Vec<_> = map.keys().collect();
                    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
                    for v in map.values() { sorted(v); }
                }
                Value::Array(values) => for v in values { sorted(v); },
                _ => {}
            }
        }
        sorted(&value);
        assert_eq!(bytes, serde_json::to_vec(&value).unwrap());
    }

    #[test]
    fn mismatched_inspection_edit_or_mapping_never_produces_a_manifest() {
        for case in ["operation", "source", "coverage", "order", "mode", "absent", "mixed", "false", "mapping", "duplicate", "unknown"] {
            let (mut plan, mut inspection, archive) = fixture();
            match case {
                "operation" => plan["operation_id"] = json!("bootstrap.1"),
                "source" => inspection["source"]["running_sha256"] = json!("e".repeat(64)),
                "coverage" => { inspection["files"].as_array_mut().unwrap().pop(); }
                "order" => inspection["files"].as_array_mut().unwrap().swap(1, 2),
                "mode" => inspection["files"][1]["mode"] = json!(384),
                "absent" => { inspection["files"][2]["sha256"] = Value::Null; inspection["files"][2]["mode"] = Value::Null; }
                "mixed" => plan["files"][2]["after_base64"] = json!("eA=="),
                "false" => plan["files"][2]["preserve"] = json!(false),
                "mapping" => plan["config"]["namespace"] = json!("other"),
                "duplicate" => {
                    plan["files"][2]["path"] = json!("/etc/supervisor.conf");
                    plan["config"]["config_files"][2] = json!("/etc/supervisor.conf");
                }
                "unknown" => plan["files"][1]["before_sha256"] = json!("f".repeat(64)),
                _ => unreachable!(),
            }
            assert!(prepared(plan, inspection, &archive).is_err(), "{case}");
        }
        let (plan, inspection, _) = fixture();
        for archive in [bundle::tests::bundle(&"f".repeat(40), false, false),
            bundle::tests::bundle(&"a".repeat(40), false, true)] {
            assert!(prepared(plan.clone(), inspection.clone(), &archive).is_err());
        }
    }

    #[test]
    fn output_is_private_durable_and_never_overwrites_an_existing_intent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (plan, inspection, archive) = fixture();
        let p = dir.path().join("plan.json"); let i = dir.path().join("inspection.json");
        let a = dir.path().join("bundle.tar.gz"); let out = dir.path().join("manifest.json");
        fs::write(&p, serde_json::to_vec(&plan).unwrap()).unwrap();
        fs::write(&i, serde_json::to_vec(&inspection).unwrap()).unwrap();
        fs::write(&a, archive).unwrap();
        let status = run(&p, &i, &a, &out).unwrap();
        assert_eq!(status["status"], "prepared");
        assert_eq!(status.as_object().unwrap().len(), 3);
        assert_eq!(fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        let original = fs::read(&out).unwrap();
        assert_eq!(status["intent_sha256"], hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&original)));
        assert!(run(&p, &i, &a, &out).is_err());
        assert_eq!(fs::read(&out).unwrap(), original);
        fs::write(&p, vec![b' '; LIMIT + 1]).unwrap();
        assert!(run(&p, &i, &a, &dir.path().join("oversized.json")).is_err());
        assert!(!dir.path().join("oversized.json").exists());
    }

    #[tokio::test]
    async fn completion_requires_exact_receipt_and_health_without_relaunch_or_redirect() {
        use axum::{Router, routing::any, extract::State, http::{Request, StatusCode}, body::Body, response::IntoResponse};
        use std::sync::{Arc, Mutex};
        let remote = Arc::new(Mutex::new((String::new(), Value::Null, Vec::<String>::new())));
        let app = Router::new().fallback(any(|State(remote): State<Arc<Mutex<(String, Value, Vec<String>)>>>, request: Request<Body>| async move {
            let mut remote = remote.lock().unwrap();
            remote.2.push(format!("{} {}", request.method(), request.uri().path()));
            assert_eq!(request.method(), "GET");
            if request.uri().path() == "/healthz" {
                assert!(!request.headers().contains_key("authorization"));
                let mut response = StatusCode::OK.into_response();
                if remote.0 == "health-non2xx" { *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE; }
                if remote.0 != "health-missing" {
                    let revision = if remote.0 == "health-wrong" { "e".repeat(40) } else { "a".repeat(40) };
                    response.headers_mut().insert("x-heyo-revision", revision.parse().unwrap());
                    if remote.0 == "health-duplicate" { response.headers_mut().append("x-heyo-revision", revision.parse().unwrap()); }
                }
                return response;
            }
            assert_eq!(request.headers()["authorization"], "Bearer test-bootstrap-token");
            if remote.0 == "missing" { return StatusCode::NOT_FOUND.into_response(); }
            if remote.0 == "busy" { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
            if remote.0 == "redirect" { return (StatusCode::TEMPORARY_REDIRECT, [("location", "/sink")]).into_response(); }
            if remote.0 == "oversized" { return vec![b' '; 65537].into_response(); }
            axum::Json(remote.1.clone()).into_response()
        })).with_state(remote.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let (plan, inspection, archive) = fixture();
        let mut manifest: Value = serde_json::from_slice(&prepared(plan, inspection, &archive).unwrap()).unwrap();
        manifest["config"]["health_url"] = json!(format!("{base}/healthz"));
        let bytes = serde_json::to_vec(&manifest).unwrap(); let intent = bundle::sha(&bytes);
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("manifest.json"); fs::write(&path, &bytes).unwrap();
        let targets = json!({"us3":{"repository":"https://repo.test/repo.git","url":base,"deployment":"app-lb-host",
            "namespace":"default","health_url":format!("{base}/healthz")}}).to_string();
        let receipt = json!({"protocol":"host-app-lb-bootstrap-v1","operation_id":"bootstrap-us3-1","intent_sha256":intent,
            "deployment":"app-lb-host","namespace":"default","source":manifest["source"],"target":manifest["target"],
            "journal_path":"/var/lib/app-lb-host-update/bootstrap.json","unit_name":format!("app-lb-bootstrap-{intent}"),
            "status":"succeeded","phase":"complete","readiness_verified":true,"error":null});
        for mode in ["success", "missing", "busy", "redirect", "oversized", "intent_sha256", "namespace", "source", "target",
            "journal_path", "unit_name", "status", "phase", "readiness_verified", "error", "health-wrong", "health-missing", "health-duplicate", "health-non2xx"] {
            { let mut r = remote.lock().unwrap(); r.0 = mode.into(); r.1 = receipt.clone(); r.2.clear();
                if receipt.get(mode).is_some() { r.1[mode] = json!("wrong"); }
            }
            let result = check(&path, &intent, "us3", Some(&targets), "test-bootstrap-token").await;
            assert_eq!(result.is_ok(), mode == "success", "{mode}: {result:?}");
            let requests = &remote.lock().unwrap().2;
            assert_eq!(requests[0], "GET /deployments/app-lb-host/update/bootstrap/bootstrap-us3-1");
            assert!(requests.len() <= 2 && requests.iter().all(|s| !s.contains("/sink")));
        }
        remote.lock().unwrap().2.clear();
        assert!(check(&path, &"f".repeat(64), "us3", Some(&targets), "test-bootstrap-token").await.is_err());
        assert!(remote.lock().unwrap().2.is_empty());
        server.abort();
    }
}
