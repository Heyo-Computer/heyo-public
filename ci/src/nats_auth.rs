//! NATS endpoint and credential resolution.
//!
//! Ported from `queue-fn/src/nats_auth.rs`, which carries the same workaround as
//! heyo's cloud service (`cloud/src/services/nats_auth.rs`). The reason all three
//! need it:
//!
//! `async_nats` 0.47 reads authentication *only* from `ConnectOptions`. Its
//! connector parses userinfo out of a URL into `ServerAddr::username`/`password`
//! and then never sends it (`async-nats-0.47.0/src/lib.rs:1643-1655` expose the
//! fields; nothing in the CONNECT frame consumes them). So `nats://token@host`
//! resolves, dials, completes the TCP handshake, and *then* fails with
//! "Authorization Violation" — the failure surfaces one layer below the mistake.
//!
//! This app has to be pointed at whatever NATS is already deployed, so it takes
//! credentials the two ways operators actually supply them: embedded in the URL
//! (the NATS CLI convention, and what `CLOUD_NATS_URL` uses), or as discrete env
//! vars. This module reconciles the two into one `NatsEndpoint`.
//!
//! Two consequences worth stating, because both are load-bearing:
//!
//! - **The resolved URL is the only one anything else may print.** A URL that
//!   arrives carrying a password must never reach a log line, an error message,
//!   or a dashboard page, so `resolve` strips userinfo and hands back sanitized
//!   servers. The credential lives in `Credential`, which has no `Debug` that
//!   reveals it.
//! - **A comma-separated cluster URL has to be split here.** `ToServerAddrs for
//!   str` parses exactly one address (`lib.rs:1683`); it does not split on
//!   commas the way the NATS CLI does. Passing `a:4222,b:4222` through as one
//!   string is a parse error, not a cluster.

use std::fmt;

/// How to authenticate, resolved from exactly one source.
///
/// Deliberately not `Debug`/`Serialize`: the whole point of routing credentials
/// through one type is that there is no derive that can leak them by accident.
/// `Display` prints the *kind*, never the secret.
pub enum Credential {
    None,
    Token(String),
    UserPassword {
        user: String,
        password: String,
    },
    /// Contents of a NATS `.creds` file (JWT + nkey seed), already read.
    Creds(String),
    NKeySeed(String),
}

impl Credential {
    /// The name of the credential kind, for logs and conflict messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Token(_) => "token",
            Self::UserPassword { .. } => "user/password",
            Self::Creds(_) => "creds file",
            Self::NKeySeed(_) => "nkey seed",
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind())
    }
}

/// Credentials supplied as discrete env vars, before validation.
///
/// All optional and all independent, so `config` can fill it from the
/// environment without deciding anything; [`NatsEndpoint::resolve`] is where the
/// combinations are judged.
#[derive(Debug, Default, PartialEq)]
pub struct EnvCredentials {
    pub user: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    /// Path to a `.creds` file. Read at resolve time so a bad path fails at
    /// startup rather than at the first reconnect.
    pub creds_file: Option<String>,
    pub nkey_seed: Option<String>,
}

impl EnvCredentials {
    fn is_empty(&self) -> bool {
        self.user.is_none()
            && self.password.is_none()
            && self.token.is_none()
            && self.creds_file.is_none()
            && self.nkey_seed.is_none()
    }

    /// Fold the env vars into at most one credential, rejecting combinations
    /// that do not name a single intent.
    fn resolve(&self) -> Result<Credential, NatsAuthError> {
        if self.is_empty() {
            return Ok(Credential::None);
        }

        let mut found: Option<Credential> = None;
        let mut claim = |c: Credential| -> Result<(), NatsAuthError> {
            if let Some(existing) = &found {
                return Err(NatsAuthError::ConflictingCredentials {
                    first: existing.kind(),
                    second: c.kind(),
                });
            }
            found = Some(c);
            Ok(())
        };

        if let Some(path) = &self.creds_file {
            let contents =
                std::fs::read_to_string(path).map_err(|e| NatsAuthError::CredsFileUnreadable {
                    path: path.clone(),
                    reason: e.to_string(),
                })?;
            claim(Credential::Creds(contents))?;
        }
        if let Some(seed) = &self.nkey_seed {
            claim(Credential::NKeySeed(seed.clone()))?;
        }
        match (&self.user, &self.password) {
            (Some(user), Some(password)) => claim(Credential::UserPassword {
                user: user.clone(),
                password: password.clone(),
            })?,
            // A user with no password is not a token — NATS would reject it,
            // and guessing which the operator meant is how you end up
            // authenticating as the wrong principal.
            (Some(_), None) => return Err(NatsAuthError::UserWithoutPassword),
            (None, Some(_)) => return Err(NatsAuthError::PasswordWithoutUser),
            (None, None) => {}
        }
        if let Some(token) = &self.token {
            claim(Credential::Token(token.clone()))?;
        }

        Ok(found.unwrap_or(Credential::None))
    }
}

