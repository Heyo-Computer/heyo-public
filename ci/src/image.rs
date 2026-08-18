//! Building a runner's VM image from a Dockerfile in the submitted tree.
//!
//! The build itself runs **on the runner, by its daemon**: `ci` uploads the
//! Dockerfile and its build context to `POST /images/build`, and heyvmd runs
//! the same `docker build → docker export → mke2fs` pipeline `heyvm mvm build`
//! runs locally, writing `~/.heyo/images/firecracker/{name}.ext4` into the
//! host's own catalog. `ci` never parses the Dockerfile and never boots a
//! builder VM — docker's semantics apply in full (multi-stage, `COPY --from`,
//! `ADD`, everything), and the host's docker layer cache makes a rebuild of a
//! mostly-unchanged Dockerfile incremental.
//!
//! Two consequences of `docker export` are inherited from the pipeline and are
//! the image author's to handle, exactly as they are for a hand-built image:
//!
//! - **OCI metadata is discarded** — `ENV`, `CMD`, `ENTRYPOINT` do not survive
//!   into the rootfs. An environment variable that steps need must be written
//!   to `/etc/profile.d` by a `RUN` (steps run under `sh -lc`, which reads it).
//! - **The VM boots `init=/init.sh`** and must print `HEYVM_READY`. An image
//!   without an init script builds fine and then fails every boot; see
//!   `deploy/image/init.sh` for the contract.
//!
//! ## The name is the cache key
//!
//! An image is named `ci-img-<12 hex>`, hashed over the Dockerfile bytes,
//! every file in the build context, and the size override. Identical inputs
//! name an image the host already has; any change names one it does not.
//! There is no invalidation step and nothing to remember to bump — "reused
//! until cache busted" is what content addressing does on its own. The daemon
//! returning `ready` for a name that already exists is what makes two jobs
//! racing to build the same image safe: the loser is told it is done, which
//! is the outcome it wanted.
//!
//! ## What is left in the catalog
//!
//! Images are not swept. A rootfs is expensive to rebuild and cheap to keep,
//! and unlike a pooled VM it holds no state from the run that made it — the
//! blunt cleanup is `rm ~/.heyo/images/firecracker/ci-img-*.ext4` on the host,
//! after which the next job rebuilds. `ci_vm_image` is this orchestrator's
//! record of what it has put on each host, and a create that fails because the
//! file went away forgets the row and rebuilds, the same way `acquire_vm`
//! already recovers from a pooled VM the daemon lost.

use crate::vm::{ImageBuild, VmSpec};
use heyo_sdk::{HeyoClient, HeyoClientOptions, RequestOptions};
use reqwest::Method;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// Hex characters kept from the digest, matching the VM pool's fingerprint.
const FINGERPRINT_LEN: usize = 12;

/// How long a build may hold its catalog claim without renewal.
///
/// Longer than the VM lease because it bounds something slower: a whole image
/// build, not a boot. A claim that lapses under a live build only costs a
/// duplicate build request, which the daemon's own idempotency then collapses.
pub const BUILD_LEASE: Duration = Duration::from_secs(30 * 60);

// ---- what gets uploaded ------------------------------------------------

/// A resolved `vm.build`: the name the image will have, and the bytes that
/// name was derived from — which are also the bytes the daemon builds from,
/// so the name can never describe inputs other than the ones sent.
#[derive(Debug)]
pub struct BuildPlan {
    pub name: String,
    pub dockerfile: String,
    /// Gzipped tar of the context directory. `None` when the context is empty,
    /// which is legal: a Dockerfile that copies nothing needs no context.
    pub context_tar_gz: Option<Vec<u8>>,
}

