//! Opt-in systemd self-update. The operator file, not the deployment recipe or
//! request, chooses every host path and command. No VM API is used here.
use crate::host_bundle::{self, sha, valid_sha};
use serde::{Deserialize, Serialize};
use std::{fs::{self, File, OpenOptions}, io::Write, os::unix::fs::PermissionsExt, path::{Path, PathBuf}, time::Duration};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub deployment: String,
    pub namespace: String,
    pub executable: PathBuf,
    pub process: Process,
    pub state_dir: PathBuf,
    pub artifact_store: String,
    pub health_url: String,
    /// Unit/environment configuration files, NOT mutable VM/workspace state.
    pub config_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Process {
    Systemd { unit: String },
    Supervisor { program: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub operation_id: String,
    pub expected_binary_sha256: String,
    pub expected_config_sha256: String,
    pub artifact_sha256: String,
    pub binary_sha256: String,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub request: Request,
    pub deployment: String,
    pub namespace: String,
    pub status: String,
    pub phase: String,
    pub error: Option<String>,
    pub source_invocation: String,
    pub readiness_verified: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger { operations: Vec<Operation> }

pub fn endpoint(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).map_err(|e| e.to_string())?;
    let local_test = cfg!(test) && url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
    if !(url.scheme() == "https" || local_test) || url.host_str().is_none() || !url.username().is_empty()
        || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("host-update URLs require HTTPS without URL credentials, query or fragment".into());
    }
    Ok(url)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn configured() -> Result<(PathBuf, Config)> {
    let path = PathBuf::from(std::env::var("APP_LB_HOST_UPDATE_CONFIG").map_err(|_| "host rollout is not configured")?);
    Ok((path.clone(), load_config(&path)?))
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.is_absolute() { return Err("host mapping path must be absolute".into()); }
    let config: Config = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let valid_process = match &config.process {
        Process::Systemd { unit } => !unit.starts_with('-') && unit.ends_with(".service") && safe_id(unit.trim_end_matches(".service")),
        Process::Supervisor { program } => safe_id(program) && !program.starts_with('-') && program != "all",
    };
    if !safe_id(&config.deployment) || config.namespace.is_empty() || !valid_process || config.config_files.is_empty() {
        return Err("invalid host-update process mapping".into());
    }
    for path in [&config.executable, &config.state_dir].into_iter().chain(config.config_files.iter()) {
        if !path.is_absolute() || !path.components().all(|c| matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_))) {
            return Err("host mapping requires absolute paths without traversal".into());
        }
    }
    endpoint(&config.artifact_store)?; endpoint(&config.health_url)?;
    Ok(config)
}

fn lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    File::open(dir.parent().ok_or("missing state parent")?).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock")).map_err(|e| e.to_string())?;
    file.try_lock().map_err(|_| "host update is busy".to_string())?;
    Ok(file)
}

