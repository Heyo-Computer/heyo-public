//! Minimal Postgres frontend/backend startup parsing.
//!
//! We only read enough of the v3 protocol to (a) answer the SSL/GSS preamble —
//! upgrading to TLS when the pooler has a cert, declining otherwise — and
//! (b) read the database and replication mode from the `StartupMessage`.
//! Physical replication ignores the database and routes by authenticated role.
//! The raw startup bytes are kept so the proxy can
//! replay them verbatim to the VM (always plaintext upstream — TLS terminates
//! here), leaving the rest of the session a pure byte splice.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::tls::TlsReloader;

// Magic protocol "version" codes that aren't real StartupMessages.
const SSL_REQUEST_CODE: i32 = 80877103;
const GSS_ENC_REQUEST_CODE: i32 = 80877104;
const PROTOCOL_V3: i32 = 196608; // 3.0

/// The client connection after the preamble: plain TCP, or TLS-wrapped when
/// the client asked for SSL and we hold a cert. Boxed so the rest of the
/// pipeline (splice) is oblivious to which it got. (Trait objects only take
/// one non-auto trait, hence the combined `ClientIo`.)
pub trait ClientIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ClientIo for T {}
pub type ClientStream = Box<dyn ClientIo>;

pub struct StartupInfo {
    /// The database name from the startup packet — our schema key.
    pub database: String,
    /// The role from the startup packet. Replayed upstream verbatim, and — for
    /// a dedicated database — the key the pooler resolves the client's
    /// credential from (see [`crate::dedicated`]).
    pub user: String,
    /// Physical replication has no PostgreSQL database context. Its VM must
    /// be selected from the authenticated replication login, not `database`.
    pub physical_replication: bool,
    /// Any explicit replication startup parameter, including logical
    /// `replication=database`. Writer routing never intercepts these sessions.
    pub replication_requested: bool,
    /// The full StartupMessage bytes (length prefix included) to replay upstream.
    pub raw: Vec<u8>,
}

/// Read the client's startup handshake and return the (possibly TLS-upgraded)
/// stream plus the resolved schema and the StartupMessage to forward to the VM.
///
/// SSLRequest handling: with `tls` configured we reply `S` and run the rustls
/// handshake, then read the real StartupMessage over TLS; without it we reply
/// `N` and libpq proceeds in plaintext (sslmode=prefer) or aborts
/// (sslmode=require). GSS encryption is always declined.
pub async fn read_startup(
    mut client: TcpStream,
    tls: Option<&TlsReloader>,
) -> Result<(ClientStream, StartupInfo)> {
    loop {
        let (code, len_buf, body) = read_message(&mut client).await?;
        match code {
            SSL_REQUEST_CODE if tls.is_some() => {
                client.write_all(b"S").await?;
                client.flush().await?;
                let acceptor = tls.unwrap().acceptor();
                let mut tls_stream = acceptor
                    .accept(client)
                    .await
                    .context("TLS handshake with client")?;
                let info = read_startup_plain(&mut tls_stream).await?;
                return Ok((Box::new(tls_stream), info));
            }
            SSL_REQUEST_CODE | GSS_ENC_REQUEST_CODE => {
                // Decline encryption; libpq then sends a plain StartupMessage.
                client.write_all(b"N").await?;
                client.flush().await?;
                continue;
            }
            PROTOCOL_V3 => {
                let info = parse_startup_message(&len_buf, &body)?;
                return Ok((Box::new(client), info));
            }
            other => bail!("unsupported startup protocol code: {other}"),
        }
    }
}

/// The post-preamble loop, generic so it runs over the TLS stream too (where a
/// repeated SSLRequest or a GSS request still gets `N` — no double upgrade).
async fn read_startup_plain<S>(stream: &mut S) -> Result<StartupInfo>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let (code, len_buf, body) = read_message(stream).await?;
        match code {
            SSL_REQUEST_CODE | GSS_ENC_REQUEST_CODE => {
                stream.write_all(b"N").await?;
                stream.flush().await?;
                continue;
            }
            PROTOCOL_V3 => return parse_startup_message(&len_buf, &body),
            other => bail!("unsupported startup protocol code: {other}"),
        }
    }
}

/// One length-prefixed startup-phase message: (code, length prefix, body).
async fn read_message<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(i32, [u8; 4], Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = i32::from_be_bytes(len_buf);
    if len < 8 {
        bail!("startup message length too small: {len}");
    }
    let body_len = (len - 4) as usize;
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body).await?;
    let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    Ok((code, len_buf, body))
}

fn parse_startup_message(len_buf: &[u8; 4], body: &[u8]) -> Result<StartupInfo> {
    let params = parse_params(&body[4..]);
    let user = params.get("user").cloned().unwrap_or_default();
    let database = params
        .get("database")
        .cloned()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| user.clone());
    if database.is_empty() {
        bail!("startup packet had neither database nor user");
    }
    let physical_replication = match params.get("replication").map(String::as_str) {
        None | Some("database") => false,
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" => true,
            "0" | "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "of" | "off" => false,
            _ => bail!("invalid replication startup parameter"),
        },
    };
    let replication_requested = params.contains_key("replication");

    let mut raw = Vec::with_capacity(4 + body.len());
    raw.extend_from_slice(len_buf);
    raw.extend_from_slice(body);
    Ok(StartupInfo { database, user, physical_replication, replication_requested, raw })
}

