//! Image-level archive: stream a stopped VM's raw `data.ext4` to S3 instead
//! of dumping its database — the fallback for schemas whose Postgres won't
//! boot or won't dump (version-mismatched pgdata, sick disks, wedged WAL).
//! The dump path needs a healthy Postgres; this path needs nothing but the
//! disk file, so it can always drain the box.
//!
//! Format is deliberately primitive: the raw ext4 image, zstd-compressed, at
//! `{prefix}{schema}.img.zst`. Disaster recovery needs nothing but
//! `curl <presigned-get> | zstd -d --sparse -o data.ext4` — no metadata
//! service, no chunk store, no pooler.
//!
//! Invariants, in the order the incident taught them:
//! - the disk is only read once nothing holds it open (fd scan after the
//!   daemon acks the stop);
//! - the image is spooled and integrity-checked (`zstd -t` + the ext4 magic)
//!   before any bytes leave the box, and the upload is verified with a HEAD
//!   against a pre-upload baseline before anything else happens;
//! - a stale dump object at the schema's dump key is *deleted* before the
//!   caller flips the tier — it is older than the disk just imaged, and a
//!   restore preferring it would be silent data loss;
//! - the source bytes are archived exactly as they are. A sick filesystem is
//!   never repaired before upload (repair belongs on a restore-time copy);
//!   the best-effort trim uses `e2fsck -fp`, the same preen the reclaim
//!   scripts run, and a disk that fails it is uploaded untrimmed.
//!
//! Restore inverts the readopt maneuver from emergency-drain.sh: create a
//! fresh VM, stop it, copy the downloaded image **in place** over its
//! `data.ext4` (same inode — a jailer hard-link must keep pointing at the
//! adopted bytes), and boot on the real data.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::Config;
use crate::s3::S3Config;

/// How long to wait after a stop for the Firecracker process to release the
/// disk file (the daemon acks the stop before the process exits).
const DISK_RELEASE_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest object uploaded as one PUT; anything bigger goes multipart. Well
/// under S3's 5GB single-PUT cap, and also the bound on pooler memory per
/// upload (a single PUT rides through one `Vec`).
const SINGLE_PUT_MAX: u64 = 100 * 1024 * 1024;

/// Multipart part size. 64MB parts bound pooler memory while keeping even a
/// 250GB image under S3's 10k-part limit.
const PART_SIZE: u64 = 64 * 1024 * 1024;

/// Floor for a plausible compressed image. Deliberately tiny — an all-zero
/// thin disk compresses savagely, and the real validity check is the ext4
/// magic — this only rejects an empty/torn spool file.
const MIN_IMAGE_BYTES: u64 = 1024;

/// Wall-clock bounds for the external tools. Compression/decompression of a
/// legacy fat disk can legitimately take a long while on one core.
const ZSTD_TIMEOUT: Duration = Duration::from_secs(3600);
const FSCK_TIMEOUT: Duration = Duration::from_secs(900);
const COPY_TIMEOUT: Duration = Duration::from_secs(1800);

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const PART_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const PRESIGN_TTL: Duration = Duration::from_secs(3600);

/// ext4 superblock magic: 0xEF53 little-endian at byte offset 1080
/// (superblock at 1024 + s_magic at 56).
const EXT4_MAGIC_OFFSET: u64 = 1080;
const EXT4_MAGIC: [u8; 2] = [0x53, 0xEF];

/// Duplicated from `vm::MIN_ARCHIVE_BYTES` (private there): the smallest
/// object that can be a real `pg_dump` archive. Used when choosing between
/// the dump and image keys at restore time — a sub-minimum dump object is a
/// torn artifact, never preferred over an image.
const MIN_DUMP_BYTES: u64 = 512;

/// What a successful image archive produced, for the caller's journal entry.
pub struct ImageArchived {
    pub bytes: u64,
    /// `PG_VERSION` read from the disk before upload ("16", "18"), when
    /// debugfs could get at it. An image archives the pgdata version along
    /// with the data, so a mismatch with the current rootfs must be visible
    /// to the operator *before* a restore fails on it.
    pub pg_version: Option<String>,
}

/// Which S3 object a restore of an archived schema should use.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RestoreKind {
    Dump,
    Image,
}

/// Pure decision: prefer the logical dump when a plausible one exists (it is
/// version-independent and only present when the image path deleted nothing),
/// fall back to the image, and default to the dump path when neither looks
/// usable — its preflight already reports "no archive to restore" with the
/// right words.
fn choose_restore(dump_len: Option<u64>, image_len: Option<u64>) -> RestoreKind {
    match (dump_len, image_len) {
        (Some(d), _) if d >= MIN_DUMP_BYTES => RestoreKind::Dump,
        (_, Some(i)) if i >= MIN_IMAGE_BYTES => RestoreKind::Image,
        _ => RestoreKind::Dump,
    }
}

