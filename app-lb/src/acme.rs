//! Automatic Let's Encrypt certificates for deployment hostnames.
//!
//! Runs as a pingora `BackgroundService` shaped like the
//! [`Autoscaler`](crate::autoscale::Autoscaler): a slow ticker for renewal
//! sweeps, plus a [`Notify`] the admin API nudges when a deployment is
//! registered, so a new hostname gets its certificate in seconds rather than at
//! the next tick.
//!
//! Validation is HTTP-01, answered by the proxy itself out of
//! [`ChallengeTable`] — see `proxy::request_filter`. That means **the plaintext
//! proxy listener must be reachable on port 80**, which is where Let's Encrypt
//! sends the validation request; there is no way to point it elsewhere.
//!
//! Only exact `host` route rules get certificates. A `host_suffix` rule matches
//! a whole subtree, which needs a wildcard certificate, and Let's Encrypt only
//! issues those over DNS-01 — a different challenge with a different set of
//! credentials to manage. Those deployments fall back to the static cert.

use crate::registry::Registry;
use crate::tls::CertStore;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// How often to sweep for certificates approaching expiry. Renewal starts 30
/// days out (`tls::RENEW_BEFORE_DAYS`), so twice a day leaves an enormous margin
/// while keeping the CA traffic negligible.
const TICK: Duration = Duration::from_secs(12 * 60 * 60);

/// First retry delay after a failed issuance, doubling up to [`BACKOFF_MAX`].
/// Let's Encrypt's failed-validation limit is low enough that a tight retry loop
/// gets the account throttled for an hour, which would block *every* hostname —
/// so backoff here protects the deployments that are working, not just this one.
const BACKOFF_START: Duration = Duration::from_secs(60);
const BACKOFF_MAX: Duration = Duration::from_secs(6 * 60 * 60);

/// How long to wait for the CA to validate a challenge and issue. The library
/// default (30s) is tight for HTTP-01 under load.
const POLL: RetryPolicy = RetryPolicy::new().timeout(Duration::from_secs(120));

/// Live HTTP-01 challenge responses, keyed by token.
///
/// Written by the ACME manager, read by the proxy on every request that hits
/// `/.well-known/acme-challenge/`. `ArcSwap` for the same reason the registry
/// uses one: reads are on the request path, writes happen a few times a year.
#[derive(Default)]
pub struct ChallengeTable(ArcSwap<HashMap<String, String>>);

impl ChallengeTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The key authorization to serve for `token`, if one is outstanding.
    pub fn get(&self, token: &str) -> Option<String> {
        self.0.load().get(token).cloned()
    }

    pub(crate) fn publish(&self, token: String, key_authorization: String) {
        self.0.rcu(|current| {
            let mut next = HashMap::clone(current);
            next.insert(token.clone(), key_authorization.clone());
            next
        });
    }

    /// Retract tokens once the order is done. Left in place they would be a
    /// small, permanent, publicly-readable leak of past challenge responses.
    fn retract(&self, tokens: &[String]) {
        if tokens.is_empty() {
            return;
        }
        self.0.rcu(|current| {
            let mut next = HashMap::clone(current);
            for token in tokens {
                next.remove(token);
            }
            next
        });
    }
}

#[derive(Debug)]
pub enum AcmeError {
    Acme(instant_acme::Error),
    Cert(crate::tls::CertError),
    Io(std::io::Error),
    /// The CA offered no HTTP-01 challenge for an authorization.
    NoHttp01,
    /// The order finished in a state other than valid.
    Order(OrderStatus),
}

impl std::fmt::Display for AcmeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acme(e) => write!(f, "{e}"),
            Self::Cert(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::NoHttp01 => write!(f, "CA offered no http-01 challenge"),
            Self::Order(s) => write!(f, "order ended in state {s:?}"),
        }
    }
}

impl From<instant_acme::Error> for AcmeError {
    fn from(e: instant_acme::Error) -> Self {
        Self::Acme(e)
    }
}

impl From<crate::tls::CertError> for AcmeError {
    fn from(e: crate::tls::CertError) -> Self {
        Self::Cert(e)
    }
}

impl From<std::io::Error> for AcmeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Per-host retry state after a failure.
struct Backoff {
    next_attempt: Instant,
    delay: Duration,
}

