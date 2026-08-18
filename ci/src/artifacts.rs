//! Where build artifacts go: disk, S3, or the `artifacts` store.
//!
//! ## The orchestrator does every upload
//!
//! A runner never talks to an artifact store. The orchestrator pulls the file
//! out of the guest and pushes it onward. For the `artifacts` sink that is not a
//! preference — the store is per-host, per-user and single-replica (its own
//! `examples/artifacts.json` pins it to one, because "each VM has its own disk,
//! so N replicas are N independent stores"). A fleet of runners each pushing to
//! their own local store would produce N stores that disagree. One central
//! `art serve`, one writer.
//!
//! ## Three constraints of the `artifacts` store shape the design
//!
//! - **Tag names cannot contain `/`** — the charset is `[A-Za-z0-9_.-]`, max 64
//!   characters. So a tag is a flattened `ci-<workflow>-<run>-<name>`, and the
//!   real coordinates go in the manifest's `annotations`.
//! - **Annotations must stay content-only.** The manifest is addressed by its
//!   own hash and carries no timestamp field on purpose, so an unchanged
//!   re-import dedupes. Putting a build time in an annotation would change the
//!   digest and destroy that. Mutable build metadata lives in `ci_artifact`,
//!   keyed by digest.
//! - **There is no `GET /tags/{name}`** despite the README; a tag resolves
//!   through `GET /manifests/{tag}`.
//!
//! The push sequence is lifted from `app-lb/serverctl/src/artifact.rs`: hash,
//! `HEAD /blobs/{digest}` (a 404 is an answer, not an error), `PUT
//! /blobs/{digest}` with an explicit `Content-Length` so the store's free-space
//! guard can refuse before the first byte, `PUT /manifests`, `PUT /tags/{name}`.

use crate::config::{ArtifactSinkKind, ArtifactsConfig, Config, S3Config};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;

/// What a stored artifact is, once stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    pub sink: &'static str,
    /// sha256, for the content-addressed sink; `None` for disk and S3.
    pub digest: Option<String>,
    pub size_bytes: u64,
    /// How to get it back — a path, an `s3://` URL, or a tag.
    pub uri: String,
}

/// Which run and job an artifact belongs to.
#[derive(Debug, Clone)]
pub struct ArtifactRef {
    pub run_id: String,
    pub job_key: String,
    pub workflow_id: String,
    pub name: String,
}

#[async_trait]
pub trait ArtifactSink: Send + Sync {
    async fn put(&self, r: &ArtifactRef, bytes: Vec<u8>) -> Result<StoredArtifact, ArtifactError>;
    fn kind(&self) -> &'static str;
}

/// Build the configured sink.
pub fn sink_for(config: &Config) -> Result<Box<dyn ArtifactSink>, ArtifactError> {
    match config.artifact_sink {
        ArtifactSinkKind::Disk => Ok(Box::new(DiskSink {
            root: config.artifact_dir.clone(),
        })),
        ArtifactSinkKind::S3 => {
            let s3 = config
                .s3
                .clone()
                .ok_or_else(|| ArtifactError::Misconfigured("CI_S3_BUCKET is not set".into()))?;
            Ok(Box::new(S3Sink { config: s3 }))
        }
        ArtifactSinkKind::Artifacts => {
            let a = config
                .artifacts
                .clone()
                .ok_or_else(|| ArtifactError::Misconfigured("CI_ARTIFACT_URL is not set".into()))?;
            Ok(Box::new(ArtifactsSink::new(a)))
        }
    }
}

// ---- disk ---------------------------------------------------------------

pub struct DiskSink {
    root: PathBuf,
}

#[async_trait]
impl ArtifactSink for DiskSink {
    fn kind(&self) -> &'static str {
        "disk"
    }

    async fn put(&self, r: &ArtifactRef, bytes: Vec<u8>) -> Result<StoredArtifact, ArtifactError> {
        let path = self
            .root
            .join(safe(&r.run_id))
            .join(safe(&r.job_key))
            .join(safe(&r.name));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ArtifactError::Io(format!("{}: {e}", parent.display())))?;
        }
        let size = bytes.len() as u64;
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| ArtifactError::Io(format!("{}: {e}", path.display())))?;
        Ok(StoredArtifact {
            sink: "disk",
            digest: None,
            size_bytes: size,
            uri: path.to_string_lossy().into_owned(),
        })
    }
}

