//! Routing for Postgres query cancellation.
//!
//! A client cancels a running query by opening a *new* connection and sending
//! a `CancelRequest` (int32 length, int32 code 80877102, int32 backend pid,
//! then the secret key) instead of a StartupMessage. The server matches pid +
//! secret against its backends, signals the one that matches, and closes
//! without replying. The client learned the pair from the `BackendKeyData`
//! message ('K') the server sent during its startup, after authentication and
//! before the first `ReadyForQuery` ('Z').
//!
//! Behind the pooler the cancel connection lands here, not on the guest, and
//! nothing in it names a database — so the pooler has to remember which VM
//! issued which key. [`KeyCapture`] watches the server->client half of each
//! spliced session until the first 'Z', records the key in a process-wide
//! [`CancelMap`] against the address the session was spliced to, and removes
//! it when the session ends. A cancel then costs one map lookup and one short
//! connection to that address: no auth, no registry checkout, no admission
//! slot, and never a VM bring-up (an unknown key is dropped silently, as the
//! server itself would).
//!
//! The secret is the only thing standing between any network client and
//! cancelling someone else's query, so it is never logged, and [`CancelKey`]'s
//! `Debug` redacts it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// The `CancelRequest` "protocol version".
pub(crate) const CANCEL_REQUEST_CODE: i32 = 80877102;

/// Protocol 3.2 (Postgres 18) allows secrets up to 256 bytes; 3.0 uses
/// exactly 4. Accepting the range costs nothing and keeps a 3.2 client's
/// cancels working against a PG18 guest.
const MAX_SECRET_LEN: usize = 256;

/// How far into the server's byte stream to look for `BackendKeyData` before
/// giving up. A real startup (auth, a dozen ParameterStatus, the key, 'Z') is
/// a few hundred bytes; this only bounds the work on a server that never says
/// 'Z'.
const SCAN_LIMIT: usize = 64 * 1024;

/// Bound on each step of forwarding a cancel. A cancel that cannot reach the
/// guest in this long is not going to help the client anyway.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

/// A backend's cancel credentials: the pid and secret from `BackendKeyData`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CancelKey {
    pid: i32,
    secret: Vec<u8>,
}

impl std::fmt::Debug for CancelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelKey")
            .field("pid", &self.pid)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl CancelKey {
    fn from_pid_and_secret(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 8 || bytes.len() > 4 + MAX_SECRET_LEN {
            bail!("cancel key has an invalid length");
        }
        Ok(Self {
            pid: i32::from_be_bytes(bytes[..4].try_into().unwrap()),
            secret: bytes[4..].to_vec(),
        })
    }

    /// Parse a `CancelRequest` body: everything after the length prefix, i.e.
    /// the code followed by pid and secret.
    pub(crate) fn from_request_body(body: &[u8]) -> Result<Self> {
        if body.len() < 4
            || i32::from_be_bytes(body[..4].try_into().unwrap()) != CANCEL_REQUEST_CODE
        {
            bail!("not a CancelRequest");
        }
        Self::from_pid_and_secret(&body[4..])
    }

    pub(crate) fn pid(&self) -> i32 {
        self.pid
    }

    /// The `CancelRequest` packet carrying this key, as a client would send it.
    pub(crate) fn request_packet(&self) -> Vec<u8> {
        let len = (12 + self.secret.len()) as i32;
        let mut packet = Vec::with_capacity(len as usize);
        packet.extend_from_slice(&len.to_be_bytes());
        packet.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        packet.extend_from_slice(&self.pid.to_be_bytes());
        packet.extend_from_slice(&self.secret);
        packet
    }
}

/// Where a session's cancels go: the address its splice dialed (the guest, or
/// the local end of its iroh tunnel, which lives as long as the session holds
/// its entry), or a peer pooler's client listener for a session this node
/// forwarded through a writer tunnel — that pooler holds the key's real route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CancelTarget {
    Addr(SocketAddr),
    Host(String, u16),
}

impl CancelTarget {
    async fn connect(&self) -> std::io::Result<TcpStream> {
        match self {
            Self::Addr(addr) => TcpStream::connect(addr).await,
            Self::Host(host, port) => TcpStream::connect((host.as_str(), *port)).await,
        }
    }
}

