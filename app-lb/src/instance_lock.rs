//! One app-lb per host.
//!
//! `Registry::controller_lock` already makes a *state directory* exclusive, but
//! two instances with different `APP_LB_STATE_PATH`s (or one started from
//! another working directory, where the relative default resolves elsewhere)
//! do not share one. Both then reach the same heyvm daemon, and each believes
//! the other's sandboxes are orphans from a previous run of its own. That is
//! how `app-lb --version` in `/root` destroyed a fleet on 2026-09-29.
//!
//! So startup also takes a host-wide lock, at a fixed path that does not depend
//! on any other setting. `APP_LB_INSTANCE_LOCK` names a different path — for a
//! host that deliberately runs several app-lbs against *separate* daemons, such
//! as a development machine — or `off` to take none.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Where the lock lives when nothing says otherwise. `/run` is per boot and
/// root-owned, which is exactly the scope of "the app-lb on this host".
pub const DEFAULT_PATH: &str = "/run/app-lb/instance.lock";

/// What `APP_LB_INSTANCE_LOCK` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Setting {
    Off,
    At(PathBuf),
}

pub fn setting(value: Option<&str>) -> Setting {
    match value.map(str::trim) {
        None | Some("") => Setting::At(PathBuf::from(DEFAULT_PATH)),
        Some(v)
            if matches!(
                v.to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "none"
            ) =>
        {
            Setting::Off
        }
        Some(v) => Setting::At(PathBuf::from(v)),
    }
}

/// Take the lock `setting` names. `Ok(None)` when it is off.
///
/// The default path needs root (`/run`); an unprivileged instance — a
/// developer's — falls back to the temp directory, which is still shared by
/// everything on the host. An explicit path never falls back: a typo there
/// should be an error, not a quieter lock somewhere else.
pub fn acquire_setting(
    setting: &Setting,
    holder: &str,
) -> std::io::Result<Option<(File, PathBuf)>> {
    let Setting::At(path) = setting else {
        return Ok(None);
    };
    match acquire(path, holder) {
        Err(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied
                && path == Path::new(DEFAULT_PATH) =>
        {
            let fallback = std::env::temp_dir().join("app-lb-instance.lock");
            acquire(&fallback, holder).map(|f| Some((f, fallback)))
        }
        other => other.map(|f| Some((f, path.clone()))),
    }
}

/// Take the lock at `path`, recording who holds it.
///
/// The returned file must live as long as the process: the lock is released
/// when it is closed. On contention the error says which process holds it,
/// read back from the file, so the refusal is actionable.
pub fn acquire(path: &Path, holder: &str) -> std::io::Result<File> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if file.try_lock().is_err() {
        let mut current = String::new();
        let _ = file.read_to_string(&mut current);
        let current = current.trim();
        return Err(std::io::Error::other(format!(
            "another app-lb holds {} ({}); only one may manage this host's sandboxes. \
             Set APP_LB_INSTANCE_LOCK to a different path only if this instance talks \
             to a different heyvm daemon",
            path.display(),
            if current.is_empty() {
                "holder unknown"
            } else {
                current
            },
        )));
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{holder}")?;
    file.sync_all()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_the_fixed_host_path() {
        assert_eq!(setting(None), Setting::At(PathBuf::from(DEFAULT_PATH)));
        assert_eq!(
            setting(Some("  ")),
            Setting::At(PathBuf::from(DEFAULT_PATH))
        );
    }

    #[test]
    fn off_in_any_spelling_disables_it() {
        for v in ["off", "OFF", "0", "false", "none"] {
            assert_eq!(setting(Some(v)), Setting::Off, "{v}");
        }
        assert_eq!(
            setting(Some("/tmp/x.lock")),
            Setting::At(PathBuf::from("/tmp/x.lock"))
        );
    }

    #[test]
    fn a_second_instance_is_refused_and_told_who_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("instance.lock");
        let first = acquire(&path, "pid 42 state=/var/lib/app-lb/app-lb-state.json").unwrap();
        let err = acquire(&path, "pid 43 state=app-lb-state.json")
            .unwrap_err()
            .to_string();
        assert!(err.contains("another app-lb holds"), "{err}");
        assert!(
            err.contains("pid 42"),
            "the refusal names the holder: {err}"
        );
        drop(first);
        // Released with the file: the next start succeeds and records itself.
        let _second = acquire(&path, "pid 44").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "pid 44");
    }
}
