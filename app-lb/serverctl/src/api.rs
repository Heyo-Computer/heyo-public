//! The admin API, as methods.
//!
//! # Typed reads, `Value` writes
//!
//! Reads come back as the structs in [`crate::types`]. Writes take
//! `serde_json::Value`.
//!
//! That asymmetry is deliberate and load-bearing. `PUT /deployments/:id`
//! replaces a *whole* spec, so a client that parsed one into a struct it only
//! half-understood and wrote it back would silently delete every field this
//! build has never heard of. Round-tripping the `Value` cannot lose anything.
//! The read types are lenient for the mirror-image reason — unknown fields land
//! in `extra` and missing ones default, so a client a version behind still
//! renders what it understands.
//!
//! Typed *builders* for writes are a reasonable thing to want, and the way to
//! have them without the hazard is to build a `Value` and pass it here.

use crate::error::{Error, Result};
use crate::transport::{Auth, HttpTransport, Method, Request, Response, Transport};
use crate::types::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Default deadline for an ordinary request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// app-lb's own default `cold_start_timeout_secs`.
///
/// Used to size the deadline on a waking `exec`, because the caller's wall clock
/// is the command timeout *plus* however long a VM takes to appear — and the
/// client cannot know a deployment's configured value without asking. A
/// deployment configured higher needs [`ExecRequest::patience`].
pub const ASSUMED_COLD_START_SECS: u64 = 120;

/// Margin over the server-side deadline, so the client is never the one that
/// gives up first. Abandoning a request app-lb is still serving turns a
/// well-defined answer into an unexplained transport error.
const EXEC_MARGIN_SECS: u64 = 15;

/// Percent-encode one path segment.
///
/// Deployment and secret ids are constrained server-side, but a *token* id, a
/// job id or a sandbox id all arrive from elsewhere, and a stray `/` or `?`
/// would silently address a different route.
fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Which admin routes a server is gating.
///
/// app-lb has two independent gates, and which are on is not discoverable from
/// configuration — only by asking. Reported so a caller can say "you need
/// credentials" before a write fails rather than after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    /// `/metrics` and `/dashboard` need a credential.
    pub view: bool,
    /// The CRUD routes need one.
    pub crud: bool,
}

impl Gates {
    /// Whether anything at all is gated. `false` means there is no credential
    /// to log in *with*, which is a different thing from being logged out.
    pub fn any(&self) -> bool {
        self.view || self.crud
    }
}

/// A client for one app-lb.
///
/// Cheap to clone: everything behind it is an `Arc`.
#[derive(Debug, Clone)]
pub struct Client {
    transport: Arc<dyn Transport>,
    /// Kept alongside the transport so the shell can build a `ws://` URL and
    /// authenticate its own upgrade — it does not go through `Transport`.
    ws: Option<Arc<WsConfig>>,
}

/// What [`crate::shell`] needs that the HTTP transport cannot provide.
#[derive(Debug)]
pub(crate) struct WsConfig {
    pub base: String,
    pub auth: Auth,
    pub insecure: bool,
}

impl Client {
    /// Connect to `server`, which may be a URL or a bare `host:port`.
    pub fn new(server: impl Into<String>, auth: Auth) -> Result<Self> {
        Self::builder(server).auth(auth).build()
    }

