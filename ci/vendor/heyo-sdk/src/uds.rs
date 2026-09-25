//! Unix-domain-socket transport for the local heyvm API (`heyvmd --socket`).
//!
//! reqwest cannot dial unix sockets, so this wraps hyper-util's legacy client
//! with a connector that ignores the request URI's authority and always dials
//! one socket path. Request URLs still look like `http://localhost/...` so the
//! daemon's Host-header middleware sees the same authority a TCP client would
//! send. Responses are converted back into `reqwest::Response` so every
//! existing call path in [`crate::client::HeyoClient`] works unchanged.
//!
//! Unix-only: gated behind `#[cfg(unix)]` at the module declaration.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use reqwest::header::HeaderMap;
use reqwest::Method;
use tokio::net::UnixStream;

use crate::errors::HeyoError;

/// Env var naming the daemon's API socket (mirrors `heyvmd`'s own
/// `HEYVM_SOCKET` handling).
const SOCKET_ENV_VAR: &str = "HEYVM_SOCKET";

/// A connection produced by [`UnixConnector`]. Newtype because hyper-util
/// only implements [`Connection`] for its own TCP types.
pub(crate) struct UdsConn(TokioIo<UnixStream>);

impl Connection for UdsConn {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl hyper::rt::Read for UdsConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for UdsConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Dials the same unix socket for every request, whatever the URI says.
#[derive(Clone, Debug)]
pub(crate) struct UnixConnector(Arc<PathBuf>);

impl tower_service::Service<hyper::Uri> for UnixConnector {
    type Response = UdsConn;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<UdsConn, std::io::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: hyper::Uri) -> Self::Future {
        let path = Arc::clone(&self.0);
        Box::pin(async move {
            let stream = UnixStream::connect(path.as_ref()).await?;
            Ok(UdsConn(TokioIo::new(stream)))
        })
    }
}

/// Connection-pooled HTTP client bound to one socket path.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Request bodies over the socket: a whole buffer (every JSON call) or a
/// stream (an image or tree upload), both boxed to one client type.
type UdsBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Clone, Debug)]
pub(crate) struct UdsTransport {
    client: Client<UnixConnector, UdsBody>,
    path: Arc<PathBuf>,
}

impl UdsTransport {
    pub(crate) fn new(path: PathBuf) -> Self {
        let path = Arc::new(path);
        let client =
            Client::builder(TokioExecutor::new()).build(UnixConnector(Arc::clone(&path)));
        Self { client, path }
    }

    /// The socket this transport dials. Reported by
    /// [`HeyoClient::socket_path`](crate::HeyoClient::socket_path) so a caller
    /// can log which transport it ended up on.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Send one request over the socket and hand back a `reqwest::Response`
    /// (streaming body), so callers built around reqwest need no changes.
    ///
    /// `url` is the full `http://localhost/...` URL; `api_path` is the API
    /// path used purely in error messages, matching the TCP error texts.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &str,
        api_path: &str,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<reqwest::Response, HeyoError> {
        let body = BodyExt::boxed_unsync(
            Full::new(Bytes::from(body.unwrap_or_default())).map_err(|never| match never {}),
        );
        self.dispatch(method, url, api_path, headers, body, timeout).await
    }

    /// As [`send`](Self::send), with a body that arrives as it is read —
    /// an upload too large to hold.
    pub(crate) async fn send_stream(
        &self,
        method: Method,
        url: &str,
        api_path: &str,
        headers: HeaderMap,
        body: crate::daemon::UploadStream,
        timeout: Duration,
    ) -> Result<reqwest::Response, HeyoError> {
        let body = BodyExt::boxed_unsync(StreamBody::new(
            body.map(|chunk| chunk.map(Frame::data).map_err(|e| Box::new(e) as BoxError)),
        ));
        self.dispatch(method, url, api_path, headers, body, timeout).await
    }

    async fn dispatch(
        &self,
        method: Method,
        url: &str,
        api_path: &str,
        headers: HeaderMap,
        body: UdsBody,
        timeout: Duration,
    ) -> Result<reqwest::Response, HeyoError> {
        let mut request = http::Request::builder()
            .method(method)
            .uri(url)
            .body(body)
            .map_err(|e| HeyoError::api(0, format!("network error calling {}: {}", api_path, e)))?;
        *request.headers_mut() = headers;
        let response = tokio::time::timeout(timeout, self.client.request(request))
            .await
            .map_err(|_| {
                HeyoError::api(
                    0,
                    format!(
                        "network error calling {}: request over socket {} timed out",
                        api_path,
                        self.path.display()
                    ),
                )
            })?
            .map_err(|e| {
                HeyoError::api(
                    0,
                    format!(
                        "network error calling {} over socket {}: {}",
                        api_path,
                        self.path.display(),
                        crate::errors::cause_chain(&e)
                    ),
                )
            })?;
        let (parts, incoming) = response.into_parts();
        let body = reqwest::Body::wrap_stream(incoming.into_data_stream());
        Ok(reqwest::Response::from(http::Response::from_parts(parts, body)))
    }
}

