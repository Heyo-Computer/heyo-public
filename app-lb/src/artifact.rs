//! Pulling a deployment's content out of an artifact store.
//!
//! The store is [`artifacts`](https://github.com/sarocu/artifacts) — content
//! addressed, and already the thing that holds heyvm's base images (`art heyvm
//! import`). A deployment's `artifact` block names a store and a reference; this
//! module turns that pair into bytes on this host, and reports the digest they
//! came from.
//!
//! What those bytes become depends on the backend, and the two are different
//! enough to be worth naming:
//!
//! * **A managed (`vm`) deployment** gets an `<name>.ext4` in heyvm's image
//!   directory that `heyvmd` can boot. [`Puller::pull`].
//! * **A site** gets a `tar`/`tar.gz` bundle unpacked into `site.root`.
//!   [`Puller::pull_tree`], with the unpacking itself in [`crate::unpack`].
//!
//! Everything below the resolve step is shared, because the hard parts are the
//! same either way: turning a tag into a digest, getting the bytes across, and
//! proving they are the bytes that were asked for.
//!
//! Two transports, chosen by how `artifact.store` is spelled, because the two
//! situations are genuinely different rather than one being a fallback:
//!
//! * **A path** — a store on this host. Handed to `art heyvm materialize`,
//!   which skips the blob's holes instead of copying its zeros. The artifacts
//!   README measures this at 581 MiB written for a 20 GiB rootfs; nothing
//!   app-lb could implement over a socket competes with that, and the binary is
//!   already there if the store is.
//! * **A URL** — an `art serve` somewhere else. app-lb streams the blob itself.
//!   This is what lets one store feed a fleet, and it is how a host with no
//!   `art` binary and no local store still boots the same image.
//!
//! ## What this module insists on
//!
//! **The digest is verified before the bytes are used.** A rootfs is the thing
//! the kernel boots and a bundle is the thing that gets written across a
//! directory on this host. A truncated body, a proxy that helpfully transcoded
//! something, a store that answered the wrong blob — all of those produce a file
//! that is not what was asked for, and the only cheap way to notice is to hash
//! it and compare.
//!
//! Where that check happens differs by path, and it is worth being precise
//! because one of them is not free:
//!
//! * **Over the wire**, [`Puller::fetch_blob`] hashes the body as it lands, so
//!   nothing is ever written under a final name unverified.
//! * **From a local store**, `art heyvm materialize` hashes as it copies — but
//!   only on the copying path. `art get` *hardlinks* when it can, which writes
//!   no bytes and therefore hashes none, so [`verify_file`] does it here before
//!   a bundle is unpacked.
//!
//! **The image is named after the digest, so a re-pull is free.** `<base>-<12
//! hex>` is a pure function of the content, so the file being present is proof
//! the right bytes are on disk and the fetch can be skipped entirely. That
//! matters more here than for a build: rebuilding a Dockerfile is minutes of
//! CPU, but re-fetching a rootfs is gigabytes over a network somebody pays for.
//!
//! **Nothing is written under its final name until it is complete.** Everything
//! lands on a temporary file in the same directory and is renamed into place
//! once it has been verified and grown. heyvmd resolves an image by looking for
//! `<name>.ext4` and does not care how long it has been there, so a partial file
//! under the final name is a VM that boots halfway.

use crate::config::ArtifactSpec;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Filename of a rootfs inside an artifact manifest. Matches `ROOTFS_FILENAME`
/// in the artifacts crate, which in turn matches mvm-ctrl's
/// `driver/sync.rs`. A manifest with several entries — a sync bundle — is
/// resolved by finding this one.
const ROOTFS_FILENAME: &str = "rootfs.ext4";

/// A sha256 in the only spelling the store accepts.
const DIGEST_HEX_LEN: usize = 64;

/// How long to wait for the store to answer at all. Short, because this is a
/// connect and a response head, not a body.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a single read of the blob body may stall before the pull is called
/// dead. Deliberately *not* a whole-request timeout: a rootfs is gigabytes, and
/// any total ceiling large enough to allow a slow link is too large to notice a
/// hung one.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// What a *rootfs* pull did. Every field is reported on the job record, because
/// "which bytes are live" is the question a pull exists to answer.
#[derive(Debug, Clone)]
pub struct Pulled {
    /// The rootfs blob's digest — what `artifact.ref` resolved to.
    pub digest: String,
    /// The heyvm image name written into `vm.image`.
    pub image: String,
    pub path: PathBuf,
    /// Final size of the file on disk, after any growth.
    pub size: u64,
    /// Bytes transferred or copied. Zero when the image was already present.
    pub bytes_written: u64,
    /// Whether the fetch was skipped because the content-addressed image was
    /// already on disk.
    pub reused: bool,
}

/// What a *site* pull did. The tree counterpart of [`Pulled`], and deliberately
/// not the same type: a site produces no image name and no file, so half of
/// [`Pulled`] would be `None` here and the other half would need explaining.
#[derive(Debug, Clone)]
pub struct PulledTree {
    /// The bundle blob's digest — what `artifact.ref` resolved to, and what the
    /// marker beside the root now records.
    pub digest: String,
    /// The directory now serving it.
    pub root: PathBuf,
    /// Regular files unpacked.
    pub files: usize,
    /// Uncompressed bytes written into the tree.
    pub unpacked: u64,
    /// Bytes transferred from the store. Zero from a local store means the blob
    /// was hardlinked rather than copied, which is the normal case.
    pub bytes_written: u64,
    /// Whether the whole pull was skipped because this digest is already the one
    /// deployed at `root`.
    pub reused: bool,
}