/// HEAD both keys and pick the restore source for an archived schema. Any
/// transport failure falls back to the dump path — exactly what every
/// archived schema used before images existed.
pub async fn pick_restore(s3: &S3Config, schema: &str) -> RestoreKind {
    let Ok(http) = reqwest::Client::builder().build() else {
        return RestoreKind::Dump;
    };
    let len_of = |r: anyhow::Result<Option<crate::s3::ObjectId>>| match r {
        Ok(id) => id.map(|i| i.content_length),
        Err(_) => None,
    };
    let dump = len_of(
        s3.head_object(&http, &s3.object_key(schema), HEAD_TIMEOUT)
            .await,
    );
    // Only ask about the image when the dump can't decide — the common case
    // (dump-archived schema) costs one HEAD, not two.
    if matches!(dump, Some(d) if d >= MIN_DUMP_BYTES) {
        return RestoreKind::Dump;
    }
    let image = len_of(
        s3.head_object(&http, &s3.image_object_key(schema), HEAD_TIMEOUT)
            .await,
    );
    choose_restore(dump, image)
}

/// Archive `sandbox_id`'s data disk for `schema` to S3. The caller must hold
/// the schema's archiving guard and have stopped the VM (a still-running
/// Firecracker fails the release wait below). On `Ok`, the image is verified
/// durable in S3 and any stale dump object is gone — the caller may flip the
/// tier and kill the VM.
pub async fn archive_disk(
    cfg: &Config,
    s3: &S3Config,
    schema: &str,
    sandbox_id: &str,
) -> Result<ImageArchived> {
    let img_cfg = cfg
        .image_archive
        .as_ref()
        .context("image archiving is not enabled (set PG_VM_POOL_IMAGE_ARCHIVE=1)")?;
    let run_dir = cfg
        .run_dir
        .as_ref()
        .context("image archiving needs the run dir (set PG_VM_POOL_RUN_DIR)")?;
    let disk = run_dir.join(sandbox_id).join("data.ext4");
    let md = tokio::fs::metadata(&disk)
        .await
        .with_context(|| format!("schema {schema}: no data disk at {}", disk.display()))?;

    wait_disk_released(&disk).await?;

    let pg_version = pg_version_of(&disk).await;
    if let Some(v) = &pg_version {
        info!("schema {schema}: disk carries pgdata v{v}");
    }

    // Best-effort trim: journal replay + hole-punch shrink the upload. A disk
    // that fails preen is uploaded exactly as it is — these bytes may be the
    // only copy, and repair belongs on a restore-time copy.
    trim_disk(schema, &disk).await;

    // Spool space: the compressed image can't exceed the disk's allocated
    // bytes, so that (plus slack) is the safe bound to check.
    let allocated = {
        use std::os::unix::fs::MetadataExt;
        // Re-stat: the trim may have released blocks.
        tokio::fs::metadata(&disk).await.map(|m| m.blocks() * 512).unwrap_or(md.len())
    };
    tokio::fs::create_dir_all(&img_cfg.spool_dir)
        .await
        .with_context(|| format!("creating spool dir {}", img_cfg.spool_dir.display()))?;
    if let Some(free) = free_bytes(&img_cfg.spool_dir).await
        && free < allocated + allocated / 10
    {
        bail!(
            "schema {schema}: spool dir {} has {} free but the disk has {} allocated — \
             not enough room to spool the image (point PG_VM_POOL_IMAGE_SPOOL_DIR at a \
             roomier filesystem)",
            img_cfg.spool_dir.display(),
            crate::orphans::human_iec(free),
            crate::orphans::human_iec(allocated),
        );
    }

    let spool = img_cfg.spool_dir.join(format!("{schema}.img.zst"));
    let res = archive_via_spool(s3, schema, &disk, &spool).await;
    // The spool file is scratch either way; a failed upload's remnant would
    // only mislead the next attempt's free-space math.
    let _ = tokio::fs::remove_file(&spool).await;
    res.map(|bytes| ImageArchived { bytes, pg_version })
}