pub struct AcmeConfig {
    /// Account contact address. Its presence is what enables ACME at all.
    pub email: String,
    /// Account key and issued certificates live here. Should be `0700`.
    pub dir: PathBuf,
    /// ACME directory URL. Point at Let's Encrypt *staging* for any testing —
    /// production rate limits are per-account-per-week and unforgiving.
    pub directory_url: String,
}

impl AcmeConfig {
    pub fn production_directory() -> String {
        LetsEncrypt::Production.url().to_string()
    }
}

pub struct AcmeManager {
    registry: Arc<Registry>,
    certs: Arc<CertStore>,
    challenges: Arc<ChallengeTable>,
    config: AcmeConfig,
    /// Nudged by the admin API when a deployment is registered or updated.
    signal: Arc<Notify>,
    /// Built lazily on first use, so a configured-but-idle app-lb never talks to
    /// the CA. `tokio` mutex because construction is async.
    account: tokio::sync::Mutex<Option<Arc<Account>>>,
    backoff: Mutex<HashMap<String, Backoff>>,
    /// Hostnames already warned about as unsupported, so the wildcard warning
    /// doesn't repeat on every sweep.
    warned: Mutex<HashSet<String>>,
}

impl AcmeManager {
    pub fn new(
        registry: Arc<Registry>,
        certs: Arc<CertStore>,
        challenges: Arc<ChallengeTable>,
        config: AcmeConfig,
    ) -> Self {
        Self {
            registry,
            certs,
            challenges,
            config,
            signal: Arc::new(Notify::new()),
            account: tokio::sync::Mutex::new(None),
            backoff: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
        }
    }

    /// Handle the admin API uses to ask for an immediate sweep.
    pub fn signal(&self) -> Arc<Notify> {
        self.signal.clone()
    }

    fn account_path(&self) -> PathBuf {
        self.config.dir.join("account.json")
    }

    /// Load the saved ACME account, or register a new one and save it.
    ///
    /// Reusing the account matters: a new account per restart would burn through
    /// the new-registrations rate limit and lose the CA-side validation cache
    /// that lets a re-issue skip the challenge entirely.
    async fn account(&self) -> Result<Arc<Account>, AcmeError> {
        let mut guard = self.account.lock().await;
        if let Some(account) = guard.as_ref() {
            return Ok(account.clone());
        }

        let path = self.account_path();
        let account = match std::fs::read(&path) {
            Ok(bytes) => {
                let credentials: AccountCredentials = serde_json::from_slice(&bytes)
                    .map_err(|e| AcmeError::Io(std::io::Error::other(e)))?;
                tracing::info!(path = %path.display(), "restored ACME account");
                Account::builder()?.from_credentials(credentials).await?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let contact = format!("mailto:{}", self.config.email);
                let (account, credentials) = Account::builder()?
                    .create(
                        &NewAccount {
                            contact: &[contact.as_str()],
                            terms_of_service_agreed: true,
                            only_return_existing: false,
                        },
                        self.config.directory_url.clone(),
                        None,
                    )
                    .await?;

                let json = serde_json::to_vec_pretty(&credentials)
                    .map_err(|e| AcmeError::Io(std::io::Error::other(e)))?;
                save_secret(&path, &json)?;
                tracing::info!(path = %path.display(), "registered new ACME account");
                account
            }
            Err(e) => return Err(AcmeError::Io(e)),
        };

        let account = Arc::new(account);
        *guard = Some(account.clone());
        Ok(account)
    }

    /// Every exact hostname across all deployments.
    ///
    /// `host_suffix` rules are deliberately excluded — see the module docs — and
    /// warned about once each so the gap is visible in the log rather than
    /// discovered at handshake time.
    fn desired_hosts(&self) -> Vec<String> {
        let mut hosts = HashSet::new();
        let mut unsupported = Vec::new();

        for deployment in self.registry.deployments().values() {
            for rule in &deployment.spec.routes {
                if let Some(host) = &rule.host {
                    hosts.insert(host.trim().to_ascii_lowercase());
                }
                if let Some(suffix) = &rule.host_suffix {
                    unsupported.push((deployment.spec.id.clone(), suffix.clone()));
                }
            }
        }

        if let Ok(mut warned) = self.warned.lock() {
            for (id, suffix) in unsupported {
                if warned.insert(suffix.clone()) {
                    tracing::warn!(
                        deployment = %id,
                        host_suffix = %suffix,
                        "no ACME certificate: wildcard hostnames need a DNS-01 challenge, \
                         which app-lb does not implement; this deployment will be served \
                         the static fallback certificate",
                    );
                }
            }
        }

        let mut hosts: Vec<_> = hosts.into_iter().filter(|h| !h.is_empty()).collect();
        hosts.sort();
        hosts
    }