    pub fn builder(server: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            server: server.into(),
            auth: Auth::None,
            timeout: DEFAULT_TIMEOUT,
            insecure: false,
        }
    }

    /// Build on an arbitrary transport — a stub, a recorder, a proxy.
    ///
    /// Shell sessions are unavailable on a client made this way: a WebSocket
    /// does not go through [`Transport`], so there is nothing to point it at.
    pub fn with_transport(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            ws: None,
        }
    }

    // -- plumbing ----------------------------------------------------------

    async fn send(&self, req: Request, kind: &'static str, name: &str) -> Result<Response> {
        let r = self.transport.send(req).await?;
        if r.is_success() {
            Ok(r)
        } else {
            Err(Error::from_response(
                r.status,
                &r.body,
                kind,
                name,
                self.transport.credential(),
            ))
        }
    }

    async fn read<T: DeserializeOwned>(
        &self,
        req: Request,
        kind: &'static str,
        name: &str,
    ) -> Result<T> {
        let r = self.send(req, kind, name).await?;
        serde_json::from_str(&r.body).map_err(Error::Decode)
    }

    async fn unit(&self, req: Request, kind: &'static str, name: &str) -> Result<()> {
        self.send(req, kind, name).await.map(|_| ())
    }

    // -- health and discovery ----------------------------------------------

    /// `GET /healthz`. Never gated, so this also proves reachability without a
    /// credential.
    pub async fn healthz(&self) -> Result<()> {
        self.unit(Request::new(Method::Get, "/healthz"), "server", "")
            .await
    }

    /// Which tiers this server gates, discovered by probing anonymously.
    ///
    /// Two requests, and they must be *unauthenticated* — the point is to learn
    /// what an anonymous caller is refused. Uses a throwaway transport rather
    /// than this client's, which is carrying a credential.
    pub async fn gates(server: &str, insecure: bool) -> Result<Gates> {
        let anon = Client::builder(server).insecure(insecure).build()?;
        let refused = |e: &Error| matches!(e, Error::Unauthorized { .. });
        let view = anon
            .read::<Value>(Request::new(Method::Get, "/metrics"), "server", "")
            .await
            .err()
            .as_ref()
            .is_some_and(refused);
        let crud = anon
            .read::<Value>(Request::new(Method::Get, "/deployments"), "server", "")
            .await
            .err()
            .as_ref()
            .is_some_and(refused);
        Ok(Gates { view, crud })
    }

    // -- deployments --------------------------------------------------------

    pub async fn deployments(&self) -> Result<Vec<DeploymentStatus>> {
        self.read(Request::new(Method::Get, "/deployments"), "deployment", "")
            .await
    }

    pub async fn deployment(&self, id: &str) -> Result<DeploymentStatus> {
        self.read(
            Request::new(Method::Get, format!("/deployments/{}", seg(id))),
            "deployment",
            id,
        )
        .await
    }

    /// Whether a deployment exists, without treating absence as an error.
    pub async fn deployment_exists(&self, id: &str) -> Result<bool> {
        match self.deployment(id).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `POST /deployments`.
    ///
    /// Note app-lb answers `201` even when this *replaced* an existing
    /// deployment, so the status does not distinguish create from update.
    /// Certificate issuance is asynchronous: a success here does not mean a
    /// certificate exists yet.
    pub async fn create_deployment(&self, spec: &Value) -> Result<DeploymentStatus> {
        let id = spec.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.read(
            Request::new(Method::Post, "/deployments").json(spec.clone()),
            "deployment",
            &id,
        )
        .await
    }

    /// `PUT /deployments/:id` — a whole-spec replace.
    ///
    /// Takes a `Value` so nothing this build does not understand is dropped on
    /// the way through. Read with [`Client::deployment`], edit the `spec` field,
    /// pass it back.
    pub async fn replace_deployment(&self, id: &str, spec: &Value) -> Result<DeploymentStatus> {
        self.read(
            Request::new(Method::Put, format!("/deployments/{}", seg(id))).json(spec.clone()),
            "deployment",
            id,
        )
        .await
    }

    /// `PATCH /deployments/:id/scaling` — a shallow merge onto the current
    /// policy. Only meaningful for a managed (VM-pool) deployment.
    pub async fn patch_scaling(&self, id: &str, patch: &Value) -> Result<DeploymentStatus> {
        self.read(
            Request::new(
                Method::Patch,
                format!("/deployments/{}/scaling", seg(id)),
            )
            .json(patch.clone()),
            "deployment",
            id,
        )
        .await
    }

    pub async fn delete_deployment(&self, id: &str) -> Result<()> {
        self.unit(
            Request::new(Method::Delete, format!("/deployments/{}", seg(id))),
            "deployment",
            id,
        )
        .await
    }

    /// Evict one VM. `force` kills immediately; otherwise it drains.
    pub async fn evict_vm(&self, id: &str, sandbox: &str, force: bool) -> Result<EvictOutcome> {
        // app-lb parses query booleans with `str::parse::<bool>()`, which takes
        // only `true`/`false` — `?force=1` is a 400, not a truthy value.
        let path = format!(
            "/deployments/{}/vms/{}?force={}",
            seg(id),
            seg(sandbox),
            if force { "true" } else { "false" }
        );
        self.read(Request::new(Method::Delete, path), "vm", sandbox)
            .await
    }

    // -- running things inside a VM ----------------------------------------

    /// Run a command in the deployment's VM and wait for it to finish.
    ///
    /// Two things to know:
    ///
    /// - **A non-zero exit is `Ok`.** The command ran; it failed. Only an
    ///   inability to *run* it is an `Err`.
    /// - **The timeout does not kill anything.** `timeout_secs` bounds app-lb's
    ///   own call to the daemon; when it expires app-lb gives up and answers
    ///   [`Error::Upstream`], and **the command keeps running in the guest**.
    ///   There is no cancellation to offer — the daemon has no streaming or
    ///   cancel API, so output is buffered until the command exits.
    pub async fn exec(&self, id: &str, req: &ExecRequest) -> Result<ExecOutput> {
        if req.command.trim().is_empty() {
            return Err(Error::Invalid("a command to exec must not be blank".into()));
        }
        let mut body = json!({ "command": req.command, "wake": req.wake });
        if let Some(cwd) = &req.cwd {
            body["cwd"] = json!(cwd);
        }
        if let Some(env) = &req.env {
            body["env"] = json!(env);
        }
        if let Some(t) = req.timeout_secs {
            body["timeout_secs"] = json!(t);
        }
        self.read(
            Request::new(Method::Post, format!("/deployments/{}/exec", seg(id)))
                .json(body)
                .timeout(req.patience()),
            "deployment",
            id,
        )
        .await
    }

    // -- secrets ------------------------------------------------------------

    pub async fn secrets(&self) -> Result<Vec<SecretSummary>> {
        self.read(Request::new(Method::Get, "/secrets"), "secret", "")
            .await
    }

    pub async fn secret(&self, id: &str) -> Result<SecretSummary> {
        self.read(
            Request::new(Method::Get, format!("/secrets/{}", seg(id))),
            "secret",
            id,
        )
        .await
    }

    pub async fn secret_exists(&self, id: &str) -> Result<bool> {
        match self.secret(id).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Store a secret. Values enter here and are never readable again — no
    /// endpoint returns one.
    pub async fn put_secret(&self, spec: &Value) -> Result<SecretSummary> {
        let id = spec.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.read(
            Request::new(Method::Post, "/secrets").json(spec.clone()),
            "secret",
            &id,
        )
        .await
    }

    /// Change individual keys. A `null` value deletes that key; absent keys are
    /// left alone.
    pub async fn patch_secret(&self, id: &str, patch: &Value) -> Result<SecretSummary> {
        self.read(
            Request::new(Method::Patch, format!("/secrets/{}", seg(id))).json(patch.clone()),
            "secret",
            id,
        )
        .await
    }

    /// Delete a secret. Refused with [`Error::Conflict`] if a deployment still
    /// references it, unless `force`.
    pub async fn delete_secret(&self, id: &str, force: bool) -> Result<()> {
        self.unit(
            Request::new(
                Method::Delete,
                format!(
                    "/secrets/{}?force={}",
                    seg(id),
                    if force { "true" } else { "false" }
                ),
            ),
            "secret",
            id,
        )
        .await
    }

    // -- app-tokens ---------------------------------------------------------

    /// Mint a token. **The secret in the reply is shown once** — app-lb keeps
    /// only its hash, and no endpoint reads it back.
    pub async fn mint_token(&self, req: &NewToken) -> Result<MintedToken> {
        if req.name.trim().is_empty() {
            return Err(Error::Invalid(
                "a token needs a name — it is how you know what to revoke".into(),
            ));
        }
        let mut body = json!({
            "name": req.name,
            "admin": req.admin,
            "deployments": req.deployments,
        });
        if let Some(s) = req.expires_in_secs {
            body["expires_in_secs"] = json!(s);
        }
        self.read(
            Request::new(Method::Post, "/tokens").json(body),
            "token",
            &req.name,
        )
        .await
    }

    pub async fn tokens(&self) -> Result<Vec<TokenSummary>> {
        self.read(Request::new(Method::Get, "/tokens"), "token", "")
            .await
    }

    pub async fn token(&self, id: &str) -> Result<TokenSummary> {
        self.read(
            Request::new(Method::Get, format!("/tokens/{}", seg(id))),
            "token",
            id,
        )
        .await
    }

    /// Re-scope a token **without changing its secret**, so narrowing a
    /// credential does not mean redistributing it.
    pub async fn patch_token(&self, id: &str, patch: &Value) -> Result<TokenSummary> {
        self.read(
            Request::new(Method::Patch, format!("/tokens/{}", seg(id))).json(patch.clone()),
            "token",
            id,
        )
        .await
    }

    /// Revoke. Effective on the next request — verification is a store lookup,
    /// not a signature check.
    pub async fn revoke_token(&self, id: &str) -> Result<()> {
        self.unit(
            Request::new(Method::Delete, format!("/tokens/{}", seg(id))),
            "token",
            id,
        )
        .await
    }

    // -- jobs ---------------------------------------------------------------

    /// Start an image build. Returns immediately with the job to poll — see
    /// [`Client::wait_for_job`](crate::wait).
    pub async fn start_build(&self, id: &str, git_ref: Option<&str>) -> Result<JobRecord> {
        // Always a body with a content-type, even when empty: app-lb takes these
        // as `Option<Json<T>>`, and axum silently swallows a malformed or
        // untyped body into the default rather than rejecting it.
        let body = git_ref.map_or_else(|| json!({}), |r| json!({ "ref": r }));
        self.read(
            Request::new(Method::Post, format!("/deployments/{}/build", seg(id))).json(body),
            "deployment",
            id,
        )
        .await
    }

    pub async fn start_pull(&self, id: &str, artifact_ref: Option<&str>, force: bool) -> Result<JobRecord> {
        let mut body = json!({ "force": force });
        if let Some(r) = artifact_ref {
            body["ref"] = json!(r);
        }
        self.read(
            Request::new(Method::Post, format!("/deployments/{}/pull", seg(id))).json(body),
            "deployment",
            id,
        )
        .await
    }

    pub async fn start_update(&self, id: &str) -> Result<JobRecord> {
        self.read(
            Request::new(Method::Post, format!("/deployments/{}/update", seg(id))).json(json!({})),
            "deployment",
            id,
        )
        .await
    }

    pub async fn jobs(&self) -> Result<Vec<JobRecord>> {
        self.read(Request::new(Method::Get, "/jobs"), "job", "")
            .await
    }

    pub async fn deployment_jobs(&self, id: &str) -> Result<Vec<JobRecord>> {
        self.read(
            Request::new(Method::Get, format!("/deployments/{}/jobs", seg(id))),
            "deployment",
            id,
        )
        .await
    }

    /// One job. A `404` here can mean it aged out of the bounded history rather
    /// than that it never existed.
    pub async fn job(&self, job_id: &str) -> Result<JobRecord> {
        self.read(
            Request::new(Method::Get, format!("/jobs/{}", seg(job_id))),
            "job",
            job_id,
        )
        .await
    }

    // -- observability ------------------------------------------------------

    /// `GET /metrics`, scoped by `query`.
    ///
    /// The unfiltered response is megabytes at fleet scale, so prefer
    /// [`MetricsQuery::summary`] and paging. Note `fleet`, `global` and `host`
    /// always describe everything the credential can see, never the page.
    pub async fn metrics(&self, query: &MetricsQuery) -> Result<MetricsResponse> {
        self.read(
            Request::new(Method::Get, format!("/metrics{}", query.to_query_string())),
            "server",
            "",
        )
        .await
    }

    pub async fn certs(&self) -> Result<Vec<CertStatus>> {
        self.read(Request::new(Method::Get, "/certs"), "certificate", "")
            .await
    }

    pub(crate) fn ws(&self) -> Option<&Arc<WsConfig>> {
        self.ws.as_ref()
    }

    /// The status a `GET` answers with, treating 4xx as an answer rather than an
    /// error.
    ///
    /// For probing: which gate is on, and whether a credential satisfies it.
    /// Every other method turns a non-2xx into an [`Error`], which is right for
    /// a request you meant and wrong for a question you are asking.
    pub async fn probe(&self, path: &str) -> Result<u16> {
        Ok(self
            .transport
            .send(Request::new(Method::Get, path.to_string()))
            .await?
            .status)
    }

    /// The base URL, when this client was built from one.
    pub fn server(&self) -> Option<&str> {
        self.ws.as_ref().map(|w| w.base.as_str())
    }

    /// Whether any credential is being presented.
    pub fn has_credentials(&self) -> bool {
        self.transport.credential() != crate::error::Credential::None
    }

    /// The same reads, as unparsed JSON.
    ///
    /// Two callers need this and neither is being lazy:
    ///
    /// - anything that **prints** a response. Re-serializing one of the typed
    ///   views would drop whatever this build does not name, so
    ///   `serverctl get … -o json` would quietly print less than the server
    ///   sent.
    /// - the read half of a read-modify-write. `PUT /deployments/:id` replaces
    ///   the whole spec, so the thing you edit has to be the thing that came
    ///   back — see the module docs.
    pub fn raw(&self) -> Raw<'_> {
        Raw(self)
    }
}

