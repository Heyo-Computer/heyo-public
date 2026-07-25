//! Detecting VM data-disk directories that heyvmd has forgotten.
//!
//! Every per-schema VM owns a `run_dir/sb-<id>/` directory (its sparse
//! `data.ext4`, a rootfs copy, logs). When a schema is archived to S3 or frozen
//! to a local dump the pooler kills its VM to reclaim that directory — but the
//! kill is a `DELETE /deployed-sandboxes/:id` the SDK treats as success on a
//! 404, and heyvmd has been observed to drop the sandbox record while leaving
//! the directory behind. The VM count falls, the bytes don't, and nothing else
//! removes them: `reclaim-disks.sh` only *trims* live schemas' disks, it never
//! deletes a dead sandbox. Over time this strands hundreds of GB.
//!
//! [`crate::registry::SchemaRegistry::sweep_orphans`] cleans that backlog. Its
//! safety gate has three independent parts, and this module provides the one
//! that guards against a heyvmd that wrongly reports a *live* VM as gone: the
//! open-file check. If any process on the host holds a file inside the
//! directory open, a VM is using it and it is never a deletion candidate,
//! whatever the daemon says. Identity is `(device, inode)`, not path, so it is
//! correct across the jailer chroot / bind mounts / mount namespaces a
//! Firecracker VM runs under (a matched path would silently miss chrooted fds
//! and trim a live disk — the exact trap `reclaim-disks.sh` documents).
//!
//! Coverage note: the scan sees a process's fds only if the scanner may read
//! `/proc/<pid>/fd`. The pooler and heyvmd/firecracker run as the same user, so
//! it sees them without root; fds owned by *other* users are silently skipped
//! (they aren't ours to reason about anyway). It is a point-in-time snapshot —
//! combined with the daemon's per-id check and a directory-age floor in the
//! caller, the window for deleting a disk that a VM starts using mid-sweep is
//! closed.

use std::collections::HashSet;
use std::path::Path;

/// A file's on-host identity, stable across chroot / bind mount / namespace.
pub type Inode = (u64, u64);

/// Snapshot every `(device, inode)` any readable process currently holds open,
/// by walking `/proc/<pid>/fd`. Best-effort: unreadable pids/fds are skipped, so
/// a partial scan under-reports (fails toward "not held"), which is why the
/// caller must *also* require the daemon to confirm the sandbox gone before
/// deleting — the two guards cover each other.
pub fn open_inodes() -> HashSet<Inode> {
    use std::os::unix::fs::MetadataExt;
    let mut set = HashSet::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return set;
    };
    for proc in procs.flatten() {
        // Only numeric entries are pids.
        if !proc
            .file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            // metadata() follows the fd symlink to the target file; sockets,
            // pipes, and deleted files either error or carry inodes we simply
            // record harmlessly. Only regular-file matches matter downstream.
            if let Ok(md) = std::fs::metadata(fd.path()) {
                set.insert((md.dev(), md.ino()));
            }
        }
    }
    set
}

/// Is any file in `dir` (one level deep — enough to cover `data.ext4`, the
/// rootfs copy, and jailer's `root/` contents) currently held open by a
/// process in `open`? Errors reading the directory fail *closed* (treated as
/// held), so an unreadable candidate is skipped rather than deleted.
pub fn dir_held_open(dir: &Path, open: &HashSet<Inode>) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            return true;
        };
        if ft.is_dir() {
            // One level deeper (jailer nests the chroot under root/).
            if let Ok(sub) = std::fs::read_dir(entry.path()) {
                for s in sub.flatten() {
                    if let Ok(md) = s.metadata()
                        && open.contains(&(md.dev(), md.ino()))
                    {
                        return true;
                    }
                }
            }
        } else if let Ok(md) = entry.metadata()
            && open.contains(&(md.dev(), md.ino()))
        {
            return true;
        }
    }
    false
}