/// compress → verify → upload → verify → delete stale dump. Split out so the
/// caller can clean the spool file on every exit path.
async fn archive_via_spool(
    s3: &S3Config,
    schema: &str,
    disk: &Path,
    spool: &Path,
) -> Result<u64> {
    // -3 is zstd's default level: the bulk of these images is zeros and
    // page-structured data where higher levels buy little for a lot of CPU.
    run_ok(
        Command::new("zstd").args(["-q", "-f", "-3", "-T0", "-o"]).arg(spool).arg(disk),
        "compressing the disk image (is zstd installed?)",
        ZSTD_TIMEOUT,
    )
    .await?;
    let len = tokio::fs::metadata(spool)
        .await
        .with_context(|| format!("statting spool file {}", spool.display()))?
        .len();
    anyhow::ensure!(
        len >= MIN_IMAGE_BYTES,
        "schema {schema}: spooled image is only {len} bytes — not a filesystem image; \
         refusing to archive it"
    );

    // Integrity of the compressed stream, then proof the payload is an ext4
    // image at all — both before a byte is uploaded. History says exactly
    // this: torn files under disk pressure that every tool "succeeded" on.
    run_ok(
        Command::new("zstd").args(["-q", "-t"]).arg(spool),
        "verifying the spooled image (zstd -t)",
        ZSTD_TIMEOUT,
    )
    .await?;
    check_compressed_ext4_magic(spool).await.with_context(|| {
        format!("schema {schema}: spooled image does not decompress to an ext4 filesystem")
    })?;

    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client for the image upload")?;
    let key = s3.image_object_key(schema);
    // Baseline first: also where a wrong-region bucket is discovered and
    // latched before anything is presigned (same order as the dump path).
    let baseline = s3.head_object(&http, &key, HEAD_TIMEOUT).await.with_context(|| {
        format!("pre-upload HEAD of s3://{}/{key} — refusing to upload unverifiably", s3.bucket)
    })?;

    upload_file(s3, &http, &key, spool, len, PART_SIZE, SINGLE_PUT_MAX).await?;

    // The object must exist, be exactly the spool file's size, and differ
    // from whatever the baseline saw — presence alone proves nothing at a
    // stable key.
    match s3.head_object(&http, &key, HEAD_TIMEOUT).await {
        Ok(Some(id)) if id.content_length == len && Some(&id) != baseline.as_ref() => {}
        Ok(Some(id)) => bail!(
            "uploaded s3://{}/{key} but the HEAD reports {} bytes against a {len}-byte \
             spool file (baseline unchanged: {}) — refusing to trust it",
            s3.bucket,
            id.content_length,
            Some(&id) == baseline.as_ref(),
        ),
        Ok(None) => bail!("uploaded s3://{}/{key} but a HEAD finds nothing", s3.bucket),
        Err(e) => return Err(e.context("verifying the uploaded image")),
    }

    // A leftover dump object at the schema's dump key is *older* than the
    // disk just imaged; a restore preferring it would be silent data loss.
    // Deleted before the caller flips the tier, so no crash window can leave
    // both keys live. A failure here fails the archive — the image object
    // stays behind harmlessly and the next attempt overwrites it.
    let dump_key = s3.object_key(schema);
    s3.delete_object(&http, &dump_key, HEAD_TIMEOUT)
        .await
        .with_context(|| {
            format!(
                "image uploaded, but deleting the stale dump at s3://{}/{dump_key} failed — \
                 not flipping the tier while an older dump could shadow the image",
                s3.bucket
            )
        })?;

    info!(
        "schema {schema}: disk image archived to s3://{}/{key} ({})",
        s3.bucket,
        crate::orphans::human_iec(len)
    );
    Ok(len)
}

/// Upload `path` (of size `len`) to `key`: one PUT when small, multipart
/// otherwise. Part/threshold sizes are parameters so tests can exercise the
/// multipart path with tiny files.
async fn upload_file(
    s3: &S3Config,
    http: &reqwest::Client,
    key: &str,
    path: &Path,
    len: u64,
    part_size: u64,
    single_put_max: u64,
) -> Result<()> {
    if len <= single_put_max {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        return s3.put_object(http, key, bytes, PART_UPLOAD_TIMEOUT).await;
    }

    let upload_id = s3.initiate_multipart(http, key).await?;
    let res = upload_parts(s3, http, key, &upload_id, path, part_size).await;
    if res.is_err()
        && let Err(e) = s3.abort_multipart(http, key, &upload_id).await
    {
        warn!("aborting failed multipart upload of s3://{}/{key} failed too: {e:#}", s3.bucket);
    }
    res
}

