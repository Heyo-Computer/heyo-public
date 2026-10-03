//! The object store every byte of a repo lives in: AWS S3 in production, a
//! directory for development and tests.
//!
//! Only a handful of operations are needed, and conditional writes are the
//! important one. `state.json` (see [`crate::repos`]) is replaced with
//! `If-Match: <etag>`, which is what lets two regional instances push to the
//! same repo without a lock service: whichever loses the race gets a
//! [`Put::PreconditionFailed`] and the client is told to retry.
//!
//! The directory backend keeps the same contract (content-hash ETags, and an
//! exclusive file lock around conditional writes) because the git hook runs
//! as a separate process. An in-memory fake could not be shared with it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use md5::{Digest, Md5};

use crate::sigv4;

#[derive(Debug)]
pub struct StoreError {
    pub status: Option<u16>,
    pub message: String,
}

impl StoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "object store answered {s}: {}", self.message),
            None => write!(f, "object store: {}", self.message),
        }
    }
}

impl std::error::Error for StoreError {}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct Object {
    pub bytes: Vec<u8>,
    pub etag: String,
}

/// The condition a write is made under.
#[derive(Debug, Clone)]
pub enum Cond {
    None,
    /// Only if nothing is there yet.
    Absent,
    /// Only if what is there still has this ETag.
    Matches(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    Written(String),
    PreconditionFailed,
}

#[derive(Clone)]
pub enum Store {
    S3(S3),
    Fs(FsStore),
}

impl Store {
    /// Create `bucket` if it does not exist, locked down. Idempotent: a bucket
    /// this credential already owns is success.
    pub async fn ensure_bucket(&self, bucket: &str) -> Result<()> {
        match self {
            Store::S3(s) => s.ensure_bucket(bucket).await,
            Store::Fs(f) => f.ensure_bucket(bucket).await,
        }
    }

    pub async fn get(&self, bucket: &str, key: &str) -> Result<Option<Object>> {
        match self {
            Store::S3(s) => s.get(bucket, key).await,
            Store::Fs(f) => f.get(bucket, key).await,
        }
    }

    pub async fn put(&self, bucket: &str, key: &str, body: Vec<u8>, cond: Cond) -> Result<Put> {
        match self {
            Store::S3(s) => s.put(bucket, key, body, cond).await,
            Store::Fs(f) => f.put(bucket, key, body, cond).await,
        }
    }

    pub async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        match self {
            Store::S3(s) => s.delete(bucket, key).await,
            Store::Fs(f) => f.delete(bucket, key).await,
        }
    }

    /// Every key under `prefix`.
    pub async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        match self {
            Store::S3(s) => s.list(bucket, prefix).await,
            Store::Fs(f) => f.list(bucket, prefix).await,
        }
    }

    pub async fn delete_prefix(&self, bucket: &str, prefix: &str) -> Result<usize> {
        let keys = self.list(bucket, prefix).await?;
        for k in &keys {
            self.delete(bucket, k).await?;
        }
        Ok(keys.len())
    }
}

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct S3 {
    /// `None` is AWS itself (virtual-hosted addressing and the bucket hardening
    /// calls); `Some` is an S3-compatible endpoint, addressed path-style.
    pub endpoint: Option<String>,
    /// Apply the public-access block and default encryption on ensure. On by
    /// default for AWS, off for other endpoints (`REMOTE_S3_HARDEN`).
    pub harden: bool,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    http: reqwest::Client,
}

impl fmt::Debug for S3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

impl S3 {
    pub fn new(
        endpoint: Option<String>,
        region: String,
        access_key: String,
        secret_key: String,
    ) -> Self {
        Self {
            harden: endpoint.is_none(),
            endpoint: endpoint.map(|e| e.trim_end_matches('/').to_string()),
            region,
            access_key,
            secret_key,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
        }
    }

    /// `(scheme, host, canonical path)` for an object, or for the bucket
    /// itself when `key` is empty.
    fn address(&self, bucket: &str, key: &str) -> (String, String, String) {
        let key = sigv4::encode_uri_path(key);
        match &self.endpoint {
            Some(ep) => {
                let (scheme, host) = sigv4::split_scheme_host(ep);
                let path = if key.is_empty() {
                    format!("/{bucket}")
                } else {
                    format!("/{bucket}/{key}")
                };
                (scheme.into(), host.into(), path)
            }
            None => (
                "https".into(),
                format!("{bucket}.s3.{}.amazonaws.com", self.region),
                format!("/{key}"),
            ),
        }
    }