/// Best-effort on-disk (allocated, not apparent) byte size of a directory tree,
/// for log lines and sweep accounting. A sparse `data.ext4` reports its full
/// *provisioned* apparent size, so summing lengths would wildly over-count;
/// summing `blocks()` (512-byte units, what `du` counts) reports what the files
/// actually pin. Runs on a blocking thread; any error (a race with removal,
/// permissions) just yields 0 — callers use this only to label output.
pub async fn dir_allocated_bytes(dir: std::path::PathBuf) -> u64 {
    tokio::task::spawn_blocking(move || {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            let mut total = 0u64;
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    total += walk(&entry.path());
                } else if let Ok(md) = entry.metadata() {
                    use std::os::unix::fs::MetadataExt;
                    total += md.blocks() * 512;
                }
            }
            total
        }
        walk(&dir)
    })
    .await
    .unwrap_or(0)
}

/// Compact IEC byte string (e.g. `51.0 GiB`) for log lines.
pub fn human_iec(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `/proc/<pid>/fd` is Linux-only — the pooler's only target. On other dev
    // platforms `open_inodes` correctly returns empty (no /proc), so this
    // liveness assertion can only be made where the mechanism exists.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_inodes_sees_a_file_this_process_holds_open() {
        use std::os::unix::fs::MetadataExt;
        // Hold a real file open, then confirm the scan finds its (dev, inode).
        let path = std::env::temp_dir().join(format!("pgfc-open-{}", std::process::id()));
        let f = std::fs::File::create(&path).unwrap();
        let md = std::fs::metadata(&path).unwrap();
        let key = (md.dev(), md.ino());

        let set = open_inodes();
        assert!(
            set.contains(&key),
            "a file this process holds open must appear in the open-inode snapshot"
        );
        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dir_held_open_matches_only_when_a_file_is_in_the_set() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("pgfc-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("data.ext4");
        std::fs::write(&disk, b"x").unwrap();
        let md = std::fs::metadata(&disk).unwrap();
        let held = (md.dev(), md.ino());

        // Empty set → not held.
        assert!(!dir_held_open(&dir, &HashSet::new()));
        // Set containing the disk's identity → held.
        let mut set = HashSet::new();
        set.insert(held);
        assert!(dir_held_open(&dir, &set));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing/unreadable directory must read as held (fail closed), so a
    /// candidate we can't inspect is never deleted.
    #[test]
    fn unreadable_dir_is_treated_as_held() {
        let missing = std::env::temp_dir().join(format!("pgfc-nope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(dir_held_open(&missing, &HashSet::new()));
    }

    #[test]
    fn human_iec_scales_and_labels() {
        assert_eq!(human_iec(0), "0 B");
        assert_eq!(human_iec(512), "512 B");
        assert_eq!(human_iec(1024), "1.0 KiB");
        assert_eq!(human_iec(1536), "1.5 KiB");
        // The number the leak warning most wants to be right: tens of GiB.
        assert_eq!(human_iec(51 * 1024 * 1024 * 1024), "51.0 GiB");
    }

    /// `dir_allocated_bytes` must count what a sparse file actually pins on disk
    /// (allocated blocks), not its apparent size — a `data.ext4` provisioned at
    /// 4GB but holding 1MB must read ~1MB, or the accounting would over-report
    /// every disk by its full provisioned cap.
    #[tokio::test]
    async fn dir_allocated_counts_blocks_not_apparent_size() {
        let dir = std::env::temp_dir().join(format!("pgfc-alloc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        // A dense 8KiB file in a subdir: real allocated blocks we should count.
        std::fs::write(dir.join("sub/dense"), vec![0u8; 8 * 1024]).unwrap();

        // A sparse file with a huge apparent size but ~no blocks: set length to
        // 4GiB without writing, so blocks() stays tiny.
        let f = std::fs::File::create(dir.join("data.ext4")).unwrap();
        f.set_len(4 * 1024 * 1024 * 1024).unwrap();
        drop(f);

        let bytes = dir_allocated_bytes(dir.clone()).await;
        assert!(
            bytes < 1024 * 1024,
            "expected allocated size well under 1MiB (dense file only), got {bytes}"
        );
        assert!(bytes >= 8 * 1024, "dense 8KiB file's blocks must be counted, got {bytes}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