// ---- s3 -----------------------------------------------------------------

pub struct S3Sink {
    config: S3Config,
}

#[async_trait]
impl ArtifactSink for S3Sink {
    fn kind(&self) -> &'static str {
        "s3"
    }

    async fn put(&self, r: &ArtifactRef, _bytes: Vec<u8>) -> Result<StoredArtifact, ArtifactError> {
        // Deliberately not implemented rather than silently succeeding: an
        // artifact that reports stored and is not there is worse than a build
        // that fails saying so. Selecting `CI_ARTIFACT_SINK=s3` is checked at
        // startup, so this is reachable only by having asked for it.
        Err(ArtifactError::NotImplemented {
            sink: "s3",
            detail: format!(
                "would upload {} to s3://{}/{}",
                r.name,
                self.config.bucket,
                self.key_for(r)
            ),
        })
    }
}

impl S3Sink {
    fn key_for(&self, r: &ArtifactRef) -> String {
        format!(
            "{}/{}/{}/{}",
            self.config.prefix.trim_matches('/'),
            safe(&r.run_id),
            safe(&r.job_key),
            safe(&r.name)
        )
    }
}

// ---- the artifacts store ------------------------------------------------

pub struct ArtifactsSink {
    http: reqwest::Client,
    config: ArtifactsConfig,
}

impl ArtifactsSink {
    pub fn new(config: ArtifactsConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
        }
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.config.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }
}

#[async_trait]
impl ArtifactSink for ArtifactsSink {
    fn kind(&self) -> &'static str {
        "artifacts"
    }

    async fn put(&self, r: &ArtifactRef, bytes: Vec<u8>) -> Result<StoredArtifact, ArtifactError> {
        let base = &self.config.url;
        let size = bytes.len() as u64;
        let digest = hex::encode(Sha256::digest(&bytes));

        // A 404 here is the answer "not stored yet", not a failure — the same
        // distinction serverctl's client makes.
        let head = self
            .auth(self.http.head(format!("{base}/blobs/{digest}")))
            .send()
            .await
            .map_err(|e| ArtifactError::Transport(e.to_string()))?;

        if !head.status().is_success() {
            let put = self
                .auth(self.http.put(format!("{base}/blobs/{digest}")))
                // Explicit, so the store's free-space guard can refuse before
                // the first byte crosses rather than after the last.
                .header(reqwest::header::CONTENT_LENGTH, size)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(bytes)
                .send()
                .await
                .map_err(|e| ArtifactError::Transport(e.to_string()))?;
            check(put, "uploading a blob").await?;
        }

        let manifest = manifest_for(r, &digest, size);
        let put_manifest = self
            .auth(self.http.put(format!("{base}/manifests")))
            .json(&manifest)
            .send()
            .await
            .map_err(|e| ArtifactError::Transport(e.to_string()))?;
        let manifest_digest: serde_json::Value = check(put_manifest, "storing a manifest")
            .await?
            .json()
            .await
            .map_err(|e| ArtifactError::Transport(e.to_string()))?;
        let manifest_digest = manifest_digest
            .get("digest")
            .and_then(|v| v.as_str())
            .unwrap_or(&digest)
            .to_string();

        let tag = tag_for(r);
        let put_tag = self
            .auth(self.http.put(format!("{base}/tags/{tag}")))
            // The body is the bare digest as text/plain, not JSON.
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(manifest_digest)
            .send()
            .await
            .map_err(|e| ArtifactError::Transport(e.to_string()))?;
        check(put_tag, "setting a tag").await?;

        Ok(StoredArtifact {
            sink: "artifacts",
            digest: Some(digest),
            size_bytes: size,
            uri: tag,
        })
    }
}

async fn check(
    response: reqwest::Response,
    what: &str,
) -> Result<reqwest::Response, ArtifactError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    let slug = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("no detail");
    Err(ArtifactError::Store {
        what: what.to_string(),
        status: status.as_u16(),
        slug: slug.to_string(),
        message: message.to_string(),
    })
}

