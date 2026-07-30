//! The data plane.
//!
//! One `ProxyHttp` serves every deployment, because pingora fixes its service
//! set at startup — `Server::run_forever(self)` consumes the server, so there is
//! no way to add a service per deployment at runtime. Dynamic registration
//! therefore lives in the `Registry` this reads on each request.
//!
//! Nothing here may block: VM boots happen only in the autoscaler. The one wait
//! is the cold-start `Notify`, which yields.

use crate::acme::ChallengeTable;
use crate::auth::{Authenticator, Decision, Identity, RequestInfo};
use crate::deployment::{Deployment, VmBackend};
use crate::metrics::Metrics;
use crate::obs::{Access, LogSink};
use crate::registry::Registry;
use async_trait::async_trait;
use pingora_core::prelude::HttpPeer;
use pingora_core::{Error, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many upstreams one request may try before giving up.
const MAX_ATTEMPTS: usize = 3;

#[derive(Default)]
pub struct Ctx {
    /// The backend currently serving, if we've incremented its counter.
    backend: Option<Arc<VmBackend>>,
    deployment: Option<Arc<Deployment>>,
    /// Upstreams already tried (by peer address); `upstream_peer` must not hand
    /// these back.
    failed: Vec<String>,
    attempts: usize,
    /// When the request entered the proxy, for latency. Set in `request_filter`
    /// so the measured span covers cold-start waits too, and `None` before then
    /// so a request rejected pre-routing simply isn't timed.
    started_at: Option<Instant>,
    /// Who the caller is, for a deployment behind a sign-in gate. Decided in
    /// `request_filter` and applied to the upstream request later, so the gate
    /// runs once per request rather than once per upstream attempt.
    identity: Option<Identity>,
}

impl Ctx {
    /// Release the in-flight slot exactly once.
    ///
    /// Taking the backend out makes this idempotent, which matters because
    /// `logging` runs on every path and must never double-release.
    fn release(&mut self) {
        if let Some(b) = self.backend.take() {
            b.release();
        }
    }
}

/// The URL prefix Let's Encrypt fetches to validate an HTTP-01 challenge.
const ACME_CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

pub struct LbProxy {
    registry: Arc<Registry>,
    metrics: Arc<Metrics>,
    /// Outstanding HTTP-01 challenge responses, published by the ACME manager.
    /// Empty (and the lookup therefore a single miss) whenever ACME is off.
    challenges: Arc<ChallengeTable>,
    /// Runs the sign-in gate for deployments that declare one. Inert for the
    /// rest: a deployment without `auth` never reaches it.
    auth: Arc<Authenticator>,
    /// Where the access log goes. `None` unless `APP_LB_OBS_URL` is configured,
    /// in which case `logging` does no extra work at all.
    access_log: Option<LogSink>,
}

impl LbProxy {
    pub fn new(
        registry: Arc<Registry>,
        metrics: Arc<Metrics>,
        challenges: Arc<ChallengeTable>,
        auth: Arc<Authenticator>,
        access_log: Option<LogSink>,
    ) -> Self {
        Self {
            registry,
            metrics,
            challenges,
            auth,
            access_log,
        }
    }
}

/// The request's target host.
///
/// HTTP/2 carries no `Host` header — clients send `:authority`, which pingora
/// surfaces on the URI — so both have to be checked or h2 traffic never routes.
/// The port is stripped so `demo.local:6188` matches a `demo.local` rule.
fn request_host(req: &RequestHeader) -> Option<String> {
    let raw = req
        .uri
        .authority()
        .map(|a| a.as_str().to_string())
        .or_else(|| {
            req.headers
                .get(http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })?;

    let host = raw.rsplit_once('@').map_or(raw.as_str(), |(_, h)| h);
    // Don't split IPv6 literals (`[::1]:80`) on the wrong colon.
    let host = if let Some(end) = host.find(']') {
        &host[..=end]
    } else {
        host.split(':').next().unwrap_or(host)
    };

    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Resolve a `host:port` (or `ip:port`) upstream to a concrete address, async so
/// a DNS lookup never blocks the proxy runtime. An `ip:port` literal resolves
/// without touching DNS; a hostname is resolved here and re-resolved on every
/// request, so DNS changes are picked up. `None` on failure or an empty result.
async fn resolve_peer(peer: &str) -> Option<SocketAddr> {
    tokio::net::lookup_host(peer).await.ok()?.next()
}

/// The key authorization to serve for `path`, if it names an outstanding
/// HTTP-01 challenge.
///
/// `None` for anything else — including a challenge-shaped path whose token is
/// unknown — so this can only ever intercept a request when a challenge for that
/// exact token is genuinely in flight. Everything else falls through to routing.
fn acme_challenge_response(challenges: &ChallengeTable, path: &str) -> Option<String> {
    challenges.get(path.strip_prefix(ACME_CHALLENGE_PREFIX)?)
}

async fn write_plain(session: &mut Session, code: u16, message: &str) -> Result<()> {
    let mut header = ResponseHeader::build(code, Some(2))?;
    header.insert_header(http::header::CONTENT_LENGTH, message.len().to_string())?;
    header.insert_header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(
            Some(bytes::Bytes::copy_from_slice(message.as_bytes())),
            true,
        )
        .await
}

/// Write a response the sign-in gate decided on: a redirect to the provider, the
/// end of a callback, or a refusal.
async fn write_gate_response(session: &mut Session, r: crate::auth::Response) -> Result<()> {
    let mut header = ResponseHeader::build(r.status, Some(6))?;
    header.insert_header(http::header::CONTENT_LENGTH, r.body.len().to_string())?;
    header.insert_header(http::header::CONTENT_TYPE, r.content_type)?;
    if let Some(location) = &r.location {
        header.insert_header(http::header::LOCATION, location)?;
    }
    // Nothing in the sign-in flow may be cached: a stored redirect would replay
    // a spent authorization code, and a stored 403 would outlive the allow-list
    // change that fixes it.
    header.insert_header(http::header::CACHE_CONTROL, "no-store")?;
    for cookie in &r.cookies {
        // Appended, not inserted: a callback sets the session cookie *and*
        // clears the flow cookie, and one `Set-Cookie` cannot carry both.
        header.append_header(http::header::SET_COOKIE, cookie)?;
    }
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session
        .write_response_body(
            Some(bytes::Bytes::copy_from_slice(r.body.as_bytes())),
            true,
        )
        .await
}

/// Collect what the gate needs out of the live request.
fn request_info<'a>(
    session: &'a Session,
    host: &'a str,
    path: &'a str,
    secure: bool,
) -> RequestInfo<'a> {
    let req = session.req_header();
    let cookies = req
        .headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_string)
        .collect();
    // A browser navigating asks for HTML; an API client asks for JSON or says
    // nothing. The difference decides whether an unauthenticated request is
    // redirected or refused with a 401 it can act on.
    let wants_html = req
        .headers
        .get(http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));

    RequestInfo {
        host,
        path,
        query: req.uri.query(),
        cookies,
        secure,
        wants_html,
    }
}