/// Resolve a job's `vm.build` against the checkout: read the Dockerfile, pack
/// the context, and derive the image name from their content.
///
/// The fingerprint hashes the Dockerfile's raw bytes rather than anything
/// parsed — `ci` no longer understands Dockerfiles, docker does — so a comment
/// edit does rebuild. That trade is deliberate: the daemon's docker layer
/// cache makes the rebuild cheap, and a parser kept only to avoid it would be
/// a second implementation of Dockerfile semantics waiting to disagree with
/// the first.
///
/// The whole context directory is hashed and shipped, as `docker build` ships
/// it. `.dockerignore` is honoured by docker *during* the build but not by
/// this hash, so editing an ignored file rebuilds needlessly — mildly wasteful,
/// never wrong.
pub fn plan_for(
    build: &ImageBuild,
    spec: &VmSpec,
    workspace: &Path,
) -> Result<BuildPlan, ImageError> {
    let dockerfile_path = workspace.join(&build.dockerfile);
    let dockerfile =
        std::fs::read_to_string(&dockerfile_path).map_err(|e| ImageError::NoDockerfile {
            path: build.dockerfile.clone(),
            reason: e.to_string(),
        })?;

    let context_dir = workspace.join(build.context_dir());
    let mut files = Vec::new();
    if context_dir.is_dir() {
        walk(&context_dir, &context_dir, &mut files)?;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut h = Sha256::new();
    // Versioned, so a change to what the hash covers renames every image
    // rather than colliding new inputs onto old files.
    h.update(b"ci-image-v2\0");
    h.update(dockerfile.as_bytes());
    h.update([0]);
    // The size override changes the ext4 the daemon writes, and the driver
    // decides which catalog the name resolves against.
    h.update(format!("{:?}\0{}\0", spec.driver, build.size_mb.unwrap_or(0)).as_bytes());
    for (rel, bytes) in &files {
        h.update(rel.as_bytes());
        h.update([0]);
        let mut fh = Sha256::new();
        fh.update(bytes);
        h.update(fh.finalize());
    }
    let name = format!("ci-img-{}", &hex::encode(h.finalize())[..FINGERPRINT_LEN]);

    let context_tar_gz = if files.is_empty() {
        None
    } else {
        Some(pack_context(&files)?)
    };

    Ok(BuildPlan {
        name,
        dockerfile,
        context_tar_gz,
    })
}

/// Read every file under `dir`, as context-relative paths.
///
/// Symlinks are skipped rather than followed: a link out of the context would
/// ship a file the fingerprint never hashed — and the tree this walks was
/// submitted by whoever ran `git submit`.
fn walk(dir: &Path, context: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), ImageError> {
    let entries = std::fs::read_dir(dir).map_err(|e| ImageError::UnreadableContext {
        path: rel_of(dir, context),
        reason: e.to_string(),
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(&path, context, out)?;
        } else if meta.is_file() {
            let bytes = std::fs::read(&path).map_err(|e| ImageError::UnreadableContext {
                path: rel_of(&path, context),
                reason: e.to_string(),
            })?;
            out.push((rel_of(&path, context), bytes));
        }
    }
    Ok(())
}

fn rel_of(path: &Path, context: &Path) -> String {
    path.strip_prefix(context)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Tar and gzip the context files for upload.
fn pack_context(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, ImageError> {
    use std::io::Write;
    let mut ar = tar::Builder::new(Vec::new());
    for (rel, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        ar.append_data(&mut header, rel, bytes.as_slice())
            .map_err(|e| ImageError::Pack(e.to_string()))?;
    }
    let tar_bytes = ar
        .into_inner()
        .map_err(|e| ImageError::Pack(e.to_string()))?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes)
        .map_err(|e| ImageError::Pack(e.to_string()))?;
    gz.finish().map_err(|e| ImageError::Pack(e.to_string()))
}

// ---- driving the daemon ------------------------------------------------

/// `POST /images/build` / `GET /images/build/status` response body. One shape
/// for both: the daemon answers `{status, name}` plus `error` on failure and
/// `size_bytes` when a ready image's size is known.
#[derive(Debug, Deserialize)]
struct BuildStatus {
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    size_bytes: Option<u64>,
}

/// What a finished remote build reports back.
pub struct Built {
    pub size_bytes: u64,
    /// A short transcript of what happened, for the log attached to the job.
    pub log: String,
}

/// Ask `runner`'s daemon to build `plan` and wait for the result.
///
/// `options` point at the runner's tunnel, exactly as VM operations do. The
/// call is fire-and-poll: the POST returns immediately and status is polled
/// until `ready` or `failed`, bounded by `deadline`. `renew` is called on each
/// poll so the catalog claim in Postgres outlives a long build — a claim that
/// lapsed mid-build would invite a second job to start a duplicate.
///
/// An `unknown` status after a POST means the daemon restarted or the failure
/// state aged out; the POST is simply re-sent — it is idempotent, and if the
/// image landed before the restart the re-POST answers `ready`.
pub async fn build_remote<F, Fut>(
    options: HeyoClientOptions,
    plan: &BuildPlan,
    size_mb: Option<u64>,
    deadline: Duration,
    mut renew: F,
) -> Result<Built, ImageError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use base64::Engine;
    use std::fmt::Write as _;

    const POLL: Duration = Duration::from_secs(3);

    let client = HeyoClient::new(options).map_err(|e| ImageError::Daemon {
        what: "building a client for the runner",
        source: e,
    })?;

    let body = serde_json::json!({
        "name": plan.name,
        "dockerfile": plan.dockerfile,
        "context_tar_gz": plan.context_tar_gz.as_deref().map(|b| {
            base64::engine::general_purpose::STANDARD.encode(b)
        }),
        "size_mb": size_mb,
    });

    let mut log = String::new();
    let _ = writeln!(
        log,
        "[ci] building image {} on the runner: {} byte Dockerfile, {} byte context{}",
        plan.name,
        plan.dockerfile.len(),
        plan.context_tar_gz.as_ref().map(|c| c.len()).unwrap_or(0),
        match size_mb {
            Some(mb) => format!(", rootfs {mb} MB"),
            None => ", rootfs auto-sized".to_string(),
        }
    );

    let post = |client: &HeyoClient, body: serde_json::Value| {
        let client = client.clone();
        async move {
            client
                .request::<BuildStatus>(
                    Method::POST,
                    "/images/build",
                    Some(&body),
                    RequestOptions {
                        // The context rides in this request; give a large one
                        // time to cross the tunnel. The build itself is not
                        // waited on here — the route returns on acceptance.
                        timeout: Some(Duration::from_secs(120)),
                        query: Vec::new(),
                    },
                )
                .await
        }
    };

    let first = post(&client, body.clone())
        .await
        .map_err(|e| ImageError::Daemon {
            what: "starting the image build",
            source: e,
        })?;
    if first.status == "ready" {
        // The daemon already had it — the whole point of content-hashed names.
        let _ = writeln!(log, "[ci] the runner already has this image");
        return Ok(Built {
            size_bytes: first.size_bytes.unwrap_or(0),
            log,
        });
    }

    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(POLL).await;
        renew().await;

        let status: BuildStatus = client
            .request(
                Method::GET,
                "/images/build/status",
                None::<&()>,
                RequestOptions {
                    timeout: Some(Duration::from_secs(30)),
                    query: vec![("name".to_string(), plan.name.clone())],
                },
            )
            .await
            .map_err(|e| ImageError::Daemon {
                what: "polling the image build",
                source: e,
            })?;

        match status.status.as_str() {
            "ready" => {
                let _ = writeln!(
                    log,
                    "[ci] image {} is ready after {:?}",
                    plan.name,
                    started.elapsed()
                );
                return Ok(Built {
                    size_bytes: status.size_bytes.unwrap_or(0),
                    log,
                });
            }
            "failed" => {
                let detail = status
                    .error
                    .unwrap_or_else(|| "the daemon reported no reason".to_string());
                let _ = writeln!(log, "[ci] build failed: {detail}");
                return Err(ImageError::Build {
                    name: plan.name.clone(),
                    detail,
                });
            }
            "building" => {}
            // The daemon restarted, or a terminal state aged out of its
            // tracker. Re-POST: idempotent, and it re-answers `ready` if the
            // image landed before the restart.
            _ => {
                let _ = writeln!(log, "[ci] build state lost on the runner; re-requesting");
                let re = post(&client, body.clone())
                    .await
                    .map_err(|e| ImageError::Daemon {
                        what: "re-requesting the image build",
                        source: e,
                    })?;
                if re.status == "ready" {
                    return Ok(Built {
                        size_bytes: re.size_bytes.unwrap_or(0),
                        log,
                    });
                }
            }
        }

        if started.elapsed() >= deadline {
            return Err(ImageError::BuildTimeout {
                name: plan.name.clone(),
                after: deadline,
            });
        }
    }
}