    async fn send(
        &self,
        method: &str,
        bucket: &str,
        key: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<reqwest::Response> {
        let (scheme, host, path) = self.address(bucket, key);
        let payload_hash = sigv4::sha256_hex(&body);
        let signed = sigv4::sign(sigv4::SignParams {
            method,
            host: &host,
            canonical_uri: &path,
            query,
            headers,
            payload_hash: &payload_hash,
            region: &self.region,
            access_key: &self.access_key,
            secret_key: &self.secret_key,
            unix_secs: sigv4::now_unix(),
        });
        let qs = sigv4::canonical_query(query);
        let url = if qs.is_empty() {
            format!("{scheme}://{host}{path}")
        } else {
            format!("{scheme}://{host}{path}?{qs}")
        };
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| StoreError::new(e.to_string()))?;
        let mut req = self.http.request(method, &url);
        for (k, v) in signed {
            req = req.header(k, v);
        }
        req.body(body)
            .send()
            .await
            .map_err(|e| StoreError::new(format!("{url}: {e}")))
    }

    async fn fail(resp: reqwest::Response) -> StoreError {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        StoreError {
            status: Some(status),
            message: xml_tag(&text, "Code")
                .map(|c| format!("{c}: {}", xml_tag(&text, "Message").unwrap_or_default()))
                .unwrap_or(text),
        }
    }

    async fn ensure_bucket(&self, bucket: &str) -> Result<()> {
        // Outside us-east-1 AWS wants the region spelled out, and inside it
        // AWS rejects the spelling. An S3-compatible endpoint wants neither.
        let body = if self.endpoint.is_none() && self.region != "us-east-1" {
            format!(
                "<CreateBucketConfiguration xmlns=\"{XMLNS}\"><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                self.region
            )
            .into_bytes()
        } else {
            Vec::new()
        };
        let resp = self.send("PUT", bucket, "", &[], &[], body).await?;
        if !resp.status().is_success() {
            let err = Self::fail(resp).await;
            if !err.message.starts_with("BucketAlreadyOwnedByYou") {
                if err.message.starts_with("BucketAlreadyExists") {
                    return Err(StoreError {
                        message: format!(
                            "bucket {bucket} belongs to a different AWS account; \
                             change REMOTE_BUCKET_PREFIX ({})",
                            err.message
                        ),
                        ..err
                    });
                }
                return Err(err);
            }
        }
        if !self.harden {
            return Ok(());
        }
        // Reapplied on every ensure, so a bucket created before hardening was
        // added, or loosened by hand, is put back.
        let block = format!(
            "<PublicAccessBlockConfiguration xmlns=\"{XMLNS}\">\
             <BlockPublicAcls>true</BlockPublicAcls><IgnorePublicAcls>true</IgnorePublicAcls>\
             <BlockPublicPolicy>true</BlockPublicPolicy><RestrictPublicBuckets>true</RestrictPublicBuckets>\
             </PublicAccessBlockConfiguration>"
        );
        self.bucket_config(bucket, "publicAccessBlock", block)
            .await?;
        let sse = format!(
            "<ServerSideEncryptionConfiguration xmlns=\"{XMLNS}\"><Rule>\
             <ApplyServerSideEncryptionByDefault><SSEAlgorithm>AES256</SSEAlgorithm></ApplyServerSideEncryptionByDefault>\
             </Rule></ServerSideEncryptionConfiguration>"
        );
        self.bucket_config(bucket, "encryption", sse).await
    }