async fn upload_parts(
    s3: &S3Config,
    http: &reqwest::Client,
    key: &str,
    upload_id: &str,
    path: &Path,
    part_size: u64,
) -> Result<()> {
    let mut f = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let mut parts: Vec<(u32, String)> = Vec::new();
    let mut buf = vec![0u8; part_size as usize];
    loop {
        // Fill a whole part (or hit EOF): read() may return short counts.
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = f.read(&mut buf[filled..]).await.context("reading the spool file")?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let part_number = parts.len() as u32 + 1;
        let etag = s3
            .upload_part(http, key, upload_id, part_number, buf[..filled].to_vec(), PART_UPLOAD_TIMEOUT)
            .await?;
        parts.push((part_number, etag));
        if filled < buf.len() {
            break;
        }
    }
    anyhow::ensure!(!parts.is_empty(), "spool file emptied out from under the upload");
    s3.complete_multipart(http, key, upload_id, &parts).await
}

/// Materialize the VM for an image-archived `schema`: download + decompress
/// the image, create a fresh VM, and swap the image in under it (see the
/// module docs for why the copy is in-place). Returns the booted, ready
/// sandbox; `ensure_vm` takes it from there.
pub async fn materialize_from_image(
    cfg: &Config,
    schema: &str,
    s3: &S3Config,
) -> Result<heyo_sdk::Sandbox> {
    let run_dir = cfg
        .run_dir
        .as_ref()
        .context("restoring a disk image needs the run dir (set PG_VM_POOL_RUN_DIR)")?;
    let key = s3.image_object_key(schema);
    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client for the image download")?;
    let expect_len = match s3.head_object(&http, &key, HEAD_TIMEOUT).await? {
        Some(id) if id.content_length >= MIN_IMAGE_BYTES => id.content_length,
        Some(id) => bail!(
            "schema {schema}: the image at s3://{}/{key} is only {} bytes — it is not a \
             filesystem image and cannot be restored",
            s3.bucket,
            id.content_length
        ),
        None => bail!(
            "schema {schema} is marked archived but neither a dump nor an image exists \
             at s3://{}/{} — there is no archive to restore",
            s3.bucket,
            key
        ),
    };

    // Scratch on the run-dir filesystem, so the final copy never crosses
    // filesystems. Dot-named: nothing scanning for `sb-*` ever sees it.
    let scratch = run_dir.join(".pg-vm-pool-restore");
    tokio::fs::create_dir_all(&scratch)
        .await
        .with_context(|| format!("creating restore scratch dir {}", scratch.display()))?;
    let zst = scratch.join(format!("{schema}.img.zst"));
    let raw = scratch.join(format!("{schema}.ext4"));

    let res = materialize_inner(cfg, schema, s3, &key, &http, expect_len, &zst, &raw).await;
    let _ = tokio::fs::remove_file(&zst).await;
    let _ = tokio::fs::remove_file(&raw).await;
    res
}

#[allow(clippy::too_many_arguments)]
async fn materialize_inner(
    cfg: &Config,
    schema: &str,
    s3: &S3Config,
    key: &str,
    http: &reqwest::Client,
    expect_len: u64,
    zst: &Path,
    raw: &Path,
) -> Result<heyo_sdk::Sandbox> {
    download(s3, http, key, expect_len, zst).await?;
    run_ok(
        Command::new("zstd").args(["-q", "-d", "-f", "--sparse", "-o"]).arg(raw).arg(zst),
        "decompressing the disk image (is zstd installed?)",
        ZSTD_TIMEOUT,
    )
    .await?;
    check_ext4_magic(raw)
        .await
        .with_context(|| format!("schema {schema}: downloaded image is not an ext4 filesystem"))?;
    // Report-only fsck: a dirty journal is expected (the archive-time stop is
    // an unclean power-off) and the guest replays it on boot; this just puts
    // the disk's state on the record before the VM gets it.
    if let Ok(out) = run(
        Command::new("e2fsck").args(["-fn"]).arg(raw),
        FSCK_TIMEOUT,
    )
    .await && !out.status.success()
    {
        info!(
            "schema {schema}: restored image has fsck findings (exit {:?}) — expected \
             for an unclean-stop archive; the guest replays the journal on boot",
            out.status.code()
        );
    }

    // The readopt maneuver: fresh VM (created booted+ready), stop it, copy
    // the image in place over its empty disk, boot on the real data.
    let name = format!("pg-{schema}");
    let sandbox = crate::vm::create_vm(cfg, &name, cfg.is_keepalive(schema)).await?;
    if let Err(e) = swap_and_boot(cfg, &sandbox, schema, raw).await {
        // The half-adopted VM must not be left running an empty (or torn)
        // database that a later find-by-name could serve as the schema.
        warn!("schema {schema}: image restore failed after VM create; stopping {}", sandbox.sandbox_id());
        let _ = tokio::time::timeout(Duration::from_secs(30), sandbox.stop()).await;
        return Err(e);
    }
    Ok(sandbox)
}

