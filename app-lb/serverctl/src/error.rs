//! What can go wrong, as something a caller can branch on.
//!
//! The CLI this crate grew out of used `anyhow` throughout, and some of its
//! messages named CLI commands ("run `serverctl login`"). A library cannot ship
//! that: a caller deciding whether to retry, wake a VM, or re-mint a token needs
//! the *shape* of the failure, not a sentence about it.
//!
//! # Two error bodies
//!
//! app-lb answers a failed request in one of two ways, and a client has to
//! handle both:
//!
//! - `{"error": "…"}` — every handler-level 4xx/5xx.
//! - **plain text** — the `401`, and every axum extractor rejection: `415` for a
//!   missing content-type, `400` for malformed JSON, and `422` for well-formed
//!   JSON of the wrong shape.
//!
//! So [`Error::from_response`] tries the envelope, falls back to the body as
//! text, and falls back again to a message derived from the status. It never
//! assumes JSON.

use std::fmt;

/// The kind of credential that was presented, for a 401 worth explaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// Nothing was sent.
    None,
    /// A username and password.
    Basic,
    /// An app-token.
    Token,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// `401`. The credential was missing, wrong, revoked or expired — app-lb
    /// does not distinguish those, deliberately, so that token ids cannot be
    /// enumerated by watching which failure comes back.
    #[error("{}", unauthorized_message(*.presented))]
    Unauthorized { presented: Credential },

    /// `403`. The credential was good and its scope was not. Re-presenting it
    /// will not help; a wider token, or a different one, is needed.
    #[error("{message}")]
    Forbidden { message: String },

    /// `404` on a named thing.
    #[error("no {kind} {name:?}")]
    NotFound { kind: &'static str, name: String },

    /// `409`. A job is already running, or a secret is still referenced.
    #[error("{message}")]
    Conflict { message: String },

    /// `409` from `exec`/`shell` with `wake: false` and nothing running.
    ///
    /// Separate from [`Error::Conflict`] because the remedy is specific: retry
    /// with `wake` set, or scale the deployment up first.
    #[error("deployment {deployment:?} has no running VM (retry with wake)")]
    NoRunningVm { deployment: String },

    /// `503` from `exec`/`shell`: a VM was asked for and none became available
    /// inside the deployment's `cold_start_timeout_secs`.
    ///
    /// Worth retrying — a boot that overran the deadline usually finishes.
    #[error("deployment {deployment:?} had no VM ready within its cold-start timeout")]
    ColdStartTimeout { deployment: String },

    /// `502`. app-lb reached the daemon and the daemon failed.
    ///
    /// For `exec` this specifically includes app-lb's own call timing out —
    /// **the command is still running in the guest**. See
    /// [`crate::api::Client::exec`].
    #[error("{message}")]
    Upstream { message: String },

    /// `400`, or any other `{"error": …}` this crate has no specific variant
    /// for.
    #[error("{message}")]
    Api { status: u16, message: String },

    /// A response this crate could not interpret: an extractor rejection, an
    /// empty-bodied router 404/405, or an intermediary's error page.
    #[error("unexpected HTTP {status}{}", if .body.is_empty() { String::new() } else { format!(": {}", .body) })]
    Malformed { status: u16, body: String },

    /// The request never got an answer.
    #[error("could not reach {0}")]
    Transport(#[source] reqwest::Error),

    /// A `200` whose body was not the shape this build expects. Distinct from
    /// [`Error::Malformed`], which is about *failed* requests.
    #[error("could not read the response: {0}")]
    Decode(#[source] serde_json::Error),

    /// The WebSocket carrying a shell failed.
    #[error("shell connection failed: {0}")]
    Shell(String),

    /// A `wait_for_*` helper gave up.
    #[error("{what} did not finish within {}s", .after.as_secs())]
    Timeout {
        what: String,
        after: std::time::Duration,
    },

    /// Bad input, caught before anything was sent.
    #[error("{0}")]
    Invalid(String),
}

fn unauthorized_message(presented: Credential) -> String {
    match presented {
        Credential::None => {
            "authentication required, and no credential was sent — supply a username and \
             password, or an app-token"
        }
        Credential::Basic => "the username or password was not accepted",
        Credential::Token => {
            "the app-token was not accepted — it may be wrong, revoked or expired \
             (app-lb does not say which)"
        }
    }
    .to_string()
}

/// The `{"error": …}` envelope. Its absence is normal, not exceptional.
#[derive(serde::Deserialize)]
struct Envelope {
    error: String,
}

impl Error {
    /// Build an error from a failed response.
    ///
    /// `kind`/`name` describe what was being addressed, so a `404` can say
    /// `no deployment "demo"` rather than `HTTP 404`. `presented` is what the
    /// caller sent, so a `401` can say something useful about it.
    pub fn from_response(
        status: u16,
        body: &str,
        kind: &'static str,
        name: &str,
        presented: Credential,
    ) -> Self {
        // The envelope if there is one; otherwise the body verbatim, which is
        // where axum's plain-text rejections live.
        let message = serde_json::from_str::<Envelope>(body)
            .map(|e| e.error)
            .ok()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| Some(body.trim().to_string()).filter(|b| !b.is_empty()));

        match (status, message) {
            (401, _) => Self::Unauthorized { presented },
            (403, m) => Self::Forbidden {
                message: m.unwrap_or_else(|| "forbidden".into()),
            },
            (404, Some(m)) => {
                // A router-level 404 has no envelope and no useful body; a
                // handler-level one names the thing. Only the latter should be
                // reported as a missing object.
                if m.starts_with("no ") {
                    Self::NotFound {
                        kind,
                        name: name.to_string(),
                    }
                } else {
                    Self::Malformed { status, body: m }
                }
            }
            (404, None) => Self::NotFound {
                kind,
                name: name.to_string(),
            },
            // These two are told apart by their message because app-lb answers
            // both with a 409, and the remedies are different: one is "retry
            // with wake", the other is "wait for the job".
            (409, Some(m)) if m.contains("no running VM") => Self::NoRunningVm {
                deployment: name.to_string(),
            },
            (409, m) => Self::Conflict {
                message: m.unwrap_or_else(|| "conflict".into()),
            },
            (503, _) => Self::ColdStartTimeout {
                deployment: name.to_string(),
            },
            (502, m) => Self::Upstream {
                message: m.unwrap_or_else(|| "the daemon did not answer".into()),
            },
            // 415/422 and a bare 400 from an extractor are not the API speaking;
            // they mean the request was malformed before a handler saw it.
            (400 | 415 | 422, m) if serde_json::from_str::<Envelope>(body).is_err() => {
                Self::Malformed {
                    status,
                    body: m.unwrap_or_default(),
                }
            }
            (s, Some(m)) => Self::Api { status: s, message: m },
            (s, None) => Self::Malformed {
                status: s,
                body: String::new(),
            },
        }
    }

    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// Deliberately conservative: a cold start that overran and a daemon that
    /// failed are both worth another go, and nothing else is. In particular a
    /// `409` is *not* retryable — the job that is already running will still be
    /// running.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ColdStartTimeout { .. } | Self::Upstream { .. } | Self::Transport(_)
        )
    }

    /// Whether this is a credential problem rather than a request problem.
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Unauthorized { .. } | Self::Forbidden { .. })
    }

    /// The HTTP status behind this error, when there was one.
    pub fn status(&self) -> Option<u16> {
        Some(match self {
            Self::Unauthorized { .. } => 401,
            Self::Forbidden { .. } => 403,
            Self::NotFound { .. } => 404,
            Self::Conflict { .. } | Self::NoRunningVm { .. } => 409,
            Self::ColdStartTimeout { .. } => 503,
            Self::Upstream { .. } => 502,
            Self::Api { status, .. } | Self::Malformed { status, .. } => *status,
            _ => return None,
        })
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Transport(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Decode(e)
    }
}

