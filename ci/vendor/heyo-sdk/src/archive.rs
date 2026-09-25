//! Build a `tar.gz` archive from a local directory and upload it as a sandbox
//! archive. Mirrors `sdk-ts/src/archive.ts`.
//!
//! ```no_run
//! use heyo_sdk::{archive_dir, ArchiveDirOptions, HeyoClientOptions};
//! # async fn run() -> Result<(), heyo_sdk::HeyoError> {
//! let archive = archive_dir("./my-app", ArchiveDirOptions::default(), HeyoClientOptions::default()).await?;
//! println!("archive id: {}", archive.id);
//! # Ok(()) }
//! ```
//!
//! Minimal v0: excludes a default deny-list of build/dep directories. Does
//! **not** parse `.gitignore` — that's a follow-up.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{HeyoClient, HeyoClientOptions, RequestOptions};
use crate::errors::HeyoError;

const DEFAULT_MOUNT_PATH: &str = "/workspace";

const EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".cache",
    ".npm",
    ".cargo",
    "dist",
    ".next",
    ".nuxt",
    "build",
    "vendor",
    ".venv",
    "venv",
    ".tox",
];

#[derive(Debug, Clone, Default)]
pub struct ArchiveDirOptions {
    /// Display name for the archive.
    pub name: Option<String>,
    /// Mount-path prefix for files inside the archive. Default: `/workspace`.
    pub mount_path: Option<String>,
    /// If true, skip the default exclude list (`.git`, `node_modules`, …).
    pub no_ignore: bool,
    /// Extra directory names to exclude (matched on the segment, not the path).
    pub extra_excludes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ArchiveResult {
    pub id: String,
    pub size_bytes: u64,
    pub created_at: String,
}

#[derive(Deserialize)]
struct PresignResponse {
    archive_id: String,
    upload_url: String,
}

#[derive(Serialize)]
struct FinalizeRequest<'a> {
    sandbox_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

#[derive(Deserialize)]
struct FinalizeResponse {
    id: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    created_at: String,
}

/// Tar+gzip `local_path` and upload it as a sandbox archive.
pub async fn archive_dir(
    local_path: impl AsRef<Path>,
    options: ArchiveDirOptions,
    client_options: HeyoClientOptions,
) -> Result<ArchiveResult, HeyoError> {
    let dir = local_path
        .as_ref()
        .canonicalize()
        .map_err(|e| HeyoError::invalid(format!("invalid path '{}': {}", local_path.as_ref().display(), e)))?;
    if !dir.is_dir() {
        return Err(HeyoError::invalid(format!(
            "'{}' is not a directory",
            dir.display()
        )));
    }
    let prefix = options
        .mount_path
        .as_deref()
        .unwrap_or(DEFAULT_MOUNT_PATH)
        .trim_start_matches('/');
    let entries = collect_files(&dir, prefix, options.no_ignore, &options.extra_excludes)?;
    if entries.is_empty() {
        return Err(HeyoError::invalid("No files found in directory to archive"));
    }

    let tar_gz = build_tar_gz(&entries)?;

    let client = HeyoClient::new(client_options)?;
    let presign: PresignResponse = client
        .request(
            Method::POST,
            "/sandbox-archives/presign",
            None::<&()>,
            RequestOptions::default(),
        )
        .await?;

    upload_to_presigned(&presign.upload_url, tar_gz, &client).await?;

    let basename = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("archive");
    let body = FinalizeRequest {
        sandbox_id: basename,
        name: options.name.as_deref(),
    };
    let finalized: FinalizeResponse = client
        .request(
            Method::POST,
            &format!("/sandbox-archives/{}/finalize", presign.archive_id),
            Some(&body),
            RequestOptions::default(),
        )
        .await?;

    Ok(ArchiveResult {
        id: finalized.id,
        size_bytes: finalized.size_bytes,
        created_at: finalized.created_at,
    })
}

struct TarFileEntry {
    path_in_archive: String,
    content: Vec<u8>,
}

fn collect_files(
    root: &Path,
    prefix: &str,
    no_ignore: bool,
    extra_excludes: &[String],
) -> Result<Vec<TarFileEntry>, HeyoError> {
    let excluded: BTreeSet<&str> = if no_ignore {
        BTreeSet::new()
    } else {
        let mut s: BTreeSet<&str> = EXCLUDED_DIRS.iter().copied().collect();
        for e in extra_excludes {
            s.insert(e.as_str());
        }
        s
    };
    let mut out = Vec::new();
    walk(root, root, prefix, &excluded, &mut out)?;
    Ok(out)
}

fn walk(
    root: &Path,
    cur: &Path,
    prefix: &str,
    excluded: &BTreeSet<&str>,
    out: &mut Vec<TarFileEntry>,
) -> Result<(), HeyoError> {
    let entries = std::fs::read_dir(cur)
        .map_err(|e| HeyoError::invalid(format!("read_dir {}: {}", cur.display(), e)))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| HeyoError::invalid(format!("read_dir entry in {}: {}", cur.display(), e)))?;
        let file_type = entry
            .file_type()
            .map_err(|e| HeyoError::invalid(format!("file_type for {}: {}", entry.path().display(), e)))?;
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if file_type.is_dir() {
            if excluded.contains(name_str) {
                continue;
            }
            walk(root, &entry.path(), prefix, excluded, out)?;
        } else if file_type.is_file() {
            let abs = entry.path();
            let rel = abs
                .strip_prefix(root)
                .map_err(|e| HeyoError::invalid(format!("strip_prefix {}: {}", abs.display(), e)))?;
            let rel_str = rel
                .to_str()
                .ok_or_else(|| HeyoError::invalid(format!("non-utf8 path: {}", rel.display())))?
                .replace('\\', "/");
            let content = std::fs::read(&abs)
                .map_err(|e| HeyoError::invalid(format!("read {}: {}", abs.display(), e)))?;
            let path_in_archive = if prefix.is_empty() {
                rel_str
            } else {
                format!("{}/{}", prefix, rel_str)
            };
            out.push(TarFileEntry {
                path_in_archive,
                content,
            });
        }
        // Symlinks intentionally skipped, matching the TS implementation.
    }
    Ok(())
}

