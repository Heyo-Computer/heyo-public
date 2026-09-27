//! The submit endpoint: `git submit` posts a bounded source descriptor here.
//!
//! The descriptor identifies an immutable base revision and target tree, carries
//! a patch for a runner-owned checkout, and includes only the workflow YAML the
//! server needs to plan the run. The server validates and stores that metadata;
//! it neither clones nor expands repository content.
//!
//! ## The signature is the whole security boundary
//!
//! This route is in the deployment's `public_paths`, because an app-lb gate
//! admits browsers only and `git submit` is not a browser. So the credential
//! here stands between the open internet and arbitrary code execution on a
//! runner.
//! It is compared in constant time, over the raw body, before the body is
//! parsed — a JSON parse on unauthenticated input is a decision, not a default.

use crate::config::Config;
use crate::paths::Changes;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use sha2::Sha256;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use subtle::ConstantTimeEq;

/// Header carrying the signature, matching what `git submit` already sends.
pub const SIGNATURE_HEADER: &str = "x-heyo-signature-256";
/// Header carrying the client's version, for diagnosing a stale client.
pub const VERSION_HEADER: &str = "x-heyo-git-ci-version";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryRef {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub release_base_sha: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitIdentity {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// The submitted tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceArchive {
    /// `tar.gz` or `git-bundle`. Named rather than assumed so an unknown format
    /// is a rejected value instead of a corrupt unpack — and so a client older
    /// than this server keeps working by saying which one it sent.
    pub format: String,
    pub content_base64: String,
    /// The archive already decoded. Never on the wire — a re-run sets it from
    /// the bytes a submit stored on disk, and [`materialize`] takes it over
    /// `content_base64` so those bytes do not go through base64 and back to
    /// arrive where they already are.
    #[serde(skip)]
    pub bytes: Option<Vec<u8>>,
}

/// The small, durable description of source a worker must reconstruct.
/// Repository location and credentials deliberately are not part of this
/// client-controlled value; the executor takes the canonical URL from ci_run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitPatchSource {
    pub base_revision: String,
    pub target_tree: String,
    pub patch_base64: String,
    #[serde(deserialize_with = "deserialize_workflows")]
    pub workflows: BTreeMap<String, String>,
    #[serde(default)]
    pub changes: Changes,
}

fn deserialize_workflows<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, MapAccess, Visitor};

    struct WorkflowsVisitor;
    impl<'de> Visitor<'de> for WorkflowsVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an object of unique workflow paths and YAML text")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut workflows = BTreeMap::new();
            while let Some((path, text)) = map.next_entry::<String, String>()? {
                if workflows.insert(path.clone(), text).is_some() {
                    return Err(A::Error::custom(format!("duplicate workflow path {path:?}")));
                }
            }
            Ok(workflows)
        }
    }

    deserializer.deserialize_map(WorkflowsVisitor)
}

impl GitPatchSource {
    pub fn patch(&self) -> Result<Vec<u8>, TriggerError> {
        base64::engine::general_purpose::STANDARD
            .decode(self.patch_base64.as_bytes())
            .map_err(|e| TriggerError::BadArchive(format!("patchBase64 is not valid base64: {e}")))
    }
}

/// What a re-run carries that a submit does not. Set only in-process by
/// [`crate::dispatch::Dispatcher::rerun`], never by a client: the field is
/// skipped by serde, so a payload cannot claim to be one.
#[derive(Debug, Clone)]
pub struct Rerun {
    /// The finished run whose source this re-plays.
    pub of: String,
    /// Carry the jobs that succeeded in `of` over as finished, and schedule
    /// only the rest. `false` runs everything again.
    pub failed_only: bool,
    /// What `of` recorded as its commit's changes — the same commit, so the
    /// same answer, and every `changed()` filter decides as it did then.
    pub changes: crate::paths::Changes,
}

/// What `git submit` posts.
///
/// Field names follow the existing `git submit` payload so the two clients stay
/// recognisably related.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitRequest {
    #[serde(default)]
    pub repository: RepositoryRef,
    #[serde(default)]
    pub r#ref: String,
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub after: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub pusher: Option<GitIdentity>,
    /// Which workflow object this run belongs to. Absent means "every workflow
    /// object whose repository matches", resolved by the caller.
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// Run only the workflow *files* these selectors name — `git submit
    /// --only`. A selector matches a file's path (`.ci/workflows/app-lb.yml`),
    /// its basename with or without the extension (`app-lb.yml`, `app-lb`), or the
    /// workflow's own `name:`, case-insensitively. Empty means every file the
    /// glob finds, which is what every client sent before the field existed.
    ///
    /// Distinct from `workflow_id`, which picks a registered workflow *object*
    /// (a glob + network + secrets scope); this picks files inside whatever
    /// glob applies. A selector that matches nothing fails the submit — an
    /// explicitly named workflow silently not running is the failure mode this
    /// flag exists to avoid.
    #[serde(default)]
    pub only: Vec<String>,
    pub source: SourceArchive,
    /// See [`Rerun`]. Absent on every real submit.
    #[serde(skip)]
    pub rerun: Option<Rerun>,
}