/// Find a live daemon socket, or `None` to fall back to TCP.
///
/// Reads `HEYVM_SOCKET` and `~/.heyo/daemon.json` and delegates the actual
/// precedence + liveness decision to [`resolve_socket`].
pub(crate) fn discover_socket() -> Option<PathBuf> {
    let env = std::env::var(SOCKET_ENV_VAR).ok();
    let daemon_json = daemon_json_path().and_then(|p| std::fs::read_to_string(p).ok());
    resolve_socket(env.as_deref(), daemon_json.as_deref(), socket_is_live)
}

/// `~/.heyo/daemon.json` — the daemon's persisted identity file, which
/// carries an optional `socket_path` while the daemon serves on a socket.
fn daemon_json_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".heyo").join("daemon.json"))
}

fn socket_is_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Pure precedence core of [`discover_socket`], parameterized for tests.
///
/// Order: `HEYVM_SOCKET` env var, then daemon.json's `socket_path`. Every
/// candidate must pass `is_live` — a crashed daemon can leave a stale
/// `socket_path` behind, so the file is a hint, not a promise.
fn resolve_socket(
    env: Option<&str>,
    daemon_json: Option<&str>,
    is_live: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let from_env = env.filter(|s| !s.is_empty()).map(PathBuf::from);
    let from_state = daemon_json
        .and_then(parse_daemon_json_socket_path)
        .map(PathBuf::from);
    [from_env, from_state]
        .into_iter()
        .flatten()
        .find(|p| is_live(p))
}

/// Extract `socket_path` from a daemon.json document. Tolerates a missing
/// field (files written by older daemons) and malformed JSON.
fn parse_daemon_json_socket_path(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value
        .get("socket_path")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_socket_wins_when_live() {
        let picked = resolve_socket(
            Some("/env.sock"),
            Some(r#"{"backend_id":"hd-x","socket_path":"/state.sock"}"#),
            |_| true,
        );
        assert_eq!(picked, Some(PathBuf::from("/env.sock")));
    }

    #[test]
    fn dead_env_socket_falls_through_to_daemon_json() {
        let picked = resolve_socket(
            Some("/env.sock"),
            Some(r#"{"socket_path":"/state.sock"}"#),
            |p| p == Path::new("/state.sock"),
        );
        assert_eq!(picked, Some(PathBuf::from("/state.sock")));
    }

    #[test]
    fn all_dead_means_tcp_fallback() {
        let picked = resolve_socket(
            Some("/env.sock"),
            Some(r#"{"socket_path":"/state.sock"}"#),
            |_| false,
        );
        assert_eq!(picked, None);
    }

    #[test]
    fn nothing_configured_means_tcp_fallback() {
        let picked = resolve_socket(None, None, |_| true);
        assert_eq!(picked, None);
    }

    #[test]
    fn empty_env_var_is_ignored() {
        let picked = resolve_socket(Some(""), None, |_| true);
        assert_eq!(picked, None);
    }

    #[test]
    fn daemon_json_without_socket_path_is_tolerated() {
        // Files written before `socket_path` existed must not break discovery.
        let picked = resolve_socket(
            None,
            Some(r#"{"backend_id":"hd-abc","name":"box"}"#),
            |_| true,
        );
        assert_eq!(picked, None);
    }

    #[test]
    fn malformed_daemon_json_is_tolerated() {
        let picked = resolve_socket(None, Some("not json at all"), |_| true);
        assert_eq!(picked, None);
    }

    #[test]
    fn parses_socket_path_field() {
        assert_eq!(
            parse_daemon_json_socket_path(
                r#"{"backend_id":"hd-abc","socket_path":"/home/u/.heyo/heyvmd.sock"}"#
            ),
            Some("/home/u/.heyo/heyvmd.sock".to_string())
        );
        assert_eq!(parse_daemon_json_socket_path(r#"{"socket_path":""}"#), None);
        assert_eq!(parse_daemon_json_socket_path(r#"{"socket_path":42}"#), None);
    }

    #[tokio::test]
    async fn send_dials_the_socket_and_yields_a_reqwest_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let path = std::env::temp_dir().join(format!(
            "heyo-sdk-uds-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        // Minimal canned HTTP/1.1 server: read the request head, answer JSON.
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut read = 0;
            loop {
                let n = stream.read(&mut buf[read..]).await.unwrap();
                assert!(n > 0, "peer hung up before finishing the request head");
                read += n;
                if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf[..read]).to_string();
            let body = br#"{"status":"ok"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            head
        });

        let transport = UdsTransport::new(path.clone());
        let resp = transport
            .send(
                Method::GET,
                "http://localhost/health",
                "/health",
                HeaderMap::new(),
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(&bytes[..], br#"{"status":"ok"}"#);

        let head = server.await.unwrap();
        assert!(head.starts_with("GET /health HTTP/1.1"), "got: {head}");
        assert!(
            head.to_ascii_lowercase().contains("host: localhost"),
            "expected Host: localhost so the daemon's Host middleware behaves; got: {head}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