/// Parse startup bytes received inside the authenticated writer tunnel.  A
/// tunnel carries exactly one v3 StartupMessage: encryption negotiation and
/// oversized or trailing input are rejected rather than replayed to Postgres.
pub(crate) fn parse_forwarded(raw: &[u8]) -> Result<StartupInfo> {
    if raw.len() < 8 || raw.len() > 10 * 1024 { bail!("invalid forwarded startup length"); }
    let len: [u8; 4] = raw[..4].try_into().unwrap();
    if i32::from_be_bytes(len) as usize != raw.len() { bail!("forwarded startup framing mismatch"); }
    let body = &raw[4..];
    if i32::from_be_bytes(body[..4].try_into().unwrap()) != PROTOCOL_V3 {
        bail!("forwarded request is not a PostgreSQL v3 startup");
    }
    // Avoid authenticating one interpretation and replaying another. Require
    // complete UTF-8 pairs, a final terminator, and unambiguous parameter keys.
    let mut params = &body[4..];
    let mut keys = std::collections::HashSet::new();
    while params != [0] {
        let end = params.iter().position(|b| *b == 0).context("unterminated startup key")?;
        let key = std::str::from_utf8(&params[..end]).context("invalid startup key")?;
        if key.is_empty() || !keys.insert(key) { bail!("empty or duplicate startup key"); }
        params = &params[end + 1..];
        let end = params.iter().position(|b| *b == 0).context("unterminated startup value")?;
        std::str::from_utf8(&params[..end]).context("invalid startup value")?;
        params = &params[end + 1..];
    }
    parse_startup_message(&len, body)
}

/// Parameters are a flat `key\0value\0...\0` list terminated by an extra `\0`.
fn parse_params(bytes: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut parts = bytes.split(|&b| b == 0);
    loop {
        let key = match parts.next() {
            Some(k) if !k.is_empty() => k,
            _ => break, // empty key == terminator (or no more pairs)
        };
        let val = parts.next().unwrap_or(&[]);
        map.insert(
            String::from_utf8_lossy(key).into_owned(),
            String::from_utf8_lossy(val).into_owned(),
        );
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_mode_matches_postgres_without_rewriting_startup() {
        for (value, physical) in [("true", true), ("T", true), ("ye", true), ("ON", true),
            ("1", true), ("database", false), ("false", false), ("of", false), ("0", false)] {
            let mut body = PROTOCOL_V3.to_be_bytes().to_vec();
            body.extend_from_slice(format!("user\0acme_pgfcrepl\0database\0replication\0replication\0{value}\0\0").as_bytes());
            let len = ((body.len() + 4) as i32).to_be_bytes();
            let info = parse_startup_message(&len, &body).unwrap();
            assert_eq!(info.physical_replication, physical, "{value}");
            assert!(info.replication_requested);
            assert_eq!(info.database, "replication");
            assert_eq!(info.user, "acme_pgfcrepl");
            assert_eq!(info.raw, [len.as_slice(), body.as_slice()].concat());
        }
        for value in ["", "o", "Database", "truth"] {
            let mut body = PROTOCOL_V3.to_be_bytes().to_vec();
            body.extend_from_slice(format!("user\0acme\0replication\0{value}\0\0").as_bytes());
            let len = ((body.len() + 4) as i32).to_be_bytes();
            assert!(parse_startup_message(&len, &body).is_err(), "{value}");
        }
    }

    #[test]
    fn forwarded_startup_rejects_non_v3_trailing_and_replication_is_visible() {
        let mut body = PROTOCOL_V3.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0acme\0database\0acme\0\0");
        let len = ((body.len() + 4) as i32).to_be_bytes();
        let raw = [len.as_slice(), body.as_slice()].concat();
        assert!(!parse_forwarded(&raw).unwrap().replication_requested);
        let mut trailing = raw.clone(); trailing.push(0);
        assert!(parse_forwarded(&trailing).is_err());
        let mut wrong = raw; wrong[4..8].copy_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        assert!(parse_forwarded(&wrong).is_err());
    }

    #[test]
    fn forwarded_startup_rejects_ambiguous_parameters() {
        for params in [b"user\0alice\0user\0bob\0\0".as_slice(), b"user\0alice\0", b"user\0alice", b"user\0\xff\0\0", b"user\0alice\0\0ignored\0x\0\0"] {
            let mut raw = ((8 + params.len()) as i32).to_be_bytes().to_vec();
            raw.extend_from_slice(&PROTOCOL_V3.to_be_bytes());
            raw.extend_from_slice(params);
            assert!(parse_forwarded(&raw).is_err());
        }
    }
}
