//! Unpacking a site's artifact bundle into the directory it is served from.
//!
//! The site counterpart of materializing a rootfs: [`crate::artifact`] resolves
//! a reference to a blob and gets the bytes onto this host, and this module
//! turns those bytes into the directory `site.root` names. A bundle is a `tar`,
//! optionally gzipped, which is what `tar czf dist.tgz -C dist .` produces and
//! what `art put` will have stored verbatim.
//!
//! ## The swap
//!
//! Unpacking straight into `site.root` would serve a half-written tree for the
//! duration, and leave one behind if the unpack failed halfway. So the new tree
//! is built beside the old one and moved in at the end:
//!
//! ```text
//! /srv/marketing/.public.incoming.4171   <- unpacked here
//! /srv/marketing/public                  <- still serving the old tree
//! ```
//!
//! Both live in `site.root`'s **parent**, so the move is a rename within one
//! filesystem. Across filesystems it would be a second full copy, and a
//! non-atomic one.
//!
//! Two renames are needed rather than one — the old tree out, the new tree in —
//! so there is a window, bounded by two `rename(2)` calls on the same directory,
//! in which the root does not exist and a request in flight gets a 404. That is
//! the same window `mv -T dist public` has and the reason this is not simply
//! left to the `update` commands: here the previous tree is kept until the new
//! one is in place, so a failed second rename is put back rather than lost.
//!
//! ## What a bundle is not allowed to contain
//!
//! Its contents are attacker-controlled in the sense that matters — whoever can
//! write to the store decides what lands on this host — so entries are refused
//! rather than sanitized:
//!
//! * **Anything that escapes the staging directory.** An absolute path or a
//!   `..` component is refused outright. Quietly dropping the `..` would turn a
//!   traversal into a successful write to a different file.
//! * **Symlinks and hardlinks.** A built static site has no need for either, and
//!   allowing them means defending against the classic tar attack of unpacking a
//!   link to `/etc` and then writing "through" it in a later entry.
//!   [`crate::site`] would refuse to *serve* an escaping symlink, but that is a
//!   check at read time and this is a write to the host.
//! * **Devices, fifos and sockets.** Nothing a web server serves.
//!
//! Permissions are not taken from the archive either. Files land under app-lb's
//! umask, so a bundle cannot ship something setuid or group-writable.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Gzip's magic number. Sniffed rather than trusted to a filename, because a
/// blob in the store has no name — it is addressed by its digest.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// What an unpack produced, for the job record.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unpacked {
    /// Regular files written. Directories are not counted: a bundle's directory
    /// entries are an artifact of how it was rolled, and "412 files" is the
    /// number that answers "did this unpack what I think it did?".
    pub files: usize,
    /// Total bytes written, uncompressed.
    pub bytes: u64,
}

/// A new tree, unpacked and waiting beside the one it replaces.
///
/// Removed on drop unless [`commit`](Self::commit) is called, so every early
/// return between staging and the swap cleans up after itself.
#[derive(Debug)]
pub struct Staged {
    root: PathBuf,
    staging: PathBuf,
    committed: bool,
}

impl Staged {
    /// Where the new tree is, so a caller can inspect it *before* it goes live.
    /// This is the point of staging: `verify_site` runs against this directory,
    /// and a bundle missing its index is refused while the old tree is still
    /// serving.
    pub fn dir(&self) -> &Path {
        &self.staging
    }

