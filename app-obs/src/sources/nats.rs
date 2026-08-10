//! Best-effort distribution of app-lb observations over core NATS.
//!
//! This is deliberately not JetStream and never participates in request
//! routing. app-obs retains the durable metric history in parquet; NATS carries
//! current snapshots to status, alerting, and optional queue-fn consumers.

use super::applb::LiveStatus;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub subject: String,
}

#[derive(Debug)]
pub struct Stats {
    subject: String,
    connected: AtomicBool,
    published: AtomicU64,
    failures: AtomicU64,
    last_published_at_ms: AtomicU64,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub configured: bool,
    pub connected: bool,
    pub subject: String,
    pub published: u64,
    pub failures: u64,
    pub last_published_at_ms: Option<u64>,
}

impl Stats {
    pub fn new(subject: String) -> Self {
        Self {
            subject,
            connected: AtomicBool::new(false),
            published: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_published_at_ms: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let last = self.last_published_at_ms.load(Ordering::Relaxed);
        Snapshot {
            configured: true,
            connected: self.connected.load(Ordering::Relaxed),
            subject: self.subject.clone(),
            published: self.published.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            last_published_at_ms: (last > 0).then_some(last),
        }
    }
}

pub struct Publisher {
    config: Config,
    client: Option<async_nats::Client>,
    stats: Arc<Stats>,
}

/// Non-blocking handoff from the HTTP poller to the NATS worker. A watch
/// channel keeps only the newest observation, which is exactly what core NATS
/// telemetry represents and prevents a disconnected broker from backing up the
/// polling and storage path.
#[derive(Clone)]
pub struct Telemetry {
    latest: tokio::sync::watch::Sender<Option<LiveStatus>>,
}

impl Telemetry {
    pub fn publish(&self, status: LiveStatus) {
        self.latest.send_replace(Some(status));
    }
}

impl Publisher {
    pub fn new(config: Config, stats: Arc<Stats>) -> Self {
        Self {
            config,
            client: None,
            stats,
        }
    }

    pub fn spawn(mut self) -> Telemetry {
        let (latest, mut observations) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            while observations.changed().await.is_ok() {
                let status = observations.borrow_and_update().clone();
                if let Some(status) = status {
                    self.publish(&status).await;
                }
            }
        });
        Telemetry { latest }
    }

    async fn connect(&mut self) -> Result<async_nats::Client, String> {
        let mut options = async_nats::ConnectOptions::new().name("app-obs");
        options = if let Some(token) = &self.config.token {
            options.token(token.clone())
        } else if let (Some(user), Some(password)) = (&self.config.user, &self.config.password) {
            options.user_and_password(user.clone(), password.clone())
        } else {
            options
        };
        let stats = self.stats.clone();
        options = options.event_callback(move |event| {
            let stats = stats.clone();
            async move {
                match event {
                    async_nats::Event::Connected => {
                        stats.connected.store(true, Ordering::Relaxed);
                    }
                    async_nats::Event::Disconnected | async_nats::Event::Closed => {
                        stats.connected.store(false, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });
        options
            .connect(self.config.url.as_str())
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn publish(&mut self, status: &LiveStatus) {
        if self.client.is_none() {
            match self.connect().await {
                Ok(client) => {
                    self.stats.connected.store(true, Ordering::Relaxed);
                    self.client = Some(client);
                }
                Err(error) => {
                    self.stats.connected.store(false, Ordering::Relaxed);
                    self.stats.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(%error, "NATS telemetry connection failed");
                    return;
                }
            }
        }

        let payload = match serde_json::to_vec(status) {
            Ok(payload) => payload,
            Err(error) => {
                self.stats.failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%error, "could not serialize NATS telemetry snapshot");
                return;
            }
        };
        let client = self.client.as_ref().expect("client was connected above");
        if client.connection_state() != async_nats::connection::State::Connected {
            self.stats.connected.store(false, Ordering::Relaxed);
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Err(error) = client
            .publish(self.config.subject.clone(), payload.into())
            .await
        {
            self.stats.connected.store(false, Ordering::Relaxed);
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
            self.client = None;
            tracing::warn!(%error, "NATS telemetry publish failed");
            return;
        }

        match tokio::time::timeout(Duration::from_secs(2), client.flush()).await {
            Ok(Ok(())) => {
                // A previous flush may have timed out without forcing an
                // async-nats reconnect event. A confirmed round trip is the
                // authoritative connection signal in that case.
                self.stats.connected.store(true, Ordering::Relaxed);
                self.stats.published.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_published_at_ms
                    .store(status.observed_at_ms.max(0) as u64, Ordering::Relaxed);
            }
            Ok(Err(error)) => {
                self.stats.connected.store(false, Ordering::Relaxed);
                self.stats.failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%error, "NATS telemetry flush failed");
            }
            Err(_) => {
                self.stats.connected.store(false, Ordering::Relaxed);
                self.stats.failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("NATS telemetry flush timed out");
            }
        }
    }
}
