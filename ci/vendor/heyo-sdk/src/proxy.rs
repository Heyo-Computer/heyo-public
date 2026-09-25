//! Minimal iroh P2P proxy **client**, inlined from the `hey-proxy` crate so the
//! SDK is self-contained (publishable to crates.io without a path dependency).
//!
//! This is only the client half: dial a `heyo://` ticket over iroh and forward
//! a local TCP listener to the remote peer. The server side (`hey-proxy`'s
//! `Server`/`ProxyHandler`, relay *registration*, and the bundle `sync` module)
//! stays internal to `local-proxy`.
//!
//! **Wire contract** (must stay in sync with `local-proxy/src/lib.rs`): ALPN
//! `hey-proxy/tcp/0`, one iroh bi-directional stream per accepted TCP
//! connection, raw byte copy in both directions. This contract is intentionally
//! tiny and stable.

use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};
use iroh::endpoint::presets::N0;
use iroh::endpoint::Connection;
use iroh::Endpoint;
use iroh_tickets::{endpoint::EndpointTicket, Ticket};
use serde::Deserialize;
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

/// ALPN for the proxy protocol. Must match `hey-proxy`'s server.
pub(crate) const ALPN: &[u8] = b"hey-proxy/tcp/0";
/// Scheme prefix on every connection ticket.
pub(crate) const TICKET_PREFIX: &str = "heyo://";
/// Env var pointing iroh at a self-hosted DERP relay (e.g.
/// `http://relay.internal:3340`). Mirrors `hey_proxy::RELAY_URL_ENV` — kept in
/// sync deliberately since the SDK is self-contained (no path dep on
/// `local-proxy`). Distinct from the ticket-broker `HEYO_RELAY_URL`.
pub(crate) const RELAY_URL_ENV: &str = "HEYO_IROH_RELAY_URL";

/// Resolve the iroh relay mode from [`RELAY_URL_ENV`]: a custom (self-hosted)
/// relay when set, otherwise iroh's default n0 relays. Malformed URLs fall back
/// to default rather than failing the bind.
fn relay_mode_from_env() -> iroh::RelayMode {
    match std::env::var(RELAY_URL_ENV) {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().parse::<iroh::RelayUrl>() {
            Ok(url) => iroh::RelayMode::Custom(iroh::RelayMap::from(url)),
            Err(_) => iroh::RelayMode::Default,
        },
        _ => iroh::RelayMode::Default,
    }
}

/// Bind an iroh [`Endpoint`] honoring [`RELAY_URL_ENV`].
async fn bind_endpoint() -> Result<Endpoint> {
    Ok(Endpoint::builder(N0)
        .relay_mode(relay_mode_from_env())
        .bind()
        .await?)
}

/// A proxy client connected to a remote peer and listening for local TCP
/// connections. Create with [`Client::connect`], read the bound address with
/// [`Client::local_addr`], then [`Client::run`] to start forwarding.
pub(crate) struct Client {
    conn: Connection,
    listener: TcpListener,
    _endpoint: Endpoint,
}

impl Client {
    /// Resolve the ticket, connect to the remote iroh peer, and bind a local
    /// TCP listener on `listen_port` (use `0` for a random port).
    pub(crate) async fn connect(
        ticket_url: &str,
        listen_port: u16,
        relay_override: Option<&str>,
    ) -> Result<Self> {
        Self::connect_with_host(ticket_url, "127.0.0.1", listen_port, relay_override).await
    }

    /// Like [`connect`](Self::connect) but binds the local listener to an
    /// explicit host (e.g. a host gateway address a VM can reach).
    pub(crate) async fn connect_with_host(
        ticket_url: &str,
        listen_host: &str,
        listen_port: u16,
        relay_override: Option<&str>,
    ) -> Result<Self> {
        let payload = ticket_url
            .strip_prefix(TICKET_PREFIX)
            .ok_or_else(|| anyhow!("connection string must start with {TICKET_PREFIX}"))?;

        let ticket_str = resolve_ticket(payload, relay_override).await?;

        let ticket = <EndpointTicket as Ticket>::decode_string(&ticket_str)
            .map_err(|e| anyhow!("invalid ticket: {e}"))?;
        let remote_addr = ticket.endpoint_addr().clone();

        let endpoint = bind_endpoint().await?;
        // Wait for the endpoint to discover its external address and connect to
        // relay/DERP servers. Without this the QUIC handshake may complete but
        // fail to transfer data (no relay fallback for NAT traversal).
        endpoint.online().await;

        let conn = endpoint
            .connect(remote_addr, ALPN)
            .await
            .context("failed to connect to remote peer")?;

        let listener = TcpListener::bind((listen_host, listen_port))
            .await
            .with_context(|| format!("failed to bind to {listen_host}:{listen_port}"))?;

        Ok(Self {
            conn,
            listener,
            _endpoint: endpoint,
        })
    }

    /// The local TCP address the client is listening on.
    pub(crate) fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept local TCP connections and proxy them to the remote peer. Runs
    /// until the listener hits an unrecoverable error.
    pub(crate) async fn run(self) -> Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (tcp_stream, peer_addr) = accepted?;
                    let conn = self.conn.clone();
                    connections.spawn(async move {
                        if let Err(e) = handle_client_connection(conn, tcp_stream).await {
                            tracing::warn!(%peer_addr, "heyo tunnel: proxied connection failed: {e:#}");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!("heyo tunnel: proxied connection task failed: {error}");
                    }
                }
            }
        }
    }
}