    /// Move the new tree into place, keeping the old one until that has
    /// succeeded.
    pub fn commit(mut self) -> Result<(), String> {
        let previous = scratch(&self.root, "previous")?;
        // A leftover from a crashed run would make the rename below fail, and
        // it describes a tree nobody is serving.
        let _ = std::fs::remove_dir_all(&previous);

        let had_previous = match std::fs::rename(&self.root, &previous) {
            Ok(()) => true,
            // First deploy: there is nothing to move out of the way.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(format!(
                    "could not move the current site out of {} : {e}",
                    self.root.display()
                ));
            }
        };

        if let Err(e) = std::fs::rename(&self.staging, &self.root) {
            let failed = format!(
                "could not move the new site into {}: {e}",
                self.root.display()
            );
            if !had_previous {
                return Err(failed);
            }
            // The root is currently missing and the site is down. Putting the
            // old tree back is the only useful thing left to do, and whether it
            // worked changes what the operator has to do next.
            return Err(match std::fs::rename(&previous, &self.root) {
                Ok(()) => format!("{failed}. The previous site has been put back"),
                Err(restore) => format!(
                    "{failed}. Worse, the previous site could not be put back either \
                     ({restore}) — it is at {} and {} does not exist",
                    previous.display(),
                    self.root.display()
                ),
            });
        }

        self.committed = true;
        if had_previous {
            // Best effort: the new site is live and serving either way, so a
            // failure here is disk to reclaim, not a broken deploy.
            if let Err(e) = std::fs::remove_dir_all(&previous) {
                tracing::warn!(
                    path = %previous.display(),
                    error = %e,
                    "could not remove the previous site tree",
                );
            }
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.staging);
        }
    }
}

/// Unpack `archive` into a staging directory beside `root`.
///
/// **Blocking.** Decompression and the write are synchronous, and a bundle can
/// be hundreds of megabytes — call it from `spawn_blocking`, not from the job
/// task directly.
///
/// `strip` drops that many leading path components from every entry, exactly as
/// `tar --strip-components` does; an entry with no components left is skipped
/// rather than being an error, which is what makes `strip: 1` work on a bundle
/// whose first entry is the bare directory `dist/`.
pub fn stage(root: &Path, archive: &Path, strip: usize) -> Result<(Staged, Unpacked), String> {
    let parent = root.parent().ok_or_else(|| {
        format!(
            "site.root {} has no parent directory, so there is nowhere to unpack beside it",
            root.display()
        )
    })?;
    // Refused rather than created, for the same reason `update.working_dir` is:
    // a typo that silently created a directory tree and deployed into it is
    // worse than an error. The root itself may be absent — that is a first
    // deploy, and the swap creates it.
    if !parent.is_dir() {
        return Err(format!(
            "{} does not exist on this host (app-lb runs as {}), so there is nowhere to \
             unpack {}",
            parent.display(),
            crate::jobs::whoami(),
            root.display()
        ));
    }

    let staging = scratch(root, "incoming")?;
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir(&staging)
        .map_err(|e| format!("could not create {}: {e}", staging.display()))?;

    // Constructed before the first write, so a failure anywhere below removes
    // the staging directory on the way out.
    let staged = Staged {
        root: root.to_path_buf(),
        staging,
        committed: false,
    };

    let unpacked = extract(archive, staged.dir(), strip)?;
    if unpacked.files == 0 {
        return Err(format!(
            "the bundle unpacked to no files at all{} — is it a tar (or tar.gz) of the \
             built site?",
            match strip {
                0 => String::new(),
                n => format!(" with strip_components: {n}"),
            }
        ));
    }
    Ok((staged, unpacked))
}

/// A dotted sibling of `root`, named `.{root}.{what}`.
///
/// Dotted so a crash leaves something identifiable next to the site rather than
/// a plausible-looking directory somebody would mistake for content, and beside
/// the root rather than inside it because everything inside is served.
fn sibling(root: &Path, what: &str) -> Result<PathBuf, String> {
    let parent = root
        .parent()
        .ok_or_else(|| format!("site.root {} has no parent directory", root.display()))?;
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("site.root {} does not end in a usable name", root.display()))?;
    Ok(parent.join(format!(".{name}.{what}")))
}

/// A sibling belonging to *this run*, so two app-lb processes sharing a host
/// cannot collide mid-swap and a crashed run's leftovers are attributable.
fn scratch(root: &Path, what: &str) -> Result<PathBuf, String> {
    sibling(root, &format!("{what}.{}", std::process::id()))
}

