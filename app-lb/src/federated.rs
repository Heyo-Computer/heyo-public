//! Federated admin auth: a bearer somebody else issued, turned into a set of
//! namespace grants by asking the Heyo auth API.
//!
//! The admin gate ([`crate::admin`]) knows two credentials of its own — the
//! startup Basic pair and app-tokens it minted itself. A managed fleet has a
//! third kind of caller: a Heyo customer, carrying the JWT (or `heyo_api_*`
//! key) the Heyo auth service gave them. app-lb never sees that user's
//! password and never mints them anything; it asks `GET /api/auth/scopes` what
//! that bearer is allowed to reach and enforces the answer with the same
//! namespace wall a local namespace token gets.
//!
//! What comes back is a list of scope strings. The grammar is the contract
//! with the auth service and is deliberately tiny:
//!
//! | scope | meaning |
//! | --- | --- |
//! | `namespace:<name>:admin` | admin tier on every deployment in `<name>` |
//! | `namespace:<name>:view` | view tier only — directory, `/metrics`, list, get |
//! | `fleet:admin` | unconfined, as the Basic operator is |
//!
//! Anything else is ignored with a debug log rather than refused, so the auth
//! service can grow new scopes without breaking an older app-lb.
//!
//! Answers are cached by the SHA-256 of the bearer for at most
//! `APP_LB_AUTH_CACHE_SECS` (and never past the token's own expiry), so a
//! dashboard polling `/metrics` costs one upstream round trip per minute rather
//! than one per poll. Refusals are cached too, briefly, so a bad token cannot
//! turn app-lb into an amplifier against the auth service. The cost of the
//! cache is revocation latency: a scope withdrawn upstream lingers here for
//! up to the TTL. That is the same trade every JWT makes, and shorter than most.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::tokens::AdminScope;

/// Bearers with this prefix are app-lb's own tokens and never leave the
/// process; the gate resolves them from the local store, not from here.
pub const LOCAL_TOKEN_PREFIX: &str = "applb_";

/// How long a refusal is remembered. Short: a token that has just been
/// created, or a transient auth-service outage, should not lock a caller out
/// for a whole cache period.
const MISS_TTL: Duration = Duration::from_secs(5);

/// Above this many cached entries the expired ones are swept on insert. A
/// bound rather than an LRU: the cache is keyed by token hash, and a caller
/// spraying fresh tokens would otherwise grow it without limit.
const MAX_ENTRIES: usize = 4096;

/// Who the auth service says the bearer is.
#[derive(Debug, Clone, Deserialize)]
pub struct Subject {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(rename = "accountId", default)]
    pub account_id: Option<String>,
    #[serde(rename = "platformRole", default)]
    pub platform_role: Option<String>,
}

/// What a federated bearer may do, as the gate consumes it.
#[derive(Debug)]
pub struct Grant {
    pub subject: Subject,
    /// Namespace → the strongest tier granted there.
    pub namespaces: BTreeMap<String, AdminScope>,
    /// `fleet:admin` was present: this caller is not behind any wall.
    pub fleet: bool,
}

enum Entry {
    Hit(Arc<Grant>, Instant),
    Miss(Instant),
}

impl Entry {
    fn expires_at(&self) -> Instant {
        match self {
            Entry::Hit(_, at) | Entry::Miss(at) => *at,
        }
    }
}

/// The resolver, one per process.
pub struct FederatedAuth {
    base_url: String,
    http: reqwest::Client,
    ttl: Duration,
    cache: Mutex<HashMap<[u8; 32], Entry>>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    data: Option<Data>,
}

#[derive(Deserialize)]
struct Data {
    subject: Subject,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(rename = "expiresIn", default)]
    expires_in: Option<u64>,
}