async fn swap_and_boot(
    cfg: &Config,
    sandbox: &heyo_sdk::Sandbox,
    schema: &str,
    raw: &Path,
) -> Result<()> {
    let run_dir = cfg.run_dir.as_ref().expect("checked by materialize_from_image");
    let disk = run_dir.join(sandbox.sandbox_id()).join("data.ext4");

    tokio::time::timeout(Duration::from_secs(30), sandbox.stop())
        .await
        .context("stopping the fresh VM for the disk swap timed out")?
        .context("stopping the fresh VM for the disk swap")?;
    wait_disk_released(&disk).await?;

    // In place (`cp`, not rename): the write goes through the existing inode,
    // so a jailer hard-link to data.ext4 keeps pointing at the adopted bytes.
    run_ok(
        Command::new("cp").arg("--sparse=always").arg(raw).arg(&disk),
        "copying the restored image over the fresh VM's disk",
        COPY_TIMEOUT,
    )
    .await
    .with_context(|| format!("schema {schema}: adopting the image into {}", disk.display()))?;

    let started = {
        // A reclaim pass must not be mid-fsck on this disk when the VM boots.
        let _permit = crate::reclaim::boot_permit().await;
        sandbox.start().await
    };
    started.context("booting the VM on the restored image")?;
    sandbox
        .wait_for_ready(cfg.ready_timeout)
        .await
        .with_context(|| format!("schema {schema}: VM did not become ready on the restored image"))?;
    Ok(())
}

/// Stream the object to `dest` and require exactly the HEAD-reported length —
/// a short body (dropped connection) must fail here, not at the ext4 check.
async fn download(
    s3: &S3Config,
    http: &reqwest::Client,
    key: &str,
    expect_len: u64,
    dest: &Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let url = s3.presign_get(key, PRESIGN_TTL);
    let mut resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET s3://{}/{key}", s3.bucket))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "GET s3://{}/{key} returned {}",
        s3.bucket,
        resp.status()
    );
    let mut f = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut written = 0u64;
    while let Some(chunk) = resp.chunk().await.context("reading the image download")? {
        f.write_all(&chunk).await.context("writing the image download")?;
        written += chunk.len() as u64;
    }
    f.flush().await.context("flushing the image download")?;
    anyhow::ensure!(
        written == expect_len,
        "downloaded {written} bytes of s3://{}/{key} but the object is {expect_len} — \
         truncated transfer",
        s3.bucket
    );
    Ok(())
}