/// Where the digest of the currently-deployed bundle is recorded. Outlives the
/// process that wrote it, so no pid.
fn marker(root: &Path) -> Result<PathBuf, String> {
    sibling(root, "artifact")
}

/// The digest of the bundle currently unpacked at `root`, if app-lb put it
/// there and the tree is still present.
///
/// The tree is checked too: the marker alone would claim a deploy that somebody
/// has since `rm -rf`'d, and a pull that skipped its own work on that basis
/// would report success over an empty site.
pub fn deployed_digest(root: &Path) -> Option<String> {
    if !root.is_dir() {
        return None;
    }
    let recorded = std::fs::read_to_string(marker(root).ok()?).ok()?;
    let recorded = recorded.trim();
    (!recorded.is_empty()).then(|| recorded.to_string())
}

/// A path for a temporary file beside `root`, for a caller that needs one —
/// the bundle itself lands here on its way in.
pub fn scratch_path(root: &Path, what: &str) -> Result<PathBuf, String> {
    scratch(root, what)
}

/// Whether a freshly-unpacked tree holds the file the site will look for.
///
/// The counterpart of the post-`update` check, run against the staging
/// directory instead of the live one — so a bundle that unpacked to the wrong
/// shape is refused while the previous site is still serving, rather than
/// diagnosed after it has replaced it.
///
/// The diagnosis is the reason this is not just an `is_file` call. By far the
/// most common way to get this wrong is a bundle rolled with `tar czf dist.tgz
/// dist`, which wraps everything one directory deeper than the site expects, so
/// when the index turns up exactly there the error says which number to set
/// instead of leaving somebody to work it out.
pub fn verify_index(dir: &Path, index: &str, strip: usize) -> Result<String, String> {
    let index = index.trim();
    if index.is_empty() {
        // No index configured: the site is a bag of files, and `stage` has
        // already established that it unpacked some.
        return Ok("no index configured".into());
    }
    if dir.join(index).is_file() {
        return Ok(format!("{index} is in place"));
    }

    // Look exactly one level down, and only when that level is unambiguous.
    let nested = std::fs::read_dir(dir)
        .ok()
        .map(|entries| entries.filter_map(|e| e.ok()).collect::<Vec<_>>())
        .filter(|entries| entries.len() == 1)
        .map(|entries| entries[0].path())
        .filter(|p| p.join(index).is_file());

    Err(match nested {
        Some(p) => {
            let wrapper = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            format!(
                "the bundle has no {index} at its top level, but it does have {wrapper}/{index} \
                 — the bundle wraps the site in a {wrapper}/ directory. Set \
                 `artifact.strip_components: {}` to drop it",
                strip + 1
            )
        }
        None => format!(
            "the bundle unpacked without {index} in it{} — is this a bundle of the built \
             site, or of the project around it?",
            match strip {
                0 => String::new(),
                n => format!(" (after dropping {n} leading path component(s))"),
            }
        ),
    })
}

/// Record what was just deployed, so the next pull of the same digest is free.
///
/// Written after the swap, so a marker can only ever describe a tree that is
/// actually in place. A failure is reported to the caller rather than ignored:
/// the deploy succeeded, but silently losing the marker turns every subsequent
/// pull into a full re-unpack, and that is worth a line in the log.
pub fn record_digest(root: &Path, digest: &str) -> Result<(), String> {
    let path = marker(root)?;
    std::fs::write(&path, format!("{digest}\n"))
        .map_err(|e| format!("could not record the deployed digest in {}: {e}", path.display()))
}

/// Remove the marker, so the next pull unpacks even if the digest matches.
pub fn forget_digest(root: &Path) {
    if let Ok(path) = marker(root) {
        let _ = std::fs::remove_file(path);
    }
}