/// The manifest for one artifact.
///
/// Content-only: no timestamps, no run duration, nothing that varies between two
/// builds of identical bytes. The manifest is addressed by its own hash, so an
/// unchanged re-upload has to produce the same digest or dedup stops working.
/// The run id *is* included, because two runs producing the same bytes are still
/// two artifacts a user needs to tell apart — and the tag already encodes it.
fn manifest_for(r: &ArtifactRef, digest: &str, size: u64) -> serde_json::Value {
    serde_json::json!({
        "schema": 1,
        "kind": "generic",
        "entries": [{ "name": r.name, "digest": digest, "size": size }],
        "annotations": {
            "ci.workflow": r.workflow_id,
            "ci.run": r.run_id,
            "ci.job": r.job_key,
            "ci.name": r.name,
        }
    })
}

/// A tag the store will accept: `[A-Za-z0-9_.-]`, at most 64 characters, no
/// leading `-` or `.`.
///
/// Everything meaningful is also in the manifest annotations, so truncation
/// loses addressability, never information.
pub fn tag_for(r: &ArtifactRef) -> String {
    let raw = format!(
        "ci-{}-{}-{}-{}",
        safe(&r.workflow_id),
        safe(&r.run_id),
        safe(&r.job_key),
        safe(&r.name)
    );
    let mut tag: String = raw.chars().take(64).collect();
    while tag.starts_with('-') || tag.starts_with('.') {
        tag.remove(0);
    }
    if tag.is_empty() {
        tag.push_str("ci-artifact");
    }
    tag
}

/// Reduce a component to the tag/path charset.
fn safe(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // `..` as a whole component would traverse on the disk sink.
    if out.chars().all(|c| c == '.') {
        return "-".to_string();
    }
    out
}

#[derive(Debug)]
pub enum ArtifactError {
    Misconfigured(String),
    Io(String),
    Transport(String),
    Store {
        what: String,
        status: u16,
        slug: String,
        message: String,
    },
    NotImplemented {
        sink: &'static str,
        detail: String,
    },
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Misconfigured(e) => write!(f, "the artifact sink is misconfigured: {e}"),
            Self::Io(e) => write!(f, "writing an artifact: {e}"),
            Self::Transport(e) => write!(f, "could not reach the artifact store: {e}"),
            Self::Store {
                what,
                status,
                slug,
                message,
            } => {
                write!(f, "{what} failed ({status}")?;
                if !slug.is_empty() {
                    write!(f, " {slug}")?;
                }
                write!(f, "): {message}")?;
                // The store's own slugs, translated into what to do about them.
                match slug.as_str() {
                    "no_space" => write!(f, ". The store is full."),
                    "unauthorized" => write!(f, ". Check CI_ARTIFACT_TOKEN."),
                    "read_only" => write!(f, ". The store has ART_READ_ONLY set."),
                    "invalid_tag" => write!(
                        f,
                        ". A tag may only contain [A-Za-z0-9_.-] and must be at most \
                         64 characters."
                    ),
                    _ => Ok(()),
                }
            }
            Self::NotImplemented { sink, detail } => write!(
                f,
                "the {sink} artifact sink is not implemented yet ({detail}). Set \
                 CI_ARTIFACT_SINK=disk or =artifacts."
            ),
        }
    }
}

