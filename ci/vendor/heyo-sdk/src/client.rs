//! HTTP transport. Wraps `reqwest::Client` with bearer auth, timeouts, and
//! HTTP-error translation matching `sdk-ts/src/client.ts`.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use url::Url;

use crate::errors::HeyoError;

const DEFAULT_BASE_URL: &str = "https://server.heyo.computer";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default base URL for a local heyvm API (the `heyvmd` daemon's `--api-port`).
/// Used by [`HeyoClient::local`] so desktop apps can drive a same-machine
/// sandbox without the cloud in the data path.
pub const DEFAULT_LOCAL_BASE_URL: &str = "http://127.0.0.1:34099";

/// Base URL used by the unix-socket transport. The socket connector ignores
/// the authority and always dials the configured path; `localhost` is kept in
/// the URL purely so the daemon's Host-header middleware sees the same
/// authority a TCP client would send.
#[cfg(unix)]
const UDS_BASE_URL: &str = "http://localhost";

/// Construction options for [`HeyoClient`]. Mirrors `HeyoClientOptions` in
/// `sdk-ts/src/client.ts`.
#[derive(Debug, Default, Clone)]
pub struct HeyoClientOptions {
    /// Bearer token. Falls back to `HEYO_API_KEY` env var when `None`.
    pub api_key: Option<String>,
    /// Cloud base URL. Default: `https://server.heyo.computer`.
    pub base_url: Option<String>,
    /// Per-request timeout. Default: 60 seconds.
    pub timeout: Option<Duration>,
}

/// Optional per-request knobs.
#[derive(Debug, Default, Clone)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    /// Query string parameters appended verbatim.
    pub query: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct HeyoClient {
    inner: Arc<Inner>,
}

struct Inner {
    /// Bearer token, or `None` for an unauthenticated (e.g. local) client.
    api_key: Option<String>,
    base_url: String,
    http: reqwest::Client,
    default_timeout: Duration,
    /// Unix-socket transport. When set, every HTTP request (and the shell
    /// WebSocket) is dialed over this socket instead of TCP; `base_url` is
    /// [`UDS_BASE_URL`] purely for URL/Host semantics.
    #[cfg(unix)]
    uds: Option<crate::uds::UdsTransport>,
    /// Keeps the iroh P2P tunnel task alive for the client's lifetime when the
    /// client was built via [`HeyoClient::connect_p2p`]. Aborted on Drop so the
    /// tunnel tears down when the last clone of the client goes away.
    _tunnel: Option<TunnelGuard>,
}

/// Owns the background task pumping the iroh tunnel. Dropping it aborts the
/// task (and so closes the local TCP listener the requests were pointed at).
struct TunnelGuard(tokio::task::JoinHandle<()>);

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl HeyoClient {
    pub fn new(opts: HeyoClientOptions) -> Result<Self, HeyoError> {
        // api_key is optional: a cloud client without one will simply get 401s,
        // while a local client (custom base_url pointed at a heyvm daemon with
        // no JWT_SECRET) needs no auth at all. We therefore no longer fail
        // construction when the key is absent — auth is enforced server-side.
        let api_key = opts
            .api_key
            .or_else(|| std::env::var("HEYO_API_KEY").ok())
            .filter(|k| !k.is_empty());
        let base_url = opts
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| HeyoError::Connection(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                api_key,
                base_url,
                http,
                default_timeout: opts.timeout.unwrap_or(DEFAULT_TIMEOUT),
                #[cfg(unix)]
                uds: None,
                _tunnel: None,
            }),
        })
    }

    /// Build a client targeting a local heyvm API at [`DEFAULT_LOCAL_BASE_URL`]
    /// (`http://127.0.0.1:34099`). No auth is sent — a same-machine daemon runs
    /// without `JWT_SECRET` and skips authentication. Use [`HeyoClient::local_at`]
    /// for a non-default address. The local API understands the same cloud HTTP
    /// dialect via its compatibility routes, so the full SDK surface works.
    pub fn local() -> Result<Self, HeyoError> {
        Self::local_at(DEFAULT_LOCAL_BASE_URL)
    }

    /// Like [`HeyoClient::local`] but targets an explicit base URL — e.g. a port
    /// some other component bound for you (an iroh tunnel, an SSH forward).
    pub fn local_at(base_url: impl Into<String>) -> Result<Self, HeyoError> {
        Self::new(HeyoClientOptions {
            base_url: Some(base_url.into()),
            api_key: None,
            timeout: None,
        })
    }

    /// Talk to a local heyvm daemon over a unix domain socket instead of
    /// loopback TCP. Pair of [`HeyoClient::local`] for daemons started with
    /// `heyvmd --socket` (default socket: `~/.heyo/heyvmd.sock`).
    ///
    /// Requests carry `http://localhost` URLs and a `Host: localhost` header,
    /// so the daemon's Host-header middleware behaves exactly as it does for
    /// a TCP client — only the transport differs. The full SDK surface works,
    /// including the interactive shell WebSocket. No auth is sent, matching
    /// [`HeyoClient::local`]. Unix-only.
    #[cfg(unix)]
    pub fn local_socket(path: impl Into<std::path::PathBuf>) -> Result<Self, HeyoError> {
        Self::local_socket_with(path, HeyoClientOptions::default())
    }

    /// [`HeyoClient::local_socket`] carrying the caller's bearer and timeout.
    ///
    /// `opts.base_url` is ignored — a socket has no address to override; the
    /// URL a request carries is [`UDS_BASE_URL`] whatever the caller asked for.
    /// Unlike [`HeyoClient::new`] there is no `HEYO_API_KEY` fallback: a socket
    /// client is same-machine by construction, and inheriting an ambient cloud
    /// key here would send it somewhere the caller never named.
    #[cfg(unix)]
    pub fn local_socket_with(
        path: impl Into<std::path::PathBuf>,
        opts: HeyoClientOptions,
    ) -> Result<Self, HeyoError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| HeyoError::Connection(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                api_key: opts.api_key.filter(|k| !k.is_empty()),
                base_url: UDS_BASE_URL.to_string(),
                http,
                default_timeout: opts.timeout.unwrap_or(DEFAULT_TIMEOUT),
                uds: Some(crate::uds::UdsTransport::new(path.into())),
                _tunnel: None,
            }),
        })
    }

    /// Build the best local client available: unix socket when one is
    /// discoverable and alive, else plain TCP ([`HeyoClient::local`]).
    ///
    /// Discovery order:
    ///
    /// 1. the `HEYVM_SOCKET` env var;
    /// 2. the optional `socket_path` field of `~/.heyo/daemon.json`.
    ///
    /// Every candidate is connect-verified before being chosen: a crashed
    /// daemon can leave a **stale** `socket_path` behind in `daemon.json`
    /// (the file is only cleaned up on graceful shutdown), so it is treated
    /// as a hint, not a promise. When no candidate accepts a connection this
    /// falls back to TCP on `127.0.0.1:34099`, which the daemon always
    /// serves. On non-unix platforms this is exactly [`HeyoClient::local`].
    pub fn local_auto() -> Result<Self, HeyoError> {
        Self::local_auto_with(HeyoClientOptions::default())
    }

    /// [`HeyoClient::local_auto`] carrying the caller's bearer and timeout.
    ///
    /// Same discovery and connect-verification as `local_auto`. `opts.base_url`
    /// is used only by the TCP fallback, where it defaults to
    /// [`DEFAULT_LOCAL_BASE_URL`] — so a caller that has an explicit address
    /// *and* wants it honoured should call [`HeyoClient::new`] instead, rather
    /// than passing it here and being handed a socket.
    pub fn local_auto_with(opts: HeyoClientOptions) -> Result<Self, HeyoError> {
        #[cfg(unix)]
        {
            if let Some(path) = crate::uds::discover_socket() {
                return Self::local_socket_with(path, opts);
            }
        }
        Self::new(HeyoClientOptions {
            base_url: Some(
                opts.base_url
                    .unwrap_or_else(|| DEFAULT_LOCAL_BASE_URL.to_string()),
            ),
            api_key: opts.api_key,
            timeout: opts.timeout,
        })
    }

    /// The unix socket this client dials, or `None` when it speaks TCP.
    ///
    /// `base_url` cannot answer this — a socket client reports
    /// [`UDS_BASE_URL`], which names no transport. Exposed so a caller can log
    /// what [`HeyoClient::local_auto`] actually chose.
    pub fn socket_path(&self) -> Option<&std::path::Path> {
        #[cfg(unix)]
        {
            self.inner.uds.as_ref().map(|u| u.path())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Connect directly to a remote heyvm daemon over iroh P2P, bypassing the
    /// cloud data path. `ticket` is the daemon's `connection_url` (fetch it from
    /// the cloud via `GET /me/daemons/{id}/connection-ticket`); `relay` is an
    /// optional iroh relay override for NAT traversal. A background task pumps
    /// the tunnel for the client's lifetime and is torn down when the client
    /// (and all its clones) drop.
    ///
    /// The client's `base_url` is set to the local TCP port the tunnel binds, so
    /// every subsequent SDK call rides the P2P link. `api_key` is forwarded as
    /// the bearer to the daemon (which may run with auth enabled); pass `None`
    /// when the daemon is unauthenticated.
    pub async fn connect_p2p(
        ticket: &str,
        relay: Option<&str>,
        api_key: Option<String>,
    ) -> Result<Self, HeyoError> {
        let proxy = crate::proxy::Client::connect(ticket, 0, relay)
            .await
            .map_err(|e| HeyoError::Connection(format!("iroh P2P connect failed: {e}")))?;
        let local = proxy
            .local_addr()
            .map_err(|e| HeyoError::Connection(format!("tunnel local_addr: {e}")))?;
        let base_url = format!("http://{}", local);
        let handle = tokio::spawn(async move {
            // The tunnel runs until the listener errors or the task is aborted
            // on Drop. A failure here just means later requests get a
            // connection error, which surfaces to the caller naturally.
            let _ = proxy.run().await;
        });
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| HeyoError::Connection(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                api_key: api_key.filter(|k| !k.is_empty()),
                base_url,
                http,
                default_timeout: DEFAULT_TIMEOUT,
                #[cfg(unix)]
                uds: None,
                _tunnel: Some(TunnelGuard(handle)),
            }),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// The bearer this client sends, if any. Public so a caller that resolves
    /// the key itself can assert the one it passed is the one in force.
    pub fn api_key(&self) -> Option<&str> {
        self.inner.api_key.as_deref()
    }

    /// Issue a request and deserialize the JSON response.
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        opts: RequestOptions,
    ) -> Result<T, HeyoError> {
        let bytes = self.request_bytes(method, path, body, opts).await?;
        if bytes.is_empty() {
            // Caller asked for T but the server returned no body. Try to
            // deserialize "null" so types like `()` (via serde_json::Value)
            // or Option<T> still work.
            return serde_json::from_slice::<T>(b"null").map_err(|e| {
                HeyoError::api(0, format!("empty response body could not be parsed: {}", e))
            });
        }
        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| HeyoError::api(0, format!("invalid JSON response: {}", e)))
    }

    /// Like `request` but returns the raw response bytes (for binary
    /// endpoints).
    pub async fn request_bytes(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        opts: RequestOptions,
    ) -> Result<Vec<u8>, HeyoError> {
        let response = self.raw_request(method, path, body, opts).await?;
        self.consume_response(response, path).await
    }

    /// Issue the request and return the raw `reqwest::Response`. Use this
    /// when you need response headers or want to stream the body.
    pub async fn raw_request(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        opts: RequestOptions,
    ) -> Result<Response, HeyoError> {
        let url = self.build_url(path, &opts.query)?;
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(key) = &self.inner.api_key {
            let auth = format!("Bearer {}", key);
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth)
                    .map_err(|e| HeyoError::api(0, format!("invalid api key header: {}", e)))?,
            );
        }
        #[cfg(unix)]
        if let Some(uds) = &self.inner.uds {
            let body_bytes = match body {
                Some(b) => Some(serde_json::to_vec(b).map_err(|e| {
                    HeyoError::api(0, format!("network error calling {}: {}", path, e))
                })?),
                None => None,
            };
            if body_bytes.is_some() {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }
            return uds
                .send(
                    method,
                    &url,
                    path,
                    headers,
                    body_bytes,
                    opts.timeout.unwrap_or(self.inner.default_timeout),
                )
                .await;
        }
        let mut builder = self
            .inner
            .http
            .request(method, url)
            .headers(headers)
            .timeout(opts.timeout.unwrap_or(self.inner.default_timeout));
        if let Some(body) = body {
            builder = builder
                .header(CONTENT_TYPE, "application/json")
                .json(body);
        }
        builder
            .send()
            .await
            .map_err(|e| HeyoError::transport_http(path, &e))
    }

    /// `GET` a route whose answer is a byte stream — an archive, an export.
    /// The response is handed back unread on success so the body can be
    /// consumed as it arrives (`bytes_stream()`); an error status is mapped
    /// like any other call's.
    pub async fn stream_get(&self, path: &str, opts: RequestOptions) -> Result<Response, HeyoError> {
        let response = self.raw_request(Method::GET, path, None::<&()>, opts).await?;
        self.check_status(response, path).await
    }

    /// Send a request whose body is a stream of byte chunks — an image or a
    /// tree upload — over either transport. `headers` are extra request
    /// headers; `path` may carry its own query string.
    pub async fn send_stream(
        &self,
        method: Method,
        path: &str,
        content_type: &str,
        headers: Vec<(String, String)>,
        body: crate::daemon::UploadStream,
        timeout: Duration,
    ) -> Result<Response, HeyoError> {
        let url = self.build_url(path, &[])?;
        let mut header_map = HeaderMap::new();
        header_map.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(key) = &self.inner.api_key {
            let auth = format!("Bearer {}", key);
            header_map.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth)
                    .map_err(|e| HeyoError::api(0, format!("invalid api key header: {}", e)))?,
            );
        }
        header_map.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(content_type)
                .map_err(|e| HeyoError::api(0, format!("invalid content-type: {}", e)))?,
        );
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| HeyoError::api(0, format!("invalid header {name}: {e}")))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|e| HeyoError::api(0, format!("invalid header value for {name}: {e}")))?;
            header_map.insert(name, value);
        }
        #[cfg(unix)]
        if let Some(uds) = &self.inner.uds {
            let response = uds
                .send_stream(method, &url, path, header_map, body, timeout)
                .await?;
            return self.check_status(response, path).await;
        }
        let response = self
            .inner
            .http
            .request(method, url)
            .headers(header_map)
            .timeout(timeout)
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await
            .map_err(|e| HeyoError::transport_http(path, &e))?;
        self.check_status(response, path).await
    }

    /// The JSON body of a response [`check_status`](Self::check_status)
    /// already admitted.
    pub(crate) async fn parse_json<T: DeserializeOwned>(
        &self,
        response: Response,
        path: &str,
    ) -> Result<T, HeyoError> {
        let bytes = self.consume_response(response, path).await?;
        if bytes.is_empty() {
            return serde_json::from_slice::<T>(b"null").map_err(|e| {
                HeyoError::api(0, format!("empty response body could not be parsed: {}", e))
            });
        }
        serde_json::from_slice::<T>(&bytes)
            .map_err(|e| HeyoError::api(0, format!("invalid JSON response: {}", e)))
    }

    /// Pass a success through unread; map anything else to the error the
    /// buffered calls would have produced.
    async fn check_status(&self, response: Response, path: &str) -> Result<Response, HeyoError> {
        if response.status().is_success() {
            return Ok(response);
        }
        match self.consume_response(response, path).await {
            Ok(_) => Err(HeyoError::api(0, format!("unexpected status calling {path}"))),
            Err(e) => Err(e),
        }
    }

    /// POST raw bytes (Content-Type: application/octet-stream by default) and
    /// return the raw response.
    pub async fn put_bytes(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
        opts: RequestOptions,
    ) -> Result<Response, HeyoError> {
        let url = self.build_url(path, &opts.query)?;
        let mut headers = HeaderMap::new();
        if let Some(key) = &self.inner.api_key {
            let auth = format!("Bearer {}", key);
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth)
                    .map_err(|e| HeyoError::api(0, format!("invalid api key header: {}", e)))?,
            );
        }
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(content_type)
                .map_err(|e| HeyoError::api(0, format!("invalid content-type: {}", e)))?,
        );
        #[cfg(unix)]
        if let Some(uds) = &self.inner.uds {
            return uds
                .send(
                    Method::PUT,
                    &url,
                    path,
                    headers,
                    Some(body),
                    opts.timeout.unwrap_or(self.inner.default_timeout),
                )
                .await;
        }
        self.inner
            .http
            .request(Method::PUT, url)
            .headers(headers)
            .timeout(opts.timeout.unwrap_or(self.inner.default_timeout))
            .body(body)
            .send()
            .await
            .map_err(|e| HeyoError::transport_http(path, &e))
    }

    /// Build the WS URL for a given path. Scheme is swapped (`http`→`ws`,
    /// `https`→`wss`).
    pub(crate) fn ws_url(&self, path: &str) -> Result<String, HeyoError> {
        let http_url = self.build_url(path, &[])?;
        let mut parsed = Url::parse(&http_url)
            .map_err(|e| HeyoError::Connection(format!("bad URL {}: {}", http_url, e)))?;
        let scheme = match parsed.scheme() {
            "https" => "wss",
            "http" => "ws",
            other => return Err(HeyoError::Connection(format!("unsupported scheme {}", other))),
        };
        parsed
            .set_scheme(scheme)
            .map_err(|_| HeyoError::Connection("could not swap to ws scheme".into()))?;
        Ok(parsed.to_string())
    }

    /// Owned form of [`HeyoClient::socket_path`]. Lets the shell WebSocket dial
    /// the same socket without borrowing the client across the connect.
    pub(crate) fn uds_socket_path(&self) -> Option<std::path::PathBuf> {
        self.socket_path().map(|p| p.to_path_buf())
    }

    pub(crate) fn ws_authorization(&self) -> String {
        // Empty when unauthenticated (local daemon with no JWT_SECRET ignores
        // the header). Cloud clients always carry a key.
        match &self.inner.api_key {
            Some(key) => format!("Bearer {}", key),
            None => String::new(),
        }
    }

    fn build_url(&self, path: &str, query: &[(String, String)]) -> Result<String, HeyoError> {
        let clean = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
        let mut url = Url::parse(&format!("{}{}", self.inner.base_url, clean))
            .map_err(|e| HeyoError::api(0, format!("bad URL {}{}: {}", self.inner.base_url, clean, e)))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url.to_string())
    }

    async fn consume_response(
        &self,
        response: Response,
        path: &str,
    ) -> Result<Vec<u8>, HeyoError> {
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| HeyoError::api(0, format!("read body for {}: {}", path, e)))?;
        if status.is_success() {
            if status == StatusCode::NO_CONTENT || status == StatusCode::RESET_CONTENT {
                return Ok(Vec::new());
            }
            return Ok(bytes.to_vec());
        }

        let mut message = format!("{} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
        let mut parsed_body: Option<serde_json::Value> = None;
        if !bytes.is_empty() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(m) = v.get("message").and_then(|x| x.as_str()) {
                    message = m.to_string();
                } else if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
                    message = e.to_string();
                }
                parsed_body = Some(v);
            } else if let Ok(text) = std::str::from_utf8(&bytes) {
                message = text.to_string();
            }
        }

        let with_path = format!("{} (calling {})", message, path);
        Err(match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => HeyoError::Authentication,
            StatusCode::NOT_FOUND => HeyoError::NotFound(with_path),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                HeyoError::InvalidArgument(with_path)
            }
            _ => HeyoError::api_with_body(status.as_u16(), with_path, parsed_body),
        })
    }
}

impl std::fmt::Debug for HeyoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeyoClient")
            .field("base_url", &self.inner.base_url)
            .field("default_timeout", &self.inner.default_timeout)
            .finish_non_exhaustive()
    }
}
