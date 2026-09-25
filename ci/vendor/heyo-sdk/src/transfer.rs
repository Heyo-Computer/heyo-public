//! Receiving a transferred VM. The *receive* half of `heyvm transfer` — the
//! only part of the P2P VM move reachable over the daemon's HTTP API (the
//! source half packages a live sandbox and serves it over iroh, which needs
//! in-process access to the local `SandboxManager` and stays inside the CLI).
//!
//! A sender runs `heyvm transfer <vm> --serve` (or the full
//! `heyvm transfer <vm> --to <daemon>`) and produces a `heyo://` ticket for its
//! iroh sync server. Handing that ticket to [`Transfer::receive`] asks the
//! daemon behind the [`HeyoClient`] to pull the bundle and restore it as a new
//! sandbox — the destination side of the move.
//!
//! Because these routes live on the heyvm daemon (not the cloud), point the
//! client at a daemon: [`HeyoClient::local`](crate::HeyoClient::local),
//! [`HeyoClient::connect_p2p`](crate::HeyoClient::connect_p2p), or a
//! cloud-proxied daemon client. A default cloud client will 404.
//!
//! ```no_run
//! use heyo_sdk::{Transfer, ReceiveOptions, HeyoClientOptions};
//! use std::time::Duration;
//! # async fn run(ticket: &str) -> Result<(), heyo_sdk::HeyoError> {
//! // Target the receiving daemon (here, a same-machine heyvmd).
//! let opts = HeyoClientOptions { base_url: Some("http://127.0.0.1:34099".into()), ..Default::default() };
//! let status = Transfer::receive_and_wait(
//!     ticket,
//!     ReceiveOptions::default(),
//!     Duration::from_secs(600),
//!     opts,
//! ).await?;
//! println!("restored as {:?} (memory: {:?})", status.restored_id, status.memory_restored);
//! # Ok(()) }
//! ```

use std::time::{Duration, Instant};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::client::{HeyoClient, HeyoClientOptions, RequestOptions};
use crate::commands::encode_path;
use crate::errors::HeyoError;

/// How long [`Transfer::receive_and_wait`] waits between progress polls.
const RECEIVE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Options for [`Transfer::receive`]. [`Default`] mirrors the daemon's
/// defaults: start the restored sandbox, degrade to disk-only if the memory
/// snapshot cannot be loaded.
#[derive(Debug, Clone)]
pub struct ReceiveOptions {
    /// Name override for the restored sandbox. Defaults to the bundle's name.
    pub name: Option<String>,
    /// Backend override (`firecracker`, `kvm`, `applevirt`, …). Defaults to the
    /// bundle's source backend.
    pub backend: Option<String>,
    /// Start the sandbox once it is restored. Default: `true`.
    pub start_after: bool,
    /// Fail the receive (and destroy the restored sandbox) if the bundle's
    /// memory snapshot could not be loaded, instead of silently degrading to a
    /// disk-only restore. Default: `false`.
    pub require_memory: bool,
    /// Iroh relay override for resolving a short-code ticket. Defaults to the
    /// daemon's configured relay.
    pub relay: Option<String>,
}

impl Default for ReceiveOptions {
    fn default() -> Self {
        Self {
            name: None,
            backend: None,
            start_after: true,
            require_memory: false,
            relay: None,
        }
    }
}

#[derive(Serialize)]
struct ReceiveRequest<'a> {
    ticket: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<&'a str>,
    start_after: bool,
    require_memory: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    relay: Option<&'a str>,
}

#[derive(Deserialize)]
struct ReceiveAccepted {
    receive_id: String,
}

/// Lifecycle phase of an inbound transfer, from `GET /sync/receives/:id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferStatus {
    /// Pulling blobs from the sender's iroh sync server.
    Pulling,
    /// Bundle received; materializing disks and (optionally) memory state.
    Restoring,
    /// Restore finished; `restored_id` names the new sandbox.
    Done,
    /// The receive failed; `error` carries the reason.
    Error,
    /// A phase this SDK build does not recognize (forward-compatible).
    #[serde(other)]
    Unknown,
}

impl TransferStatus {
    /// Whether the transfer has reached a terminal phase (`Done` or `Error`).
    pub fn is_terminal(self) -> bool {
        matches!(self, TransferStatus::Done | TransferStatus::Error)
    }
}

/// Progress of an inbound transfer (`GET /sync/receives/:id`).
#[derive(Debug, Clone, Deserialize)]
pub struct TransferReceiveStatus {
    /// Receive id (`rcv-…`) this status belongs to.
    pub receive_id: String,
    /// Staging bundle id (`bnd-…`) on the destination.
    pub bundle_id: String,
    /// Current phase.
    pub status: TransferStatus,
    /// Id of the restored sandbox, once `status` is `Done`.
    #[serde(default)]
    pub restored_id: Option<String>,
    /// Whether the memory snapshot was restored (vs. a disk-only cold boot).
    /// `None` until the restore resolves.
    #[serde(default)]
    pub memory_restored: Option<bool>,
    /// Failure reason, set when `status` is `Error`.
    #[serde(default)]
    pub error: Option<String>,
    /// Bytes pulled from the sender so far.
    #[serde(default)]
    pub bytes_received: u64,
}

/// Static API for receiving a transferred VM onto the daemon a client targets.
pub struct Transfer;