/// Unparsed reads. See [`Client::raw`].
#[derive(Debug, Clone, Copy)]
pub struct Raw<'a>(&'a Client);

/// A collection route: no id, fixed path.
macro_rules! raw_list {
    ($($name:ident => $kind:literal, $path:literal;)*) => {
        $(
            pub async fn $name(&self) -> Result<Value> {
                self.0.read(Request::new(Method::Get, $path), $kind, "").await
            }
        )*
    };
}

/// An item route: the id goes into the path, percent-encoded.
macro_rules! raw_item {
    ($($name:ident => $kind:literal, $fmt:literal;)*) => {
        $(
            pub async fn $name(&self, id: &str) -> Result<Value> {
                self.0
                    .read(Request::new(Method::Get, format!($fmt, seg(id))), $kind, id)
                    .await
            }
        )*
    };
}

impl Raw<'_> {
    raw_list! {
        deployments => "deployment", "/deployments";
        secrets     => "secret",     "/secrets";
        tokens      => "token",      "/tokens";
        jobs        => "job",        "/jobs";
        certs       => "certificate", "/certs";
    }

    raw_item! {
        deployment      => "deployment", "/deployments/{}";
        secret          => "secret",     "/secrets/{}";
        token           => "token",      "/tokens/{}";
        job             => "job",        "/jobs/{}";
        deployment_jobs => "deployment", "/deployments/{}/jobs";
    }

    pub async fn metrics(&self, query: &MetricsQuery) -> Result<Value> {
        self.0
            .read(
                Request::new(Method::Get, format!("/metrics{}", query.to_query_string())),
                "server",
                "",
            )
            .await
    }

    // The write side, for the same reason: a caller that prints what a write
    // returned should print what the server said, not a re-serialization of a
    // struct that may know fewer fields than the server sent.

    pub async fn create_deployment(&self, spec: &Value) -> Result<Value> {
        let id = spec.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.0
            .read(
                Request::new(Method::Post, "/deployments").json(spec.clone()),
                "deployment",
                &id,
            )
            .await
    }

    pub async fn replace_deployment(&self, id: &str, spec: &Value) -> Result<Value> {
        self.0
            .read(
                Request::new(Method::Put, format!("/deployments/{}", seg(id))).json(spec.clone()),
                "deployment",
                id,
            )
            .await
    }

    pub async fn patch_scaling(&self, id: &str, patch: &Value) -> Result<Value> {
        self.0
            .read(
                Request::new(Method::Patch, format!("/deployments/{}/scaling", seg(id)))
                    .json(patch.clone()),
                "deployment",
                id,
            )
            .await
    }

    pub async fn put_secret(&self, spec: &Value) -> Result<Value> {
        let id = spec.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        self.0
            .read(
                Request::new(Method::Post, "/secrets").json(spec.clone()),
                "secret",
                &id,
            )
            .await
    }

    pub async fn patch_secret(&self, id: &str, patch: &Value) -> Result<Value> {
        self.0
            .read(
                Request::new(Method::Patch, format!("/secrets/{}", seg(id))).json(patch.clone()),
                "secret",
                id,
            )
            .await
    }

    pub async fn mint_token(&self, req: &NewToken) -> Result<Value> {
        let body = json!({
            "name": req.name,
            "admin": req.admin,
            "deployments": req.deployments,
            "expires_in_secs": req.expires_in_secs,
        });
        self.0
            .read(
                Request::new(Method::Post, "/tokens").json(body),
                "token",
                &req.name,
            )
            .await
    }

    pub async fn patch_token(&self, id: &str, patch: &Value) -> Result<Value> {
        self.0
            .read(
                Request::new(Method::Patch, format!("/tokens/{}", seg(id))).json(patch.clone()),
                "token",
                id,
            )
            .await
    }

    pub async fn evict_vm(&self, id: &str, sandbox: &str, force: bool) -> Result<Value> {
        self.0
            .read(
                Request::new(
                    Method::Delete,
                    format!(
                        "/deployments/{}/vms/{}?force={}",
                        seg(id),
                        seg(sandbox),
                        if force { "true" } else { "false" }
                    ),
                ),
                "vm",
                sandbox,
            )
            .await
    }

    pub async fn start_build(&self, id: &str, git_ref: Option<&str>) -> Result<Value> {
        let body = git_ref.map_or_else(|| json!({}), |r| json!({ "ref": r }));
        self.0
            .read(
                Request::new(Method::Post, format!("/deployments/{}/build", seg(id))).json(body),
                "deployment",
                id,
            )
            .await
    }

    pub async fn start_pull(&self, id: &str, artifact_ref: Option<&str>, force: bool) -> Result<Value> {
        let mut body = json!({ "force": force });
        if let Some(r) = artifact_ref {
            body["ref"] = json!(r);
        }
        self.0
            .read(
                Request::new(Method::Post, format!("/deployments/{}/pull", seg(id))).json(body),
                "deployment",
                id,
            )
            .await
    }

    pub async fn start_update(&self, id: &str) -> Result<Value> {
        self.0
            .read(
                Request::new(Method::Post, format!("/deployments/{}/update", seg(id)))
                    .json(json!({})),
                "deployment",
                id,
            )
            .await
    }

    /// A deployment's spec alone, ready to edit and hand back to
    /// [`Client::replace_deployment`].
    pub async fn spec(&self, id: &str) -> Result<Value> {
        let status = self.deployment(id).await?;
        status
            .get("spec")
            .cloned()
            .ok_or_else(|| Error::Decode(serde::de::Error::custom("the response had no `spec`")))
    }
}

