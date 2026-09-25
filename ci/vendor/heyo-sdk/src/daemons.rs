//! User-owned heyvmd daemons (Mac mini, lab box, anywhere the user runs the
//! `heyvmd` networking sidecar) registered with the cloud. Mirrors
//! `sdk-ts/src/daemons.ts`.
//!
//! Each daemon advertises an iroh tunnel back to the local heyvm HTTP API; the
//! cloud proxies SDK calls into it, so once a daemon is registered the SDK
//! surface (`Sandbox`, `commands().run`, `sandbox.shell()`, file I/O) works
//! against sandboxes on that daemon exactly as it does for cloud-hosted ones.
//!
//! For a direct P2P data path to a daemon (bypassing the cloud), pair this with
//! [`HeyoClient::connect_p2p`](crate::HeyoClient::connect_p2p) or
//! [`P2pTunnel`](crate::P2pTunnel) using the daemon's connection ticket.
//!
//! ```no_run
//! use heyo_sdk::{Daemons, HeyoClientOptions};
//! # async fn run() -> Result<(), heyo_sdk::HeyoError> {
//! let daemons = Daemons::list(HeyoClientOptions::default()).await?;
//! if let Some(home) = daemons.iter().find(|d| d.name.as_deref() == Some("homelab")) {
//!     println!("{} is {:?}", home.id, home.status);
//! }
//! # Ok(()) }
//! ```

use reqwest::Method;
use serde::Deserialize;

use crate::client::{HeyoClient, HeyoClientOptions, RequestOptions};
use crate::commands::encode_path;
use crate::errors::HeyoError;

/// Status the cloud reports for a registered daemon. Derived from the cloud's
/// stale-host watcher: `Online` means a fresh heartbeat within ~3 minutes,
/// `Stale` means missed heartbeats, `Offline` means the cloud has flipped the
/// row away from `available`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonStatus {
    Online,
    Stale,
    Offline,
}

/// A daemon the authenticated user has registered with the cloud.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    /// Stable id assigned when the daemon first registered (`hd-…`).
    pub id: String,
    /// Human-readable label set by the daemon (defaults to hostname).
    #[serde(default)]
    pub name: Option<String>,
    pub status: DaemonStatus,
    /// RFC 3339 timestamp of the most recent heartbeat.
    pub last_seen_at: String,
    /// RFC 3339 timestamp from initial registration.
    pub created_at: String,
}

#[derive(Deserialize)]
struct DaemonsEnvelope {
    #[serde(default)]
    daemons: Vec<DaemonInfo>,
}

/// Static API for managing the caller's registered daemons.
pub struct Daemons;

impl Daemons {
    /// List every daemon the authenticated user has registered.
    pub async fn list(client_options: HeyoClientOptions) -> Result<Vec<DaemonInfo>, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let env: DaemonsEnvelope = client
            .request(Method::GET, "/me/daemons", None::<&()>, RequestOptions::default())
            .await?;
        Ok(env.daemons)
    }

    /// Fetch a single daemon. Returns [`HeyoError::NotFound`] if the caller owns
    /// no such daemon.
    pub async fn get(
        id: &str,
        client_options: HeyoClientOptions,
    ) -> Result<DaemonInfo, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let path = format!("/me/daemons/{}", encode_path(id));
        client
            .request(Method::GET, &path, None::<&()>, RequestOptions::default())
            .await
    }

    /// Unregister a daemon. Idempotent on 404 — passing an unknown id is a no-op,
    /// so reconciliation loops can call this freely. Existing sandboxes the daemon
    /// hosts are *not* deleted; they become unreachable until the daemon
    /// re-registers (or another daemon claims the same id).
    pub async fn delete(id: &str, client_options: HeyoClientOptions) -> Result<(), HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let path = format!("/me/daemons/{}", encode_path(id));
        match client
            .request::<serde_json::Value>(
                Method::DELETE,
                &path,
                None::<&()>,
                RequestOptions::default(),
            )
            .await
        {
            Ok(_) => Ok(()),
            // Swallow 404 — caller likely already removed it.
            Err(HeyoError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}
