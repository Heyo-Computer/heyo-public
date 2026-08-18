//! Live log streaming, and the token that makes it reachable.
//!
//! ## Why this needs a token at all
//!
//! Behind an app-lb `AuthGate`, **the gate admits browsers and nothing else**.
//! The split is `Accept: text/html`, so a page's own `EventSource` — which sends
//! `Accept: text/event-stream` — gets `401 {"error":"authentication required"}`
//! exactly like curl does. A dashboard whose live logs never connect is a
//! dashboard that looks broken to the one person who passed the login.
//!
//! So the stream route lives in `public_paths` and carries its own credential: a
//! short-lived token, scoped to one job, minted by the page that opens it. The
//! page was itself fetched through the gate, so only someone who authenticated
//! can obtain one.
//!
//! ## The token
//!
//! `<expiry_unix>.<hex hmac-sha256(expiry ‖ run ‖ job)>`, signed with a key
//! generated fresh in each process. Three properties matter:
//!
//! - **Scoped to one job.** A token for one job cannot read another's logs, so
//!   handing one to a colleague shares that page, not the whole dashboard.
//! - **Short-lived.** Minutes, not sessions. A URL copied out of devtools stops
//!   working on its own.
//! - **Not persisted.** A restart invalidates every outstanding token, which is
//!   correct: the browser reconnects by re-fetching the page, and that fetch
//!   goes through the gate again.

use crate::config::Config;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

/// How long a freshly minted stream token lasts.
///
/// Long enough for a build nobody is watching to still be tailable when someone
/// opens the tab, short enough that a leaked URL is not a standing grant. The
/// page mints a new one on every load.
const TOKEN_TTL_SECS: u64 = 60 * 60;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn sign(key: &[u8; 32], expiry: u64, run_id: &str, job_key: &str) -> String {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("hmac accepts any key length");
    // Length-prefixed rather than concatenated: without it, run `ab` + job `c`
    // and run `a` + job `bc` sign the same bytes, and a token for one job would
    // verify for another.
    mac.update(&expiry.to_be_bytes());
    mac.update(&(run_id.len() as u64).to_be_bytes());
    mac.update(run_id.as_bytes());
    mac.update(&(job_key.len() as u64).to_be_bytes());
    mac.update(job_key.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Mint a token for one job's log stream.
pub fn mint(config: &Config, run_id: &str, job_key: &str) -> String {
    let expiry = now() + TOKEN_TTL_SECS;
    format!(
        "{expiry}.{}",
        sign(&config.stream_key, expiry, run_id, job_key)
    )
}

/// Check a token against the job it claims to be for.
pub fn verify(config: &Config, token: &str, run_id: &str, job_key: &str) -> bool {
    let Some((expiry_str, provided)) = token.split_once('.') else {
        return false;
    };
    let Ok(expiry) = expiry_str.parse::<u64>() else {
        return false;
    };
    if expiry < now() {
        return false;
    }
    let expected = sign(&config.stream_key, expiry, run_id, job_key);
    // Constant-time: a byte-wise `==` returns at the first difference, which
    // leaks the correct prefix one request at a time.
    provided.len() == expected.len() && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn config() -> Arc<Config> {
        crate::web::tests::test_config()
    }

    #[test]
    fn a_freshly_minted_token_verifies_for_its_own_job() {
        let c = config();
        let t = mint(&c, "run-1", "build");
        assert!(verify(&c, &t, "run-1", "build"));
    }

    /// The scoping property: a token is for one job, so sharing a page shares
    /// that page and not the whole dashboard.
    #[test]
    fn a_token_does_not_verify_for_a_different_job_or_run() {
        let c = config();
        let t = mint(&c, "run-1", "build");
        assert!(!verify(&c, &t, "run-1", "deploy"));
        assert!(!verify(&c, &t, "run-2", "build"));
        assert!(!verify(&c, &t, "", ""));
    }

    /// Regression: concatenating the two fields without lengths makes
    /// (`ab`,`c`) and (`a`,`bc`) sign identical bytes, so a token for one job
    /// verifies for another.
    #[test]
    fn field_boundaries_are_unambiguous() {
        let c = config();
        let t = mint(&c, "ab", "c");
        assert!(verify(&c, &t, "ab", "c"));
        assert!(!verify(&c, &t, "a", "bc"), "the split must be unambiguous");
    }

    #[test]
    fn an_expired_token_is_refused() {
        let c = config();
        let past = now() - 1;
        let t = format!("{past}.{}", sign(&c.stream_key, past, "run-1", "build"));
        assert!(!verify(&c, &t, "run-1", "build"));
    }

    /// Moving the expiry forward must not work without re-signing.
    #[test]
    fn the_expiry_cannot_be_extended_by_editing_the_token() {
        let c = config();
        let t = mint(&c, "run-1", "build");
        let (_, sig) = t.split_once('.').unwrap();
        let forged = format!("{}.{sig}", now() + 86_400 * 365);
        assert!(!verify(&c, &forged, "run-1", "build"));
    }

    #[test]
    fn malformed_tokens_are_refused_rather_than_panicking() {
        let c = config();
        for bad in ["", ".", "abc", "abc.def", "999999999999.zz", "..", "1.1.1"] {
            assert!(!verify(&c, bad, "run-1", "build"), "{bad:?}");
        }
    }

    /// Each process generates its own key, so a restart invalidates outstanding
    /// tokens — the browser re-fetches the page, which goes through the gate.
    #[test]
    fn a_token_from_another_process_does_not_verify() {
        let a = config();
        let mut other = Config::from_env().expect("config");
        other.stream_key = [7u8; 32];
        let t = mint(&a, "run-1", "build");
        assert!(!verify(&other, &t, "run-1", "build"));
    }
}