pub struct ClientBuilder {
    server: String,
    auth: Auth,
    timeout: Duration,
    insecure: bool,
}

impl ClientBuilder {
    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    /// Authenticate with an app-token. The normal choice for a program.
    pub fn token(self, token: impl Into<String>) -> Self {
        self.auth(Auth::Token(token.into()))
    }

    /// Authenticate with the operator credential.
    pub fn basic(self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth(Auth::Basic {
            user: user.into(),
            password: password.into(),
        })
    }

    /// Per-request deadline. `exec` computes its own, larger, one.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Skip TLS verification. For a self-signed admin listener behind a tunnel.
    pub fn insecure(mut self, yes: bool) -> Self {
        self.insecure = yes;
        self
    }

    pub fn build(self) -> Result<Client> {
        let base = crate::transport::normalize_base(&self.server);
        let transport = HttpTransport::new(
            base.clone(),
            self.auth.clone(),
            self.timeout,
            self.insecure,
        )?;
        Ok(Client {
            transport: Arc::new(transport),
            ws: Some(Arc::new(WsConfig {
                base,
                auth: self.auth,
                insecure: self.insecure,
            })),
        })
    }
}

/// A command to run in a VM.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// Run through `sh -c` in the guest.
    pub command: String,
    pub cwd: Option<String>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Bounds app-lb's call to the daemon. Clamped server-side to `1..=3600`.
    pub timeout_secs: Option<u64>,
    /// Boot or resume a VM if none is running. On by default: a sandbox that
    /// scaled to zero should still answer. `false` asks for
    /// [`Error::NoRunningVm`] instead of a wait.
    pub wake: bool,
    /// Override the client-side deadline. See [`ExecRequest::patience`].
    pub patience: Option<Duration>,
}