/// Live sessions' cancel keys -> where to send their cancels.
#[derive(Default)]
pub(crate) struct CancelMap {
    // The id distinguishes registrations of the same key (a pid and secret
    // repeated across two VMs is astronomically unlikely, but a stale drop
    // must never remove a newer session's entry).
    inner: Mutex<HashMap<CancelKey, (u64, CancelTarget)>>,
    next_id: AtomicU64,
}

impl CancelMap {
    /// Record `key` for a live session. Dropping the returned registration
    /// (when the session ends) removes it again.
    pub(crate) fn register(self: &Arc<Self>, key: CancelKey, target: CancelTarget) -> Registration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().unwrap().insert(key.clone(), (id, target));
        Registration {
            map: self.clone(),
            key,
            id,
        }
    }

    pub(crate) fn lookup(&self, key: &CancelKey) -> Option<CancelTarget> {
        self.inner.lock().unwrap().get(key).map(|(_, t)| t.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

/// A live session's entry in a [`CancelMap`]; removed on drop.
pub(crate) struct Registration {
    map: Arc<CancelMap>,
    key: CancelKey,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut inner = self.map.inner.lock().unwrap();
        if inner.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            inner.remove(&self.key);
        }
    }
}

/// The pooler's map. One per process: a cancel arrives on the shared client
/// listener with nothing but the key to route by.
pub(crate) fn global() -> &'static Arc<CancelMap> {
    static MAP: LazyLock<Arc<CancelMap>> = LazyLock::new(Arc::default);
    &MAP
}

/// Relay a cancel to the session that owns `key`. Returns whether the key was
/// known. An unknown key is not an error: Postgres drops those silently too,
/// and a client can't tell the difference.
pub(crate) async fn forward(map: &CancelMap, key: &CancelKey) -> Result<bool> {
    let Some(target) = map.lookup(key) else {
        return Ok(false);
    };
    let mut upstream = tokio::time::timeout(FORWARD_TIMEOUT, target.connect())
        .await
        .context("timed out connecting to forward a cancel")?
        .context("connecting to forward a cancel")?;
    tokio::time::timeout(FORWARD_TIMEOUT, async {
        upstream.write_all(&key.request_packet()).await?;
        upstream.flush().await?;
        // Postgres closes once it has acted on the request. Waiting for that
        // keeps libpq's semantics — PQcancel returns after the server has
        // seen the cancel — since the client's own connection closes when
        // this returns.
        let mut sink = [0u8; 64];
        while upstream.read(&mut sink).await? > 0 {}
        Ok::<_, std::io::Error>(())
    })
    .await
    .context("timed out forwarding a cancel")?
    .context("forwarding a cancel")?;
    Ok(true)
}

/// Incremental scan of a backend's startup-phase message stream for
/// `BackendKeyData`. Only framing is parsed — a type byte and an int32 length
/// per message — and only the key's body is ever buffered. Done at the first
/// `ReadyForQuery`, on anything malformed, or after [`SCAN_LIMIT`] bytes.
#[derive(Default)]
pub(crate) struct KeyScanner {
    header: [u8; 5],
    header_len: usize,
    /// Body bytes still to pass for the current message.
    remaining: usize,
    /// Set while inside a 'K' message.
    key_body: Option<Vec<u8>>,
    scanned: usize,
    done: bool,
}

impl KeyScanner {
    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// Feed the next bytes the server sent. Returns the key once its message
    /// is complete (at most once per scanner).
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) -> Option<CancelKey> {
        let mut found = None;
        while !self.done && !bytes.is_empty() {
            if self.scanned >= SCAN_LIMIT {
                self.done = true;
                break;
            }
            if self.header_len < 5 {
                let n = (5 - self.header_len).min(bytes.len());
                self.header[self.header_len..self.header_len + n].copy_from_slice(&bytes[..n]);
                self.header_len += n;
                self.scanned += n;
                bytes = &bytes[n..];
                if self.header_len < 5 {
                    break;
                }
                let len = i32::from_be_bytes(self.header[1..5].try_into().unwrap());
                if len < 4 {
                    self.done = true; // not a protocol stream we understand
                    break;
                }
                self.remaining = len as usize - 4;
                match self.header[0] {
                    // ReadyForQuery: startup is over, the key (if any) was sent.
                    b'Z' => self.done = true,
                    b'K' if (8..=4 + MAX_SECRET_LEN).contains(&self.remaining) => {
                        self.key_body = Some(Vec::with_capacity(self.remaining));
                    }
                    b'K' => self.done = true,
                    _ => {}
                }
                if self.done {
                    break;
                }
            }
            let n = self.remaining.min(bytes.len());
            if let Some(body) = &mut self.key_body {
                body.extend_from_slice(&bytes[..n]);
            }
            self.remaining -= n;
            self.scanned += n;
            bytes = &bytes[n..];
            if self.remaining == 0 {
                self.header_len = 0;
                if let Some(body) = self.key_body.take() {
                    found = CancelKey::from_pid_and_secret(&body).ok();
                }
            }
        }
        found
    }
}