fn ledger(config: &Config) -> Result<Ledger> {
    match fs::read(config.state_dir.join("operations.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(e.to_string()),
    }
}

fn atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let temp = path.with_extension("writing");
    let mut file = OpenOptions::new().create(true).truncate(true).write(true).open(&temp).map_err(|e| e.to_string())?;
    file.set_permissions(fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
    fs::rename(temp, path).map_err(|e| e.to_string())?;
    File::open(path.parent().ok_or("missing parent")?).and_then(|f| f.sync_all()).map_err(|e| e.to_string())
}

fn persist(config: &Config, state: &Ledger) -> Result<()> {
    atomic(&config.state_dir.join("operations.json"), &serde_json::to_vec(state).map_err(|e| e.to_string())?, 0o600)
}

fn file_sha(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    std::io::copy(&mut File::open(path).map_err(|e| e.to_string())?, &mut hash).map_err(|e| e.to_string())?;
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
tokio::task_local! { static TEST_HOST: PathBuf; }

fn running_path() -> PathBuf {
    #[cfg(test)]
    if let Ok(root) = TEST_HOST.try_with(Clone::clone) { return root.join("running"); }
    PathBuf::from("/proc/self/exe")
}

fn running_sha() -> Result<String> { file_sha(&running_path()) }

fn compiled_revision() -> String {
    #[cfg(test)]
    if let Ok(root) = TEST_HOST.try_with(Clone::clone) { return fs::read_to_string(root.join("revision")).unwrap(); }
    env!("APP_LB_BUILD_REVISION").into()
}

async fn command(program: &str, args: &[&str]) -> Result<String> {
    #[cfg(test)]
    let fake = TEST_HOST.try_with(|root| root.join(Path::new(program).file_name().unwrap())).ok();
    #[cfg(test)]
    let program = fake.as_ref().and_then(|p| p.to_str()).unwrap_or(program);
    let mut cmd = tokio::process::Command::new(program);
    // Supervisor's default config discovery includes cwd. Admission and the
    // independently launched helper must use the identical fixed directory.
    cmd.args(args).current_dir("/").kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(45), cmd.output()).await
        .map_err(|_| "systemd command timed out; outcome unknown")?.map_err(|e| e.to_string())?;
    if !output.status.success() { return Err(format!("systemd command failed ({})", output.status)); }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

async fn unit(config: &Config) -> Result<(String, String)> {
    // ExecStart's show representation embeds runtime PID/timestamps. Hash the
    // unit text plus stable loaded properties, never those volatile fields.
    let config_text = match &config.process {
        Process::Systemd { unit } => {
            let mut text = command("/usr/bin/systemctl", &["show", unit, "--property=LoadState,FragmentPath,DropInPaths,User,Group,Environment,EnvironmentFiles,WorkingDirectory,RootDirectory,RootImage"] ).await?;
            if !text.lines().any(|l| l == "LoadState=loaded") { return Err("mapped systemd unit is not loaded".into()); }
            text.push_str(&command("/usr/bin/systemctl", &["cat", unit]).await?);
            text
        }
        Process::Supervisor { .. } => String::new(), // explicit config_files cover Supervisor and environment
    };
    let pid = process_pid(config).await?;
    let process = process_path(pid);
    let stat = fs::read_to_string(process.join("stat")).map_err(|e| e.to_string())?;
    let started = stat.rsplit_once(')').and_then(|(_,rest)| rest.split_whitespace().nth(19))
        .filter(|value| value.parse::<u64>().is_ok()).ok_or("invalid process start-time evidence")?;
    Ok((config_text, format!("{pid}:{started}")))
}

fn process_path(pid: u32) -> PathBuf {
    #[cfg(test)]
    if let Ok(root) = TEST_HOST.try_with(Clone::clone) { return root.join("proc"); }
    PathBuf::from(format!("/proc/{pid}"))
}

async fn process_pid(config: &Config) -> Result<u32> {
    let text = match &config.process {
        Process::Systemd { unit } => command("/usr/bin/systemctl", &["show", unit, "--property=MainPID", "--value"]).await?,
        Process::Supervisor { program } => command("/usr/bin/supervisorctl", &["pid", program]).await?,
    };
    text.trim().parse::<u32>().ok().filter(|p| *p > 0).ok_or("mapped service has no running process".into())
}

async fn is_service_process(config: &Config) -> Result<bool> {
    Ok(process_pid(config).await? == std::process::id())
}

fn config_sha(config: &Config, unit: &str) -> Result<String> {
    let mut bytes = serde_json::to_vec(config).map_err(|e| e.to_string())?;
    bytes.extend_from_slice(unit.as_bytes());
    for path in &config.config_files { bytes.extend_from_slice(file_sha(path)?.as_bytes()); }
    Ok(sha(&bytes))
}

pub async fn snapshot(config: &Config) -> Result<serde_json::Value> {
    let (unit, _) = unit(config).await?;
    let binary = file_sha(&config.executable)?;
    if binary != running_sha()? || !is_service_process(config).await? { return Err("mapped executable/unit is not this running controller".into()); }
    Ok(serde_json::json!({"deployment":config.deployment,"namespace":config.namespace,
        "binary_sha256":binary,"config_sha256":config_sha(config,&unit)?,"health_url":config.health_url,
        "artifact_store":config.artifact_store,"protocol":"host-app-lb-v1"}))
}

fn admit(config: &Config, request: Request, binary: &str, fingerprint: &str, invocation: &str) -> Result<(Operation, bool)> {
    let _lock = lock(&config.state_dir)?;
    let mut state = ledger(config)?;
    if let Some(old) = state.operations.iter().find(|o| o.request.operation_id == request.operation_id) {
        return if old.request == request && old.deployment == config.deployment && old.namespace == config.namespace { Ok((old.clone(), false)) } else { Err("operation payload conflicts".into()) };
    }
    if !safe_id(&request.operation_id) || !valid_sha(&request.revision, 40)
        || [&request.expected_binary_sha256, &request.expected_config_sha256, &request.artifact_sha256, &request.binary_sha256].iter().any(|s| !valid_sha(s, 64)) {
        return Err("invalid operation or artifact identity".into());
    }
    if state.operations.last().is_some_and(|o| o.status != "succeeded") {
        return Err("prior host update requires reconciliation".into());
    }
    if binary != request.expected_binary_sha256 || fingerprint != request.expected_config_sha256 {
        return Err("host executable or configuration changed".into());
    }
    let operation = Operation { request, deployment: config.deployment.clone(), namespace: config.namespace.clone(),
        status:"running".into(),phase:"accepted".into(),error:None,source_invocation:invocation.into(),readiness_verified:false };
    state.operations.push(operation.clone()); persist(config, &state)?;
    Ok((operation, true))
}

fn update(config: &Config, id: &str, change: impl FnOnce(&mut Operation)) -> Result<Operation> {
    let _lock = lock(&config.state_dir)?;
    let mut state = ledger(config)?;
    let op = state.operations.last_mut().filter(|o| o.request.operation_id == id).ok_or("operation is no longer current")?;
    change(op); let out = op.clone(); persist(config, &state)?; Ok(out)
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120)).build().map_err(|e| e.to_string())
}

