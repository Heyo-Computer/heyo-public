//! Google sign-in as a gate in front of a deployment.
//!
//! A gated deployment's requests do not reach a backend until the caller has
//! signed in with Google and passed the deployment's allow-list. The application
//! behind it is unchanged and unaware: it sees only requests that got through,
//! optionally with the caller's identity in `x-auth-request-*` headers.
//!
//! The flow is the OAuth 2.0 authorization code grant with PKCE, run by the
//! proxy on the deployment's own hostname:
//!
//! ```text
//! GET /anything          →  302 accounts.google.com/…  + a short-lived flow cookie
//! GET <base>/callback    →  POST oauth2.googleapis.com/token
//!                        →  302 back to /anything      + the session cookie
//! GET /anything          →  proxied to the backend
//! ```
//!
//! **No server-side session store.** Both cookies are self-describing and signed
//! with HMAC-SHA256, so a restart does not sign everyone out, and there is no
//! table to grow or to replicate. The key lives in one file (`APP_LB_AUTH_KEY`),
//! generated on first use.
//!
//! **The id token's signature is deliberately not verified.** It arrives in the
//! body of app-lb's own HTTPS POST to Google's token endpoint, authenticated by
//! the client secret — not from the browser — so the channel already establishes
//! the issuer. OpenID Connect Core §3.1.3.7 says exactly this: a token received
//! directly from the token endpoint over a TLS-protected channel may be
//! validated by that fact instead of by its signature, which is why there is no
//! JWKS fetch, cache or rotation to get wrong here. The *claims* are still
//! checked: audience, issuer, expiry, and `email_verified`.

use crate::config::AuthGate;
use crate::deployment::now_secs;
use crate::secrets::SecretStore;
use base64::Engine;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Where the browser is sent to sign in.
const GOOGLE_AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Where app-lb exchanges the code for tokens, server to server.
const GOOGLE_TOKEN: &str = "https://oauth2.googleapis.com/token";
/// Both spellings Google uses for `iss`.
const GOOGLE_ISSUERS: [&str; 2] = ["accounts.google.com", "https://accounts.google.com"];

/// The sign-in round trip has to outlive a password prompt and a 2FA push, and
/// nothing else. The flow cookie is worthless afterwards.
const FLOW_TTL: Duration = Duration::from_secs(600);
/// Ceiling on the token exchange. A login blocks a request, so this is a
/// latency budget, not a generosity budget.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Cookie carrying the in-flight sign-in (nonce, return path, PKCE verifier).
const FLOW_COOKIE: &str = "applb_auth_flow";

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Who the caller is, as far as the provider says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The provider's stable subject id — the only field that is guaranteed not
    /// to change when someone's address does.
    pub subject: String,
    pub email: String,
    pub name: Option<String>,
    /// The Google Workspace domain governing the account, when there is one.
    pub hosted_domain: Option<String>,
}

/// What the gate decided.
pub enum Decision {
    /// Let the request through. `Some` when there is an identity to forward;
    /// `None` for a path the gate does not cover.
    Allow(Box<Option<Identity>>),
    /// The gate answered the request itself — a redirect, a 403, the callback.
    /// Nothing more should happen to it.
    Answered(Response),
}

/// A response the gate wants written. Kept as data rather than written here so
/// this module needs no pingora types, and every decision is unit-testable.
pub struct Response {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
    pub location: Option<String>,
    /// Complete `Set-Cookie` values.
    pub cookies: Vec<String>,
}

impl Response {
    fn redirect(location: String, cookies: Vec<String>) -> Self {
        Self {
            status: 302,
            body: String::new(),
            content_type: "text/plain; charset=utf-8",
            location: Some(location),
            cookies,
        }
    }

    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "text/plain; charset=utf-8",
            location: None,
            cookies: Vec::new(),
        }
    }

    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            content_type: "application/json",
            location: None,
            cookies: Vec::new(),
        }
    }
}

/// The parts of a request the gate needs. A plain struct rather than a borrowed
/// `Session` so the decision logic can be exercised without a live connection.
pub struct RequestInfo<'a> {
    pub host: &'a str,
    pub path: &'a str,
    /// Raw query string, without `?`.
    pub query: Option<&'a str>,
    /// Every `Cookie` header value, joined by the caller or passed in order.
    pub cookies: Vec<String>,
    /// Whether the client reached app-lb over TLS. Decides the `Secure`
    /// attribute — setting it on a plaintext connection would make the cookie
    /// unusable, and omitting it on an encrypted one would leak the session.
    pub secure: bool,
    /// True when the caller looks like a browser navigating, which is what makes
    /// a redirect the right answer instead of a 401.
    pub wants_html: bool,
    /// The value of an `Authorization: Bearer …` header, if there was one. An
    /// app-token gate checks this; a Google gate ignores it.
    pub bearer: Option<String>,
    /// The socket peer, for the SIEM's per-source rules. `None` in tests and for
    /// a non-inet peer. Never derived from a forwarded header — see
    /// `proxy::request_info`.
    pub client: Option<std::net::IpAddr>,
}

pub struct Authenticator {
    key: Vec<u8>,
    secrets: Arc<SecretStore>,
    /// Minted app-tokens, for gates that accept them. `None` in the tests that
    /// only exercise the Google path.
    tokens: Option<Arc<crate::tokens::TokenStore>>,
    http: reqwest::Client,
    /// Overridable so the token exchange can be pointed at a stub in tests.
    token_endpoint: String,
    authorize_endpoint: String,
    /// Queues rejected sign-ins for analysis. `None` when `APP_LB_SIEM=0`, and in
    /// tests.
    security: Option<crate::siem::SecuritySink>,
}

impl Authenticator {
    /// Queue one refused sign-in for analysis.
    ///
    /// Never carries a token or a code — only which step refused, and for the
    /// allow-list case the address that was refused, which is already in the
    /// `tracing::info!` beside it and is what the enumeration rule counts.
    fn observe_auth(
        &self,
        deployment: &str,
        req: &RequestInfo<'_>,
        action: crate::siem::AuthAction,
        subject: Option<&str>,
    ) {
        if let Some(siem) = &self.security {
            siem.observe_auth(crate::siem::AuthObs {
                ts: crate::obs::now_millis(),
                client: req.client,
                deployment: Some(Box::from(deployment)),
                path: Box::from(req.path),
                action,
                scheme: match req.bearer {
                    Some(_) => crate::siem::AuthScheme::Bearer,
                    None => crate::siem::AuthScheme::None,
                },
                subject: subject.map(Box::from),
            });
        }
    }
}

impl std::fmt::Debug for Authenticator {
    /// Hand-written so no `{:?}` can print the signing key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("token_endpoint", &self.token_endpoint)
            .finish()
    }
}

impl Authenticator {
    pub fn new(
        key: Vec<u8>,
        secrets: Arc<SecretStore>,
        tokens: Option<Arc<crate::tokens::TokenStore>>,
        security: Option<crate::siem::SecuritySink>,
    ) -> Self {
        Self {
            key,
            secrets,
            tokens,
            security,
            http: reqwest::Client::builder()
                .timeout(TOKEN_TIMEOUT)
                // A login is a person waiting; there is no retry that helps.
                .build()
                .unwrap_or_default(),
            token_endpoint: GOOGLE_TOKEN.to_string(),
            authorize_endpoint: GOOGLE_AUTHORIZE.to_string(),
        }
    }