/// A resolved, connectable NATS target: sanitized servers plus one credential.
pub struct NatsEndpoint {
    /// Server addresses with userinfo stripped. Safe to log.
    pub servers: Vec<String>,
    pub credential: Credential,
    /// True when the credential came from the URL rather than an env var. Only
    /// used to warn that a secret was passed somewhere it may be captured (a
    /// shell history, a process list, a container spec).
    pub credential_from_url: bool,
}

/// Hand-written rather than derived, and the reason is the point of the type:
/// a derive would print the secret, and `{:?}` on a config struct is exactly
/// how credentials reach logs. Printing the credential's *kind* keeps the
/// diagnostic value — "it tried token auth" is the useful half.
impl fmt::Debug for NatsEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsEndpoint")
            .field("servers", &self.servers)
            .field("credential", &self.credential.kind())
            .finish()
    }
}

impl NatsEndpoint {
    /// Reconcile a URL and env vars into one endpoint.
    ///
    /// Env vars win over URL userinfo — an operator who sets `CI_NATS_TOKEN`
    /// has stated an intent more specifically than one who inherited a URL, and
    /// silently preferring the URL would make the explicit setting look broken.
    /// Conflicts *within* the env vars are an error rather than a precedence
    /// rule, because there is no reading of "token and creds file both set"
    /// that makes one of them obviously the intended one.
    pub fn resolve(url: &str, env: &EnvCredentials) -> Result<Self, NatsAuthError> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return Err(NatsAuthError::EmptyUrl);
        }

        let mut servers = Vec::new();
        let mut url_credential = Credential::None;
        for raw in trimmed.split(',') {
            let entry = raw.trim();
            if entry.is_empty() {
                return Err(NatsAuthError::EmptyServer);
            }
            let (clean, credential) = split_userinfo(entry)?;
            // The credential is taken from the first server that carries one.
            // A cluster URL repeats the same credential on every entry, so
            // later ones are redundant rather than conflicting; taking the
            // first keeps `nats://u:p@a,nats://u:p@b` working as written.
            if url_credential.is_none() {
                url_credential = credential;
            }
            servers.push(clean);
        }

        let env_credential = env.resolve()?;
        let credential_from_url = env_credential.is_none() && !url_credential.is_none();
        let credential = if env_credential.is_none() {
            url_credential
        } else {
            env_credential
        };

        Ok(Self {
            servers,
            credential,
            credential_from_url,
        })
    }

    /// The sanitized servers, comma-joined. This is what may be logged.
    pub fn redacted(&self) -> String {
        self.servers.join(",")
    }

    /// Build `ConnectOptions` carrying the credential, plus the parsed server
    /// list. Kept here rather than in `bus` so that no other module ever holds a
    /// `Credential` long enough to print one.
    ///
    /// `name` shows up in `nats server report connections`; with shared
    /// infrastructure an unnamed client is indistinguishable from any other
    /// consumer of the same account.
    pub fn connect_options(
        &self,
        name: &str,
    ) -> Result<(async_nats::ConnectOptions, Vec<async_nats::ServerAddr>), NatsAuthError> {
        let mut opts = async_nats::ConnectOptions::new().name(name);
        opts = match &self.credential {
            Credential::None => opts,
            Credential::Token(t) => opts.token(t.clone()),
            Credential::UserPassword { user, password } => {
                opts.user_and_password(user.clone(), password.clone())
            }
            Credential::NKeySeed(seed) => opts.nkey(seed.clone()),
            Credential::Creds(contents) => opts
                .credentials(contents)
                .map_err(|e| NatsAuthError::InvalidCreds(e.to_string()))?,
        };

        // Split into `ServerAddr`s rather than passing the joined string:
        // `ToServerAddrs for str` parses exactly one address, so a cluster URL
        // would be a parse error instead of a failover list.
        let servers = self
            .servers
            .iter()
            .map(|s| s.parse::<async_nats::ServerAddr>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| NatsAuthError::MalformedServer(e.to_string()))?;

        Ok((opts, servers))
    }
}

