//! nats-server's HTTP monitoring port, as Rust types.
//!
//! Three endpoints answer everything this dashboard shows:
//!
//! | Endpoint | What it carries |
//! | --- | --- |
//! | `/varz`  | the server itself, and the cumulative message counters throughput is derived from |
//! | `/connz` | one row per connected client |
//! | `/jsz`   | every account's streams and consumers — depth, and who is draining it |
//!
//! **Nothing in this module can consume, publish, or administer anything.** It
//! speaks HTTP GET to a read-only port; it never opens a NATS client
//! connection, so there is no code path here that could bind a consumer to a
//! `WorkQueue` stream and start eating somebody's work, and none that could
//! reach `$SYS.REQ.SERVER.<id>.SHUTDOWN`. That is a stronger guarantee than a
//! carefully-permissioned system-account credential, and it is the reason this
//! app is built on the monitoring port rather than on a NATS connection.
//!
//! ## Deserialization is deliberately permissive
//!
//! Every field is `#[serde(default)]` and unknown fields are ignored, so a NATS
//! upgrade that adds a key — or one that drops a key this version does not
//! populate — degrades to a zero in one column rather than a dashboard that
//! stops updating. The shapes below were read off a live NATS 2.12.6.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// `/jsz`'s query string. `consumers=1` is what fills `consumer_detail`, and
/// `config=1` is what stops `config` coming back as an object of empty fields —
/// which reads like a server bug rather than a missing parameter.
const JSZ_QUERY: &str = "accounts=1&streams=1&consumers=1&config=1";

/// `/connz`'s. `sort=last` puts the most recently active client first, which is
/// the order somebody scanning for "who is still talking" wants; `subs=1` adds
/// each connection's subject list, the thing that answers "which client holds
/// the consumer on this subject".
const CONNZ_SORT: &str = "sort=last&subs=1";

/// What went wrong reaching the monitoring port.
///
/// One variant per thing an operator would fix differently: a URL that does not
/// parse is a typo in the unit file, a transport failure is a server that is
/// down or a port that is closed, and a decode failure is a NATS version whose
/// answer this build cannot read.
#[derive(Debug)]
pub enum MonitorError {
    Request(String),
    Status(u16),
    Decode(String),
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // No "cannot reach…" framing here: the dashboard already says which
            // URL it was reading when it shows this, and reqwest's own message
            // names the URL again. Three copies of the same fact is what a
            // banner reading "Cannot reach X: cannot reach X: …" is made of.
            Self::Request(e) => write!(f, "{e}"),
            Self::Status(code) => write!(f, "monitoring port answered {code}"),
            Self::Decode(e) => write!(f, "cannot read the monitoring response: {e}"),
        }
    }
}

impl std::error::Error for MonitorError {}

/// Trim a configured base URL to something the paths below can be appended to.
///
/// A trailing slash is the ordinary way to write a base URL and would otherwise
/// produce `//varz`, which some proxies answer and some do not — a difference
/// that shows up only once the app is behind one.
fn normalize_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// A client for one server's monitoring port.
#[derive(Clone)]
pub struct Monitor {
    http: reqwest::Client,
    base: String,
}

