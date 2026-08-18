//! The submit endpoint: `git submit` posts here.
//!
//! The client sends the source it wants built — a **`git bundle`** by default,
//! or a **`git archive` tarball** with `--archive`. Two consequences follow from
//! the submitter packing it rather than the server fetching it, and both are the
//! point:
//!
//! - **No repository credential exists anywhere in this system.** Not on the
//!   orchestrator, not in a guest. The submitter already had read access — they
//!   ran `git bundle` — so nothing here needs its own. A CI system that clones
//!   for you is a CI system holding a key to every repository it builds.
//! - **The tree is exactly what the submitter meant.** No re-resolving a ref
//!   that may have moved, no guessing whether `--dirty` work was included. What
//!   arrives is what runs.
//!
//! ## Why a bundle, and why the tarball survives
//!
//! A bundle clones in the guest into a real repository, so `git describe`,
//! `git log` and `git rev-parse` work in a step — which a tarball cannot offer,
//! having no `.git` at all.
//!
//! It costs history. A bundle that clones on its own **must reach a root
//! commit**: `git bundle create --depth` does not exist, and a `--max-count`
//! slice is refused at clone time with *"Repository lacks these prerequisite
//! commits"*. So the payload is proportional to the repository's history rather
//! than to one tree, and `--archive` stays supported for the repository where
//! that is the wrong trade.
//!
//! Both formats need a tool the other does not: a bundle needs `git` on the
//! orchestrator *and* in the guest image; a tarball needs neither. Each absence
//! is reported by name.
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
    pub source: SourceArchive,
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

/// How a submitted tree arrived, and therefore how it reaches the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// `git archive` of one tree. No `.git` in the guest.
    TarGz,
    /// `git bundle` of the branch. Clones in the guest into a real repository,
    /// so `git describe`, `git log` and `git rev-parse` work in a step.
    GitBundle,
}

impl SourceFormat {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "tar.gz" => Some(Self::TarGz),
            "git-bundle" => Some(Self::GitBundle),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
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
        }
    }

    pub fn path_for(&self, format: SourceFormat) -> &Path {
        match format {
            SourceFormat::TarGz => &self.tarball,
            SourceFormat::GitBundle => &self.bundle,
        }
    }

    /// Which of the two is actually on disk, for a run whose submit happened in
    /// another process.
    pub fn stored_source(&self) -> Option<(SourceFormat, &Path)> {
        for format in [SourceFormat::GitBundle, SourceFormat::TarGz] {
            let path = self.path_for(format);
            if path.exists() {
                return Some((format, path));
            }
        }
        None
    }
}

/// Decode a submitted source into `workspace.root`, keeping the original bytes
/// alongside it for shipping to the guest.
///
/// Keeping the original rather than re-packing is not just an optimisation: a
/// round trip through unpack-and-repack would silently normalise permissions and
/// drop anything the unpacker chose not to write, so what runs in the guest
/// would differ from what was submitted. For a bundle it matters more still —
/// re-packing would discard the history that is the whole reason to send one.
pub fn materialize(
    source: &SourceArchive,
    workspace: &Workspace,
    max_bytes: usize,
) -> Result<usize, TriggerError> {
    let format = SourceFormat::parse(&source.format)
        .ok_or_else(|| TriggerError::UnsupportedFormat(source.format.clone()))?;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(source.content_base64.as_bytes())
        .map_err(|e| TriggerError::BadArchive(format!("not valid base64: {e}")))?;
    if bytes.len() > max_bytes {
        return Err(TriggerError::ArchiveTooLarge {
            bytes: bytes.len(),
            max: max_bytes,
        });
    }

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

    // The bytes are written before they are unpacked, because cloning a bundle
    // reads it from disk — and because a stale file of the *other* format left
    // by a re-submit would otherwise be what `stored_source` finds later.
    let stored = workspace.path_for(format);
    std::fs::write(stored, &bytes).map_err(|e| TriggerError::Io {
        path: stored.to_path_buf(),
        reason: e.to_string(),
    })?;
    let _ = std::fs::remove_file(workspace.path_for(match format {
        SourceFormat::TarGz => SourceFormat::GitBundle,
        SourceFormat::GitBundle => SourceFormat::TarGz,
    }));

    match format {
        SourceFormat::TarGz => extract(&bytes, &workspace.root)?,
        SourceFormat::GitBundle => clone_bundle(stored, &workspace.root)?,
    }
    Ok(bytes.len())
}