impl FederatedAuth {
    pub fn new(base_url: String, ttl_secs: u64, timeout_secs: u64) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs.max(1)))
                .build()
                .unwrap_or_default(),
            ttl: Duration::from_secs(ttl_secs.max(1)),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The grant behind `bearer`, or `None` when the auth service refuses it
    /// (or cannot be reached — an outage fails closed).
    pub async fn resolve(&self, bearer: &str) -> Option<Arc<Grant>> {
        if bearer.is_empty() || bearer.starts_with(LOCAL_TOKEN_PREFIX) {
            return None;
        }
        let key: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
        let now = Instant::now();
        if let Some(cached) = self.cached(&key, now) {
            return cached;
        }
        let fetched = self.fetch(bearer).await;
        let entry = match &fetched {
            Some((grant, ttl)) => Entry::Hit(grant.clone(), now + *ttl),
            None => Entry::Miss(now + MISS_TTL.min(self.ttl)),
        };
        self.store(key, entry, now);
        fetched.map(|(g, _)| g)
    }

    /// `Some(answer)` when the cache has an unexpired entry, where `answer` is
    /// itself `None` for a remembered refusal.
    fn cached(&self, key: &[u8; 32], now: Instant) -> Option<Option<Arc<Grant>>> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(key) {
            Some(e) if e.expires_at() > now => Some(match e {
                Entry::Hit(g, _) => Some(g.clone()),
                Entry::Miss(_) => None,
            }),
            _ => None,
        }
    }

    fn store(&self, key: [u8; 32], entry: Entry, now: Instant) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= MAX_ENTRIES {
            cache.retain(|_, e| e.expires_at() > now);
        }
        cache.insert(key, entry);
    }

    async fn fetch(&self, bearer: &str) -> Option<(Arc<Grant>, Duration)> {
        let url = format!("{}/api/auth/scopes", self.base_url);
        let resp = match self.http.get(&url).bearer_auth(bearer).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "auth service unreachable; refusing federated bearer");
                return None;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            tracing::debug!(status = %status, "auth service refused federated bearer");
            return None;
        }
        let env: Envelope = match resp.json().await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "auth service returned an unparseable scopes response");
                return None;
            }
        };
        let (Some(data), true) = (env.data, env.success) else {
            return None;
        };
        let (namespaces, fleet) = Self::parse_scopes(&data.scopes);
        let ttl = data
            .expires_in
            .map(Duration::from_secs)
            .map_or(self.ttl, |e| e.min(self.ttl));
        tracing::debug!(
            user = %data.subject.user_id,
            namespaces = namespaces.len(),
            fleet,
            "resolved federated bearer"
        );
        Some((
            Arc::new(Grant {
                subject: data.subject,
                namespaces,
                fleet,
            }),
            ttl,
        ))
    }

    /// The scope grammar. Pure, so it is tested without a server.
    ///
    /// A namespace granted twice keeps the stronger tier. A namespace that
    /// would not validate as a spec's namespace is dropped: it could never
    /// match a deployment, and letting it into the map would only make the
    /// grant *look* wider than it is.
    pub fn parse_scopes(scopes: &[String]) -> (BTreeMap<String, AdminScope>, bool) {
        let mut namespaces: BTreeMap<String, AdminScope> = BTreeMap::new();
        let mut fleet = false;
        for scope in scopes {
            let parts: Vec<&str> = scope.split(':').collect();
            match parts.as_slice() {
                ["fleet", "admin"] => fleet = true,
                ["namespace", ns, tier] if crate::config::is_valid_namespace(ns) => {
                    let granted = match *tier {
                        "admin" => AdminScope::Admin,
                        "view" => AdminScope::View,
                        _ => {
                            tracing::debug!(scope = %scope, "ignoring scope with unknown tier");
                            continue;
                        }
                    };
                    let slot = namespaces.entry((*ns).to_string()).or_insert(AdminScope::None);
                    if granted.satisfies(*slot) {
                        *slot = granted;
                    }
                }
                _ => tracing::debug!(scope = %scope, "ignoring unrecognised scope"),
            }
        }
        (namespaces, fleet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_scope_grammar_is_parsed_and_the_rest_ignored() {
        let (ns, fleet) = FederatedAuth::parse_scopes(&s(&[
            "namespace:team-a:admin",
            "namespace:team-b:view",
            "namespace:team-b:admin", // stronger duplicate wins
            "namespace:team-c:owner", // unknown tier
            "namespace:bad name:admin", // invalid namespace
            "billing:read",           // not ours
        ]));
        assert!(!fleet);
        assert_eq!(ns.get("team-a"), Some(&AdminScope::Admin));
        assert_eq!(ns.get("team-b"), Some(&AdminScope::Admin));
        assert!(!ns.contains_key("team-c"));
        assert_eq!(ns.len(), 2);

        let (ns, fleet) = FederatedAuth::parse_scopes(&s(&["fleet:admin"]));
        assert!(fleet);
        assert!(ns.is_empty());
    }

    #[derive(Clone)]
    struct Mock {
        calls: Arc<AtomicUsize>,
        body: Arc<serde_json::Value>,
    }

    async fn scopes(State(m): State<Mock>, headers: HeaderMap) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        m.calls.fetch_add(1, Ordering::SeqCst);
        let ok = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "Bearer good");
        if ok {
            (axum::http::StatusCode::OK, Json((*m.body).clone()))
        } else {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"success": false, "code": "INVALID_TOKEN"})),
            )
        }
    }

    async fn serve(body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = Mock { calls: calls.clone(), body: Arc::new(body) };
        let app = Router::new().route("/api/auth/scopes", get(scopes)).with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/"), calls)
    }

    fn good_body() -> serde_json::Value {
        serde_json::json!({
            "success": true,
            "data": {
                "subject": {"userId": "u1", "email": "a@b", "accountId": "acc", "platformRole": "user"},
                "scopes": ["namespace:team-a:admin"],
                "expiresIn": 3600
            }
        })
    }

    #[tokio::test]
    async fn a_good_bearer_is_resolved_once_and_then_served_from_cache() {
        let (url, calls) = serve(good_body()).await;
        let auth = FederatedAuth::new(url, 60, 5);
        let g = auth.resolve("good").await.expect("resolved");
        assert_eq!(g.subject.user_id, "u1");
        assert_eq!(g.namespaces.get("team-a"), Some(&AdminScope::Admin));
        assert!(!g.fleet);
        let again = auth.resolve("good").await.expect("cached");
        assert!(Arc::ptr_eq(&g, &again));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_bearer_is_remembered_briefly() {
        let (url, calls) = serve(good_body()).await;
        let auth = FederatedAuth::new(url, 60, 5);
        assert!(auth.resolve("bad").await.is_none());
        assert!(auth.resolve("bad").await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the refusal was cached");
    }

    #[tokio::test]
    async fn local_tokens_and_dead_servers_never_resolve() {
        let (url, calls) = serve(good_body()).await;
        let auth = FederatedAuth::new(url, 60, 5);
        assert!(auth.resolve("applb_abc_def").await.is_none());
        assert!(auth.resolve("").await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let dead = FederatedAuth::new("http://127.0.0.1:1".into(), 60, 1);
        assert!(dead.resolve("good").await.is_none(), "an outage fails closed");
    }
}
