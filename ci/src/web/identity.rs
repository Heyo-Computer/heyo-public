//! Who is making this request, according to app-lb.
//!
//! app-lb's proxy sets exactly three headers on a gated deployment
//! (`app-lb/src/auth.rs:993`, applied at `src/proxy.rs:600-616`):
//!
//! ```text
//! x-auth-request-email
//! x-auth-request-user     the stable Google `sub`
//! x-auth-request-name
//! ```
//!
//! It **removes all three unconditionally before setting them**, which is the
//! anti-spoof measure and the reason they can be trusted — but only on a gated
//! deployment. An ungated one forwards whatever the client sent, so
//! [`Identity::from_headers`] is safe exactly to the extent that the deployment
//! spec carries an `auth` block with `forward_identity: true`.
//!
//! `x-auth-request-user` is the primary key, not the email: it is Google's
//! stable subject, and it survives an address change that would otherwise
//! silently create a second account for the same person.
//!
//! **A token is not a person.** app-lb forwards no identity headers for an
//! app-token request, so a machine caller arrives anonymous. That is why
//! authorization here is a property of the route, not of a role attached to
//! every request.

pub const HEADER_EMAIL: &str = "x-auth-request-email";
pub const HEADER_USER: &str = "x-auth-request-user";
pub const HEADER_NAME: &str = "x-auth-request-name";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Google's stable `sub`. The primary key for a person.
    pub subject: String,
    pub email: String,
    pub name: Option<String>,
}

impl Identity {
    /// Read an identity out of request headers, or `None` when app-lb did not
    /// supply one (an app-token caller, or a local run with no gate).
    ///
    /// Both `subject` and `email` must be present and non-empty. A half-set
    /// identity means something upstream is misconfigured, and treating it as
    /// anonymous is the safe reading — inventing a subject from the email would
    /// create an account keyed on the mutable half.
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Option<Self> {
        let get = |name: &str| -> Option<String> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        Some(Self {
            subject: get(HEADER_USER)?,
            email: get(HEADER_EMAIL)?,
            name: get(HEADER_NAME),
        })
    }

    /// What to show in a UI: the display name if app-lb had one, else the email.
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
}

// There is deliberately no `FromRequestParts` impl. An anonymous request is a
// normal state — health checks, the submit endpoint, and every app-token caller
// arrive without headers — so a handler takes `HeaderMap` and calls
// `from_headers`, making the absent case something it must visibly decide about
// rather than something an extractor rejects on its behalf.

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_full_set_of_headers_becomes_an_identity() {
        let id = Identity::from_headers(&headers(&[
            (HEADER_USER, "108143..."),
            (HEADER_EMAIL, "sam@sarocu.com"),
            (HEADER_NAME, "Sam Currie"),
        ]))
        .expect("identity");
        assert_eq!(id.subject, "108143...");
        assert_eq!(id.email, "sam@sarocu.com");
        assert_eq!(id.display(), "Sam Currie");
    }

    /// app-lb omits the name for accounts that have none; that is not a reason
    /// to treat the request as anonymous.
    #[test]
    fn a_missing_display_name_falls_back_to_the_email() {
        let id = Identity::from_headers(&headers(&[
            (HEADER_USER, "sub-1"),
            (HEADER_EMAIL, "nobody@example.com"),
        ]))
        .expect("identity");
        assert_eq!(id.name, None);
        assert_eq!(id.display(), "nobody@example.com");
    }

    /// An app-token caller arrives with no identity headers at all — "a token is
    /// not a person".
    #[test]
    fn no_headers_is_anonymous_rather_than_an_error() {
        assert_eq!(Identity::from_headers(&HeaderMap::new()), None);
    }

    /// Half an identity means something upstream is wrong. Keying an account on
    /// the email because the subject is missing would key it on the mutable half.
    #[test]
    fn a_partial_identity_is_rejected() {
        assert_eq!(
            Identity::from_headers(&headers(&[(HEADER_EMAIL, "sam@sarocu.com")])),
            None
        );
        assert_eq!(
            Identity::from_headers(&headers(&[(HEADER_USER, "sub-1")])),
            None
        );
    }

    #[test]
    fn empty_header_values_are_treated_as_absent() {
        assert_eq!(
            Identity::from_headers(&headers(&[(HEADER_USER, "  "), (HEADER_EMAIL, "a@b.c")])),
            None
        );
    }
}
