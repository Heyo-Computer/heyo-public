//! Guest readiness probing.
//!
//! The daemon has no event stream or boot-complete signal we can subscribe to
//! (its Firecracker driver watches the serial console for a `HEYVM_READY`
//! marker, but never surfaces that over HTTP). Combined with `wait_for_ready`
//! being unreliable, that leaves probing the guest ourselves as the only honest
//! way to know a VM can serve.

use crate::config::HealthCheck;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Probe `addr`, returning whether the guest is serving.
///
/// With `check.path` set this does a minimal HTTP GET and requires a non-5xx
/// status; otherwise a successful TCP connect is enough. Any error is just
/// "not ready" — a booting VM refuses connections, which is expected, so this
/// deliberately doesn't distinguish failure modes.
pub async fn probe(addr: SocketAddr, check: &HealthCheck) -> bool {
    let target = match check.port {
        Some(p) => SocketAddr::new(addr.ip(), p),
        None => addr,
    };
    let timeout = Duration::from_secs(check.timeout_secs.max(1));

    match tokio::time::timeout(timeout, probe_inner(target, check.path.as_deref())).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::trace!(%target, error = %e, "health probe failed");
            false
        }
        Err(_) => {
            tracing::trace!(%target, "health probe timed out");
            false
        }
    }
}

async fn probe_inner(target: SocketAddr, path: Option<&str>) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(target).await?;
    let Some(path) = path else {
        return Ok(()); // TCP connect was the whole check.
    };

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: app-lb/health\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // The status line is all we need, and it arrives in the first packet.
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "guest closed connection without responding",
        ));
    }

    match parse_status(&buf[..n]) {
        Some(code) if code < 500 => Ok(()),
        Some(code) => Err(std::io::Error::other(format!("unhealthy status {code}"))),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed HTTP response",
        )),
    }
}

/// Pull the status code out of an HTTP status line: `HTTP/1.1 200 OK`.
fn parse_status(buf: &[u8]) -> Option<u16> {
    let head = std::str::from_utf8(buf).ok()?;
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n\r\n"), Some(200));
        assert_eq!(parse_status(b"HTTP/1.0 404 Not Found\r\n"), Some(404));
        assert_eq!(
            parse_status(b"HTTP/1.1 503 Service Unavailable\r\n"),
            Some(503)
        );
        assert_eq!(parse_status(b"garbage"), None);
        assert_eq!(parse_status(b"NOTHTTP 200 OK"), None);
    }

    #[tokio::test]
    async fn tcp_only_probe_passes_on_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let check = HealthCheck {
            path: None,
            port: None,
            timeout_secs: 2,
        };
        assert!(probe(addr, &check).await);
    }

    #[tokio::test]
    async fn probe_fails_when_nothing_is_listening() {
        // Bind then drop to get a port nothing is on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let check = HealthCheck::default();
        assert!(!probe(addr, &check).await);
    }

    async fn serve_once(response: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn http_probe_accepts_2xx() {
        let addr = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        assert!(probe(addr, &HealthCheck::default()).await);
    }

    #[tokio::test]
    async fn http_probe_accepts_non_5xx() {
        // A 404 still proves the guest's HTTP stack is up and serving.
        let addr = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
        assert!(probe(addr, &HealthCheck::default()).await);
    }

    #[tokio::test]
    async fn http_probe_rejects_5xx() {
        let addr =
            serve_once("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
        assert!(!probe(addr, &HealthCheck::default()).await);
    }

    #[tokio::test]
    async fn http_probe_rejects_a_silent_guest() {
        // Accepts the connection but never responds: a TCP-only check would
        // pass here, which is exactly why the HTTP path exists.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _keep = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let check = HealthCheck {
            path: Some("/".into()),
            port: None,
            timeout_secs: 1,
        };
        assert!(!probe(addr, &check).await);
    }
}