impl std::error::Error for ArtifactError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use std::path::Path;

    fn aref() -> ArtifactRef {
        ArtifactRef {
            run_id: "019fca648a6e-00000000".into(),
            job_key: "build-x86_64".into(),
            workflow_id: "myapp".into(),
            name: "binary.tar.gz".into(),
        }
    }

    /// The store's tag charset excludes `/`, which every natural artifact
    /// coordinate contains.
    #[test]
    fn a_tag_fits_the_stores_charset_and_length() {
        let tag = tag_for(&aref());
        assert!(
            tag.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "{tag}"
        );
        assert!(tag.len() <= 64, "{} chars: {tag}", tag.len());
        assert!(!tag.starts_with('-') && !tag.starts_with('.'));
        assert!(tag.contains("myapp"), "{tag}");
    }

    #[test]
    fn a_slash_in_any_component_never_reaches_the_tag() {
        let mut r = aref();
        r.name = "dist/app.tar.gz".into();
        r.workflow_id = "org/app".into();
        let tag = tag_for(&r);
        assert!(!tag.contains('/'), "{tag}");
    }

    /// Truncation must still produce something the store accepts.
    #[test]
    fn a_very_long_name_is_truncated_to_a_legal_tag() {
        let mut r = aref();
        r.name = "x".repeat(300);
        let tag = tag_for(&r);
        assert_eq!(tag.len(), 64);
        assert!(!tag.starts_with('-'));
    }

    #[test]
    fn a_pathological_component_still_yields_a_usable_tag() {
        let r = ArtifactRef {
            run_id: "..".into(),
            job_key: "..".into(),
            workflow_id: "..".into(),
            name: "..".into(),
        };
        let tag = tag_for(&r);
        assert!(!tag.is_empty());
        assert!(!tag.starts_with('.'), "{tag}");
    }

    /// The manifest is addressed by its own hash, so two uploads of identical
    /// bytes from the same run must produce byte-identical manifests — that is
    /// what makes the store dedupe.
    #[test]
    fn a_manifest_is_content_only_and_therefore_stable() {
        let a = manifest_for(&aref(), "deadbeef", 42);
        let b = manifest_for(&aref(), "deadbeef", 42);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        let text = serde_json::to_string(&a).unwrap();
        for forbidden in ["createdAt", "timestamp", "builtAt", "duration"] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} would break dedup: {text}"
            );
        }
        assert_eq!(a["entries"][0]["digest"], "deadbeef");
        assert_eq!(a["annotations"]["ci.run"], "019fca648a6e-00000000");
    }

    #[tokio::test]
    async fn the_disk_sink_writes_under_run_and_job() {
        let root = std::env::temp_dir().join(format!("ci-art-{}", crate::vm::new_id()));
        let sink = DiskSink { root: root.clone() };
        let stored = sink.put(&aref(), b"payload".to_vec()).await.unwrap();
        assert_eq!(stored.sink, "disk");
        assert_eq!(stored.size_bytes, 7);
        assert_eq!(std::fs::read(&stored.uri).unwrap(), b"payload");
        assert!(stored.uri.contains("build-x86_64"), "{}", stored.uri);
        std::fs::remove_dir_all(&root).ok();
    }

    /// An artifact name arrives from a workflow file; one `..` would write
    /// outside the artifact directory.
    #[tokio::test]
    async fn the_disk_sink_cannot_be_escaped_by_a_hostile_name() {
        let root = std::env::temp_dir().join(format!("ci-art-{}", crate::vm::new_id()));
        let mut r = aref();
        r.name = "../../escaped".into();
        let stored = DiskSink { root: root.clone() }
            .put(&r, b"x".to_vec())
            .await
            .unwrap();
        assert!(
            Path::new(&stored.uri).starts_with(&root),
            "escaped to {}",
            stored.uri
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Reporting an artifact as stored when it is not is worse than failing.
    #[tokio::test]
    async fn the_s3_sink_fails_loudly_rather_than_pretending() {
        let sink = S3Sink {
            config: S3Config {
                bucket: "bkt".into(),
                prefix: "ci".into(),
                region: None,
                endpoint: None,
            },
        };
        let err = sink.put(&aref(), b"x".to_vec()).await.unwrap_err();
        assert!(matches!(err, ArtifactError::NotImplemented { .. }));
        assert!(err.to_string().contains("CI_ARTIFACT_SINK=disk"), "{err}");
    }

    #[test]
    fn an_s3_key_is_stable_and_slash_separated() {
        let sink = S3Sink {
            config: S3Config {
                bucket: "bkt".into(),
                prefix: "/ci/".into(),
                region: None,
                endpoint: None,
            },
        };
        assert_eq!(
            sink.key_for(&aref()),
            "ci/019fca648a6e-00000000/build-x86_64/binary.tar.gz"
        );
    }

    /// The store's slugs are machine-readable; the error turns them into what to
    /// do about them.
    #[test]
    fn store_errors_translate_the_slug_into_an_action() {
        let e = ArtifactError::Store {
            what: "uploading a blob".into(),
            status: 507,
            slug: "no_space".into(),
            message: "insufficient storage".into(),
        };
        assert!(e.to_string().contains("The store is full"), "{e}");

        let e = ArtifactError::Store {
            what: "uploading a blob".into(),
            status: 401,
            slug: "unauthorized".into(),
            message: "nope".into(),
        };
        assert!(e.to_string().contains("CI_ARTIFACT_TOKEN"), "{e}");
    }
}