/// Whether the client reached app-lb over TLS.
///
/// The TLS digest is the truth for a connection app-lb terminated itself.
/// `x-forwarded-proto` is consulted only as a fallback, for a deployment behind
/// something that already terminated TLS — it is a client-settable header, and
/// the cost of believing a false one is a `Secure` cookie the browser then
/// refuses to send back, not a leaked session.
fn is_secure_request(session: &Session) -> bool {
    if session
        .digest()
        .is_some_and(|d| d.ssl_digest.is_some())
    {
        return true;
    }
    session
        .req_header()
        .headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

#[async_trait]
impl ProxyHttp for LbProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Self::CTX {
        Ctx::default()
    }

    /// Resolve the deployment up front so an unroutable request is rejected
    /// before any upstream work.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        // Start the latency clock before anything else so the span reflects the
        // whole time app-lb held the request, cold-start wait included.
        ctx.started_at = Some(Instant::now());

        let (host, path) = {
            let req = session.req_header();
            (request_host(req), req.uri.path().to_string())
        };

        // ACME HTTP-01 validation, answered before routing on purpose. The CA
        // sends an arbitrary `Host` and the hostname being certified often has
        // no matching deployment yet, so routing this would 404 and fail the
        // order. An unknown token falls through to normal routing, so this
        // cannot shadow a real route unless a challenge is genuinely
        // outstanding for that exact token.
        if let Some(key_authorization) = acme_challenge_response(&self.challenges, &path) {
            tracing::debug!(%path, "answering ACME http-01 challenge");
            write_plain(session, 200, &key_authorization).await?;
            return Ok(true);
        }

        let Some(deployment) = self.registry.route(host.as_deref(), &path) else {
            tracing::debug!(?host, %path, "no deployment matches request");
            write_plain(session, 404, "no deployment matches this request\n").await?;
            return Ok(true); // response already written; stop proxying
        };

        // The sign-in gate, for the deployments that declare one. It runs after
        // routing (the gate is the deployment's own configuration) and before
        // anything touches a backend — including the cold-start wait, so an
        // unauthenticated request never boots a VM.
        if let Some(gate) = deployment.spec.auth.clone() {
            // A gate needs a hostname to build its callback URL against; a
            // request routed purely by path prefix with no Host header cannot
            // complete a sign-in, and saying so beats redirecting to a URL the
            // provider will refuse.
            let Some(host) = host.as_deref() else {
                write_plain(
                    session,
                    400,
                    "this deployment requires sign-in, which needs a Host header\n",
                )
                .await?;
                ctx.deployment = Some(deployment);
                return Ok(true);
            };

            let secure = is_secure_request(session);
            let info = request_info(session, host, &path, secure);
            match self.auth.decide(&gate, &deployment.spec.id, &info).await {
                Decision::Allow(identity) => ctx.identity = *identity,
                Decision::Answered(response) => {
                    write_gate_response(session, response).await?;
                    // Recorded against the deployment: a wall of 302s or 403s
                    // here is exactly the symptom of a misconfigured gate, and
                    // it should show up in its metrics.
                    ctx.deployment = Some(deployment);
                    return Ok(true);
                }
            }
        }

        ctx.deployment = Some(deployment);
        Ok(false)
    }

    /// Attach the caller's identity for a gated deployment — and strip the same
    /// headers when there is none, so a client cannot present its own.
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let Some(gate) = ctx.deployment.as_ref().and_then(|d| d.spec.auth.as_ref()) else {
            return Ok(());
        };
        // Unconditional, before anything is set: on a gated deployment these
        // header names belong to app-lb, and an inbound one is either a mistake
        // or an attempt to impersonate a signed-in user.
        for name in crate::auth::IDENTITY_HEADERS {
            upstream.remove_header(name);
        }

        if let (true, Some(identity)) = (gate.forward_identity, ctx.identity.as_ref()) {
            upstream.insert_header("x-auth-request-email", crate::auth::header_safe(&identity.email))?;
            upstream.insert_header("x-auth-request-user", crate::auth::header_safe(&identity.subject))?;
            if let Some(name) = &identity.name {
                let name = crate::auth::header_safe(name);
                if !name.is_empty() {
                    upstream.insert_header("x-auth-request-name", name)?;
                }
            }
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let deployment = ctx
            .deployment
            .clone()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "no deployment in ctx"))?;

        ctx.attempts += 1;
        if ctx.attempts > MAX_ATTEMPTS {
            return Err(Error::explain(
                ErrorType::ConnectProxyFailure,
                "exhausted upstream retries",
            ));
        }

        // Pick a backend and resolve its address to a concrete `SocketAddr`. We
        // resolve here — with async DNS — rather than handing the `host:port`
        // string to `HttpPeer::new`, because that constructor resolves with a
        // *blocking* `to_socket_addrs().unwrap()` that would stall the runtime on
        // a static hostname and panic if it failed to resolve. A backend whose
        // address doesn't resolve is treated like a connect failure: marked
        // unhealthy and skipped, so a bad static upstream fails over to a good one
        // (and the autoscaler's health re-probe restores it once it resolves).
        let (backend, addr) = loop {
            let backend = match deployment.select(&ctx.failed) {
                Some(b) => b,
                None => {
                    // Nothing ready. If the deployment can still grow, hold the
                    // request while a VM boots rather than failing the caller.
                    // (A static deployment never grows, so this returns at once.)
                    match wait_for_capacity(&deployment, &ctx.failed, &self.metrics).await {
                        Some(b) => b,
                        None => {
                            return Err(Error::explain(
                                ErrorType::ConnectProxyFailure,
                                "no healthy backend available for deployment",
                            ));
                        }
                    }
                }
            };

            match resolve_peer(&backend.peer).await {
                Some(addr) => break (backend, addr),
                None => {
                    tracing::warn!(
                        peer = %backend.peer,
                        "upstream address did not resolve; marking unhealthy",
                    );
                    backend.set_healthy(false);
                    ctx.failed.push(backend.peer.clone());
                    // Loop: pick another backend (or give up when none remain).
                }
            }
        };

        // Release any slot held from a previous attempt before taking a new one.
        ctx.release();
        backend.acquire();
        ctx.backend = Some(backend);

        // Plaintext, in both modes: a managed VM's guest IP is on a host-local
        // tap network, and static proxy_pass upstreams are plaintext by design.
        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Makes it possible to see which VM served a request, which is how the
        // load-spreading and retry behaviour get verified.
        if let Some(b) = &ctx.backend {
            upstream_response.insert_header("x-vm-id", &b.sandbox_id)?;
        }
        Ok(())
    }

    /// A VM we couldn't reach is dead to us: drop it from selection, mark the
    /// error retryable, and let `upstream_peer` run again against another VM.
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<Error>,
    ) -> Box<Error> {
        if let Some(b) = &ctx.backend {
            tracing::warn!(
                sandbox = %b.sandbox_id,
                addr = %b.peer,
                "upstream connect failed; marking unhealthy",
            );
            b.set_healthy(false);
            ctx.failed.push(b.peer.clone());
        } else {
            tracing::warn!(peer = %peer, "upstream connect failed with no backend in ctx");
        }
        // This attempt never got off the ground, so give the slot back now
        // rather than holding it through the retry.
        ctx.release();

        if ctx.attempts < MAX_ATTEMPTS {
            e.set_retry(true);
        }
        e
    }

    /// Runs on every request, success or failure. If this ever misses a path,
    /// `in_flight` leaks upward and the deployment pins at max replicas.
    async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX) {
        // Which backend served, read before `release` — it takes the backend out
        // of the ctx, so anything that needs its identity must ask first.
        // `sandbox_id` is the sandbox for a managed VM and the `host:port` for a
        // static upstream, which is exactly how app-obs keys a backend. Skipped
        // when nothing is shipping, so an app-lb without app-obs allocates
        // nothing extra per request.
        let backend = match &self.access_log {
            Some(_) => ctx.backend.as_ref().map(|b| b.sandbox_id.clone()),
            None => None,
        };
        ctx.release();

        let status = session.response_written().map(|r| r.status.as_u16());

        // Record latency and outcome for any request that got as far as being
        // routed. A request rejected before routing (404, no deployment) has no
        // `started_at`/`deployment` and is intentionally left out of a
        // deployment's numbers.
        if let (Some(started), Some(deployment)) = (ctx.started_at, ctx.deployment.as_ref()) {
            self.metrics
                .record_request(&deployment.spec.id, status, started.elapsed());
        }

        // The access log app-obs stores. Unlike the metrics above it also carries
        // requests that matched no deployment, under the sink's own deployment id
        // (`_lb` unless `APP_LB_OBS_DEPLOYMENT` says otherwise) — a wall of 404s
        // for a hostname somebody expected to work is invisible in a
        // per-deployment view by construction. `started_at` is set first thing in
        // `request_filter`, so its absence means the request never got that far
        // and there is nothing to describe.
        if let (Some(sink), Some(started)) = (&self.access_log, ctx.started_at) {
            let req = session.req_header();
            let method = req.method.as_str().to_string();
            // The path alone, never the query: a sign-in callback carries the
            // OAuth `code` there, and a shared log store is the last place a
            // credential should come to rest.
            let path = req.uri.path().to_string();
            let host = request_host(req);
            sink.send_access(Access {
                deployment: ctx.deployment.as_ref().map(|d| d.spec.id.as_str()),
                backend,
                method: &method,
                path: &path,
                host: host.as_deref(),
                status,
                duration: started.elapsed(),
                bytes: session.body_bytes_sent(),
                // The address only. The ephemeral port identifies the
                // connection, not the caller.
                client: session.client_addr().map(|addr| match addr.as_inet() {
                    Some(inet) => inet.ip().to_string(),
                    None => addr.to_string(),
                }),
                error: e.map(|err| err.to_string()),
            });
        }

        if let Some(err) = e {
            tracing::warn!(
                deployment = ctx.deployment.as_ref().map(|d| d.spec.id.as_str()),
                status,
                error = %err,
                "request failed",
            );
        }
    }
}