async fn download(config: &Config, request: &Request) -> Result<Vec<u8>> {
    let url = format!("{}/blobs/{}", config.artifact_store.trim_end_matches('/'), request.artifact_sha256);
    let mut response = http()?.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() { return Err("artifact download refused; redirects forbidden".into()); }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if bytes.len() as u64 + chunk.len() as u64 > host_bundle::LIMIT { return Err("artifact exceeds budget".into()); }
        bytes.extend_from_slice(&chunk);
    }
    if sha(&bytes) != request.artifact_sha256 { return Err("artifact archive checksum mismatch".into()); }
    let binary = host_bundle::executable(&bytes, &request.revision)?;
    if sha(&binary) != request.binary_sha256 { return Err("artifact executable checksum mismatch".into()); }
    Ok(binary)
}

/// Replay never relaunches. A persisted launch with no process evidence needs an
/// operator, not a guessed retry. The helper unit name is never recycled here.
pub async fn start(path: &Path, config: &Config, request: Request) -> Result<Operation> {
    if let Some(old) = ledger(config)?.operations.iter().find(|o| o.request.operation_id == request.operation_id) {
        return if old.request == request && old.deployment == config.deployment && old.namespace == config.namespace { Ok(old.clone()) } else { Err("operation payload conflicts".into()) };
    }
    let (unit, invocation) = unit(config).await?;
    let fingerprint = config_sha(config, &unit)?;
    let source = running_sha()?;
    if file_sha(&config.executable)? != source || !is_service_process(config).await? { return Err("mapped executable/unit differs from running controller".into()); }
    let (op, inserted) = admit(config, request, &source, &fingerprint, &invocation)?;
    if !inserted { return Ok(op); }
    let work = stage(path.to_path_buf(), config.clone(), op.clone(), source);
    #[cfg(not(test))]
    tokio::spawn(work);
    #[cfg(test)]
    {
        let root = TEST_HOST.try_with(Clone::clone).ok();
        tokio::spawn(async move { if let Some(root) = root { TEST_HOST.scope(root, work).await } else { work.await } });
    }
    Ok(op)
}