impl ExecRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            env: None,
            timeout_secs: None,
            wake: true,
            patience: None,
        }
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env
            .get_or_insert_with(Default::default)
            .insert(key.into(), value.into());
        self
    }

    pub fn timeout_secs(mut self, s: u64) -> Self {
        self.timeout_secs = Some(s);
        self
    }

    /// Ask for [`Error::NoRunningVm`] rather than waiting on a cold start.
    pub fn no_wake(mut self) -> Self {
        self.wake = false;
        self
    }

    /// How long the *client* waits, overriding the computed default.
    pub fn patient_for(mut self, d: Duration) -> Self {
        self.patience = Some(d);
        self
    }

    /// The client-side deadline.
    ///
    /// Must exceed what the server might take, or the client abandons a request
    /// app-lb is still serving and the caller gets a transport error instead of
    /// an answer. The server's worst case is the command timeout **plus** a cold
    /// start when `wake` is set — a deployment's actual
    /// `cold_start_timeout_secs` is not knowable without another round trip, so
    /// this assumes app-lb's default. Override on a deployment configured
    /// higher.
    pub fn patience(&self) -> Duration {
        if let Some(d) = self.patience {
            return d;
        }
        let command = self.timeout_secs.unwrap_or(60).clamp(1, 3600);
        let cold = if self.wake { ASSUMED_COLD_START_SECS } else { 0 };
        Duration::from_secs(command + cold + EXEC_MARGIN_SECS)
    }
}