/// Materializes a store's blobs onto this host: rootfs images into one
/// directory, site bundles into the directory each site is served from.
pub struct Puller {
    /// The `art` CLI, for the local-store path.
    art_bin: String,
    /// heyvm's Firecracker image directory. Everything is written here — both
    /// the temporary file and the final image — so the rename is same-filesystem
    /// and therefore atomic.
    ///
    /// A `Result` because resolving it can fail (see [`default_images_dir`]) and
    /// the failure must not stop app-lb from starting: an LB whose deployments
    /// all name prebuilt images has no use for this directory and should not
    /// refuse to boot over it. Carried this far so the error surfaces on the
    /// pull that needed it, with its own message intact.
    images_dir: Result<PathBuf, String>,
    /// `HOME` for the `art` child, when app-lb and heyvmd run as different
    /// users. `art` does not read `$HOME` for the store root (that comes from
    /// `--root`), but it does for `~/.artifacts` defaults and for anything the
    /// binary itself resolves, so it is passed for the same reason `heyvm` gets
    /// it.
    home: Option<String>,
    http: reqwest::Client,
}

impl Puller {
    pub fn new(art_bin: String, images_dir: Result<PathBuf, String>, home: Option<String>) -> Self {
        Self {
            art_bin,
            images_dir,
            home,
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn images_dir(&self) -> Result<&Path, String> {
        self.images_dir.as_deref().map_err(String::clone)
    }

    /// Resolve `spec.artifact_ref`, put the rootfs behind it in the image
    /// directory, and say what happened.
    ///
    /// `api_key` is the resolved value of `spec.auth`, already read out of the
    /// secret store — this module never touches secrets itself, so the value has
    /// exactly one way in and cannot be logged by accident.
    ///
    /// `log` receives progress lines. Errors are strings: they go straight onto
    /// a job record for a person to read.
    pub async fn pull(
        &self,
        deployment_id: &str,
        spec: &ArtifactSpec,
        api_key: Option<&str>,
        force: bool,
        log: &mut (dyn FnMut(String) + Send),
    ) -> Result<Pulled, String> {
        let dir = self.images_dir()?;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

        if spec.is_remote() {
            self.pull_remote(dir, deployment_id, spec, api_key, force, log).await
        } else {
            if api_key.is_some() {
                // Said rather than ignored: a key configured here is a
                // credential somebody believes is protecting something.
                log("artifact.auth is set on a local store; a store root is protected by \
                     file permissions, not by an API key, and the secret is unused"
                    .to_string());
            }
            self.pull_local(dir, deployment_id, spec, force, log).await
        }
    }

    // -- sites: a bundle unpacked into the served directory ------------------

    /// Resolve `spec.artifact_ref`, unpack the bundle behind it into
    /// `site.root`, and say what happened.
    ///
    /// The order is the point, and it is not the order the equivalent shell
    /// commands run in. The bundle is fetched, verified, unpacked *beside* the
    /// live tree and checked for the index the site will look for — and only
    /// then swapped in. Every way this can fail therefore fails with the
    /// previous site still serving, which is the one thing `git pull && npm run
    /// build && mv -T dist public` cannot promise.
    pub async fn pull_tree(
        &self,
        spec: &ArtifactSpec,
        site: &crate::config::SiteSpec,
        api_key: Option<&str>,
        force: bool,
        log: &mut (dyn FnMut(String) + Send),
    ) -> Result<PulledTree, String> {
        let root = PathBuf::from(site.root.trim());
        let remote = spec.is_remote();
        let base = spec.store.trim().trim_end_matches('/').to_string();

        // Resolve first: it is one round trip, and it is what makes the reuse
        // check below possible without moving any bytes.
        let (digest, size) = if remote {
            self.resolve_remote(&base, &spec.artifact_ref, api_key, None)
                .await?
        } else {
            if api_key.is_some() {
                log("artifact.auth is set on a local store; a store root is protected by \
                     file permissions, not by an API key, and the secret is unused"
                    .to_string());
            }
            self.stat_local(&base, &spec.artifact_ref).await?
        };
        log(format!(
            "{} resolves to {digest} ({}) {} {base}",
            spec.artifact_ref,
            human(size),
            if remote { "at" } else { "in" },
        ));

        // The site counterpart of "the image is already on disk". The digest is
        // recorded beside the root rather than encoded in a filename, because a
        // directory cannot be content-addressed by its name the way an image
        // file is.
        if !force && crate::unpack::deployed_digest(&root).as_deref() == Some(digest.as_str()) {
            log(format!(
                "{} is already serving this digest; nothing to unpack",
                root.display()
            ));
            return Ok(PulledTree {
                digest,
                root,
                files: 0,
                unpacked: 0,
                bytes_written: 0,
                reused: true,
            });
        }

        // Beside the root, so a bundle too large for `/tmp` is not a surprise
        // and the local store's hardlink lands on the filesystem the files are
        // going to anyway.
        let bundle = Scratch::new(crate::unpack::scratch_path(&root, "bundle")?);
        let bytes_written = if remote {
            self.fetch_blob(&base, &digest, api_key, bundle.path(), size, log)
                .await?
        } else {
            let written = self.get_local(&base, &digest, bundle.path()).await?;
            // `art get` hardlinks when it can, and a hardlink hashes nothing.
            verify_file(bundle.path(), &digest).await?;
            log(format!(
                "{} from {base} ({})",
                human(size),
                if written == 0 { "hardlinked" } else { "copied" }
            ));
            written
        };

        // Unpacking is synchronous, and a bundle can be hundreds of megabytes
        // of gzip: on the job task directly it would stall every other future
        // on this runtime thread for the duration.
        let strip = spec.strip();
        let index = site.index.trim().to_string();
        let (root, unpacked) = {
            let root = root.clone();
            let bundle_path = bundle.path().to_path_buf();
            tokio::task::spawn_blocking(move || {
                let (staged, unpacked) = crate::unpack::stage(&root, &bundle_path, strip)?;
                crate::unpack::verify_index(staged.dir(), &index, strip)?;
                staged.commit()?;
                Ok::<_, String>((root, unpacked))
            })
            .await
            .map_err(|e| format!("the unpack task did not finish: {e}"))??
        };

        log(format!(
            "unpacked {} file{} ({}) into {}",
            unpacked.files,
            if unpacked.files == 1 { "" } else { "s" },
            human(unpacked.bytes),
            root.display(),
        ));

        // After the swap, so the marker can only ever describe a tree that is
        // actually in place.
        //
        // If it cannot be written the *old* marker must go, and that matters
        // more than it looks: a marker left claiming the digest this deploy
        // replaced would make a later pull of that digest skip its work and
        // report "already serving" over a tree holding something else. Losing
        // the marker only costs the next pull its shortcut, so the failure is
        // logged rather than raised — the deploy itself succeeded.
        if let Err(e) = crate::unpack::record_digest(&root, &digest) {
            crate::unpack::forget_digest(&root);
            log(format!("{e}; the next pull will unpack again rather than skip"));
        }

        Ok(PulledTree {
            digest,
            root,
            files: unpacked.files,
            unpacked: unpacked.bytes,
            bytes_written,
            reused: false,
        })
    }

    /// `art get` — the blob's bytes at a path, hardlinked if the filesystem
    /// allows it. Returns what was actually written, which is `0` for a link.
    async fn get_local(&self, root: &str, digest: &str, dest: &Path) -> Result<u64, String> {
        // `art get` refuses to clobber, and a crashed run can leave one behind.
        let _ = tokio::fs::remove_file(dest).await;
        let mut cmd = self.art(root);
        cmd.arg("get").arg(digest).arg("-o").arg(dest);
        let out = self.run(cmd).await?;
        let v: serde_json::Value = serde_json::from_str(&out)
            .map_err(|e| format!("`art get` produced output that is not JSON: {e}"))?;
        Ok(v.get("bytesWritten").and_then(|b| b.as_u64()).unwrap_or(0))
    }

    // -- remote: art serve over HTTP ---------------------------------------

    async fn pull_remote(
        &self,
        dir: &Path,
        deployment_id: &str,
        spec: &ArtifactSpec,
        api_key: Option<&str>,
        force: bool,
        log: &mut (dyn FnMut(String) + Send),
    ) -> Result<Pulled, String> {
        let base = spec.store.trim().trim_end_matches('/');
        let (digest, expected_size) = self
            .resolve_remote(base, &spec.artifact_ref, api_key, Some(ROOTFS_FILENAME))
            .await?;
        log(format!(
            "{} resolves to {digest} ({}) at {base}",
            spec.artifact_ref,
            human(expected_size),
        ));

        let image = spec.image_for(deployment_id, &digest);
        let dest = dir.join(format!("{image}.ext4"));

        if !force
            && let Some(size) = usable_existing(&dest, expected_size, spec.grow_gb).await
        {
            log(format!(
                "{} is already on disk with this digest; nothing to fetch",
                dest.display()
            ));
            return Ok(Pulled {
                digest,
                image,
                path: dest,
                size,
                bytes_written: 0,
                reused: true,
            });
        }

        let tmp = TempImage::new(dir, &image);
        let written = self
            .fetch_blob(base, &digest, api_key, tmp.path(), expected_size, log)
            .await?;
        let size = self.finish(tmp, &dest, spec.grow_gb, log).await?;

        Ok(Pulled {
            digest,
            image,
            path: dest,
            size,
            bytes_written: written,
            reused: false,
        })
    }

    /// Turn a tag or a digest into the rootfs blob's digest and size.
    ///
    /// `GET /manifests/{ref}` is asked first for either spelling, because that
    /// is the route that resolves a tag *and* the route that describes what a
    /// manifest digest contains. Only if that fails and the reference is itself
    /// 64 hex characters is it treated as naming the blob directly — the store
    /// permits tagging a blob, and `HEAD /blobs/{digest}` is how to find out.
    async fn resolve_remote(
        &self,
        base: &str,
        reference: &str,
        api_key: Option<&str>,
        expected: Option<&str>,
    ) -> Result<(String, u64), String> {
        let url = format!("{base}/manifests/{reference}");
        let manifest_err = match self.get(&url, api_key).await {
            Ok(resp) if resp.status().is_success() => {
                let m: Manifest = resp
                    .json()
                    .await
                    .map_err(|e| format!("the manifest at {url} was not readable: {e}"))?;
                return blob_entry(&m, reference, expected);
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                format!("GET {url} answered {status}{}", detail(&body))
            }
            Err(e) => format!("GET {url} failed: {e}"),
        };

        // Not a manifest. It may still name a blob directly: `art put --tag web
        // rootfs.ext4` points a tag straight at the blob rather than at a
        // manifest describing it, and that image is just as bootable as one
        // `art heyvm import` put in. A digest is its own answer; a tag has to be
        // looked up in the tag list, because the store exposes no
        // `GET /tags/{name}`.
        let digest = if is_digest(reference) {
            reference.to_string()
        } else {
            match self.tag_digest(base, reference, api_key).await {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return Err(format!(
                        "{manifest_err}. The store has no tag called {reference:?} — push one \
                         with `serverctl artifact push --tag {reference}`, or \
                         `art tag {reference} <digest>` on the store itself"
                    ));
                }
                Err(e) => return Err(format!("{manifest_err}, and {e}")),
            }
        };

        let url = format!("{base}/blobs/{digest}");
        let resp = self
            .head(&url, api_key)
            .await
            .map_err(|e| format!("HEAD {url} failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(format!(
                "{manifest_err}, and HEAD {url} answered {status} — the store has neither a \
                 manifest nor a blob for {reference:?}"
            ));
        }
        let size = resp
            .content_length()
            .ok_or_else(|| format!("HEAD {url} answered without a Content-Length"))?;
        Ok((digest, size))
    }

