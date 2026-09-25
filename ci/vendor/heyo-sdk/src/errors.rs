use std::time::Duration;

use thiserror::Error;

/// Every fallible call in the SDK returns `Result<T, HeyoError>`. Variants
/// mirror the TypeScript SDK's error classes so the same recovery patterns
/// translate across languages.
#[derive(Debug, Error)]
pub enum HeyoError {
    /// No API key was provided and `HEYO_API_KEY` is unset.
    #[error("Missing API key. Pass `api_key` or set HEYO_API_KEY.")]
    Authentication,

    /// 4xx other than 401/403/404 (e.g. 400 Bad Request, 422 Unprocessable).
    #[error("{0}")]
    InvalidArgument(String),

    /// 404 Not Found.
    #[error("not found: {0}")]
    NotFound(String),

    /// 5xx, network error, or non-classified HTTP failure.
    #[error("api error ({status}): {message}")]
    Api {
        status: u16,
        message: String,
        body: Option<serde_json::Value>,
    },

    /// A `wait_for_*` call exceeded its budget.
    #[error("timeout after {0:?}: {1}")]
    Timeout(Duration, String),

    /// Sandbox provisioning ended in `failed`.
    #[error("sandbox {sandbox_id} failed: {reason}")]
    SandboxFailed {
        sandbox_id: String,
        reason: String,
    },

    /// Shell-stream WebSocket could not be (re)established.
    #[error("connection error: {0}")]
    Connection(String),

    /// Server refused to resume the shell session.
    #[error("shell session expired{}", .session_id.as_deref().map(|s| format!(" ({})", s)).unwrap_or_default())]
    SessionExpired { session_id: Option<String> },

    /// Remote shell exited unexpectedly.
    #[error("shell exited with code {0}")]
    ShellExit(i32),
}

impl HeyoError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        HeyoError::InvalidArgument(msg.into())
    }
    pub(crate) fn api(status: u16, message: impl Into<String>) -> Self {
        HeyoError::Api {
            status,
            message: message.into(),
            body: None,
        }
    }
    pub(crate) fn api_with_body(
        status: u16,
        message: impl Into<String>,
        body: Option<serde_json::Value>,
    ) -> Self {
        HeyoError::Api {
            status,
            message: message.into(),
            body,
        }
    }

    /// A transport failure, rendered with its whole cause chain.
    ///
    /// `reqwest::Error`'s `Display` prints only its own layer: a send failure
    /// stringifies to `error sending request for url (…)`, naming the URL and
    /// nothing about what went wrong. What separates a refused connection from
    /// a reset one, or from a peer that closed the stream mid-body, lives in
    /// [`std::error::Error::source`] — so this walks it.
    ///
    /// Stays `Api { status: 0 }` rather than becoming [`HeyoError::Connection`]:
    /// callers classify transport failures on that status, and moving the
    /// variant would silently change which failures they treat as retryable.
    pub(crate) fn transport(path: &str, e: &(dyn std::error::Error + 'static)) -> Self {
        HeyoError::api(
            0,
            format!("network error calling {}: {}", path, cause_chain(e)),
        )
    }

    /// [`HeyoError::transport`] tagged with reqwest's own classification.
    ///
    /// Not cosmetic: a request that outran its timeout and a body that stopped
    /// arriving can bottom out in the same io error, and only reqwest knows
    /// which of its phases raised it. `is_request` is deliberately not a tag —
    /// it covers nearly every send failure, so it separates nothing.
    pub(crate) fn transport_http(path: &str, e: &reqwest::Error) -> Self {
        let tags: Vec<&str> = [
            (e.is_timeout(), "timeout"),
            (e.is_connect(), "connect"),
            (e.is_body(), "body"),
            (e.is_decode(), "decode"),
            (e.is_redirect(), "redirect"),
        ]
        .into_iter()
        .filter_map(|(set, name)| set.then_some(name))
        .collect();

        if tags.is_empty() {
            return Self::transport(path, e);
        }
        HeyoError::api(
            0,
            format!(
                "network error calling {} [{}]: {}",
                path,
                tags.join(","),
                cause_chain(e)
            ),
        )
    }
}

/// Render `e` and every error beneath it as one line.
///
/// Consecutive layers routinely restate the one below them — hyper wrapping an
/// `io::Error` is the usual case — and the same sentence printed twice reads as
/// two separate faults, so a layer that only repeats its cause is skipped.
pub(crate) fn cause_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(err) = cause {
        let text = err.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cause = err.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One link in a synthetic cause chain. Real ones come from hyper and the
    /// io layer, neither of which can be constructed with a chosen `source`.
    #[derive(Debug)]
    struct Layer {
        text: String,
        source: Option<Box<Layer>>,
    }

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.text)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source
                .as_deref()
                .map(|s| s as &(dyn std::error::Error + 'static))
        }
    }

    /// Build a chain outermost-first, the order the layers are given in.
    fn chain(texts: &[&str]) -> Layer {
        let mut it = texts.iter().rev();
        let mut err = Layer {
            text: it.next().expect("a chain needs a layer").to_string(),
            source: None,
        };
        for t in it {
            err = Layer {
                text: t.to_string(),
                source: Some(Box::new(err)),
            };
        }
        err
    }

    #[test]
    fn every_layer_beneath_the_top_one_is_printed() {
        // The failure this exists for: reqwest says only the first of these,
        // and the last is the sole layer that says what actually happened.
        let e = chain(&[
            "error sending request for url (http://127.0.0.1:37453/images/build)",
            "client error (SendRequest)",
            "connection closed before message completed",
        ]);
        assert_eq!(
            cause_chain(&e),
            "error sending request for url (http://127.0.0.1:37453/images/build): \
             client error (SendRequest): connection closed before message completed"
        );
    }

    #[test]
    fn a_layer_that_only_restates_its_cause_is_not_printed_twice() {
        let e = chain(&["send failed: connection reset", "connection reset"]);
        assert_eq!(cause_chain(&e), "send failed: connection reset");
    }

    #[test]
    fn a_lone_error_reads_exactly_as_it_did_before() {
        assert_eq!(cause_chain(&chain(&["connection refused"])), "connection refused");
    }

    #[test]
    fn a_transport_failure_keeps_the_zero_status_callers_classify_on() {
        // `ci`'s `is_transport` matches `Api { status: 0 }` and evicts the iroh
        // tunnel on it. Enriching the message must not move the variant, or
        // that eviction silently stops firing.
        match HeyoError::transport("/images/build", &chain(&["outer", "inner"])) {
            HeyoError::Api { status, message, body } => {
                assert_eq!(status, 0);
                assert_eq!(body, None);
                assert_eq!(message, "network error calling /images/build: outer: inner");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }
}