/// What to ask `/metrics` for.
#[derive(Debug, Clone, Default)]
pub struct MetricsQuery {
    /// Exactly one deployment.
    pub deployment: Option<String>,
    /// Every deployment whose id starts with this.
    pub prefix: Option<String>,
    /// Drop per-VM detail, which is most of the payload.
    pub summary: bool,
    pub limit: Option<usize>,
    pub offset: usize,
}

impl MetricsQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn deployment(mut self, id: impl Into<String>) -> Self {
        self.deployment = Some(id.into());
        self
    }

    pub fn prefix(mut self, p: impl Into<String>) -> Self {
        self.prefix = Some(p.into());
        self
    }

    pub fn summary(mut self, yes: bool) -> Self {
        self.summary = yes;
        self
    }

    pub fn page(mut self, offset: usize, limit: usize) -> Self {
        self.offset = offset;
        self.limit = Some(limit);
        self
    }

    fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(d) = &self.deployment {
            parts.push(format!("deployment={}", seg(d)));
        }
        if let Some(p) = &self.prefix {
            parts.push(format!("prefix={}", seg(p)));
        }
        if self.summary {
            // Only `true`/`false` parse server-side.
            parts.push("summary=true".into());
        }
        if let Some(l) = self.limit {
            parts.push(format!("limit={l}"));
        }
        if self.offset > 0 {
            parts.push(format!("offset={}", self.offset));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("?{}", parts.join("&"))
        }
    }
}

