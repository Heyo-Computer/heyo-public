//! Raw byte splice between a client and its schema's VM Postgres.

use std::time::Duration;

use anyhow::{Context, Result};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, copy_bidirectional};
use tokio::net::TcpStream;
use tracing::warn;

use crate::registry::SchemaEntry;

/// Idle time on a spliced socket before the first keepalive probe goes out.
/// Short enough that a slot held by a vanished client comes back inside a
/// scale-down cycle, long enough to cost nothing on a healthy connection (one
/// packet a minute on an idle session).
const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);

/// Gap between probes once one goes unanswered.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Unanswered probes before the socket is failed — 60s + 3x10s, so a dead peer
/// is declared dead in ~90s.
const KEEPALIVE_RETRIES: u32 = 3;

/// Ceiling on how long *unacknowledged* data may sit before the socket errors
/// (Linux `TCP_USER_TIMEOUT`). Keepalive probes only run on an idle
/// connection, so they don't cover a peer that dies mid-result-set with bytes
/// still in flight: that path is governed by retransmission, which takes ~15
/// minutes (`tcp_retries2` = 15) to give up. Matching the keepalive budget
/// makes both ways of dying cost the same ~90s.
#[cfg(target_os = "linux")]
const KEEPALIVE_USER_TIMEOUT: Duration = Duration::from_secs(90);

/// Arm TCP keepalive (plus `TCP_USER_TIMEOUT` on Linux) on one leg of a splice.
///
/// `copy_bidirectional` below returns only once *both* directions have seen
/// EOF, and the `ConnGuard` holding this connection's admission permit is
/// dropped only when it returns. A client that disappears without sending a
/// FIN — a SIGKILLed pod, a preempted node, a NAT or load balancer silently
/// dropping an idle flow — leaves an ESTABLISHED socket that never reads EOF
/// and never errors, because the kernel does not time out an idle established
/// connection unless keepalive is on. The splice task then blocks forever, its
/// permit is never returned, and the schema's client slots drain away one dead
/// client at a time until every new client queues against permits held by
/// sockets whose peer no longer exists. On a keep-alive schema nothing
/// recovers from that short of restarting the pooler: the entry — and with it
/// the semaphore — is exempt from every eviction tier, and the leaked `active`
/// count blocks the rest anyway.
///
/// Probing converts that silence into an error the splice can return from, so
/// the guard drops and the slot goes back. Armed on both legs: the guest's
/// backend is just as stuck when the pooler is the half that disappears.
///
/// Best-effort, like `TCP_NODELAY` — a failed sockopt costs the leak
/// protection, and must not drop an otherwise healthy connection.
pub(crate) fn arm_keepalive(sock: &TcpStream, leg: &str) {
    let sock = SockRef::from(sock);
    let ka = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_RETRIES);
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        warn!("could not set TCP keepalive on the {leg} leg: {e}");
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = sock.set_tcp_user_timeout(Some(KEEPALIVE_USER_TIMEOUT)) {
        warn!("could not set TCP_USER_TIMEOUT on the {leg} leg: {e}");
    }
}

/// Dial the schema's VM Postgres (guest IP directly, or the tunnel's local end),
/// replay the buffered StartupMessage, then pipe both directions until *both*
/// close — `copy_bidirectional` shuts down the opposing write half on the first
/// EOF but keeps waiting for the second, so one side closing is not enough to
/// end the splice (and release the slot). Generic over the client stream:
/// plain TCP or the TLS-upgraded
/// stream from `read_startup` (upstream is always plaintext — TLS terminates
/// at the pooler).
pub async fn splice<C>(mut client: C, entry: &SchemaEntry, startup_raw: &[u8]) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut upstream = TcpStream::connect(entry.target)
        .await
        .with_context(|| format!("connecting to VM Postgres at {}", entry.target))?;

    // Disable Nagle on the pooler->VM leg. Postgres and libpq both set
    // TCP_NODELAY on their own sockets precisely because the wire protocol is
    // request/response; a byte-splicing proxy in the middle that leaves Nagle
    // on reintroduces the per-round-trip (Nagle + delayed-ACK) latency the
    // endpoints were avoiding, which is what makes a chatty many-small-statement
    // client slower through the pooler than on a direct connection. Best-effort:
    // a failed sockopt must not drop an otherwise-healthy connection.
    if let Err(e) = upstream.set_nodelay(true) {
        warn!(
            "could not set TCP_NODELAY on upstream to {}: {e}",
            entry.target
        );
    }
    arm_keepalive(&upstream, "pooler->VM");

    upstream
        .write_all(startup_raw)
        .await
        .context("replaying startup packet upstream")?;
    upstream.flush().await?;

    copy_bidirectional(&mut client, &mut upstream)
        .await
        .context("proxying client <-> VM")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::net::{TcpListener, TcpStream};

    /// The splice path disables Nagle on both legs; this pins that the option
    /// actually takes on the `tokio::net::TcpStream` type the proxy uses, so a
    /// future refactor that drops the call (or a platform where it silently
    /// fails) is caught here rather than as a latency regression in production.
    #[tokio::test]
    async fn set_nodelay_takes_on_both_legs() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let client = TcpStream::connect(addr).await.unwrap(); // the pooler->VM leg shape
        let server = accept.await.unwrap(); // the client->pooler (accepted) leg shape

        for (sock, leg) in [(&client, "connect"), (&server, "accept")] {
            assert!(
                !sock.nodelay().unwrap(),
                "{leg}: expected Nagle on by default"
            );
            sock.set_nodelay(true).unwrap();
            assert!(sock.nodelay().unwrap(), "{leg}: TCP_NODELAY did not take");
        }
    }

    /// Same pin for keepalive, which is load-bearing for a different reason:
    /// without it a vanished peer holds its client slot forever, so a refactor
    /// that drops the call turns into a slow slot leak on pinned schemas rather
    /// than anything visible at the connection that caused it.
    #[tokio::test]
    async fn keepalive_arms_on_both_legs() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let client = TcpStream::connect(addr).await.unwrap(); // the pooler->VM leg shape
        let server = accept.await.unwrap(); // the client->pooler (accepted) leg shape

        for (sock, leg) in [(&client, "connect"), (&server, "accept")] {
            let sr = socket2::SockRef::from(sock);
            assert!(
                !sr.keepalive().unwrap(),
                "{leg}: expected keepalive off by default"
            );
            super::arm_keepalive(sock, leg);
            assert!(
                sr.keepalive().unwrap(),
                "{leg}: SO_KEEPALIVE did not take — a dead peer would hold its slot"
            );
        }
    }
}