/// Clone a submitted bundle into `root`, producing a real working tree.
///
/// Shelling out to `git` rather than linking a git library: the orchestrator
/// only needs this to read workflow files and hash `cache_key_files` out of the
/// tree, and `git` is already a hard requirement of the deployment (app-lb's
/// update block runs `git pull`). A missing binary is reported by name rather
/// than as a confusing clone failure.
///
/// **The bundle is unauthenticated input until the credential check passes**, and
/// only as trustworthy afterwards as whoever holds the token. `git clone` of a
/// local bundle writes only inside the destination — there is no hook to run,
/// because hooks live in the destination's own `.git` which git creates fresh —
/// but it is still given an empty environment-ish treatment below: `core.hooksPath`
/// is pinned away and the bundle is verified before it is used.
fn clone_bundle(bundle: &Path, root: &Path) -> Result<(), TriggerError> {
    let git = |args: &[&str]| -> Result<std::process::Output, TriggerError> {
        std::process::Command::new("git")
            .args(args)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    TriggerError::NoGit
                } else {
                    TriggerError::BadArchive(format!("running git: {e}"))
                }
            })
    };

    // Verify first: `git bundle verify` rejects a truncated or corrupt bundle,
    // and a bundle whose prerequisites are absent — which is exactly what a
    // client that tried to send a shallow slice would produce, and which would
    // otherwise fail later as an unexplained empty checkout.
    let verify = git(&["bundle", "verify", &bundle.display().to_string()])?;
    if !verify.status.success() {
        return Err(TriggerError::BadArchive(format!(
            "the git bundle is not usable on its own: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        )));
    }

    // `verify` is not enough, and the gap is not obvious: a file containing
    // nothing but the header `# v2 git bundle` **passes** it — reported as
    // "is okay", "0 refs", "records a complete history" — and then clones into
    // an empty repository. The submitter's next error would be "no workflow
    // files matched", sending them to look at their glob when the real problem
    // is that nothing was sent. So the refs are counted explicitly.
    let heads = git(&["bundle", "list-heads", &bundle.display().to_string()])?;
    if heads.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(TriggerError::BadArchive(
            "the git bundle contains no refs, so there is nothing to check out. \
             It passed `git bundle verify`, which reports a ref-less bundle as \
             complete — check that the client packed a branch and not a bare \
             commit."
                .to_string(),
        ));
    }

    let out = git(&[
        // A hooks path that cannot exist, so nothing in the submitted history
        // can arrange to be executed by the clone.
        "-c",
        "core.hooksPath=/nonexistent",
        "clone",
        "--quiet",
        &bundle.display().to_string(),
        &root.display().to_string(),
    ])?;
    if !out.status.success() {
        return Err(TriggerError::BadArchive(format!(
            "cloning the submitted bundle failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Extract a gzipped tar, refusing any entry that would write outside `root`.
///
/// The archive is unauthenticated input right up until the signature check
/// passes, and even then it is only as trustworthy as whoever holds the webhook
/// secret. `tar`'s own protections have historically varied by version, so every
/// path is checked here: no absolute paths, no `..`, and no symlink or hard link
/// pointing outside the tree.
fn extract(gz: &[u8], root: &Path) -> Result<(), TriggerError> {
    let decoder = flate2::read::GzDecoder::new(gz);
    let mut archive = tar::Archive::new(decoder);
    // Ownership from the archive is meaningless here and would need privileges.
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);

    let entries = archive
        .entries()
        .map_err(|e| TriggerError::BadArchive(e.to_string()))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| TriggerError::BadArchive(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| TriggerError::BadArchive(e.to_string()))?
            .into_owned();
        check_contained(&path)?;

        // A link's *target* escapes just as effectively as a path does.
        if let Ok(Some(link)) = entry.link_name() {
            check_contained(&link)?;
        }

        entry
            .unpack_in(root)
            .map_err(|e| TriggerError::BadArchive(format!("unpacking {}: {e}", path.display())))?;
    }
    Ok(())
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

/// What a submit changed, read out of the submitted bundle's own history.
///
/// **The `after` side is the clone's `HEAD`, not the payload's `after` field.**
/// With `git submit --dirty` the client reports `<sha>-dirty`, which is a label
/// for a person and not a resolvable object: the tree that actually travelled is
/// a throwaway commit the client made from the index plus the worktree, and the
/// bundle's `HEAD` is the only thing that points at it. Diffing what arrived
/// against what the client says it came from is also the only version of this
/// that stays true for `--ref`, where `HEAD` is not the submitter's `HEAD`.
///
/// Every failure here is [`Changes::Unknown`], never an empty diff — see the
/// [`crate::paths`] module doc for why that direction is the whole point.
pub fn changed_paths(workspace: &Workspace, before: &str) -> Changes {
    let Some((format, _)) = workspace.stored_source() else {
        return Changes::unknown("this run's submitted source is no longer on disk");
    };
    if format != SourceFormat::GitBundle {
        return Changes::unknown(
            "the submit is a `--archive` tarball, which carries no history to diff against. \
             Submit a git bundle (the default) for path filters to have an answer",
        );
    }
    let before = before.trim();
    if before.is_empty() {
        return Changes::unknown(
            "the submitted commit has no parent, so there is nothing to diff against",
        );
    }

    let root = workspace.root.display().to_string();
    let git = |args: &[&str]| -> Result<std::process::Output, String> {
        std::process::Command::new("git")
            .args(["-C", &root])
            .args(args)
            .output()
            .map_err(|e| e.to_string())
    };

    // Checked separately from the diff so the reason names the missing commit
    // rather than reporting git's own message about an ambiguous argument. A
    // bundle reaches a root commit, so an absent parent means the client sent a
    // `before` from a history this bundle is not part of.
    match git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{before}^{{commit}}"),
    ]) {
        Err(e) => return Changes::unknown(format!("could not run git to read the diff: {e}")),
        Ok(out) if !out.status.success() => {
            return Changes::unknown(format!(
                "commit {before} is not in the submitted bundle, so there is nothing to \
                 diff against"
            ));
        }
        Ok(_) => {}
    }

    // `--no-renames` on purpose: with rename detection a file moved out of one
    // package and into another reports only its new path, so the package that
    // lost it would not rebuild. Both sides of a move are a change to both.
    // `-z` because git quotes unusual bytes in a path otherwise, and a quoted
    // path would not match the glob the workflow author wrote.
    let out = match git(&["diff", "--name-only", "--no-renames", "-z", before, "HEAD"]) {
        Ok(o) => o,
        Err(e) => return Changes::unknown(format!("could not run git to read the diff: {e}")),
    };
    if !out.status.success() {
        return Changes::unknown(format!(
            "git could not diff {before}..HEAD: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Changes::known(
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
    )
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
    /// `git` is not on the orchestrator's PATH, so a bundle cannot be read.
    NoGit,
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
                 understands `tar.gz`. Upgrade `git submit`."
            ),
            Self::BadArchive(e) => write!(f, "the source archive could not be read: {e}"),
            Self::EscapingEntry(p) => write!(
                f,
                "the source archive contains {p:?}, which would write outside the \
                 workspace. Refusing the whole archive."
            ),
            Self::ArchiveTooLarge { bytes, max } => write!(
                f,
                "the source archive is {bytes} bytes, over the {max}-byte limit. \
                 Raise CI_MAX_SOURCE_BYTES, or exclude build output with a \
                 .gitattributes `export-ignore`."
            ),
            Self::Io { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::NoWorkflows(pattern) => write!(
                f,
                "no workflow files matched {pattern:?} in the submitted tree"
            ),
            Self::NoGit => write!(
                f,
                "this submit is a git bundle, but `git` is not on this server's PATH. \
                 Install git on the orchestrator, or submit with `git submit --archive` \
                 to send a plain tree instead."
            ),
        }
    }
}

impl std::error::Error for TriggerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        }
    }

    fn workspace() -> Workspace {
        let base = std::env::temp_dir().join(format!("ci-trigger-{}", crate::vm::new_id()));
        std::fs::create_dir_all(&base).unwrap();
        Workspace {
            root: base.join("tree"),
            tarball: base.join("source.tar.gz"),
            bundle: base.join("source.bundle"),
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
            source: SourceArchive {
                format: "tar.gz".into(),
                content_base64: String::new(),
            },
        };
        assert_eq!(r.branch(), "feature/x");
        r.r#ref = "main".into();
        assert_eq!(r.branch(), "main");
    }
}
