//! Process configuration, supplied entirely by environment variables.
//!
//! Mirrors app-obs (`app-obs/src/config.rs`) and app-lb: no config file, no CLI
//! arguments, so a supervisor unit is the single source of truth for how a host
//! runs this.

use std::time::Duration;

/// nats-server's monitoring listener. Loopback because that is where it belongs
/// — `/varz`, `/connz` and `/jsz` take no credential, so the port is protected
/// by being unreachable rather than by auth. This process is the thing that
/// crosses that line, once, on purpose.
fn default_monitor_url() -> String {
    "http://127.0.0.1:8222".into()
}

/// The dashboard binds loopback for the same reason app-obs' does: it is meant
/// to be reached through app-lb, which terminates TLS and puts a sign-in in
/// front of it. 9700 is unclaimed by the rest of the fleet (app-lb 9090,
/// app-obs 9500/9514/9600).
fn default_api_addr() -> String {
    "127.0.0.1:9700".into()
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of nats-server's HTTP monitoring port (`http:` in
    /// nats-server.conf). Path-less: this appends `/varz`, `/connz`, `/jsz`.
    pub monitor_url: String,
    pub api_addr: String,
    /// Shared secret required on the dashboard and its JSON. `None` leaves them
    /// open, which is only reasonable while the listener is on loopback behind
    /// app-lb's gate. `/healthz` stays open either way.
    pub api_token: Option<String>,
    /// How often the three monitoring endpoints are scraped.
    ///
    /// This is also the resolution of every rate on the page: `/varz` reports
    /// cumulative counters, so throughput is a difference between two scrapes
    /// and cannot be finer than the gap between them.
    pub poll_interval: Duration,
    /// Samples kept for the charts. At the default 5s poll that is an hour of
    /// history — enough to see a burst you were paged for, and small enough
    /// that the whole thing is memory the process can lose on restart without
    /// anyone minding.
    pub history_points: usize,
    /// nats-server's log file (`logfile:` in its config, or the file a
    /// supervisor captures its stdout into). Unset disables log collection —
    /// the safe default, because a wrong path would otherwise be a panel that
    /// silently stays empty.
    pub log_file: Option<String>,
    /// Log lines held in memory. The oldest is dropped when the buffer is full.
    pub log_lines: usize,
    /// How much of an existing log file to read on startup, so the panel has
    /// content before nats-server next says anything. A quiet, healthy server
    /// can go hours without logging a line.
    pub log_prime_bytes: u64,
    /// Ceiling on the client list pulled from `/connz`. A server at its
    /// `max_connections` should make this page slow to read, not slow to serve.
    pub max_clients: usize,
    /// Deadline on a single monitoring request. Comfortably under
    /// `poll_interval`, so a hung scrape cannot stack up behind the next one.
    pub request_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            monitor_url: default_monitor_url(),
            api_addr: default_api_addr(),
            api_token: None,
            poll_interval: Duration::from_secs(5),
            history_points: 720,
            log_file: None,
            log_lines: 2_000,
            log_prime_bytes: 64 * 1024,
            max_clients: 256,
            request_timeout: Duration::from_secs(4),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("QUEUE_NATS_MONITOR_URL") {
            cfg.monitor_url = v;
        }
        if let Ok(v) = std::env::var("QUEUE_API_ADDR") {
            cfg.api_addr = v;
        }
        if let Ok(v) = std::env::var("QUEUE_API_TOKEN")
            && !v.is_empty()
        {
            cfg.api_token = Some(v);
        }
        if let Ok(v) = std::env::var("QUEUE_NATS_LOG_FILE")
            && !v.trim().is_empty()
        {
            cfg.log_file = Some(v);
        }
        if let Some(v) = parse_env("QUEUE_POLL_SECS") {
            cfg.poll_interval = Duration::from_secs(v);
        }
        if let Some(v) = parse_env("QUEUE_HISTORY_POINTS") {
            cfg.history_points = v;
        }
        if let Some(v) = parse_env("QUEUE_LOG_LINES") {
            cfg.log_lines = v;
        }
        if let Some(v) = parse_env("QUEUE_LOG_PRIME_BYTES") {
            cfg.log_prime_bytes = v;
        }
        if let Some(v) = parse_env("QUEUE_MAX_CLIENTS") {
            cfg.max_clients = v;
        }
        if let Some(v) = parse_env("QUEUE_REQUEST_TIMEOUT_SECS") {
            cfg.request_timeout = Duration::from_secs(v);
        }
        cfg.clamp()
    }

    /// Fix values that would make the process useless rather than refusing to
    /// start over them.
    ///
    /// A dashboard is not worth a crash loop, and every one of these has an
    /// obviously correct floor: a zero poll interval is a hot loop against
    /// somebody else's server, a zero-length history is a chart with nothing to
    /// draw, and a request timeout at or above the poll interval lets scrapes
    /// overlap. Each says so once at startup so the operator can see their
    /// value was not the one used.
    fn clamp(mut self) -> Self {
        if self.poll_interval.is_zero() {
            tracing::warn!("QUEUE_POLL_SECS=0 would spin the monitoring port; using 1s");
            self.poll_interval = Duration::from_secs(1);
        }
        if self.history_points == 0 {
            tracing::warn!("QUEUE_HISTORY_POINTS=0 leaves the charts empty; using 2");
            self.history_points = 2;
        }
        if self.log_lines == 0 {
            tracing::warn!("QUEUE_LOG_LINES=0 discards every line; using 1");
            self.log_lines = 1;
        }
        if self.request_timeout >= self.poll_interval {
            // Integer arithmetic rather than a float multiply: `mul_f32(0.8)`
            // on one second lands on 800.000012ms, which is a fine timeout and
            // an impossible thing to assert on.
            let capped = (self.poll_interval * 4 / 5).max(Duration::from_millis(500));
            tracing::warn!(
                timeout_secs = self.request_timeout.as_secs(),
                poll_secs = self.poll_interval.as_secs(),
                "request timeout is not under the poll interval; capping it so scrapes cannot overlap",
            );
            self.request_timeout = capped;
        }
        self
    }
}

/// Parse a numeric env var, warning rather than failing on garbage: a typo in
/// one tuning knob should not stop the dashboard from starting, since the
/// default it falls back to is a working value.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_timeout_at_the_poll_interval_is_capped_below_it() {
        let cfg = Config {
            poll_interval: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            ..Default::default()
        }
        .clamp();
        assert!(
            cfg.request_timeout < cfg.poll_interval,
            "a scrape allowed to run for {:?} on a {:?} tick would overlap the next one",
            cfg.request_timeout,
            cfg.poll_interval,
        );
    }

    #[test]
    fn a_zero_poll_interval_becomes_a_second_rather_than_a_hot_loop() {
        let cfg = Config {
            poll_interval: Duration::ZERO,
            ..Default::default()
        }
        .clamp();
        assert_eq!(cfg.poll_interval, Duration::from_secs(1));
    }

    /// The floor has to leave a usable timeout even on a 1s tick, or clamping
    /// one bad value would produce a second one.
    #[test]
    fn clamping_a_fast_poll_still_leaves_time_to_answer() {
        let cfg = Config {
            poll_interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(10),
            ..Default::default()
        }
        .clamp();
        assert_eq!(cfg.request_timeout, Duration::from_millis(800));
    }
}