/// The upstream leg of a splice, observed until startup completes so its
/// cancel key can be registered. Reads and writes pass straight through and
/// unmodified; once the scan is done the only cost is one `Option` check per
/// read. The registration lives as long as this stream — that is, as long as
/// the session.
pub(crate) struct KeyCapture<S> {
    inner: S,
    scanner: Option<KeyScanner>,
    map: Arc<CancelMap>,
    target: CancelTarget,
    _registration: Option<Registration>,
}

impl<S> KeyCapture<S> {
    pub(crate) fn new(inner: S, map: Arc<CancelMap>, target: CancelTarget) -> Self {
        Self {
            inner,
            scanner: Some(KeyScanner::default()),
            map,
            target,
            _registration: None,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for KeyCapture<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        let this = &mut *self;
        if let (Poll::Ready(Ok(())), Some(scanner)) = (&polled, &mut this.scanner) {
            if let Some(key) = scanner.feed(&buf.filled()[before..]) {
                this._registration = Some(this.map.register(key, this.target.clone()));
            }
            if scanner.is_done() || buf.filled().len() == before {
                this.scanner = None;
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for KeyCapture<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn key(pid: i32, secret: &[u8]) -> CancelKey {
        CancelKey {
            pid,
            secret: secret.to_vec(),
        }
    }

    fn msg(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut m = vec![kind];
        m.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
        m.extend_from_slice(body);
        m
    }

    /// What a trust-auth Postgres sends after a StartupMessage, then the
    /// first bytes of the session proper.
    fn server_startup(pid: i32, secret: &[u8]) -> Vec<u8> {
        let mut s = msg(b'R', &0i32.to_be_bytes()); // AuthenticationOk
        s.extend(msg(b'S', b"server_version\x0016.4\0"));
        s.extend(msg(b'S', b"client_encoding\0UTF8\0"));
        let mut kd = pid.to_be_bytes().to_vec();
        kd.extend_from_slice(secret);
        s.extend(msg(b'K', &kd));
        s.extend(msg(b'Z', b"I"));
        s.extend(msg(b'T', &[0, 0])); // post-startup traffic is ignored
        s
    }

    #[test]
    fn request_packet_round_trips_and_debug_hides_the_secret() {
        let k = key(4242, &0x0BAD_F00Du32.to_be_bytes());
        let packet = k.request_packet();
        assert_eq!(packet.len(), 16);
        assert_eq!(&packet[..4], &16i32.to_be_bytes());
        assert_eq!(CancelKey::from_request_body(&packet[4..]).unwrap(), k);
        assert!(!format!("{k:?}").contains("195948557"));
        assert!(!format!("{k:?}").to_lowercase().contains("bad"));
        assert!(CancelKey::from_request_body(&80877103i32.to_be_bytes()).is_err());
        assert!(CancelKey::from_request_body(&packet[4..10]).is_err());
    }

    #[test]
    fn scanner_captures_the_key_however_the_stream_is_split() {
        let stream = server_startup(77, &[1, 2, 3, 4]);
        for chunk in 1..=stream.len() {
            let mut scanner = KeyScanner::default();
            let mut found = Vec::new();
            for piece in stream.chunks(chunk) {
                found.extend(scanner.feed(piece));
            }
            assert_eq!(found, vec![key(77, &[1, 2, 3, 4])], "chunk size {chunk}");
            assert!(scanner.is_done(), "chunk size {chunk}");
        }
        // A protocol-3.2 length secret is captured whole.
        let long = [9u8; 32];
        let mut scanner = KeyScanner::default();
        assert_eq!(scanner.feed(&server_startup(5, &long)), Some(key(5, &long)));
    }

    #[test]
    fn scanner_stops_at_ready_for_query_or_garbage() {
        // No key before 'Z' (an auth failure, a pre-key error): nothing after
        // it is inspected, even something shaped like a key.
        let mut s = msg(b'R', &0i32.to_be_bytes());
        s.extend(msg(b'Z', b"I"));
        s.extend(msg(b'K', &[0, 0, 0, 1, 0, 0, 0, 2]));
        let mut scanner = KeyScanner::default();
        assert_eq!(scanner.feed(&s), None);
        assert!(scanner.is_done());
        for bad in [
            msg(b'K', &[0, 0, 0, 1]),   // too short for pid + secret
            vec![b'R', 0, 0, 0, 2],     // impossible length
            msg(b'K', &[0u8; 4 + 257]), // secret over the protocol's cap
        ] {
            let mut scanner = KeyScanner::default();
            assert_eq!(scanner.feed(&bad), None);
            assert!(scanner.is_done());
        }
    }

    #[test]
    fn map_entries_live_exactly_as_long_as_their_registration() {
        let map = Arc::new(CancelMap::default());
        let target = CancelTarget::Addr("127.0.0.1:5432".parse().unwrap());
        let a = map.register(key(1, &[1; 4]), target.clone());
        let b = map.register(key(2, &[2; 4]), target.clone());
        assert_eq!(map.len(), 2);
        assert_eq!(map.lookup(&key(1, &[1; 4])), Some(target.clone()));
        assert_eq!(
            map.lookup(&key(1, &[9; 4])),
            None,
            "the secret is part of the key"
        );
        drop(a);
        assert_eq!(map.lookup(&key(1, &[1; 4])), None);
        assert_eq!(map.len(), 1);
        // A re-registered key survives the older registration's drop.
        let other = CancelTarget::Host("peer.example".into(), 6432);
        let newer = map.register(key(2, &[2; 4]), other.clone());
        drop(b);
        assert_eq!(map.lookup(&key(2, &[2; 4])), Some(other));
        drop(newer);
        assert_eq!(map.len(), 0);
    }

    #[tokio::test]
    async fn key_capture_registers_while_the_session_lives() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let stream = server_startup(31337, &[7, 7, 7, 7]);
            // Dribble it out so the key straddles reads.
            for piece in stream.chunks(3) {
                sock.write_all(piece).await.unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            sock
        });
        let map = Arc::new(CancelMap::default());
        let target = CancelTarget::Addr(addr);
        let upstream = TcpStream::connect(addr).await.unwrap();
        let mut capture = KeyCapture::new(upstream, map.clone(), target.clone());
        let expected = server_startup(31337, &[7, 7, 7, 7]);
        let mut got = vec![0u8; expected.len()];
        capture.read_exact(&mut got).await.unwrap();
        assert_eq!(got, expected, "bytes pass through unmodified");
        assert_eq!(map.lookup(&key(31337, &[7, 7, 7, 7])), Some(target));
        drop(capture);
        assert_eq!(map.len(), 0, "the session's end removes its key");
        drop(server.await.unwrap());
    }

    #[tokio::test]
    async fn forward_sends_the_exact_cancel_packet_to_the_sessions_target() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let guest = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            // Postgres reads the 16 bytes and closes; read to the client's EOF.
            let mut buf = [0u8; 16];
            sock.read_exact(&mut buf).await.unwrap();
            got.extend_from_slice(&buf);
            got
        });
        let map = Arc::new(CancelMap::default());
        let k = key(1234, &0x5EC2_E7u32.to_be_bytes());
        let _session = map.register(k.clone(), CancelTarget::Addr(addr));
        assert!(forward(&map, &k).await.unwrap());
        let got = guest.await.unwrap();
        let mut expected = 16i32.to_be_bytes().to_vec();
        expected.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        expected.extend_from_slice(&1234i32.to_be_bytes());
        expected.extend_from_slice(&0x5EC2_E7u32.to_be_bytes());
        assert_eq!(got, expected);
        // Unknown keys go nowhere and are not an error.
        assert!(!forward(&map, &key(1234, &[0; 4])).await.unwrap());
    }
}