    /// Load the session signing key, generating it on first use.
    ///
    /// Persisted rather than random-per-boot so a restart doesn't sign everyone
    /// out, and `0600` because anyone holding it can mint a session for any
    /// gated deployment.
    pub fn load_key(path: &Path) -> std::io::Result<Vec<u8>> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() >= 32 => return Ok(bytes),
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "{} is too short to be a signing key; delete it to have a new one \
                     generated (every session will be invalidated)",
                    path.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        let mut key = vec![0u8; 32];
        openssl::rand::rand_bytes(&mut key)
            .map_err(|e| std::io::Error::other(format!("no entropy for a signing key: {e}")))?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            crate::tls::create_dir_private(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, b"")?;
        crate::tls::restrict(&tmp)?;
        std::fs::write(&tmp, &key)?;
        std::fs::rename(&tmp, path)?;
        tracing::info!(path = %path.display(), "generated a session signing key");
        Ok(key)
    }

    // -- the gate ----------------------------------------------------------

    /// Decide what happens to one request.
    ///
    /// Everything except the token exchange is pure; the exchange is the one
    /// await, and only on the callback path.
    pub async fn decide(
        &self,
        gate: &AuthGate,
        deployment_id: &str,
        req: &RequestInfo<'_>,
    ) -> Decision {
        // app-lb's own endpoints first — they live under the deployment's
        // hostname, so they have to be claimed before the app sees them.
        if req.path == gate.callback_path() {
            return Decision::Answered(self.callback(gate, deployment_id, req).await);
        }
        if req.path == gate.logout_path() {
            return Decision::Answered(self.logout(gate, req));
        }
        if req.path == gate.login_path() {
            // An explicit sign-in link. Always starts a fresh flow, so it also
            // works as "switch account" after a logout.
            return Decision::Answered(self.start(gate, deployment_id, req, "/"));
        }

        if gate.is_public(req.path) {
            return Decision::Allow(Box::new(None));
        }

        // An app-token, if this gate takes them. Checked before the session
        // cookie because a request carrying an explicit credential means it, and
        // because a program presenting a token should never be handed a redirect
        // to a sign-in page.
        //
        // Providers are alternatives: a gate listing both admits a person with a
        // Google session *or* a program with a token, and neither has to know
        // the other exists.
        if gate.accepts_app_token()
            && let Some(presented) = &req.bearer
            && let Some(tokens) = &self.tokens
            && let Some(token) = tokens.verify(presented, now_secs())
            && token.allows(deployment_id)
        {
            // No `Identity`: a token is not a person, and forwarding
            // `x-auth-request-email` for one would put a name upstream that
            // belongs to nobody. The token's own name is in the LB's logs.
            tracing::debug!(
                deployment = %deployment_id,
                token = %token.id,
                "request admitted by app-token",
            );
            return Decision::Allow(Box::new(None));
        }

        // Reaching here with a bearer in hand means it was presented and did not
        // admit — expired, revoked, forged, or scoped to another deployment.
        //
        // Gated on `bearer.is_some()` deliberately: a browser sends no
        // `Authorization` header on any request to a gated deployment, so
        // observing the no-credential case would flood the queue with the single
        // most common non-event there is.
        if gate.accepts_app_token() && req.bearer.is_some() {
            self.observe_auth(deployment_id, req, crate::siem::AuthAction::GateToken, None);
        }

        match self.session(gate, deployment_id, req) {
            Some(identity) => Decision::Allow(Box::new(Some(identity))),
            None => {
                let return_to = match req.query {
                    Some(q) if !q.is_empty() => format!("{}?{}", req.path, q),
                    _ => req.path.to_string(),
                };
                Decision::Answered(self.start(gate, deployment_id, req, &return_to))
            }
        }
    }

    /// Begin a sign-in: redirect to Google, remembering where to come back to.
    fn start(
        &self,
        gate: &AuthGate,
        deployment_id: &str,
        req: &RequestInfo<'_>,
        return_to: &str,
    ) -> Response {
        // A token-only gate has no sign-in flow to start: there is no provider to
        // redirect to and no cookie to set. Say what would actually work rather
        // than sending a browser into an OAuth round trip that ends in a blank
        // `client_id`.
        let Some((client_id, _)) = gate.google_credentials() else {
            return Response::json(
                401,
                "{\"error\":\"authentication required\",\"accepts\":[\"app-token\"],\
                 \"detail\":\"present an app-token as `Authorization: Bearer applb_…` \
                 or `?app_token=`\"}\n"
                    .to_string(),
            );
        };

        // An API client gets a 401 it can act on rather than a redirect it would
        // follow into an HTML sign-in page and then fail to parse.
        if !req.wants_html {
            let login = format!("{}{}", origin(req), gate.login_path());
            return Response::json(
                401,
                format!(
                    "{{\"error\":\"authentication required\",\"login_url\":\"{login}\"}}\n"
                ),
            );
        }

        let nonce = random_token();
        let verifier = random_token();
        let flow = Flow {
            nonce: nonce.clone(),
            return_to: safe_return_path(return_to),
            verifier: verifier.clone(),
            deployment: deployment_id.to_string(),
            exp: now_secs() + FLOW_TTL.as_secs(),
        };
        let redirect_uri = self.redirect_uri(gate, req);

        let url = format!(
            "{}?{}",
            self.authorize_endpoint,
            form_urlencoded::Serializer::new(String::new())
                .append_pair("client_id", client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("response_type", "code")
                .append_pair("scope", "openid email profile")
                // The nonce is the `state`: an opaque value the browser echoes
                // back, which we compare against the signed cookie. That pairing
                // is what stops a third party from completing a login into
                // somebody else's browser.
                .append_pair("state", &nonce)
                .append_pair("code_challenge", &pkce_challenge(&verifier))
                .append_pair("code_challenge_method", "S256")
                // Ask for the account chooser rather than silently reusing
                // whichever Google session the browser happens to have.
                .append_pair("prompt", "select_account")
                .finish()
        );

        Response::redirect(
            url,
            // The flow cookie stays host-only even in a shared realm: it is a
            // single round trip that starts and ends on this hostname, and
            // widening it would put a live PKCE verifier on every sibling host
            // for no gain.
            vec![set_cookie(
                FLOW_COOKIE,
                &self.sign(&flow.encode()),
                req.secure,
                Some(FLOW_TTL.as_secs()),
                None,
            )],
        )
    }

    /// Handle Google's redirect back: verify, exchange, admit or refuse.
    async fn callback(
        &self,
        gate: &AuthGate,
        deployment_id: &str,
        req: &RequestInfo<'_>,
    ) -> Response {
        let params = query_pairs(req.query);
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());

        // Google reports a refusal (`access_denied` when the user cancels) here
        // rather than by not coming back at all.
        if let Some(error) = get("error") {
            tracing::info!(deployment = %deployment_id, %error, "sign-in was refused at the provider");
            return Response::text(
                403,
                format!("sign-in was not completed ({error})\n"),
            );
        }

        let (Some(code), Some(state)) = (get("code"), get("state")) else {
            return Response::text(400, "the sign-in callback is missing `code` or `state`\n");
        };

        let Some(raw) = cookie_value(&req.cookies, FLOW_COOKIE) else {
            // Usually a bookmarked callback URL or a cookie dropped by a
            // `SameSite` policy, not an attack. Start over rather than dead-end.
            tracing::debug!(deployment = %deployment_id, "sign-in callback with no flow cookie");
            return self.start(gate, deployment_id, req, "/");
        };
        let Some(flow) = self.verify(&raw).and_then(|b| Flow::decode(&b)) else {
            return Response::text(400, "the sign-in state could not be verified\n");
        };
        if flow.exp <= now_secs() {
            return self.start(gate, deployment_id, req, &flow.return_to);
        }
        // The state parameter came back through the browser; the nonce it is
        // compared against came from a signed cookie. Constant-time because the
        // comparison is against a secret-ish value.
        if flow.deployment != deployment_id || !ct_eq(state.as_bytes(), flow.nonce.as_bytes()) {
            tracing::warn!(
                deployment = %deployment_id,
                "sign-in state did not match the flow cookie; refusing",
            );
            self.observe_auth(deployment_id, req, crate::siem::AuthAction::SigninState, None);
            return Response::text(400, "the sign-in state did not match\n");
        }

        let Some((client_id, client_secret)) = gate.google_credentials() else {
            tracing::error!(
                deployment = %deployment_id,
                "a sign-in callback arrived for a gate with no Google credentials",
            );
            return Response::text(500, "the sign-in gate is misconfigured on the server\n");
        };
        let secret = match self.secrets.resolve(client_secret) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(deployment = %deployment_id, error = %e, "auth client secret is missing");
                return Response::text(500, "the sign-in gate is misconfigured on the server\n");
            }
        };

        let redirect_uri = self.redirect_uri(gate, req);
        let claims = match self
            .exchange(client_id, &secret, code, &flow.verifier, &redirect_uri)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(deployment = %deployment_id, error = %e, "token exchange failed");
                self.observe_auth(
                    deployment_id,
                    req,
                    crate::siem::AuthAction::SigninExchange,
                    None,
                );
                return Response::text(502, "could not complete sign-in with the provider\n");
            }
        };

        let identity = match validate_claims(&claims, client_id) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(deployment = %deployment_id, error = %e, "id token rejected");
                self.observe_auth(deployment_id, req, crate::siem::AuthAction::SigninToken, None);
                return Response::text(403, format!("sign-in was rejected: {e}\n"));
            }
        };

        if !gate.allows(&identity.email, identity.hosted_domain.as_deref()) {
            // Deliberately says who was refused: the person is looking at this
            // page, and "which account am I signed in as" is the first thing
            // they need. The logout link lets them switch without clearing
            // cookies by hand.
            tracing::info!(
                deployment = %deployment_id,
                email = %identity.email,
                "sign-in refused by the allow-list",
            );
            self.observe_auth(
                deployment_id,
                req,
                crate::siem::AuthAction::SigninRefused,
                Some(&identity.email),
            );
            return Response::text(
                403,
                format!(
                    "{} is not allowed to access this deployment.\n\nSign in with a \
                     different account: {}{}\n",
                    identity.email,
                    origin(req),
                    gate.login_path()
                ),
            );
        }

        // Computed from the host that actually served the callback, not from the
        // spec alone: a realm that does not cover this hostname must fall back
        // to a host-only cookie, or the browser drops it and the user loops.
        let realm = gate.cookie_domain_for(req.host);
        if gate.cookie_domain.is_some() && realm.is_none() {
            tracing::warn!(
                deployment = %deployment_id,
                host = %req.host,
                cookie_domain = ?gate.cookie_domain,
                "auth.cookie_domain does not cover this hostname; issuing a host-only \
                 session cookie, so sign-in will not be shared with sibling deployments",
            );
        }

        let session = Session {
            subject: identity.subject.clone(),
            email: identity.email.clone(),
            name: identity.name.clone(),
            hosted_domain: identity.hosted_domain.clone(),
            deployment: deployment_id.to_string(),
            policy: gate.policy_fingerprint(),
            exp: now_secs() + gate.session_ttl_secs,
        };
        tracing::info!(
            deployment = %deployment_id,
            email = %identity.email,
            "sign-in succeeded",
        );

        Response::redirect(
            format!("{}{}", origin(req), flow.return_to),
            vec![
                set_cookie(
                    &gate.cookie_name,
                    &self.sign(&session.encode()),
                    req.secure,
                    Some(gate.session_ttl_secs),
                    realm.as_deref(),
                ),
                clear_cookie(FLOW_COOKIE, req.secure, None),
            ],
        )
    }

    fn logout(&self, gate: &AuthGate, req: &RequestInfo<'_>) -> Response {
        // Only app-lb's own cookie is cleared: signing the user out of Google
        // itself is not app-lb's to do, and doing it would sign them out of
        // every other tab they have open.
        Response::redirect(
            format!("{}{}", origin(req), gate.login_path()),
            vec![
                clear_cookie(&gate.cookie_name, req.secure, gate.cookie_domain_for(req.host).as_deref()),
                clear_cookie(FLOW_COOKIE, req.secure, None),
            ],
        )
    }

    /// The identity in the request's session cookie, if it has a valid one.
    fn session(
        &self,
        gate: &AuthGate,
        deployment_id: &str,
        req: &RequestInfo<'_>,
    ) -> Option<Identity> {
        let raw = cookie_value(&req.cookies, &gate.cookie_name)?;
        let session = Session::decode(&self.verify(&raw)?)?;
        if session.exp <= now_secs() {
            return None;
        }
        // A session does not outlive a tightened allow-list: the fingerprint
        // covers the provider, the client id, both allow-lists, and whether the
        // gate shares sessions at all.
        if session.policy != gate.policy_fingerprint() {
            return None;
        }
        // And it is scoped to the deployment that issued it — unless this gate
        // opted into a shared realm, which is what makes one sign-in cover every
        // deployment under a parent domain instead of one per hostname.
        //
        // Relaxing this is safe *because* of the check above, not in spite of
        // it. Reaching here means the two gates have byte-identical policy, so
        // whoever holds this session is somebody this gate would have admitted
        // anyway; the round trip it skips would have ended in exactly this
        // identity. A gate with a different allow-list has a different
        // fingerprint and refuses the cookie, shared realm or not.
        if session.deployment != deployment_id && gate.cookie_domain.is_none() {
            return None;
        }
        Some(Identity {
            subject: session.subject,
            email: session.email,
            name: session.name,
            hosted_domain: session.hosted_domain,
        })
    }

    /// `https://<host><base_path>/callback`, or the spec's override.
    fn redirect_uri(&self, gate: &AuthGate, req: &RequestInfo<'_>) -> String {
        match &gate.redirect_url {
            Some(u) => u.clone(),
            None => format!("{}{}", origin(req), gate.callback_path()),
        }
    }

    /// Trade the authorization code for tokens. Server to server, authenticated
    /// with the client secret, which is why the id token that comes back can be
    /// trusted without checking its signature.
    async fn exchange(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<IdClaims, String> {
        let response = self
            .http
            .post(&self.token_endpoint)
            .form(&[
                ("code", code),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("redirect_uri", redirect_uri),
                ("grant_type", "authorization_code"),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(|e| format!("could not reach the token endpoint: {e}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("could not read the token response: {e}"))?;
        if !status.is_success() {
            // Google's error body names the cause (`redirect_uri_mismatch` is
            // the usual one), and it contains no credential, so it is worth
            // keeping — it is the difference between a five-minute fix and an
            // afternoon.
            return Err(format!(
                "the provider rejected the exchange (HTTP {status}): {}",
                body.chars().take(300).collect::<String>()
            ));
        }

        let token: TokenResponse =
            serde_json::from_str(&body).map_err(|e| format!("token response was not JSON: {e}"))?;
        let id_token = token
            .id_token
            .ok_or("the token response carried no id_token")?;
        decode_id_token(&id_token)
    }

    // -- signing -----------------------------------------------------------

    /// `<payload>.<mac>`, both base64url. The payload is not encrypted: it holds
    /// an email address and an expiry, which the person it belongs to already
    /// knows. The signature is what makes it unforgeable.
    fn sign(&self, payload: &[u8]) -> String {
        let encoded = b64().encode(payload);
        let mac = hmac(&self.key, encoded.as_bytes());
        format!("{encoded}.{}", b64().encode(mac))
    }

    fn verify(&self, token: &str) -> Option<Vec<u8>> {
        let (encoded, mac) = token.rsplit_once('.')?;
        let presented = b64().decode(mac).ok()?;
        let expected = hmac(&self.key, encoded.as_bytes());
        if !ct_eq(&presented, &expected) {
            return None;
        }
        b64().decode(encoded).ok()
    }
}

/// The in-flight sign-in, carried in a cookie for the duration of the round trip.
#[derive(Debug, serde::Serialize, Deserialize, PartialEq, Eq)]
struct Flow {
    #[serde(rename = "n")]
    nonce: String,
    #[serde(rename = "r")]
    return_to: String,
    /// PKCE code verifier. In the cookie and never in the URL — the challenge
    /// (its hash) is what goes to the provider.
    #[serde(rename = "v")]
    verifier: String,
    #[serde(rename = "d")]
    deployment: String,
    #[serde(rename = "e")]
    exp: u64,
}

impl Flow {
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("flow state serializes")
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// A signed-in session.
#[derive(Debug, serde::Serialize, Deserialize, PartialEq, Eq)]
struct Session {
    #[serde(rename = "s")]
    subject: String,
    #[serde(rename = "m")]
    email: String,
    #[serde(rename = "nm", skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(rename = "hd", skip_serializing_if = "Option::is_none")]
    hosted_domain: Option<String>,
    #[serde(rename = "d")]
    deployment: String,
    /// Fingerprint of the allow-list this was issued under.
    #[serde(rename = "p")]
    policy: String,
    #[serde(rename = "e")]
    exp: u64,
}

impl Session {
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("session serializes")
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
}

/// The claims app-lb reads out of the id token.
#[derive(Debug, Deserialize, Default)]
struct IdClaims {
    iss: Option<String>,
    aud: Option<String>,
    sub: Option<String>,
    email: Option<String>,
    #[serde(default)]
    email_verified: BoolLike,
    hd: Option<String>,
    name: Option<String>,
    exp: Option<u64>,
}

/// `email_verified` is a boolean in Google's tokens, but the spec allows the
/// string form and other providers send it — accepting both costs nothing and
/// avoids a parse failure that would read as "sign-in is broken".
#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum BoolLike {
    Bool(bool),
    Str(String),
    #[default]
    Missing,
}

impl BoolLike {
    fn is_true(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Str(s) => s.eq_ignore_ascii_case("true"),
            Self::Missing => false,
        }
    }
}

/// Pull the claim set out of a JWT without verifying its signature — see the
/// module docs for why that is sound here, and only here.
fn decode_id_token(token: &str) -> Result<IdClaims, String> {
    let mut parts = token.split('.');
    let (_header, payload) = (
        parts.next().ok_or("id_token is not a JWT")?,
        parts.next().ok_or("id_token has no payload")?,
    );
    let bytes = b64()
        .decode(payload)
        .map_err(|e| format!("id_token payload is not base64url: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("id_token payload is not JSON: {e}"))
}

/// Check the claims that decide whether this token is about the right person,
/// from the right issuer, for this client, right now.
fn validate_claims(claims: &IdClaims, client_id: &str) -> Result<Identity, String> {
    let iss = claims.iss.as_deref().unwrap_or_default();
    if !GOOGLE_ISSUERS.contains(&iss) {
        return Err(format!("unexpected issuer {iss:?}"));
    }
    // Without this, a token minted for *another* application could be replayed
    // here — the single most important check in the set.
    match claims.aud.as_deref() {
        Some(aud) if ct_eq(aud.as_bytes(), client_id.as_bytes()) => {}
        _ => return Err("the id token was issued for a different client".into()),
    }
    if claims.exp.is_none_or(|exp| exp <= now_secs()) {
        return Err("the id token has expired".into());
    }
    let email = claims
        .email
        .clone()
        .ok_or("the id token carried no email address")?;
    if !claims.email_verified.is_true() {
        return Err(format!("{email} is not a verified address"));
    }

    Ok(Identity {
        subject: claims.sub.clone().unwrap_or_else(|| email.clone()),
        email: email.to_ascii_lowercase(),
        name: claims.name.clone(),
        hosted_domain: claims.hd.clone(),
    })
}

// -- small helpers ---------------------------------------------------------

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let pkey = openssl::pkey::PKey::hmac(key).expect("hmac key");
    let mut signer = openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &pkey)
        .expect("hmac signer");
    signer.update(data).expect("hmac update");
    signer.sign_to_vec().expect("hmac sign")
}

/// Length-then-content comparison that doesn't short-circuit, so a matching
/// prefix cannot be timed out of a signature or a nonce.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn random_token() -> String {
    let mut bytes = [0u8; 24];
    openssl::rand::rand_bytes(&mut bytes).expect("entropy for a sign-in nonce");
    b64().encode(bytes)
}

/// S256: the provider gets the hash, the cookie keeps the verifier.
fn pkce_challenge(verifier: &str) -> String {
    let digest = openssl::hash::hash(
        openssl::hash::MessageDigest::sha256(),
        verifier.as_bytes(),
    )
    .expect("sha256 of a byte string cannot fail");
    b64().encode(digest)
}

/// `https://host` or `http://host`, for building absolute URLs.
fn origin(req: &RequestInfo<'_>) -> String {
    let scheme = if req.secure { "https" } else { "http" };
    format!("{scheme}://{}", req.host)
}

/// Reduce a return target to something that cannot leave this site.
///
/// The value comes from the request line and is later used as a `Location`, so
/// it has to be a path and only a path: `//evil.example` is a protocol-relative
/// URL that browsers follow off-site, and a full URL would be an open redirect.
fn safe_return_path(path: &str) -> String {
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && !path.starts_with("/\\")
        && !path.contains(|c: char| c.is_control())
        && path.len() <= 2048;
    if ok { path.to_string() } else { "/".to_string() }
}

/// Find one cookie by name across every `Cookie` header on the request.
fn cookie_value(headers: &[String], name: &str) -> Option<String> {
    for header in headers {
        for pair in header.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=')
                && k.trim() == name
            {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn set_cookie(
    name: &str,
    value: &str,
    secure: bool,
    max_age: Option<u64>,
    domain: Option<&str>,
) -> String {
    let mut c = format!("{name}={value}; Path=/; HttpOnly");
    // Lax, not Strict: the provider's redirect back is a cross-site top-level
    // navigation, and Strict would withhold the flow cookie on exactly that
    // request — the sign-in would loop forever. It is also what lets a link from
    // the admin directory into a gated deployment carry the session, since that
    // navigation is cross-site too.
    c.push_str("; SameSite=Lax");
    // Present only for a realm session. A `Domain` widens the cookie to every
    // host under it, so it is never added by default and never guessed at — see
    // `AuthGate::cookie_domain`.
    if let Some(d) = domain {
        c.push_str(&format!("; Domain={d}"));
    }
    if secure {
        c.push_str("; Secure");
    }
    if let Some(age) = max_age {
        c.push_str(&format!("; Max-Age={age}"));
    }
    c
}

/// The `Domain` matters here too, and getting it wrong is a silent failure: a
/// cookie set with `Domain=example.com` and one set host-only are two different
/// cookies, and clearing the wrong one leaves the user signed in with a logout
/// that reported success.
fn clear_cookie(name: &str, secure: bool, domain: Option<&str>) -> String {
    let mut c = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if let Some(d) = domain {
        c.push_str(&format!("; Domain={d}"));
    }
    if secure {
        c.push_str("; Secure");
    }
    c
}

fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// A header value the caller's identity can safely be put in: printable ASCII,
/// bounded. A display name comes from the provider and can hold anything.
pub fn header_safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(256)
        .collect()
}

/// The headers app-lb sets from the session — and strips from every incoming
/// request to a gated deployment, so a client cannot forge them.
pub const IDENTITY_HEADERS: [&str; 3] = [
    "x-auth-request-email",
    "x-auth-request-user",
    "x-auth-request-name",
];

/// The key file's default location.
pub fn default_key_path() -> PathBuf {
    PathBuf::from("app-lb-auth-key")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::config::AuthProvider;
    use crate::secrets::{SecretRef, SecretSpec};
    use std::collections::BTreeMap;

    fn gate() -> AuthGate {
        AuthGate {
            provider: Default::default(),
            client_id: Some("cid.apps.googleusercontent.com".into()),
            client_secret: Some(SecretRef {
                secret: "google".into(),
                key: "client_secret".into(),
                username: None,
            }),
            allowed_domains: vec!["example.com".into()],
            allowed_emails: vec![],
            public_paths: vec![],
            base_path: "/__applb/auth".into(),
            session_ttl_secs: 3600,
            cookie_name: "applb_session".into(),
            cookie_domain: None,
            redirect_url: None,
            forward_identity: true,
        }
    }

    fn auth() -> Authenticator {
        let store = Arc::new(SecretStore::new("unused.json", None));
        store.put(SecretSpec {
            id: "google".into(),
            description: None,
            data: BTreeMap::from([("client_secret".to_string(), "s3cret".to_string())]),
            updated_at: 0,
        });
        Authenticator::new(vec![7u8; 32], store, None, None)
    }

    fn req<'a>(path: &'a str, cookies: Vec<String>) -> RequestInfo<'a> {
        RequestInfo {
            host: "app.example.com",
            path,
            query: None,
            cookies,
            secure: true,
            wants_html: true,
            bearer: None,
            client: None,
        }
    }

    fn identity(email: &str, hd: Option<&str>) -> Identity {
        Identity {
            subject: "sub-1".into(),
            email: email.into(),
            name: Some("A Person".into()),
            hosted_domain: hd.map(str::to_string),
        }
    }

    fn session_cookie(a: &Authenticator, g: &AuthGate, id: &Identity, exp: u64) -> String {
        let s = Session {
            subject: id.subject.clone(),
            email: id.email.clone(),
            name: id.name.clone(),
            hosted_domain: id.hosted_domain.clone(),
            deployment: "web".into(),
            policy: g.policy_fingerprint(),
            exp,
        };
        format!("{}={}", g.cookie_name, a.sign(&s.encode()))
    }

    // -- shared sign-in across sibling hostnames ---------------------------

    /// One sign-in covering every deployment under a parent domain is the whole
    /// point of `cookie_domain`; without this the session is refused at the
    /// second hostname and the user goes back to the provider for each card they
    /// open from the directory.
    mod realm {
        use super::*;

        fn realm_gate() -> AuthGate {
            AuthGate {
                cookie_domain: Some("example.com".into()),
                ..gate()
            }
        }

        fn at<'a>(host: &'a str, cookies: Vec<String>) -> RequestInfo<'a> {
            RequestInfo {
                host,
                ..req("/private", cookies)
            }
        }

        #[tokio::test]
        async fn a_session_from_one_deployment_is_accepted_at_a_sibling() {
            let (a, g) = (auth(), realm_gate());
            let id = identity("someone@example.com", Some("example.com"));
            // Issued by `web`, presented at `api`.
            let cookie = session_cookie(&a, &g, &id, now_secs() + 3600);
            let Decision::Allow(who) = a
                .decide(&g, "api", &at("api.example.com", vec![cookie]))
                .await
            else {
                panic!("a realm session must be accepted at a sibling deployment");
            };
            assert_eq!(who.unwrap().email, "someone@example.com");
        }

        /// The default. A host-only gate must keep refusing a session another
        /// deployment issued, or adding this feature would have quietly widened
        /// every existing gate.
        #[tokio::test]
        async fn without_a_realm_a_foreign_session_is_still_refused() {
            let (a, g) = (auth(), gate());
            let id = identity("someone@example.com", Some("example.com"));
            let cookie = session_cookie(&a, &g, &id, now_secs() + 3600);
            let Decision::Answered(r) = a
                .decide(&g, "api", &at("api.example.com", vec![cookie]))
                .await
            else {
                panic!("a host-only gate must not accept another deployment's session");
            };
            assert_eq!(r.status, 302, "and it starts a fresh sign-in");
        }

        /// The security property that makes sharing safe: sibling gates share a
        /// session only when either would have admitted the same person. A gate
        /// with a narrower allow-list has a different fingerprint and refuses.
        #[tokio::test]
        async fn a_sibling_with_a_different_allow_list_refuses_the_session() {
            let a = auth();
            let issuer = realm_gate();
            let id = identity("someone@example.com", Some("example.com"));
            let cookie = session_cookie(&a, &issuer, &id, now_secs() + 3600);

            let stricter = AuthGate {
                allowed_domains: vec![],
                allowed_emails: vec!["only-me@example.com".into()],
                ..realm_gate()
            };
            let Decision::Answered(r) = a
                .decide(&stricter, "api", &at("api.example.com", vec![cookie]))
                .await
            else {
                panic!("a differently-scoped gate must not honour the shared session");
            };
            assert_eq!(r.status, 302);
        }

        /// Turning sharing on changes the fingerprint, so a session minted by
        /// the host-only version of the same gate cannot be replayed once the
        /// realm is switched on — and vice versa.
        #[test]
        fn the_realm_is_part_of_the_policy_fingerprint() {
            assert_ne!(gate().policy_fingerprint(), realm_gate().policy_fingerprint());
            // And an unshared gate hashes exactly what it did before the field
            // existed, so upgrading does not sign everybody out.
            let other = AuthGate {
                cookie_domain: None,
                ..gate()
            };
            assert_eq!(gate().policy_fingerprint(), other.policy_fingerprint());
        }

        #[test]
        fn the_cookie_carries_the_domain_only_when_the_realm_covers_the_host() {
            let g = realm_gate();
            assert_eq!(g.cookie_domain_for("api.example.com").as_deref(), Some("example.com"));
            assert_eq!(g.cookie_domain_for("example.com").as_deref(), Some("example.com"));
            assert_eq!(g.cookie_domain_for("EXAMPLE.COM").as_deref(), Some("example.com"));
            // Suffix, but not at a label boundary — the classic way to write a
            // rule that looks right and matches the wrong domain.
            assert_eq!(g.cookie_domain_for("notexample.com"), None);
            assert_eq!(g.cookie_domain_for("example.com.evil.test"), None);
            assert_eq!(gate().cookie_domain_for("api.example.com"), None);
        }

        /// A logout must clear the cookie it actually set. A `Domain` cookie and
        /// a host-only one are two different cookies, so clearing the wrong one
        /// reports success and leaves the user signed in.
        #[test]
        fn logout_clears_the_realm_cookie_not_a_host_only_one() {
            let (a, g) = (auth(), realm_gate());
            let r = a.logout(&g, &at("api.example.com", vec![]));
            let session = r
                .cookies
                .iter()
                .find(|c| c.starts_with(&g.cookie_name))
                .expect("the session cookie is cleared");
            assert!(session.contains("Domain=example.com"), "{session}");
            assert!(session.contains("Max-Age=0"), "{session}");
            // The flow cookie was never widened, so clearing it must not be.
            let flow = r
                .cookies
                .iter()
                .find(|c| c.starts_with(FLOW_COOKIE))
                .expect("the flow cookie is cleared");
            assert!(!flow.contains("Domain="), "{flow}");
        }
    }

    // -- app-tokens at the data plane --------------------------------------

    mod app_token {
        use super::*;
        use crate::tokens::{AdminScope, NewToken, TokenStore};

        /// An authenticator that knows about tokens, plus the store to mint from.
        fn with_tokens() -> (Authenticator, Arc<TokenStore>) {
            let tokens = Arc::new(TokenStore::new("/nonexistent/tokens.json"));
            let secrets = Arc::new(SecretStore::new("/nonexistent/secrets.json", None));
            (
                Authenticator::new(vec![7u8; 32], secrets, Some(tokens.clone()), None),
                tokens,
            )
        }

        fn mint(t: &TokenStore, deployments: &[&str]) -> String {
            t.mint(
                NewToken {
                    name: "agent".into(),
                    // No admin API access at all — the point of this credential is
                    // to reach the application, not the LB.
                    admin: AdminScope::None,
                    deployments: deployments.iter().map(|d| d.to_string()).collect(),
                    expires_in_secs: None,
                },
                now_secs(),
            )
            .unwrap()
            .1
        }

        fn token_gate(providers: &str) -> AuthGate {
            serde_json::from_str(&format!(
                r#"{{"provider":{providers},"client_id":"cid","client_secret":{{"secret":"g"}},
                    "allowed_domains":["example.com"]}}"#
            ))
            .unwrap()
        }

        fn bearing<'a>(path: &'a str, token: &str) -> RequestInfo<'a> {
            RequestInfo {
                bearer: Some(token.to_string()),
                ..req(path, vec![])
            }
        }

        #[tokio::test]
        async fn a_scoped_token_gets_through_a_token_gate() {
            let (a, t) = with_tokens();
            let g = token_gate(r#""app-token""#);
            let secret = mint(&t, &["web"]);

            let Decision::Allow(identity) = a.decide(&g, "web", &bearing("/private", &secret)).await
            else {
                panic!("a scoped token should have been admitted");
            };
            assert!(
                identity.is_none(),
                "a token is not a person: nothing should be forwarded as an identity"
            );
        }

        #[tokio::test]
        async fn a_token_scoped_elsewhere_does_not_get_through() {
            let (a, t) = with_tokens();
            let g = token_gate(r#""app-token""#);
            let secret = mint(&t, &["other"]);

            // Refused as if no credential were presented — the deployment it *is*
            // scoped to is none of this deployment's business.
            let Decision::Answered(r) = a.decide(&g, "web", &bearing("/private", &secret)).await
            else {
                panic!("expected a refusal");
            };
            assert_eq!(r.status, 401);
        }

        #[tokio::test]
        async fn a_google_only_gate_ignores_tokens_entirely() {
            let (a, t) = with_tokens();
            let g = token_gate(r#""google""#);
            let secret = mint(&t, &["web"]);

            // A perfectly good token, on a gate that was never told to accept
            // one. Opting in is the spec's job, not the token's.
            let Decision::Answered(r) = a.decide(&g, "web", &bearing("/private", &secret)).await
            else {
                panic!("expected a redirect to the provider");
            };
            assert_eq!(r.status, 302);
        }

        /// The "both, either satisfies" shape: a person signs in, a program
        /// presents a token, and neither has to know the other exists.
        #[tokio::test]
        async fn a_gate_can_accept_a_session_or_a_token() {
            let (a, t) = with_tokens();
            let g = token_gate(r#"["google","app-token"]"#);
            let secret = mint(&t, &["web"]);

            // Program.
            assert!(matches!(
                a.decide(&g, "web", &bearing("/private", &secret)).await,
                Decision::Allow(_)
            ));

            // Person.
            let id = identity("someone@example.com", Some("example.com"));
            let cookie = session_cookie(&a, &g, &id, now_secs() + 3600);
            let Decision::Allow(who) = a.decide(&g, "web", &req("/private", vec![cookie])).await
            else {
                panic!("a signed-in person should still get through");
            };
            assert_eq!(who.as_ref().as_ref().map(|i| i.email.as_str()), Some("someone@example.com"));

            // Neither.
            assert!(matches!(
                a.decide(&g, "web", &req("/private", vec![])).await,
                Decision::Answered(_)
            ));
        }

        #[tokio::test]
        async fn a_revoked_token_stops_working_at_the_data_plane_too() {
            let (a, t) = with_tokens();
            let g = token_gate(r#""app-token""#);
            let secret = mint(&t, &["web"]);
            assert!(matches!(
                a.decide(&g, "web", &bearing("/private", &secret)).await,
                Decision::Allow(_)
            ));

            t.revoke(&t.list()[0].id);
            assert!(matches!(
                a.decide(&g, "web", &bearing("/private", &secret)).await,
                Decision::Answered(_)
            ));
        }

        /// A token-only gate has no sign-in flow, so a browser must be told what
        /// would actually work rather than bounced into an OAuth round trip that
        /// ends at an empty `client_id`.
        #[tokio::test]
        async fn a_browser_at_a_token_only_gate_is_told_what_it_needs() {
            let (a, _) = with_tokens();
            let g: AuthGate = serde_json::from_str(r#"{"provider":"app-token"}"#).unwrap();

            let Decision::Answered(r) = a.decide(&g, "web", &req("/private", vec![])).await else {
                panic!("expected a refusal");
            };
            assert_eq!(r.status, 401);
            assert!(r.location.is_none(), "there is nowhere to redirect to");
            assert!(r.body.contains("app-token"), "{}", r.body);
        }

        #[tokio::test]
        async fn public_paths_are_still_public_on_a_token_gate() {
            let (a, _) = with_tokens();
            let g: AuthGate =
                serde_json::from_str(r#"{"provider":"app-token","public_paths":["/healthz"]}"#)
                    .unwrap();
            assert!(matches!(
                a.decide(&g, "web", &req("/healthz", vec![])).await,
                Decision::Allow(_)
            ));
        }
    }

    #[tokio::test]
    async fn an_unauthenticated_browser_is_sent_to_the_provider() {
        let (a, g) = (auth(), gate());
        let Decision::Answered(r) = a.decide(&g, "web", &req("/dashboard", vec![])).await else {
            panic!("expected a redirect");
        };
        assert_eq!(r.status, 302);
        let location = r.location.unwrap();
        assert!(location.starts_with(GOOGLE_AUTHORIZE), "{location}");
        assert!(location.contains("client_id=cid.apps.googleusercontent.com"));
        assert!(location.contains("code_challenge_method=S256"));
        assert!(
            location.contains("redirect_uri=https%3A%2F%2Fapp.example.com%2F__applb%2Fauth%2Fcallback"),
            "{location}",
        );
        // The verifier must never reach the provider — only its hash.
        let flow_cookie = r.cookies.iter().find(|c| c.starts_with(FLOW_COOKIE)).unwrap();
        assert!(flow_cookie.contains("HttpOnly"), "{flow_cookie}");
        assert!(flow_cookie.contains("Secure"), "{flow_cookie}");
        assert!(flow_cookie.contains("SameSite=Lax"), "{flow_cookie}");
    }

    /// Regression: `SameSite=Strict` withholds the cookie on the provider's
    /// redirect back, which turns sign-in into an infinite loop.
    #[tokio::test]
    async fn the_flow_cookie_is_lax_so_the_callback_can_read_it() {
        let (a, g) = (auth(), gate());
        let Decision::Answered(r) = a.decide(&g, "web", &req("/", vec![])).await else {
            panic!("expected a redirect");
        };
        let flow = r.cookies.iter().find(|c| c.starts_with(FLOW_COOKIE)).unwrap();
        assert!(flow.contains("SameSite=Lax") && !flow.contains("Strict"), "{flow}");
    }

    #[tokio::test]
    async fn an_api_client_gets_a_401_with_a_login_url_instead_of_a_redirect() {
        let (a, g) = (auth(), gate());
        let mut r = req("/api/things", vec![]);
        r.wants_html = false;
        let Decision::Answered(response) = a.decide(&g, "web", &r).await else {
            panic!("expected a 401");
        };
        assert_eq!(response.status, 401);
        assert_eq!(response.content_type, "application/json");
        assert!(response.body.contains("https://app.example.com/__applb/auth/login"));
    }

    #[tokio::test]
    async fn a_valid_session_passes_through_with_its_identity() {
        let (a, g) = (auth(), gate());
        let id = identity("someone@example.com", Some("example.com"));
        let cookie = session_cookie(&a, &g, &id, now_secs() + 600);

        let Decision::Allow(got) = a.decide(&g, "web", &req("/dashboard", vec![cookie])).await
        else {
            panic!("expected the request to pass");
        };
        assert_eq!(*got, Some(id));
    }

    #[tokio::test]
    async fn public_paths_skip_the_gate_entirely() {
        let (a, mut g) = (auth(), gate());
        g.public_paths = vec!["/healthz".into(), "/hooks/".into()];

        for path in ["/healthz", "/hooks/github"] {
            let Decision::Allow(id) = a.decide(&g, "web", &req(path, vec![])).await else {
                panic!("{path} should be public");
            };
            assert_eq!(*id, None, "a public path has no identity to forward");
        }
        // ...and only those.
        assert!(matches!(
            a.decide(&g, "web", &req("/health", vec![])).await,
            Decision::Answered(_)
        ));
    }

    #[tokio::test]
    async fn a_tampered_or_expired_session_is_not_a_session() {
        let (a, g) = (auth(), gate());
        let id = identity("someone@example.com", Some("example.com"));

        // Expired.
        let stale = session_cookie(&a, &g, &id, now_secs() - 1);
        assert!(matches!(
            a.decide(&g, "web", &req("/", vec![stale])).await,
            Decision::Answered(_)
        ));

        // Signed with a different key.
        let other = Authenticator::new(vec![9u8; 32], Arc::new(SecretStore::new("x.json", None)), None, None);
        let forged = session_cookie(&other, &g, &id, now_secs() + 600);
        assert!(matches!(
            a.decide(&g, "web", &req("/", vec![forged])).await,
            Decision::Answered(_)
        ));

        // Payload edited, signature left alone.
        let good = session_cookie(&a, &g, &id, now_secs() + 600);
        let (name, token) = good.split_once('=').unwrap();
        let (payload, mac) = token.rsplit_once('.').unwrap();
        let mut edited = b64().decode(payload).unwrap();
        edited[0] ^= 0x20;
        let tampered = format!("{name}={}.{mac}", b64().encode(edited));
        assert!(matches!(
            a.decide(&g, "web", &req("/", vec![tampered])).await,
            Decision::Answered(_)
        ));
    }

    /// A session must not be usable at a different deployment, whose allow-list
    /// may be narrower.
    #[tokio::test]
    async fn a_session_is_bound_to_its_deployment() {
        let (a, g) = (auth(), gate());
        let cookie = session_cookie(&a, &g, &identity("someone@example.com", Some("example.com")), now_secs() + 600);
        assert!(matches!(
            a.decide(&g, "other", &req("/", vec![cookie])).await,
            Decision::Answered(_)
        ));
    }

    /// Removing somebody from the allow-list has to sign them out, not wait for
    /// their cookie to expire.
    #[tokio::test]
    async fn tightening_the_allow_list_invalidates_existing_sessions() {
        let (a, g) = (auth(), gate());
        let id = identity("someone@example.com", Some("example.com"));
        let cookie = session_cookie(&a, &g, &id, now_secs() + 600);
        assert!(matches!(
            a.decide(&g, "web", &req("/", vec![cookie.clone()])).await,
            Decision::Allow(_)
        ));

        let mut tightened = gate();
        tightened.allowed_domains = vec!["other.example".into()];
        assert!(matches!(
            a.decide(&tightened, "web", &req("/", vec![cookie])).await,
            Decision::Answered(_)
        ));
    }

    #[test]
    fn the_allow_list_matches_the_hosted_domain_not_the_email_suffix() {
        let mut g = gate();
        g.allowed_domains = vec!["example.com".into()];

        assert!(g.allows("someone@example.com", Some("example.com")));
        // A personal account whose address merely *looks* like the domain: the
        // `hd` claim is absent, so it is not governed by that Workspace.
        assert!(!g.allows("someone@example.com", None));
        assert!(!g.allows("someone@example.com", Some("elsewhere.com")));

        // An explicit address gets in regardless of domain.
        g.allowed_emails = vec!["Contractor@Gmail.com".into()];
        assert!(g.allows("contractor@gmail.com", None));

        // The escape hatch.
        let any = AuthGate {
            allowed_domains: vec!["*".into()],
            allowed_emails: vec![],
            ..gate()
        };
        assert!(any.allows("anyone@anywhere.example", None));
    }

    #[test]
    fn claims_are_checked_for_audience_issuer_expiry_and_verification() {
        let ok = IdClaims {
            iss: Some("https://accounts.google.com".into()),
            aud: Some("cid.apps.googleusercontent.com".into()),
            sub: Some("sub-1".into()),
            email: Some("Someone@Example.com".into()),
            email_verified: BoolLike::Bool(true),
            hd: Some("example.com".into()),
            name: Some("A Person".into()),
            exp: Some(now_secs() + 300),
        };
        let id = validate_claims(&ok, "cid.apps.googleusercontent.com").unwrap();
        assert_eq!(id.email, "someone@example.com", "normalized");
        assert_eq!(id.hosted_domain.as_deref(), Some("example.com"));

        // A token minted for another application must not be accepted here.
        let err = validate_claims(&ok, "someone-elses-client").unwrap_err();
        assert!(err.contains("different client"), "{err}");

        for (mutate, expect) in [
            (
                IdClaims { iss: Some("https://evil.example".into()), ..clone_claims(&ok) },
                "issuer",
            ),
            (IdClaims { exp: Some(now_secs() - 1), ..clone_claims(&ok) }, "expired"),
            (
                IdClaims { email_verified: BoolLike::Bool(false), ..clone_claims(&ok) },
                "not a verified",
            ),
            (IdClaims { email: None, ..clone_claims(&ok) }, "no email"),
        ] {
            let err = validate_claims(&mutate, "cid.apps.googleusercontent.com").unwrap_err();
            assert!(err.contains(expect), "expected {expect:?}, got {err:?}");
        }
    }

    /// `IdClaims` is not `Clone` (it is only ever built by serde), so the test
    /// above builds variants from a copy made here.
    fn clone_claims(c: &IdClaims) -> IdClaims {
        IdClaims {
            iss: c.iss.clone(),
            aud: c.aud.clone(),
            sub: c.sub.clone(),
            email: c.email.clone(),
            email_verified: match &c.email_verified {
                BoolLike::Bool(b) => BoolLike::Bool(*b),
                BoolLike::Str(s) => BoolLike::Str(s.clone()),
                BoolLike::Missing => BoolLike::Missing,
            },
            hd: c.hd.clone(),
            name: c.name.clone(),
            exp: c.exp,
        }
    }

    #[test]
    fn email_verified_is_accepted_as_a_bool_or_a_string() {
        assert!(BoolLike::Bool(true).is_true());
        assert!(BoolLike::Str("true".into()).is_true());
        assert!(BoolLike::Str("TRUE".into()).is_true());
        assert!(!BoolLike::Str("false".into()).is_true());
        // Absent means unverified, never "assume yes".
        assert!(!BoolLike::Missing.is_true());
    }

    #[test]
    fn an_id_token_payload_is_decoded_without_its_signature() {
        let payload = serde_json::json!({
            "iss": "https://accounts.google.com",
            "aud": "cid",
            "email": "a@b.com",
            "email_verified": true,
        });
        let token = format!(
            "{}.{}.{}",
            b64().encode(b"{\"alg\":\"RS256\"}"),
            b64().encode(payload.to_string()),
            b64().encode(b"not-a-real-signature"),
        );
        let claims = decode_id_token(&token).unwrap();
        assert_eq!(claims.email.as_deref(), Some("a@b.com"));

        assert!(decode_id_token("not-a-jwt").is_err());
        assert!(decode_id_token("aaa.!!!not-base64!!!.ccc").is_err());
    }

    /// A `Location` built from the request must not be able to leave the site.
    #[test]
    fn return_paths_cannot_become_an_open_redirect() {
        assert_eq!(safe_return_path("/dashboard?tab=1"), "/dashboard?tab=1");
        for hostile in [
            "//evil.example/",
            "https://evil.example/",
            "/\\evil.example",
            "dashboard",
            "",
        ] {
            assert_eq!(safe_return_path(hostile), "/", "{hostile:?} should be dropped");
        }
    }

    #[test]
    fn cookies_are_found_across_headers_and_neighbours() {
        let headers = vec![
            "other=1; applb_session=abc.def".to_string(),
            "unrelated=2".to_string(),
        ];
        assert_eq!(cookie_value(&headers, "applb_session").as_deref(), Some("abc.def"));
        assert_eq!(cookie_value(&headers, "unrelated").as_deref(), Some("2"));
        assert_eq!(cookie_value(&headers, "missing"), None);
        // A cookie whose *name* merely ends with the one we want must not match.
        let confusable = vec!["not_applb_session=nope".to_string()];
        assert_eq!(cookie_value(&confusable, "applb_session"), None);
    }

    #[test]
    fn signing_round_trips_and_rejects_anything_edited() {
        let a = auth();
        let token = a.sign(b"hello");
        assert_eq!(a.verify(&token).as_deref(), Some(&b"hello"[..]));
        assert_eq!(a.verify("garbage"), None);
        assert_eq!(a.verify(&format!("{token}x")), None);

        let other = Authenticator::new(vec![1u8; 32], Arc::new(SecretStore::new("x.json", None)), None, None);
        assert_eq!(other.verify(&token), None, "a different key must not verify");
    }

    #[test]
    fn the_pkce_challenge_is_the_hash_of_the_verifier() {
        // Vector from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_key_file_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("app-lb-authkey-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("auth-key");

        let first = Authenticator::load_key(&path).unwrap();
        assert_eq!(first.len(), 32);
        let second = Authenticator::load_key(&path).unwrap();
        assert_eq!(first, second, "a restart must not invalidate every session");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        }

        // A truncated key is an error, not a silent regeneration that would
        // invalidate sessions without saying so.
        std::fs::write(&path, b"short").unwrap();
        assert!(Authenticator::load_key(&path).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_callback_refuses_a_state_that_does_not_match_the_cookie() {
        let (a, g) = (auth(), gate());
        let flow = Flow {
            nonce: "the-real-nonce".into(),
            return_to: "/dashboard".into(),
            verifier: "v".into(),
            deployment: "web".into(),
            exp: now_secs() + 300,
        };
        let cookie = format!("{FLOW_COOKIE}={}", a.sign(&flow.encode()));

        let mut r = req("/__applb/auth/callback", vec![cookie]);
        r.query = Some("code=abc&state=a-different-nonce");
        let Decision::Answered(response) = a.decide(&g, "web", &r).await else {
            panic!("the callback always answers");
        };
        assert_eq!(response.status, 400);
        assert!(response.body.contains("did not match"), "{}", response.body);
    }

    #[tokio::test]
    async fn the_callback_reports_a_refusal_at_the_provider() {
        let (a, g) = (auth(), gate());
        let mut r = req("/__applb/auth/callback", vec![]);
        r.query = Some("error=access_denied");
        let Decision::Answered(response) = a.decide(&g, "web", &r).await else {
            panic!("the callback always answers");
        };
        assert_eq!(response.status, 403);
        assert!(response.body.contains("access_denied"));
    }

    /// A bookmarked callback URL has no flow cookie; starting over beats a
    /// dead-end error page.
    #[tokio::test]
    async fn a_callback_without_a_flow_cookie_starts_a_new_sign_in() {
        let (a, g) = (auth(), gate());
        let mut r = req("/__applb/auth/callback", vec![]);
        r.query = Some("code=abc&state=xyz");
        let Decision::Answered(response) = a.decide(&g, "web", &r).await else {
            panic!("the callback always answers");
        };
        assert_eq!(response.status, 302);
        assert!(response.location.unwrap().starts_with(GOOGLE_AUTHORIZE));
    }

    #[tokio::test]
    async fn logout_clears_the_session_cookie() {
        let (a, g) = (auth(), gate());
        let id = identity("someone@example.com", Some("example.com"));
        let cookie = session_cookie(&a, &g, &id, now_secs() + 600);
        let Decision::Answered(r) = a
            .decide(&g, "web", &req("/__applb/auth/logout", vec![cookie]))
            .await
        else {
            panic!("logout always answers");
        };
        assert_eq!(r.status, 302);
        let cleared = r.cookies.iter().find(|c| c.starts_with(&g.cookie_name)).unwrap();
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
    }

    #[test]
    fn a_plaintext_connection_gets_a_cookie_it_can_actually_send() {
        // `Secure` on a cookie set over http is dropped by the browser, which
        // would make the session invisible and loop the sign-in.
        assert!(!set_cookie("n", "v", false, None, None).contains("Secure"));
        assert!(set_cookie("n", "v", true, None, None).contains("Secure"));
    }

    #[test]
    fn identity_headers_are_reduced_to_something_a_header_can_hold() {
        assert_eq!(header_safe("A Person"), "A Person");
        assert_eq!(header_safe("bad\r\ninjected: yes"), "badinjected: yes");
        assert_eq!(header_safe("naïve 🙂").trim(), "nave");
        assert!(header_safe(&"x".repeat(500)).len() <= 256);
    }
}
