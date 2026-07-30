//! Dashboard credentials are a separate identity from the API key.
//!
//! `ART_API_KEY` authenticates *machines* — heyvm, CI, another store pulling
//! blobs — and travels in a header no browser will ever send. The dashboard is
//! read by *people*, so it gets its own username and password, its own session,
//! and its own failure mode (a redirect to a login page rather than a `401`
//! carrying `WWW-Authenticate`). Giving a person the machine key so they can
//! look at a usage page is how machine keys end up in browser histories.
//!
//! **The dashboard is off unless something asks for it.** Not open — off. A
//! store's tag names and blob sizes describe what an organisation builds and
//! deploys, and defaulting that to public because someone forgot a variable is
//! not a trade this module is willing to make on the operator's behalf.
//!
//! A password is one way to ask; `ART_DASHBOARD_OPEN` is the other, for a
//! listener that is already private. Nothing in this module implements that
//! second case — an open dashboard simply has no [`AdminAuth`] at all, so there
//! is no bypass here to get wrong. See [`crate::config::DashboardAccess`].
//!
//! The session cookie carries a random token minted at startup, never the
//! password and never anything derived from it. A restart invalidates every
//! session, which is the correct behaviour for a process whose whole state is a
//! directory of files.

use base64::Engine as _;
use std::io::Read;
use subtle::ConstantTimeEq;

/// Cookie name for the dashboard session.
pub const SESSION_COOKIE: &str = "art_session";

/// Configured dashboard credentials, plus the session token for this process.
pub struct AdminAuth {
    user: String,
    password: String,
    /// Random per-process token. Hex, so it is cookie-safe without escaping.
    session: String,
    /// Precomputed `Basic …` header value, so a request costs one comparison
    /// rather than a base64 decode and two string splits.
    basic_header: String,
}

impl AdminAuth {
    /// `None` when no password is configured — the dashboard is then not
    /// mounted at all.
    pub fn new(user: String, password: String) -> AdminAuth {
        let session = random_token();
        let basic_header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        );
        AdminAuth {
            user,
            password,
            session,
            basic_header,
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// The value to put in the session cookie once a login succeeds.
    pub fn session_token(&self) -> &str {
        &self.session
    }

    /// Constant-time check of a submitted login form.
    pub fn verify_login(&self, user: &str, password: &str) -> bool {
        // Both compared, and both in constant time: a username oracle is a
        // smaller leak than a password oracle but it is still a leak.
        ct_eq(user.as_bytes(), self.user.as_bytes())
            & ct_eq(password.as_bytes(), self.password.as_bytes())
    }

    /// Constant-time check of a session cookie value.
    pub fn verify_session(&self, presented: &str) -> bool {
        ct_eq(presented.as_bytes(), self.session.as_bytes())
    }

    /// Constant-time check of an `Authorization: Basic …` header, so `curl -u`
    /// and scripted checks work without a login round-trip.
    pub fn verify_basic(&self, header: &str) -> bool {
        ct_eq(header.as_bytes(), self.basic_header.as_bytes())
    }
}

/// Length-independent equality. `ct_eq` is only constant-time for equal-length
/// inputs, so unequal lengths short-circuit to `false` *without* comparing —
/// which leaks length, not content. That is the standard trade and it is why
/// nothing here is length-sensitive beyond the password itself.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// 32 bytes from the kernel, hex-encoded.
///
/// Straight from `/dev/urandom` rather than pulling in a random-number crate:
/// this is Linux-only code that already speaks to the kernel directly
/// everywhere else, and there is exactly one call site.
fn random_token() -> String {
    let mut buf = [0u8; 32];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => hex::encode(buf),
        // A machine that cannot produce randomness cannot host a session; a
        // guessable token would be worse than no dashboard.
        Err(e) => {
            panic!("cannot read /dev/urandom to mint a session token: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> AdminAuth {
        AdminAuth::new("admin".into(), "hunter2".into())
    }

    #[test]
    fn accepts_the_configured_credentials() {
        let a = auth();
        assert!(a.verify_login("admin", "hunter2"));
    }

    #[test]
    fn rejects_wrong_user_or_password() {
        let a = auth();
        for (u, p) in [
            ("admin", "hunter3"),
            ("admin", ""),
            ("root", "hunter2"),
            ("", "hunter2"),
            ("admin ", "hunter2"),
            ("admin", "hunter2 "),
            // A prefix must not pass — the length check comes first.
            ("admin", "hunter"),
            ("admin", "hunter22"),
        ] {
            assert!(!a.verify_login(u, p), "accepted {u:?}/{p:?}");
        }
    }

    #[test]
    fn session_tokens_are_long_random_hex() {
        let a = auth();
        let t = a.session_token();
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(a.verify_session(t));
        assert!(!a.verify_session(""));
        assert!(!a.verify_session(&"0".repeat(64)));
    }

    #[test]
    fn each_process_mints_a_distinct_session() {
        // Two stores on one host must not accept each other's cookies, and a
        // restart must invalidate outstanding sessions.
        assert_ne!(auth().session_token(), auth().session_token());
    }

    #[test]
    fn session_token_is_not_derived_from_the_password() {
        let a = AdminAuth::new("admin".into(), "hunter2".into());
        let t = a.session_token();
        assert!(!t.contains("hunter2"));
        // And two instances with identical credentials differ, so the token
        // cannot be a function of them.
        let b = AdminAuth::new("admin".into(), "hunter2".into());
        assert_ne!(t, b.session_token());
    }

    #[test]
    fn accepts_a_correct_basic_header() {
        let a = auth();
        let good = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:hunter2")
        );
        assert!(a.verify_basic(&good));
    }

    #[test]
    fn rejects_bad_basic_headers() {
        let a = auth();
        let wrong = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:nope")
        );
        for h in [
            wrong.as_str(),
            "Basic not-base64",
            "Basic ",
            "Bearer hunter2",
            "admin:hunter2",
            "",
        ] {
            assert!(!a.verify_basic(h), "accepted {h:?}");
        }
    }

    #[test]
    fn a_password_containing_a_colon_still_round_trips() {
        // Basic auth splits on the first colon, so the password may contain
        // colons and must still match.
        let a = AdminAuth::new("admin".into(), "a:b:c".into());
        let good = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("admin:a:b:c")
        );
        assert!(a.verify_basic(&good));
        assert!(a.verify_login("admin", "a:b:c"));
    }
}
