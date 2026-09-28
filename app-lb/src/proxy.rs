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
use crate::auth::Authenticator;
#[cfg(test)]
use crate::deployment::{Deployment, VmBackend};
use crate::guard::Guard;
use crate::metrics::Metrics;
use crate::obs::{Access, LogSink};
use crate::siem::SecuritySink;
use crate::registry::Registry;
use async_trait::async_trait;
use pingora_core::prelude::HttpPeer;
use pingora_core::protocols::TcpKeepalive;
use pingora_core::{Error, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Ctx {
    request: crate::request_control::RequestState,
    /// When the request entered the proxy, for latency. Set in `request_filter`
    /// so the measured span covers cold-start waits too, and `None` before then
    /// so a request rejected pre-routing simply isn't timed.
    started_at: Option<Instant>,
}

pub struct LbProxy {
    control: Arc<crate::request_control::RequestControl>,
    metrics: Arc<Metrics>,
    /// Where the access log goes. `None` unless `APP_LB_OBS_URL` is configured,
    /// in which case `logging` does no extra work at all.
    access_log: Option<LogSink>,
    /// Where the same requests go to be analysed for attacks. Independent of
    /// `access_log` on purpose: the SIEM is on unless `APP_LB_SIEM=0`, and must
    /// not inherit "off whenever no log collector is configured".
    security: Option<SecuritySink>,
}

impl LbProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<Registry>,
        metrics: Arc<Metrics>,
        challenges: Arc<ChallengeTable>,
        auth: Arc<Authenticator>,
        access_log: Option<LogSink>,
        security: Option<SecuritySink>,
        guard: Arc<Guard>,
        feed: Arc<crate::feed::Feed>,
        auth_providers: Arc<crate::auth_providers::AuthProviderStore>,
        secrets: Arc<crate::secrets::SecretStore>,
    ) -> Self {
        Self {
            control: Arc::new(crate::request_control::RequestControl::new(
                registry, metrics.clone(), challenges, auth, guard, feed, auth_providers, secrets,
            )),
            metrics,
            access_log,
            security,
        }
    }
}

/// The request's target host.
///
/// HTTP/2 carries no `Host` header — clients send `:authority`, which pingora
/// surfaces on the URI — so both have to be checked or h2 traffic never routes.
/// The port is stripped so `demo.local:6188` matches a `demo.local` rule.
fn request_host(req: &RequestHeader) -> Option<String> {
    crate::request_control::RequestHead {
        method: req.method.clone(),
        uri: req.uri.clone(),
        headers: req.headers.clone(),
        peer: None,
        tls_terminated: false,
    }
    .host()
}

/// Keepalive on every upstream connection, so a backend that vanishes without
/// a word is noticed. A destroyed Firecracker VM takes its tap device with it:
/// no RST ever arrives, and a request waiting on a response has nothing
/// unacknowledged in flight, so without probes the socket sits in ESTABLISHED
/// forever — holding the caller, the backend's `in_flight` slot, and anything
/// queued behind that caller. Probes are answered by a live peer's kernel, so a
/// slow response or a long-lived stream is unaffected; a dead peer is dropped
/// within about a minute of its last word.
const UPSTREAM_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const UPSTREAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const UPSTREAM_KEEPALIVE_COUNT: usize = 3;
/// `TCP_USER_TIMEOUT`: the same bound for data that was sent and never
/// acknowledged — a pooled keep-alive connection reused after its VM died.
#[cfg(target_os = "linux")]
const UPSTREAM_USER_TIMEOUT: Duration = Duration::from_secs(60);

