//! One management owner supervises a forwarding-only subprocess.
//! This is process isolation, not a hot-takeover protocol: only one child is
//! started at a time, and a replacement follows confirmed process exit.

use crate::metrics::Metrics;
use crate::obs::LogSink;
use crate::request_control::RequestControl;
use crate::siem::SecuritySink;
use crate::tls::{CertSnapshot, CertStore, SniResolver};
use crate::worker_rpc::{self, Client};
use async_trait::async_trait;
use pingora_core::server::{Server, ShutdownWatch};
use pingora_core::services::background::{background_service, BackgroundService};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::JoinSet;

const VERSION: u16 = 1;

async fn stopped(shutdown: &mut ShutdownWatch) {
    // Drop watch::Ref before returning: select branches may await child exit.
    let _ = shutdown.wait_for(|stop| *stop).await;
}

#[derive(Serialize, Deserialize)]
struct Bootstrap {
    version: u16,
    proxy_addr: String,
    tls_addr: Option<String>,
    certificates: CertSnapshot,
}

pub struct Supervisor {
    pub control: Arc<RequestControl>,
    pub metrics: Arc<Metrics>,
    pub access_log: Option<LogSink>,
    pub security: Option<SecuritySink>,
    pub certs: Arc<CertStore>,
    pub proxy_addr: String,
    pub tls_addr: Option<String>,
}

impl Supervisor {
    async fn incarnation(&self, shutdown: &mut ShutdownWatch) -> Result<(), String> {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?.as_nanos();
        let dir = std::env::temp_dir().join(format!("applb-{}-{nonce}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|e| e.to_string())?;
        let _directory = SocketDirectory(dir.clone());
        let requests = UnixListener::bind(dir.join("requests")).map_err(|e| e.to_string())?;
        let snapshots = UnixListener::bind(dir.join("bootstrap")).map_err(|e| e.to_string())?;
        let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command.arg("--forwarding-worker").arg(&dir).kill_on_drop(true);
        // A manager killed without a shutdown callback must not leave a
        // forwarding child serving with abandoned admission ownership.
        #[cfg(target_os = "linux")]
        unsafe {
            let parent = libc::getpid();
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::other("manager exited before worker exec"));
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        tracing::info!(worker_pid = child.id(), "forwarding worker started");
        let (exited, witness) = watch::channel(false);
        let mut tasks = JoinSet::new();
        let exit = loop {
            tokio::select! {
                status = child.wait() => break status,
                _ = stopped(shutdown) => {
                    if let Some(pid) = child.id() {
                        // Signal only our still-owned, unreaped child.
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                    }
                    break match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
                        Ok(status) => status,
                        Err(_) => {
                            child.start_kill().unwrap_or_else(|e| supervision_failed(&e.to_string()));
                            child.wait().await
                        }
                    };
                }
                accepted = requests.accept() => {
                    let (stream, _) = accepted.unwrap_or_else(|e| supervision_failed(&e.to_string()));
                    let control = self.control.clone();
                    let metrics = self.metrics.clone();
                    let access = self.access_log.clone();
                    let security = self.security.clone();
                    let witness = witness.clone();
                    tasks.spawn(async move {
                        if let Err(error) = worker_rpc::serve(stream, control, metrics, access, security, witness).await {
                            tracing::debug!(%error, "worker request control ended");
                        }
                    });
                }
                accepted = snapshots.accept() => {
                    let (mut stream, _) = accepted.unwrap_or_else(|e| supervision_failed(&e.to_string()));
                    let bootstrap = Bootstrap {
                        version: VERSION, proxy_addr: self.proxy_addr.clone(), tls_addr: self.tls_addr.clone(),
                        certificates: self.certs.snapshot().unwrap_or_else(|e| supervision_failed(&e.to_string())),
                    };
                    tasks.spawn(async move {
                        if let Err(error) = worker_rpc::write_frame(&mut stream, &bootstrap).await {
                            tracing::debug!(%error, "worker bootstrap connection ended");
                        }
                    });
                }
                result = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = result { supervision_failed(&error.to_string()); }
                }
            }
        };
        let status = exit.unwrap_or_else(|e| supervision_failed(&e.to_string()));
        tracing::info!(%status, "forwarding worker exit confirmed");
        let _ = exited.send(true);
        // Cancellation is safe only here: the child's sockets and streams
        // are gone, so no reservation can still represent live forwarding.
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

#[async_trait]
impl BackgroundService for Supervisor {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        while !*shutdown.borrow() {
            if let Err(error) = self.incarnation(&mut shutdown).await {
                // Never spawn a new incarnation after an unconfirmed exit.
                tracing::error!(%error, "forwarding supervision failed; stopping manager");
                std::process::exit(1);
            }
            tokio::select! {
                _ = stopped(&mut shutdown) => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
}

struct SocketDirectory(PathBuf);
impl Drop for SocketDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(%error, "could not remove worker socket directory");
        }
    }
}

async fn bootstrap(dir: &Path) -> Result<Bootstrap, String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = UnixStream::connect(dir.join("bootstrap")).await.map_err(|e| e.to_string())?;
        let value: Bootstrap = worker_rpc::read_frame(&mut stream).await?;
        if value.version != VERSION { return Err("worker bootstrap protocol mismatch".into()); }
        Ok(value)
    }).await.map_err(|_| "worker bootstrap timed out".to_string())?
}

struct CertificateWatch { dir: PathBuf, certs: Arc<CertStore> }
#[async_trait]
impl BackgroundService for CertificateWatch {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        loop {
            tokio::select! {
                _ = stopped(&mut shutdown) => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            let snapshot = bootstrap(&self.dir).await.unwrap_or_else(|error| control_lost(&error));
            self.certs.replace_snapshot(snapshot.certificates)
                .unwrap_or_else(|error| control_lost(&error.to_string()));
        }
    }
}

/// Called before any persistent management store is opened.
pub fn run(dir: PathBuf) -> ! {
    crate::init_tracing(None);
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let initial = {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .expect("worker bootstrap runtime");
        runtime.block_on(bootstrap(&dir)).unwrap_or_else(|error| control_lost(&error))
    };
    let certs = Arc::new(CertStore::from_snapshot(initial.certificates)
        .unwrap_or_else(|error| control_lost(&error.to_string())));
    let mut server = Server::new(None).expect("worker server");
    server.bootstrap();
    let mut proxy = pingora_proxy::http_proxy_service(
        &server.configuration, crate::proxy::LbProxy::new(Client::new(dir.join("requests"))),
    );
    proxy.add_tcp(&initial.proxy_addr);
    if let Some(addr) = initial.tls_addr {
        let settings = pingora_core::listeners::tls::TlsSettings::with_callbacks(Box::new(SniResolver::new(certs.clone())))
            .expect("worker TLS settings");
        proxy.add_tls_with_settings(&addr, None, settings);
    }
    server.add_service(background_service("worker-certificates", CertificateWatch { dir, certs }));
    server.add_service(proxy);
    server.run_forever();
}

pub fn control_lost(error: &str) -> ! {
    tracing::error!(%error, "worker lost management authority; exiting before replacement");
    std::process::exit(1)
}

fn supervision_failed(error: &str) -> ! {
    // Exit without unwinding request state into zero counters while the child
    // may still exist. Linux's parent-death signal terminates that child.
    tracing::error!(%error, "worker exit unconfirmed; stopping management authority");
    std::process::exit(1)
}