    async fn bucket_config(&self, bucket: &str, sub: &str, body: String) -> Result<()> {
        let md5 = base64::engine::general_purpose::STANDARD.encode(Md5::digest(body.as_bytes()));
        let resp = self
            .send(
                "PUT",
                bucket,
                "",
                &[(sub.into(), String::new())],
                &[("content-md5".into(), md5)],
                body.into_bytes(),
            )
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(Self::fail(resp).await)
        }
    }

    async fn get(&self, bucket: &str, key: &str) -> Result<Option<Object>> {
        let resp = self.send("GET", bucket, key, &[], &[], Vec::new()).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Self::fail(resp).await);
        }
        let etag = etag_of(&resp);
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| StoreError::new(e.to_string()))?
            .to_vec();
        Ok(Some(Object { bytes, etag }))
    }

    async fn put(&self, bucket: &str, key: &str, body: Vec<u8>, cond: Cond) -> Result<Put> {
        let headers: Vec<(String, String)> = match cond {
            Cond::None => vec![],
            Cond::Absent => vec![("if-none-match".into(), "*".into())],
            Cond::Matches(e) => vec![("if-match".into(), format!("\"{e}\""))],
        };
        let resp = self.send("PUT", bucket, key, &[], &headers, body).await?;
        let status = resp.status().as_u16();
        // 409 is S3's "a concurrent conditional write to this key is in
        // flight": the same loss as a 412, just noticed earlier.
        if status == 412 || status == 409 {
            return Ok(Put::PreconditionFailed);
        }
        if !resp.status().is_success() {
            return Err(Self::fail(resp).await);
        }
        Ok(Put::Written(etag_of(&resp)))
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        let resp = self
            .send("DELETE", bucket, key, &[], &[], Vec::new())
            .await?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::fail(resp).await)
        }
    }

    async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), prefix.to_string()),
            ];
            if let Some(t) = &token {
                q.push(("continuation-token".into(), t.clone()));
            }
            let resp = self.send("GET", bucket, "", &q, &[], Vec::new()).await?;
            if !resp.status().is_success() {
                return Err(Self::fail(resp).await);
            }
            let text = resp
                .text()
                .await
                .map_err(|e| StoreError::new(e.to_string()))?;
            keys.extend(xml_tags(&text, "Key").into_iter().map(|k| xml_unescape(&k)));
            match xml_tag(&text, "NextContinuationToken") {
                Some(t) if xml_tag(&text, "IsTruncated").as_deref() == Some("true") => {
                    token = Some(xml_unescape(&t))
                }
                _ => break,
            }
        }
        Ok(keys)
    }
}

fn etag_of(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

/// The text of the first `<tag>…</tag>`. S3's XML is flat enough that a
/// parser would be more code than this.
fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    xml_tags(xml, tag).into_iter().next()
}

