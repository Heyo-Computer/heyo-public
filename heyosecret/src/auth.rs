use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Name of the dashboard session cookie.
pub const SESSION_COOKIE: &str = "heyosecret_session";

/// Constant-time comparison of a submitted admin password against the
/// configured one. Returns false when no password is configured.
pub fn verify_admin_password(configured: &str, submitted: &str) -> bool {
    let configured = configured.trim();
    if configured.is_empty() {
        return false;
    }
    submitted
        .as_bytes()
        .ct_eq(configured.as_bytes())
        .into()
}

/// Mint a signed session token valid for `ttl_seconds` from now.
///
/// Format: `<expiry_unix>.<hex(hmac_sha256(key, expiry_unix))>`.
pub fn mint_session(signing_key: &[u8], ttl_seconds: u64) -> String {
    let expiry = Utc::now().timestamp() + ttl_seconds as i64;
    let payload = expiry.to_string();
    let sig = sign(signing_key, payload.as_bytes());
    format!("{payload}.{}", hex::encode(sig))
}

/// Validate a session token: signature must verify and the token must not be
/// expired.
pub fn session_is_valid(signing_key: &[u8], token: &str) -> bool {
    let Some((payload, sig_hex)) = token.split_once('.') else {
        return false;
    };
    let Ok(provided_sig) = hex::decode(sig_hex) else {
        return false;
    };
    let expected_sig = sign(signing_key, payload.as_bytes());
    // Constant-time signature comparison.
    let sig_ok: bool = expected_sig.ct_eq(&provided_sig).into();
    if !sig_ok {
        return false;
    }
    let Ok(expiry) = payload.parse::<i64>() else {
        return false;
    };
    expiry > Utc::now().timestamp()
}

fn sign(signing_key: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(signing_key)
        .expect("HMAC accepts keys of any length");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

/// Extract the session token from the incoming `Cookie` header, if present.
pub fn session_cookie_value(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(COOKIE)?.to_str().ok()?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return Some(value.to_string());
        }
    }
    None
}

/// True when the request carries a valid dashboard session cookie.
pub fn request_has_valid_session(signing_key: &[u8], headers: &HeaderMap) -> bool {
    session_cookie_value(headers)
        .map(|token| session_is_valid(signing_key, &token))
        .unwrap_or(false)
}

/// Build a `Set-Cookie` header that installs the session token.
pub fn set_session_cookie(token: &str, ttl_seconds: u64, secure: bool) -> HeaderValue {
    build_cookie(token, ttl_seconds as i64, secure)
}

/// Build a `Set-Cookie` header that clears the session (used on logout).
pub fn clear_session_cookie(secure: bool) -> HeaderValue {
    build_cookie("", 0, secure)
}

fn build_cookie(value: &str, max_age: i64, secure: bool) -> HeaderValue {
    let mut cookie = format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    // Values are hex/decimal ASCII (or empty), so this never fails.
    HeaderValue::from_str(&cookie).expect("session cookie is valid ASCII")
}

/// Attach a `Set-Cookie` header to an outgoing header map.
pub fn apply_set_cookie(headers: &mut HeaderMap, cookie: HeaderValue) {
    headers.append(SET_COOKIE, cookie);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_valid_session() {
        let key = b"unit-test-signing-key";
        let token = mint_session(key, 60);
        assert!(session_is_valid(key, &token));
    }

    #[test]
    fn rejects_expired_session() {
        let key = b"unit-test-signing-key";
        // ttl of 0 => expiry == now, which is not strictly greater than now.
        let token = mint_session(key, 0);
        assert!(!session_is_valid(key, &token));
    }

    #[test]
    fn rejects_tampered_signature() {
        let key = b"unit-test-signing-key";
        let token = mint_session(key, 60);
        let (payload, _) = token.split_once('.').unwrap();
        let forged = format!("{payload}.{}", hex::encode([0u8; 32]));
        assert!(!session_is_valid(key, &forged));
    }

    #[test]
    fn rejects_wrong_key() {
        let token = mint_session(b"key-a", 60);
        assert!(!session_is_valid(b"key-b", &token));
    }

    #[test]
    fn password_check_is_empty_safe() {
        assert!(!verify_admin_password("", "anything"));
        assert!(!verify_admin_password("   ", "anything"));
        assert!(verify_admin_password("hunter2", "hunter2"));
        assert!(!verify_admin_password("hunter2", "hunter3"));
    }

    #[test]
    fn parses_cookie_among_many() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("foo=bar; heyosecret_session=abc.def; baz=qux"),
        );
        assert_eq!(session_cookie_value(&headers).as_deref(), Some("abc.def"));
    }
}