fn http_peer(selected: crate::request_control::Peer) -> HttpPeer {
    let mut peer = HttpPeer::new(selected.address, selected.tls, selected.sni);
    peer.options.tcp_keepalive = Some(TcpKeepalive {
        idle: UPSTREAM_KEEPALIVE_IDLE,
        interval: UPSTREAM_KEEPALIVE_INTERVAL,
        count: UPSTREAM_KEEPALIVE_COUNT,
        #[cfg(target_os = "linux")]
        user_timeout: UPSTREAM_USER_TIMEOUT,
    });
    peer
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

async fn write_control_response(
    session: &mut Session,
    response: crate::request_control::ResponseData,
) -> Result<()> {
    let mut header = ResponseHeader::build(response.status, Some(8))?;
    header.insert_header(http::header::CONTENT_LENGTH, response.body.len().to_string())?;
    header.insert_header(http::header::CONTENT_TYPE, response.content_type)?;
    if let Some(cache) = response.cache_control {
        header.insert_header(http::header::CACHE_CONTROL, cache)?;
    }
    for (name, value) in response.headers {
        header.append_header(name, value)?;
    }
    session.write_response_header(Box::new(header), false).await?;
    session.write_response_body(Some(bytes::Bytes::from(response.body)), true).await
}

/// The newest events an exposed or admin-served feed returns. Half the ring:
/// a reader wants "recent", and the full ring is the debugging view.
pub use crate::request_control::FEED_PAGE;

/// Answer a request routed to a `site` deployment, out of its directory.
///
/// Everything about *which* file is `site::resolve`'s decision; this is the HTTP
/// around it — status, headers, conditional requests, ranges, and getting the
/// bytes onto the wire without reading a large file into memory.
async fn serve_site(session: &mut Session, spec: &crate::config::SiteSpec, path: &str) -> Result<()> {
    use crate::site::{self, Resolved};

    let (file, status) = match site::resolve(spec, path) {
        Resolved::File(f) => (f, 200),
        Resolved::Redirect(to) => {
            let mut header = ResponseHeader::build(301, Some(3))?;
            header.insert_header(http::header::LOCATION, &to)?;
            header.insert_header(http::header::CONTENT_LENGTH, "0")?;
            session.write_response_header(Box::new(header), true).await?;
            return Ok(());
        }
        Resolved::NotFound(Some(page)) => (page, 404),
        Resolved::NotFound(None) => {
            return write_plain(session, 404, "not found\n").await;
        }
    };

    let mut handle = match tokio::fs::File::open(&file).await {
        Ok(f) => f,
        Err(e) => {
            // Resolved a moment ago, so this is a deploy landing mid-request or
            // a permissions problem — either way not the client's fault.
            tracing::warn!(path = %file.display(), error = %e, "could not open a site file");
            return write_plain(session, 404, "not found\n").await;
        }
    };
    let Ok(meta) = handle.metadata().await else {
        return write_plain(session, 404, "not found\n").await;
    };
    let len = meta.len();
    let etag = site::etag(&meta);

    // A cached client gets a 304 and no body. Only for a 200 — a 404 page is
    // not a representation of the requested URL, so it must not be revalidated
    // as one.
    if status == 200
        && session
            .req_header()
            .headers
            .get(http::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| site::etag_matches(v, &etag))
    {
        let mut header = ResponseHeader::build(304, Some(3))?;
        header.insert_header(http::header::ETAG, &etag)?;
        header.insert_header(http::header::CACHE_CONTROL, &spec.cache_control)?;
        session.write_response_header(Box::new(header), true).await?;
        return Ok(());
    }

    // Ranges, so a paused download or a seeking `<video>` works. Only on a 200:
    // a range of an error page is meaningless.
    let mut offset = 0u64;
    let mut count = len;
    let mut status = status;
    if status == 200 {
        let range = session
            .req_header()
            .headers
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok());
        if let Some(raw) = range {
            match site::parse_range(raw, len) {
                Ok(Some((first, last))) => {
                    offset = first;
                    count = last - first + 1;
                    status = 206;
                }
                Ok(None) => {} // unsupported form; the whole file is a valid answer
                Err(()) => {
                    let mut header = ResponseHeader::build(416, Some(3))?;
                    header.insert_header(http::header::CONTENT_RANGE, format!("bytes */{len}"))?;
                    header.insert_header(http::header::CONTENT_LENGTH, "0")?;
                    session.write_response_header(Box::new(header), true).await?;
                    return Ok(());
                }
            }
        }
    }

    let mut header = ResponseHeader::build(status, Some(6))?;
    header.insert_header(http::header::CONTENT_TYPE, site::content_type(&file))?;
    header.insert_header(http::header::CONTENT_LENGTH, count.to_string())?;
    header.insert_header(http::header::ETAG, &etag)?;
    header.insert_header(http::header::CACHE_CONTROL, &spec.cache_control)?;
    // Advertised whether or not this request used one, so a client knows it can
    // resume rather than restarting a large download.
    header.insert_header(http::header::ACCEPT_RANGES, "bytes")?;
    if status == 206 {
        let last = offset + count - 1;
        header.insert_header(
            http::header::CONTENT_RANGE,
            format!("bytes {offset}-{last}/{len}"),
        )?;
    }
    let head_only = session.req_header().method == http::Method::HEAD;
    session
        .write_response_header(Box::new(header), head_only)
        .await?;
    if head_only {
        return Ok(());
    }

    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        if handle.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return Ok(()); // header is already out; nothing better to say
        }
    }

    // Streamed in chunks rather than read whole: a fleet of sites serving large
    // assets would otherwise hold every concurrent response's full size in
    // memory at once.
    use tokio::io::AsyncReadExt;
    let mut remaining = count;
    let mut buf = vec![0u8; site::STREAM_CHUNK.min(count.max(1) as usize)];
    while remaining > 0 {
        let want = (buf.len() as u64).min(remaining) as usize;
        let read = match handle.read(&mut buf[..want]).await {
            Ok(0) => break, // truncated under us; stop rather than pad
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "site file read failed mid-response");
                break;
            }
        };
        remaining -= read as u64;
        session
            .write_response_body(Some(bytes::Bytes::copy_from_slice(&buf[..read])), remaining == 0)
            .await?;
    }
    Ok(())
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

        let req = session.req_header();
        let head = crate::request_control::RequestHead {
            method: req.method.clone(),
            uri: req.uri.clone(),
            headers: req.headers.clone(),
            peer: session.client_addr().and_then(|a| a.as_inet().map(|a| *a)),
            tls_terminated: session.digest().is_some_and(|d| d.ssl_digest.is_some()),
        };
        let mut decision = self.control.decide(&head, &mut ctx.request).await;
        if matches!(decision, crate::request_control::RequestDecision::ReadLoginBody) {
            let mut body = Vec::new();
            while let Some(chunk) = session.read_request_body().await? {
                if body.len() + chunk.len() > crate::request_control::MAX_LOGIN_BODY {
                    body.resize(crate::request_control::MAX_LOGIN_BODY + 1, 0);
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            decision = self.control.continue_login(&mut ctx.request, &body).await;
        }
        // Pingora tracks header names separately; preserve its bookkeeping in
        // this transport adapter even though authority evaluated owned headers.
        for name in crate::gateway::HEADERS.into_iter().chain([
            crate::regional::GENERATION, crate::regional::ENVIRONMENT,
            crate::regional::PROBE, crate::regional::ACTIVE_PROBE,
        ]) {
            session.req_header_mut().remove_header(name);
        }
        match decision {
            crate::request_control::RequestDecision::Respond(response) => {
                write_control_response(session, response).await?;
                return Ok(true);
            }
            crate::request_control::RequestDecision::ServeSite { spec, path } => {
                serve_site(session, &spec, &path).await?;
                return Ok(true);
            }
            crate::request_control::RequestDecision::Proxy => return Ok(false),
            crate::request_control::RequestDecision::ReadLoginBody => unreachable!("login continuation returns a terminal decision"),
        }
    }

    /// Attach the caller's identity for a gated deployment — and strip the same
    /// headers when there is none, so a client cannot present its own.
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        for modification in ctx.request.forwarding_modifications(&upstream.uri).map_err(|error| {
            Error::explain(ErrorType::InternalError, error)
        })? {
            match modification {
                crate::request_control::HeaderModification::Remove(name) => {
                    upstream.remove_header(&name);
                }
                crate::request_control::HeaderModification::Set(name, value) => {
                    upstream.insert_header(name, value)?;
                }
                crate::request_control::HeaderModification::RewriteUri(rewritten) => {
                    let mut parts = upstream.uri.clone().into_parts();
                    parts.path_and_query = Some(rewritten.parse().map_err(|error| Error::explain(ErrorType::InternalError, format!("failed to rewrite upstream path: {error}")))?);
                    upstream.set_uri(http::Uri::from_parts(parts).map_err(|error| Error::explain(ErrorType::InternalError, format!("failed to build rewritten upstream URI: {error}")))?);
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
        let selected = self.control.next_peer(&mut ctx.request)
            .await
            .map_err(|error| Error::explain(
                match error.kind {
                    crate::request_control::ErrorKind::Internal => ErrorType::InternalError,
                    crate::request_control::ErrorKind::Connect => ErrorType::ConnectProxyFailure,
                },
                error.message,
            ))?;
        Ok(Box::new(http_peer(selected)))
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Makes it possible to see which VM served a request, which is how the
        // load-spreading and retry behaviour get verified.
        if let Some(b) = ctx.request.backend() {
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
        if let Some(b) = ctx.request.connection_failed() {
            tracing::warn!(
                sandbox = %b.sandbox_id,
                addr = %b.peer,
                "upstream connect failed; marking unhealthy",
            );
        } else {
            tracing::warn!(peer = %peer, "upstream connect failed with no backend in ctx");
        }
        // This attempt never got off the ground, so give the slot back now
        // rather than holding it through the retry.
        if ctx.request.retry_allowed() {
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
        // Either consumer needs it: with `APP_LB_OBS_ACCESS_LOG=0` and the SIEM
        // on, checking only `access_log` here would leave every alert without a
        // backend, which reads as a bug in the SIEM rather than as this line.
        let observing = self.access_log.is_some() || self.security.is_some();
        let backend = match observing {
            true => ctx.request.backend().map(|b| b.sandbox_id.clone()),
            false => None,
        };
        ctx.request.complete();

        let status = session.response_written().map(|r| r.status.as_u16());

        // Record latency and outcome for any request that got as far as being
        // routed. A request rejected before routing (404, no deployment) has no
        // `started_at`/`deployment` and is intentionally left out of a
        // deployment's numbers.
        if let (Some(started), Some(deployment)) = (ctx.started_at, ctx.request.deployment()) {
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
        if let (true, Some(started)) = (observing, ctx.started_at) {
            let req = session.req_header();
            let method = req.method.as_str().to_string();
            // The path alone, never the query: a sign-in callback carries the
            // OAuth `code` there, and a shared log store is the last place a
            // credential should come to rest.
            //
            // The SIEM is the one exception, and a deliberately narrow one: it
            // is handed the query as a *separate argument* below, matches attack
            // signatures against the parameter values, and drops it. On a hit the
            // alert records the parameter name only, never the value — so the
            // `code` above still cannot reach a log store, while `?id=1' OR '1'='1`
            // stops being invisible. `APP_LB_SIEM_SCAN_QUERY=0` turns it off.
            let path = req.uri.path().to_string();
            let query = req.uri.query().map(str::to_string);
            let host = request_host(req);
            let access = Access {
                deployment: ctx.request.deployment().map(|d| d.spec.id.as_str()),
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
            };

            // SIEM first: it borrows, and `send_access` moves. `Access` is not
            // `Clone`, so reordering these two stops compiling rather than
            // silently dropping the analysis.
            if let Some(siem) = &self.security {
                siem.observe_access(&access, query.as_deref());
            }
            if let Some(sink) = &self.access_log {
                sink.send_access(access);
            }
        }

        if let Some(err) = e {
            tracing::warn!(
                deployment = ctx.request.deployment().map(|d| d.spec.id.as_str()),
                status,
                error = %err,
                "request failed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeploymentSpec, HealthCheck, RouteRule, ScalingPolicy, VmSpec};
    use crate::deployment::PendingVm;
    use crate::config::Driver;

    #[test]
    fn https_backend_builds_a_tls_peer_with_url_hostname_as_sni() {
        let backend = VmBackend::for_upstream("https://ci.eu1.heyo.work:443".into());
        let peer = http_peer(crate::request_control::Peer {
            address: "127.0.0.1:443".parse().unwrap(), tls: backend.tls, sni: backend.sni.clone(),
        });
        assert!(peer.is_tls());
        assert_eq!(peer.sni, "ci.eu1.heyo.work");
        assert!(peer.options.verify_cert);
        assert!(peer.options.verify_hostname);
    }

    #[test]
    fn upstream_peers_probe_for_a_vanished_backend() {
        let backend = backend("172.25.128.50:8080");
        let peer = http_peer(crate::request_control::Peer {
            address: "172.25.128.50:8080".parse().unwrap(), tls: backend.tls, sni: backend.sni.clone(),
        });
        let ka = peer
            .options
            .tcp_keepalive
            .as_ref()
            .expect("keepalive is set on every upstream");
        let detect = ka.idle + ka.interval * ka.count as u32;
        assert!(
            detect <= Duration::from_secs(90),
            "a dead VM should be dropped well inside cold-start budgets, got {detect:?}"
        );
        #[cfg(target_os = "linux")]
        assert!(
            !ka.user_timeout.is_zero(),
            "unacknowledged writes to a dead VM must time out too"
        );
    }

    fn deployment(scaling: ScalingPolicy) -> Arc<Deployment> {
        Arc::new(Deployment::new(DeploymentSpec {
            ingress: None,
            account_id: None,
            user_id: None,
            namespace: "default".into(),
            feed: None,
            id: "demo".into(),
            routes: vec![RouteRule {
                host: Some("demo.local".into()),
                host_suffix: None,
                path_prefix: None,
                strip_prefix: false,
            }],
            vm: Some(VmSpec {
                correlated_creates: false,
                env_from: vec![],
                workspace_archive: None,
                image_download_url: None,
                image_size_bytes: None,
                image_sha256: None,
                driver: Driver::Firecracker,
                image: None,
                port: 8080,
                start_command: None,
                size_class: None,
                disk_size_gb: None,
                working_directory: None,
                env_vars: None,
                setup_hooks: None,
                open_ports: vec![],
                mounts: vec![],
                workspace: None,
                ttl_seconds: 3600,
            }),
            scaling,
            maintenance: false,
            health: HealthCheck::default(),
            upstreams: vec![],
            discovery: None,
            gateway: None,
            build: None,
            artifact: None,
            site: None,
            update: None,
            auth: None,
        }))
    }

    fn backend(addr: &str) -> Arc<VmBackend> {
        Arc::new(VmBackend::new("sb-1".into(), addr.parse().unwrap()))
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
        assert!(crate::request_control::wait_for_capacity(&d, &[], &Metrics::new(), &crate::feed::Feed::new()).await.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_for_capacity_times_out_when_no_vm_arrives() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            cold_start_timeout_secs: 1,
            ..Default::default()
        });
        assert!(crate::request_control::wait_for_capacity(&d, &[], &Metrics::new(), &crate::feed::Feed::new()).await.is_none());
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

        let got = crate::request_control::wait_for_capacity(&d, &[], &Metrics::new(), &crate::feed::Feed::new()).await;
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
        assert!(crate::request_control::wait_for_capacity(&d, &[], &Metrics::new(), &crate::feed::Feed::new()).await.is_some());
    }
}