    /// `true` if this host is not in a post-failure cooldown.
    fn may_attempt(&self, host: &str) -> bool {
        let Ok(backoff) = self.backoff.lock() else {
            return true;
        };
        backoff
            .get(host)
            .is_none_or(|b| Instant::now() >= b.next_attempt)
    }

    fn record_failure(&self, host: &str) {
        let Ok(mut backoff) = self.backoff.lock() else {
            return;
        };
        let entry = backoff.entry(host.to_string()).or_insert(Backoff {
            next_attempt: Instant::now(),
            delay: BACKOFF_START,
        });
        entry.next_attempt = Instant::now() + entry.delay;
        entry.delay = (entry.delay * 2).min(BACKOFF_MAX);
        tracing::debug!(host, retry_in_secs = entry.delay.as_secs(), "backing off");
    }

    fn record_success(&self, host: &str) {
        if let Ok(mut backoff) = self.backoff.lock() {
            backoff.remove(host);
        }
    }

    /// Issue or renew every hostname that needs it, one at a time.
    ///
    /// Sequential on purpose: orders against one account share rate limits, and
    /// a burst of parallel orders is the fastest way to get throttled.
    async fn reconcile(&self) {
        let wanted = self.desired_hosts();
        if wanted.is_empty() {
            return;
        }

        let due: Vec<String> = self
            .certs
            .needing_renewal(wanted.iter().map(String::as_str))
            .into_iter()
            .filter(|h| self.may_attempt(h))
            .collect();

        if due.is_empty() {
            return;
        }

        let account = match self.account().await {
            Ok(a) => a,
            Err(e) => {
                // Every host is blocked by this, so back them all off rather
                // than hammering a CA that just refused us.
                tracing::error!(error = %e, "could not obtain ACME account; deferring issuance");
                for host in &due {
                    self.record_failure(host);
                }
                return;
            }
        };

        tracing::info!(count = due.len(), "issuing certificates");
        for host in due {
            match self.issue(&account, &host).await {
                Ok(()) => {
                    tracing::info!(host = %host, "certificate installed");
                    self.record_success(&host);
                }
                Err(e) => {
                    tracing::error!(host = %host, error = %e, "certificate issuance failed");
                    self.record_failure(&host);
                }
            }
        }
    }

    /// Run one HTTP-01 order end to end and install the result.
    async fn issue(&self, account: &Account, host: &str) -> Result<(), AcmeError> {
        let identifiers = [Identifier::Dns(host.to_string())];
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;

        // Publish each challenge response before telling the CA it's ready,
        // otherwise validation races the proxy and fails.
        let mut tokens = Vec::new();
        let published = async {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization?;
                // The CA caches successful validations; a re-issue inside that
                // window arrives already valid with nothing to answer.
                if authorization.status == AuthorizationStatus::Valid {
                    continue;
                }

                let Some(mut challenge) = authorization.challenge(ChallengeType::Http01) else {
                    return Err(AcmeError::NoHttp01);
                };
                let token = challenge.token.clone();
                self.challenges
                    .publish(token.clone(), challenge.key_authorization().as_str().to_string());
                tokens.push(token);
                challenge.set_ready().await?;
            }
            Ok(())
        }
        .await;

        // From here every exit path must retract the tokens, so the result is
        // held rather than propagated with `?`.
        let result = match published {
            Ok(()) => self.finish_order(&mut order, host).await,
            Err(e) => Err(e),
        };
        self.challenges.retract(&tokens);
        result
    }

    /// Wait for validation, finalize, and install the issued chain.
    async fn finish_order(
        &self,
        order: &mut instant_acme::Order,
        host: &str,
    ) -> Result<(), AcmeError> {
        let status = order.poll_ready(&POLL).await?;
        if status != OrderStatus::Ready {
            return Err(AcmeError::Order(status));
        }

        // `finalize` generates the keypair and CSR and hands back the private
        // key as PEM — the only copy, so it goes straight to the store.
        let key_pem = order.finalize().await?;
        let chain_pem = order.poll_certificate(&POLL).await?;

        self.certs
            .insert(host, chain_pem.as_bytes(), key_pem.as_bytes())?;
        Ok(())
    }
}