/// Split `scheme://[userinfo@]host[:port][/path]` into a userinfo-free address
/// and whatever credential the userinfo encoded.
///
/// Hand-rolled rather than pulling in `url`: the grammar being parsed here is
/// one authority component, and the crate's value is in the parts (relative
/// resolution, normalization, IDNA) that would be wrong to apply to a NATS
/// address anyway.
fn split_userinfo(entry: &str) -> Result<(String, Credential), NatsAuthError> {
    let Some(scheme_end) = entry.find("://") else {
        // Schemeless `host:port` is what the NATS CLI accepts and cannot carry
        // userinfo, so there is nothing to strip.
        if entry.contains('@') {
            return Err(NatsAuthError::MalformedServer(entry.to_string()));
        }
        return Ok((entry.to_string(), Credential::None));
    };
    let after_scheme = scheme_end + 3;
    let rest = &entry[after_scheme..];

    // Userinfo lives before the first `/` that ends the authority. Bounding the
    // search matters: a token in a path or query would otherwise be mistaken
    // for userinfo and stripped out of the address.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    let Some(at) = authority.rfind('@') else {
        return Ok((entry.to_string(), Credential::None));
    };
    // `rfind` rather than `find`: RFC 3986 requires an `@` inside userinfo to be
    // percent-encoded, but passwords containing a literal `@` are common enough
    // in practice that splitting on the first one would silently authenticate
    // with a truncated password.
    let userinfo = &authority[..at];
    let host = &authority[at + 1..];
    if host.is_empty() {
        return Err(NatsAuthError::MalformedServer(entry.to_string()));
    }

    let clean = format!(
        "{}{}{}",
        &entry[..after_scheme],
        host,
        &rest[authority_end..]
    );

    // NATS CLI convention, matched by cloud/src/services/nats_auth.rs:
    //   <token>@host        => token auth
    //   <user>:<pass>@host  => user/password auth
    let credential = match userinfo.split_once(':') {
        Some((user, password)) => Credential::UserPassword {
            user: percent_decode(user),
            password: percent_decode(password),
        },
        None if userinfo.is_empty() => Credential::None,
        None => Credential::Token(percent_decode(userinfo)),
    };
    Ok((clean, credential))
}

/// Percent-decode a userinfo component.
///
/// Required, not cosmetic: a password containing `@`, `:`, or `/` has to be
/// encoded to survive the URL grammar, so decoding is what turns the transported
/// form back into the actual secret. Invalid escapes are passed through
/// unchanged rather than rejected — a `%` in a password that was never encoded
/// is a likelier explanation than a corrupt URL.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[derive(Debug, PartialEq)]
pub enum NatsAuthError {
    EmptyUrl,
    EmptyServer,
    MalformedServer(String),
    ConflictingCredentials {
        first: &'static str,
        second: &'static str,
    },
    UserWithoutPassword,
    PasswordWithoutUser,
    CredsFileUnreadable {
        path: String,
        reason: String,
    },
    InvalidCreds(String),
}