// ---- the catalog -------------------------------------------------------

/// One image this orchestrator has built, or is building, on one runner.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    pub runner_hd_id: String,
    pub status: String,
    pub workflow_id: String,
    pub built_by_job: Option<String>,
    pub size_bytes: i64,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub ready_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl CatalogEntry {
    fn from_row(r: &sqlx::postgres::PgRow) -> Self {
        Self {
            name: r.get("name"),
            runner_hd_id: r.get("runner_hd_id"),
            status: r.get("status"),
            workflow_id: r.get("workflow_id"),
            built_by_job: r.get("built_by_job"),
            size_bytes: r.get("size_bytes"),
            error: r.get("error"),
            created_at: r.get("created_at"),
            ready_at: r.get("ready_at"),
        }
    }
}

/// What this orchestrator has put in each runner's image catalog.
///
/// The daemon has no route to list its images — `heyvm mvm images` reads the
/// directory locally — so "does this host already have it" cannot be asked over
/// the tunnel. This table answers it instead, on the same reasoning
/// `ci_vm_pool` is the source of truth for VMs: a record kept here is one query,
/// and drift is self-healing because a create against a missing image fails and
/// forgets the row.
#[derive(Clone)]
pub struct Catalog {
    db: PgPool,
}

/// What [`Catalog::claim`] found.
pub enum Claim {
    /// The runner has it. Use it.
    Ready,
    /// Nobody is building it and this caller now owns doing so.
    Build,
    /// Somebody else is building it; wait rather than build a second copy.
    InProgress,
}