/// Write a secret with a restrictive mode, via a temp file so a crash can't
/// leave a half-written key behind.
fn save_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        crate::tls::create_dir_private(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    crate::tls::restrict(&tmp)?;
    std::fs::rename(&tmp, path)
}

#[async_trait]
impl BackgroundService for AcmeManager {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        tracing::info!(
            directory = %self.config.directory_url,
            dir = %self.config.dir.display(),
            "ACME enabled",
        );

        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Issue for anything already registered (restored from persisted state)
        // without waiting out the first tick.
        self.reconcile().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => self.reconcile().await,
                _ = self.signal.notified() => self.reconcile().await,
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("ACME manager shutting down");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeploymentSpec, RouteRule};

    fn registry_with(routes: Vec<RouteRule>) -> Arc<Registry> {
        let registry = Arc::new(Registry::new("/dev/null"));
        registry.upsert(DeploymentSpec {
            id: "test".into(),
            routes,
            vm: None,
            scaling: Default::default(),
            health: Default::default(),
            upstreams: vec!["127.0.0.1:8080".into()],
        });
        registry
    }

    fn manager(registry: Arc<Registry>) -> AcmeManager {
        AcmeManager::new(
            registry,
            Arc::new(CertStore::new("/tmp/app-lb-acme-test", None)),
            Arc::new(ChallengeTable::new()),
            AcmeConfig {
                email: "test@example.com".into(),
                dir: "/tmp/app-lb-acme-test".into(),
                directory_url: LetsEncrypt::Staging.url().to_string(),
            },
        )
    }

    #[test]
    fn desired_hosts_collects_exact_hosts_only() {
        let manager = manager(registry_with(vec![
            RouteRule {
                host: Some("A.example.com".into()),
                host_suffix: None,
                path_prefix: None,
            },
            RouteRule {
                host: Some("b.example.com".into()),
                host_suffix: None,
                path_prefix: None,
            },
            // Wildcards need DNS-01; must not appear.
            RouteRule {
                host: None,
                host_suffix: Some("apps.example.com".into()),
                path_prefix: None,
            },
            // Path-only routes have no hostname to certify.
            RouteRule {
                host: None,
                host_suffix: None,
                path_prefix: Some("/api".into()),
            },
        ]));

        assert_eq!(
            manager.desired_hosts(),
            vec!["a.example.com".to_string(), "b.example.com".to_string()],
            "lowercased, sorted, exact hosts only",
        );
    }

    #[test]
    fn wildcard_warning_is_logged_once_per_suffix() {
        let manager = manager(registry_with(vec![RouteRule {
            host: None,
            host_suffix: Some("apps.example.com".into()),
            path_prefix: None,
        }]));

        manager.desired_hosts();
        let warned_after_first = manager.warned.lock().unwrap().len();
        manager.desired_hosts();
        let warned_after_second = manager.warned.lock().unwrap().len();

        assert_eq!(warned_after_first, 1);
        assert_eq!(warned_after_second, 1, "the sweep must not re-warn");
    }

    #[test]
    fn challenge_table_publishes_and_retracts() {
        let table = ChallengeTable::new();
        assert_eq!(table.get("tok"), None);

        table.publish("tok".into(), "tok.thumbprint".into());
        assert_eq!(table.get("tok").as_deref(), Some("tok.thumbprint"));

        table.retract(&["tok".to_string()]);
        assert_eq!(table.get("tok"), None, "tokens must not outlive the order");
    }

    #[test]
    fn backoff_grows_and_clears_on_success() {
        let manager = manager(registry_with(vec![]));
        assert!(manager.may_attempt("a.example.com"), "no history, no wait");

        manager.record_failure("a.example.com");
        assert!(!manager.may_attempt("a.example.com"), "cooling down");

        let first = manager.backoff.lock().unwrap()["a.example.com"].delay;
        manager.record_failure("a.example.com");
        let second = manager.backoff.lock().unwrap()["a.example.com"].delay;
        assert!(second > first, "delay must grow");

        manager.record_success("a.example.com");
        assert!(manager.may_attempt("a.example.com"), "success clears backoff");
    }

    #[test]
    fn backoff_is_capped() {
        let manager = manager(registry_with(vec![]));
        for _ in 0..50 {
            manager.record_failure("a.example.com");
        }
        assert_eq!(
            manager.backoff.lock().unwrap()["a.example.com"].delay,
            BACKOFF_MAX,
            "unbounded growth would strand a host forever",
        );
    }
}