fn build_tar_gz(entries: &[TarFileEntry]) -> Result<Vec<u8>, HeyoError> {
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut blocks: Vec<Vec<u8>> = Vec::new();

    // Directory entries (sorted, deduped) so the archive plays nicely with
    // tools that need explicit dirs.
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        let mut acc = String::new();
        let mut parts: Vec<&str> = entry.path_in_archive.split('/').collect();
        parts.pop(); // drop filename
        for part in parts {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            dirs.insert(acc.clone());
        }
    }
    for dir in dirs {
        let path = format!("{}/", dir);
        append_entry(&mut blocks, &path, &[], 0o755, mtime, b'5')?;
    }
    for entry in entries {
        append_entry(
            &mut blocks,
            &entry.path_in_archive,
            &entry.content,
            0o644,
            mtime,
            b'0',
        )?;
    }
    // Two trailing 512-byte zero blocks.
    blocks.push(vec![0; 1024]);

    let tar: Vec<u8> = blocks.into_iter().flatten().collect();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&tar)
        .map_err(|e| HeyoError::api(0, format!("gzip write: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| HeyoError::api(0, format!("gzip finish: {}", e)))
}

fn append_entry(
    out: &mut Vec<Vec<u8>>,
    name: &str,
    content: &[u8],
    mode: u32,
    mtime: u64,
    typeflag: u8,
) -> Result<(), HeyoError> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > 100 {
        // GNU LongLink extension.
        let mut long_name = name_bytes.to_vec();
        long_name.push(0);
        let header = build_header(
            b"././@LongLink",
            0,
            0,
            0,
            long_name.len() as u64,
            0,
            b'L',
            *b"ustar ",
            *b" \0",
        );
        out.push(header.to_vec());
        let padded_len = pad_to_512(long_name.len());
        let mut padded = long_name;
        padded.resize(padded_len, 0);
        out.push(padded);
    }
    let truncated = if name_bytes.len() > 100 {
        &name_bytes[..100]
    } else {
        name_bytes
    };
    let header = build_header(
        truncated,
        mode,
        1000,
        1000,
        content.len() as u64,
        mtime,
        typeflag,
        *b"ustar\0",
        *b"00",
    );
    out.push(header.to_vec());
    if !content.is_empty() {
        let padded_len = pad_to_512(content.len());
        let mut padded = content.to_vec();
        padded.resize(padded_len, 0);
        out.push(padded);
    }
    Ok(())
}

fn pad_to_512(len: usize) -> usize {
    if len % 512 == 0 {
        len
    } else {
        len + (512 - (len % 512))
    }
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    name: &[u8],
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: u64,
    typeflag: u8,
    magic: [u8; 6],
    version: [u8; 2],
) -> [u8; 512] {
    let mut buf = [0u8; 512];
    let nlen = name.len().min(100);
    buf[..nlen].copy_from_slice(&name[..nlen]);
    write_octal(&mut buf, 100, 8, mode as u64, false);
    write_octal(&mut buf, 108, 8, uid as u64, false);
    write_octal(&mut buf, 116, 8, gid as u64, false);
    write_octal(&mut buf, 124, 12, size, true);
    write_octal(&mut buf, 136, 12, mtime, true);
    for b in &mut buf[148..156] {
        *b = b' ';
    }
    buf[156] = typeflag;
    buf[257..263].copy_from_slice(&magic);
    buf[263..265].copy_from_slice(&version);

    let mut sum: u32 = 0;
    for b in buf.iter() {
        sum += *b as u32;
    }
    let sum_str = format!("{:06o}", sum);
    let sum_bytes = sum_str.as_bytes();
    let take = sum_bytes.len().min(6);
    buf[148..148 + take].copy_from_slice(&sum_bytes[..take]);
    buf[154] = 0;
    buf[155] = b' ';
    buf
}

fn write_octal(buf: &mut [u8], offset: usize, width: usize, value: u64, trailing_space: bool) {
    let s = format!("{:0>width$o}", value, width = width - 1);
    let bytes = s.as_bytes();
    for i in 0..(width - 1) {
        buf[offset + i] = bytes.get(i).copied().unwrap_or(b'0');
    }
    buf[offset + width - 1] = if trailing_space { b' ' } else { 0 };
}

async fn upload_to_presigned(url: &str, body: Vec<u8>, client: &HeyoClient) -> Result<(), HeyoError> {
    // The presigned URL belongs to the object store, not our cloud — don't
    // attach the bearer header. Build a one-shot request directly via reqwest.
    let http = reqwest::Client::new();
    let len = body.len();
    let resp = http
        .put(url)
        .header("Content-Type", "application/gzip")
        .header("Content-Length", len.to_string())
        .body(body)
        .send()
        .await
        .map_err(|e| HeyoError::api(0, format!("archive PUT to presigned URL: {}", e)))?;
    let _ = client; // keep parameter for symmetry with TS sig.
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(HeyoError::api(status, format!("archive upload failed: {}", body)));
    }
    Ok(())
}
