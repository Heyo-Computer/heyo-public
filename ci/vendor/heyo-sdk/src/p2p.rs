//! Generic P2P (iroh) TCP/HTTP tunnel to a sandbox-shared service port.
//!
//! Unlike [`HeyoClient::connect_p2p`](crate::HeyoClient::connect_p2p), which
//! speaks the heyvm daemon HTTP API over the tunnel, [`P2pTunnel`] is
//! protocol-agnostic: it binds a local TCP listener and forwards every
//! connection to the remote peer's shared port over iroh. Point any HTTP/TCP
//! client at [`local_url`](P2pTunnel::local_url) / [`local_port`](P2pTunnel::local_port).
//!
//! The remote side is a `heyvm proxy start <port>` (or `heyvm share`) endpoint
//! that exposes a TCP service over iroh and hands out a `heyo://` ticket.
//!
//! ```no_run
//! use heyo_sdk::P2pTunnel;
//!
//! # async fn run() -> Result<(), heyo_sdk::HeyoError> {
//! let tunnel = P2pTunnel::connect("heyo://…ticket…", None).await?;
//! // The shared service is now reachable locally; tunnel lives until dropped.
//! let url = tunnel.local_url(); // e.g. "http://127.0.0.1:54321"
//! # let _ = url;
//! # Ok(()) }
//! ```

use crate::errors::HeyoError;

/// A live P2P tunnel: a local TCP listener forwarding to a remote peer's TCP
/// service over iroh. The background forwarding task is aborted on `Drop`,
/// which closes the local listener — hold this value for as long as the tunnel
/// is needed.
pub struct P2pTunnel {
    local_port: u16,
    _guard: TunnelGuard,
}

/// Owns the background task pumping the iroh tunnel. Dropping it aborts the
/// task (and so closes the local TCP listener the requests were pointed at).
struct TunnelGuard(tokio::task::JoinHandle<()>);

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl P2pTunnel {
    /// Resolve `ticket` (a `heyo://` URL or a relay shortname), connect to the
    /// remote peer over iroh, bind a local TCP listener on an OS-assigned port,
    /// and start forwarding in the background. `relay` overrides the iroh relay
    /// used to resolve short tickets (pass `None` for full base32 tickets).
    ///
    /// The chosen local port is available immediately via [`local_port`](Self::local_port)
    /// — no need to wait for or parse any output.
    pub async fn connect(ticket: &str, relay: Option<&str>) -> Result<Self, HeyoError> {
        let proxy = crate::proxy::Client::connect(ticket, 0, relay)
            .await
            .map_err(|e| HeyoError::Connection(format!("iroh P2P connect failed: {e}")))?;
        let local = proxy
            .local_addr()
            .map_err(|e| HeyoError::Connection(format!("tunnel local_addr: {e}")))?;
        let local_port = local.port();
        let handle = tokio::spawn(async move {
            // Runs until the listener errors or the task is aborted on Drop. A
            // failure here just means later requests get a connection error,
            // which surfaces to the caller naturally.
            let _ = proxy.run().await;
        });
        Ok(Self {
            local_port,
            _guard: TunnelGuard(handle),
        })
    }

    /// Like [`connect`](Self::connect) but binds the local TCP listener to an
    /// explicit host instead of `127.0.0.1`. Pass `0.0.0.0` to expose the
    /// tunnel to other machines on the LAN — this is what the `paper-bridge`
    /// uses so an embedded HTTP client (CrossInk) can reach a sandbox's HTTP
    /// API without speaking iroh itself. Use `0` for `port` to let the OS
    /// assign one; the chosen port is then available via [`local_port`](Self::local_port).
    pub async fn connect_on_host(
        ticket: &str,
        host: &str,
        port: u16,
        relay: Option<&str>,
    ) -> Result<Self, HeyoError> {
        let proxy = crate::proxy::Client::connect_with_host(ticket, host, port, relay)
            .await
            .map_err(|e| HeyoError::Connection(format!("iroh P2P connect failed: {e}")))?;
        let local = proxy
            .local_addr()
            .map_err(|e| HeyoError::Connection(format!("tunnel local_addr: {e}")))?;
        let local_port = local.port();
        let handle = tokio::spawn(async move {
            let _ = proxy.run().await;
        });
        Ok(Self {
            local_port,
            _guard: TunnelGuard(handle),
        })
    }

    /// The local TCP port the tunnel is listening on (bound to `127.0.0.1`).
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// `http://127.0.0.1:<port>` — point an HTTP client here to ride the tunnel.
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}
