//! The data plane.
//!
//! One `ProxyHttp` serves every deployment, because pingora fixes its service
//! set at startup — `Server::run_forever(self)` consumes the server, so there is
//! no way to add a service per deployment at runtime. Dynamic registration
//! therefore lives in the `Registry` this reads on each request.
//!
//! Nothing here may block: VM boots happen only in the autoscaler. The one wait
//! is the cold-start `Notify`, which yields.

use crate::deployment::{Deployment, VmBackend};
use crate::registry::Registry;
use async_trait::async_trait;
use pingora_core::prelude::HttpPeer;
use pingora_core::{Error, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// How many upstreams one request may try before giving up.
const MAX_ATTEMPTS: usize = 3;

#[derive(Default)]
pub struct Ctx {
    /// The backend currently serving, if we've incremented its counter.
    backend: Option<Arc<VmBackend>>,
    deployment: Option<Arc<Deployment>>,
    /// Upstreams already tried; `upstream_peer` must not hand these back.
    failed: Vec<SocketAddr>,
    attempts: usize,
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

pub struct LbProxy {
    registry: Arc<Registry>,
}

impl LbProxy {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
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

async fn write_error(session: &mut Session, code: u16, message: &str) -> Result<()> {
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

#[async_trait]
impl ProxyHttp for LbProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Self::CTX {
        Ctx::default()
    }

    /// Resolve the deployment up front so an unroutable request is rejected
    /// before any upstream work.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let (host, path) = {
            let req = session.req_header();
            (request_host(req), req.uri.path().to_string())
        };

        match self.registry.route(host.as_deref(), &path) {
            Some(deployment) => {
                ctx.deployment = Some(deployment);
                Ok(false)
            }
            None => {
                tracing::debug!(?host, %path, "no deployment matches request");
                write_error(session, 404, "no deployment matches this request\n").await?;
                Ok(true) // response already written; stop proxying
            }
        }
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

        let backend = match deployment.select(&ctx.failed) {
            Some(b) => b,
            None => {
                // Nothing ready. If the deployment can still grow, hold the
                // request while a VM boots rather than failing the caller.
                match wait_for_capacity(&deployment, &ctx.failed).await {
                    Some(b) => b,
                    None => {
                        return Err(Error::explain(
                            ErrorType::ConnectProxyFailure,
                            "no healthy VM available for deployment",
                        ));
                    }
                }
            }
        };

        // Release any slot held from a previous attempt before taking a new one.
        ctx.release();
        backend.acquire();
        let addr = backend.addr;
        ctx.backend = Some(backend);

        // Plaintext: the guest IP is on a host-local tap network.
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
                addr = %b.addr,
                "upstream connect failed; marking unhealthy",
            );
            b.set_healthy(false);
            ctx.failed.push(b.addr);
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
        ctx.release();

        if let Some(err) = e {
            tracing::warn!(
                deployment = ctx.deployment.as_ref().map(|d| d.spec.id.as_str()),
                status = session.response_written().map(|r| r.status.as_u16()),
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
    exclude: &[SocketAddr],
) -> Option<Arc<VmBackend>> {
    if !deployment.can_grow() {
        return None;
    }

    // Count this request as demand *before* nudging. A waiting request holds no
    // in-flight slot (it has no backend yet), so without this the autoscaler
    // sees an idle deployment and leaves it at zero while we wait.
    let _waiter = deployment.track_waiter();

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
            return Some(b);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            tracing::warn!(
                deployment = %deployment.spec.id,
                "cold start timed out with no VM available",
            );
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
                path_prefix: None,
            }],
            vm: VmSpec {
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
            },
            scaling,
            health: HealthCheck::default(),
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
        assert!(wait_for_capacity(&d, &[]).await.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_for_capacity_times_out_when_no_vm_arrives() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            cold_start_timeout_secs: 1,
            ..Default::default()
        });
        assert!(wait_for_capacity(&d, &[]).await.is_none());
    }

    #[tokio::test]
    async fn wait_for_capacity_returns_a_vm_that_becomes_ready() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            cold_start_timeout_secs: 30,
            ..Default::default()
        });
        d.set_pending(vec![PendingVm {
            sandbox_id: "sb-1".into(),
            created_at: 0,
        }]);

        let d2 = d.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            d2.set_backends(vec![backend("10.0.0.1:80")]);
            d2.ready_signal.notify_waiters();
        });

        let got = wait_for_capacity(&d, &[]).await;
        assert_eq!(got.unwrap().addr, "10.0.0.1:80".parse().unwrap());
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
        assert!(wait_for_capacity(&d, &[]).await.is_some());
    }
}