impl Catalog {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    /// Decide, in one statement, whether to use, build, or wait.
    ///
    /// The insert is what makes it a decision rather than a read: two jobs
    /// racing arrive at the same primary key, and exactly one of them inserts.
    /// The other is told `InProgress` and waits — which is the difference
    /// between one image build on a host and one per concurrent job.
    ///
    /// A claim whose lease has lapsed is taken over rather than waited on, so a
    /// dispatcher that died mid-build does not block the image for ever.
    pub async fn claim(
        &self,
        name: &str,
        runner: &str,
        workflow_id: &str,
        job_id: &str,
        lease: Duration,
    ) -> Result<Claim, ImageError> {
        let row = sqlx::query(
            "INSERT INTO ci_vm_image
                (name, runner_hd_id, workflow_id, status, built_by_job, leased_until)
             VALUES ($1,$2,$3,'building',$4, now() + make_interval(secs => $5))
             ON CONFLICT (name, runner_hd_id) DO UPDATE
                SET status='building', built_by_job=$4, error=NULL,
                    leased_until=now() + make_interval(secs => $5)
              WHERE ci_vm_image.status <> 'ready'
                AND (ci_vm_image.leased_until IS NULL OR ci_vm_image.leased_until < now())
             RETURNING status",
        )
        .bind(name)
        .bind(runner)
        .bind(workflow_id)
        .bind(job_id)
        .bind(lease.as_secs() as f64)
        .fetch_optional(&self.db)
        .await
        .map_err(ImageError::sql)?;

        if row.is_some() {
            return Ok(Claim::Build);
        }
        // The upsert did nothing, so a row exists that it was not allowed to
        // take: either it is ready, or somebody's lease is still live.
        match self.status_of(name, runner).await? {
            Some(s) if s == "ready" => Ok(Claim::Ready),
            _ => Ok(Claim::InProgress),
        }
    }