impl Transfer {
    /// Ask the daemon behind `client_options` to pull and restore a VM from a
    /// `heyo://` transfer `ticket`. The pull + restore run in the background on
    /// the daemon; this returns the `receive_id` immediately — poll
    /// [`Transfer::status`] (or use [`Transfer::receive_and_wait`]) to follow it.
    pub async fn receive(
        ticket: &str,
        options: ReceiveOptions,
        client_options: HeyoClientOptions,
    ) -> Result<String, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        Self::receive_with(&client, ticket, &options).await
    }

    /// Progress of a previously started receive. Returns [`HeyoError::NotFound`]
    /// if the daemon has no such receive id (they are in-memory and do not
    /// survive a daemon restart).
    pub async fn status(
        receive_id: &str,
        client_options: HeyoClientOptions,
    ) -> Result<TransferReceiveStatus, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        Self::status_with(&client, receive_id).await
    }

    /// Start a receive and poll until it finishes. On success returns the final
    /// status (inspect `restored_id` / `memory_restored`); a receive that ends
    /// in the `Error` phase is surfaced as [`HeyoError::Api`], and exceeding
    /// `timeout` as [`HeyoError::Timeout`] (the daemon-side receive keeps
    /// running — re-attach with [`Transfer::status`]).
    pub async fn receive_and_wait(
        ticket: &str,
        options: ReceiveOptions,
        timeout: Duration,
        client_options: HeyoClientOptions,
    ) -> Result<TransferReceiveStatus, HeyoError> {
        let client = HeyoClient::new(client_options)?;
        let receive_id = Self::receive_with(&client, ticket, &options).await?;
        let deadline = Instant::now() + timeout;
        loop {
            let status = Self::status_with(&client, &receive_id).await?;
            match status.status {
                TransferStatus::Done => return Ok(status),
                TransferStatus::Error => {
                    return Err(HeyoError::api(
                        0,
                        format!(
                            "transfer receive {} failed: {}",
                            receive_id,
                            status.error.as_deref().unwrap_or("no reason reported")
                        ),
                    ));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(HeyoError::Timeout(
                    timeout,
                    format!("transfer receive {} did not finish", receive_id),
                ));
            }
            sleep(RECEIVE_POLL_INTERVAL).await;
        }
    }

    async fn receive_with(
        client: &HeyoClient,
        ticket: &str,
        options: &ReceiveOptions,
    ) -> Result<String, HeyoError> {
        let body = ReceiveRequest {
            ticket,
            name: options.name.as_deref(),
            backend: options.backend.as_deref(),
            start_after: options.start_after,
            require_memory: options.require_memory,
            relay: options.relay.as_deref(),
        };
        let accepted: ReceiveAccepted = client
            .request(Method::POST, "/sync/receive", Some(&body), RequestOptions::default())
            .await?;
        Ok(accepted.receive_id)
    }

    async fn status_with(
        client: &HeyoClient,
        receive_id: &str,
    ) -> Result<TransferReceiveStatus, HeyoError> {
        let path = format!("/sync/receives/{}", encode_path(receive_id));
        client
            .request(Method::GET, &path, None::<&()>, RequestOptions::default())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_request_omits_unset_options_and_keeps_flags() {
        let opts = ReceiveOptions::default();
        let body = ReceiveRequest {
            ticket: "heyo://abc",
            name: opts.name.as_deref(),
            backend: opts.backend.as_deref(),
            start_after: opts.start_after,
            require_memory: opts.require_memory,
            relay: opts.relay.as_deref(),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["ticket"], "heyo://abc");
        // Defaults: start the sandbox, don't require memory.
        assert_eq!(v["start_after"], true);
        assert_eq!(v["require_memory"], false);
        // Unset options never reach the wire.
        assert!(v.get("name").is_none());
        assert!(v.get("backend").is_none());
        assert!(v.get("relay").is_none());
    }

    #[test]
    fn status_decodes_done_row() {
        let json = r#"{
            "receive_id":"rcv-1234abcd","bundle_id":"bnd-5678ef01",
            "status":"done","restored_id":"sb-99","memory_restored":true,
            "error":null,"bytes_received":1048576
        }"#;
        let s: TransferReceiveStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.status, TransferStatus::Done);
        assert!(s.status.is_terminal());
        assert_eq!(s.restored_id.as_deref(), Some("sb-99"));
        assert_eq!(s.memory_restored, Some(true));
        assert_eq!(s.bytes_received, 1_048_576);
    }

    #[test]
    fn status_tolerates_partial_pulling_row() {
        // Mid-flight rows omit restored_id/memory_restored; bytes may be absent.
        let json = r#"{"receive_id":"rcv-1","bundle_id":"bnd-1","status":"pulling"}"#;
        let s: TransferReceiveStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.status, TransferStatus::Pulling);
        assert!(!s.status.is_terminal());
        assert!(s.restored_id.is_none());
        assert_eq!(s.bytes_received, 0);
    }

    #[test]
    fn status_maps_unknown_phase() {
        let json = r#"{"receive_id":"rcv-1","bundle_id":"bnd-1","status":"verifying"}"#;
        let s: TransferReceiveStatus = serde_json::from_str(json).unwrap();
        assert_eq!(s.status, TransferStatus::Unknown);
    }
}
