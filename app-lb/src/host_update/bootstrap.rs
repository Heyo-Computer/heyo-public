//! Explicit root-authorized, one-shot provisioning. Only authenticated GET can
//! complete this journal. An interrupted mutation is never automatically replayed.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{io::Read, os::unix::fs::{MetadataExt, OpenOptionsExt}};
use serde_json::{json, Value};

const LIMIT: usize = 4 * 1024 * 1024;
const PROTOCOL: &str = "host-app-lb-bootstrap-v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Generation { boot_id: String, pid: u32, start_time: u64 }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Source { disk_sha256: String, running_sha256: String, generation: Generation }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target { artifact_sha256: String, binary_sha256: String, revision: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    path: PathBuf, before_sha256: Option<String>, mode: u32,
    #[serde(default, skip_serializing_if="Option::is_none")]
    after_base64: Option<String>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    preserve: Option<bool>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    supervisor_environment: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    operation_id: String, helper_sha256: String, source: Source, config: Config,
    mapping_path: PathBuf, files: Vec<Change>, target: Target,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Original { path: PathBuf, sha256: Option<String>, mode: Option<u32> }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    manifest: Manifest, intent_sha256: String, originals: Vec<Original>,
    status: String, phase: String, error: Option<String>,
}

fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    fn sort(v: &mut Value) {
        match v {
            Value::Object(m) => { m.sort_keys(); for v in m.values_mut() { sort(v); } }
            Value::Array(a) => for v in a { sort(v); }, _ => (),
        }
    }
    let mut v = serde_json::to_value(value).map_err(|e| e.to_string())?;
    sort(&mut v); serde_json::to_vec(&v).map_err(|e| e.to_string())
}