    pub async fn status_of(&self, name: &str, runner: &str) -> Result<Option<String>, ImageError> {
        let row =
            sqlx::query("SELECT status FROM ci_vm_image WHERE name = $1 AND runner_hd_id = $2")
                .bind(name)
                .bind(runner)
                .fetch_optional(&self.db)
                .await
                .map_err(ImageError::sql)?;
        Ok(row.map(|r| r.get("status")))
    }

    /// Hold a claim while a long build runs.
    pub async fn renew(&self, name: &str, runner: &str, lease: Duration) -> Result<(), ImageError> {
        sqlx::query(
            "UPDATE ci_vm_image SET leased_until = now() + make_interval(secs => $3)
              WHERE name = $1 AND runner_hd_id = $2 AND status = 'building'",
        )
        .bind(name)
        .bind(runner)
        .bind(lease.as_secs() as f64)
        .execute(&self.db)
        .await
        .map_err(ImageError::sql)?;
        Ok(())
    }

    pub async fn mark_ready(
        &self,
        name: &str,
        runner: &str,
        size_bytes: u64,
    ) -> Result<(), ImageError> {
        sqlx::query(
            "UPDATE ci_vm_image
                SET status='ready', ready_at=now(), leased_until=NULL, error=NULL,
                    size_bytes=$3
              WHERE name = $1 AND runner_hd_id = $2",
        )
        .bind(name)
        .bind(runner)
        .bind(size_bytes as i64)
        .execute(&self.db)
        .await
        .map_err(ImageError::sql)?;
        Ok(())
    }

    /// Record a failed build, keeping the row so the page can say what happened.
    ///
    /// `failed` rather than deleted, and unleased: the next job to want this
    /// image takes the claim and tries again, which is right for a build that
    /// failed on a transient apt mirror — while the row still carries the
    /// reason the last attempt gave.
    pub async fn mark_failed(
        &self,
        name: &str,
        runner: &str,
        error: &str,
    ) -> Result<(), ImageError> {
        sqlx::query(
            "UPDATE ci_vm_image
                SET status='failed', leased_until=NULL, error=$3
              WHERE name = $1 AND runner_hd_id = $2 AND status <> 'ready'",
        )
        .bind(name)
        .bind(runner)
        .bind(error)
        .execute(&self.db)
        .await
        .map_err(ImageError::sql)?;
        Ok(())
    }

    /// Drop the record of an image the runner turns out not to have.
    pub async fn forget(&self, name: &str, runner: &str) -> Result<(), ImageError> {
        sqlx::query("DELETE FROM ci_vm_image WHERE name = $1 AND runner_hd_id = $2")
            .bind(name)
            .bind(runner)
            .execute(&self.db)
            .await
            .map_err(ImageError::sql)?;
        Ok(())
    }

    /// Every image on the runners this instance serves.
    pub async fn inventory(&self, runners: &[String]) -> Result<Vec<CatalogEntry>, ImageError> {
        if runners.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT * FROM ci_vm_image
              WHERE runner_hd_id = ANY($1)
              ORDER BY runner_hd_id, created_at DESC",
        )
        .bind(runners)
        .fetch_all(&self.db)
        .await
        .map_err(ImageError::sql)?;
        Ok(rows.iter().map(CatalogEntry::from_row).collect())
    }
}

#[derive(Debug)]
pub enum ImageError {
    NoDockerfile {
        path: String,
        reason: String,
    },
    UnreadableContext {
        path: String,
        reason: String,
    },
    Pack(String),
    /// The daemon could not be asked, or stopped answering. The source is kept
    /// typed so a transport-level failure — the tunnel, not the build — can be
    /// told from the daemon actually refusing.
    Daemon {
        what: &'static str,
        source: heyo_sdk::HeyoError,
    },
    /// The daemon ran the build and it failed — a Dockerfile problem, named.
    Build {
        name: String,
        detail: String,
    },
    BuildTimeout {
        name: String,
        after: Duration,
    },
    Sql(String),
    /// Somebody else's build did not finish inside the window this job could
    /// wait for it.
    WaitTimeout {
        name: String,
        waited: Duration,
    },
}

