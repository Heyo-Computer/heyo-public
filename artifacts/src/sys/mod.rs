//! Every raw syscall in this crate is wrapped here; no `unsafe` appears
//! outside `src/sys/`.
//!
//! These wrappers are all synchronous and all of them can block on IO. Callers
//! run them inside `tokio::task::spawn_blocking` — never directly on a runtime
//! thread. That is deliberate: the alternative is an async facade over
//! syscalls that have no async form, which hides the blocking instead of
//! confining it.

pub mod space;
pub mod sparse;
pub mod tmpfile;

use std::ffi::CString;
use std::io;
use std::path::Path;

/// Convert a path to a NUL-terminated C string.
///
/// An embedded NUL is rejected here rather than silently truncating the path —
/// truncation would open or unlink a *different* file than the caller named.
pub(crate) fn cpath(p: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains an interior NUL"))
}

/// Map a syscall's `-1` return to the current `errno`.
pub(crate) fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn cpath_rejects_interior_nul() {
        let p = Path::new(OsStr::from_bytes(b"/tmp/a\0b"));
        assert!(cpath(p).is_err());
    }

    #[test]
    fn cpath_round_trips_normal_paths() {
        let c = cpath(Path::new("/tmp/blob")).unwrap();
        assert_eq!(c.to_bytes(), b"/tmp/blob");
    }
}
