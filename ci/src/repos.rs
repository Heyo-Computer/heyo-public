//! Registered repositories and the tokens they submit with.
//!
//! `git submit` needs a credential, and until now that credential was
//! `CI_WEBHOOK_SECRET` — one shared secret, handed to every person who submits
//! from every repository. That has three problems and they are all the same
//! problem: it cannot be scoped, it cannot be revoked for one repository, and it
//! cannot say *which* repository is submitting. A leaked copy is a leak of the
//! whole system, and rotating it means finding everyone.
//!
//! A registered repository fixes all three. It has its own tokens, each
//! revocable on its own, and the token *is* the statement of which repository
//! the submit is for — so the payload's `repository` field stops being something
//! the server has to take on trust.
//!
//! ## Hashed bearer, not an HMAC over the body
//!
//! The shared-secret path signs the request body with HMAC-SHA256, which proves
//! possession without ever transmitting the secret. That is the stronger
//! property on the wire, and it is kept for compatibility. It is not the right
//! one here, because **verifying an HMAC requires the server to hold the key**:
//! one read of `ci_repo_token` would disclose every repository's submit
//! credential, and a submit credential is arbitrary code execution on a runner.
//!
//! A repository token is therefore a bearer token, stored as a SHA-256 digest.
//! The secret transits inside TLS — which this deployment already requires,
//! since app-lb's gate terminates it — and at rest the database holds something
//! that cannot be used to submit. That is the trade worth making for a
//! credential an operator mints, lists and revokes from a dashboard.
//!
//! ## The shape of a token
//!
//! ```text
//! cis_<key id>.<secret>
//!     └ ci_repo_token.id, not secret; it is what the lookup is by
//!                   └ 32 random bytes, base64url. Only its SHA-256 is stored.
//! ```
//!
//! The key id is in the token so verification is one indexed lookup followed by
//! one comparison. Without it the server would have to hash the presentation
//! against every stored digest, which is a table scan on an unauthenticated
//! request — a submit endpoint on the open internet is exactly where that
//! matters.

use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Marks a string as one of ours. Deliberately distinctive: a token that
/// escapes into a repository is greppable, and a value pasted into the wrong
/// field is recognisable in a log without being usable from one.
pub const TOKEN_PREFIX: &str = "cis_";

/// A freshly minted token. The plaintext exists only in this struct and in the
/// one response that shows it; the database gets [`Self::secret_hash`].
pub struct Minted {
    /// `ci_repo_token.id`, and the lookup half of the token.
    pub key_id: String,
    /// SHA-256 of the secret half, hex. What is stored.
    pub secret_hash: String,
    /// The whole token, shown once and never recoverable afterwards.
    pub plaintext: String,
}

/// Mint a token for a token row that will be created with `key_id`.
pub fn mint(key_id: &str) -> Minted {
    let secret = random_secret();
    Minted {
        key_id: key_id.to_string(),
        secret_hash: hash_secret(&secret),
        plaintext: format!("{TOKEN_PREFIX}{key_id}.{secret}"),
    }
}

/// Split a presented token into `(key id, secret)`.
///
/// Returns `None` for anything that is not shaped like one of ours, so a
/// malformed value costs no database round trip.
pub fn parse(token: &str) -> Option<(&str, &str)> {
    let rest = token.trim().strip_prefix(TOKEN_PREFIX)?;
    let (key_id, secret) = rest.split_once('.')?;
    if key_id.is_empty() || secret.is_empty() {
        return None;
    }
    // The key id reaches a parameterised query, so injection is not the risk —
    // but it is also interpolated into log lines and error messages, and an id
    // is only ever the alphabet `new_id` mints.
    if !key_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    Some((key_id, secret))
}

pub fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Whether a presented secret matches a stored digest.
///
/// Constant-time, like every other credential comparison here. Both sides are
/// hex digests of fixed length, so an early return would leak how much of a
/// digest a guess got right — and a digest is guessable in a way a 256-bit
/// secret is not, because the attacker chooses the input.
pub fn secret_matches(secret: &str, stored_hash: &str) -> bool {
    let computed = hash_secret(secret);
    computed.len() == stored_hash.len()
        && bool::from(computed.as_bytes().ct_eq(stored_hash.as_bytes()))
}

/// 32 random bytes, base64url without padding.
///
/// From two v4 UUIDs, for the reason `config::random_key` gives: `uuid` is
/// already a dependency and its v4 generator is a CSPRNG, so this avoids
/// linking `rand` for one call.
fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether two clone URLs name the same repository.
///
/// `git@github.com:me/app.git` and `https://github.com/me/app` are the same
/// repository written two ways, and someone who registers one spelling and
/// submits from a clone of the other should not be told the token is for a
/// different repository. Compared on the host-and-path tail with the `.git`
/// suffix and any credentials removed.
pub fn same_repo(a: &str, b: &str) -> bool {
    !a.trim().is_empty() && normalize(a) == normalize(b)
}