/// Whether one `--only` selector names this workflow file.
///
/// `path` is the file's path as [`find_workflows`] reports it (relative, forward
/// slashes); `name` is the parsed workflow's `name:`, when it has one.
pub fn selector_matches(selector: &str, path: &str, name: Option<&str>) -> bool {
    let sel = selector.trim();
    if sel.is_empty() {
        return false;
    }
    if sel.eq_ignore_ascii_case(path) {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    if sel.eq_ignore_ascii_case(base) {
        return true;
    }
    let stem = base
        .strip_suffix(".yml")
        .or_else(|| base.strip_suffix(".yaml"))
        .unwrap_or(base);
    if sel.eq_ignore_ascii_case(stem) {
        return true;
    }
    name.is_some_and(|n| sel.eq_ignore_ascii_case(n.trim()))
}

impl SubmitRequest {
    /// The branch name, with `refs/heads/` stripped.
    pub fn branch(&self) -> &str {
        self.r#ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&self.r#ref)
    }
}

/// Verify `x-heyo-signature-256` over the raw body.
///
/// The header is `sha256=<hex>`, matching `git submit` and GitHub's webhook
/// convention. Compared with `subtle`'s constant-time equality: a byte-wise `==`
/// returns as soon as it finds a difference, which leaks the correct prefix one
/// request at a time.
pub fn verify_signature(
    secret: &str,
    body: &[u8],
    header: Option<&str>,
) -> Result<(), TriggerError> {
    let Some(header) = header else {
        return Err(TriggerError::MissingSignature);
    };
    let hex_sig = header
        .trim()
        .strip_prefix("sha256=")
        .ok_or(TriggerError::MalformedSignature)?;
    let provided = hex::decode(hex_sig).map_err(|_| TriggerError::MalformedSignature)?;

    let mut mac = <Hmac<Sha256>>::new_from_slice(secret.as_bytes())
        .map_err(|_| TriggerError::MalformedSignature)?;
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    // Length is checked first because `ct_eq` on differing lengths is not a
    // meaningful comparison; the length of a signature is not a secret.
    if provided.len() == expected.len() && bool::from(provided.ct_eq(&expected)) {
        Ok(())
    } else {
        Err(TriggerError::BadSignature)
    }
}

/// A stored source format. Legacy formats remain identifiable so historical
/// files can be retained and rejected explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    GitPatch,
    /// Historical `git archive` of one tree; no longer accepted.
    TarGz,
    /// Historical `git bundle` of a branch; no longer accepted.
    GitBundle,
}

impl SourceFormat {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "git-patch" => Some(Self::GitPatch),
            // Recognized for locating historical data. New materialization
            // below still rejects both legacy bulk formats.
            "tar.gz" => Some(Self::TarGz),
            "git-bundle" => Some(Self::GitBundle),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GitPatch => "git-patch",
            Self::TarGz => "tar.gz",
            Self::GitBundle => "git-bundle",
        }
    }

    /// The extension the submitted bytes are stored under.
    ///
    /// Distinct per format on purpose: the executor re-derives a [`Workspace`]
    /// from a run id alone, long after the submit that chose the format, and
    /// the file that exists is what tells it which one to ship.
    fn extension(&self) -> &'static str {
        match self {
            Self::GitPatch => "source.json",
            Self::TarGz => "tar.gz",
            Self::GitBundle => "bundle",
        }
    }
}

/// Where a run's tree and the bytes it came from live.
pub struct Workspace {
    pub root: PathBuf,
    /// `git archive` tarball, when that is what was submitted.
    pub tarball: PathBuf,
    /// `git bundle`, when that is what was submitted.
    pub bundle: PathBuf,
    /// Validated git-patch descriptor. This is metadata, never a repository.
    pub descriptor: PathBuf,
}

impl Workspace {
    pub fn for_run(config: &Config, run_id: &str) -> Self {
        Self {
            root: config.workspace_dir.join(run_id),
            tarball: config
                .workspace_dir
                .join(format!("{run_id}.{}", SourceFormat::TarGz.extension())),
            bundle: config
                .workspace_dir
                .join(format!("{run_id}.{}", SourceFormat::GitBundle.extension())),
            descriptor: config
                .workspace_dir
                .join(format!("{run_id}.{}", SourceFormat::GitPatch.extension())),
        }
    }

    pub fn path_for(&self, format: SourceFormat) -> &Path {
        match format {
            SourceFormat::GitPatch => &self.descriptor,
            SourceFormat::TarGz => &self.tarball,
            SourceFormat::GitBundle => &self.bundle,
        }
    }

    /// Which source is on disk, including retained historical submissions.
    pub fn stored_source(&self) -> Option<(SourceFormat, &Path)> {
        for format in [SourceFormat::GitPatch, SourceFormat::GitBundle, SourceFormat::TarGz] {
            let path = self.path_for(format);
            if path.exists() {
                return Some((format, path));
            }
        }
        None
    }
}

/// Validate and persist a source descriptor and its bounded workflow metadata.
pub fn materialize(
    source: &SourceArchive,
    workspace: &Workspace,
    max_bytes: usize,
) -> Result<usize, TriggerError> {
    let format = SourceFormat::parse(&source.format)
        .ok_or_else(|| TriggerError::UnsupportedFormat(source.format.clone()))?;
    if format != SourceFormat::GitPatch {
        return Err(TriggerError::UnsupportedFormat(source.format.clone()));
    }

    let bytes = match &source.bytes {
        Some(bytes) => bytes.clone(),
        None => base64::engine::general_purpose::STANDARD
            .decode(source.content_base64.as_bytes())
            .map_err(|e| TriggerError::BadArchive(format!("not valid base64: {e}")))?,
    };
    if bytes.len() > max_bytes {
        return Err(TriggerError::ArchiveTooLarge {
            bytes: bytes.len(),
            max: max_bytes,
        });
    }

    // Validation must precede every filesystem mutation. A malformed retry for
    // an existing run must leave its last valid descriptor and workflows intact.
    let descriptor = decode_descriptor(&bytes)?;

    if workspace.root.exists() {
        std::fs::remove_dir_all(&workspace.root).map_err(|e| TriggerError::Io {
            path: workspace.root.clone(),
            reason: e.to_string(),
        })?;
    }
    std::fs::create_dir_all(&workspace.root).map_err(|e| TriggerError::Io {
        path: workspace.root.clone(),
        reason: e.to_string(),
    })?;

    // Persist only bounded metadata and workflow YAML. The patch remains in the
    // descriptor and is never applied or expanded by the CI server.
    for (path, text) in &descriptor.workflows {
        let destination = workspace.root.join(path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TriggerError::Io {
                path: parent.to_path_buf(), reason: e.to_string(),
            })?;
        }
        std::fs::write(&destination, text).map_err(|e| TriggerError::Io {
            path: destination, reason: e.to_string(),
        })?;
    }

    // The bytes are written after validation; malformed submissions cannot
    // replace durable source that a rerun may read later.
    let stored = workspace.path_for(format);
    std::fs::write(stored, &bytes).map_err(|e| TriggerError::Io {
        path: stored.to_path_buf(),
        reason: e.to_string(),
    })?;
    Ok(bytes.len())
}

const MAX_WORKFLOWS: usize = 128;
const MAX_WORKFLOW_BYTES: usize = 1024 * 1024;

