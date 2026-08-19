//! `st_nlink == 1` means "referenced by nothing but the store's own directory
//! entry".
//!
//! The kernel maintains that count in the same journal transaction as the link
//! itself, so unlike a userspace refcount there is no window in which the count
//! and reality disagree — and no reconciliation to perform after a crash. A
//! materialization in `~/.heyo` or `/tmp` is a garbage-collection root that the
//! store cannot enumerate, and does not need to: `st_nlink` already sees it.
//!
//! Manifests do not raise `nlink` — a manifest is JSON naming a digest, not a
//! link to it — so reachability still needs a mark phase. Building a `refs/`
//! directory of hardlinks to avoid that would double the store's inode count
//! and reintroduce exactly the crash window `st_nlink` eliminates.
//!
//! # The race, and why there are two closures
//!
//! A writer links a blob that is, necessarily, in no manifest yet — the
//! manifest names the blob's digest, so it can only be written afterwards. A
//! sweeper holding a mark set from before that link sees `nlink == 1`, finds it
//! in no manifest, and deletes a blob that is about to be referenced.
//!
//! 1. **The store lock.** The sweep holds it exclusively across mark *and*
//!    sweep; a writer holds it shared across "link the blobs, write the
//!    manifest". Neither can interleave with the other.
//! 2. **The age grace window.** A blob younger than `min_age` is never swept,
//!    which covers anything that bypasses the lock: a crashed writer, a
//!    different build, a human copying a file in by hand.

use crate::digest::Digest;
use crate::error::Result;
use crate::lock::LockMode;
use crate::manifest::Manifest;
use crate::store::Store;
use crate::sys::sparse;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct GcPolicy {
    /// Blobs created more recently than this are always kept.
    pub min_age: Duration,
    /// Report what would be removed without removing it.
    pub dry_run: bool,
}

impl Default for GcPolicy {
    fn default() -> Self {
        GcPolicy {
            min_age: crate::config::DEFAULT_GC_MIN_AGE,
            dry_run: false,
        }
    }
}

/// A blob kept alive by a hardlink somewhere outside the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub digest: Digest,
    pub size: u64,
    pub allocated: u64,
    /// Materializations outstanding — `st_nlink` minus the store's own entry.
    pub links: u64,
}