/// Wait until nothing on the host holds the disk file open. The fd scan is
/// the same (device, inode) sweep the orphan sweep trusts; a scan that can
/// see nothing (no /proc visibility) degrades to reporting "free", so a short
/// settle-and-recheck follows the first free reading either way.
async fn wait_disk_released(disk: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = tokio::fs::metadata(disk)
        .await
        .with_context(|| format!("statting {}", disk.display()))?;
    let target = (md.dev(), md.ino());
    let held = || async {
        tokio::task::spawn_blocking(move || crate::orphans::open_inodes().contains(&target))
            .await
            .unwrap_or(false)
    };
    let deadline = tokio::time::Instant::now() + DISK_RELEASE_TIMEOUT;
    loop {
        if !held().await {
            // The daemon acks stops before Firecracker exits; give a straggler
            // a beat to close, then confirm.
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !held().await {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{} is still held open {DISK_RELEASE_TIMEOUT:?} after the stop — \
                 refusing to touch a disk something is writing",
                disk.display()
            );
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Best-effort `e2fsck -fp -E discard`: replays a dirty journal (the stop was
/// an unclean power-off) and punches freed blocks out of the sparse file.
/// Exit 0/1 = clean/corrected; anything else archives the disk as-is.
async fn trim_disk(schema: &str, disk: &Path) {
    match run(
        Command::new("e2fsck").args(["-fp", "-E", "discard"]).arg(disk),
        FSCK_TIMEOUT,
    )
    .await
    {
        Ok(out) if matches!(out.status.code(), Some(0) | Some(1)) => {}
        Ok(out) => warn!(
            "schema {schema}: pre-archive trim failed (fsck exit {:?}); archiving the \
             disk exactly as it is: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or_default()
        ),
        Err(e) => warn!("schema {schema}: pre-archive trim unavailable ({e:#}); archiving as-is"),
    }
}

/// `PG_VERSION` from `/pgdata` on the (unmounted) disk, via debugfs — with
/// the `-c` fallback for a dirty journal, where normal open refuses on bitmap
/// checksums. Best-effort provenance, never load-bearing.
async fn pg_version_of(disk: &Path) -> Option<String> {
    for catastrophic in [false, true] {
        let mut cmd = Command::new("debugfs");
        if catastrophic {
            cmd.arg("-c");
        }
        cmd.args(["-R", "cat /pgdata/PG_VERSION"]).arg(disk);
        let Ok(out) = run(&mut cmd, Duration::from_secs(30)).await else {
            return None;
        };
        let v: String = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !v.is_empty() && v.len() <= 4 && v.bytes().all(|b| b.is_ascii_digit()) {
            return Some(v);
        }
    }
    None
}

/// The ext4 magic, read straight from a raw image file.
async fn check_ext4_magic(path: &Path) -> Result<()> {
    use tokio::io::AsyncSeekExt;
    let mut f = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    f.seek(std::io::SeekFrom::Start(EXT4_MAGIC_OFFSET))
        .await
        .context("seeking to the superblock")?;
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic)
        .await
        .context("image is too small to hold an ext4 superblock")?;
    anyhow::ensure!(
        magic == EXT4_MAGIC,
        "no ext4 magic at offset {EXT4_MAGIC_OFFSET} (found {magic:02x?})"
    );
    Ok(())
}

/// The ext4 magic, read from the *decompressed* head of a spooled `.zst` —
/// proves the payload without materializing it. `zstd -dc` is killed as soon
/// as the superblock has streamed out.
async fn check_compressed_ext4_magic(spool: &Path) -> Result<()> {
    let mut child = Command::new("zstd")
        .args(["-q", "-dc"])
        .arg(spool)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning zstd -dc for the magic check")?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut head = vec![0u8; (EXT4_MAGIC_OFFSET + 2) as usize];
    let read = tokio::time::timeout(Duration::from_secs(60), async {
        let mut filled = 0usize;
        while filled < head.len() {
            let n = stdout.read(&mut head[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok::<usize, std::io::Error>(filled)
    })
    .await
    .context("reading the decompressed head timed out")?
    .context("reading the decompressed head")?;
    drop(stdout);
    let _ = child.kill().await;
    anyhow::ensure!(
        read == head.len(),
        "decompressed payload is only {read} bytes — too small for an ext4 superblock"
    );
    let magic = &head[EXT4_MAGIC_OFFSET as usize..];
    anyhow::ensure!(
        magic == EXT4_MAGIC,
        "no ext4 magic at offset {EXT4_MAGIC_OFFSET} (found {magic:02x?})"
    );
    Ok(())
}

/// Free bytes on `dir`'s filesystem, via `df -Pk` (std has no statvfs).
/// `None` when df is missing/unparseable — the caller then skips the check
/// rather than failing an archive on a metrics gap.
async fn free_bytes(dir: &Path) -> Option<u64> {
    let out = run(Command::new("df").arg("-Pk").arg(dir), Duration::from_secs(30))
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // POSIX -P format: header, then one line; "Available" is field 4.
    let avail_kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

/// Run an external command with a wall-clock bound. `kill_on_drop` reaps it
/// if the timeout (or the caller) abandons the wait.
async fn run(cmd: &mut Command, timeout: Duration) -> Result<std::process::Output> {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(res) => res.map_err(anyhow::Error::from),
        Err(_) => bail!("command did not finish within {timeout:?}"),
    }
}

/// [`run`], requiring exit 0; the error carries the first stderr lines.
async fn run_ok(cmd: &mut Command, what: &str, timeout: Duration) -> Result<()> {
    let out = run(cmd, timeout).await.with_context(|| what.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let head: String = stderr.lines().take(3).collect::<Vec<_>>().join(" | ");
    bail!("{what}: exit {:?}: {head}", out.status.code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn restore_prefers_a_plausible_dump() {
        // The common case: dump-archived schema, no image.
        assert_eq!(choose_restore(Some(4096), None), RestoreKind::Dump);
        // Image-archived schema: the dump key was deleted.
        assert_eq!(choose_restore(None, Some(1 << 20)), RestoreKind::Image);
        // A torn (sub-minimum) dump object must never shadow a real image.
        assert_eq!(choose_restore(Some(0), Some(1 << 20)), RestoreKind::Image);
        assert_eq!(choose_restore(Some(511), Some(1 << 20)), RestoreKind::Image);
        // Both plausible shouldn't happen (the image path deletes the dump),
        // but if it does, the dump is the safe, version-independent choice.
        assert_eq!(choose_restore(Some(4096), Some(1 << 20)), RestoreKind::Dump);
        // Neither: default to the dump path, whose preflight names the truth.
        assert_eq!(choose_restore(None, None), RestoreKind::Dump);
        assert_eq!(choose_restore(Some(100), Some(10)), RestoreKind::Dump);
    }

    #[tokio::test]
    async fn ext4_magic_check_accepts_and_rejects() {
        let dir = std::env::temp_dir().join(format!("imgarchive-magic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.ext4");
        let mut bytes = vec![0u8; 4096];
        bytes[EXT4_MAGIC_OFFSET as usize] = EXT4_MAGIC[0];
        bytes[EXT4_MAGIC_OFFSET as usize + 1] = EXT4_MAGIC[1];
        std::fs::write(&good, &bytes).unwrap();
        check_ext4_magic(&good).await.unwrap();

        let bad = dir.join("bad.ext4");
        std::fs::write(&bad, vec![0u8; 4096]).unwrap();
        assert!(check_ext4_magic(&bad).await.is_err());

        let tiny = dir.join("tiny.ext4");
        std::fs::write(&tiny, b"hi").unwrap();
        assert!(check_ext4_magic(&tiny).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end spool pipeline against the real zstd binary: compress a
    /// sparse pseudo-image, verify it, magic-check it compressed, decompress
    /// it back, and confirm holes survived the round trip (a restore that
    /// re-inflates disks would undo the whole thin-provisioning effort).
    #[tokio::test]
    async fn zstd_round_trip_preserves_magic_and_holes() {
        if run(Command::new("zstd").arg("--version"), Duration::from_secs(10)).await.is_err() {
            eprintln!("skipping: zstd not installed");
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("imgarchive-zstd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.ext4");
        // 8MB apparent, sparse: magic + a little data at the front, a hole,
        // data at the tail.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::File::create(&src).unwrap();
            let mut head = vec![0u8; 4096];
            head[EXT4_MAGIC_OFFSET as usize] = EXT4_MAGIC[0];
            head[EXT4_MAGIC_OFFSET as usize + 1] = EXT4_MAGIC[1];
            f.write_all(&head).unwrap();
            f.seek(SeekFrom::Start(8 * 1024 * 1024 - 4096)).unwrap();
            f.write_all(&[0xAB; 4096]).unwrap();
        }
        let src_alloc = std::fs::metadata(&src).unwrap().blocks() * 512;
        assert!(src_alloc < 8 * 1024 * 1024, "source should be sparse");

        let spool = dir.join("src.img.zst");
        run_ok(
            Command::new("zstd").args(["-q", "-f", "-3", "-T0", "-o"]).arg(&spool).arg(&src),
            "compress",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        run_ok(Command::new("zstd").args(["-q", "-t"]).arg(&spool), "verify", Duration::from_secs(60))
            .await
            .unwrap();
        check_compressed_ext4_magic(&spool).await.unwrap();

        let back = dir.join("back.ext4");
        run_ok(
            Command::new("zstd").args(["-q", "-d", "-f", "--sparse", "-o"]).arg(&back).arg(&spool),
            "decompress",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        check_ext4_magic(&back).await.unwrap();
        let md = std::fs::metadata(&back).unwrap();
        assert_eq!(md.len(), 8 * 1024 * 1024, "apparent size must round-trip");
        assert!(
            md.blocks() * 512 < 8 * 1024 * 1024,
            "holes must be reconstituted, not written as zeros ({} allocated)",
            md.blocks() * 512
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In-process S3 stub pinning the upload orchestration: what the server
    /// must see for a single PUT vs a multipart (initiate → parts → complete,
    /// with the manifest naming every ETag), and that a Complete answering
    /// 200-with-an-`<Error>`-body is treated as the failure it is.
    #[derive(Default)]
    struct StubState {
        puts: Vec<(String, usize)>,           // (query, body length)
        posts: Vec<(String, String)>,         // (query, body)
        fail_complete: bool,
        aborted: Vec<String>,
    }

    async fn spawn_stub(state: Arc<Mutex<StubState>>) -> String {
        use axum::extract::{RawQuery, State};
        use axum::routing::any;
        let app = axum::Router::new()
            .route(
                "/{*path}",
                any(
                    |State(st): State<Arc<Mutex<StubState>>>,
                     RawQuery(q): RawQuery,
                     req: axum::http::Request<axum::body::Body>| async move {
                        let q = q.unwrap_or_default();
                        let method = req.method().clone();
                        let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                            .await
                            .unwrap_or_default();
                        let mut st = st.lock().unwrap();
                        match method.as_str() {
                            "PUT" => {
                                st.puts.push((q.clone(), body.len()));
                                let n = st.puts.len();
                                axum::http::Response::builder()
                                    .header("etag", format!("\"etag-{n}\""))
                                    .body(axum::body::Body::empty())
                                    .unwrap()
                            }
                            "POST" if q.contains("uploads") && !q.contains("uploadId") => {
                                st.posts.push((q, String::new()));
                                axum::http::Response::new(axum::body::Body::from(
                                    "<InitiateMultipartUploadResult><UploadId>UP123</UploadId>\
                                     </InitiateMultipartUploadResult>",
                                ))
                            }
                            "POST" => {
                                let fail = st.fail_complete;
                                st.posts.push((q, String::from_utf8_lossy(&body).into_owned()));
                                axum::http::Response::new(axum::body::Body::from(if fail {
                                    "<Error><Code>InternalError</Code></Error>"
                                } else {
                                    "<CompleteMultipartUploadResult/>"
                                }))
                            }
                            "DELETE" => {
                                st.aborted.push(q);
                                axum::http::Response::builder()
                                    .status(204)
                                    .body(axum::body::Body::empty())
                                    .unwrap()
                            }
                            // HEADs from head_object during these tests.
                            _ => axum::http::Response::builder()
                                .status(404)
                                .body(axum::body::Body::empty())
                                .unwrap(),
                        }
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn stub_s3(endpoint: String) -> S3Config {
        S3Config {
            bucket: "wb".into(),
            prefix: "pg-vm-pool/".into(),
            region: "us-east-1".into(),
            discovered_region: Default::default(),
            endpoint: Some(endpoint),
            access_key_id: "AK".into(),
            secret_access_key: "sk".into(),
        }
    }

    #[tokio::test]
    async fn small_files_go_as_one_put() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let s3 = stub_s3(spawn_stub(state.clone()).await);
        let http = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("imgarchive-put-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("small.zst");
        std::fs::write(&f, vec![7u8; 1000]).unwrap();

        upload_file(&s3, &http, "pg-vm-pool/s.img.zst", &f, 1000, 100, 4096)
            .await
            .unwrap();
        let st = state.lock().unwrap();
        assert_eq!(st.puts.len(), 1, "one plain PUT");
        assert_eq!(st.puts[0].1, 1000);
        assert!(!st.puts[0].0.contains("partNumber"), "not a part upload");
        assert!(st.posts.is_empty(), "no multipart traffic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn large_files_go_multipart_with_a_complete_manifest() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let s3 = stub_s3(spawn_stub(state.clone()).await);
        let http = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("imgarchive-mp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big.zst");
        // 2.5 parts at a 1000-byte part size.
        std::fs::write(&f, vec![9u8; 2500]).unwrap();

        upload_file(&s3, &http, "pg-vm-pool/b.img.zst", &f, 2500, 1000, 1024)
            .await
            .unwrap();
        let st = state.lock().unwrap();
        let parts: Vec<_> = st.puts.iter().filter(|(q, _)| q.contains("partNumber")).collect();
        assert_eq!(parts.len(), 3, "1000+1000+500");
        assert_eq!(parts.iter().map(|(_, n)| n).sum::<usize>(), 2500);
        assert!(parts.iter().all(|(q, _)| q.contains("uploadId=UP123")));
        let complete = st.posts.iter().find(|(q, _)| q.contains("uploadId")).unwrap();
        for etag in ["etag-1", "etag-2", "etag-3"] {
            assert!(complete.1.contains(etag), "manifest must carry {etag}: {}", complete.1);
        }
        assert!(st.aborted.is_empty(), "nothing to abort on success");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn complete_with_error_body_fails_and_aborts() {
        let state = Arc::new(Mutex::new(StubState { fail_complete: true, ..Default::default() }));
        let s3 = stub_s3(spawn_stub(state.clone()).await);
        let http = reqwest::Client::new();
        let dir = std::env::temp_dir().join(format!("imgarchive-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("big.zst");
        std::fs::write(&f, vec![9u8; 2500]).unwrap();

        let err = upload_file(&s3, &http, "pg-vm-pool/b.img.zst", &f, 2500, 1000, 1024)
            .await
            .expect_err("a 200-with-<Error> Complete must fail the upload");
        assert!(format!("{err:#}").contains("completing multipart"), "unexpected error: {err:#}");
        let st = state.lock().unwrap();
        assert_eq!(st.aborted.len(), 1, "failed upload must be aborted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