impl fmt::Display for NatsAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "the NATS URL is empty; set CI_NATS_URL"),
            Self::EmptyServer => write!(
                f,
                "the NATS URL has an empty entry: a comma-separated cluster URL \
                 must not have a trailing or doubled comma"
            ),
            // The entry is echoed with userinfo intact only when the parse
            // failed, i.e. when there is no userinfo to be found — so this
            // cannot print a credential.
            Self::MalformedServer(entry) => {
                write!(f, "could not parse the NATS server address {entry:?}")
            }
            Self::ConflictingCredentials { first, second } => write!(
                f,
                "conflicting NATS credentials: {first} and {second} are both set, \
                 and there is no order of precedence between them that is not a \
                 guess — set exactly one"
            ),
            Self::UserWithoutPassword => write!(
                f,
                "CI_NATS_USER is set without CI_NATS_PASSWORD; for token auth \
                 use CI_NATS_TOKEN instead"
            ),
            Self::PasswordWithoutUser => {
                write!(f, "CI_NATS_PASSWORD is set without CI_NATS_USER")
            }
            Self::CredsFileUnreadable { path, reason } => {
                write!(f, "could not read the NATS creds file {path:?}: {reason}")
            }
            Self::InvalidCreds(reason) => {
                write!(
                    f,
                    "the NATS creds file is not a valid JWT + nkey pair: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for NatsAuthError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(url: &str) -> NatsEndpoint {
        NatsEndpoint::resolve(url, &EnvCredentials::default()).expect("resolves")
    }

    fn assert_user_password(c: &Credential, want_user: &str, want_password: &str) {
        match c {
            Credential::UserPassword { user, password } => {
                assert_eq!(user, want_user);
                assert_eq!(password, want_password);
            }
            other => panic!("expected user/password, got {other}"),
        }
    }

    #[test]
    fn a_url_without_userinfo_is_passed_through_unchanged() {
        let e = resolve("nats://127.0.0.1:4222");
        assert_eq!(e.servers, ["nats://127.0.0.1:4222"]);
        assert!(e.credential.is_none());
        assert!(!e.credential_from_url);
    }

    /// The NATS CLI convention cloud follows: a lone userinfo component is a
    /// token, not a username.
    #[test]
    fn a_lone_userinfo_component_is_a_token() {
        let e = resolve("nats://s3cr3t@nats.internal:4222");
        assert_eq!(e.servers, ["nats://nats.internal:4222"]);
        match &e.credential {
            Credential::Token(t) => assert_eq!(t, "s3cr3t"),
            other => panic!("expected a token, got {other}"),
        }
        assert!(e.credential_from_url);
    }

    #[test]
    fn a_colon_separated_userinfo_is_a_user_and_password() {
        let e = resolve("nats://alice:hunter2@nats.internal:4222");
        assert_eq!(e.servers, ["nats://nats.internal:4222"]);
        assert_user_password(&e.credential, "alice", "hunter2");
    }

    /// Regression: this is the failure the module exists to prevent. The URL is
    /// logged on startup and again in the connect-failure panic, so a password
    /// left in it lands in the log with the error that draws someone to read it.
    #[test]
    fn the_redacted_form_never_contains_the_secret() {
        for url in [
            "nats://s3cr3t@nats.internal:4222",
            "nats://alice:hunter2@nats.internal:4222",
            "wss://tok3n@nats.internal:443",
            "nats://alice:hunter2@a:4222,nats://alice:hunter2@b:4222",
        ] {
            let redacted = resolve(url).redacted();
            for secret in ["s3cr3t", "hunter2", "tok3n"] {
                assert!(
                    !redacted.contains(secret),
                    "{url} leaked {secret} as {redacted}"
                );
            }
        }
    }

    /// `ToServerAddrs for str` parses one address and does not split on commas
    /// (async-nats-0.47.0/src/lib.rs:1683), so a cluster URL passed through
    /// whole is a parse error rather than a cluster.
    #[test]
    fn a_comma_separated_cluster_url_becomes_one_entry_per_server() {
        let e = resolve("nats://a:4222,nats://b:4222,nats://c:4222");
        assert_eq!(
            e.servers,
            ["nats://a:4222", "nats://b:4222", "nats://c:4222"]
        );
    }

    #[test]
    fn a_cluster_url_takes_the_credential_from_the_first_server_that_carries_one() {
        let e = resolve("nats://alice:hunter2@a:4222,nats://alice:hunter2@b:4222");
        assert_eq!(e.servers, ["nats://a:4222", "nats://b:4222"]);
        assert_user_password(&e.credential, "alice", "hunter2");
    }

    #[test]
    fn a_doubled_or_trailing_comma_is_rejected() {
        for url in ["nats://a:4222,", "nats://a:4222,,nats://b:4222", ","] {
            assert_eq!(
                NatsEndpoint::resolve(url, &EnvCredentials::default()).unwrap_err(),
                NatsAuthError::EmptyServer,
                "url {url}",
            );
        }
    }

    #[test]
    fn an_empty_url_is_rejected() {
        assert_eq!(
            NatsEndpoint::resolve("   ", &EnvCredentials::default()).unwrap_err(),
            NatsAuthError::EmptyUrl
        );
    }

    /// Regression: splitting on the first `@` truncates a password at its own
    /// `@`, which authenticates with a wrong-but-plausible secret — a failure
    /// that reads as a server-side permissions problem.
    #[test]
    fn a_password_containing_an_at_sign_is_not_truncated() {
        let e = resolve("nats://alice:p@ssw0rd@nats.internal:4222");
        assert_eq!(e.servers, ["nats://nats.internal:4222"]);
        assert_user_password(&e.credential, "alice", "p@ssw0rd");
    }

    #[test]
    fn percent_encoded_userinfo_is_decoded() {
        let e = resolve("nats://al%40ice:hunter%3A2@nats.internal:4222");
        assert_user_password(&e.credential, "al@ice", "hunter:2");
    }

    /// A `%` that is not an escape is far likelier to be a literal character in
    /// a password than a corrupt URL, so it survives decoding.
    #[test]
    fn an_invalid_percent_escape_is_left_alone() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("%41"), "A");
    }

    /// Regression: an unbounded search for `@` finds one in a path or query and
    /// strips the host along with it.
    #[test]
    fn an_at_sign_after_the_authority_is_not_userinfo() {
        let e = resolve("nats://nats.internal:4222/route@here");
        assert_eq!(e.servers, ["nats://nats.internal:4222/route@here"]);
        assert!(e.credential.is_none());
    }

    #[test]
    fn a_schemeless_host_port_is_accepted() {
        let e = resolve("127.0.0.1:4222");
        assert_eq!(e.servers, ["127.0.0.1:4222"]);
        assert!(e.credential.is_none());
    }

    #[test]
    fn userinfo_with_no_host_is_rejected() {
        assert!(matches!(
            NatsEndpoint::resolve("nats://alice:hunter2@", &EnvCredentials::default()),
            Err(NatsAuthError::MalformedServer(_))
        ));
    }

    #[test]
    fn an_env_token_wins_over_url_userinfo() {
        let env = EnvCredentials {
            token: Some("from-env".into()),
            ..Default::default()
        };
        let e = NatsEndpoint::resolve("nats://from-url@host:4222", &env).unwrap();
        assert_eq!(e.servers, ["nats://host:4222"]);
        match &e.credential {
            Credential::Token(t) => assert_eq!(t, "from-env"),
            other => panic!("expected the env token, got {other}"),
        }
        assert!(!e.credential_from_url, "the credential came from the env");
    }

    #[test]
    fn env_user_and_password_resolve_together() {
        let env = EnvCredentials {
            user: Some("alice".into()),
            password: Some("hunter2".into()),
            ..Default::default()
        };
        let e = NatsEndpoint::resolve("nats://host:4222", &env).unwrap();
        assert_user_password(&e.credential, "alice", "hunter2");
    }

    #[test]
    fn a_user_without_a_password_is_rejected_rather_than_read_as_a_token() {
        let env = EnvCredentials {
            user: Some("alice".into()),
            ..Default::default()
        };
        assert_eq!(
            NatsEndpoint::resolve("nats://host:4222", &env).unwrap_err(),
            NatsAuthError::UserWithoutPassword
        );
    }

    #[test]
    fn a_password_without_a_user_is_rejected() {
        let env = EnvCredentials {
            password: Some("hunter2".into()),
            ..Default::default()
        };
        assert_eq!(
            NatsEndpoint::resolve("nats://host:4222", &env).unwrap_err(),
            NatsAuthError::PasswordWithoutUser
        );
    }

    #[test]
    fn two_env_credential_kinds_are_a_startup_error() {
        let env = EnvCredentials {
            token: Some("t".into()),
            nkey_seed: Some("SU...".into()),
            ..Default::default()
        };
        assert_eq!(
            NatsEndpoint::resolve("nats://host:4222", &env).unwrap_err(),
            NatsAuthError::ConflictingCredentials {
                first: "nkey seed",
                second: "token",
            }
        );
    }

    #[test]
    fn an_unreadable_creds_file_fails_at_startup() {
        let env = EnvCredentials {
            creds_file: Some("/nonexistent/ci.creds".into()),
            ..Default::default()
        };
        assert!(matches!(
            NatsEndpoint::resolve("nats://host:4222", &env),
            Err(NatsAuthError::CredsFileUnreadable { .. })
        ));
    }

    #[test]
    fn a_creds_file_is_read_at_resolve_time() {
        let path = std::env::temp_dir().join("ci-test-nats.creds");
        std::fs::write(&path, "-----BEGIN NATS USER JWT-----\nfake\n").unwrap();
        let env = EnvCredentials {
            creds_file: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let e = NatsEndpoint::resolve("nats://host:4222", &env).unwrap();
        match &e.credential {
            Credential::Creds(c) => assert!(c.contains("NATS USER JWT")),
            other => panic!("expected creds, got {other}"),
        }
        std::fs::remove_file(&path).ok();
    }

    /// A tls:// or wss:// URL is handled by async-nats' own scheme detection;
    /// all this module owes it is to not mangle the address.
    #[test]
    fn tls_and_websocket_schemes_survive_sanitizing() {
        let e = resolve("wss://tok3n@nats.example.com:443");
        assert_eq!(e.servers, ["wss://nats.example.com:443"]);
        let e = resolve("tls://alice:hunter2@nats.example.com:4222");
        assert_eq!(e.servers, ["tls://nats.example.com:4222"]);
    }

    #[test]
    fn the_credential_kind_is_printable_but_the_secret_is_not() {
        let c = Credential::Token("s3cr3t".into());
        assert_eq!(c.to_string(), "token");
        assert!(!format!("{c}").contains("s3cr3t"));
    }
}