fn xml_tags(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(after[..end].to_string());
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// Directory
// ---------------------------------------------------------------------------

/// `root/<bucket>/<key>`. For development and tests, never production: it is
/// one host's disk, so it cannot be the authority two regions share.
#[derive(Clone, Debug)]
pub struct FsStore {
    pub root: PathBuf,
}

impl FsStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        if bucket.is_empty() || bucket.contains('/') || bucket.starts_with('.') {
            return Err(StoreError::new(format!("bad bucket name {bucket:?}")));
        }
        if key.split('/').any(|s| s == ".." || s == ".") || key.starts_with('/') {
            return Err(StoreError::new(format!("bad key {key:?}")));
        }
        Ok(self.root.join(bucket).join(key))
    }

    async fn blocking<T: Send + 'static>(
        f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> Result<T> {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| StoreError::new(e.to_string()))?
            .map_err(|e| StoreError::new(e.to_string()))
    }

    async fn ensure_bucket(&self, bucket: &str) -> Result<()> {
        let dir = self.path(bucket, "")?;
        Self::blocking(move || std::fs::create_dir_all(dir)).await
    }

    async fn get(&self, bucket: &str, key: &str) -> Result<Option<Object>> {
        let path = self.path(bucket, key)?;
        Self::blocking(move || match std::fs::read(&path) {
            Ok(bytes) => {
                let etag = sigv4::sha256_hex(&bytes);
                Ok(Some(Object { bytes, etag }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
    }

    async fn put(&self, bucket: &str, key: &str, body: Vec<u8>, cond: Cond) -> Result<Put> {
        let path = self.path(bucket, key)?;
        let bucket_dir = self.path(bucket, "")?;
        Self::blocking(move || {
            if !bucket_dir.is_dir() {
                return Err(std::io::Error::other(format!(
                    "no such bucket {}",
                    bucket_dir.display()
                )));
            }
            // One lock per bucket, held across the compare and the swap, and
            // taken by the hook process as well as this one.
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(bucket_dir.join(".lock"))?;
            lock.lock()?;
            let current = match std::fs::read(&path) {
                Ok(b) => Some(sigv4::sha256_hex(&b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            let ok = match &cond {
                Cond::None => true,
                Cond::Absent => current.is_none(),
                Cond::Matches(e) => current.as_deref() == Some(e.as_str()),
            };
            if !ok {
                return Ok(Put::PreconditionFailed);
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(&tmp, &body)?;
            std::fs::rename(&tmp, &path)?;
            Ok(Put::Written(sigv4::sha256_hex(&body)))
        })
        .await
    }

    async fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        let path = self.path(bucket, key)?;
        Self::blocking(move || match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        })
        .await
    }

    async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        let base = self.path(bucket, "")?;
        let prefix = prefix.to_string();
        Self::blocking(move || {
            let mut out = Vec::new();
            walk(&base, &base, &mut out)?;
            out.retain(|k| k.starts_with(&prefix) && k != ".lock");
            out.sort();
            Ok(out)
        })
        .await
    }
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(base, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(base) {
            let rel = rel.to_string_lossy().to_string();
            if !rel.contains(".tmp") {
                out.push(rel);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_addresses_aws_virtual_hosted_and_endpoints_path_style() {
        let aws = S3::new(None, "eu-west-1".into(), "a".into(), "b".into());
        assert_eq!(
            aws.address("bkt", "ns/a b"),
            (
                "https".into(),
                "bkt.s3.eu-west-1.amazonaws.com".into(),
                "/ns/a%20b".into()
            )
        );
        let minio = S3::new(
            Some("http://minio:9000/".into()),
            "us-east-1".into(),
            "a".into(),
            "b".into(),
        );
        assert_eq!(
            minio.address("bkt", ""),
            ("http".into(), "minio:9000".into(), "/bkt".into())
        );
        assert_eq!(
            minio.address("bkt", "k"),
            ("http".into(), "minio:9000".into(), "/bkt/k".into())
        );
        assert!(!format!("{aws:?}").contains("\"b\""), "secret is redacted");
    }

    #[test]
    fn s3_xml_is_read_by_tag() {
        let xml = "<R><Contents><Key>a&amp;b</Key></Contents><Contents><Key>c</Key></Contents>\
                   <IsTruncated>false</IsTruncated></R>";
        assert_eq!(
            xml_tags(xml, "Key")
                .iter()
                .map(|k| xml_unescape(k))
                .collect::<Vec<_>>(),
            vec!["a&b", "c"]
        );
        assert_eq!(xml_tag(xml, "Missing"), None);
    }

    #[tokio::test]
    async fn fs_store_honours_conditional_writes() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::Fs(FsStore::new(dir.path().into()));
        assert!(
            s.put("b", "k", b"x".to_vec(), Cond::None).await.is_err(),
            "no bucket yet"
        );
        s.ensure_bucket("b").await.unwrap();
        let Put::Written(e1) = s
            .put("b", "k", b"one".to_vec(), Cond::Absent)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            s.put("b", "k", b"x".to_vec(), Cond::Absent).await.unwrap(),
            Put::PreconditionFailed
        );
        assert_eq!(
            s.put("b", "k", b"x".to_vec(), Cond::Matches("stale".into()))
                .await
                .unwrap(),
            Put::PreconditionFailed
        );
        assert!(matches!(
            s.put("b", "k", b"two".to_vec(), Cond::Matches(e1))
                .await
                .unwrap(),
            Put::Written(_)
        ));
        assert_eq!(s.get("b", "k").await.unwrap().unwrap().bytes, b"two");
        s.put("b", "dir/x", vec![], Cond::None).await.unwrap();
        assert_eq!(s.list("b", "dir/").await.unwrap(), vec!["dir/x"]);
        assert_eq!(s.delete_prefix("b", "").await.unwrap(), 2);
        assert!(s.get("b", "k").await.unwrap().is_none());
        assert!(s.get("b", "../escape").await.is_err());
    }
}