impl ImageError {
    fn sql(e: sqlx::Error) -> Self {
        Self::Sql(e.to_string())
    }
}

impl ImageError {
    /// True when the failure was reaching the daemon at all — see
    /// [`crate::vm::is_transport`]. A build the daemon refused or failed is
    /// never this; those answers arrived.
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Daemon { source, .. } if crate::vm::is_transport(source))
    }
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDockerfile { path, reason } => write!(
                f,
                "vm.build.dockerfile {path:?} could not be read from the submitted tree: {reason}"
            ),
            Self::UnreadableContext { path, reason } => {
                write!(f, "build context file {path:?} could not be read: {reason}")
            }
            Self::Pack(e) => write!(f, "could not pack the build context: {e}"),
            Self::Daemon { what, source } => write!(f, "{what}: {source}"),
            Self::Build { name, detail } => {
                write!(f, "the runner could not build image {name}: {detail}")
            }
            Self::BuildTimeout { name, after } => write!(
                f,
                "the runner did not finish building image {name} within {after:?}"
            ),
            Self::Sql(e) => write!(f, "database error: {e}"),
            Self::WaitTimeout { name, waited } => write!(
                f,
                "another job has been building image {name} on this runner for {waited:?} and \
                 has not finished. This job gave up waiting rather than building a second copy; \
                 it will be retried."
            ),
        }
    }
}

impl std::error::Error for ImageError {}

#[cfg(test)]
mod tests {
    use super::*;
    use heyo_sdk::{SandboxDriver, SandboxSize};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn spec() -> VmSpec {
        VmSpec {
            driver: SandboxDriver::Firecracker,
            image: None,
            build: None,
            size_class: Some(SandboxSize::Medium),
            disk_size_gb: Some(20),
            working_directory: None,
            env_vars: BTreeMap::new(),
            setup_hooks: vec![],
            cache_key_files: vec![],
            reuse: true,
            ttl_seconds: None,
        }
    }

    fn build(dockerfile: &str) -> ImageBuild {
        ImageBuild {
            dockerfile: dockerfile.into(),
            context: None,
            size_mb: None,
        }
    }