async fn handle_client_connection(conn: Connection, tcp_stream: TcpStream) -> Result<()> {
    // Timeout prevents indefinite hangs when the iroh connection is alive at the
    // QUIC level but the data path is broken (relay issues, NAT re-binding).
    let (iroh_send, iroh_recv) =
        tokio::time::timeout(std::time::Duration::from_secs(15), conn.open_bi())
            .await
            .map_err(|_| anyhow!("timed out opening bi-directional stream (15s)"))?
            .context("failed to open bi-directional stream")?;

    let (tcp_read, tcp_write) = tcp_stream.into_split();
    proxy_streams(iroh_recv, iroh_send, tcp_read, tcp_write).await
}

async fn proxy_streams<IR, IW, TR, TW>(
    mut iroh_recv: IR,
    mut iroh_send: IW,
    mut tcp_read: TR,
    mut tcp_write: TW,
) -> Result<()>
where
    IR: AsyncRead + Unpin,
    IW: AsyncWrite + Unpin,
    TR: AsyncRead + Unpin,
    TW: AsyncWrite + Unpin,
{
    let iroh_to_tcp = async {
        io::copy(&mut iroh_recv, &mut tcp_write).await?;
        tcp_write.shutdown().await
    };
    let tcp_to_iroh = async {
        io::copy(&mut tcp_read, &mut iroh_send).await?;
        iroh_send.shutdown().await
    };
    tokio::pin!(iroh_to_tcp, tcp_to_iroh);

    tokio::select! {
        result = &mut iroh_to_tcp => {
            result.context("iroh-to-TCP copy failed")?;
            tcp_to_iroh.await.context("TCP-to-iroh copy failed")?;
        }
        result = &mut tcp_to_iroh => {
            result.context("TCP-to-iroh copy failed")?;
            iroh_to_tcp.await.context("iroh-to-TCP copy failed")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Error;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncReadExt, ReadBuf};

    #[tokio::test]
    async fn each_eof_closes_the_opposite_writer_and_preserves_asymmetric_response() {
        let (iroh_peer, iroh_proxy) = io::duplex(64);
        let (tcp_peer, tcp_proxy) = io::duplex(64);
        let (iroh_recv, iroh_send) = io::split(iroh_proxy);
        let (tcp_read, tcp_write) = io::split(tcp_proxy);
        let (mut iroh_read, mut iroh_write) = io::split(iroh_peer);
        let (mut tcp_read_peer, mut tcp_write_peer) = io::split(tcp_peer);
        let proxy = tokio::spawn(proxy_streams(iroh_recv, iroh_send, tcp_read, tcp_write));

        tcp_write_peer.write_all(b"short request").await.unwrap();
        tcp_write_peer.shutdown().await.unwrap();
        let mut request = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), iroh_read.read_to_end(&mut request))
            .await.unwrap().unwrap();
        assert_eq!(request, b"short request");

        iroh_write.write_all(b"a much longer response after request EOF").await.unwrap();
        iroh_write.shutdown().await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), tcp_read_peer.read_to_end(&mut response))
            .await.unwrap().unwrap();
        assert_eq!(response, b"a much longer response after request EOF");
        tokio::time::timeout(std::time::Duration::from_secs(1), proxy)
            .await.unwrap().unwrap().unwrap();
    }

    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(self: Pin<&mut Self>, _cx: &mut Context<'_>, _buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(Error::other("injected read failure")))
        }
    }

    #[tokio::test]
    async fn io_error_releases_pending_opposite_half() {
        let (pending_peer, pending_proxy) = io::duplex(64);
        let (pending_read, pending_write) = io::split(pending_proxy);
        let (_unused_peer, unused_proxy) = io::duplex(64);
        let (_unused_read, unused_write) = io::split(unused_proxy);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            proxy_streams(FailingReader, unused_write, pending_read, pending_write),
        ).await.expect("copy error must not wait for the opposite input");
        assert!(result.unwrap_err().to_string().contains("iroh-to-TCP"));

        let (mut pending_peer_read, _) = io::split(pending_peer);
        let mut bytes = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), pending_peer_read.read_to_end(&mut bytes))
            .await.expect("error must release the pending writer").unwrap();
    }
}

// ── Relay ticket resolution (client-side lookup only) ──

#[derive(Deserialize)]
struct RelayLookupResponse {
    ticket: String,
}

/// Look up a full ticket string from the relay server by short code.
async fn lookup_from_relay(relay_url: &str, code: &str) -> Result<String> {
    let base = relay_url.trim_end_matches('/');
    let resp: RelayLookupResponse = reqwest::Client::new()
        .get(format!("{base}/api/lookup/{code}"))
        .send()
        .await
        .context("failed to contact relay server")?
        .error_for_status()
        .context("ticket not found on relay server")?
        .json()
        .await?;
    Ok(resp.ticket)
}

/// Resolve a ticket payload (the part after `heyo://`) to a full serialized
/// ticket string. Supports:
///   - `host:port/short-code` — relay-backed short URL (auto `http://host:port`)
///   - `short-code` (< 64 chars) — bare code, requires `relay_override`
///   - long base32 string — raw ticket, returned as-is
async fn resolve_ticket(payload: &str, relay_override: Option<&str>) -> Result<String> {
    if let Some((authority, code)) = payload.split_once('/') {
        let relay_url = relay_override
            .map(String::from)
            .unwrap_or_else(|| format!("http://{authority}"));
        lookup_from_relay(&relay_url, code).await
    } else if payload.len() < 64 {
        let relay_url =
            relay_override.ok_or_else(|| anyhow!("short code requires a relay URL"))?;
        lookup_from_relay(relay_url, payload).await
    } else {
        Ok(payload.to_string())
    }
}