/// The comparison key for a clone URL, and the unique key of `ci_repo`.
pub fn normalize(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/');
    // Strip a scheme, then any `user@` — an SSH URL carries one and an HTTPS
    // clone URL may carry a token.
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..];
    }
    if let Some(idx) = s.rfind('@') {
        s = &s[idx + 1..];
    }
    // `github.com:me/app` and `github.com/me/app` are the SSH and HTTPS
    // spellings of one path.
    let s = s.replacen(':', "/", 1);
    s.trim_end_matches(".git").to_ascii_lowercase()
}

/// A display name for a repository nobody named: `me/app` from any spelling of
/// its clone URL.
///
/// Two segments rather than one, because a dashboard listing `app`, `app` and
/// `app` for three owners is a dashboard nobody can use.
pub fn name_from_url(url: &str) -> String {
    let normalized = normalize(url);
    let mut segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    // Drop the host; what identifies the repository is the tail.
    if segments.len() > 2 {
        segments = segments.split_off(segments.len() - 2);
    } else if segments.len() == 2 && segments[0].contains('.') {
        // `github.com/app` — a host and one segment, so the host is not part of
        // the name.
        segments.remove(0);
    }
    let name = segments.join("/");
    if name.is_empty() { normalized } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_verifies_against_its_own_digest() {
        let m = mint("019fca648a6e-00000000");
        let (key_id, secret) = parse(&m.plaintext).expect("parses");
        assert_eq!(key_id, "019fca648a6e-00000000");
        assert!(secret_matches(secret, &m.secret_hash));
    }

    /// The property the whole storage decision rests on: what is stored must
    /// not be usable to submit, and must not be the token itself.
    #[test]
    fn the_stored_value_is_a_digest_and_not_the_secret() {
        let m = mint("k1");
        let (_, secret) = parse(&m.plaintext).unwrap();
        assert_ne!(m.secret_hash, secret);
        assert!(!m.plaintext.contains(&m.secret_hash));
        assert_eq!(m.secret_hash.len(), 64, "sha256, hex");
    }

    #[test]
    fn two_mints_do_not_collide() {
        let a = mint("k1");
        let b = mint("k1");
        assert_ne!(a.plaintext, b.plaintext);
        assert_ne!(a.secret_hash, b.secret_hash);
    }

    #[test]
    fn a_wrong_secret_is_refused() {
        let m = mint("k1");
        assert!(!secret_matches("not-the-secret", &m.secret_hash));
        assert!(!secret_matches("", &m.secret_hash));
    }

    /// A malformed presentation must not reach the database at all — this is an
    /// unauthenticated public endpoint.
    #[test]
    fn anything_not_shaped_like_a_token_is_rejected_before_a_lookup() {
        for bad in [
            "",
            "cis_",
            "cis_.secret",
            "cis_keyid.",
            "keyid.secret",
            "Bearer cis_k.s",
            "cis_key id.secret",
            // The key id reaches log lines; keep it to the id alphabet.
            "cis_key/../id.secret",
            "cis_key'id.secret",
        ] {
            assert!(parse(bad).is_none(), "{bad:?} must not parse");
        }
        assert_eq!(parse("  cis_k1.s3cret  "), Some(("k1", "s3cret")));
    }

    /// The same repository written the SSH way and the HTTPS way is one
    /// repository, or a token registered from one clone is useless from another.
    #[test]
    fn the_two_spellings_of_a_clone_url_are_the_same_repository() {
        for (a, b) in [
            ("git@github.com:me/app.git", "https://github.com/me/app"),
            (
                "https://github.com/me/app.git",
                "https://github.com/me/app/",
            ),
            ("git@github.com:Me/App.git", "https://github.com/me/app"),
            (
                "https://x-token:abc123@github.com/me/app.git",
                "git@github.com:me/app.git",
            ),
        ] {
            assert!(same_repo(a, b), "{a} should match {b}");
        }
    }

    #[test]
    fn different_repositories_do_not_match() {
        assert!(!same_repo(
            "git@github.com:me/app.git",
            "git@github.com:me/other.git"
        ));
        assert!(!same_repo(
            "git@github.com:me/app.git",
            "git@gitlab.com:me/app.git"
        ));
        // A prefix is not a match: `app` must not match `app2`.
        assert!(!same_repo(
            "git@github.com:me/app.git",
            "git@github.com:me/app2.git"
        ));
    }

    /// An empty URL matching everything would make "this token is for another
    /// repository" unenforceable against a client that sends no remote.
    #[test]
    fn an_empty_url_matches_nothing() {
        assert!(!same_repo("", ""));
        assert!(!same_repo("", "git@github.com:me/app.git"));
        assert!(!same_repo("git@github.com:me/app.git", ""));
    }

    #[test]
    fn a_name_is_derived_from_the_tail_of_a_clone_url() {
        assert_eq!(name_from_url("git@github.com:me/app.git"), "me/app");
        assert_eq!(name_from_url("https://github.com/Me/App"), "me/app");
        assert_eq!(
            name_from_url("https://git.example.com/team/group/app.git"),
            "group/app"
        );
        assert_eq!(name_from_url("/home/sarocu/Projects/ci"), "projects/ci");
        assert_eq!(name_from_url("app"), "app");
    }
}