fn validate_descriptor(source: &GitPatchSource) -> Result<(), TriggerError> {
    for (field, value) in [("baseRevision", &source.base_revision), ("targetTree", &source.target_tree)] {
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(TriggerError::BadArchive(format!("{field} must be a full hexadecimal Git object ID")));
        }
    }
    source.patch()?;
    if source.workflows.len() > MAX_WORKFLOWS {
        return Err(TriggerError::BadArchive(format!("at most {MAX_WORKFLOWS} workflow files may be submitted")));
    }
    let mut total = 0usize;
    for (path, text) in &source.workflows {
        let p = Path::new(path);
        check_contained(p)?;
        if path.contains('\\') || p.components().any(|c| matches!(c, Component::CurDir) || matches!(c, Component::Normal(v) if v == ".git"))
            || !matches!(p.extension().and_then(|v| v.to_str()), Some("yml" | "yaml"))
        {
            return Err(TriggerError::BadArchive(format!("workflow path {path:?} must be a normalized relative YAML path without .git components")));
        }
        let normalized = p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        if normalized != *path {
            return Err(TriggerError::BadArchive(format!("workflow path {path:?} is not normalized")));
        }
        total = total.saturating_add(path.len()).saturating_add(text.len());
    }
    if total > MAX_WORKFLOW_BYTES {
        return Err(TriggerError::BadArchive(format!("workflow metadata exceeds {MAX_WORKFLOW_BYTES} bytes")));
    }
    Ok(())
}

pub fn read_descriptor(workspace: &Workspace) -> Result<GitPatchSource, TriggerError> {
    read_descriptor_path(&workspace.descriptor)
}

pub fn read_descriptor_path(path: &Path) -> Result<GitPatchSource, TriggerError> {
    let bytes = std::fs::read(path).map_err(|e| TriggerError::Io {
        path: path.to_path_buf(), reason: e.to_string(),
    })?;
    decode_descriptor(&bytes)
}

pub fn decode_descriptor(bytes: &[u8]) -> Result<GitPatchSource, TriggerError> {
    let source = serde_json::from_slice(bytes)
        .map_err(|e| TriggerError::BadArchive(format!("stored git-patch descriptor is invalid: {e}")))?;
    validate_descriptor(&source)?;
    Ok(source)
}

/// Whether a path stays inside the extraction root.
fn check_contained(path: &Path) -> Result<(), TriggerError> {
    for c in path.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(TriggerError::EscapingEntry(path.display().to_string()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(TriggerError::EscapingEntry(path.display().to_string()));
            }
        }
    }
    Ok(())
}

/// What a submit changed, as recorded in its validated descriptor.
pub fn changed_paths(workspace: &Workspace, _before: &str) -> Changes {
    if let Ok(source) = read_descriptor(workspace) {
        return source.changes;
    }
    let Some((format, _)) = workspace.stored_source() else {
        return Changes::unknown("this run's submitted source is no longer on disk");
    };
    Changes::unknown(format!(
        "this historical {} submission is retained but cannot be loaded; resubmit with a git-patch client",
        format.as_str()
    ))
}

/// Find workflow files in an extracted tree.
///
/// `pattern` is the workflow object's `path`, e.g. `.ci/workflows/*.yml`. Only a
/// trailing `*` in the final segment is supported — enough for every real
/// spelling, and a full glob engine here would be a way to walk the filesystem
/// with a pattern that came over the wire.
pub fn find_workflows(root: &Path, pattern: &str) -> Result<Vec<(String, String)>, TriggerError> {
    let pattern = pattern.trim().trim_start_matches("./");
    check_contained(Path::new(pattern))?;

    let (dir_part, file_pattern) = match pattern.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", pattern),
    };
    let dir = root.join(dir_part);

    let mut found = Vec::new();
    let Ok(read) = std::fs::read_dir(&dir) else {
        // A repository with no workflow directory is not an error; it just has
        // no workflows, and saying so beats a stat failure.
        return Ok(found);
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches_pattern(&name, file_pattern) {
            continue;
        }
        if !entry.path().is_file() {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).map_err(|e| TriggerError::Io {
            path: entry.path(),
            reason: e.to_string(),
        })?;
        let rel = if dir_part.is_empty() {
            name.clone()
        } else {
            format!("{dir_part}/{name}")
        };
        found.push((rel, text));
    }
    // Sorted so a run's workflows are always planned in the same order.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// `*.yml`, `build.*`, `*`, or a literal name.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => name == pattern,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TriggerError {
    MissingSignature,
    MalformedSignature,
    BadSignature,
    UnsupportedFormat(String),
    BadArchive(String),
    EscapingEntry(String),
    ArchiveTooLarge {
        bytes: usize,
        max: usize,
    },
    Io {
        path: PathBuf,
        reason: String,
    },
    NoWorkflows(String),
}

impl TriggerError {
    /// The HTTP status this should be reported as.
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::MissingSignature | Self::MalformedSignature | Self::BadSignature => {
                StatusCode::UNAUTHORIZED
            }
            Self::ArchiveTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Deliberately identical wording for all three: telling a caller
            // *why* their signature failed tells an attacker how far they got.
            Self::MissingSignature | Self::MalformedSignature | Self::BadSignature => {
                write!(
                    f,
                    "the request carries no usable credential. Register the repository \
                     on /repos and set `git config ci.token`, or sign with the shared \
                     CI_WEBHOOK_SECRET and check that the client and the server agree \
                     on it."
                )
            }
            Self::UnsupportedFormat(fmt) => write!(
                f,
                "source archive format {fmt:?} is not supported; this server \
                 accepts only `git-patch`. Upgrade `git submit` and resubmit; legacy \
                 tar.gz and git-bundle runs are retained but cannot be run or rerun."
            ),
            Self::BadArchive(e) => write!(f, "the source descriptor could not be read: {e}"),
            Self::EscapingEntry(p) => write!(
                f,
                "the source archive contains {p:?}, which would write outside the \
                 workspace. Refusing the whole archive."
            ),
            Self::ArchiveTooLarge { bytes, max } => write!(
                f,
                "the source descriptor is {bytes} bytes, over the {max}-byte limit. \
                 Raise CI_MAX_SOURCE_BYTES or reduce the submitted patch/workflow metadata."
            ),
            Self::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::NoWorkflows(pattern) => write!(
                f,
                "no workflow files matched {pattern:?} in the submitted tree"
            ),
        }
    }
}

impl std::error::Error for TriggerError {}

