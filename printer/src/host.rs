//! Host-capability predicates: can the `computer` tool actually drive this
//! machine's desktop? Shared by `review` (auto-route UI review to the host),
//! `test` (gate `printer test`), and `agent` (gate the computer MCP server so
//! it's only offered to Claude on a real, non-sandboxed display).

use std::path::{Path, PathBuf};

/// Mirror the preconditions of the `computer` CLI's
/// `Connection::connect_to_env()` + `/dev/uinput` open: a real display the
/// computer tool can drive exists iff a Wayland/X11 session is advertised AND
/// `/dev/uinput` is present.
pub(crate) fn host_display_available() -> bool {
    let has_session = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || matches!(
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
            Some("wayland") | Some("x11")
        );
    has_session && Path::new("/dev/uinput").exists()
}

/// Resolve the `computer` CLI on PATH, returning its path. Scans each PATH
/// entry for an executable named `computer`. Used both to gate host UI review
/// and to supply the absolute `command` for the computer MCP server.
/// (If sandboxed UI review were ever pursued, the alternative would be to
/// build/copy the `computer` bin into the heyvm image — out of scope here.)
pub(crate) fn locate_computer_binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("computer"))
        .find(|cand| cand.is_file())
}

/// Is the `computer` CLI on PATH? Thin bool wrapper over
/// [`locate_computer_binary`].
pub(crate) fn computer_on_path() -> bool {
    locate_computer_binary().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computer_on_path_finds_known_bin() {
        // A dir containing an executable file named `computer` is detected.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("computer");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Empty PATH -> not found.
        let prev = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "/nonexistent-printer-test-dir") };
        assert!(!computer_on_path());
        assert!(locate_computer_binary().is_none());
        // PATH with our temp dir -> found, and the resolved path is the bin.
        unsafe { std::env::set_var("PATH", dir.path()) };
        assert!(computer_on_path());
        assert_eq!(locate_computer_binary().as_deref(), Some(bin.as_path()));
        match prev {
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}