/// A token to mint.
///
/// Both scope fields default to nothing: a token minted with no scope can do
/// nothing, which is a harmless mistake. The other default would turn a
/// forgotten field into fleet-wide credentials.
#[derive(Debug, Clone, Default)]
pub struct NewToken {
    pub name: String,
    pub admin: AdminScope,
    pub deployments: Vec<String>,
    pub expires_in_secs: Option<u64>,
}

impl NewToken {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn admin(mut self, scope: AdminScope) -> Self {
        self.admin = scope;
        self
    }

    /// Scope to specific deployments. Such a token is refused the fleet-wide
    /// routes, including minting — so it cannot widen itself.
    pub fn for_deployments<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deployments = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Scope to every deployment.
    pub fn fleet_wide(mut self) -> Self {
        self.deployments = vec!["*".into()];
        self
    }

    pub fn expires_in(mut self, d: Duration) -> Self {
        self.expires_in_secs = Some(d.as_secs());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::stub::Stub;

    fn client(stub: Stub) -> (Client, Arc<Stub>) {
        let s = Arc::new(stub);
        (Client::with_transport(s.clone()), s)
    }

    #[tokio::test]
    async fn a_read_is_typed_and_a_write_is_not() {
        let (c, stub) = client(Stub::new().json(
            201,
            json!({"spec": {"id": "demo", "routes": [], "unknown_future_field": 7},
                   "kind": "vm", "desired_replicas": 1, "ready": 0, "pending": 1,
                   "total_in_flight": 0, "vms": []}),
        ));
        let spec = json!({"id": "demo", "routes": [], "unknown_future_field": 7});
        let got = c.create_deployment(&spec).await.unwrap();
        assert_eq!(got.kind, "vm");

        // The body went out verbatim — a field this build has never heard of is
        // not something a client gets to drop.
        assert_eq!(stub.calls()[0].body.as_ref().unwrap(), &spec);
    }

    #[tokio::test]
    async fn a_failed_request_becomes_a_typed_error() {
        let (c, _) = client(Stub::new().json(404, json!({"error": "no deployment \"demo\""})));
        let e = c.deployment("demo").await.unwrap_err();
        assert!(matches!(&e, Error::NotFound { kind: "deployment", name } if name == "demo"));
    }

    #[tokio::test]
    async fn absence_is_a_bool_not_an_error_where_that_is_the_question() {
        let (c, _) = client(Stub::new().json(404, json!({"error": "no deployment \"demo\""})));
        assert!(!c.deployment_exists("demo").await.unwrap());

        // But a *real* failure still propagates rather than reading as absence.
        let (c, _) = client(Stub::new().reply(401, "authentication required\n"));
        assert!(c.deployment_exists("demo").await.is_err());
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_not_an_error() {
        let (c, _) = client(Stub::new().json(
            200,
            json!({"sandbox_id": "sb-1", "exit_code": 42, "stdout": "", "stderr": "nope\n",
                   "output": "nope\n"}),
        ));
        let out = c.exec("demo", &ExecRequest::new("false")).await.unwrap();
        assert_eq!(out.exit_code, 42);
        assert!(!out.ok());
    }

    #[tokio::test]
    async fn a_blank_command_never_reaches_the_wire() {
        let (c, stub) = client(Stub::new());
        let e = c.exec("demo", &ExecRequest::new("   ")).await.unwrap_err();
        assert!(matches!(e, Error::Invalid(_)));
        assert_eq!(stub.call_count(), 0, "nothing should have been sent");
    }

    /// The CLI this replaces waited `timeout + 30s`, which is shorter than the
    /// server's own worst case whenever a cold start is possible — so it
    /// abandoned requests app-lb was still serving.
    #[test]
    fn the_exec_deadline_outlasts_the_servers_worst_case() {
        let waking = ExecRequest::new("x").timeout_secs(60);
        assert!(
            waking.patience() > Duration::from_secs(60 + ASSUMED_COLD_START_SECS),
            "a waking exec must outlast command timeout + cold start"
        );

        // With no wake there is no boot to wait through.
        let not_waking = ExecRequest::new("x").timeout_secs(60).no_wake();
        assert!(not_waking.patience() < waking.patience());
        assert!(not_waking.patience() > Duration::from_secs(60));

        // The server clamps its own timeout to 3600; the client must still
        // outlast that rather than trusting the number it was handed.
        let absurd = ExecRequest::new("x").timeout_secs(99_999).no_wake();
        assert!(absurd.patience() > Duration::from_secs(3600));

        assert_eq!(
            ExecRequest::new("x").patient_for(Duration::from_secs(5)).patience(),
            Duration::from_secs(5),
            "an explicit override wins"
        );
    }

    #[tokio::test]
    async fn query_booleans_are_spelled_the_only_way_app_lb_accepts() {
        // `?force=1` is a 400 server-side, not a truthy value.
        let (c, stub) = client(Stub::new().json(200, json!({"sandbox_id": "s", "outcome": "killed"})));
        c.evict_vm("demo", "sb-1", true).await.unwrap();
        assert!(stub.calls()[0].path.ends_with("?force=true"), "{:?}", stub.calls()[0].path);

        let (c, stub) = client(Stub::new().json(200, json!({"sandbox_id": "s", "outcome": "killed"})));
        c.evict_vm("demo", "sb-1", false).await.unwrap();
        assert!(stub.calls()[0].path.ends_with("?force=false"));
    }

    #[tokio::test]
    async fn ids_are_escaped_into_the_path() {
        let (c, stub) = client(Stub::new().json(404, json!({"error": "no deployment"})));
        let _ = c.deployment("a/b?c=d").await;
        assert_eq!(stub.calls()[0].path, "/deployments/a%2Fb%3Fc%3Dd");
    }

    /// app-lb takes these as `Option<Json<T>>`, and axum turns *any* extractor
    /// rejection on an `Option` into the default — so a build started with no
    /// content-type silently loses its `ref` instead of failing.
    #[tokio::test]
    async fn build_and_pull_always_send_a_json_body() {
        let (c, stub) = client(Stub::new().json(202, json!({"id": "j1", "deployment": "d",
            "kind": "image-build", "status": "running", "started_at": 0})));
        c.start_build("demo", None).await.unwrap();
        assert_eq!(stub.calls()[0].body, Some(json!({})));

        let (c, stub) = client(Stub::new().json(202, json!({"id": "j1", "deployment": "d",
            "kind": "image-build", "status": "running", "started_at": 0})));
        c.start_build("demo", Some("v2")).await.unwrap();
        assert_eq!(stub.calls()[0].body, Some(json!({"ref": "v2"})));
    }

    #[test]
    fn a_metrics_query_serializes_only_what_was_asked_for() {
        assert_eq!(MetricsQuery::new().to_query_string(), "");
        assert_eq!(
            MetricsQuery::new().deployment("sb-1").to_query_string(),
            "?deployment=sb-1"
        );
        assert_eq!(
            MetricsQuery::new().summary(true).page(20, 10).to_query_string(),
            "?summary=true&limit=10&offset=20"
        );
        // offset 0 is the default and adds nothing.
        assert_eq!(
            MetricsQuery::new().page(0, 10).to_query_string(),
            "?limit=10"
        );
    }

    #[tokio::test]
    async fn minting_requires_a_name_before_anything_is_sent() {
        let (c, stub) = client(Stub::new());
        let e = c.mint_token(&NewToken::new("  ")).await.unwrap_err();
        assert!(matches!(e, Error::Invalid(_)));
        assert_eq!(stub.call_count(), 0);
    }

    #[tokio::test]
    async fn a_minted_token_carries_its_secret_exactly_once() {
        let (c, stub) = client(Stub::new().json(
            201,
            json!({"id": "abc", "name": "ci", "admin": "admin", "deployments": ["*"],
                   "created_at": 1, "token": "applb_abc_secret"}),
        ));
        let t = c
            .mint_token(&NewToken::new("ci").admin(AdminScope::Admin).fleet_wide())
            .await
            .unwrap();
        assert_eq!(t.token, "applb_abc_secret");
        assert_eq!(t.summary.id, "abc");
        assert_eq!(
            stub.calls()[0].body,
            Some(json!({"name": "ci", "admin": "admin", "deployments": ["*"]}))
        );
    }

    #[tokio::test]
    async fn a_scoped_mint_does_not_quietly_become_fleet_wide() {
        let (c, stub) = client(Stub::new().json(
            201,
            json!({"id": "abc", "name": "a", "admin": "none", "deployments": ["sb-1"],
                   "created_at": 1, "token": "applb_abc_s"}),
        ));
        c.mint_token(&NewToken::new("a").for_deployments(["sb-1"]))
            .await
            .unwrap();
        let body = stub.calls()[0].body.clone().unwrap();
        assert_eq!(body["deployments"], json!(["sb-1"]));
        assert_eq!(body["admin"], json!("none"), "the safe default, not admin");
    }

    #[tokio::test]
    async fn gates_are_discovered_from_what_an_anonymous_caller_is_refused() {
        // Not reachable through the stub (it builds its own client), so this
        // pins the shape of the decision instead.
        let refused = Error::from_response(
            401,
            "authentication required\n",
            "server",
            "",
            crate::error::Credential::None,
        );
        assert!(matches!(refused, Error::Unauthorized { .. }));
    }
}