#[cfg(test)]
mod tests {
    mod patches {
        use super::super::*;
        use base64::Engine;

        fn descriptor(path: &str) -> SourceArchive {
            let value = serde_json::json!({
                "baseRevision": "a".repeat(40),
                "targetTree": "b".repeat(40),
                "patchBase64": base64::engine::general_purpose::STANDARD.encode(b"diff --git a/x b/x\n"),
                "workflows": { path: "name: test\non: [submit]\njobs: {}\n" },
                "changes": { "kind": "known", "paths": ["deleted", "binary", "executable"] }
            });
            SourceArchive {
                format: "git-patch".into(),
                content_base64: base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value).unwrap()),
                bytes: None,
            }
        }

        fn workspace() -> (PathBuf, Workspace) {
            let dir = std::env::temp_dir().join(format!("ci-patch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let ws = Workspace {
                root: dir.join("run"),
                tarball: dir.join("run.tar.gz"),
                bundle: dir.join("run.bundle"),
                descriptor: dir.join("run.source.json"),
            };
            (dir, ws)
        }

        #[test]
        fn materializes_only_workflow_metadata_and_persists_descriptor() {
            let (dir, ws) = workspace();
            materialize(&descriptor(".ci/workflows/build.yml"), &ws, 1 << 20).unwrap();
            assert!(ws.descriptor.is_file());
            assert!(ws.root.join(".ci/workflows/build.yml").is_file());
            assert_eq!(std::fs::read_dir(&ws.root).unwrap().count(), 1);
            assert_eq!(read_descriptor(&ws).unwrap().changes.paths(), &["deleted", "binary", "executable"]);
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn rejects_legacy_bulk_source_and_malicious_workflow_paths() {
            let legacy = SourceArchive { format: "git-bundle".into(), content_base64: String::new(), bytes: Some(vec![]) };
            let (dir, ws) = workspace();
            assert!(matches!(materialize(&legacy, &ws, 100), Err(TriggerError::UnsupportedFormat(_))));
            for path in ["../evil.yml", "/evil.yml", ".git/hooks/evil.yml", "a/./evil.yml", "evil.txt"] {
                assert!(materialize(&descriptor(path), &ws, 1 << 20).is_err(), "accepted {path}");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn invalid_replacement_preserves_the_last_valid_source() {
            let (dir, ws) = workspace();
            let first = descriptor(".ci/workflows/old.yml");
            materialize(&first, &ws, 1 << 20).unwrap();
            let stored = std::fs::read(&ws.descriptor).unwrap();

            let invalid = descriptor("../escape.yml");
            assert!(materialize(&invalid, &ws, 1 << 20).is_err());
            assert_eq!(std::fs::read(&ws.descriptor).unwrap(), stored);
            assert!(ws.root.join(".ci/workflows/old.yml").is_file());
            assert!(!ws.root.join("escape.yml").exists());
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_valid_retry_replaces_workflow_metadata_without_merging() {
            let (dir, ws) = workspace();
            materialize(&descriptor(".ci/workflows/old.yml"), &ws, 1 << 20).unwrap();
            let replacement = descriptor(".ci/workflows/new.yml");
            let expected_size = base64::engine::general_purpose::STANDARD
                .decode(&replacement.content_base64).unwrap().len();
            assert_eq!(materialize(&replacement, &ws, 1 << 20).unwrap(), expected_size);
            assert!(!ws.root.join(".ci/workflows/old.yml").exists());
            assert!(ws.root.join(".ci/workflows/new.yml").is_file());
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn rerun_bytes_recreate_equivalent_workspace_and_changes() {
            let (dir, original) = workspace();
            materialize(&descriptor(".ci/workflows/build.yml"), &original, 1 << 20).unwrap();
            let bytes = std::fs::read(&original.descriptor).unwrap();
            let rerun = Workspace { root: dir.join("rerun"), tarball: dir.join("rerun.tar.gz"), bundle: dir.join("rerun.bundle"), descriptor: dir.join("rerun.source.json") };
            let source = SourceArchive { format: "git-patch".into(), content_base64: String::new(), bytes: Some(bytes.clone()) };
            assert_eq!(materialize(&source, &rerun, 1 << 20).unwrap(), bytes.len());
            assert_eq!(std::fs::read(&rerun.descriptor).unwrap(), bytes);
            assert_eq!(changed_paths(&original, "ignored"), changed_paths(&rerun, "ignored"));
            assert_eq!(find_workflows(&original.root, ".ci/workflows/*.yml").unwrap(), find_workflows(&rerun.root, ".ci/workflows/*.yml").unwrap());
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn malformed_descriptors_and_duplicate_paths_are_rejected() {
            let (dir, ws) = workspace();
            for json in [
                r#"{"baseRevision":"no","targetTree":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","patchBase64":"","workflows":{}}"#.to_string(),
                format!(r#"{{"baseRevision":"{}","targetTree":"{}","patchBase64":"%%%","workflows":{{}}}}"#, "a".repeat(40), "b".repeat(40)),
                format!(r#"{{"baseRevision":"{}","targetTree":"{}","patchBase64":"","workflows":{{"a.yml":"one","a.yml":"two"}}}}"#, "a".repeat(40), "b".repeat(40)),
            ] {
                let source = SourceArchive { format: "git-patch".into(), content_base64: base64::engine::general_purpose::STANDARD.encode(json), bytes: None };
                assert!(matches!(materialize(&source, &ws, 1 << 20), Err(TriggerError::BadArchive(_))));
            }
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn descriptor_reload_preserves_known_and_unknown_changes() {
            for changes in [serde_json::json!({"kind":"known","paths":["src/a.rs"]}), serde_json::json!({"kind":"unknown","reason":"base unavailable"})] {
                let (dir, ws) = workspace();
                let mut value: serde_json::Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(descriptor("build.yml").content_base64).unwrap()).unwrap();
                value["changes"] = changes;
                let source = SourceArchive { format: "git-patch".into(), content_base64: base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value).unwrap()), bytes: None };
                materialize(&source, &ws, 1 << 20).unwrap();
                assert_eq!(changed_paths(&ws, "unused"), read_descriptor(&ws).unwrap().changes);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[cfg(any())]
    mod bundles {
        use super::super::clone_bundle;
        use std::path::{Path, PathBuf};
        use std::process::Command;

        fn sh_git(dir: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn temp(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("ci-bundle-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// The plain path: a complete bundle clones into a working tree. The
        /// verify runs in a scratch repository the code makes itself — the
        /// production process does not sit inside a git repository, and `git
        /// bundle verify` refuses to run without one ("need a repository to
        /// verify a bundle"). `cargo test` *does* run inside one, which is how
        /// that dependence originally went unnoticed; the scratch repo is what
        /// makes this path independent of where the process happens to be.
        #[test]
        fn a_complete_bundle_clones() {
            let dir = temp("ok");
            let repo = dir.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            sh_git(&repo, &["init", "-q", "-b", "main"]);
            std::fs::write(repo.join("a.txt"), "hello").unwrap();
            sh_git(&repo, &["add", "a.txt"]);
            sh_git(&repo, &["commit", "-qm", "one"]);
            let bundle = dir.join("src.bundle");
            sh_git(
                &repo,
                &["bundle", "create", &bundle.display().to_string(), "--all"],
            );

            let root = dir.join("root");
            std::fs::create_dir_all(&root).unwrap();
            clone_bundle(&bundle, &root).expect("a complete bundle clones");
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).unwrap(),
                "hello"
            );
            assert!(
                !dir.join("src.verify").exists(),
                "the scratch repo is cleaned up"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A sliced bundle — one with prerequisite commits — is refused at
        /// verify, by name. The scratch repository being *empty* is what makes
        /// this real: verified against some ambient repository that happens to
        /// hold the objects, a slice would falsely pass and then clone into an
        /// empty checkout.
        #[test]
        fn a_bundle_with_prerequisites_is_refused() {
            let dir = temp("slice");
            let repo = dir.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            sh_git(&repo, &["init", "-q", "-b", "main"]);
            std::fs::write(repo.join("a.txt"), "one").unwrap();
            sh_git(&repo, &["add", "a.txt"]);
            sh_git(&repo, &["commit", "-qm", "one"]);
            std::fs::write(repo.join("a.txt"), "two").unwrap();
            sh_git(&repo, &["commit", "-aqm", "two"]);
            let bundle = dir.join("src.bundle");
            sh_git(
                &repo,
                &[
                    "bundle",
                    "create",
                    &bundle.display().to_string(),
                    "main^..main",
                ],
            );

            let root = dir.join("root");
            std::fs::create_dir_all(&root).unwrap();
            let err = clone_bundle(&bundle, &root).expect_err("a slice must be refused");
            let text = err.to_string();
            assert!(
                text.contains("not usable on its own"),
                "the refusal names the bundle, got: {text}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    mod only_selectors {
        use crate::trigger::selector_matches;

        #[test]
        fn a_selector_names_a_file_by_path_basename_stem_or_workflow_name() {
            let path = ".ci/workflows/apps.yml";
            let name = Some("Platform apps");
            for sel in [
                ".ci/workflows/apps.yml",
                "apps.yml",
                "apps",
                "APPS",
                "platform apps",
            ] {
                assert!(selector_matches(sel, path, name), "{sel}");
            }
            for sel in ["app", "apps.yaml", "ci", "", "  "] {
                assert!(!selector_matches(sel, path, name), "{sel}");
            }
        }

        #[test]
        fn yaml_extension_and_no_name_still_match() {
            assert!(selector_matches("nightly", "workflows/nightly.yaml", None));
            assert!(selector_matches(
                "nightly.yaml",
                "workflows/nightly.yaml",
                None
            ));
            assert!(!selector_matches(
                "nightly.yml",
                "workflows/nightly.yaml",
                None
            ));
        }
    }

    use super::*;
    const SECRET: &str = "0123456789abcdef";

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn a_correct_signature_is_accepted() {
        let body = br#"{"hello":"world"}"#;
        let sig = sign(SECRET, body);
        assert_eq!(verify_signature(SECRET, body, Some(&sig)), Ok(()));
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let sig = sign(SECRET, b"original");
        assert_eq!(
            verify_signature(SECRET, b"tampered", Some(&sig)),
            Err(TriggerError::BadSignature)
        );
    }

    #[test]
    fn the_wrong_secret_is_rejected() {
        let body = b"body";
        let sig = sign("a-different-secret", body);
        assert_eq!(
            verify_signature(SECRET, body, Some(&sig)),
            Err(TriggerError::BadSignature)
        );
    }

    #[test]
    fn a_missing_or_malformed_signature_is_rejected() {
        assert_eq!(
            verify_signature(SECRET, b"x", None),
            Err(TriggerError::MissingSignature)
        );
        for bad in ["", "deadbeef", "sha256=zzz", "md5=abcd", "sha256="] {
            assert!(
                verify_signature(SECRET, b"x", Some(bad)).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    /// All three failures must read identically. Distinguishing them tells an
    /// attacker whether they got the format right, which is a rung on the ladder.
    #[test]
    fn every_signature_failure_reads_the_same() {
        let a = TriggerError::MissingSignature.to_string();
        let b = TriggerError::MalformedSignature.to_string();
        let c = TriggerError::BadSignature.to_string();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(TriggerError::BadSignature.status(), 401);
    }

    #[cfg(any())]
    mod obsolete_bulk_materialization_tests {
    use super::*;
    use std::io::Write;

    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut ar = tar::Builder::new(Vec::new());
        for (name, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append_data(&mut header, name, *content).unwrap();
        }
        let tar = ar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    fn source(bytes: &[u8]) -> SourceArchive {
        SourceArchive {
            format: "tar.gz".into(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            bytes: None,
        }
    }

    fn workspace() -> Workspace {
        let base = std::env::temp_dir().join(format!("ci-trigger-{}", crate::vm::new_id()));
        std::fs::create_dir_all(&base).unwrap();
        Workspace {
            root: base.join("tree"),
            tarball: base.join("source.tar.gz"),
            bundle: base.join("source.bundle"),
            descriptor: base.join("source.json"),
        }
    }

    #[test]
    fn a_tree_extracts_and_the_archive_is_kept() {
        let ws = workspace();
        let gz = tarball(&[
            ("Cargo.lock", b"version = 3"),
            (".ci/workflows/build.yml", b"name: build"),
        ]);
        let n = materialize(&source(&gz), &ws, 1 << 20).unwrap();
        assert_eq!(n, gz.len());
        assert_eq!(
            std::fs::read_to_string(ws.root.join("Cargo.lock")).unwrap(),
            "version = 3"
        );
        assert!(
            ws.tarball.exists(),
            "the original archive is kept for the guest"
        );
        assert_eq!(std::fs::read(&ws.tarball).unwrap(), gz);
        assert_eq!(
            ws.stored_source().map(|(f, _)| f),
            Some(SourceFormat::TarGz)
        );
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// Build a tar entry whose name (and optionally link target) bypasses the
    /// `tar` crate's own write-side validation.
    ///
    /// `Builder::append_data` refuses a path containing `..`, so a hostile
    /// archive cannot be produced through the safe API — which is exactly why
    /// the header bytes are written directly here. A real attacker has no such
    /// constraint: they emit the bytes.
    fn raw_entry(name: &str, link: Option<&str>, content: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(if link.is_some() {
            0
        } else {
            content.len() as u64
        });
        header.set_mode(0o644);
        header.set_entry_type(if link.is_some() {
            tar::EntryType::Symlink
        } else {
            tar::EntryType::Regular
        });

        // Write the name straight into the 100-byte field, past validation.
        {
            let old = header.as_old_mut();
            let bytes = name.as_bytes();
            old.name[..bytes.len()].copy_from_slice(bytes);
            if let Some(link) = link {
                let lb = link.as_bytes();
                old.linkname[..lb.len()].copy_from_slice(lb);
            }
        }
        header.set_cksum();

        let mut out = header.as_bytes().to_vec();
        if link.is_none() {
            out.extend_from_slice(content);
            // Entries are padded to a 512-byte boundary.
            let pad = (512 - (content.len() % 512)) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out
    }

    fn gzip(tar_bytes: Vec<u8>) -> Vec<u8> {
        let mut full = tar_bytes;
        // Two zero blocks end an archive.
        full.extend(std::iter::repeat_n(0u8, 1024));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&full).unwrap();
        gz.finish().unwrap()
    }

    /// The headline security property: an archive is attacker-supplied, and one
    /// `..` entry would let a submit overwrite anything the process can write.
    #[test]
    fn an_entry_escaping_the_workspace_rejects_the_whole_archive() {
        for evil in ["../escaped.txt", "a/../../escaped.txt", "/etc/passwd"] {
            let ws = workspace();
            let mut bytes = raw_entry("safe.txt", None, b"ok");
            bytes.extend(raw_entry(evil, None, b"pwned"));
            let gz = gzip(bytes);

            let err = materialize(&source(&gz), &ws, 1 << 20).unwrap_err();
            assert!(
                matches!(err, TriggerError::EscapingEntry(_)),
                "{evil:?} produced {err:?}"
            );
            let outside = ws.root.parent().unwrap().join("escaped.txt");
            assert!(!outside.exists(), "{evil:?} wrote outside the workspace");
            assert!(
                !Path::new("/etc/passwd.ci-test").exists(),
                "absolute paths must not be honoured"
            );
            std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
        }
    }

    /// A symlink escapes just as well as a path does: extract `link ->
    /// ../../etc/passwd` and a later entry writing through `link` lands outside.
    #[test]
    fn a_symlink_pointing_outside_is_rejected() {
        let ws = workspace();
        let gz = gzip(raw_entry("link", Some("../../../etc/passwd"), b""));
        let err = materialize(&source(&gz), &ws, 1 << 20).unwrap_err();
        assert!(matches!(err, TriggerError::EscapingEntry(_)), "{err:?}");
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// A symlink that stays inside the tree is ordinary and must survive —
    /// plenty of repositories contain one.
    #[test]
    fn a_symlink_inside_the_tree_is_allowed() {
        let ws = workspace();
        let mut bytes = raw_entry("real.txt", None, b"hi");
        bytes.extend(raw_entry("alias.txt", Some("real.txt"), b""));
        let gz = gzip(bytes);
        materialize(&source(&gz), &ws, 1 << 20).expect("an internal symlink is fine");
        assert!(ws.root.join("alias.txt").symlink_metadata().is_ok());
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    #[test]
    fn an_oversized_archive_is_refused_with_the_limit_named() {
        let ws = workspace();
        let gz = tarball(&[("big.bin", &vec![0u8; 4096])]);
        let err = materialize(&source(&gz), &ws, 8).unwrap_err();
        match &err {
            TriggerError::ArchiveTooLarge { max, .. } => assert_eq!(*max, 8),
            other => panic!("{other:?}"),
        }
        assert!(err.to_string().contains("CI_MAX_SOURCE_BYTES"), "{err}");
        assert_eq!(err.status(), 413);
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    // ---- git bundles -----------------------------------------------------

    /// Build a real bundle the way the client does: a throwaway bare repo whose
    /// object store is borrowed through `alternates`, so the fixture repository
    /// never gains a ref.
    fn bundle_of(entries: &[(&str, &str)]) -> (Vec<u8>, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("ci-bundle-{}", crate::vm::new_id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        for (name, body) in entries {
            let path = repo.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "fixture"]);

        let bundle = base.join("source.bundle");
        git(
            &repo,
            &["bundle", "create", &bundle.display().to_string(), "--all"],
        );
        (std::fs::read(&bundle).unwrap(), base)
    }

    fn bundle_source(bytes: &[u8]) -> SourceArchive {
        SourceArchive {
            format: "git-bundle".into(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            bytes: None,
        }
    }

    /// The point of the format: the workspace is a real repository, not a bare
    /// tree, so a step can run `git describe` — and the bundle itself is kept
    /// for shipping to the guest.
    #[test]
    fn a_bundle_clones_into_a_working_tree_with_history() {
        let (bytes, base) = bundle_of(&[
            ("Cargo.lock", "version = 3"),
            (".ci/workflows/build.yml", "name: build"),
        ]);
        let ws = workspace();

        let n = materialize(&bundle_source(&bytes), &ws, 1 << 22).unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(
            std::fs::read_to_string(ws.root.join("Cargo.lock")).unwrap(),
            "version = 3"
        );
        assert!(
            ws.root.join(".git").exists(),
            "a bundle must produce a repository, which is the whole reason to send one"
        );
        assert_eq!(
            ws.stored_source().map(|(f, _)| f),
            Some(SourceFormat::GitBundle)
        );
        assert_eq!(std::fs::read(&ws.bundle).unwrap(), bytes);

        // And the tree is readable by the same workflow discovery a tarball gets.
        let found = find_workflows(&ws.root, ".ci/workflows/*.yml").unwrap();
        assert_eq!(found.len(), 1, "{found:?}");

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// Re-submitting the same run in the other format must not leave the old
    /// one behind: `stored_source` picks by what exists, so a stale file would
    /// make the executor ship the wrong bytes.
    #[test]
    fn switching_format_removes_the_previous_source() {
        let ws = workspace();
        materialize(&source(&tarball(&[("a.txt", b"1")])), &ws, 1 << 20).unwrap();
        assert!(ws.tarball.exists());

        let (bytes, base) = bundle_of(&[("a.txt", "2")]);
        materialize(&bundle_source(&bytes), &ws, 1 << 22).unwrap();
        assert!(ws.bundle.exists());
        assert!(
            !ws.tarball.exists(),
            "the previous format's bytes must not survive to be shipped"
        );
        assert_eq!(
            ws.stored_source().map(|(f, _)| f),
            Some(SourceFormat::GitBundle)
        );

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// The failure that made the client design what it did: a bundle that does
    /// not reach a root commit clones with "Repository lacks these prerequisite
    /// commits". Catching it at `verify` turns a confusing empty checkout into a
    /// refused submit.
    #[test]
    fn a_bundle_that_cannot_stand_alone_is_refused_at_submit() {
        let ws = workspace();
        // Not a bundle at all is the same class of failure and needs no fixture
        // repository to produce.
        let err = materialize(&bundle_source(b"not a bundle"), &ws, 1 << 20).unwrap_err();
        match &err {
            TriggerError::BadArchive(m) => {
                assert!(m.contains("not usable on its own"), "{m}")
            }
            TriggerError::NoGit => return, // no git here; nothing to assert
            other => panic!("{other:?}"),
        }
        assert_eq!(err.status(), 400);
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// Regression for a trap `git bundle verify` walks straight into: a file
    /// that is only the bundle header passes it — "is okay", "0 refs",
    /// "records a complete history" — and clones into an empty repository. The
    /// submitter would then be told no workflow files matched, which points at
    /// the wrong thing entirely.
    #[test]
    fn a_bundle_with_no_refs_is_refused_rather_than_cloning_empty() {
        let ws = workspace();
        let header_only = b"# v2 git bundle\n".to_vec();
        let err = match materialize(&bundle_source(&header_only), &ws, 1 << 20) {
            Err(TriggerError::NoGit) => return, // no git here; nothing to assert
            Err(e) => e,
            Ok(_) => panic!("a ref-less bundle must not be accepted"),
        };
        let message = err.to_string();
        assert!(message.contains("no refs"), "{message}");
        assert!(
            !ws.root.join(".git").exists() || std::fs::read_dir(&ws.root).unwrap().count() <= 1,
            "nothing usable should have been produced"
        );
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// Both formats are named in the payload, so an unknown one is a rejected
    /// value rather than a corrupt unpack.
    #[test]
    fn source_formats_round_trip_through_their_wire_names() {
        assert_eq!(SourceFormat::parse("tar.gz"), Some(SourceFormat::TarGz));
        assert_eq!(
            SourceFormat::parse("git-bundle"),
            Some(SourceFormat::GitBundle)
        );
        assert_eq!(SourceFormat::parse("zip"), None);
        assert_eq!(SourceFormat::TarGz.as_str(), "tar.gz");
        assert_eq!(SourceFormat::GitBundle.as_str(), "git-bundle");
    }

    #[test]
    fn a_non_targz_format_is_refused_by_name() {
        let ws = workspace();
        let s = SourceArchive {
            format: "zip".into(),
            content_base64: String::new(),
            bytes: None,
        };
        let err = materialize(&s, &ws, 1 << 20).unwrap_err();
        assert!(matches!(err, TriggerError::UnsupportedFormat(_)), "{err:?}");
        assert!(err.to_string().contains("tar.gz"), "{err}");
    }

    /// Re-submitting the same run must not leave a previous tree's files behind,
    /// or a deleted file would still be there for the fingerprint to hash.
    #[test]
    fn materializing_twice_replaces_the_tree_rather_than_merging() {
        let ws = workspace();
        materialize(&source(&tarball(&[("old.txt", b"1")])), &ws, 1 << 20).unwrap();
        assert!(ws.root.join("old.txt").exists());
        materialize(&source(&tarball(&[("new.txt", b"2")])), &ws, 1 << 20).unwrap();
        assert!(ws.root.join("new.txt").exists());
        assert!(
            !ws.root.join("old.txt").exists(),
            "a file removed on the branch must be gone from the workspace"
        );
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    // ---- what a submit changed -------------------------------------------

    /// A bundle of two commits, returning `(bytes, parent_sha, tempdir)`.
    ///
    /// Two is the minimum that has a diff at all, and the parent is what the
    /// client sends as `before` — so this is the ordinary push, which is the
    /// only shape that produces a real answer.
    fn bundle_with_parent(
        first: &[(&str, &str)],
        second: &[(&str, &str)],
        removed: &[&str],
    ) -> (Vec<u8>, String, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("ci-diff-{}", crate::vm::new_id()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let write = |entries: &[(&str, &str)]| {
            for (name, body) in entries {
                let path = repo.join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, body).unwrap();
            }
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        write(first);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first"]);
        let parent = git(&["rev-parse", "HEAD"]);

        write(second);
        for r in removed {
            std::fs::remove_file(repo.join(r)).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "second"]);

        let bundle = base.join("out.bundle");
        git(&["bundle", "create", &bundle.display().to_string(), "--all"]);
        (std::fs::read(&bundle).unwrap(), parent, base)
    }

    /// The ordinary push: the diff is read out of the bundle's own history, and
    /// it is what the path filters are matched against.
    #[test]
    fn a_bundle_yields_the_paths_its_commit_changed() {
        let (bytes, parent, base) = bundle_with_parent(
            &[
                ("packages/api/main.rs", "1"),
                ("packages/web/index.html", "1"),
                ("gone.txt", "1"),
            ],
            &[("packages/api/main.rs", "2"), ("packages/api/new.rs", "1")],
            &["gone.txt"],
        );
        let ws = workspace();
        if materialize(&bundle_source(&bytes), &ws, 1 << 22).is_err() {
            return; // no git here; nothing to assert
        }

        let changes = changed_paths(&ws, &parent);
        assert!(changes.is_known(), "{changes}");
        let mut got = changes.paths().to_vec();
        got.sort();
        assert_eq!(
            got,
            ["gone.txt", "packages/api/main.rs", "packages/api/new.rs"],
            "a deleted file is a change to the package that held it"
        );
        // Which is the whole point: the untouched package does not build.
        assert!(changes.matches_any(&["packages/api/**".to_string()]));
        assert!(!changes.matches_any(&["packages/web/**".to_string()]));

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// A file moved between packages must rebuild both, which is why rename
    /// detection is off: with it, git reports only the destination path and the
    /// package that lost the file would sit out the build that broke it.
    #[test]
    fn a_move_between_packages_counts_as_a_change_to_both() {
        let body = "fn main() {}\n".repeat(40);
        let (bytes, parent, base) = bundle_with_parent(
            &[("packages/api/moved.rs", &body)],
            &[("packages/web/moved.rs", &body)],
            &["packages/api/moved.rs"],
        );
        let ws = workspace();
        if materialize(&bundle_source(&bytes), &ws, 1 << 22).is_err() {
            return;
        }

        let changes = changed_paths(&ws, &parent);
        assert!(changes.is_known(), "{changes}");
        assert!(
            changes.matches_any(&["packages/api/**".to_string()]),
            "the package the file left must rebuild: {:?}",
            changes.paths()
        );
        assert!(changes.matches_any(&["packages/web/**".to_string()]));

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// Every way of having no answer, and the one thing they must all agree on:
    /// the result is `Unknown`, which builds — never an empty diff, which
    /// would skip.
    #[test]
    fn every_undiffable_submit_is_unknown_rather_than_empty() {
        // A tarball has no history at all.
        let ws = workspace();
        materialize(&source(&tarball(&[("a.txt", b"1")])), &ws, 1 << 20).unwrap();
        let c = changed_paths(&ws, "deadbeef");
        assert!(!c.is_known());
        assert!(c.reason().unwrap().contains("--archive"), "{c}");
        assert!(c.matches_any(&["anything/**".to_string()]));
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();

        let (bytes, parent, base) = bundle_with_parent(&[("a.txt", "1")], &[("a.txt", "2")], &[]);
        let ws = workspace();
        if materialize(&bundle_source(&bytes), &ws, 1 << 22).is_err() {
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        // A root commit: the client could not resolve a parent and sent "".
        for empty in ["", "   "] {
            let c = changed_paths(&ws, empty);
            assert!(!c.is_known(), "{empty:?}");
            assert!(c.reason().unwrap().contains("no parent"), "{c}");
            assert!(c.matches_any(&["anything/**".to_string()]));
        }

        // A `before` from a history this bundle is not part of — which is also
        // what `--dirty` used to produce before the `after` side was pinned to
        // the clone's own HEAD.
        let c = changed_paths(&ws, "0123456789012345678901234567890123456789");
        assert!(!c.is_known());
        assert!(
            c.reason().unwrap().contains("not in the submitted bundle"),
            "{c}"
        );
        assert!(c.matches_any(&["anything/**".to_string()]));

        // And the control: the same workspace with a real parent does answer.
        assert!(changed_paths(&ws, &parent).is_known());

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    #[test]
    fn workflows_are_found_by_pattern_and_returned_sorted() {
        let ws = workspace();
        let gz = tarball(&[
            (".ci/workflows/b.yml", b"name: b"),
            (".ci/workflows/a.yml", b"name: a"),
            (".ci/workflows/notes.md", b"not a workflow"),
            ("src/main.rs", b"fn main() {}"),
        ]);
        materialize(&source(&gz), &ws, 1 << 20).unwrap();

        let found = find_workflows(&ws.root, ".ci/workflows/*.yml").unwrap();
        let names: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, [".ci/workflows/a.yml", ".ci/workflows/b.yml"]);
        assert_eq!(found[0].1, "name: a");
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// A repository with no workflow directory has no workflows; that is not a
    /// failure to stat something.
    #[test]
    fn a_tree_with_no_workflow_directory_yields_nothing() {
        let ws = workspace();
        materialize(&source(&tarball(&[("README", b"hi")])), &ws, 1 << 20).unwrap();
        assert!(
            find_workflows(&ws.root, ".ci/workflows/*.yml")
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    /// The pattern arrives from a workflow object, which an operator wrote — but
    /// it still must not be able to read outside the submitted tree.
    #[test]
    fn a_workflow_pattern_cannot_escape_the_tree() {
        let ws = workspace();
        materialize(&source(&tarball(&[("README", b"hi")])), &ws, 1 << 20).unwrap();
        assert!(matches!(
            find_workflows(&ws.root, "../../etc/*"),
            Err(TriggerError::EscapingEntry(_))
        ));
        std::fs::remove_dir_all(ws.root.parent().unwrap()).ok();
    }

    #[test]
    fn patterns_match_the_shapes_people_write() {
        assert!(matches_pattern("build.yml", "*.yml"));
        assert!(matches_pattern("a.yaml", "*"));
        assert!(matches_pattern("build.yml", "build.*"));
        assert!(matches_pattern("build.yml", "build.yml"));
        assert!(!matches_pattern("build.yaml", "*.yml"));
        assert!(!matches_pattern("notes.md", "*.yml"));
        // A pattern must not match a shorter name by overlapping its own
        // prefix and suffix: `*.yml` must not match `.yml`'s own dot.
        assert!(!matches_pattern(".yml", "*x.yml"));
    }
    }

    #[test]
    fn a_ref_reduces_to_its_branch() {
        let mut r = SubmitRequest {
            repository: RepositoryRef::default(),
            r#ref: "refs/heads/feature/x".into(),
            before: String::new(),
            after: String::new(),
            dry_run: false,
            pusher: None,
            workflow_id: None,
            only: Vec::new(),
            rerun: None,
            source: SourceArchive {
                format: "tar.gz".into(),
                content_base64: String::new(),
                bytes: None,
            },
        };
        assert_eq!(r.branch(), "feature/x");
        r.r#ref = "main".into();
        assert_eq!(r.branch(), "main");
    }
}