/// Hold the request while the autoscaler boots a VM.
///
/// Returns as soon as a backend becomes available, or `None` on timeout or if
/// the deployment is already at `max_replicas` with nothing healthy (in which
/// case waiting cannot help).
async fn wait_for_capacity(
    deployment: &Arc<Deployment>,
    exclude: &[String],
    metrics: &Metrics,
) -> Option<Arc<VmBackend>> {
    if !deployment.can_grow() {
        // Not a cold-start wait — the pool is at max with nothing healthy, so
        // there is nothing to hold for. Left out of the cold-start tally.
        return None;
    }

    // Count this request as demand *before* nudging. A waiting request holds no
    // in-flight slot (it has no backend yet), so without this the autoscaler
    // sees an idle deployment and leaves it at zero while we wait.
    let _waiter = deployment.track_waiter();
    metrics.record_cold_start_wait(&deployment.spec.id);

    // Nudge the autoscaler: this deployment may be at zero and nothing else
    // would wake it.
    deployment.scale_signal.notify_one();

    let budget = Duration::from_secs(deployment.spec.scaling.cold_start_timeout_secs);
    let deadline = tokio::time::Instant::now() + budget;
    tracing::info!(
        deployment = %deployment.spec.id,
        timeout_secs = deployment.spec.scaling.cold_start_timeout_secs,
        "holding request for cold start",
    );

    loop {
        // Subscribe *before* re-checking so a VM that becomes ready between the
        // check and the wait can't be missed.
        let notified = deployment.ready_signal.notified();
        if let Some(b) = deployment.select(exclude) {
            metrics.record_cold_start_hit(&deployment.spec.id);
            return Some(b);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            tracing::warn!(
                deployment = %deployment.spec.id,
                "cold start timed out with no VM available",
            );
            metrics.record_cold_start_timeout(&deployment.spec.id);
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeploymentSpec, HealthCheck, RouteRule, ScalingPolicy, VmSpec};
    use crate::deployment::PendingVm;
    use heyo_sdk::SandboxDriver;

    #[test]
    fn acme_challenge_answers_only_outstanding_tokens() {
        let challenges = ChallengeTable::new();
        challenges.publish("live-token".into(), "live-token.thumbprint".into());

        assert_eq!(
            acme_challenge_response(&challenges, "/.well-known/acme-challenge/live-token")
                .as_deref(),
            Some("live-token.thumbprint"),
        );

        // An unknown token must fall through to routing rather than 404 from
        // here — otherwise the branch would shadow a real route.
        assert_eq!(
            acme_challenge_response(&challenges, "/.well-known/acme-challenge/stale-token"),
            None,
        );
        assert_eq!(acme_challenge_response(&challenges, "/.well-known/acme-challenge/"), None);
        assert_eq!(acme_challenge_response(&challenges, "/live-token"), None);
        assert_eq!(acme_challenge_response(&challenges, "/"), None);
    }

    #[test]
    fn acme_challenge_is_inert_when_acme_is_disabled() {
        // The table is empty whenever ACME is off, so no request can be
        // intercepted — the branch costs one map lookup and nothing else.
        let challenges = ChallengeTable::new();
        assert_eq!(
            acme_challenge_response(&challenges, "/.well-known/acme-challenge/anything"),
            None,
        );
    }

    fn header(host: Option<&str>, path: &str) -> RequestHeader {
        let mut h = RequestHeader::build("GET", path.as_bytes(), None).unwrap();
        if let Some(v) = host {
            h.insert_header("host", v).unwrap();
        }
        h
    }

    #[test]
    fn host_header_is_lowercased_and_port_stripped() {
        assert_eq!(
            request_host(&header(Some("Demo.Local:6188"), "/")).as_deref(),
            Some("demo.local")
        );
        assert_eq!(
            request_host(&header(Some("demo.local"), "/")).as_deref(),
            Some("demo.local")
        );
    }

    #[test]
    fn missing_host_is_none() {
        assert_eq!(request_host(&header(None, "/")), None);
    }

    #[tokio::test]
    async fn resolve_peer_handles_literals_and_bad_addresses() {
        // An ip:port literal resolves without DNS.
        assert_eq!(
            resolve_peer("127.0.0.1:8080").await,
            Some("127.0.0.1:8080".parse().unwrap()),
        );
        // localhost resolves to a loopback address.
        let local = resolve_peer("localhost:8080").await;
        assert!(local.is_some_and(|a| a.ip().is_loopback()), "got {local:?}");
        // A malformed / unresolvable address is None, not a panic — this is what
        // keeps a bad static upstream from taking down the proxy runtime.
        assert_eq!(resolve_peer("no-port").await, None);
        assert!(
            resolve_peer("definitely-not-a-real-host.invalid:80").await.is_none()
        );
    }

    #[test]
    fn ipv6_literal_host_survives_port_stripping() {
        assert_eq!(
            request_host(&header(Some("[::1]:8080"), "/")).as_deref(),
            Some("[::1]")
        );
    }

    /// HTTP/2 sends no Host header — the h2 crate parses `:authority` into the
    /// URI's authority, which is what pingora hands us. Without this branch, all
    /// h2 traffic would fail to route.
    #[test]
    fn authority_is_used_when_present() {
        let mut h = RequestHeader::build("GET", b"/x", None).unwrap();
        h.set_uri("http://demo.local:6188/x".parse().unwrap());
        assert_eq!(request_host(&h).as_deref(), Some("demo.local"));
    }

    /// An h2 request with both: `:authority` is authoritative per RFC 9113.
    #[test]
    fn authority_wins_over_a_conflicting_host_header() {
        let mut h = header(Some("stale.local"), "/x");
        h.set_uri("http://demo.local/x".parse().unwrap());
        assert_eq!(request_host(&h).as_deref(), Some("demo.local"));
    }

    #[test]
    fn userinfo_is_stripped_from_authority() {
        let mut h = RequestHeader::build("GET", b"/x", None).unwrap();
        h.set_uri("http://user:pass@demo.local:6188/x".parse().unwrap());
        assert_eq!(request_host(&h).as_deref(), Some("demo.local"));
    }

    fn deployment(scaling: ScalingPolicy) -> Arc<Deployment> {
        Arc::new(Deployment::new(DeploymentSpec {
            id: "demo".into(),
            routes: vec![RouteRule {
                host: Some("demo.local".into()),
                host_suffix: None,
                path_prefix: None,
            }],
            vm: Some(VmSpec {
                driver: SandboxDriver::Firecracker,
                image: None,
                port: 8080,
                start_command: None,
                size_class: None,
                disk_size_gb: None,
                working_directory: None,
                env_vars: None,
                setup_hooks: None,
                open_ports: vec![],
                ttl_seconds: 3600,
            }),
            scaling,
            health: HealthCheck::default(),
            upstreams: vec![],
            build: None,
            artifact: None,
            update: None,
            auth: None,
        }))
    }

    fn backend(addr: &str) -> Arc<VmBackend> {
        Arc::new(VmBackend::new("sb-1".into(), addr.parse().unwrap()))
    }

    #[test]
    fn ctx_release_is_idempotent() {
        let b = backend("10.0.0.1:80");
        b.acquire();
        assert_eq!(b.in_flight(), 1);

        let mut ctx = Ctx {
            backend: Some(b.clone()),
            ..Default::default()
        };
        ctx.release();
        assert_eq!(b.in_flight(), 0);
        // A second release (e.g. fail_to_connect then logging) must not
        // decrement a slot it no longer owns.
        ctx.release();
        assert_eq!(b.in_flight(), 0);
    }

    #[tokio::test]
    async fn wait_for_capacity_gives_up_when_at_max_replicas() {
        let d = deployment(ScalingPolicy {
            max_replicas: 1,
            cold_start_timeout_secs: 30,
            ..Default::default()
        });
        // At max with an unhealthy VM: booting more is impossible, so this must
        // fail fast rather than burn the cold-start budget.
        let b = backend("10.0.0.1:80");
        b.set_healthy(false);
        d.set_backends(vec![b]);

        let started = std::time::Instant::now();
        assert!(wait_for_capacity(&d, &[], &Metrics::new()).await.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_for_capacity_times_out_when_no_vm_arrives() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            cold_start_timeout_secs: 1,
            ..Default::default()
        });
        assert!(wait_for_capacity(&d, &[], &Metrics::new()).await.is_none());
    }

    #[tokio::test]
    async fn wait_for_capacity_returns_a_vm_that_becomes_ready() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            cold_start_timeout_secs: 30,
            ..Default::default()
        });
        d.set_pending(vec![PendingVm::new("sb-1".into())]);

        let d2 = d.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            d2.set_backends(vec![backend("10.0.0.1:80")]);
            d2.ready_signal.notify_waiters();
        });

        let got = wait_for_capacity(&d, &[], &Metrics::new()).await;
        assert_eq!(got.unwrap().peer, "10.0.0.1:80");
    }

    /// The pool can fill between the availability check and the wait; the
    /// re-check inside the loop must catch that rather than hanging.
    #[tokio::test]
    async fn wait_for_capacity_sees_a_vm_that_was_already_ready() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            cold_start_timeout_secs: 30,
            ..Default::default()
        });
        d.set_backends(vec![backend("10.0.0.1:80")]);
        assert!(wait_for_capacity(&d, &[], &Metrics::new()).await.is_some());
    }
}