async fn stage(path: PathBuf, config: Config, op: Operation, source: String) {
    let config = &config;
    // Stage only after durable admission. An interrupted stage remains fenced.
    let result: Result<()> = async {
        let binary = download(config, &op.request).await?;
        let _guard = lock(&config.state_dir)?;
        let current = ledger(config)?;
        if current.operations.last().is_none_or(|o| o.request != op.request || o.phase != "accepted") { return Err("operation changed while staging".into()); }
        let candidate = config.state_dir.join(format!("{}.candidate", op.request.operation_id));
        atomic(&candidate, &binary, 0o755)?;
        let helper = config.state_dir.join(format!("{}.previous", op.request.operation_id));
        let previous = fs::read(running_path()).map_err(|e| e.to_string())?;
        if sha(&previous) != source { return Err("source changed while preserving executable".into()); }
        atomic(&helper, &previous, 0o755)?;
        drop(_guard);
        update(config, &op.request.operation_id, |o| o.phase = "launching".into())?;
        let name = format!("app-lb-host-{}", sha(op.request.operation_id.as_bytes()));
        command("/usr/bin/systemd-run", &["--unit", &name, "--no-block", "--property=Type=oneshot", "--property=RemainAfterExit=yes", "--property=Restart=no",
            "--", helper.to_str().ok_or("invalid helper path")?, "--apply-host-update", path.to_str().ok_or("invalid config path")?, &op.request.operation_id]).await?;
        Ok(())
    }.await;
    if let Err(error) = result {
        // A helper may already be running. Never rewrite its durable phase or
        // signal it; GET still reconciles exact replacement identity.
        let _ = update(config, &op.request.operation_id, |o| o.error = Some(error.clone()));
    }
}

pub async fn get(config: &Config, id: &str) -> Result<Operation> {
    let mut op = ledger(config)?.operations.into_iter().find(|o| o.request.operation_id == id).ok_or("operation not found")?;
    if op.deployment != config.deployment || op.namespace != config.namespace { return Err("operation target identity changed".into()); }
    if !matches!(op.phase.as_str(), "switching" | "restarting" | "complete") { return Ok(op); }
    let (unit, invocation) = unit(config).await?;
    let exact = running_sha()? == op.request.binary_sha256 && file_sha(&config.executable)? == op.request.binary_sha256
        && compiled_revision() == op.request.revision && invocation != op.source_invocation
        && config_sha(config, &unit)? == op.request.expected_config_sha256 && is_service_process(config).await?;
    if exact {
        let response = http()?.get(&config.health_url).send().await.map_err(|e| e.to_string())?;
        if response.status().is_success() && response.headers().get_all("x-heyo-revision").iter().count() == 1
            && response.headers().get("x-heyo-revision").and_then(|v| v.to_str().ok()) == Some(op.request.revision.as_str()) {
            // Resolve an interrupted rename/directory-sync before claiming a
            // durable success, including a helper that died after the switch.
            let previous = config.state_dir.join(format!("{id}.previous"));
            if file_sha(&previous)? != op.request.expected_binary_sha256 { return Err("preserved predecessor identity mismatch".into()); }
            for path in [&config.executable, config.executable.parent().ok_or("executable parent missing")?, &previous, &config.state_dir] {
                File::open(path).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
            }
            if op.status != "succeeded" {
                op = update(config, id, |o| { o.status = "succeeded".into(); o.phase = "complete".into(); o.readiness_verified = true; o.error = None; })?;
            }
            return Ok(op);
        }
    }
    // Do not present a historical completion as current host verification.
    if op.status == "succeeded" { op.status = "reconciliation_required".into(); op.readiness_verified = false; }
    Ok(op)
}

