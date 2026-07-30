//! Process configuration, supplied entirely by environment variables.
//!
//! Mirrors app-lb's approach (`app-lb/src/main.rs`): no config file, no CLI
//! arguments, so a supervisor unit is the single source of truth.

use std::time::Duration;

/// Where partitioned parquet is written.
fn default_data_dir() -> String {
    "/var/lib/app-obs/data".into()
}

/// Ingest binds all interfaces on purpose. Every microVM sits on its own /30
/// with the host at `guest_ip - 1`, so there is no single address that works
/// for every guest — they each reach us on their own tap gateway. See the
/// module docs in `ingest`.
fn default_ingest_addr() -> String {
    "0.0.0.0:9500".into()
}

fn default_syslog_addr() -> String {
    "0.0.0.0:9514".into()
}

/// The API and dashboard stay on loopback: they are meant to be reached through
/// app-lb, which terminates TLS and can gate them behind its admin auth.
fn default_api_addr() -> String {
    "127.0.0.1:9600".into()
}

fn default_applb_url() -> String {
    "http://127.0.0.1:9090".into()
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: String,
    pub ingest_addr: String,
    pub syslog_addr: String,
    pub api_addr: String,
    /// Shared secret required on `POST /ingest`. Unset leaves ingest open,
    /// which is only reasonable when the listener is unreachable from outside
    /// the tap networks.
    pub ingest_token: Option<String>,
    /// app-lb's admin API, polled for metrics.
    pub applb_url: String,
    /// Credentials for app-lb when `APP_LB_ADMIN_AUTH` is on there.
    pub applb_user: Option<String>,
    pub applb_password: Option<String>,
    pub poll_interval: Duration,
    /// Delete partitions older than this.
    pub retain_days: u32,
    /// Flush a partition's buffer once it reaches this many rows...
    pub flush_rows: usize,
    /// ...or this long since its first row, whichever comes first. Bounds how
    /// long a low-volume deployment's logs sit unqueryable in memory.
    pub flush_interval: Duration,
    /// Bound on the ingest queue. Full means records are dropped and counted —
    /// never blocked. A collector falling behind must not turn into
    /// backpressure on somebody's application.
    pub queue_capacity: usize,
    /// Dashboard queries allowed in flight at once. Reading parquet is CPU-bound
    /// work on the same runtime that accepts ingest, so this is a limit on how
    /// much of the machine looking at the data may take from collecting it.
    pub query_concurrency: usize,
    /// Ceiling on a single query. A dashboard that gets a 504 is a nuisance; one
    /// that pins a core for ten minutes is a problem.
    pub query_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            ingest_addr: default_ingest_addr(),
            syslog_addr: default_syslog_addr(),
            api_addr: default_api_addr(),
            ingest_token: None,
            applb_url: default_applb_url(),
            applb_user: None,
            applb_password: None,
            poll_interval: Duration::from_secs(10),
            retain_days: 30,
            flush_rows: 10_000,
            flush_interval: Duration::from_secs(60),
            queue_capacity: 65_536,
            query_concurrency: 4,
            query_timeout: Duration::from_secs(30),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("APP_OBS_DATA_DIR") {
            cfg.data_dir = v;
        }
        if let Ok(v) = std::env::var("APP_OBS_INGEST_ADDR") {
            cfg.ingest_addr = v;
        }
        if let Ok(v) = std::env::var("APP_OBS_SYSLOG_ADDR") {
            cfg.syslog_addr = v;
        }
        if let Ok(v) = std::env::var("APP_OBS_API_ADDR") {
            cfg.api_addr = v;
        }
        if let Ok(v) = std::env::var("APP_OBS_INGEST_TOKEN") {
            cfg.ingest_token = Some(v);
        }
        if let Ok(v) = std::env::var("APP_LB_URL") {
            cfg.applb_url = v;
        }
        if let Ok(v) = std::env::var("APP_LB_USER") {
            cfg.applb_user = Some(v);
        }
        if let Ok(v) = std::env::var("APP_LB_PASSWORD") {
            cfg.applb_password = Some(v);
        }
        if let Some(v) = parse_env("APP_OBS_POLL_SECS") {
            cfg.poll_interval = Duration::from_secs(v);
        }
        if let Some(v) = parse_env("APP_OBS_RETAIN_DAYS") {
            cfg.retain_days = v;
        }
        if let Some(v) = parse_env("APP_OBS_FLUSH_ROWS") {
            cfg.flush_rows = v;
        }
        if let Some(v) = parse_env("APP_OBS_FLUSH_SECS") {
            cfg.flush_interval = Duration::from_secs(v);
        }
        if let Some(v) = parse_env("APP_OBS_QUEUE_CAPACITY") {
            cfg.queue_capacity = v;
        }
        if let Some(v) = parse_env("APP_OBS_QUERY_CONCURRENCY") {
            cfg.query_concurrency = v;
        }
        if let Some(v) = parse_env("APP_OBS_QUERY_TIMEOUT_SECS") {
            cfg.query_timeout = Duration::from_secs(v);
        }
        cfg
    }
}

/// Parse a numeric env var, warning rather than failing on garbage: a typo in
/// one tuning knob shouldn't stop the collector from starting and losing data
/// while somebody notices.
fn parse_env<T: std::str::FromStr>(key: &str) -> Option<T> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().parse() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(key, value = %raw, "ignoring unparseable value; using the default");
            None
        }
    }
}
