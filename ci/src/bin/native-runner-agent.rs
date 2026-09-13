//! Native Windows/Intel Mac pull agent. One process executes one job at a time.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

#[path = "../expr.rs"]
mod expr;
#[path = "../paths.rs"]
mod paths;

const VERSION: u32 = 1;
#[derive(Clone)]
struct Config {
    endpoint: String,
    token: String,
    id: String,
    name: String,
    labels: Vec<String>,
    platform: String,
    workdir: PathBuf,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Registration<'a> {
    runner_id: &'a str,
    name: &'a str,
    labels: &'a [String],
    platform: &'a str,
    arch: &'static str,
    protocol_version: u32,
    max_concurrent_jobs: i32,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Poll<'a> {
    runner_id: &'a str,
    protocol_version: u32,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Update<'a> {
    runner_id: &'a str,
    lease_token: Uuid,
}
#[derive(Deserialize)]
struct PollResponse {
    job: Option<Lease>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lease {
    job_id: String,
    run_id: String,
    lease_token: Uuid,
    plan: Plan,
    source_url: String,
    context: Value,
    mask_values: Vec<String>,
}
#[derive(Clone, Deserialize)]
struct Plan {
    key: String,
    env: BTreeMap<String, String>,
    steps: Vec<Step>,
    timeout: Duration,
}
#[derive(Clone, Deserialize)]
struct Step {
    name: Option<String>,
    id: Option<String>,
    #[serde(rename = "if")]
    condition: Option<String>,
    uses: Option<String>,
    #[serde(default)]
    with: BTreeMap<String, String>,
    run: Option<String>,
    shell: Option<String>,
    #[serde(rename = "working-directory")]
    working_directory: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(rename = "timeout-minutes")]
    timeout_minutes: Option<u64>,
    #[serde(rename = "continue-on-error", default)]
    continue_on_error: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Completion<'a> {
    runner_id: &'a str,
    lease_token: Uuid,
    status: &'a str,
    error: Option<String>,
    outputs: Value,
    steps: Vec<StepResult>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StepResult {
    index: usize,
    status: String,
    exit_code: Option<i32>,
    log: String,
    error: Option<String>,
    outputs: Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    let c = config()?;
    tokio::fs::create_dir_all(&c.workdir).await?;
    validate_endpoint(&c.endpoint)?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).redirect(reqwest::redirect::Policy::none()).build()?;
    post(
        &http,
        &c,
        "register",
        &Registration {
            runner_id: &c.id,
            name: &c.name,
            labels: &c.labels,
            platform: &c.platform,
            arch: "x86_64",
            protocol_version: VERSION,
            max_concurrent_jobs: 1,
        },
    )
    .await?;
    loop {
        let Some(job) = poll(&http, &c).await? else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let hc = http.clone();
        let cc = c.clone();
        let token = job.lease_token;
        let (lost_tx,lost_rx)=tokio::sync::watch::channel(false);
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if post(
                    &hc,
                    &cc,
                    "heartbeat",
                    &Update {
                        runner_id: &cc.id,
                        lease_token: token,
                    },
                )
                .await
                .is_err()
                {
                    let _=lost_tx.send(true);
                    break;
                }
            }
        });
        let result = execute(&http, &c, &job, lost_rx).await;
        heartbeat.abort();
        let (status, error, steps) = match result {
            Ok(s) => { let failed=s.iter().any(|r|r.status=="failure"); (if failed{"failure"}else{"success"}, None, s) },
            Err(e) => ("failure", Some(format!("{e:#}")), failure_reports(&job, &format!("{e:#}"))),
        };
        post(
            &http,
            &c,
            "complete",
            &Completion {
                runner_id: &c.id,
                lease_token: job.lease_token,
                status,
                error,
                outputs: serde_json::json!({}),
                steps,
            },
        )
        .await?;
    }
}
fn validate_endpoint(endpoint:&str)->Result<()> {
    let u=reqwest::Url::parse(endpoint)?;
    let loopback=u.host_str().is_some_and(|h|h=="localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|ip|ip.is_loopback()));
    if u.scheme()!="https" && !(u.scheme()=="http" && loopback) { bail!("CI_ENDPOINT must use HTTPS except for loopback") }
    Ok(())
}
fn config() -> Result<Config> {
    let profile = std::env::var("CI_NATIVE_PROFILE")
        .context("CI_NATIVE_PROFILE must be mac-intel or windows-x64")?;
    let (platform, labels) = match profile.as_str() {
        "mac-intel" => (
            "macos",
            vec![
                "namespace-profile-mac-build",
                "macos",
                "macos-intel",
                "x86_64-apple-darwin",
            ],
        ),
        "windows-x64" => (
            "windows",
            vec![
                "blacksmith-2vcpu-windows-2025",
                "windows",
                "windows-x64",
                "x86_64-pc-windows-msvc",
            ],
        ),
        _ => bail!("unknown profile"),
    };
    if std::env::consts::ARCH != "x86_64" || std::env::consts::OS != platform {
        bail!(
            "profile {profile} requires {platform} x86_64; this host is {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }
    Ok(Config {
        endpoint: std::env::var("CI_ENDPOINT")?.trim_end_matches('/').into(),
        token: std::env::var("CI_NATIVE_RUNNER_SECRET")?,
        id: std::env::var("CI_NATIVE_RUNNER_ID")?,
        name: std::env::var("CI_NATIVE_RUNNER_NAME").unwrap_or_else(|_| profile),
        labels: labels.into_iter().map(str::to_string).collect(),
        platform: platform.into(),
        workdir: std::env::var_os("CI_NATIVE_WORKDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("heyo-native")),
    })
}
async fn poll(http: &reqwest::Client, c: &Config) -> Result<Option<Lease>> {
    loop {
        let r = match http
            .post(format!("{}/api/native/poll", c.endpoint))
            .bearer_auth(&c.token)
            .json(&Poll { runner_id: &c.id, protocol_version: VERSION })
            .send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("poll failed: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        if r.status().is_server_error() || r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            eprintln!("poll temporarily unavailable ({}); retrying in 5s", r.status());
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        if !r.status().is_success() {
            bail!("poll: {} {}", r.status(), r.text().await?)
        }
        return Ok(r.json::<PollResponse>().await?.job);
    }
}
async fn post<T: Serialize>(h: &reqwest::Client, c: &Config, path: &str, value: &T) -> Result<()> {
    let mut last=String::new();
    for attempt in 0..3 {
        match h.post(format!("{}/api/native/{path}", c.endpoint)).bearer_auth(&c.token).json(value).send().await {
            Ok(r) if r.status().is_success()=>return Ok(()),
            Ok(r)=>{last=format!("{} {}",r.status(),r.text().await.unwrap_or_default());if !last.starts_with('5'){break}},
            Err(e)=>last=e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
    }
    bail!("{path}: {last}")
}
async fn execute(h: &reqwest::Client, c: &Config, j: &Lease, mut lost:tokio::sync::watch::Receiver<bool>) -> Result<Vec<StepResult>> {
    let root = c.workdir.join(format!("{}-{}", j.run_id, j.job_id));
    if root.exists() {
        tokio::fs::remove_dir_all(&root).await?
    }
    tokio::fs::create_dir_all(&root).await?;
    let source=reqwest::Url::parse(&j.source_url)?;
    let endpoint=reqwest::Url::parse(&c.endpoint)?;
    if source.origin()!=endpoint.origin() || !source.path().starts_with("/api/native/jobs/") { bail!("source URL is outside the configured CI origin/path") }
    let response = h
        .get(&j.source_url)
        .bearer_auth(&c.token)
        .send()
        .await?;
    if response.status().is_redirection(){bail!("source redirects are forbidden")}
    let bytes=response.error_for_status()?.bytes().await?;
    extract_source(bytes.to_vec(),root.clone()).await?;
    let mut reports = vec![];
    let mut ctx=expr::Context::from_value(j.context.clone());
    let mut step_scope=serde_json::Map::new();
    let mut blocking_failed=false;
    for (i, s) in j.plan.steps.iter().enumerate() {
        ctx.set("steps",Value::Object(step_scope.clone())).set_status(if blocking_failed{"failure"}else{"success"});
        let should_run = match s.condition.as_deref() {
            Some(condition) => ctx.eval_condition(condition)?,
            None => !blocking_failed,
        };
        if !should_run {
            reports.push(StepResult {
                index: i,
                status: "skipped".into(),
                exit_code: None,
                log: String::new(),
                error: None,
                outputs:serde_json::json!({}),
            });
            continue;
        }
        if s.uses.as_deref()==Some("ci/upload-artifact") {
            let result=upload_artifact(h,c,j,i,s,&root,&ctx).await;
            let (status,error)=match result{Ok(())=>("success",None),Err(e)=>("failure",Some(format!("{e:#}")))};
            reports.push(StepResult{index:i,status:status.into(),exit_code:None,log:String::new(),error,outputs:serde_json::json!({})});
            if status=="failure"&&!s.continue_on_error{blocking_failed=true} continue
        }
        if s.uses.as_deref()==Some("ci/checkout-release") {
            let result=checkout_release(h,c,j,i,&root).await;
            let (status,error,outputs)=match result{Ok(sha)=>("success",None,serde_json::json!({"sha":sha})),Err(e)=>("failure",Some(format!("{e:#}")),serde_json::json!({}))};
            reports.push(StepResult{index:i,status:status.into(),exit_code:None,log:String::new(),error,outputs:outputs.clone()});
            if let Some(id)=&s.id {step_scope.insert(id.clone(),serde_json::json!({"outcome":status,"conclusion":status,"outputs":outputs}));}
            if status=="failure"&&!s.continue_on_error{blocking_failed=true} continue
        }
        if let Some(u) = &s.uses {
            reports.push(StepResult{index:i,status:"failure".into(),exit_code:None,log:String::new(),error:Some(format!("native builtin `{u}` is unsupported; release builtins must run in a Linux job")),outputs:serde_json::json!({})});
            blocking_failed=true;
            continue;
        }
        let script = ctx.substitute(s.run.as_deref().context("step has neither run nor uses")?);
        let cwd = s
            .working_directory
            .as_deref()
            .map(|p| root.join(ctx.substitute(p)))
            .unwrap_or_else(|| root.clone());
        ensure_inside(&root, &cwd)?;
        let mut cmd = if c.platform == "windows" {
            let shell = s.shell.as_deref().unwrap_or("powershell");
            if !matches!(shell,"powershell"|"pwsh") { bail!("native Windows shell must be powershell or pwsh") }
            let mut x = Command::new(shell);
            let script = format!("$ErrorActionPreference='Stop'; $global:LASTEXITCODE=0; {script}\nif ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}");
            x.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            x
        } else {
            let shell = s.shell.as_deref().unwrap_or("bash");
            let mut x = Command::new(shell);
            x.args(["-eo", "pipefail", "-c", &script]);
            x
        };
        cmd.current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(j.plan.env.iter().map(|(k,v)|(k,ctx.substitute(v))))
            .envs(s.env.iter().map(|(k,v)|(k,ctx.substitute(v))))
            .env_remove("CI_NATIVE_RUNNER_SECRET");
        configure_process_tree(&mut cmd);
        cmd.kill_on_drop(true);
        let output_file=root.join(format!(".heyo-output-{i}"));
        cmd.env("GITHUB_OUTPUT",&output_file).env("HEYO_OUTPUT",&output_file);
        let child=cmd.spawn()?;
        let pid=child.id().context("spawned process has no id")?;
        let timeout=s.timeout_minutes.map(|m|Duration::from_secs(m*60)).unwrap_or(j.plan.timeout);
        let out=tokio::select! { r=child.wait_with_output()=>r?, _=lost.changed()=>{terminate_tree(pid).await;bail!("lease lost or job cancelled")}, _=tokio::time::sleep(timeout)=>{terminate_tree(pid).await;bail!("step timed out")} };
        let log = mask(&format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),&j.mask_values);
        let outputs=parse_outputs(&output_file).await?;
        let ok = out.status.success();
        reports.push(StepResult {
            index: i,
            status: if ok { "success" } else { "failure" }.into(),
            exit_code: out.status.code(),
            log,
            error: if ok {
                None
            } else {
                Some("command failed".into())
            },
            outputs:outputs.clone(),
        });
        if let Some(id)=&s.id {step_scope.insert(id.clone(),serde_json::json!({"outcome":if ok{"success"}else{"failure"},"conclusion":if ok{"success"}else{"failure"},"outputs":outputs}));}
        if !ok && !s.continue_on_error {
            blocking_failed=true;
        }
    }
    let _ = tokio::fs::remove_dir_all(root).await;
    Ok(reports)
}
fn failure_reports(j:&Lease,error:&str)->Vec<StepResult>{j.plan.steps.iter().enumerate().map(|(i,_)|StepResult{index:i,status:"failure".into(),exit_code:None,log:String::new(),error:Some(error.into()),outputs:serde_json::json!({})}).collect()}
fn mask(text:&str,values:&[String])->String{values.iter().filter(|v|v.len()>=4).fold(text.to_string(),|s,v|s.replace(v,"***"))}
async fn parse_outputs(path:&Path)->Result<Value>{let bytes=match tokio::fs::read(path).await{Ok(v)=>v,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(serde_json::json!({})),Err(e)=>return Err(e.into())};let text=if bytes.starts_with(&[0xff,0xfe]){if bytes.len()%2!=0{bail!("invalid UTF-16 output")};String::from_utf16(&bytes[2..].chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect::<Vec<_>>())?}else{String::from_utf8(bytes)?};let mut m=serde_json::Map::new();for line in text.trim_start_matches('\u{feff}').lines(){let (k,v)=line.split_once('=').context("invalid output; expected name=value")?;if k.is_empty()||k.contains(|c:char|!c.is_ascii_alphanumeric()&&c!='_'&&c!='-'){bail!("invalid output name")};m.insert(k.into(),Value::String(v.into()));}Ok(Value::Object(m))}
async fn extract_source(bytes:Vec<u8>,root:PathBuf)->Result<()>{tokio::task::spawn_blocking(move||->Result<()>{let gz=flate2::read::GzDecoder::new(bytes.as_slice());let mut ar=tar::Archive::new(gz);for entry in ar.entries()?{let mut e=entry?;let p=e.path()?.into_owned();if p.is_absolute()||p.components().any(|c|matches!(c,std::path::Component::ParentDir)){bail!("source archive path escapes workspace")};let kind=e.header().entry_type();if kind.is_symlink()||kind.is_hard_link(){bail!("source archive links are forbidden")};e.unpack_in(&root)?;}Ok(())}).await??;Ok(())}
async fn checkout_release(h:&reqwest::Client,c:&Config,j:&Lease,index:usize,root:&Path)->Result<String>{
    let url=format!("{}/api/native/jobs/{}/release-source/{index}",c.endpoint,j.lease_token);
    let parsed=reqwest::Url::parse(&url)?;let endpoint=reqwest::Url::parse(&c.endpoint)?;
    if parsed.origin()!=endpoint.origin()||!parsed.path().starts_with("/api/native/jobs/"){bail!("release source URL is outside the configured CI origin/path")}
    let response=h.get(parsed).bearer_auth(&c.token).send().await?;
    if response.status().is_redirection(){bail!("release source redirects are forbidden")}
    let response=response.error_for_status()?;
    let sha=response.headers().get("x-heyo-release-sha").context("release source omitted sha")?.to_str()?.to_string();
    if sha.len()!=40||!sha.bytes().all(|b|b.is_ascii_hexdigit()){bail!("release source returned invalid sha")}
    let bytes=response.bytes().await?.to_vec();
    replace_checkout(bytes,c,root).await?;
    Ok(sha)
}
async fn replace_checkout(bytes:Vec<u8>,c:&Config,root:&Path)->Result<()>{
    let workdir=tokio::fs::canonicalize(&c.workdir).await?;let current=tokio::fs::canonicalize(root).await?;
    if !current.starts_with(&workdir)||current==workdir{bail!("job checkout is outside native workdir")}
    let parent=current.parent().context("job checkout has no parent")?;
    let staging=parent.join(format!(".release-{}",Uuid::new_v4()));let backup=parent.join(format!(".previous-{}",Uuid::new_v4()));
    tokio::fs::create_dir(&staging).await?;
    if let Err(e)=extract_source(bytes,staging.clone()).await{let _=tokio::fs::remove_dir_all(&staging).await;return Err(e)}
    tokio::fs::rename(&current,&backup).await?;
    if let Err(e)=tokio::fs::rename(&staging,&current).await{let _=tokio::fs::rename(&backup,&current).await;return Err(e.into())}
    tokio::fs::remove_dir_all(backup).await?;
    Ok(())
}
async fn upload_artifact(h:&reqwest::Client,c:&Config,j:&Lease,index:usize,s:&Step,root:&Path,ctx:&expr::Context)->Result<()>{
    let name=ctx.substitute(s.with.get("name").context("upload-artifact requires with.name")?);
    if name.is_empty()||name.contains('/')||name.contains('\\')||name==".."{bail!("invalid artifact name")}
    let rel=ctx.substitute(s.with.get("path").context("upload-artifact requires with.path")?);
    let path=root.join(&rel);ensure_inside(root,&path)?;let canonical=tokio::fs::canonicalize(&path).await?;let canonical_root=tokio::fs::canonicalize(root).await?;if !canonical.starts_with(&canonical_root){bail!("artifact path escapes workspace")}
    let bytes=tokio::task::spawn_blocking(move||->Result<Vec<u8>>{let mut gz=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());{let mut tar=tar::Builder::new(&mut gz);if canonical.is_dir(){tar.append_dir_all(".",&canonical)?}else{tar.append_path_with_name(&canonical,canonical.file_name().context("artifact file has no name")?)?};tar.finish()?;}Ok(gz.finish()?)}).await??;
    let mut request=h.post(format!("{}/api/native/jobs/{}/artifacts/{index}",c.endpoint,j.lease_token)).bearer_auth(&c.token).query(&[("name",name.as_str())]);
    if let Some(v)=s.with.get("description"){request=request.query(&[("description",ctx.substitute(v))])} if s.with.get("public").is_some_and(|v|ctx.substitute(v).eq_ignore_ascii_case("true")){request=request.query(&[("public","true")])}
    let response=request.body(bytes).send().await?;if !response.status().is_success(){bail!("artifact upload: {} {}",response.status(),response.text().await?)} Ok(())
}
#[cfg(unix)] fn configure_process_tree(cmd:&mut Command){use std::os::unix::process::CommandExt;cmd.as_std_mut().process_group(0);}
#[cfg(windows)] fn configure_process_tree(cmd:&mut Command){use std::os::windows::process::CommandExt;cmd.as_std_mut().creation_flags(0x00000200);}
#[cfg(unix)] async fn terminate_tree(pid:u32){let _=Command::new("kill").args(["-TERM",&format!("-{pid}")]).status().await;tokio::time::sleep(Duration::from_secs(2)).await;let _=Command::new("kill").args(["-KILL",&format!("-{pid}")]).status().await;}
#[cfg(windows)] async fn terminate_tree(pid:u32){let _=Command::new("taskkill").args(["/PID",&pid.to_string(),"/T","/F"]).status().await;}
fn ensure_inside(root: &Path, path: &Path) -> Result<()> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("working-directory escapes workspace")
    }
    // Windows rooted/drive-relative paths need not satisfy is_absolute().
    // Callers already joined the path to root, so require that prefix always.
    if !path.starts_with(root) {
        bail!("working-directory escapes workspace")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_step_preserves_workflow_fields_and_omitted_maps() {
        let step: Step = serde_json::from_value(serde_json::json!({"run":"exit 3", "if":"always()",
            "working-directory":"src", "timeout-minutes":2, "continue-on-error":true})).unwrap();
        assert_eq!(step.condition.as_deref(),Some("always()"));
        assert_eq!(step.working_directory.as_deref(),Some("src"));
        assert_eq!(step.timeout_minutes,Some(2));
        assert!(step.continue_on_error && step.env.is_empty() && step.with.is_empty());
    }
    #[test]
    fn workspace_gate_rejects_escape() {
        assert!(ensure_inside(Path::new("/tmp/root"), Path::new("../bad")).is_err());
        assert!(ensure_inside(Path::new("/tmp/root"), Path::new("/etc")).is_err());
        assert!(ensure_inside(Path::new("C:/work/job"), Path::new("C:outside")).is_err());
        assert!(ensure_inside(Path::new("/tmp/root"), Path::new("/tmp/root/src")).is_ok());
    }

    fn step(run:&str)->Step{Step{name:None,id:None,condition:None,uses:None,with:BTreeMap::new(),run:Some(run.into()),shell:None,working_directory:None,env:BTreeMap::new(),timeout_minutes:None,continue_on_error:false}}

    async fn source_server()->(String,tokio::task::JoinHandle<()>) {
        let mut gz=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());
        {let mut tar=tar::Builder::new(&mut gz);tar.finish().unwrap();}
        let bytes=gz.finish().unwrap();
        let app=axum::Router::new().route("/api/native/jobs/test/source",axum::routing::get(move||{let bytes=bytes.clone();async move{bytes}}));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let task=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});(format!("http://{addr}"),task)
    }
    fn lease(endpoint:&str,steps:Vec<Step>,timeout:Duration)->Lease{Lease{job_id:"job".into(),run_id:Uuid::new_v4().to_string(),lease_token:Uuid::new_v4(),plan:Plan{key:"job".into(),env:BTreeMap::new(),steps,timeout},source_url:format!("{endpoint}/api/native/jobs/test/source"),context:serde_json::json!({}),mask_values:vec![]}}
    fn config(endpoint:&str,dir:&Path)->Config{Config{endpoint:endpoint.into(),token:"test".into(),id:"runner".into(),name:"runner".into(),labels:vec![],platform:if cfg!(windows){"windows"}else{"macos"}.into(),workdir:dir.into()}}

    #[tokio::test]
    async fn polling_survives_transient_server_errors_but_refuses_bad_credentials() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        for statuses in [vec![500,503,429,200], vec![401], vec![403]] {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let responses = statuses.clone();
            let app = axum::Router::new().route("/api/native/poll", axum::routing::post(move || {
                let index = seen.fetch_add(1, Ordering::SeqCst);
                let status = responses[index.min(responses.len()-1)];
                async move { (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(serde_json::json!({"job":null}))) }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let dir = tempfile::tempdir().unwrap();
            let result = tokio::time::timeout(Duration::from_secs(20),
                poll(&reqwest::Client::new(), &config(&endpoint, dir.path()))).await;
            server.abort();
            let result = result.expect("poll did not recover or reject within its expected retry window");
            if statuses.len() > 1 {
                assert!(result.unwrap().is_none());
            } else {
                assert!(result.err().expect("credential errors must be terminal").to_string().contains(&statuses[0].to_string()));
            }
            assert_eq!(calls.load(Ordering::SeqCst), statuses.len());
        }
    }

    #[tokio::test]
    async fn executor_reports_failure_skips_condition_and_hands_off_outputs(){
        let (endpoint,server)=source_server().await;let dir=tempfile::tempdir().unwrap();let mut first=step(if cfg!(windows){"'answer=42' >> $env:GITHUB_OUTPUT"}else{"echo answer=42 >> \"$GITHUB_OUTPUT\""});first.id=Some("build".into());let mut skipped=step("exit 99");skipped.condition=Some("${{ false }}".into());let handoff=step(if cfg!(windows){"if ('${{ steps.build.outputs.answer }}' -ne '42') { exit 9 }"}else{"test '${{ steps.build.outputs.answer }}' = 42"});let mut failed=step("exit 7");failed.continue_on_error=true;
        let reports=execute(&reqwest::Client::new(),&config(&endpoint,dir.path()),&lease(&endpoint,vec![first,skipped,handoff,failed],Duration::from_secs(5)),tokio::sync::watch::channel(false).1).await.unwrap();server.abort();
        assert_eq!(reports.iter().map(|x|x.status.as_str()).collect::<Vec<_>>(),vec!["success","skipped","success","failure"]);assert_eq!(reports[3].exit_code,Some(7));
    }

    #[tokio::test]
    async fn executor_timeout_kills_and_reports_job_failure(){
        let (endpoint,server)=source_server().await;let dir=tempfile::tempdir().unwrap();let job=lease(&endpoint,vec![step("sleep 30")],Duration::from_millis(50));
        let error=execute(&reqwest::Client::new(),&config(&endpoint,dir.path()),&job,tokio::sync::watch::channel(false).1).await.unwrap_err();server.abort();assert!(error.to_string().contains("timed out"));
        assert_eq!(failure_reports(&job,&error.to_string())[0].status,"failure");
    }

    fn archive_file(name:&str,contents:&[u8])->Vec<u8>{let mut gz=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());{let mut tar=tar::Builder::new(&mut gz);let mut header=tar::Header::new_gnu();header.set_size(contents.len() as u64);header.set_mode(0o644);header.set_cksum();tar.append_data(&mut header,name,contents).unwrap();tar.finish().unwrap();}gz.finish().unwrap()}

    #[tokio::test]
    async fn release_checkout_reports_server_sha_and_replaces_submitted_bytes(){
        let bytes=archive_file("version.txt",b"bumped");let sha="0123456789abcdef0123456789abcdef01234567";
        let app=axum::Router::new().route("/api/native/jobs/{lease}/release-source/{index}",axum::routing::get(move||{let bytes=bytes.clone();async move{([(axum::http::HeaderName::from_static("x-heyo-release-sha"),sha)],bytes)}}));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let dir=tempfile::tempdir().unwrap();let root=dir.path().join("job");tokio::fs::create_dir(&root).await.unwrap();tokio::fs::write(root.join("version.txt"),b"submitted").await.unwrap();
        let endpoint=format!("http://{addr}");let job=lease(&endpoint,vec![],Duration::from_secs(1));
        assert_eq!(checkout_release(&reqwest::Client::new(),&config(&endpoint,dir.path()),&job,0,&root).await.unwrap(),sha);
        assert_eq!(tokio::fs::read(root.join("version.txt")).await.unwrap(),b"bumped");server.abort();
    }

    #[tokio::test]
    async fn unsafe_release_archive_is_rejected_without_destroying_checkout(){
        let mut gz=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());{let mut tar=tar::Builder::new(&mut gz);let mut header=tar::Header::new_gnu();header.set_entry_type(tar::EntryType::Symlink);header.set_size(0);header.set_mode(0o777);header.set_link_name("outside").unwrap();header.set_cksum();tar.append_data(&mut header,"link",std::io::empty()).unwrap();tar.finish().unwrap();}let bytes=gz.finish().unwrap();
        let dir=tempfile::tempdir().unwrap();let root=dir.path().join("job");tokio::fs::create_dir(&root).await.unwrap();tokio::fs::write(root.join("keep"),b"safe").await.unwrap();
        assert!(replace_checkout(bytes,&config("http://localhost",dir.path()),&root).await.is_err());
        assert_eq!(tokio::fs::read(root.join("keep")).await.unwrap(),b"safe");
    }
}
