//! Manager-owned request admission, backend reservation, and retry state.
//!
//! This module deliberately knows nothing about Pingora sessions or peers. The
//! proxy adapter supplies HTTP-derived admission inputs, then translates the
//! selected [`Peer`] into its transport's peer type.

use crate::deployment::{Deployment, VmBackend};
use crate::feed::Feed;
use crate::metrics::Metrics;
use crate::regional::Assignment;
use crate::registry::Registry;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Internal,
    Connect,
}

#[derive(Debug)]
pub struct RequestError {
    pub kind: ErrorKind,
    pub message: &'static str,
}

impl RequestError {
    fn internal(message: &'static str) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message,
        }
    }
    fn connect(message: &'static str) -> Self {
        Self {
            kind: ErrorKind::Connect,
            message,
        }
    }
}

/// Transport-independent result of backend selection and reservation.
pub struct Peer {
    pub address: SocketAddr,
    pub tls: bool,
    pub sni: String,
}

#[derive(Default)]
pub struct RequestState {
    deployment: Option<Arc<Deployment>>,
    backend: Option<Arc<VmBackend>>,
    failed: Vec<String>,
    attempts: usize,
    regional_assignment: Option<Assignment>,
}

impl RequestState {
    pub fn deployment(&self) -> Option<&Arc<Deployment>> {
        self.deployment.as_ref()
    }
    pub fn set_deployment(&mut self, deployment: Arc<Deployment>) {
        self.deployment = Some(deployment);
    }
    pub fn backend(&self) -> Option<&Arc<VmBackend>> {
        self.backend.as_ref()
    }
    pub fn regional_assignment(&self) -> Option<&Assignment> {
        self.regional_assignment.as_ref()
    }
    /// Apply the deployment's backend admission policy once. The adapter owns
    /// header parsing/removal; this state owns the resulting regional
    /// assignment so it cannot be replayed against another backend generation.
    pub fn admit(
        &mut self,
        deployment: &Deployment,
        headers: &http::HeaderMap,
        secrets: &crate::secrets::SecretStore,
    ) -> Result<Option<String>, u16> {
        if let Some(router) = &deployment.regional {
            let discovery = deployment
                .spec
                .discovery
                .as_ref()
                .expect("regional discovery configured");
            let assignment = router.admit(
                discovery.regional.as_ref().unwrap(),
                &discovery.service_id,
                discovery.region.as_deref().unwrap(),
                headers,
                secrets,
            )?;
            self.regional_assignment = Some(assignment);
            Ok(None)
        } else if [crate::regional::GENERATION, crate::regional::ENVIRONMENT]
            .iter()
            .any(|header| headers.contains_key(*header))
        {
            Err(403)
        } else {
            crate::gateway::admit(deployment.spec.gateway.as_ref(), headers, secrets)
        }
    }
    #[cfg(test)]
    fn set_reserved_backend(&mut self, backend: Arc<VmBackend>) {
        self.backend = Some(backend);
    }

    /// Release the in-flight slot exactly once.
    pub fn release(&mut self) {
        if let Some(backend) = self.backend.take() {
            backend.release();
        }
    }

    /// End the whole request, including its generation-pinned assignment.
    /// An attempt failure releases only the backend; completion releases both.
    pub fn complete(&mut self) {
        self.release();
        self.regional_assignment.take();
    }

    /// Record a failed connection and return its backend for diagnostics.
    pub fn connection_failed(&mut self) -> Option<Arc<VmBackend>> {
        let backend = self.backend.as_ref().cloned();
        if let Some(backend) = &backend {
            backend.set_healthy(false);
            self.failed.push(backend.peer.clone());
        }
        self.release();
        backend
    }

    pub fn retry_allowed(&self) -> bool {
        self.regional_assignment.is_none() && self.attempts < MAX_ATTEMPTS
    }