impl Pin {
    /// POSIX offers no reverse map from an inode to its names, so the best the
    /// store can do for an operator chasing a leaked materialization is tell
    /// them how to find it.
    pub fn find_hint(&self, root: &std::path::Path) -> String {
        format!(
            "find {} -xdev -samefile {}/blobs/{}/{}",
            std::env::var("HOME").unwrap_or_else(|_| "/".into()),
            root.display(),
            self.digest.shard(),
            self.digest
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub scanned: u64,
    pub removed: Vec<Digest>,
    pub bytes_freed: u64,
    pub kept_reachable: u64,
    pub kept_young: u64,
    pub pinned: Vec<Pin>,
    pub manifests_scanned: u64,
    pub manifests_removed: Vec<Digest>,
    /// Labels whose digest no longer names anything. See the sweep below for
    /// why they are counted rather than listed.
    pub labels_removed: u64,
}

impl GcReport {
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// Mark from the tags, then sweep everything else.
pub async fn collect(store: &Store, policy: GcPolicy) -> Result<GcReport> {
    let inner = store.inner().clone();
    let root = store.root().to_path_buf();
    crate::store::run_blocking(move || {
        // Exclusive for the whole of mark and sweep: a blob linked between the
        // two phases would be invisible to the mark and eligible for the sweep.
        let _guard = inner.lock().acquire(LockMode::Exclusive)?;

        let (live_blobs, live_manifests) = mark(&inner)?;
        let now = SystemTime::now();
        let mut report = GcReport::default();

        // -- blobs ---------------------------------------------------------
        for i in 0u16..256 {
            let shard = format!("{i:02x}");
            for (name, _ino) in inner.shard_names("blobs", &shard)? {
                let Ok(digest) = Digest::parse(&name) else {
                    continue;
                };
                report.scanned += 1;

                let Ok(st) = inner.blob_stat(&digest) else {
                    // Vanished under us; nothing to do.
                    continue;
                };

                if live_blobs.contains(&digest) {
                    report.kept_reachable += 1;
                    continue;
                }
                if st.nlink > 1 {
                    report.pinned.push(Pin {
                        digest,
                        size: st.size,
                        allocated: st.allocated,
                        links: st.nlink - 1,
                    });
                    continue;
                }
                if is_young(now, st.created, policy.min_age) {
                    report.kept_young += 1;
                    continue;
                }

                report.bytes_freed += st.allocated;
                if !policy.dry_run {
                    sparse::unlink_if_present(&inner.blob_path_of(&digest))?;
                }
                report.removed.push(digest);
            }
        }

        // -- manifests -----------------------------------------------------
        for digest in inner.manifest_digests()? {
            report.manifests_scanned += 1;
            if live_manifests.contains(&digest) {
                continue;
            }
            let path = inner.manifest_path_of(&digest);
            if let Ok(st) = crate::sys::space::stat_path(&path)
                && is_young(now, st.created, policy.min_age)
            {
                continue;
            }
            if !policy.dry_run {
                sparse::unlink_if_present(&path)?;
            }
            report.manifests_removed.push(digest);
        }

        // -- labels --------------------------------------------------------
        //
        // A label describes a digest and is referenced by nothing, so it cannot
        // keep anything alive and nothing keeps it alive either. What it can do
        // is outlive its subject: collect the blob a label names and the label
        // is left describing an address that resolves to nothing.
        //
        // Swept last, so the two phases that could have orphaned a label have
        // already run. There is no grace window: a label is only removed once
        // its subject is gone, and re-inserting those bytes later deserves a
        // fresh description rather than the one that outlived them.
        //
        // "Gone" means *not on disk or removed by this sweep*, and the second
        // half is what makes a dry run tell the truth. Under `dry_run` nothing
        // is unlinked, so asking the filesystem alone would find every subject
        // still present and report that no labels would be removed — while a
        // real run of the same sweep removed them. A dry run that under-reports
        // is worse than no dry run, because it is the thing people read before
        // deciding to run it for real.
        //
        // Counted rather than listed: a caller reporting a sweep wants to know
        // the tidying happened, and a hundred digests whose objects were just
        // reported as removed is a second copy of the same list.
        let swept: HashSet<&Digest> = report
            .removed
            .iter()
            .chain(report.manifests_removed.iter())
            .collect();
        for digest in inner.label_digests()? {
            let gone = swept.contains(&digest)
                || !(inner.blob_exists(&digest) || inner.manifest_exists(&digest));
            if !gone {
                continue;
            }
            if !policy.dry_run {
                sparse::unlink_if_present(&inner.label_path_of(&digest))?;
            }
            report.labels_removed += 1;
        }

        tracing::info!(
            scanned = report.scanned,
            removed = report.removed.len(),
            bytes_freed = report.bytes_freed,
            pinned = report.pinned.len(),
            labels_removed = report.labels_removed,
            dry_run = policy.dry_run,
            root = %root.display(),
            "garbage collection finished"
        );
        Ok(report)
    })
    .await
}

/// Everything reachable from a tag.
///
/// A tag may name a blob directly or a manifest; a manifest's entries are
/// reachable, but only when the manifest itself is tagged. An untagged manifest
/// does **not** keep its blobs alive — otherwise garbage would sustain itself
/// and nothing would ever be collected.
fn mark(inner: &crate::store::Inner) -> Result<(HashSet<Digest>, HashSet<Digest>)> {
    let mut blobs = HashSet::new();
    let mut manifests = HashSet::new();

    for (_tag, digest) in inner.all_tags()? {
        // A digest is a blob, a manifest, or dangling; try both.
        blobs.insert(digest.clone());
        if let Ok(bytes) = inner.manifest_bytes(&digest) {
            manifests.insert(digest.clone());
            match Manifest::from_json(&bytes) {
                Ok(m) => {
                    for e in &m.entries {
                        blobs.insert(e.digest.clone());
                    }
                }
                // An unparseable manifest is kept (it is tagged) but cannot
                // contribute reachability. Failing the sweep over one bad file
                // would make the store impossible to clean up.
                Err(e) => {
                    tracing::warn!(digest = %digest, error = %e, "tagged manifest is unreadable");
                }
            }
        }
    }
    Ok((blobs, manifests))
}

fn is_young(now: SystemTime, created: SystemTime, min_age: Duration) -> bool {
    match now.duration_since(created) {
        Ok(age) => age < min_age,
        // Created in the future — a clock step. Treat as young; the next sweep
        // will collect it once the clock agrees.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::manifest::KIND_GENERIC;
    use crate::store::Materialize;
    use crate::tags::TagName;
    use std::path::PathBuf;

    fn tmpdir() -> tempfile::TempDir {
        match std::env::var_os("ART_TEST_DIR").map(PathBuf::from) {
            Some(b) => tempfile::tempdir_in(b).unwrap(),
            None => tempfile::tempdir().unwrap(),
        }
    }

    fn store_in(d: &tempfile::TempDir) -> Store {
        Store::open(&Config {
            root: d.path().join("store"),
            min_free_bytes: 0,
            gc_min_age: Duration::ZERO,
            heyvm_images_dir: d.path().join("images"),
        })
        .unwrap()
    }

    fn now_policy() -> GcPolicy {
        GcPolicy {
            min_age: Duration::ZERO,
            dry_run: false,
        }
    }

    #[tokio::test]
    async fn sweeps_an_orphan_and_keeps_a_tagged_blob() {
        let d = tmpdir();
        let s = store_in(&d);
        let keep = s.insert_bytes(b"keep".to_vec()).await.unwrap();
        let drop = s.insert_bytes(b"drop".to_vec()).await.unwrap();
        s.set_tag(&TagName::parse("keeper").unwrap(), &keep.digest)
            .await
            .unwrap();

        let r = collect(&s, now_policy()).await.unwrap();
        assert_eq!(r.removed, vec![drop.digest.clone()]);
        assert_eq!(r.kept_reachable, 1);
        assert!(s.has(&keep.digest).await.unwrap());
        assert!(!s.has(&drop.digest).await.unwrap());
    }

    #[tokio::test]
    async fn reaches_blobs_through_a_tagged_manifest() {
        let d = tmpdir();
        let s = store_in(&d);
        let member = s.insert_bytes(b"member".to_vec()).await.unwrap();
        let orphan = s.insert_bytes(b"orphan".to_vec()).await.unwrap();
        let m = Manifest::new(KIND_GENERIC).with_entry(
            "member",
            member.digest.clone(),
            member.size,
        );
        let md = s.put_manifest(&m).await.unwrap();
        s.set_tag(&TagName::parse("bundle").unwrap(), &md)
            .await
            .unwrap();

        let r = collect(&s, now_policy()).await.unwrap();
        assert_eq!(r.removed, vec![orphan.digest]);
        assert!(s.has(&member.digest).await.unwrap());
        assert!(s.get_manifest(&md).await.is_ok());
    }

    #[tokio::test]
    async fn an_untagged_manifest_does_not_keep_its_blobs_alive() {
        // Otherwise garbage sustains itself and the store never shrinks.
        let d = tmpdir();
        let s = store_in(&d);
        let member = s.insert_bytes(b"member".to_vec()).await.unwrap();
        let m = Manifest::new(KIND_GENERIC).with_entry(
            "member",
            member.digest.clone(),
            member.size,
        );
        let md = s.put_manifest(&m).await.unwrap();

        let r = collect(&s, now_policy()).await.unwrap();
        assert!(r.removed.contains(&member.digest));
        assert!(r.manifests_removed.contains(&md));
        assert!(!s.has(&member.digest).await.unwrap());
    }

    #[tokio::test]
    async fn a_hardlink_outside_the_store_pins_a_blob() {
        let d = tmpdir();
        let s = store_in(&d);
        let info = s.insert_bytes(b"in use by a vm".to_vec()).await.unwrap();
        let dest = d.path().join("vm/rootfs");
        let m = s
            .materialize(&info.digest, &dest, Materialize::ReadOnly)
            .await
            .unwrap();

        // Untagged and in no manifest, yet still live: the kernel's link count
        // is what makes GC safe to run while VMs are booting.
        let r = collect(&s, now_policy()).await.unwrap();
        assert!(r.removed.is_empty(), "removed {:?}", r.removed);
        assert_eq!(r.pinned.len(), 1);
        assert_eq!(r.pinned[0].digest, info.digest);
        assert_eq!(r.pinned[0].links, 1);
        assert!(s.has(&info.digest).await.unwrap());

        // Once released it becomes collectable.
        s.release(&m).await.unwrap();
        let r = collect(&s, now_policy()).await.unwrap();
        assert_eq!(r.removed, vec![info.digest.clone()]);
        assert!(!s.has(&info.digest).await.unwrap());
    }

    #[tokio::test]
    async fn the_grace_window_protects_a_fresh_blob() {
        let d = tmpdir();
        let s = store_in(&d);
        let info = s.insert_bytes(b"just written".to_vec()).await.unwrap();

        let r = collect(
            &s,
            GcPolicy {
                min_age: Duration::from_secs(3600),
                dry_run: false,
            },
        )
        .await
        .unwrap();
        assert!(r.removed.is_empty());
        assert_eq!(r.kept_young, 1);
        assert!(s.has(&info.digest).await.unwrap());
    }

    #[tokio::test]
    async fn dry_run_reports_without_deleting() {
        let d = tmpdir();
        let s = store_in(&d);
        let info = s.insert_bytes(b"doomed".to_vec()).await.unwrap();

        let dry = collect(
            &s,
            GcPolicy {
                min_age: Duration::ZERO,
                dry_run: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(dry.removed, vec![info.digest.clone()]);
        assert!(dry.bytes_freed > 0);
        assert!(s.has(&info.digest).await.unwrap(), "dry run deleted a blob");

        // And the real run reports the same set.
        let wet = collect(&s, now_policy()).await.unwrap();
        assert_eq!(wet.removed, dry.removed);
        assert!(!s.has(&info.digest).await.unwrap());
    }

    #[tokio::test]
    async fn an_empty_store_sweeps_cleanly() {
        let d = tmpdir();
        let s = store_in(&d);
        let r = collect(&s, now_policy()).await.unwrap();
        assert_eq!(r.scanned, 0);
        assert!(r.removed.is_empty());
    }

    #[tokio::test]
    async fn a_dangling_tag_does_not_break_the_sweep() {
        let d = tmpdir();
        let s = store_in(&d);
        let gone = s.insert_bytes(b"gone".to_vec()).await.unwrap();
        let keep = s.insert_bytes(b"keep".to_vec()).await.unwrap();
        s.set_tag(&TagName::parse("dangling").unwrap(), &gone.digest)
            .await
            .unwrap();
        // Remove the blob behind the tag by hand.
        std::fs::remove_file(s.blob_path(&gone.digest)).unwrap();

        let r = collect(&s, now_policy()).await.unwrap();
        assert!(r.removed.contains(&keep.digest));
    }

    #[tokio::test]
    async fn sweeping_is_idempotent() {
        let d = tmpdir();
        let s = store_in(&d);
        s.insert_bytes(b"a".to_vec()).await.unwrap();
        s.insert_bytes(b"b".to_vec()).await.unwrap();
        let first = collect(&s, now_policy()).await.unwrap();
        assert_eq!(first.removed.len(), 2);
        let second = collect(&s, now_policy()).await.unwrap();
        assert!(second.removed.is_empty());
        assert_eq!(second.scanned, 0);
    }

    #[tokio::test]
    async fn reports_freed_bytes() {
        let d = tmpdir();
        let s = store_in(&d);
        s.insert_bytes(vec![7u8; 200_000]).await.unwrap();
        let r = collect(&s, now_policy()).await.unwrap();
        assert_eq!(r.removed.len(), 1);
        assert!(r.bytes_freed >= 200_000, "freed {}", r.bytes_freed);
    }

    #[test]
    fn find_hint_names_the_blob_path() {
        let p = Pin {
            digest: Digest::parse(&hex::encode([1u8; 32])).unwrap(),
            size: 1,
            allocated: 1,
            links: 1,
        };
        let hint = p.find_hint(std::path::Path::new("/srv/art"));
        assert!(hint.contains("/srv/art/blobs/01/"));
        assert!(hint.contains("-samefile"));
    }

    #[test]
    fn a_future_timestamp_counts_as_young() {
        // A clock step must not make the sweeper delete everything.
        let now = SystemTime::now();
        let future = now + Duration::from_secs(600);
        assert!(is_young(now, future, Duration::ZERO));
    }

    /// A label describes a digest and cannot keep anything alive. What it can do
    /// is outlive its subject, which is the one thing the sweep has to fix.
    #[tokio::test]
    async fn labels_outliving_their_subject_are_swept() {
        let d = tmpdir();
        let s = store_in(&d);

        let kept = s.insert_bytes(b"kept".to_vec()).await.unwrap();
        let doomed = s.insert_bytes(b"doomed".to_vec()).await.unwrap();
        s.set_tag(&TagName::parse("keep").unwrap(), &kept.digest).await.unwrap();

        for (digest, name) in [(&kept.digest, "the kept one"), (&doomed.digest, "the doomed one")] {
            s.set_label(digest, &crate::Label::new(Some(name.into()), None).unwrap())
                .await
                .unwrap();
        }

        let report = collect(&s, now_policy()).await.unwrap();
        assert_eq!(report.removed, vec![doomed.digest.clone()]);
        assert_eq!(report.labels_removed, 1, "the orphaned label goes with its blob");

        assert!(
            s.get_label(&kept.digest).await.unwrap().is_some(),
            "a label whose subject survived is untouched",
        );
        assert_eq!(s.get_label(&doomed.digest).await.unwrap(), None);
    }

    /// A dry run reports the tidying it would do and does none of it — the same
    /// contract the blob and manifest phases keep.
    #[tokio::test]
    async fn a_dry_run_leaves_orphaned_labels_alone() {
        let d = tmpdir();
        let s = store_in(&d);
        let doomed = s.insert_bytes(b"doomed".to_vec()).await.unwrap();
        s.set_label(&doomed.digest, &crate::Label::new(Some("x".into()), None).unwrap())
            .await
            .unwrap();

        let report = collect(
            &s,
            GcPolicy {
                dry_run: true,
                ..now_policy()
            },
        )
        .await
        .unwrap();
        assert_eq!(report.labels_removed, 1);
        assert!(s.get_label(&doomed.digest).await.unwrap().is_some());
    }

    /// A manifest's label is swept when the manifest is, and kept when it is
    /// not — the sweep asks about both kinds because one label store serves
    /// both.
    #[tokio::test]
    async fn a_tagged_manifests_label_survives_collection() {
        let d = tmpdir();
        let s = store_in(&d);
        let blob = s.insert_bytes(b"entry".to_vec()).await.unwrap();
        let m = crate::Manifest::new(KIND_GENERIC).with_entry("f", blob.digest.clone(), blob.size);
        let md = s.put_manifest(&m).await.unwrap();
        s.set_tag(&TagName::parse("bundle").unwrap(), &md).await.unwrap();
        s.set_label(&md, &crate::Label::new(Some("a bundle".into()), None).unwrap())
            .await
            .unwrap();

        let report = collect(&s, now_policy()).await.unwrap();
        assert_eq!(report.labels_removed, 0);
        assert!(s.get_label(&md).await.unwrap().is_some());
    }
}