impl Monitor {
    pub fn new(base_url: &str, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            // The monitoring port is a single loopback endpoint scraped on a
            // timer; a pool is neither needed nor free.
            .pool_max_idle_per_host(1)
            .build()
            // The only failure here is a TLS backend that will not initialise,
            // which is a broken build rather than a runtime condition.
            .expect("failed to build the HTTP client");
        Self {
            http,
            base: normalize_base(base_url),
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, MonitorError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| MonitorError::Request(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(MonitorError::Status(status.as_u16()));
        }
        // Read the body as bytes and decode separately, so a decode failure can
        // say which endpoint it was on rather than surfacing as a bare serde
        // message with no context.
        let body = response
            .bytes()
            .await
            .map_err(|e| MonitorError::Request(e.to_string()))?;
        serde_json::from_slice(&body).map_err(|e| MonitorError::Decode(format!("{path}: {e}")))
    }

    pub async fn varz(&self) -> Result<Varz, MonitorError> {
        self.get("/varz").await
    }

    pub async fn connz(&self, limit: usize) -> Result<Connz, MonitorError> {
        self.get(&format!("/connz?{CONNZ_SORT}&limit={limit}"))
            .await
    }

    pub async fn jsz(&self) -> Result<Jsz, MonitorError> {
        self.get(&format!("/jsz?{JSZ_QUERY}")).await
    }
}

// ---------------------------------------------------------------------------
// /varz
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Varz {
    pub server_id: String,
    pub server_name: String,
    pub version: String,
    pub host: String,
    pub port: u16,
    /// When this process started, RFC3339. Used as the identity of a server
    /// *run*: when it changes, every counter below restarted at zero and the
    /// rate across that boundary is not a rate.
    pub start: String,
    pub now: String,
    pub uptime: String,
    pub mem: u64,
    pub cores: u32,
    pub cpu: f64,
    pub connections: u64,
    pub total_connections: u64,
    pub subscriptions: u64,
    pub routes: u64,
    pub remotes: u64,
    pub leafnodes: u64,
    pub max_connections: u64,
    pub max_payload: u64,
    /// Cumulative since `start` — never a rate. See [`crate::state`].
    pub in_msgs: u64,
    pub out_msgs: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    /// Cumulative too, and the one counter whose *absolute* value matters: a
    /// slow consumer is a client the server gave up writing to, which on a
    /// WorkQueue stream means work that has to be redelivered.
    pub slow_consumers: u64,
    pub jetstream: VarzJetStream,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VarzJetStream {
    pub config: VarzJetStreamConfig,
    pub stats: VarzJetStreamStats,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VarzJetStreamConfig {
    pub max_memory: u64,
    pub max_storage: u64,
    pub store_dir: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VarzJetStreamStats {
    pub memory: u64,
    pub storage: u64,
    pub accounts: u64,
    pub api: JszApi,
}

// ---------------------------------------------------------------------------
// /connz
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Connz {
    pub now: String,
    pub num_connections: u64,
    /// Connections on the server, which is larger than `connections.len()`
    /// whenever the limit truncated the page.
    pub total: u64,
    pub limit: u64,
    pub connections: Vec<ConnInfo>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConnInfo {
    pub cid: u64,
    /// `Client`, `Router`, `Leafnode`… — worth carrying because a cluster route
    /// in the client list is a confusing thing to have to explain.
    pub kind: String,
    pub ip: String,
    pub port: u16,
    pub start: String,
    pub last_activity: String,
    /// Pre-formatted by the server ("237µs"), not a duration to do arithmetic
    /// on. Displayed as given.
    pub rtt: String,
    pub uptime: String,
    pub idle: String,
    pub pending_bytes: u64,
    pub in_msgs: u64,
    pub out_msgs: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    pub subscriptions: u64,
    /// The client's `name` from its CONNECT — empty for a client that did not
    /// set one, which is exactly why every service in this fleet does.
    pub name: String,
    pub lang: String,
    pub version: String,
    /// Present only on a server with accounts configured.
    pub account: String,
    pub subscriptions_list: Vec<String>,
}

// ---------------------------------------------------------------------------
// /jsz
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Jsz {
    pub server_id: String,
    pub now: String,
    pub streams: u64,
    pub consumers: u64,
    pub messages: u64,
    pub bytes: u64,
    pub memory: u64,
    pub storage: u64,
    pub account_details: Vec<JszAccount>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszAccount {
    pub name: String,
    pub id: String,
    pub memory: u64,
    pub storage: u64,
    pub api: JszApi,
    pub stream_detail: Vec<JszStream>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszApi {
    pub total: u64,
    /// Rejected JetStream API calls. Climbing errors with a flat total is a
    /// client that is being refused something, which no gauge on a stream shows.
    pub errors: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszStream {
    pub name: String,
    pub created: String,
    pub config: JszStreamConfig,
    pub state: JszStreamState,
    /// Populated only because the request asks for `consumers=1`. A stream with
    /// no consumers has no key here at all, not an empty array.
    pub consumer_detail: Vec<JszConsumer>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszStreamConfig {
    pub name: String,
    pub description: String,
    pub subjects: Vec<String>,
    /// Lowercase strings on the wire — `"workqueue"`, `"limits"`,
    /// `"interest"` — not the CamelCase the Rust client's enums print.
    pub retention: String,
    pub storage: String,
    pub discard: String,
    pub max_msgs: i64,
    pub max_bytes: i64,
    /// Nanoseconds, and `0` means unlimited rather than "expires immediately".
    pub max_age: i64,
    pub num_replicas: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszStreamState {
    /// Messages held right now — the stream's queue depth.
    pub messages: u64,
    pub bytes: u64,
    pub first_seq: u64,
    /// Total ever published, and so the only field a publish rate can be
    /// derived from: `messages` falls as consumers ack, and on a WorkQueue
    /// stream it sits at zero however busy the stream is.
    pub last_seq: u64,
    pub first_ts: String,
    pub last_ts: String,
    pub consumer_count: u64,
    pub num_subjects: u64,
    pub num_deleted: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszConsumer {
    pub stream_name: String,
    pub name: String,
    pub created: String,
    pub config: JszConsumerConfig,
    pub delivered: JszSeqPair,
    pub ack_floor: JszSeqPair,
    pub num_ack_pending: u64,
    pub num_redelivered: u64,
    pub num_waiting: u64,
    /// Messages matching this consumer's filter that it has not been delivered
    /// yet — the backlog *this worker* is behind on, as opposed to the stream's
    /// total depth.
    pub num_pending: u64,
    /// When the server generated this report. **Not** last activity: JSZ
    /// carries no per-consumer idle time, and presenting the report time as one
    /// would make every consumer look busy.
    pub ts: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszConsumerConfig {
    pub durable_name: String,
    pub filter_subject: String,
    pub ack_policy: String,
    /// Raw nanoseconds, not seconds and not a duration string.
    pub ack_wait: i64,
    pub max_deliver: i64,
    pub max_ack_pending: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct JszSeqPair {
    pub consumer_seq: u64,
    pub stream_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live NATS 2.12.6 — trimmed, but every key here is one
    /// the server actually sent.
    const JSZ_SAMPLE: &str = r#"{
      "server_id": "NCN5", "now": "2026-09-05T01:16:31.817972134Z",
      "streams": 8, "consumers": 1, "messages": 0, "bytes": 0,
      "future_field_from_a_later_nats": {"nested": true},
      "account_details": [{
        "name": "$G", "id": "$G",
        "api": {"level": 3, "total": 216, "errors": 9},
        "stream_detail": [{
          "name": "CITEST_JOBS", "created": "2026-08-10T21:55:34Z",
          "config": {"name": "CITEST_JOBS", "subjects": ["citest.job.>"],
                     "retention": "workqueue", "storage": "file", "discard": "new",
                     "max_msgs": -1, "max_bytes": -1, "max_age": 0, "num_replicas": 1},
          "state": {"messages": 3, "bytes": 900, "first_seq": 1, "last_seq": 12,
                    "first_ts": "0001-01-01T00:00:00Z", "last_ts": "2026-09-05T01:00:00Z",
                    "consumer_count": 1},
          "consumer_detail": [{
            "stream_name": "CITEST_JOBS", "name": "worker",
            "config": {"durable_name": "worker", "filter_subject": "citest.job.r.x",
                       "ack_policy": "explicit", "ack_wait": 60000000000, "max_deliver": 4},
            "delivered": {"consumer_seq": 9, "stream_seq": 9},
            "ack_floor": {"consumer_seq": 7, "stream_seq": 7},
            "num_ack_pending": 2, "num_redelivered": 1, "num_waiting": 0, "num_pending": 3,
            "ts": "2026-09-05T01:16:41Z"
          }]
        }]
      }]
    }"#;

    /// The guarantee the `#[serde(default)]` blanket buys: a NATS release that
    /// adds a key must not be able to blank this dashboard.
    #[test]
    fn a_jsz_reply_carrying_unknown_fields_still_parses() {
        let jsz: Jsz = serde_json::from_str(JSZ_SAMPLE).expect("unknown keys must be ignored");
        assert_eq!(jsz.streams, 8);
        assert_eq!(jsz.account_details.len(), 1);
    }

    /// The transcription hazard: JSZ spells its sequence fields `first_seq` /
    /// `last_seq` / `stream_seq`, where the Rust NATS client spells the same
    /// values `first_sequence` / `last_sequence` / `stream_sequence`. A typo
    /// here parses fine and reports zero forever.
    #[test]
    fn the_sequence_fields_are_read_under_the_names_jsz_actually_uses() {
        let jsz: Jsz = serde_json::from_str(JSZ_SAMPLE).unwrap();
        let stream = &jsz.account_details[0].stream_detail[0];
        assert_eq!(
            stream.state.last_seq, 12,
            "last_seq drives the publish rate"
        );
        let consumer = &stream.consumer_detail[0];
        assert_eq!(consumer.delivered.stream_seq, 9);
        assert_eq!(
            consumer.ack_floor.stream_seq, 7,
            "ack_floor drives the drain rate"
        );
        assert_eq!(consumer.num_pending, 3);
    }

    /// A stream with no consumers omits `consumer_detail` entirely rather than
    /// sending `[]`, so the field has to default to an empty vector.
    #[test]
    fn a_stream_with_no_consumers_omits_the_key_rather_than_sending_an_empty_list() {
        let jsz: Jsz = serde_json::from_str(
            r#"{"account_details":[{"name":"$G","stream_detail":[
                 {"name":"QUIET","state":{"messages":0}}]}]}"#,
        )
        .unwrap();
        let stream = &jsz.account_details[0].stream_detail[0];
        assert!(stream.consumer_detail.is_empty());
        assert_eq!(stream.state.consumer_count, 0);
    }

    /// An empty server is a legitimate answer, not a failure — a fresh install
    /// with JetStream on and nothing created yet looks exactly like this.
    #[test]
    fn a_server_with_no_accounts_parses_as_empty_rather_than_failing() {
        let jsz: Jsz = serde_json::from_str(r#"{"streams":0,"consumers":0}"#).unwrap();
        assert!(jsz.account_details.is_empty());
    }

    #[test]
    fn varz_reads_the_counters_throughput_is_derived_from() {
        let varz: Varz = serde_json::from_str(
            r#"{"server_name":"qfn-nats-1","version":"2.12.6","connections":3,
                "in_msgs":1566,"out_msgs":2886,"in_bytes":206519,"out_bytes":387508,
                "slow_consumers":0,"start":"2026-08-30T16:17:44Z",
                "jetstream":{"stats":{"storage":4096,"api":{"total":216,"errors":9}}}}"#,
        )
        .unwrap();
        assert_eq!(varz.in_msgs, 1566);
        assert_eq!(varz.jetstream.stats.api.errors, 9);
        assert_eq!(
            varz.max_payload, 0,
            "an absent field is zero, not a parse error"
        );
    }

    /// A trailing slash in the configured URL must not produce `//varz`.
    #[test]
    fn a_base_url_with_a_trailing_slash_is_normalised() {
        assert_eq!(
            normalize_base("http://127.0.0.1:8222/"),
            "http://127.0.0.1:8222"
        );
        assert_eq!(normalize_base(" http://nats:8222 "), "http://nats:8222");
    }
}