    fn ws(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ci-img-{}", crate::vm::new_id()));
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole cache story in one test: the name is the content, so an
    /// unchanged Dockerfile reuses and any change rebuilds.
    #[test]
    fn the_image_name_is_the_hash_of_the_dockerfile_and_its_context() {
        let files = [
            (
                "img/Dockerfile",
                "FROM debian\nCOPY marker.txt /etc/marker\n",
            ),
            ("img/marker.txt", "one\n"),
        ];
        let w = ws(&files);
        let b = build("img/Dockerfile");

        let first = plan_for(&b, &spec(), &w).unwrap();
        assert!(first.name.starts_with("ci-img-"), "{}", first.name);
        assert_eq!(first.name.len(), "ci-img-".len() + FINGERPRINT_LEN);
        assert_eq!(
            first.name,
            plan_for(&b, &spec(), &w).unwrap().name,
            "the same inputs must name the same image, or nothing is ever reused"
        );
        // The Dockerfile itself is part of the context dir here, and the
        // context rides along for the daemon to build from.
        assert!(first.context_tar_gz.is_some());

        // A context file changing is a different image.
        std::fs::write(w.join("img/marker.txt"), "two\n").unwrap();
        let after_file = plan_for(&b, &spec(), &w).unwrap();
        assert_ne!(
            first.name, after_file.name,
            "a changed context file must bust the cache"
        );

        // So is the Dockerfile changing — including only a comment, because
        // the hash is over raw bytes now that docker owns the semantics.
        std::fs::write(
            w.join("img/Dockerfile"),
            "# c\nFROM debian\nCOPY marker.txt /etc/marker\n",
        )
        .unwrap();
        assert_ne!(after_file.name, plan_for(&b, &spec(), &w).unwrap().name);

        // And so is the size override: a different ext4 is a different image.
        let mut sized = build("img/Dockerfile");
        sized.size_mb = Some(6144);
        assert_ne!(
            plan_for(&sized, &spec(), &w).unwrap().name,
            plan_for(&build("img/Dockerfile"), &spec(), &w)
                .unwrap()
                .name
        );

        std::fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn a_dockerfile_with_no_context_files_ships_none() {
        let w = ws(&[]);
        std::fs::write(w.join("Dockerfile"), "FROM debian\nRUN true\n").unwrap();
        // Context defaults to the Dockerfile's directory — which here contains
        // only the Dockerfile itself, so it *is* shipped (docker would too).
        let plan = plan_for(&build("Dockerfile"), &spec(), &w).unwrap();
        assert!(plan.context_tar_gz.is_some());
        std::fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn a_directory_deep_in_the_context_still_busts_the_cache() {
        let w = ws(&[
            ("img/Dockerfile", "FROM debian\nCOPY . /app\n"),
            ("img/nested/deep/b.txt", "two"),
        ]);
        let b = build("img/Dockerfile");
        let before = plan_for(&b, &spec(), &w).unwrap();
        std::fs::write(w.join("img/nested/deep/b.txt"), "changed").unwrap();
        assert_ne!(before.name, plan_for(&b, &spec(), &w).unwrap().name);
        std::fs::remove_dir_all(&w).ok();
    }

    #[test]
    fn a_missing_dockerfile_names_the_path() {
        let w = ws(&[]);
        let e = plan_for(&build("absent/Dockerfile"), &spec(), &w).unwrap_err();
        assert!(e.to_string().contains("absent/Dockerfile"), "{e}");
        std::fs::remove_dir_all(&w).ok();
    }

    /// `heyvm mvm build -f x/Dockerfile` defaults its context to `x`, and a
    /// workflow that says only `dockerfile:` should mean the same thing.
    #[test]
    fn the_context_defaults_to_the_dockerfiles_directory() {
        let b = build("deploy/image/Dockerfile");
        assert_eq!(b.context_dir(), "deploy/image");
        assert_eq!(build("Dockerfile").context_dir(), ".");
        let mut b = build("deploy/image/Dockerfile");
        b.context = Some("deploy".into());
        assert_eq!(b.context_dir(), "deploy");
    }

    /// The packed context round-trips through tar+gzip, which is what the
    /// daemon unpacks on the other side.
    #[test]
    fn the_context_archive_round_trips() {
        let w = ws(&[
            ("img/Dockerfile", "FROM debian\n"),
            ("img/a.txt", "alpha"),
            ("img/sub/b.txt", "beta"),
        ]);
        let plan = plan_for(&build("img/Dockerfile"), &spec(), &w).unwrap();
        let gz = plan.context_tar_gz.expect("context");

        let mut seen = std::collections::BTreeMap::new();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::Cursor::new(gz)));
        for entry in ar.entries().unwrap() {
            use std::io::Read;
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let mut body = String::new();
            entry.read_to_string(&mut body).unwrap();
            seen.insert(path, body);
        }
        assert_eq!(seen.get("a.txt").map(String::as_str), Some("alpha"));
        assert_eq!(seen.get("sub/b.txt").map(String::as_str), Some("beta"));
        assert!(seen.contains_key("Dockerfile"));
        std::fs::remove_dir_all(&w).ok();
    }

    // ---- the catalog ----------------------------------------------------
    //
    //   CI_TEST_DATABASE_URL=... cargo test -- --ignored image::

    async fn test_catalog() -> Catalog {
        let url = std::env::var("CI_TEST_DATABASE_URL").expect("CI_TEST_DATABASE_URL");
        let dir = std::env::temp_dir().join(format!("ci-img-logs-{}", crate::vm::new_id()));
        let store = crate::store::Store::connect(&url, dir).await.unwrap();
        store
            .migrate(Path::new("migrations"))
            .await
            .expect("migrations");
        Catalog::new(store.pool().clone())
    }

    /// A distinct runner per test so concurrent runs never contend.
    fn runner_id() -> String {
        format!("hd-{}", crate::vm::new_id().replace('-', ""))
    }

    const LEASE: Duration = Duration::from_secs(600);
    const LAPSED: Duration = Duration::from_secs(0);

    /// The whole point of the table: one build per host, and every later job
    /// finds it ready instead of rebuilding a multi-gigabyte rootfs.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn one_job_builds_an_image_and_the_rest_wait_then_reuse_it() {
        let c = test_catalog().await;
        let runner = runner_id();
        let name = "ci-img-aaaaaaaaaaaa";

        assert!(matches!(
            c.claim(name, &runner, "wf", "job-1", LEASE).await.unwrap(),
            Claim::Build
        ));
        // A second job arriving mid-build must not start its own.
        assert!(matches!(
            c.claim(name, &runner, "wf", "job-2", LEASE).await.unwrap(),
            Claim::InProgress
        ));

        c.mark_ready(name, &runner, 4096).await.unwrap();
        for job in ["job-2", "job-3"] {
            assert!(
                matches!(
                    c.claim(name, &runner, "wf", job, LEASE).await.unwrap(),
                    Claim::Ready
                ),
                "{job} must reuse the image rather than rebuild it"
            );
        }

        // A ready image is never taken over, however old its row.
        assert!(matches!(
            c.claim(name, &runner, "wf", "job-4", LAPSED).await.unwrap(),
            Claim::Ready
        ));

        let seen = c.inventory(std::slice::from_ref(&runner)).await.unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].status, "ready");
        assert_eq!(seen[0].size_bytes, 4096);
        c.forget(name, &runner).await.unwrap();
    }

    /// An image is a file on one host's disk, so one runner having it says
    /// nothing about another.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn an_image_on_one_runner_is_not_an_image_on_another() {
        let c = test_catalog().await;
        let (a, b) = (runner_id(), runner_id());
        let name = "ci-img-bbbbbbbbbbbb";

        c.claim(name, &a, "wf", "job-1", LEASE).await.unwrap();
        c.mark_ready(name, &a, 1).await.unwrap();

        assert!(matches!(
            c.claim(name, &b, "wf", "job-2", LEASE).await.unwrap(),
            Claim::Build,
        ));
        assert_eq!(c.inventory(&[a.clone()]).await.unwrap().len(), 1);
        c.forget(name, &a).await.unwrap();
        c.forget(name, &b).await.unwrap();
    }

    /// A dispatcher that died mid-build must not block the image for ever, and
    /// a failed build must be retried by the next job that wants it — with the
    /// last reason still on the row for whoever looks.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_lapsed_or_failed_build_is_taken_over_by_the_next_job() {
        let c = test_catalog().await;
        let runner = runner_id();
        let name = "ci-img-cccccccccccc";

        // A holder that stopped renewing.
        c.claim(name, &runner, "wf", "dead-job", LAPSED)
            .await
            .unwrap();
        assert!(matches!(
            c.claim(name, &runner, "wf", "job-2", LEASE).await.unwrap(),
            Claim::Build,
        ));
        // And renewing keeps it held against a third.
        c.renew(name, &runner, LEASE).await.unwrap();
        assert!(matches!(
            c.claim(name, &runner, "wf", "job-3", LEASE).await.unwrap(),
            Claim::InProgress
        ));

        c.mark_failed(name, &runner, "apt-get exited 100")
            .await
            .unwrap();
        let seen = c.inventory(std::slice::from_ref(&runner)).await.unwrap();
        assert_eq!(seen[0].status, "failed");
        assert_eq!(seen[0].error.as_deref(), Some("apt-get exited 100"));

        // Retried rather than stuck: a mirror that was down is worth another go.
        assert!(matches!(
            c.claim(name, &runner, "wf", "job-4", LEASE).await.unwrap(),
            Claim::Build
        ));
        c.forget(name, &runner).await.unwrap();
        assert!(c.inventory(&[runner]).await.unwrap().is_empty());
    }
}