// Every writable ancestor must be root-owned and private to its owner. No
// symlinks, including ancestor symlinks. Root remains the trust boundary.
fn trusted(path: &Path) -> Result<()> {
    if !path.is_absolute() || !path.components().all(|c| matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_))) {
        return Err("bootstrap paths must be absolute without traversal".into());
    }
    #[cfg(test)]
    let test_root = TEST_HOST.try_with(Clone::clone).ok();
    for ancestor in path.ancestors() {
        #[cfg(test)]
        if test_root.as_ref().is_some_and(|r| !ancestor.starts_with(r)) { continue; }
        match fs::symlink_metadata(ancestor) {
            Ok(m) => {
                #[cfg(not(test))]
                let owner = 0;
                #[cfg(test)]
                let owner = if test_root.is_some() { unsafe { libc::geteuid() } } else { 0 };
                if m.uid() != owner || m.mode() & 0o022 != 0 || m.file_type().is_symlink() || (m.is_file() && m.nlink() != 1) {
                    return Err("bootstrap path has untrusted ownership, permissions or symlink".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    trusted(path)?;
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(|e| e.to_string())?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > limit as u64 { return Err("bootstrap file exceeds budget or is not regular".into()); }
    let mut data = Vec::new(); f.take(limit as u64 + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
    if data.len() > limit { return Err("bootstrap file exceeds budget".into()); }
    Ok(data)
}

fn mkdir(path: &Path) -> Result<()> {
    trusted(path)?;
    if !path.exists() {
        mkdir(path.parent().ok_or("missing parent")?)?;
        fs::create_dir(path).map_err(|e| e.to_string())?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        File::open(path.parent().unwrap()).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
    }
    if !path.is_dir() { return Err("bootstrap parent is not a directory".into()); }
    Ok(())
}
fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    trusted(path)?; trusted(&path.with_extension("writing"))?;
    mkdir(path.parent().ok_or("missing parent")?)?;
    atomic(path, bytes, mode)
}
fn journal_path(c: &Config) -> PathBuf { c.state_dir.join("bootstrap.json") }
fn backup(c: &Config, index: usize) -> PathBuf { c.state_dir.join(format!("bootstrap/original-{index}")) }
fn helper(c: &Config) -> PathBuf { c.state_dir.join("bootstrap/helper") }
fn unit_name(j: &Journal) -> String { format!("app-lb-bootstrap-{}", j.intent_sha256) }
fn save(j: &Journal) -> Result<()> { write(&journal_path(&j.manifest.config), &serde_json::to_vec(j).map_err(|e| e.to_string())?, 0o600) }
fn save_locked(j: &Journal) -> Result<()> {
    let _guard = lock(&j.manifest.config.state_dir)?;
    save(j)
}
fn load(path: &Path, intent: Option<&str>) -> Result<Journal> {
    let j: Journal = serde_json::from_slice(&read(path, LIMIT * 2)?).map_err(|e| e.to_string())?;
    if path != journal_path(&j.manifest.config) || sha(&canonical(&j.manifest)?) != j.intent_sha256
        || intent.is_some_and(|i| i != j.intent_sha256) { return Err("bootstrap journal identity conflict".into()); }
    validate(&j.manifest)?; Ok(j)
}
pub(super) fn unblocked(c: &Config) -> Result<()> {
    let path = journal_path(c);
    match fs::symlink_metadata(&path) {
        Ok(_) if load(&path,None)?.status != "succeeded" => return Err("bootstrap requires authenticated reconciliation".into()),
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}
fn output(j: &Journal) -> Value {
    json!({"protocol":PROTOCOL,"operation_id":j.manifest.operation_id,"intent_sha256":j.intent_sha256,
        "journal_path":journal_path(&j.manifest.config),"unit_name":unit_name(j),"status":j.status,"phase":j.phase,
        "deployment":j.manifest.config.deployment,"namespace":j.manifest.config.namespace,
        "readiness_verified":j.status == "succeeded","error":j.error,"source":j.manifest.source,"target":j.manifest.target})
}

fn validate(m: &Manifest) -> Result<()> {
    validate_config(&m.config)?;
    if !safe_id(&m.operation_id) || !valid_sha(&m.target.revision,40)
        || [&m.helper_sha256,&m.source.disk_sha256,&m.source.running_sha256,&m.target.artifact_sha256,&m.target.binary_sha256].iter().any(|s| !valid_sha(s,64))
        || m.helper_sha256 != m.target.binary_sha256 || m.source.disk_sha256 != m.source.running_sha256
        || m.files.is_empty() || m.files.len() > 32 { return Err("invalid bootstrap identity or file budget".into()); }
    if m.config.executable.starts_with(&m.config.state_dir) || m.files.iter().filter(|f| f.supervisor_environment.is_some()).count()>1 {
        return Err("bootstrap requires separate executable/state and at most one native environment edit".into());
    }
    let paths: Vec<_> = m.files.iter().map(|f| f.path.clone()).collect();
    let unique: std::collections::HashSet<_> = paths.iter().collect();
    if paths != m.config.config_files || unique.len() != paths.len() || !paths.contains(&m.mapping_path) {
        return Err("config_files must exactly enumerate unique manifest files including mapping".into());
    }
    for p in paths.iter().chain(std::iter::once(&m.config.executable)) {
        let temp = p.with_extension("writing");
        if temp == *p || paths.contains(&temp) || temp == m.config.executable {
            return Err("bootstrap target overlaps an atomic temporary path".into());
        }
    }
    let mut total = 0;
    for f in &m.files {
        trusted(&f.path)?;
        if f.path == m.config.executable || f.path.starts_with(&m.config.state_dir)
            || f.mode > 0o777 || f.mode & 0o022 != 0 || f.before_sha256.as_ref().is_some_and(|s| !valid_sha(s,64)) {
            return Err("invalid bootstrap configuration file".into());
        }
        if usize::from(f.after_base64.is_some()) + usize::from(f.preserve.is_some()) + usize::from(f.supervisor_environment.is_some()) != 1
            || f.preserve == Some(false) || f.supervisor_environment == Some(false)
            || (f.after_base64.is_none() && f.before_sha256.is_none()) {
            return Err("file requires exactly one literal AFTER, preserve:true or supervisor_environment:true".into());
        }
        if f.supervisor_environment.is_some() && !matches!(m.config.process,Process::Supervisor {..}) { return Err("native environment edit requires Supervisor".into()); }
        if let Some(encoded) = &f.after_base64 {
            if !matches!(f.mode,0o600|0o644) { return Err("literal AFTER mode must be 0600 or 0644".into()); }
            let bytes = STANDARD.decode(encoded).map_err(|_| "invalid after_base64")?;
            total += bytes.len(); if total > LIMIT { return Err("decoded configuration exceeds budget".into()); }
            if f.path == m.mapping_path {
                let mapping: Config = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if mapping != m.config { return Err("AFTER mapping differs from authorized Config".into()); }
            }
        } else if f.path == m.mapping_path {
            return Err("mapping file requires explicit non-secret AFTER Config".into());
        }
    }
    trusted(&m.config.executable)?; trusted(&m.config.state_dir)?;
    Ok(())
}
fn original(path: &Path, limit: usize) -> Result<Original> {
    trusted(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Original {path:path.into(),sha256:Some(sha(&read(path,limit)?)),mode:Some(meta.mode() & 0o777)}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Original {path:path.into(),sha256:None,mode:None}),
        Err(e) => Err(e.to_string()),
    }
}
async fn generation(c: &Config) -> Result<Generation> {
    let (_, invocation) = unit(c).await?;
    let (pid,start) = invocation.split_once(':').ok_or("missing process generation")?;
    #[cfg(test)]
    let boot = TEST_HOST.try_with(|r| r.join("boot-id")).unwrap_or_else(|_| "/proc/sys/kernel/random/boot_id".into());
    #[cfg(not(test))]
    let boot = PathBuf::from("/proc/sys/kernel/random/boot_id");
    Ok(Generation {boot_id:fs::read_to_string(boot).map_err(|e| e.to_string())?.trim().into(),
        pid:pid.parse().map_err(|_| "invalid pid")?,start_time:start.parse().map_err(|_| "invalid start time")?})
}
async fn source(c: &Config) -> Result<Source> {
    let observed = generation(c).await?;
    let executable = fs::read_link(process_path(observed.pid).join("exe")).map_err(|e| e.to_string())?;
    #[cfg(test)]
    let executable = if let Ok(root) = TEST_HOST.try_with(Clone::clone) {
        PathBuf::from(fs::read_to_string(root.join("process-executable")).map_err(|e| e.to_string())?)
    } else { executable };
    if executable != c.executable { return Err("mapped process executable path differs".into()); }
    let value = Source {disk_sha256:sha(&read(&c.executable,host_bundle::LIMIT as usize)?),
        running_sha256:file_sha(&process_path(observed.pid).join("exe"))?,generation:observed};
    if value.generation != generation(c).await? { return Err("process changed during inspection".into()); }
    Ok(value)
}
fn environment(pid: u32) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new(); File::open(process_path(pid).join("environ")).map_err(|e| e.to_string())?
        .take(LIMIT as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > LIMIT { return Err("process environment exceeds budget".into()); }
    let values: Vec<_> = bytes.split(|b| *b == 0).filter_map(|s| s.strip_prefix(b"APP_LB_HOST_UPDATE_CONFIG=")).collect();
    if values.len() > 1 { return Err("ambiguous mapping environment".into()); }
    Ok(values.first().map(|v| v.to_vec()))
}
async fn preflight(m: &Manifest) -> Result<()> {
    if source(&m.config).await? != m.source || environment(m.source.generation.pid)?.is_some() {
        return Err("predecessor changed or already has host-update configuration".into());
    }
    for f in &m.files {
        let old = original(&f.path,LIMIT)?;
        if old.sha256 != f.before_sha256 || (f.after_base64.is_none() && old.mode != Some(f.mode)) { return Err("configuration preimage or preserved mode changed".into()); }
    }
    if let Process::Supervisor {program} = &m.config.process {
        let status = command("/usr/bin/supervisorctl", &["status",program]).await?;
        if status.lines().count() != 1 || status.split_whitespace().next() != Some(program.as_str()) {
            return Err("Supervisor target must be a single ungrouped program".into());
        }
    }
    Ok(())
}

async fn admit(m: Manifest, intent: &str) -> Result<Value> {
    validate(&m)?;
    if !valid_sha(intent,64) || sha(&canonical(&m)?) != intent || running_sha()? != m.helper_sha256 || compiled_revision() != m.target.revision {
        return Err("unauthorized bootstrap helper or intent".into());
    }
    mkdir(&m.config.state_dir)?; trusted(&m.config.state_dir.join("lock"))?;
    let path = journal_path(&m.config);
    if path.exists() { return Ok(output(&load(&path,Some(intent))?)); }
    preflight(&m).await?;
    let guard = lock(&m.config.state_dir)?;
    if path.exists() { return Ok(output(&load(&path,Some(intent))?)); }
    if ledger(&m.config)?.operations.iter().any(|o| o.status != "succeeded") { return Err("normal host update remains unresolved".into()); }
    let mut originals = vec![original(&m.config.executable,host_bundle::LIMIT as usize)?];
    for f in &m.files { originals.push(original(&f.path,LIMIT)?); }
    let mut j = Journal {manifest:m,intent_sha256:intent.into(),originals,status:"running".into(),phase:"preserving".into(),error:None};
    save(&j)?; // This is the durable fence, before any launch or target mutation.
    drop(guard);
    let result: Result<()> = async {
        for (i,old) in j.originals.iter().enumerate() {
            if let Some(hash) = &old.sha256 {
                let bytes = read(&old.path,host_bundle::LIMIT as usize)?;
                if sha(&bytes) != *hash { return Err("source changed during preservation".into()); }
                write(&backup(&j.manifest.config,i),&bytes,0o600)?;
            }
        }
        let m = &j.manifest;
        let request = Request {operation_id:m.operation_id.clone(),expected_binary_sha256:m.source.disk_sha256.clone(),
            expected_config_sha256:String::new(),artifact_sha256:m.target.artifact_sha256.clone(),binary_sha256:m.target.binary_sha256.clone(),revision:m.target.revision.clone()};
        let bytes = download(&m.config,&request).await?;
        write(&helper(&m.config),&bytes,0o700)?;
        preflight(m).await?;
        let mut total=0;
        for i in 0..m.files.len() { total+=desired(&j,i)?.len(); if total>LIMIT { return Err("derived AFTER configuration exceeds budget".into()); } }
        j.phase = "launching".into(); save_locked(&j)?;
        Ok(())
    }.await;
    if let Err(e) = result { j.status="reconciliation_required".into(); j.error=Some(e); save_locked(&j)?; return Ok(output(&j)); }
    let launch = command("/usr/bin/systemd-run", &["--unit",&unit_name(&j),"--no-block","--property=Type=oneshot",
        "--property=RemainAfterExit=yes","--property=Restart=no","--property=WorkingDirectory=/","--",
        helper(&j.manifest.config).to_str().ok_or("invalid helper path")?,"--bootstrap-host-update","apply",
        path.to_str().ok_or("invalid journal path")?,intent]).await;
    let _guard = lock(&j.manifest.config.state_dir)?;
    j = load(&path,Some(intent))?;
    if let Err(e) = launch { if j.status != "succeeded" { j.error=Some(e); save(&j)?; } }
    Ok(output(&j))
}

fn preserved(j: &Journal) -> Result<()> {
    if j.originals.len() != j.manifest.files.len()+1 || j.originals[0].path != j.manifest.config.executable
        || j.originals[0].sha256.as_ref() != Some(&j.manifest.source.disk_sha256) { return Err("invalid preservation evidence".into()); }
    for (i,old) in j.originals.iter().enumerate() {
        if i > 0 && (old.path != j.manifest.files[i-1].path || old.sha256 != j.manifest.files[i-1].before_sha256) {
            return Err("preservation identity mismatch".into());
        }
        if let Some(hash) = &old.sha256 {
            if sha(&read(&backup(&j.manifest.config,i),host_bundle::LIMIT as usize)?) != *hash { return Err("preserved source is corrupt".into()); }
        }
    }
    Ok(())
}

fn desired(j: &Journal, index: usize) -> Result<Vec<u8>> {
    let f=&j.manifest.files[index];
    if let Some(encoded)=&f.after_base64 { return STANDARD.decode(encoded).map_err(|_| "invalid after_base64".into()); }
    let before=read(&backup(&j.manifest.config,index+1),LIMIT)?;
    if f.before_sha256.as_ref() != Some(&sha(&before)) { return Err("preserved file digest differs".into()); }
    if f.preserve == Some(true) { return Ok(before); }
    let Process::Supervisor {program}=&j.manifest.config.process else { return Err("native edit requires Supervisor".into()); };
    supervisor_environment(&before,program,&j.manifest.mapping_path)
}

/// Deliberately supports only one single-line environment in one ungrouped
/// program section. Never parses/prints secret values into the caller's intent.
/// Unsupported continuations, comments and duplicate assignments fail closed.
fn supervisor_environment(before: &[u8], program: &str, mapping: &Path) -> Result<Vec<u8>> {
    let text=std::str::from_utf8(before).map_err(|_| "Supervisor file must be UTF-8")?;
    let path=mapping.to_str().ok_or("invalid mapping path")?;
    if !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b)) { return Err("native Supervisor mapping path requires plain ASCII path characters".into()); }
    let section=format!("[program:{program}]"); let assignment=format!("APP_LB_HOST_UPDATE_CONFIG=\"{path}\"");
    let mut offset=0; let mut header=None; let mut active=false; let mut env=None; let mut previous_indent=None;
    for line in text.split_inclusive('\n') {
        let body=line.trim_end_matches(['\r','\n']); let trimmed=body.trim();
        if trimmed.starts_with('[') {
            active=trimmed == section;
            if active {
                if header.is_some() { return Err("duplicate Supervisor program section".into()); }
                header=Some((offset+line.len(),if line.ends_with("\r\n") {"\r\n"} else {"\n"},line.ends_with('\n')));
            }
            previous_indent=None;
        } else if active && !trimmed.is_empty() && !trimmed.starts_with(['#',';']) {
            let indent=body.len()-body.trim_start().len();
            if previous_indent.is_some_and(|i| indent>i) { return Err("multiline Supervisor program values are unsupported".into()); }
            previous_indent=Some(indent);
            if trimmed.split_once(':').is_some_and(|(k,_)| k.trim().eq_ignore_ascii_case("environment")) {
                return Err("Supervisor environment must use '=' delimiter".into());
            }
            if let Some((key,value))=trimmed.split_once('=') {
                if key.trim().eq_ignore_ascii_case("environment") {
                    if env.is_some() { return Err("duplicate Supervisor environment".into()); }
                    let mut quote=None; let mut escaped=false; let mut start=0; let mut assignments=Vec::new();
                    for (i,ch) in value.char_indices() {
                        if escaped { escaped=false; continue; }
                        if ch=='\\' { escaped=true; continue; }
                        if quote == Some(ch) { quote=None; continue; }
                        if quote.is_none() {
                            if ch=='\'' || ch=='"' { quote=Some(ch); }
                            else if ch==',' { assignments.push(&value[start..i]); start=i+1; }
                            else if ch=='#' || ch==';' { return Err("commented Supervisor environment is unsupported".into()); }
                        }
                    }
                    if quote.is_some() || escaped { return Err("ambiguous Supervisor environment quoting".into()); }
                    if !value.trim().is_empty() { assignments.push(&value[start..]); }
                    let mut keys=std::collections::HashSet::new();
                    for item in assignments {
                        let (name,value)=item.split_once('=').ok_or("invalid Supervisor environment assignment")?;
                        let name=name.trim();
                        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b==b'_') || value.trim().is_empty()
                            || name=="APP_LB_HOST_UPDATE_CONFIG" || !keys.insert(name) { return Err("duplicate, invalid or preconfigured Supervisor environment key".into()); }
                    }
                    env=Some((offset+body.len(),!value.trim().is_empty()));
                }
            }
        }
        offset+=line.len();
    }
    let (header_end,newline,has_newline)=header.ok_or("mapped Supervisor program section missing")?;
    let mut after=text.to_string();
    if let Some((end,nonempty))=env { after.insert_str(end,&format!("{}{assignment}",if nonempty {","} else {""})); }
    else { after.insert_str(header_end,&format!("{}environment={assignment}{newline}",if has_newline {""} else {newline})); }
    if after.len()>LIMIT { return Err("derived Supervisor configuration exceeds budget".into()); }
    Ok(after.into_bytes())
}

async fn apply(path: &Path, intent: &str) -> Result<Value> {
    let mut j = load(path,Some(intent))?;
    let c = j.manifest.config.clone();
    mkdir(&c.state_dir.join("executor"))?; trusted(&c.state_dir.join("executor/lock"))?;
    let _executor = executor_lock(&c.state_dir.join("executor"))?;
    let guard = lock(&c.state_dir)?;
    j = load(path,Some(intent))?;
    if j.phase != "launching" || j.status != "running" || running_sha()? != j.manifest.helper_sha256 {
        return Err("bootstrap helper is not authorized in this phase".into());
    }
    j.phase="checking".into(); save(&j)?;
    drop(guard);
    let result: Result<()> = async {
        preflight(&j.manifest).await?; preserved(&j)?;
        for (i,old) in j.originals.iter().enumerate() {
            if original(&old.path,if i == 0 {host_bundle::LIMIT as usize} else {LIMIT})? != *old { return Err("preserved preimage or mode changed".into()); }
        }
        let bytes = read(&helper(&c),host_bundle::LIMIT as usize)?;
        if sha(&bytes) != j.manifest.target.binary_sha256 { return Err("bootstrap executable is corrupt".into()); }
        j.phase="installing".into(); save_locked(&j)?;
        for (i,f) in j.manifest.files.iter().enumerate() {
            if f.preserve != Some(true) { write(&f.path,&desired(&j,i)?,f.mode)?; }
        }
        write(&c.executable,&bytes,j.originals[0].mode.ok_or("missing executable mode")?)?;
        j.phase="reloading".into(); save_locked(&j)?;
        match &c.process {
            Process::Systemd {..} => { command("/usr/bin/systemctl", &["daemon-reload"]).await?; }
            Process::Supervisor {program} => {
                let changed = command("/usr/bin/supervisorctl", &["reread"]).await?;
                if changed.trim() != format!("{program}: changed") { return Err("Supervisor reread must change only the mapped program".into()); }
            }
        }
        if generation(&c).await? != j.manifest.source.generation { return Err("source process changed before restart".into()); }
        j.phase="restarting".into(); save_locked(&j)?;
        match &c.process {
            Process::Systemd {unit} => { command("/usr/bin/systemctl", &["restart",unit]).await?; }
            Process::Supervisor {program} => { command("/usr/bin/supervisorctl", &["update",program]).await?; }
        }
        j.phase="awaiting_verification".into(); save_locked(&j)?; Ok(())
    }.await;
    if let Err(e) = result { j.status="reconciliation_required".into(); j.error=Some(e); save_locked(&j)?; }
    Ok(output(&j))
}

/// Called only by the namespace-admin authenticated route, never by the CLI.
pub async fn get(c: &Config, mapping: &Path, id: &str) -> Result<Value> {
    let path = journal_path(c);
    if !path.exists() { return Err("operation not found".into()); }
    // A verified bootstrap must also have relinquished mutation ownership
    // before allowing normal rollout. The executor may span awaits; ledger
    // locks never do. A still-running helper yields a retryable GET error.
    mkdir(&c.state_dir.join("executor"))?; trusted(&c.state_dir.join("executor/lock"))?;
    let _executor = executor_lock(&c.state_dir.join("executor"))?;
    let mut j = load(&path,None)?;
    if j.manifest.operation_id != id { return Err("operation not found".into()); }
    if &j.manifest.config != c || j.manifest.mapping_path != mapping { return Err("bootstrap target configuration changed".into()); }
    if !matches!(j.phase.as_str(),"installing"|"reloading"|"restarting"|"awaiting_verification"|"complete") { return Ok(output(&j)); }
    let verified: Result<()> = async {
        preserved(&j)?;
        let observed = source(c).await?;
        if observed.generation == j.manifest.source.generation || observed.generation.pid != std::process::id()
            || observed.disk_sha256 != j.manifest.target.binary_sha256 || observed.running_sha256 != j.manifest.target.binary_sha256
            || running_sha()? != j.manifest.target.binary_sha256 || compiled_revision() != j.manifest.target.revision
            || environment(observed.generation.pid)?.as_deref() != Some(mapping.as_os_str().as_encoded_bytes()) {
            return Err("replacement process identity or effective mapping differs".into());
        }
        for (i,f) in j.manifest.files.iter().enumerate() {
            let bytes = read(&f.path,LIMIT)?;
            if bytes != desired(&j,i)?
                || fs::metadata(&f.path).map_err(|e| e.to_string())?.mode() & 0o777 != f.mode { return Err("AFTER configuration differs".into()); }
        }
        let response = http()?.get(&c.health_url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() || response.headers().get_all("x-heyo-revision").iter().count() != 1
            || response.headers().get("x-heyo-revision").and_then(|v| v.to_str().ok()) != Some(j.manifest.target.revision.as_str()) {
            return Err("public revision verification failed".into());
        }
        for p in std::iter::once(&c.executable).chain(c.config_files.iter()) {
            File::open(p).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
            File::open(p.parent().unwrap()).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        }
        if source(c).await? != observed { return Err("replacement changed during verification".into()); }
        Ok(())
    }.await;
    match verified {
        Ok(()) => { j.status="succeeded".into(); j.phase="complete".into(); j.error=None; save_locked(&j)?; }
        Err(e) => { j.status="reconciliation_required".into(); j.error=Some(e); }
    }
    Ok(output(&j))
}

pub(super) fn cli(args: &[String]) -> i32 {
    let runtime = tokio::runtime::Runtime::new().expect("bootstrap runtime");
    let result: Result<Value> = runtime.block_on(async {
        let action = args.get(2).ok_or("missing bootstrap action")?;
        let path = Path::new(args.get(3).ok_or("missing bootstrap file")?);
        match (action.as_str(),args.len()) {
            ("inspect",4) => {
                let c: Config = serde_json::from_slice(&read(path,LIMIT)?).map_err(|e| e.to_string())?; validate_config(&c)?;
                if c.config_files.len() > 32 { return Err("too many configuration files".into()); }
                let mut files = Vec::new(); for p in &c.config_files { files.push(original(p,LIMIT)?); }
                Ok(json!({"protocol":PROTOCOL,"source":source(&c).await?,"files":files}))
            }
            ("status",5) => {
                trusted(path)?;
                if !path.exists() { return Ok(json!({"protocol":PROTOCOL,"status":"not_found","intent_sha256":args[4]})); }
                Ok(output(&load(path,Some(&args[4]))?))
            }
            ("admit",5) => {
                let bytes = read(path,LIMIT)?;
                if fs::metadata(path).map_err(|e| e.to_string())?.mode() & 0o077 != 0 { return Err("manifest must be owner-only".into()); }
                admit(serde_json::from_slice(&bytes).map_err(|e| e.to_string())?,&args[4]).await
            }
            ("apply",5) => apply(path,&args[4]).await,
            _ => Err("invalid bootstrap arguments".into()),
        }
    });
    match result { Ok(v) => { println!("{v}"); 0 }, Err(e) => { println!("{}",json!({"protocol":PROTOCOL,"error":e})); 1 } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::{fixture, Fixture};

    async fn manifest(f: &Fixture) -> Manifest {
        let root = f.dir.path();
        fs::write(root.join("boot-id"),"11111111-2222-3333-4444-555555555555").unwrap();
        fs::write(root.join("process-executable"),f.config.executable.to_str().unwrap()).unwrap();
        fs::write(root.join("proc/environ"),b"SAFE=1\0").unwrap();
        fs::remove_file(root.join("proc/exe")).unwrap();
        fs::copy(root.join("installed"),root.join("source-running")).unwrap();
        std::os::unix::fs::symlink(root.join("source-running"),root.join("proc/exe")).unwrap();
        let bytes = host_bundle::tests::bundle(&f.request.revision,false,false);
        fs::write(root.join("running"),host_bundle::executable(&bytes,&f.request.revision).unwrap()).unwrap();
        let script = format!(r##"#!/bin/sh
set -eu
pwd >> '{}'
cd '{}'
printf '%s\n' "$*" >> calls
case "$1" in
cat) cat unit;;
show) case "$*" in *MainPID*) echo {};; *) printf 'LoadState=loaded\nUser=root\n';; esac;;
pid) echo {};;
status) echo 'app-lb RUNNING pid 1';;
daemon-reload) [ ! -e fail-reload ];;
reread) if [ -e other-program ]; then printf 'app-lb: changed\nother: changed\n'; else echo 'app-lb: changed'; fi;;
restart|update)
 printf '%s\n' "$*" >> restarts
 [ ! -e fail-restart ] || exit 1
 cp installed source-running
 cp installed running
 cp next-stat proc/stat
 printf 'APP_LB_HOST_UPDATE_CONFIG={}\0' > proc/environ
 ;;