/// Runs in a systemd-owned process outside the service being restarted. Every
/// filesystem change is preceded by intent; uncertain restart is never retried.
async fn apply(path: &Path, id: &str) -> Result<()> {
    let config = load_config(path)?;
    let execution = config.state_dir.join("executor");
    let _executor = lock(&execution)?;
    let op = ledger(&config)?.operations.into_iter().last().filter(|o| o.request.operation_id == id).ok_or("unknown operation")?;
    if op.phase != "launching" || op.status != "running" { return Err("helper invocation is not authorized by current operation".into()); }
    let result: Result<()> = async {
        let (unit, invocation) = unit(&config).await?;
        if config_sha(&config, &unit)? != op.request.expected_config_sha256 || invocation != op.source_invocation
            || file_sha(&config.executable)? != op.request.expected_binary_sha256 || running_sha()? != op.request.expected_binary_sha256
            || file_sha(&process_path(process_pid(&config).await?).join("exe"))? != op.request.expected_binary_sha256 {
            return Err("host source or configuration drift before switch".into());
        }
        let candidate = config.state_dir.join(format!("{id}.candidate"));
        if fs::metadata(&candidate).map_err(|e| e.to_string())?.len() > host_bundle::LIMIT { return Err("staged executable exceeds budget".into()); }
        let binary = fs::read(candidate).map_err(|e| e.to_string())?;
        if sha(&binary) != op.request.binary_sha256 { return Err("staged executable is corrupt".into()); }
        update(&config, id, |o| o.phase = "switching".into())?;
        // Same-directory temp+rename stays atomic even when staging is on a
        // different filesystem. Preserve the previous executable indefinitely.
        let metadata = fs::symlink_metadata(&config.executable).map_err(|e| e.to_string())?;
        if !metadata.file_type().is_file() { return Err("mapped executable must be a regular file, not a symlink".into()); }
        atomic(&config.executable, &binary, metadata.permissions().mode() & 0o777)?;
        #[cfg(test)]
        if TEST_HOST.try_with(|root| root.join("block-after-switch").exists()).unwrap_or(false) {
            fs::create_dir(config.state_dir.join("operations.writing")).map_err(|e| e.to_string())?;
        }
        update(&config, id, |o| o.phase = "restarting".into())?;
        match &config.process {
            Process::Systemd { unit } => { command("/usr/bin/systemctl", &["restart", unit]).await?; }
            Process::Supervisor { program } => { command("/usr/bin/supervisorctl", &["restart", program]).await?; }
        }
        Ok(())
    }.await;
    if let Err(error) = &result {
        let _ = update(&config, id, |o| { if o.status != "succeeded" { o.error = Some(error.clone()); o.status = "reconciliation_required".into(); } });
    }
    result
}

pub fn helper_main() -> Option<i32> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--apply-host-update") { return None; }
    if args.len() != 4 || !safe_id(&args[3]) { return Some(2); }
    let runtime = tokio::runtime::Runtime::new().expect("helper runtime");
    Some(match runtime.block_on(apply(Path::new(&args[2]), &args[3])) { Ok(()) => 0, Err(e) => { eprintln!("host update: {e}"); 1 } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get as route_get, response::IntoResponse, http::{StatusCode, HeaderMap, HeaderValue}};
    use std::sync::{Arc, Mutex};

    struct Fixture {
        dir: tempfile::TempDir, config: Config, path: PathBuf, request: Request,
        health: Arc<Mutex<(u16, Option<String>)>>, server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture { fn drop(&mut self) { self.server.abort(); } }

    async fn fixture(supervisor: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap(); let root = dir.path();
        fs::create_dir(root.join("proc")).unwrap();
        fs::write(root.join("running"), b"\x7fELFprevious executable").unwrap();
        fs::copy(root.join("running"), root.join("installed")).unwrap();
        std::os::unix::fs::symlink(root.join("running"), root.join("proc/exe")).unwrap();
        fs::write(root.join("revision"), "a".repeat(40)).unwrap();
        let old_stat = format!("1 (app-lb) {}100\n", "0 ".repeat(19));
        let new_stat = format!("1 (app-lb) {}200\n", "0 ".repeat(19));
        fs::write(root.join("proc/stat"), old_stat).unwrap();
        fs::write(root.join("next-stat"), new_stat).unwrap();
        fs::write(root.join("unit"), "ExecStart=installed\nEnvironment=SAFE=1\n").unwrap();
        fs::write(root.join("preserve-workspace"), "never touched").unwrap();
        // Real subprocess argv exercise both supervisors. No real service is
        // started or stopped; /proc and compiled identity are task-local fake
        // process evidence, while file switch/persistence/HTTP are real.
        let script = format!(r##"#!/bin/sh
set -eu
pwd >> '{}'
cd '{}'
case "$1" in
cat) cat unit;;
show) case "$*" in *MainPID*) echo {};; *) printf 'LoadState=loaded\nUser=root\n';; esac;;
pid) echo {};;
restart)
 printf '%s\n' "$*" >> restarts
 [ ! -e fail-restart ] || exit 1
 cp installed running
 cp next-stat proc/stat
 ;;