    /// Select and reserve the next peer. Reservation happens before DNS awaits,
    /// preserving the cordon/drain admission boundary.
    pub async fn next_peer(
        &mut self,
        registry: &Registry,
        metrics: &Metrics,
        feed: &Feed,
    ) -> Result<Peer, RequestError> {
        let mut deployment = self
            .deployment
            .clone()
            .ok_or_else(|| RequestError::internal("no deployment in request state"))?;
        self.attempts += 1;
        if self.attempts > MAX_ATTEMPTS {
            return Err(RequestError::connect("exhausted upstream retries"));
        }
        self.release();

        if let Some(assignment) = &self.regional_assignment {
            if self.attempts != 1 || !assignment.backend.try_acquire() {
                return Err(RequestError::connect(
                    "regional assignment unavailable; replay forbidden",
                ));
            }
            let backend = assignment.backend.clone();
            self.backend = Some(backend.clone());
            let address = resolve_peer(&backend.address).await.ok_or_else(|| {
                RequestError::connect("regional assignment address did not resolve")
            })?;
            return Ok(peer(&backend, address));
        }

        let address = loop {
            let observed = deployment.clone();
            let changed = observed.ready_signal.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(current) = registry.get(&deployment.spec.id) {
                if !Arc::ptr_eq(&current, &deployment) {
                    check_flat_admission_refresh(&deployment, &current)?;
                    drop(changed);
                    deployment = current;
                    self.deployment = Some(deployment.clone());
                    continue;
                }
            }
            let backend = match deployment.select(&self.failed) {
                Some(backend) => backend,
                None => match tokio::select! {
                    result = wait_for_capacity(&deployment, &self.failed, metrics, feed) => result,
                    _ = &mut changed => continue,
                } {
                    Some(backend) => backend,
                    None => {
                        return Err(RequestError::connect(
                            "no healthy backend available for deployment",
                        ));
                    }
                },
            };
            if !backend.try_acquire() {
                self.failed.push(backend.peer.clone());
                continue;
            }
            self.backend = Some(backend.clone());
            match resolve_peer(&backend.address).await {
                Some(address) => break address,
                None => {
                    tracing::warn!(peer = %backend.peer, "upstream address did not resolve; marking unhealthy");
                    backend.set_healthy(false);
                    self.failed.push(backend.peer.clone());
                    self.release();
                }
            }
        };
        Ok(peer(
            self.backend
                .as_ref()
                .expect("selected backend remains in request state"),
            address,
        ))
    }
}

impl Drop for RequestState {
    fn drop(&mut self) {
        self.release();
    }
}

fn peer(backend: &VmBackend, address: SocketAddr) -> Peer {
    Peer {
        address,
        tls: backend.tls,
        sni: backend.sni.clone(),
    }
}

fn check_flat_admission_refresh(
    previous: &Deployment,
    current: &Deployment,
) -> Result<(), RequestError> {
    if current.spec.gateway != previous.spec.gateway
        || current.regional.is_some()
        || previous.regional.is_some()
    {
        return Err(RequestError::connect(
            "gateway policy changed after admission",
        ));
    }
    Ok(())
}

async fn resolve_peer(peer: &str) -> Option<SocketAddr> {
    tokio::net::lookup_host(peer).await.ok()?.next()
}