*) exit 9;;
esac
"##,root.join("caller-cwds").display(),root.display(),std::process::id(),std::process::id(),f.path.display());
        for name in ["systemctl","supervisorctl"] { atomic(&root.join(name),script.as_bytes(),0o755).unwrap(); }
        let mut config = f.config.clone(); config.config_files.push(f.path.clone());
        fs::remove_file(&f.path).unwrap();
        let source = source(&config).await.unwrap();
        Manifest {operation_id:"bootstrap-1".into(),helper_sha256:f.request.binary_sha256.clone(),source,
            files:vec![Change {path:root.join("unit"),before_sha256:Some(file_sha(&root.join("unit")).unwrap()),
                after_base64:Some(STANDARD.encode(format!("Environment=APP_LB_HOST_UPDATE_CONFIG={}\n",f.path.display()))),preserve:None,supervisor_environment:None,mode:0o600},
                Change {path:f.path.clone(),before_sha256:None,after_base64:Some(STANDARD.encode(serde_json::to_vec(&config).unwrap())),preserve:None,supervisor_environment:None,mode:0o600}],
            config,mapping_path:f.path.clone(),target:Target {artifact_sha256:f.request.artifact_sha256.clone(),
                binary_sha256:f.request.binary_sha256.clone(),revision:f.request.revision.clone()}}
    }

    #[tokio::test]
    async fn bootstrap_both_supervisors_preserve_and_only_get_unfences() {
        for supervisor in [false,true] {
            let f = fixture(supervisor).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(),async {
                let m = manifest(&f).await; let hash = sha(&canonical(&m).unwrap());
                let value = admit(m.clone(),&hash).await.unwrap();
                assert_eq!(value["phase"],"launching"); assert_eq!(value["intent_sha256"],hash);
                assert!(!value.to_string().contains("after_base64"));
                assert!(unblocked(&m.config).is_err());
                let path = journal_path(&m.config);
                assert_eq!(apply(&path,&hash).await.unwrap()["status"],"running");
                assert!(unblocked(&m.config).is_err());
                assert!(apply(&path,&hash).await.is_err());
                for (code,header) in [(200,None),(200,Some("b".repeat(40))),(302,Some(m.target.revision.clone())),(500,Some(m.target.revision.clone()))] {
                    *f.health.lock().unwrap()=(code,header);
                    assert_eq!(get(&m.config,&f.path,&m.operation_id).await.unwrap()["status"],"reconciliation_required");
                    assert!(unblocked(&m.config).is_err());
                }
                *f.health.lock().unwrap()=(200,Some(m.target.revision.clone()));
                assert_eq!(get(&m.config,&f.path,&m.operation_id).await.unwrap()["status"],"succeeded");
                unblocked(&m.config).unwrap();
                admit(m.clone(),&hash).await.unwrap();
                assert_eq!(fs::read_to_string(f.dir.path().join("launches")).unwrap().lines().count(),1);
                assert_eq!(fs::read_to_string(f.dir.path().join("restarts")).unwrap(),if supervisor {"update app-lb\n"} else {"restart app-lb-eu1.service\n"});
                assert!(fs::read_to_string(f.dir.path().join("caller-cwds")).unwrap().lines().all(|l| l == "/"));
                assert_eq!(fs::read(backup(&m.config,0)).unwrap(),b"\x7fELFprevious executable");
                assert!(load(&path,Some(&hash)).unwrap().originals[2].sha256.is_none());
                assert_eq!(fs::read_to_string(f.dir.path().join("preserve-workspace")).unwrap(),"never touched");
                let s = snapshot(&m.config).await.unwrap();
                let mut next = f.request.clone(); next.expected_binary_sha256=m.target.binary_sha256.clone();
                next.expected_config_sha256=s["config_sha256"].as_str().unwrap().into();
                let (_,generation) = unit(&m.config).await.unwrap();
                assert!(super::super::admit(&m.config,next,&m.target.binary_sha256,s["config_sha256"].as_str().unwrap(),&generation).unwrap().1);
            }).await;
        }
    }

    #[tokio::test]
    async fn bootstrap_lost_launch_conflict_and_concurrent_replay_never_relaunch() {
        let f = fixture(false).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(),async {
            let m = manifest(&f).await; let hash=sha(&canonical(&m).unwrap());
            fs::write(f.dir.path().join("lose-launch"),"").unwrap();
            let (a,b)=tokio::join!(admit(m.clone(),&hash),admit(m.clone(),&hash));
            assert!(a.is_ok() || b.is_ok());
            admit(m.clone(),&hash).await.unwrap();
            assert_eq!(fs::read_to_string(f.dir.path().join("launches")).unwrap().lines().count(),1);
            let mut wrong=m.clone(); wrong.operation_id="another".into();
            assert!(admit(wrong.clone(),&sha(&canonical(&wrong).unwrap())).await.is_err());
            apply(&journal_path(&m.config),&hash).await.unwrap();
            assert_eq!(get(&load_config(&f.path).unwrap(),&f.path,&m.operation_id).await.unwrap()["status"],"succeeded");
        }).await;
    }

    #[tokio::test]
    async fn bootstrap_faults_remain_fenced_without_retries_or_data_loss() {
        for fault in ["config","source","generation","backup","candidate","reload","restart","other-program","config-write","binary-write","journal-write"] {
            let f=fixture(true).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(),async {
                let m=manifest(&f).await; let hash=sha(&canonical(&m).unwrap());
                admit(m.clone(),&hash).await.unwrap();
                match fault {
                    "config" => fs::write(f.dir.path().join("unit"),"drift").unwrap(),
                    "source" => fs::write(&m.config.executable,"drift").unwrap(),
                    "generation" => fs::write(f.dir.path().join("boot-id"),"another-boot").unwrap(),
                    "backup" => fs::write(backup(&m.config,0),"corrupt").unwrap(),
                    "candidate" => fs::write(helper(&m.config),"corrupt").unwrap(),
                    "reload" | "other-program" => fs::write(f.dir.path().join("other-program"),"").unwrap(),
                    "restart" => fs::write(f.dir.path().join("fail-restart"),"").unwrap(),
                    "config-write" => fs::create_dir(f.path.with_extension("writing")).unwrap(),
                    "binary-write" => fs::create_dir(m.config.executable.with_extension("writing")).unwrap(),
                    "journal-write" => fs::create_dir(journal_path(&m.config).with_extension("writing")).unwrap(),
                    _=>unreachable!(),
                }
                let result=apply(&journal_path(&m.config),&hash).await;
                if let Ok(value)=result { assert_ne!(value["status"],"succeeded"); }
                assert!(unblocked(&m.config).is_err());
                assert_ne!(get(&m.config,&f.path,&m.operation_id).await.unwrap()["status"],"succeeded");
                admit(m.clone(),&hash).await.unwrap();
                assert_eq!(fs::read_to_string(f.dir.path().join("launches")).unwrap().lines().count(),1);
                assert_eq!(fs::read_to_string(f.dir.path().join("preserve-workspace")).unwrap(),"never touched");
                if fault != "backup" { assert_eq!(fs::read(backup(&m.config,0)).unwrap(),b"\x7fELFprevious executable"); }
            }).await;
        }
    }

    #[tokio::test]
    async fn bootstrap_persisted_prelaunch_and_partial_install_are_not_replayed() {
        for phase in ["preserving","checking","installing","reloading","restarting"] {
            let f=fixture(false).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(),async {
                let m=manifest(&f).await; let hash=sha(&canonical(&m).unwrap());
                admit(m.clone(),&hash).await.unwrap();
                let path=journal_path(&m.config); let mut j=load(&path,Some(&hash)).unwrap();
                j.phase=phase.into(); save(&j).unwrap();
                if phase != "preserving" { write(&m.files[0].path,&STANDARD.decode(m.files[0].after_base64.as_ref().unwrap()).unwrap(),m.files[0].mode).unwrap(); }
                assert!(apply(&path,&hash).await.is_err());
                admit(m.clone(),&hash).await.unwrap();
                assert_ne!(get(&m.config,&f.path,&m.operation_id).await.unwrap()["status"],"succeeded");
                assert!(!f.dir.path().join("restarts").exists());
                assert!(unblocked(&m.config).is_err());
            }).await;
        }
    }

    #[tokio::test]
    async fn bootstrap_completed_install_still_requires_exact_local_evidence() {
        for fault in ["environment","compiled","running","path","configuration","backup","owner-active"] {
            let f=fixture(false).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(),async {
                let m=manifest(&f).await; let hash=sha(&canonical(&m).unwrap());
                admit(m.clone(),&hash).await.unwrap(); apply(&journal_path(&m.config),&hash).await.unwrap();
                let mut owner=None;
                match fault {
                    "environment"=>fs::write(f.dir.path().join("proc/environ"),"APP_LB_HOST_UPDATE_CONFIG=/wrong\0").unwrap(),
                    "compiled"=>fs::write(f.dir.path().join("revision"),"b".repeat(40)).unwrap(),
                    "running"=>fs::write(f.dir.path().join("source-running"),"other binary").unwrap(),
                    "path"=>fs::write(f.dir.path().join("process-executable"),"/other/app-lb").unwrap(),
                    "configuration"=>fs::write(&m.files[0].path,"different AFTER").unwrap(),
                    "backup"=>fs::write(backup(&m.config,1),"different BEFORE").unwrap(),
                    "owner-active"=>owner=Some(executor_lock(&m.config.state_dir.join("executor")).unwrap()),
                    _=>unreachable!(),
                }
                let result=get(&m.config,&f.path,&m.operation_id).await;
                if let Ok(value)=result { assert_ne!(value["status"],"succeeded"); }
                assert!(unblocked(&m.config).is_err()); drop(owner);
            }).await;
        }
    }

    #[tokio::test]
    async fn bootstrap_corrupt_archive_and_initial_persistence_failure_do_not_launch() {
        for corrupt in [true,false] {
            let f=fixture(false).await;
            TEST_HOST.scope(f.dir.path().to_path_buf(),async {
                let mut m=manifest(&f).await;
                if corrupt { m.target.artifact_sha256="f".repeat(64); }
                else { mkdir(&m.config.state_dir).unwrap(); fs::create_dir(journal_path(&m.config).with_extension("writing")).unwrap(); }
                let hash=sha(&canonical(&m).unwrap());
                let result=admit(m.clone(),&hash).await;
                if corrupt {
                    assert_eq!(result.unwrap()["status"],"reconciliation_required");
                    admit(m.clone(),&hash).await.unwrap(); assert!(unblocked(&m.config).is_err());
                } else { assert!(result.is_err()); }
                assert!(!f.dir.path().join("launches").exists());
                assert_eq!(fs::read(&m.config.executable).unwrap(),b"\x7fELFprevious executable");
            }).await;
        }
    }

    #[tokio::test]
    async fn bootstrap_duplicate_public_header_does_not_attest() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let f=fixture(false).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(),async {
            let mut m=manifest(&f).await;
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            m.config.health_url=format!("http://{}/healthz",listener.local_addr().unwrap());
            m.files[1].after_base64=Some(STANDARD.encode(serde_json::to_vec(&m.config).unwrap()));
            let revision=m.target.revision.clone();
            let server=tokio::spawn(async move {
                let (mut stream,_)=listener.accept().await.unwrap(); let mut buf=[0;4096]; stream.read(&mut buf).await.unwrap();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nx-heyo-revision: {revision}\r\nx-heyo-revision: {revision}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            let hash=sha(&canonical(&m).unwrap()); admit(m.clone(),&hash).await.unwrap(); apply(&journal_path(&m.config),&hash).await.unwrap();
            assert_eq!(get(&m.config,&f.path,&m.operation_id).await.unwrap()["status"],"reconciliation_required");
            assert!(unblocked(&m.config).is_err()); server.await.unwrap();
        }).await;
    }

    #[tokio::test]
    async fn bootstrap_preserves_secret_files_and_edits_supervisor_locally() {
        let f=fixture(true).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(),async {
            let mut m=manifest(&f).await;
            let before=b"[program:other]\nenvironment=KEEP=untouched\n[program:app-lb]\ncommand=/usr/local/bin/app-lb\nenvironment=TOKEN=\"local-secret,with;punctuation\",OTHER=\"a=b\"\n";
            fs::write(&m.files[0].path,before).unwrap();
            m.files[0].before_sha256=Some(sha(before)); m.files[0].mode=0o644;
            m.files[0].after_base64=None; m.files[0].supervisor_environment=Some(true);
            let secret=f.dir.path().join("unchanged.env"); fs::write(&secret,b"ONLY_LOCAL=keep-this-secret\n").unwrap();
            let inode=fs::metadata(&secret).unwrap().ino();
            m.config.config_files.push(secret.clone());
            m.files[1].after_base64=Some(STANDARD.encode(serde_json::to_vec(&m.config).unwrap()));
            m.files.push(Change {path:secret.clone(),before_sha256:Some(file_sha(&secret).unwrap()),mode:0o644,
                after_base64:None,preserve:Some(true),supervisor_environment:None});
            let encoded=canonical(&m).unwrap(); assert!(!String::from_utf8_lossy(&encoded).contains("secret"));
            let hash=sha(&encoded); let result=admit(m.clone(),&hash).await.unwrap();
            assert_eq!(result["phase"],"launching"); assert!(!result.to_string().contains("secret"));
            apply(&journal_path(&m.config),&hash).await.unwrap();
            let expected=format!("{},APP_LB_HOST_UPDATE_CONFIG=\"{}\"\n",std::str::from_utf8(before).unwrap().trim_end_matches('\n'),f.path.display());
            assert_eq!(fs::read(&m.files[0].path).unwrap(),expected.as_bytes());
            assert_eq!(fs::metadata(&secret).unwrap().ino(),inode,"preserve never rewrites the original file");
            assert_eq!(fs::read(&secret).unwrap(),b"ONLY_LOCAL=keep-this-secret\n");
            assert_eq!(get(&m.config,&f.path,&m.operation_id).await.unwrap()["status"],"succeeded");
        }).await;
    }

    #[test]
    fn bootstrap_native_environment_rejects_ambiguous_syntax_and_preserves_bytes() {
        let path=Path::new("/opt/app-lb/mapping.json");
        assert_eq!(supervisor_environment(b"[program:app-lb]\r\ncommand=x\r\n[program:other]\r\ncommand=y\r\n","app-lb",path).unwrap(),
            b"[program:app-lb]\r\nenvironment=APP_LB_HOST_UPDATE_CONFIG=\"/opt/app-lb/mapping.json\"\r\ncommand=x\r\n[program:other]\r\ncommand=y\r\n");
        for body in ["environment=X=1\n  Y=2\n","environment=X=1 ; comment\n","environment=X=\"unterminated\n",
            "environment: X=1\n","environment=X=1\nenvironment=Y=2\n","environment=APP_LB_HOST_UPDATE_CONFIG=old\n",
            "environment=X=1,X=2\n","[program:app-lb]\n","command=foo\n  environment=X=1\n"] {
            assert!(supervisor_environment(format!("[program:app-lb]\n{body}").as_bytes(),"app-lb",path).is_err(),"{body}");
        }
    }

    #[tokio::test]
    async fn bootstrap_schema_path_budget_and_source_rejections() {
        let f=fixture(false).await;
        TEST_HOST.scope(f.dir.path().to_path_buf(),async {
            let m=manifest(&f).await;
            let mut v=serde_json::to_value(&m).unwrap(); v["extra"]=json!(true);
            assert!(serde_json::from_value::<Manifest>(v).is_err());
            assert!(admit(m.clone(),&"0".repeat(64)).await.is_err());
            fs::write(f.dir.path().join("revision"),"unknown").unwrap();
            assert!(admit(m.clone(),&sha(&canonical(&m).unwrap())).await.is_err());
            fs::write(f.dir.path().join("revision"),&m.target.revision).unwrap();
            let mut wrong=m.clone(); wrong.source.generation.start_time+=1;
            assert!(admit(wrong.clone(),&sha(&canonical(&wrong).unwrap())).await.is_err());
            wrong=m.clone(); wrong.files[0].preserve=Some(true); assert!(validate(&wrong).is_err());
            wrong=m.clone(); wrong.files[0].after_base64=None; wrong.files[0].preserve=Some(false); assert!(validate(&wrong).is_err());
            wrong=m.clone(); wrong.files[0].after_base64=Some(STANDARD.encode(vec![0;LIMIT+1])); assert!(validate(&wrong).is_err());
            fs::write(f.dir.path().join("oversize"),vec![0;LIMIT+1]).unwrap(); assert!(read(&f.dir.path().join("oversize"),LIMIT).is_err());
            std::os::unix::fs::symlink(&m.files[0].path,f.dir.path().join("link")).unwrap(); assert!(trusted(&f.dir.path().join("link")).is_err());
            fs::set_permissions(&m.files[0].path,fs::Permissions::from_mode(0o666)).unwrap(); assert!(validate(&m).is_err());
            assert!(!journal_path(&m.config).exists());
        }).await;
    }
}