    /// What a tag points at, via the tag listing.
    ///
    /// The store has `GET /tags` but no `GET /tags/{name}`, so this reads the
    /// whole list and filters. That is fine at the sizes involved — a store
    /// holds tens of images — and it is only reached when the tag turned out
    /// not to name a manifest.
    async fn tag_digest(
        &self,
        base: &str,
        tag: &str,
        api_key: Option<&str>,
    ) -> Result<Option<String>, String> {
        let url = format!("{base}/tags");
        let resp = self
            .get(&url, api_key)
            .await
            .map_err(|e| format!("GET {url} failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} answered {status}{}", detail(&body)));
        }
        let tags: Vec<TagEntry> = resp
            .json()
            .await
            .map_err(|e| format!("the tag list at {url} was not readable: {e}"))?;
        Ok(tags
            .into_iter()
            .find(|t| t.tag == tag)
            .map(|t| t.digest)
            .filter(|d| is_digest(d)))
    }

    /// Stream `GET /blobs/{digest}` into `dest`, hashing as it lands.
    ///
    /// The hash is the point. Everything downstream — heyvmd booting it, the
    /// image name claiming to be content-addressed — assumes the bytes on disk
    /// are the bytes the digest names, and this is the only place that is ever
    /// checked on the wire path.
    async fn fetch_blob(
        &self,
        base: &str,
        digest: &str,
        api_key: Option<&str>,
        dest: &Path,
        expected_size: u64,
        log: &mut (dyn FnMut(String) + Send),
    ) -> Result<u64, String> {
        let url = format!("{base}/blobs/{digest}");
        let resp = self
            .get(&url, api_key)
            .await
            .map_err(|e| format!("GET {url} failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GET {url} answered {status}{}", detail(&body)));
        }

        log(format!("fetching {} from {url}", human(expected_size)));
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| format!("could not create {}: {e}", dest.display()))?;
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                format!(
                    "the transfer failed after {} of {}: {e}",
                    human(written),
                    human(expected_size)
                )
            })?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("writing {}: {e}", dest.display()))?;
            written += chunk.len() as u64;
        }
        // Flush before the digest is pronounced good: a buffered tail that never
        // reached the kernel would make the check describe memory, not the file
        // that is about to be renamed into place and booted.
        file.flush()
            .await
            .map_err(|e| format!("flushing {}: {e}", dest.display()))?;
        file.sync_all()
            .await
            .map_err(|e| format!("syncing {}: {e}", dest.display()))?;
        drop(file);

        let actual = hex(&hasher.finalize());
        if actual != digest {
            return Err(format!(
                "the store answered {written} bytes that hash to {actual}, but {digest} was \
                 asked for — this is a corrupted or substituted rootfs and it has not been \
                 kept"
            ));
        }
        Ok(written)
    }

    // -- local: a store root on this host ----------------------------------

    async fn pull_local(
        &self,
        dir: &Path,
        deployment_id: &str,
        spec: &ArtifactSpec,
        force: bool,
        log: &mut (dyn FnMut(String) + Send),
    ) -> Result<Pulled, String> {
        let root = spec.store.trim();

        // Resolve first, so an image already on disk costs one `art stat`
        // instead of a full materialization. `stat` reports the blob a
        // reference resolves to, which is exactly the name the image gets.
        match self.stat_local(root, &spec.artifact_ref).await {
            Ok((digest, size)) => {
                log(format!(
                    "{} resolves to {digest} ({}) in {root}",
                    spec.artifact_ref,
                    human(size)
                ));
                let image = spec.image_for(deployment_id, &digest);
                let dest = dir.join(format!("{image}.ext4"));
                if !force
                    && let Some(size) = usable_existing(&dest, size, spec.grow_gb).await
                {
                    log(format!(
                        "{} is already on disk with this digest; nothing to materialize",
                        dest.display()
                    ));
                    return Ok(Pulled {
                        digest,
                        image,
                        path: dest,
                        size,
                        bytes_written: 0,
                        reused: true,
                    });
                }
            }
            Err(e) => {
                // `art stat` refuses a manifest with several entries, which a
                // sync bundle has; `art heyvm materialize` resolves those by
                // picking the rootfs. So a failure here is not fatal — it just
                // costs the shortcut.
                log(format!("could not resolve {} up front ({e}); materializing to find out",
                    spec.artifact_ref));
            }
        }

        let tmp = TempImage::new(dir, &format!("pull-{}", sanitize(deployment_id)));
        let out = self.materialize(root, spec, tmp.path()).await?;
        let image = spec.image_for(deployment_id, &out.digest);
        let dest = dir.join(format!("{image}.ext4"));
        log(format!(
            "materialized {} ({} written, {})",
            out.digest,
            human(out.bytes_written),
            out.method,
        ));

        // Growth is `art`'s job on this path — it was passed --grow-gb — so the
        // rename is all that is left.
        let size = self.finish(tmp, &dest, None, log).await?;
        Ok(Pulled {
            digest: out.digest,
            image,
            path: dest,
            size,
            bytes_written: out.bytes_written,
            reused: false,
        })
    }

    /// `art stat` — the digest a reference resolves to, without doing any work.
    async fn stat_local(&self, root: &str, reference: &str) -> Result<(String, u64), String> {
        let mut cmd = self.art(root);
        cmd.arg("stat").arg(reference);
        let out = self.run(cmd).await?;
        let v: serde_json::Value = serde_json::from_str(&out)
            .map_err(|e| format!("`art stat` produced output that is not JSON: {e}"))?;
        let digest = v
            .get("digest")
            .and_then(|d| d.as_str())
            .ok_or("`art stat` reported no digest")?
            .to_string();
        let size = v.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        Ok((digest, size))
    }

    async fn materialize(
        &self,
        root: &str,
        spec: &ArtifactSpec,
        dest: &Path,
    ) -> Result<Materialized, String> {
        let mut cmd = self.art(root);
        cmd.arg("heyvm")
            .arg("materialize")
            .arg(&spec.artifact_ref)
            .arg(dest);
        if let Some(gb) = spec.grow_gb {
            cmd.arg("--grow-gb").arg(gb.to_string());
        }
        let out = self.run(cmd).await?;
        let v: serde_json::Value = serde_json::from_str(&out)
            .map_err(|e| format!("`art heyvm materialize` produced output that is not JSON: {e}"))?;
        Ok(Materialized {
            digest: v
                .get("digest")
                .and_then(|d| d.as_str())
                .ok_or("`art heyvm materialize` reported no digest")?
                .to_string(),
            bytes_written: v.get("bytesWritten").and_then(|b| b.as_u64()).unwrap_or(0),
            method: v
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("copy")
                .to_string(),
        })
    }

    /// An `art` invocation against one store root, in JSON mode.
    ///
    /// `--root` rather than `ART_ROOT` so the store is visible in the process
    /// list next to the command that used it, and so an `ART_ROOT` inherited
    /// from app-lb's own environment cannot quietly redirect a pull.
    fn art(&self, root: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.art_bin);
        cmd.arg("--root").arg(root).arg("--json");
        if let Some(home) = &self.home {
            cmd.env("HOME", home);
        }
        cmd
    }

    async fn run(&self, mut cmd: tokio::process::Command) -> Result<String, String> {
        let bin = self.art_bin.clone();
        let out = cmd
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => format!(
                    "{bin} is not on app-lb's PATH. A deployment pulling from a local store \
                     needs the `art` CLI on this host — install it, or set APP_LB_ART_BIN to \
                     its path, or point artifact.store at an `art serve` URL instead"
                ),
                _ => format!("could not run {bin}: {e}"),
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            return Err(format!(
                "{bin} exited {}{}",
                out.status.code().unwrap_or(-1),
                if msg.is_empty() {
                    String::new()
                } else {
                    format!(": {msg}")
                }
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    // -- shared ------------------------------------------------------------

    /// Grow if asked, then rename into place. Returns the final size.
    async fn finish(
        &self,
        tmp: TempImage,
        dest: &Path,
        grow_gb: Option<u64>,
        log: &mut (dyn FnMut(String) + Send),
    ) -> Result<u64, String> {
        if let Some(gb) = grow_gb {
            let target = gb * 1024 * 1024 * 1024;
            let current = tokio::fs::metadata(tmp.path())
                .await
                .map_err(|e| format!("stat {}: {e}", tmp.path().display()))?
                .len();
            if target > current {
                // Sparse: `set_len` is ftruncate, so the extra length costs no
                // blocks until something writes to them. heyvm runs the
                // `resize2fs` that lets the guest filesystem use the room.
                let f = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(tmp.path())
                    .await
                    .map_err(|e| format!("open {} to grow: {e}", tmp.path().display()))?;
                f.set_len(target)
                    .await
                    .map_err(|e| format!("growing {} to {gb} GiB: {e}", tmp.path().display()))?;
                log(format!("grown from {} to {gb} GiB", human(current)));
            }
        }

        let size = tokio::fs::metadata(tmp.path())
            .await
            .map_err(|e| format!("stat {}: {e}", tmp.path().display()))?
            .len();
        tmp.commit(dest).await?;
        Ok(size)
    }

    async fn get(&self, url: &str, api_key: Option<&str>) -> reqwest::Result<reqwest::Response> {
        self.authed(self.http.get(url), api_key).send().await
    }

    async fn head(&self, url: &str, api_key: Option<&str>) -> reqwest::Result<reqwest::Response> {
        self.authed(self.http.head(url), api_key).send().await
    }

    /// `Authorization: Bearer`, which the store accepts alongside `X-Api-Key`.
    /// Bearer is the one that survives a proxy in between without a config
    /// change, since a custom header is the thing an intermediary strips.
    fn authed(&self, req: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
        match api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }
}

struct Materialized {
    digest: String,
    bytes_written: u64,
    method: String,
}

/// Just enough of the store's manifest to find a rootfs in it.
///
/// Deliberately not the `artifacts` crate's `Manifest`: taking that dependency
/// would pull an axum tree and a `libc` syscall layer into app-lb to read four
/// fields, and this side only ever *reads* a manifest. `#[serde(default)]` on
/// everything so a manifest gaining a field is not a failed pull.
#[derive(serde::Deserialize)]
struct Manifest {
    #[serde(default)]
    entries: Vec<ManifestEntry>,
}

/// One row of `GET /tags`.
#[derive(serde::Deserialize)]
struct TagEntry {
    tag: String,
    digest: String,
}

#[derive(serde::Deserialize)]
struct ManifestEntry {
    #[serde(default)]
    name: String,
    digest: String,
    #[serde(default)]
    size: u64,
}

/// The blob a manifest is standing in for: the entry with the expected name, or
/// the only one.
///
/// Both rules are needed. `art heyvm import` writes a single-entry manifest and
/// names it `rootfs.ext4`, but a manifest holding one unrelated blob is still
/// unambiguous, and `art put --tag` produces exactly that. Anything else is
/// reported rather than guessed — picking an entry out of a bundle by position
/// would deploy whichever file happened to sort first.
///
/// `expected` is what the caller is looking for by name, and there is one only
/// for a rootfs: a site bundle has no filename convention, so it relies on the
/// single-entry rule alone.
fn blob_entry(m: &Manifest, reference: &str, expected: Option<&str>) -> Result<(String, u64), String> {
    let entry = expected
        .and_then(|name| m.entries.iter().find(|e| e.name == name))
        .or(if m.entries.len() == 1 {
            m.entries.first()
        } else {
            None
        });
    match entry {
        Some(e) if is_digest(&e.digest) => Ok((e.digest.clone(), e.size)),
        Some(e) => Err(format!(
            "the manifest for {reference:?} names {:?} with digest {:?}, which is not a sha256",
            e.name, e.digest
        )),
        None if m.entries.is_empty() => {
            Err(format!("the manifest for {reference:?} is empty"))
        }
        None => Err(format!(
            "the manifest for {reference:?} holds {} entries{}: {}. Tag the blob you want \
             directly — a manifest with several entries does not say which one this \
             deployment is for",
            m.entries.len(),
            match expected {
                Some(name) => format!(" and none is called {name}"),
                None => String::new(),
            },
            m.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", "),
        )),
    }
}

/// Whether an image already on disk can stand in for a fetch.
///
/// The filename carries the digest, so the file existing is already strong
/// evidence about its *content*. What the name cannot describe is its length,
/// and two things make that matter:
///
/// * A previous pull that died mid-write leaves a file shorter than its blob.
/// * `grow_gb` is not part of the name — it is about the guest, not the content
///   — so a spec that gains one would otherwise keep reusing the ungrown image
///   and silently give the guest none of the room it asked for.
///
/// So the bar is the larger of the blob and the grow target, and anything under
/// it is re-fetched (and then grown) rather than accepted.
async fn usable_existing(dest: &Path, expected_size: u64, grow_gb: Option<u64>) -> Option<u64> {
    let meta = tokio::fs::metadata(dest).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let required = expected_size.max(grow_gb.map(|gb| gb * 1024 * 1024 * 1024).unwrap_or(0));
    let size = meta.len();
    (required == 0 || size >= required).then_some(size)
}

/// A partial image, removed unless it is committed.
///
/// The name is dotted and carries the pid, so a crash leaves something
/// identifiable in the image directory rather than a plausible-looking
/// `<name>.ext4` that `heyvm mvm images` would list and someone would boot. It
/// is in the image directory rather than `/tmp` so the commit is a rename on one
/// filesystem — across filesystems it would be a second full copy of a rootfs,
/// and a non-atomic one.
struct TempImage {
    path: PathBuf,
    committed: bool,
}

impl TempImage {
    fn new(dir: &Path, stem: &str) -> Self {
        Self {
            path: dir.join(format!(".{stem}.{}.ext4.part", std::process::id())),
            committed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    async fn commit(mut self, dest: &Path) -> Result<(), String> {
        tokio::fs::rename(&self.path, dest).await.map_err(|e| {
            format!("could not move {} into place at {}: {e}", self.path.display(), dest.display())
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TempImage {
    fn drop(&mut self) {
        if !self.committed {
            // Synchronous on purpose: `Drop` cannot await, and leaving a
            // multi-gigabyte partial file behind to be tidied up "later" means
            // the next pull on a full disk fails for the wrong reason.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A file removed when it goes out of scope, however the scope ends.
///
/// Unlike [`TempImage`] there is no commit: a bundle is read and discarded, and
/// what survives a site pull is the unpacked tree rather than the archive.
struct Scratch(PathBuf);

impl Scratch {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Hash a file already on disk and confirm it is the blob that was asked for.
///
/// The wire path does this as the bytes land ([`Puller::fetch_blob`]); this is
/// for the local path, where `art get` hardlinks the blob and so writes — and
/// hashes — nothing at all. Cheap next to the unpack it precedes, and it is the
/// only thing standing between a store somebody has written to and a directory
/// this host serves.
async fn verify_file(path: &Path, digest: &str) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("could not read {} back to verify it: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut read: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }

    let actual = hex(&hasher.finalize());
    if actual != digest {
        return Err(format!(
            "the store produced {read} bytes that hash to {actual}, but {digest} was asked \
             for — this is a corrupted or substituted bundle and it has not been unpacked"
        ));
    }
    Ok(())
}

/// Where a pulled image is written, resolved the way mvm-ctrl resolves it.
///
/// `$MVM_DATA_DIR` first, then `<home>/.heyo` — and `home` is the *daemon's*
/// home, which is why `APP_LB_HEYVM_HOME` feeds it. The daemon looks for
/// `<data dir>/images/firecracker/<name>.ext4` and nowhere else, so a pull that
/// writes under app-lb's home when the daemon runs as another user succeeds and
/// then boots nothing.
pub fn default_images_dir(heyvm_home: Option<&str>) -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("MVM_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir).join("images").join("firecracker"));
    }
    let home = heyvm_home
        .map(str::to_string)
        .or_else(|| std::env::var("HOME").ok())
        .filter(|h| !h.trim().is_empty())
        .ok_or(
            "cannot tell where heyvm keeps its images: neither MVM_DATA_DIR nor HOME is set. \
             Set APP_LB_IMAGES_DIR to the daemon's images/firecracker directory",
        )?;
    Ok(PathBuf::from(home)
        .join(".heyo")
        .join("images")
        .join("firecracker"))
}

fn is_digest(s: &str) -> bool {
    s.len() == DIGEST_HEX_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A deployment id is only constrained by the route table, and this ends up in a
/// filename.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(64)
        .collect()
}

/// An error body worth appending, or nothing. The store answers errors as JSON
/// with an `error` key; anything else is passed through trimmed.
fn detail(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return String::new();
    }
    let msg = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| Some(v.get("error")?.as_str()?.to_string()))
        .unwrap_or_else(|| body.chars().take(200).collect());
    format!(": {msg}")
}

/// Byte counts a person can read at a glance. Shared with the job records,
/// which report the same numbers.
pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(entries: &[(&str, &str, u64)]) -> Manifest {
        Manifest {
            entries: entries
                .iter()
                .map(|(name, digest, size)| ManifestEntry {
                    name: name.to_string(),
                    digest: digest.to_string(),
                    size: *size,
                })
                .collect(),
        }
    }

    fn d(prefix: &str) -> String {
        let mut s = prefix.to_string();
        while s.len() < DIGEST_HEX_LEN {
            s.push('a');
        }
        s
    }

    /// What a rootfs pull looks for.
    fn rootfs_entry(m: &Manifest, reference: &str) -> Result<(String, u64), String> {
        blob_entry(m, reference, Some(ROOTFS_FILENAME))
    }

    #[test]
    fn a_heyvm_import_manifest_resolves_to_its_rootfs() {
        let m = manifest(&[("rootfs.ext4", &d("c74abee2"), 21_474_836_480)]);
        assert_eq!(
            rootfs_entry(&m, "debian-hermes").unwrap(),
            (d("c74abee2"), 21_474_836_480)
        );
    }

    #[test]
    fn a_bundle_resolves_by_name_not_by_position() {
        // `manifest.json` sorts first; picking entry zero would boot it.
        let m = manifest(&[
            ("manifest.json", &d("1111"), 512),
            ("rootfs.ext4", &d("2222"), 4096),
        ]);
        assert_eq!(rootfs_entry(&m, "bundle").unwrap().0, d("2222"));
    }

    #[test]
    fn a_lone_entry_resolves_whatever_it_is_called() {
        // `art put --tag` names the entry after the file it came from.
        let m = manifest(&[("web.ext4", &d("3333"), 100)]);
        assert_eq!(rootfs_entry(&m, "web").unwrap().0, d("3333"));
    }

    /// A site bundle has no filename convention — `dist.tgz`, `site.tar`,
    /// whatever the CI job called it — so it resolves on the single-entry rule
    /// alone, and `rootfs.ext4` means nothing special to it.
    #[test]
    fn a_site_bundle_resolves_without_an_expected_name() {
        let m = manifest(&[("dist.tgz", &d("6666"), 4_194_304)]);
        assert_eq!(
            blob_entry(&m, "marketing-live", None).unwrap(),
            (d("6666"), 4_194_304)
        );

        // And with several entries it is ambiguous rather than guessed, even
        // though one of them is a rootfs: nothing says a site wants that one.
        let m = manifest(&[("rootfs.ext4", &d("7777"), 1), ("dist.tgz", &d("8888"), 2)]);
        let err = blob_entry(&m, "mixed", None).unwrap_err();
        assert!(err.contains("dist.tgz"), "{err}");
        assert!(!err.contains("none is called"), "no name was expected: {err}");
    }

    #[test]
    fn an_ambiguous_manifest_is_reported_with_what_it_holds() {
        let m = manifest(&[("a.img", &d("4444"), 1), ("b.img", &d("5555"), 2)]);
        let err = rootfs_entry(&m, "pair").unwrap_err();
        assert!(err.contains("a.img"), "{err}");
        assert!(err.contains("b.img"), "{err}");
        assert!(err.contains(ROOTFS_FILENAME), "it should name what it looked for: {err}");
    }

    #[test]
    fn an_empty_manifest_says_so_rather_than_panicking() {
        assert!(rootfs_entry(&manifest(&[]), "empty").is_err());
        assert!(blob_entry(&manifest(&[]), "empty", None).is_err());
    }

    #[test]
    fn a_manifest_entry_whose_digest_is_not_a_sha256_is_refused() {
        let m = manifest(&[("rootfs.ext4", "not-a-digest", 1)]);
        assert!(rootfs_entry(&m, "bad").is_err());
    }

    #[tokio::test]
    async fn a_bundle_that_is_not_the_blob_that_was_asked_for_is_refused() {
        let dir = std::env::temp_dir().join(format!("applb-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle");
        std::fs::write(&path, b"hello").unwrap();

        // sha256("hello")
        let real = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_file(&path, real).await.is_ok());

        let err = verify_file(&path, &d("dead")).await.unwrap_err();
        assert!(err.contains("has not been unpacked"), "{err}");
        assert!(err.contains(real), "it should report what it actually got: {err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn digests_are_lowercase_hex_of_exactly_the_right_length() {
        assert!(is_digest(&d("c74abee2ce84")));
        assert!(!is_digest(&d("C74ABEE2")), "uppercase is a second spelling");
        assert!(!is_digest("c74abee2"), "too short");
        assert!(!is_digest(&(d("c74a") + "a")), "too long");
    }

    #[test]
    fn a_temp_image_is_removed_unless_it_is_committed() {
        let dir = std::env::temp_dir().join(format!("applb-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = {
            let tmp = TempImage::new(&dir, "web");
            std::fs::write(tmp.path(), b"partial").unwrap();
            tmp.path().to_path_buf()
        };
        assert!(!path.exists(), "a dropped temp image left {} behind", path.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_committed_temp_image_lands_under_its_final_name() {
        let dir = std::env::temp_dir().join(format!("applb-art-commit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("web-c74abee2ce84.ext4");
        let tmp = TempImage::new(&dir, "web");
        std::fs::write(tmp.path(), b"rootfs").unwrap();
        let tmp_path = tmp.path().to_path_buf();
        tmp.commit(&dest).await.unwrap();
        assert!(dest.exists());
        assert!(!tmp_path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_image_shorter_than_its_blob_is_not_reused() {
        let dir = std::env::temp_dir().join(format!("applb-art-reuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web.ext4");
        std::fs::write(&path, b"truncated").unwrap();

        assert_eq!(usable_existing(&path, 9, None).await, Some(9));
        // A grown image is longer than the blob and still usable.
        assert_eq!(usable_existing(&path, 4, None).await, Some(9));
        // A short one is the remains of a failed write.
        assert_eq!(usable_existing(&path, 4096, None).await, None);
        assert_eq!(usable_existing(&dir.join("absent.ext4"), 1, None).await, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_image_that_predates_a_grow_gb_is_not_reused() {
        // `grow_gb` is about the guest, not the content, so it is not in the
        // image's name. Without this the image would keep being reused at its
        // old length and the guest would silently get none of the room the
        // spec now asks for.
        let dir = std::env::temp_dir().join(format!("applb-art-grow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web.ext4");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();

        assert_eq!(usable_existing(&path, 4096, None).await, Some(4096));
        assert_eq!(
            usable_existing(&path, 4096, Some(1)).await,
            None,
            "a 4 KiB image cannot stand in for one asked to be 1 GiB"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_images_dir_follows_the_daemons_home_not_app_lbs() {
        // Only meaningful when MVM_DATA_DIR is unset, which is the normal case.
        if std::env::var("MVM_DATA_DIR").is_ok() {
            return;
        }
        let dir = default_images_dir(Some("/home/heyo")).unwrap();
        assert_eq!(dir, PathBuf::from("/home/heyo/.heyo/images/firecracker"));
    }

    #[test]
    fn an_error_body_is_unwrapped_when_it_is_the_stores_json() {
        assert_eq!(detail(r#"{"error":"blob not found"}"#), ": blob not found");
        assert_eq!(detail("plain text"), ": plain text");
        assert_eq!(detail("  "), "");
    }

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(21_474_836_480), "20.0 GiB");
    }
}