pub async fn wait_for_capacity(
    deployment: &Arc<Deployment>,
    exclude: &[String],
    metrics: &Metrics,
    feed: &Feed,
) -> Option<Arc<VmBackend>> {
    if !deployment.can_grow() {
        return None;
    }
    let _waiter = deployment.track_waiter();
    metrics.record_cold_start_wait(&deployment.spec.id);
    deployment.scale_signal.notify_one();
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(deployment.spec.scaling.cold_start_timeout_secs);
    tracing::info!(deployment = %deployment.spec.id,
        timeout_secs = deployment.spec.scaling.cold_start_timeout_secs,
        "holding request for cold start");
    loop {
        let notified = deployment.ready_signal.notified();
        if let Some(backend) = deployment.select(exclude) {
            metrics.record_cold_start_hit(&deployment.spec.id);
            return Some(backend);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            tracing::warn!(deployment = %deployment.spec.id, "cold start timed out with no VM available");
            metrics.record_cold_start_timeout(&deployment.spec.id);
            feed.issue(
                &deployment.spec,
                format!("{}: cold start timed out", deployment.spec.id),
                format!(
                    "a request waited {}s and no VM became available",
                    deployment.spec.scaling.cold_start_timeout_secs
                ),
                crate::deployment::now_secs(),
            );
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeploymentSpec;

    #[tokio::test]
    async fn peer_resolution_is_async_and_fallible() {
        assert_eq!(
            resolve_peer("127.0.0.1:8080").await,
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert!(
            resolve_peer("localhost:8080")
                .await
                .is_some_and(|address| address.ip().is_loopback())
        );
        assert_eq!(resolve_peer("no-port").await, None);
        assert!(
            resolve_peer("definitely-not-a-real-host.invalid:80")
                .await
                .is_none()
        );
    }

    #[test]
    fn request_state_releases_once_and_on_drop() {
        let backend = Arc::new(VmBackend::new(
            "sb-1".into(),
            "10.0.0.1:80".parse().unwrap(),
        ));
        // A second request distinguishes exact-once release from a saturating
        // double release which would incorrectly erase someone else's work.
        backend.acquire();
        backend.acquire();
        let mut state = RequestState::default();
        state.set_reserved_backend(backend.clone());
        state.release();
        state.release();
        assert_eq!(backend.in_flight(), 1);
        backend.acquire();
        state.set_reserved_backend(backend.clone());
        drop(state);
        assert_eq!(
            backend.in_flight(),
            1,
            "cancellation must return the reservation"
        );
        backend.release();
    }

    #[tokio::test]
    async fn regional_completion_releases_assignment_but_failed_attempt_does_not() {
        let spec: DeploymentSpec = serde_json::from_value(serde_json::json!({
            "id":"app", "routes":[{"host":"app.example"}],
            "discovery":{"service_id":"svc","region":"us3","regional":{
                "gateway_id":"us","backend_server_id":"host-us","environment":"test","auth":{"secret":"peer"}}}
        })).unwrap();
        let deployment = Arc::new(Deployment::new(spec));
        let router = deployment.regional.as_ref().unwrap();
        let regional = deployment.spec.discovery.as_ref().unwrap().regional.as_ref().unwrap();
        let backend = Arc::new(VmBackend::for_upstream("127.0.0.1:8888".into()));
        let snapshot = serde_json::from_value(serde_json::json!({
            "protocolVersion":1,"serviceId":"svc","environment":"test","region":"us3",
            "gatewayId":"us","bootId":router.boot_id,"version":1,"operationId":"op",
            "phase":"wait_policy_adopted","proposalGeneration":1,"activeGeneration":1,
            "drainTarget":"us3","closedThroughGeneration":0,"endpoints":[],"policies":[{
                "generation":1,"policy":{"version":1,"regions":[{"region":"us3","weight":1,
                    "gateways":[{"id":"us","backendServerId":"host-us","url":"https://us.example"}]}]}}]
        })).unwrap();
        router.apply(snapshot, regional, "svc", "us3", vec![backend.clone()]).unwrap();
        let secrets = crate::secrets::SecretStore::new("unused-request-control-test", None);
        let mut request = RequestState::default();
        request.set_deployment(deployment.clone());
        request.admit(&deployment, &http::HeaderMap::new(), &secrets).unwrap();
        let registry = Registry::new("unused-request-control-registry");
        let metrics = Metrics::new();
        let feed = Feed::new();
        request.next_peer(&registry, &metrics, &feed).await.unwrap();
        assert_eq!(backend.in_flight(), 1);
        request.connection_failed();
        assert_eq!(backend.in_flight(), 0);
        assert!(!request.retry_allowed());
        assert!(request.next_peer(&registry, &metrics, &feed).await.is_err());
        assert_eq!(router.status(regional, true)["report"]["localTarget"], 1);
        request.complete();
        assert_eq!(router.status(regional, true)["report"]["localTarget"], 0);
        assert_eq!(router.status(regional, true)["report"]["outgoingTarget"], 0);
        request.complete();
        drop(request);
    }

    #[test]
    fn stale_flat_admission_cannot_be_replayed_into_a_regional_route() {
        let flat: DeploymentSpec = serde_json::from_value(serde_json::json!({
            "id":"app", "routes":[{"host":"app.example"}],
            "discovery":{"service_id":"svc"}, "upstreams":["127.0.0.1:8000"]
        }))
        .unwrap();
        let previous = Deployment::new(flat.clone());
        let mut refreshed = flat.clone();
        refreshed.upstreams = vec!["127.0.0.1:8001".into()];
        assert!(check_flat_admission_refresh(&previous, &Deployment::new(refreshed)).is_ok());
        let mut regional = flat;
        regional.discovery.as_mut().unwrap().region = Some("us3".into());
        regional.discovery.as_mut().unwrap().regional = Some(serde_json::from_value(serde_json::json!({
            "gateway_id":"us", "backend_server_id":"host-us", "environment":"prod", "auth":{"secret":"peer"}
        })).unwrap());
        let current = Deployment::new(regional);
        assert!(check_flat_admission_refresh(&previous, &current).is_err());
        assert!(check_flat_admission_refresh(&current, &previous).is_err());
    }
}