/// So `Error::Transport`'s message can name the host without holding a `String`.
impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "no credential",
            Self::Basic => "a username and password",
            Self::Token => "an app-token",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(status: u16, body: &str) -> Error {
        Error::from_response(status, body, "deployment", "demo", Credential::Token)
    }

    #[test]
    fn the_json_envelope_is_unwrapped() {
        let e = from(400, r#"{"error":"command must not be empty"}"#);
        assert!(matches!(e, Error::Api { status: 400, .. }));
        assert_eq!(e.to_string(), "command must not be empty");
    }

    /// The 401, and every axum extractor rejection, are plain text. A client
    /// that assumes the envelope reports "expected value at line 1 column 1"
    /// instead of what actually happened.
    #[test]
    fn a_plain_text_body_is_not_mistaken_for_json() {
        let e = from(401, "authentication required\n");
        assert!(matches!(e, Error::Unauthorized { .. }));
        assert!(e.to_string().contains("app-token"), "{e}");

        let e = from(415, "Expected request with `Content-Type: application/json`");
        assert!(matches!(e, Error::Malformed { status: 415, .. }));
        assert!(e.to_string().contains("Content-Type"), "{e}");

        // Well-formed JSON of the wrong shape is a 422, not the 400 you'd guess.
        let e = from(422, "Failed to deserialize the JSON body: missing field `command`");
        assert!(matches!(e, Error::Malformed { status: 422, .. }));
        assert!(e.to_string().contains("missing field"), "{e}");
    }

    #[test]
    fn an_empty_body_still_produces_something_sayable() {
        let e = from(405, "");
        assert!(matches!(e, Error::Malformed { status: 405, .. }));
        assert_eq!(e.to_string(), "unexpected HTTP 405");
    }

    #[test]
    fn a_missing_deployment_names_itself() {
        let e = from(404, r#"{"error":"no deployment \"demo\""}"#);
        assert!(matches!(&e, Error::NotFound { kind: "deployment", name } if name == "demo"));
    }

    /// A router-level 404 (an unknown *path*) is not a missing deployment, and
    /// saying it is would send someone looking for the wrong bug.
    #[test]
    fn an_unrouted_path_is_not_a_missing_object() {
        let e = from(404, "");
        assert!(matches!(e, Error::NotFound { .. }), "empty body: assume the object");
        let e = from(404, "<html>404 not found</html>");
        assert!(matches!(e, Error::Malformed { status: 404, .. }), "{e:?}");
    }

    /// Both are 409 and the remedies differ, so the message is what tells them
    /// apart. If app-lb ever rewords this, the fallback is `Conflict`, which is
    /// merely less specific rather than wrong.
    #[test]
    fn a_sleeping_vm_is_distinguishable_from_a_running_job() {
        let e = from(
            409,
            r#"{"error":"deployment \"demo\" has no running VM (pass wake=true to start one)"}"#,
        );
        assert!(matches!(&e, Error::NoRunningVm { deployment } if deployment == "demo"));

        let e = from(409, r#"{"error":"a build is already running"}"#);
        assert!(matches!(e, Error::Conflict { .. }));
    }

    #[test]
    fn the_two_timeouts_are_told_apart() {
        // The cold start overran: worth retrying.
        let e = from(503, r#"{"error":"…none became available…"}"#);
        assert!(matches!(e, Error::ColdStartTimeout { .. }));
        assert!(e.is_retryable());

        // The daemon failed: also worth retrying.
        let e = from(502, r#"{"error":"could not run the command in sb-1: timeout"}"#);
        assert!(matches!(e, Error::Upstream { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn retryability_is_conservative() {
        assert!(!from(409, r#"{"error":"a build is already running"}"#).is_retryable());
        assert!(!from(400, r#"{"error":"bad"}"#).is_retryable());
        assert!(!from(401, "authentication required\n").is_retryable());
        assert!(!from(403, r#"{"error":"out of scope"}"#).is_retryable());
        assert!(!from(404, "").is_retryable());
    }

    #[test]
    fn scope_failures_are_distinguishable_from_credential_failures() {
        let bad_creds = from(401, "authentication required\n");
        let bad_scope = from(403, r#"{"error":"this token is not scoped to deployment \"x\""}"#);
        assert!(bad_creds.is_auth() && bad_scope.is_auth());
        assert!(matches!(bad_creds, Error::Unauthorized { .. }));
        assert!(matches!(bad_scope, Error::Forbidden { .. }));
        // The 403 keeps app-lb's own explanation, which names the missing scope.
        assert!(bad_scope.to_string().contains("scoped to"), "{bad_scope}");
    }

    #[test]
    fn the_401_message_reflects_what_was_sent() {
        let says = |c| {
            Error::from_response(401, "authentication required\n", "deployment", "d", c).to_string()
        };
        assert!(says(Credential::None).contains("no credential was sent"));
        assert!(says(Credential::Basic).contains("username or password"));
        assert!(says(Credential::Token).contains("revoked or expired"));
    }

    #[test]
    fn statuses_round_trip() {
        for (status, body) in [
            (400u16, r#"{"error":"x"}"#),
            (403, r#"{"error":"x"}"#),
            (404, r#"{"error":"no deployment \"demo\""}"#),
            (409, r#"{"error":"x"}"#),
            (415, ""),
            (502, r#"{"error":"x"}"#),
            (503, r#"{"error":"x"}"#),
        ] {
            assert_eq!(from(status, body).status(), Some(status), "status {status}");
        }
        assert_eq!(
            Error::Invalid("nope".into()).status(),
            None,
            "an error raised before sending has no status"
        );
    }
}