/// Unpack `archive` straight into `dest`, which must already exist.
///
/// The staging-and-swap dance in [`stage`] exists because a site's directory is
/// *being served* while it is replaced. A Docker build context is not: it is
/// written into a scratch directory that the caller just emptied, and nothing
/// reads it until the build starts. So this is the same extractor under the same
/// rules — no `..`, no absolute paths, no symlinks, hardlinks or devices — with
/// none of the swap.
///
/// The symlink rule is worth flagging, because `docker build` itself would allow
/// one: a context that needs a symlink is refused here rather than unpacked. A
/// link is how a tar reaches outside the directory it was told to fill, and a
/// build context arriving from a store is exactly the untrusted input that rule
/// was written for.
///
/// **Blocking.** Call it from `spawn_blocking`.
pub fn extract_into(archive: &Path, dest: &Path, strip: usize) -> Result<Unpacked, String> {
    extract(archive, dest, strip)
}

fn extract(archive: &Path, dest: &Path, strip: usize) -> Result<Unpacked, String> {
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("could not open the bundle at {}: {e}", archive.display()))?;
    let mut head = [0u8; 2];
    let gzipped = {
        let mut probe = &file;
        match probe.read_exact(&mut head) {
            Ok(()) => head == GZIP_MAGIC,
            // Shorter than two bytes: not a tar either, and `entries()` would
            // report it as an unhelpful unexpected-EOF.
            Err(_) => return Err("the bundle is empty or truncated".to_string()),
        }
    };
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("could not open the bundle at {}: {e}", archive.display()))?;

    let reader: Box<dyn Read> = if gzipped {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut tar = tar::Archive::new(reader);
    let entries = tar
        .entries()
        .map_err(|e| format!("the bundle is not a tar archive{}: {e}", if gzipped { " (after gunzip)" } else { "" }))?;

    let mut out = Unpacked::default();
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("the bundle could not be read: {e}"))?;
        let kind = entry.header().entry_type();
        // Archive metadata the `tar` crate surfaces rather than consumes. Not
        // content, and nothing to write.
        if kind.is_pax_global_extensions() || kind.is_gnu_longname() || kind.is_gnu_longlink() {
            continue;
        }

        let path = entry
            .path()
            .map_err(|e| format!("the bundle holds an unreadable path: {e}"))?
            .into_owned();
        let Some(relative) = relative_within(&path, strip)? else {
            continue;
        };
        let target = dest.join(&relative);

        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("could not create {}: {e}", relative.display()))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!(
                "the bundle holds {} , which is a {}. A site bundle may only contain files \
                 and directories — symlinks, hardlinks and devices are refused rather than \
                 unpacked",
                relative.display(),
                describe(kind),
            ));
        }

        // The entry's own directories may not have their own entries: `tar`
        // does not require them, and `--no-recursion` bundles routinely omit
        // them. Safe because `relative_within` has already established that
        // every component is a plain name.
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("could not create the directory for {}: {e}", relative.display()))?;
        }
        let mut file = std::fs::File::create(&target)
            .map_err(|e| format!("could not write {}: {e}", relative.display()))?;
        let bytes = std::io::copy(&mut entry, &mut file)
            .map_err(|e| format!("could not write {}: {e}", relative.display()))?;
        out.files += 1;
        out.bytes += bytes;
    }
    Ok(out)
}

/// An entry's path with `strip` components dropped, or `None` if nothing is
/// left of it.
///
/// This is the containment check, and it is done on the *components* rather
/// than by canonicalizing the result: the destination does not exist yet, and a
/// check that has to be run after the write is not a check.
fn relative_within(path: &Path, strip: usize) -> Result<Option<PathBuf>, String> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            // `tar czf … -C dist .` writes every entry as `./index.html`, so a
            // leading `.` is the common case, not a suspicious one.
            Component::CurDir => continue,
            Component::Normal(part) => {
                if part.is_empty() || part.as_encoded_bytes().contains(&0) {
                    return Err(format!("the bundle holds an unusable path: {}", path.display()));
                }
                parts.push(part);
            }
            // Everything else leaves the tree: `..`, a leading `/`, or a Windows
            // prefix. Refused rather than dropped.
            _ => {
                return Err(format!(
                    "the bundle holds {}, which would unpack outside the site root. \
                     A bundle may only contain relative paths",
                    path.display()
                ));
            }
        }
    }
    if parts.len() <= strip {
        return Ok(None);
    }
    Ok(Some(parts[strip..].iter().collect()))
}

