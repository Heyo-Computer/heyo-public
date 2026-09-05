//! Client-facing TLS: a rustls acceptor built from PEM files, with mtime-based
//! hot reload.
//!
//! Cert lifecycle is external (certbot/Let's Encrypt): the pooler only reads
//! `fullchain.pem` + `privkey.pem` and rebuilds its acceptor when either file
//! changes on disk. That makes certbot's ~60-day renewals take effect on the
//! next client handshake — no restart, no signal. A failed reload keeps
//! serving the previous cert (warn, never drop TLS mid-flight).

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use anyhow::{Context, Result};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// (cert mtime, key mtime) — the reload trigger.
type Mtimes = (SystemTime, SystemTime);

pub struct TlsReloader {
    cert_path: PathBuf,
    key_path: PathBuf,
    cached: RwLock<(Mtimes, TlsAcceptor)>,
}

impl TlsReloader {
    /// Build eagerly so a bad cert/key fails pooler startup, not the first
    /// client handshake.
    pub fn new(cert_path: PathBuf, key_path: PathBuf) -> Result<Self> {
        let acceptor = build_acceptor(&cert_path, &key_path)?;
        let mtimes = stat_mtimes(&cert_path, &key_path)?;
        Ok(Self {
            cert_path,
            key_path,
            cached: RwLock::new((mtimes, acceptor)),
        })
    }

    /// The acceptor to use for one handshake. Stats both PEM files (cheap) and
    /// rebuilds when either changed — i.e. certbot renewed.
    pub fn acceptor(&self) -> TlsAcceptor {
        let current = stat_mtimes(&self.cert_path, &self.key_path).ok();
        {
            let cached = self.cached.read().unwrap();
            if current.is_none() || current == Some(cached.0) {
                // Unstat-able (transiently mid-rotation?) or unchanged: serve
                // what we have.
                return cached.1.clone();
            }
        }
        let mtimes = current.unwrap();
        match build_acceptor(&self.cert_path, &self.key_path) {
            Ok(acceptor) => {
                info!("TLS cert reloaded from {}", self.cert_path.display());
                let mut cached = self.cached.write().unwrap();
                *cached = (mtimes, acceptor.clone());
                acceptor
            }
            Err(e) => {
                warn!(
                    "TLS cert changed on disk but reload failed ({e:#}); \
                     continuing with the previous cert"
                );
                // Remember the mtimes anyway so we don't retry (and re-warn)
                // on every handshake until the files change again.
                let mut cached = self.cached.write().unwrap();
                cached.0 = mtimes;
                cached.1.clone()
            }
        }
    }
}

fn stat_mtimes(cert: &PathBuf, key: &PathBuf) -> Result<Mtimes> {
    Ok((
        std::fs::metadata(cert)?.modified()?,
        std::fs::metadata(key)?.modified()?,
    ))
}

fn build_acceptor(cert_path: &PathBuf, key_path: &PathBuf) -> Result<TlsAcceptor> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("reading TLS cert chain {}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing TLS cert chain {}", cert_path.display()))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", cert_path.display());
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("reading TLS private key {}", key_path.display()))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS server config (cert/key mismatch?)")?;
    Ok(TlsAcceptor::from(std::sync::Arc::new(config)))
}