*) exit 9;;
esac
"##, root.join("caller-cwds").display(), root.display(), std::process::id(), std::process::id());
        for name in ["systemctl", "supervisorctl"] { atomic(&root.join(name), script.as_bytes(), 0o755).unwrap(); }
        atomic(&root.join("systemd-run"), format!("#!/bin/sh\ncd '{}'\nprintf '%s\\n' \"$*\" >> launches\n[ ! -e lose-launch ]\n", root.display()).as_bytes(), 0o755).unwrap();
        let revision = "a".repeat(40);
        let bytes = host_bundle::tests::bundle(&revision, false, false);
        let artifact = sha(&bytes);
        let health = Arc::new(Mutex::new((200,Some(revision.clone()))));
        let h = health.clone(); let blob = bytes.clone();
        let app = Router::new().route("/blobs/:digest", route_get(move || { let data = blob.clone(); async move { data } }))
            .route("/healthz", route_get(move || { let h = h.clone(); async move {
                let (status, value) = h.lock().unwrap().clone();
                let mut headers = HeaderMap::new(); if let Some(v) = value { headers.insert("x-heyo-revision", HeaderValue::from_str(&v).unwrap()); }
                (StatusCode::from_u16(status).unwrap(), headers, "ok").into_response()
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let config = Config { deployment:"host".into(),namespace:"default".into(),executable:root.join("installed"),
            process: if supervisor { Process::Supervisor {program:"app-lb".into()} } else { Process::Systemd {unit:"app-lb-eu1.service".into()} },
            state_dir:root.join("state"),artifact_store:url.clone(),health_url:format!("{url}/healthz"),config_files:vec![root.join("unit")] };
        let path = root.join("mapping.json"); fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let fingerprint = TEST_HOST.scope(root.to_path_buf(), snapshot(&config)).await.unwrap();
        let request = Request { operation_id:"ci-host-operation".into(),expected_binary_sha256:fingerprint["binary_sha256"].as_str().unwrap().into(),
            expected_config_sha256:fingerprint["config_sha256"].as_str().unwrap().into(),artifact_sha256:artifact,
            binary_sha256:sha(&host_bundle::executable(&bytes,&revision).unwrap()),revision };
        Fixture { dir, config, path, request, health, server }
    }

    async fn launch(f: &Fixture) {
        start(&f.path,&f.config,f.request.clone()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            while !f.dir.path().join("launches").exists() {
                let state = ledger(&f.config).unwrap();
                assert!(state.operations[0].error.is_none(), "stage failed: {:?}", state.operations[0]);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert!(result.is_ok(), "helper did not start: {:?}", ledger(&f.config).unwrap().operations);
    }

    #[tokio::test]
    async fn host_update_systemd_and_supervisor_preserve_data_and_require_exact_health() {
        for supervisor in [false,true] {
            let f = fixture(supervisor).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(), async {
                launch(&f).await;
                assert_eq!(get(&f.config,&f.request.operation_id).await.unwrap().status,"running");
                assert_eq!(fs::read(&f.config.executable).unwrap(), b"\x7fELFprevious executable");
                apply(&f.path,&f.request.operation_id).await.unwrap();
                for (status,header) in [(200,None),(200,Some("b".repeat(40))),(302,Some("a".repeat(40))),(500,Some("a".repeat(40)))] {
                    *f.health.lock().unwrap() = (status,header);
                    assert_ne!(get(&f.config,&f.request.operation_id).await.unwrap().status,"succeeded");
                }
                *f.health.lock().unwrap() = (200,Some("a".repeat(40)));
                assert_eq!(get(&load_config(&f.path).unwrap(),&f.request.operation_id).await.unwrap().status,"succeeded");
                assert!(apply(&f.path,&f.request.operation_id).await.is_err(), "never recycle terminal helper");
                start(&f.path,&f.config,f.request.clone()).await.unwrap();
                assert_eq!(fs::read_to_string(f.dir.path().join("launches")).unwrap().lines().count(),1);
                let restart = fs::read_to_string(f.dir.path().join("restarts")).unwrap();
                assert_eq!(restart, if supervisor {"restart app-lb\n"} else {"restart app-lb-eu1.service\n"});
                assert!(fs::read_to_string(f.dir.path().join("caller-cwds")).unwrap().lines().all(|line| line == "/"));
                assert_eq!(fs::read(f.config.state_dir.join("ci-host-operation.previous")).unwrap(),b"\x7fELFprevious executable");
                assert_eq!(fs::read_to_string(f.dir.path().join("preserve-workspace")).unwrap(),"never touched");
                fs::write(f.dir.path().join("running"),b"wrong executable with same revision").unwrap();
                assert_ne!(get(&f.config,&f.request.operation_id).await.unwrap().status,"succeeded");
            }).await;
        }
    }

    #[tokio::test]
    async fn host_update_lost_launch_replay_and_concurrent_admission_launch_once() {
        let f = fixture(false).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(), async {
            fs::write(f.dir.path().join("lose-launch"),"").unwrap();
            let (a,b) = tokio::join!(start(&f.path,&f.config,f.request.clone()), start(&f.path,&f.config,f.request.clone()));
            assert!(a.is_ok() || b.is_ok());
            launch(&f).await;
            // Simulate the independent helper surviving a lost launch reply and
            // the HTTP-owning app-lb process dying. Reload only durable state.
            apply(&f.path,&f.request.operation_id).await.unwrap();
            let recovered = load_config(&f.path).unwrap();
            assert_eq!(get(&recovered,&f.request.operation_id).await.unwrap().status,"succeeded");
            assert_eq!(fs::read_to_string(f.dir.path().join("launches")).unwrap().lines().count(),1);
            let mut conflicting = f.request.clone(); conflicting.binary_sha256 = "e".repeat(64);
            assert!(start(&f.path,&recovered,conflicting).await.is_err());
        }).await;
    }

    #[tokio::test]
    async fn host_update_drift_corruption_and_ambiguous_switch_fail_closed() {
        for fault in ["source","config","candidate","restart","before-switch","after-switch"] {
            let f = fixture(false).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(), async {
                launch(&f).await;
                match fault {
                    "source" => fs::write(&f.config.executable,b"changed source").unwrap(),
                    "config" => fs::write(f.dir.path().join("unit"),b"changed configuration").unwrap(),
                    "candidate" => fs::write(f.config.state_dir.join("ci-host-operation.candidate"),b"corrupt").unwrap(),
                    "restart" => fs::write(f.dir.path().join("fail-restart"),b"").unwrap(),
                    "before-switch" | "after-switch" => {
                        update(&f.config,&f.request.operation_id,|o| o.phase="switching".into()).unwrap();
                        if fault == "after-switch" { fs::copy(f.config.state_dir.join("ci-host-operation.candidate"),&f.config.executable).unwrap(); }
                    }
                    _ => unreachable!(),
                }
                assert!(apply(&f.path,&f.request.operation_id).await.is_err());
                assert_ne!(get(&load_config(&f.path).unwrap(),&f.request.operation_id).await.unwrap().status,"succeeded");
                start(&f.path,&f.config,f.request.clone()).await.unwrap();
                assert_eq!(fs::read_to_string(f.dir.path().join("launches")).unwrap().lines().count(),1);
                assert!(f.config.state_dir.join("ci-host-operation.previous").exists());
                assert_eq!(fs::read_to_string(f.dir.path().join("preserve-workspace")).unwrap(),"never touched");
            }).await;
        }
    }

    #[tokio::test]
    async fn host_update_stale_cas_persistence_failure_and_restart_before_launch() {
        let f = fixture(true).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(), async {
            let (text,invocation) = unit(&f.config).await.unwrap();
            let fingerprint = config_sha(&f.config,&text).unwrap();
            assert!(admit(&f.config,f.request.clone(),"stale",&fingerprint,&invocation).is_err());
            fs::create_dir(f.config.state_dir.join("operations.writing")).unwrap();
            assert!(admit(&f.config,f.request.clone(),&f.request.expected_binary_sha256,&fingerprint,&invocation).is_err());
            assert!(ledger(&f.config).unwrap().operations.is_empty());
            fs::remove_dir(f.config.state_dir.join("operations.writing")).unwrap();
            admit(&f.config,f.request.clone(),&f.request.expected_binary_sha256,&fingerprint,&invocation).unwrap();
            start(&f.path,&load_config(&f.path).unwrap(),f.request.clone()).await.unwrap();
            assert!(!f.dir.path().join("launches").exists(),"unknown prior stage is not replayed");
            let mut next = f.request.clone(); next.operation_id="another".into();
            assert!(admit(&f.config,next,&f.request.expected_binary_sha256,&fingerprint,&invocation).is_err());
        }).await;
    }

    #[tokio::test]
    async fn host_update_switch_commit_failure_reconciles_only_verified_new_process() {
        let f = fixture(false).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(), async {
            launch(&f).await;
            fs::write(f.dir.path().join("block-after-switch"),"").unwrap();
            assert!(apply(&f.path,&f.request.operation_id).await.is_err());
            assert_eq!(file_sha(&f.config.executable).unwrap(),f.request.binary_sha256);
            assert_eq!(ledger(&f.config).unwrap().operations[0].phase,"switching");
            assert!(!f.dir.path().join("restarts").exists());
            assert_ne!(get(&f.config,&f.request.operation_id).await.unwrap().status,"succeeded");
            // A later independently observed restart is not a guessed replay.
            command("/usr/bin/systemctl", &["restart","app-lb-eu1.service"]).await.unwrap();
            assert!(get(&f.config,&f.request.operation_id).await.is_err(),"cannot report success while persistence fails");
            fs::remove_dir(f.config.state_dir.join("operations.writing")).unwrap();
            assert_eq!(get(&load_config(&f.path).unwrap(),&f.request.operation_id).await.unwrap().status,"succeeded");
            assert_eq!(fs::read(f.config.state_dir.join("ci-host-operation.previous")).unwrap(),b"\x7fELFprevious executable");
        }).await;
    }

    #[tokio::test]
    async fn host_update_corrupt_download_and_namespace_change_cannot_launch() {
        let mut f = fixture(false).await;
        f.request.artifact_sha256 = "e".repeat(64);
        TEST_HOST.scope(f.dir.path().to_path_buf(), async {
            start(&f.path,&f.config,f.request.clone()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3),async {
                while ledger(&f.config).unwrap().operations[0].error.is_none() {tokio::time::sleep(Duration::from_millis(10)).await;}
            }).await.unwrap();
            assert!(!f.dir.path().join("launches").exists());
            assert_eq!(fs::read(&f.config.executable).unwrap(),b"\x7fELFprevious executable");
            let mut foreign = f.config.clone(); foreign.namespace="other".into();
            assert!(get(&foreign,&f.request.operation_id).await.is_err());
            assert!(start(&f.path,&foreign,f.request.clone()).await.is_err());
        }).await;
    }

    #[tokio::test]
    async fn host_update_mapping_rejects_broad_or_option_targets() {
        let f = fixture(true).await;
        for process in [Process::Supervisor {program:"all".into()}, Process::Supervisor {program:"-x".into()},
            Process::Systemd {unit:"-x.service".into()}] {
            let mut config = f.config.clone(); config.process = process;
            fs::write(&f.path,serde_json::to_vec(&config).unwrap()).unwrap();
            assert!(load_config(&f.path).is_err());
        }
    }
}