fn describe(kind: tar::EntryType) -> &'static str {
    match kind {
        k if k.is_symlink() => "symlink",
        k if k.is_hard_link() => "hardlink",
        k if k.is_fifo() => "fifo",
        k if k.is_block_special() => "block device",
        k if k.is_character_special() => "character device",
        _ => "special file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A tar built in memory. `None` content means a directory entry.
    fn tarball(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, content) in entries {
            let mut header = tar::Header::new_gnu();
            match content {
                Some(bytes) => {
                    header.set_size(bytes.len() as u64);
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(0o644);
                    header.set_cksum();
                    builder.append_data(&mut header, name, *bytes).unwrap();
                }
                None => {
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(0o755);
                    header.set_cksum();
                    builder.append_data(&mut header, name, std::io::empty()).unwrap();
                }
            }
        }
        builder.into_inner().unwrap()
    }

    /// A one-entry tar whose name is written straight into the header.
    ///
    /// `Builder::append_data` refuses a `..` or a rooted path, which is exactly
    /// why it cannot be used here: a hostile bundle was not built with this
    /// crate, and a test that can only express well-formed archives proves
    /// nothing about the ones this module exists to refuse.
    fn hostile(name: &str, content: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        let bytes = name.as_bytes();
        assert!(bytes.len() < 100, "test name must fit the old header field");
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_cksum();

        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, content).unwrap();
        builder.into_inner().unwrap()
    }

    fn linked(name: &str, target: &str, kind: tar::EntryType) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(kind);
        header.set_mode(0o777);
        header.set_link_name(target).unwrap();
        header.set_cksum();
        builder.append_data(&mut header, name, std::io::empty()).unwrap();
        builder.into_inner().unwrap()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    struct Fixture {
        _dir: tempdir::TempDir,
        root: PathBuf,
        archive: PathBuf,
    }

    fn fixture(bundle: &[u8]) -> Fixture {
        let dir = tempdir::TempDir::new();
        let root = dir.path().join("public");
        let archive = dir.path().join("bundle.tar");
        std::fs::write(&archive, bundle).unwrap();
        Fixture { _dir: dir, root, archive }
    }

    /// A minimal `TempDir`, so the tests do not add a dependency for four lines.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct TempDir(PathBuf);
        impl TempDir {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                // Counter as well as pid: several tests run concurrently in one
                // process, and two of them sharing a directory is a flake that
                // only shows up under load.
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("app-lb-unpack-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_plain_tar_becomes_the_site_root() {
        let f = fixture(&tarball(&[
            ("index.html", Some(b"<h1>hi</h1>")),
            ("assets/", None),
            ("assets/app.js", Some(b"console.log(1)")),
        ]));
        let (staged, out) = stage(&f.root, &f.archive, 0).unwrap();
        assert_eq!(out.files, 2, "directories are not counted");
        assert_eq!(out.bytes, 11 + 14);
        // Still staged: the root does not exist until the commit.
        assert!(!f.root.exists());
        staged.commit().unwrap();

        assert_eq!(std::fs::read_to_string(f.root.join("index.html")).unwrap(), "<h1>hi</h1>");
        assert!(f.root.join("assets/app.js").is_file());
    }

    #[test]
    fn gzip_is_sniffed_rather_than_taken_from_a_filename() {
        // The blob has no name in the store, so the archive is written here
        // under a name that lies about its contents.
        let f = fixture(&gzip(&tarball(&[("index.html", Some(b"gz"))])));
        let (staged, out) = stage(&f.root, &f.archive, 0).unwrap();
        assert_eq!(out.files, 1);
        staged.commit().unwrap();
        assert_eq!(std::fs::read_to_string(f.root.join("index.html")).unwrap(), "gz");
    }

    /// `tar czf dist.tgz dist` is what people actually run, and it wraps
    /// everything in `dist/`.
    #[test]
    fn strip_components_drops_the_wrapper_directory() {
        let bundle = tarball(&[
            ("dist/", None),
            ("dist/index.html", Some(b"x")),
            ("dist/css/site.css", Some(b"y")),
        ]);
        let f = fixture(&bundle);
        let (staged, out) = stage(&f.root, &f.archive, 1).unwrap();
        assert_eq!(out.files, 2);
        staged.commit().unwrap();
        assert!(f.root.join("index.html").is_file());
        assert!(f.root.join("css/site.css").is_file());
        assert!(!f.root.join("dist").exists());
    }

    /// Without it, the same bundle deploys a site whose index is one directory
    /// down — which is exactly the 404-everything outcome `verify_site` exists
    /// to catch.
    #[test]
    fn without_strip_the_wrapper_survives() {
        let f = fixture(&tarball(&[("dist/index.html", Some(b"x"))]));
        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();
        staged.commit().unwrap();
        assert!(f.root.join("dist/index.html").is_file());
        assert!(!f.root.join("index.html").exists());
    }

    #[test]
    fn a_leading_dot_is_not_a_component_to_strip() {
        // `tar czf … -C dist .` — every entry is `./…` and strip must stay 0.
        let f = fixture(&tarball(&[("./index.html", Some(b"x")), ("./a/b.txt", Some(b"y"))]));
        let (staged, out) = stage(&f.root, &f.archive, 0).unwrap();
        assert_eq!(out.files, 2);
        staged.commit().unwrap();
        assert!(f.root.join("index.html").is_file());
        assert!(f.root.join("a/b.txt").is_file());
    }

    #[test]
    fn a_climbing_path_is_refused_not_sanitized() {
        for name in ["../escaped.html", "a/../../escaped.html", "/etc/passwd"] {
            let f = fixture(&hostile(name, b"x"));
            let e = stage(&f.root, &f.archive, 0).unwrap_err();
            assert!(
                e.contains("outside the site root"),
                "{name} should be refused, got: {e}"
            );
            assert!(!f.root.exists(), "{name} must not have deployed anything");
        }
    }

    /// The write-through attack: unpack a link pointing out of the tree, then
    /// write to a path under it.
    #[test]
    fn links_are_refused() {
        for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
            let f = fixture(&linked("passwd", "/etc/passwd", kind));
            let e = stage(&f.root, &f.archive, 0).unwrap_err();
            assert!(e.contains("refused rather than"), "got: {e}");
        }
    }

    #[test]
    fn a_bundle_that_unpacks_to_nothing_is_an_error() {
        // The shape of a `strip_components` set one too high.
        let f = fixture(&tarball(&[("dist/index.html", Some(b"x"))]));
        let e = stage(&f.root, &f.archive, 2).unwrap_err();
        assert!(e.contains("strip_components: 2"), "got: {e}");

        let f = fixture(b"not a tar at all, not even close");
        assert!(stage(&f.root, &f.archive, 0).is_err());
    }

    /// The point of staging: the old tree serves until the new one is proven,
    /// and an abandoned stage leaves nothing behind.
    #[test]
    fn dropping_a_stage_leaves_the_live_site_alone() {
        let f = fixture(&tarball(&[("index.html", Some(b"new"))]));
        std::fs::create_dir_all(&f.root).unwrap();
        std::fs::write(f.root.join("index.html"), b"old").unwrap();

        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();
        let staging = staged.dir().to_path_buf();
        assert_eq!(std::fs::read_to_string(f.root.join("index.html")).unwrap(), "old");
        drop(staged);

        assert!(!staging.exists(), "the staging directory must be cleaned up");
        assert_eq!(std::fs::read_to_string(f.root.join("index.html")).unwrap(), "old");
    }

    #[test]
    fn a_commit_replaces_the_previous_tree_entirely() {
        let f = fixture(&tarball(&[("index.html", Some(b"new"))]));
        std::fs::create_dir_all(f.root.join("old-dir")).unwrap();
        std::fs::write(f.root.join("stale.html"), b"stale").unwrap();

        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();
        staged.commit().unwrap();

        assert_eq!(std::fs::read_to_string(f.root.join("index.html")).unwrap(), "new");
        assert!(!f.root.join("stale.html").exists(), "the old tree must not survive");
        assert!(!f.root.join("old-dir").exists());
        // And nothing is left beside it.
        let leftovers: Vec<_> = std::fs::read_dir(f.root.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".public."))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn the_digest_marker_round_trips_and_sits_outside_the_root() {
        let f = fixture(&tarball(&[("index.html", Some(b"x"))]));
        assert_eq!(deployed_digest(&f.root), None, "nothing deployed yet");

        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();
        staged.commit().unwrap();
        record_digest(&f.root, "abc123").unwrap();

        assert_eq!(deployed_digest(&f.root).as_deref(), Some("abc123"));
        // Nothing servable was added.
        assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 1);
        assert!(f.root.parent().unwrap().join(".public.artifact").is_file());

        forget_digest(&f.root);
        assert_eq!(deployed_digest(&f.root), None);
    }

    /// A marker outliving the tree it describes would let a pull skip its work
    /// and report success over a directory somebody has since removed.
    #[test]
    fn a_marker_without_a_tree_does_not_count_as_deployed() {
        let f = fixture(&tarball(&[("index.html", Some(b"x"))]));
        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();
        staged.commit().unwrap();
        record_digest(&f.root, "abc123").unwrap();

        std::fs::remove_dir_all(&f.root).unwrap();
        assert_eq!(deployed_digest(&f.root), None);
    }

    /// The check that keeps a badly-shaped bundle from ever going live, and the
    /// one place a `strip_components` mistake gets diagnosed rather than merely
    /// reported.
    #[test]
    fn a_missing_index_names_the_wrapper_directory_when_there_is_one() {
        let f = fixture(&tarball(&[("dist/index.html", Some(b"x")), ("dist/app.js", Some(b"y"))]));
        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();

        let e = verify_index(staged.dir(), "index.html", 0).unwrap_err();
        assert!(e.contains("dist/index.html"), "{e}");
        assert!(e.contains("strip_components: 1"), "it should say what to set: {e}");

        // And with the wrapper dropped it passes.
        let (staged, _) = stage(&f.root, &f.archive, 1).unwrap();
        assert_eq!(verify_index(staged.dir(), "index.html", 1).unwrap(), "index.html is in place");
    }

    #[test]
    fn a_missing_index_with_no_obvious_cause_says_only_what_it_knows() {
        // Several top-level entries, so there is no single wrapper to blame.
        let f = fixture(&tarball(&[("src/main.ts", Some(b"x")), ("package.json", Some(b"y"))]));
        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();

        let e = verify_index(staged.dir(), "index.html", 0).unwrap_err();
        assert!(e.contains("of the built site"), "{e}");
        assert!(!e.contains("strip_components"), "it must not guess: {e}");
    }

    /// `site.index: ""` turns a directory request into a 404 by design, so
    /// there is no index to insist on.
    #[test]
    fn a_site_with_no_index_configured_has_nothing_to_verify() {
        let f = fixture(&tarball(&[("a.txt", Some(b"x"))]));
        let (staged, _) = stage(&f.root, &f.archive, 0).unwrap();
        assert!(verify_index(staged.dir(), "  ", 0).is_ok());
    }

    #[test]
    fn a_missing_parent_is_an_error_rather_than_a_directory_tree() {
        let f = fixture(&tarball(&[("index.html", Some(b"x"))]));
        let nested = f.root.join("deeper").join("public");
        let e = stage(&nested, &f.archive, 0).unwrap_err();
        assert!(e.contains("does not exist on this host"), "got: {e}");
    }
}
