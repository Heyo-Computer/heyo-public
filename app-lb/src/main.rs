//! app-lb: an application load balancer for heyvm Firecracker/KVM microVMs.
//!
//! Register a deployment (VM template + routes + scaling policy) against the
//! admin API and the proxy routes traffic to a pool of VMs, booting and reaping
//! them to match load.

mod acme;
mod admin;
mod artifact;
mod auth;
mod autoscale;
mod config;
mod deployment;
mod disks;
mod dns;
mod guard;
mod health;
mod jobs;
mod metrics;
mod obs;
mod proxy;
mod registry;
mod secrets;
mod siem;
mod site;
mod tls;
mod tokens;
mod unpack;
mod vm;
mod workflows;

use crate::acme::{AcmeConfig, AcmeManager, ChallengeTable};
use crate::admin::AdminApi;
use crate::auth::Authenticator;
use crate::autoscale::Autoscaler;
use crate::config::LbConfig;
use crate::jobs::{JobConfig, Jobs};
use crate::metrics::Metrics;
use crate::proxy::LbProxy;
use crate::registry::Registry;
use crate::secrets::SecretStore;
use crate::tls::{CertStore, SniResolver};
use crate::vm::VmManager;
use pingora_core::listeners::tls::TlsSettings;
use pingora_core::server::Server;
use pingora_core::services::background::background_service;
use std::sync::Arc;

fn config_from_env() -> LbConfig {
    let mut cfg = LbConfig::default();
    if let Ok(v) = std::env::var("APP_LB_PROXY_ADDR") {
        cfg.proxy_addr = v;
    }
    if let Ok(v) = std::env::var("APP_LB_ADMIN_ADDR") {
        cfg.admin_addr = v;
    }
    if let Ok(v) = std::env::var("APP_LB_STATE_PATH") {
        cfg.state_path = v;
    }
    if let Ok(v) = std::env::var("APP_LB_SECRETS_PATH") {
        cfg.secrets_path = v;
    }
    if let Ok(v) = std::env::var("APP_LB_TOKENS_PATH") {
        cfg.tokens_path = v;
    }
    if let Ok(v) = std::env::var("APP_LB_GUARD_PATH") {
        cfg.guard_path = v;
    }
    if let Ok(v) = std::env::var("APP_LB_NAME") {
        cfg.name = v;
    }
    if let Ok(v) = std::env::var("APP_LB_DAEMON_URL") {
        cfg.daemon_url = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_DASHBOARD_USER") {
        cfg.dashboard_user = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_DASHBOARD_PASSWORD") {
        cfg.dashboard_password = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_ADMIN_AUTH") {
        cfg.admin_auth = matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    if let Ok(v) = std::env::var("APP_LB_PROXY_TLS_ADDR") {
        cfg.tls_addr = v;
        cfg.tls_addr_explicit = true;
    }
    if let Ok(v) = std::env::var("APP_LB_TLS_CERT") {
        cfg.tls_cert_path = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_TLS_KEY") {
        cfg.tls_key_path = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_EMAIL") {
        cfg.acme_email = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_DIR") {
        cfg.acme_dir = v;
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_DIRECTORY") {
        cfg.acme_directory = v;
    }
    if let Ok(v) = std::env::var("APP_LB_BUILD_DIR") {
        cfg.build_dir = v;
    }
    if let Ok(v) = std::env::var("APP_LB_HEYVM_BIN") {
        cfg.heyvm_bin = v;
    }
    if let Ok(v) = std::env::var("APP_LB_ART_BIN") {
        cfg.art_bin = v;
    }
    if let Ok(v) = std::env::var("APP_LB_IMAGES_DIR") {
        cfg.images_dir = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_GIT_BIN") {
        cfg.git_bin = v;
    }
    if let Ok(v) = std::env::var("APP_LB_AWS_BIN") {
        cfg.aws_bin = v;
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_WILDCARD") {
        cfg.acme_wildcards = v
            .split(',')
            .map(|d| d.trim().trim_start_matches("*.").trim_end_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
    }
    if let Ok(v) = std::env::var("APP_LB_ROUTE53_ZONE_ID") {
        cfg.route53_zone_id = Some(v.trim().to_string()).filter(|z| !z.is_empty());
    }
    if let Ok(v) = std::env::var("APP_LB_UPDATE_SHELL") {
        cfg.update_shell = v;
    }
    if let Ok(v) = std::env::var("APP_LB_BUILD_TIMEOUT_SECS") {
        match v.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => cfg.build_timeout_secs = secs,
            _ => panic!("APP_LB_BUILD_TIMEOUT_SECS must be a positive number of seconds, got {v:?}"),
        }
    }
    if let Ok(v) = std::env::var("APP_LB_HEYVM_HOME") {
        cfg.heyvm_home = Some(v);
    }
    cfg
}

/// Install the subscriber: stderr always, plus a layer that forwards app-lb's own
/// events to app-obs when one is configured.
///
/// The `EnvFilter` sits in front of both, so `RUST_LOG` still decides what app-lb
/// logs *at all* and shipping only ever sees a subset of that. The layer applies
/// its own INFO floor on top — see `obs::EventLayer`.
fn init_tracing(events: Option<obs::LogSink>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,app_lb=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(events.map(obs::EventLayer::new))
        .init();
}

fn main() {
    // Before the subscriber, because shipping app-lb's own events means adding a
    // layer to it, and a subscriber can only be built once. Reads the environment
    // and allocates a channel — no threads, nothing that a later fork would lose.
    //
    // A misconfigured endpoint disables shipping and is reported once the
    // subscriber exists; it is deliberately not fatal. Panicking here would let a
    // typo in the *observability* configuration take the data plane down, which is
    // the one thing `obs` is built not to do.
    let (obs, obs_error) = match obs::from_env() {
        Ok(obs) => (obs, None),
        Err(e) => (None, Some(e)),
    };
    init_tracing(obs.as_ref().and_then(|o| o.events.clone()));
    if let Some(e) = obs_error {
        tracing::error!(
            error = %e,
            "APP_LB_OBS_URL is unusable, so no logs will be shipped to app-obs; \
             everything else starts normally",
        );
    }

    // rustls needs a process-level `CryptoProvider` chosen explicitly whenever
    // more than one is compiled in, and app-lb's graph has both `ring` (iroh,
    // hickory) and `aws-lc-rs` (instant-acme). Without this the *first* rustls
    // handshake panics rather than erroring — which lands in the ACME background
    // service, the only part of app-lb that speaks TLS as a client.
    //
    // pingora installed this itself under its `rustls` feature; the `openssl`
    // feature app-lb now uses never runs that code, so it belongs here. Must
    // happen before any service starts.
    //
    // `Err` means a provider is already installed, which is the desired end
    // state either way.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("rustls crypto provider was already installed");
    }

    let cfg = config_from_env();

    // Fail fast on a gate that can't enforce anything: asking to protect the
    // admin API while giving it no credential would silently leave it open.
    if cfg.admin_auth && cfg.dashboard_password.is_none() {
        panic!(
            "APP_LB_ADMIN_AUTH is set but APP_LB_DASHBOARD_PASSWORD is not; the admin \
             gate reuses the dashboard credentials, so set a password or unset APP_LB_ADMIN_AUTH"
        );
    }

    // Pre-flight the listener addresses.
    //
    // Pingora binds inside a service task, and a failure there panics *that task
    // only* — the process survives with a dead proxy, the admin API still
    // answering, and the supervisor reporting a healthy service. Checking here
    // turns that silent half-dead state into a startup failure naming the port
    // and the reason.
    for addr in [Some(&cfg.proxy_addr), cfg.tls_enabled().then_some(&cfg.tls_addr)]
        .into_iter()
        .flatten()
    {
        if let Err(e) = std::net::TcpListener::bind(addr) {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                "; binding a port below 1024 as a non-root user needs \
                 `setcap 'cap_net_bind_service=+ep'` on the binary (re-run it after every \
                 reinstall — capabilities do not survive replacing the file), or \
                 `sysctl net.ipv4.ip_unprivileged_port_start=80`"
            } else {
                ""
            };
            panic!("cannot bind {addr}: {e}{hint}");
        }
    }

    // Setting the HTTPS address is a clear statement of intent, but it only says
    // *where* to bind, not whether to. Without ACME or a static cert there is
    // nothing to serve, and the listener is skipped — previously in silence,
    // which looks identical to a bind that failed.
    if cfg.tls_addr_explicit && !cfg.tls_enabled() {
        tracing::warn!(
            tls = %cfg.tls_addr,
            "APP_LB_PROXY_TLS_ADDR is set but no HTTPS listener will be bound: TLS needs \
             either APP_LB_ACME_EMAIL (automatic certificates) or an APP_LB_TLS_CERT / \
             APP_LB_TLS_KEY pair. Set one of them, or unset APP_LB_PROXY_TLS_ADDR.",
        );
    }

    // HTTP-01 validation is fetched on port 80 and nowhere else, so a proxy
    // bound anywhere else can only be validated if something in front forwards
    // that path. Worth saying loudly at startup rather than leaving it to be
    // diagnosed from a CA error much later.
    if cfg.acme_enabled() && !cfg.proxy_addr.ends_with(":80") {
        tracing::warn!(
            proxy = %cfg.proxy_addr,
            "ACME is enabled but the plaintext proxy is not on port 80; Let's Encrypt \
             validates http-01 on port 80 only, so issuance will fail unless something \
             forwards /.well-known/acme-challenge/ to this listener",
        );
    }

    // A wildcard is only issued over DNS-01, which needs somewhere to write the
    // challenge record. Warned rather than fatal: everything else — including
    // per-host issuance — still works, and taking the LB down over a certificate
    // that is not yet configured would be the wrong trade.
    if !cfg.acme_wildcards.is_empty() {
        if !cfg.acme_enabled() {
            tracing::warn!(
                "APP_LB_ACME_WILDCARD is set but APP_LB_ACME_EMAIL is not, so no \
                 certificates are issued at all; set the contact address to enable ACME",
            );
        } else if cfg.route53_zone_id.is_none() {
            tracing::warn!(
                wildcards = ?cfg.acme_wildcards,
                "APP_LB_ACME_WILDCARD is set without APP_LB_ROUTE53_ZONE_ID; wildcard \
                 certificates are issued over DNS-01 and there is nowhere to publish the \
                 challenge, so these domains will be served the fallback certificate",
            );
        } else {
            tracing::info!(
                wildcards = ?cfg.acme_wildcards,
                aws_bin = %cfg.aws_bin,
                "wildcard certificates enabled; hosts beneath these domains will not be \
                 issued certificates of their own",
            );
        }
    }

    let registry = Arc::new(Registry::new(&cfg.state_path));
    match registry.load() {
        Ok(0) => {
            tracing::info!(dir = %registry.state_dir().display(), "no persisted deployments")
        }
        Ok(n) => tracing::info!(count = n, "restored deployments"),
        Err(e) => tracing::error!(error = %e, "failed to load persisted state; starting empty"),
    }

    // Beside the deployment state, derived the same way: `app-lb-state.json`
    // gives `app-lb-workflows.d/`. One directory per object kind keeps a
    // listing readable and a delete unambiguous.
    let workflows = Arc::new(crate::workflows::WorkflowStore::new(
        crate::workflows::workflow_dir(&cfg.state_path),
    ));
    match workflows.load() {
        (0, 0) => tracing::info!(dir = %workflows.dir().display(), "no CI workflows"),
        (n, 0) => tracing::info!(count = n, "restored CI workflows"),
        (n, skipped) => tracing::warn!(
            count = n,
            skipped,
            dir = %workflows.dir().display(),
            "restored CI workflows; some objects were unreadable and were left on disk"
        ),
    }
    // A deregistration whose file removal failed would otherwise resurrect the
    // deployment on this start. Declines to run if the load above skipped
    // anything, so it can never delete a spec it merely failed to understand.
    match registry.sweep_orphan_state() {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "removed orphaned deployment state files"),
        Err(e) => tracing::warn!(error = %e, "failed to sweep orphaned state files"),
    }

    // Secrets are read straight from the environment rather than through
    // `LbConfig`, which derives `Debug` and `Serialize` — key material that is
    // never in the struct can never be printed out of it.
    let secret_key = std::env::var("APP_LB_SECRET_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .map(|k| secrets::derive_key(&k));
    let secrets = Arc::new(SecretStore::new(&cfg.secrets_path, secret_key));
    match secrets.load() {
        Ok(0) => tracing::info!(path = %secrets.path().display(), "no stored secrets"),
        Ok(n) => tracing::info!(
            count = n,
            encrypted = secrets.is_encrypted(),
            "loaded secrets"
        ),
        // Fatal, unlike a bad deployment spec: starting with an empty store
        // would let the first write replace secrets that are perfectly good and
        // merely unreadable with the key this process was given.
        Err(e) => panic!(
            "cannot read {}: {e}",
            std::path::Path::new(&cfg.secrets_path).display()
        ),
    }
    let tokens = Arc::new(tokens::TokenStore::new(&cfg.tokens_path));
    match tokens.load() {
        Ok(0) => tracing::info!(path = %tokens.path().display(), "no app-tokens"),
        Ok(n) => tracing::info!(count = n, "loaded app-tokens"),
        // Fatal for the same reason the secret store is: starting with an empty
        // token store would revoke every client at once, and would look exactly
        // like a healthy server until their calls started failing.
        Err(e) => panic!(
            "cannot read {}: {e}",
            std::path::Path::new(&cfg.tokens_path).display()
        ),
    }
    if tokens.sweep_expired(deployment::now_secs()) > 0 {
        let _ = tokens.persist();
    }

    // Block rules, restored before the data plane accepts anything. Fatal on a
    // corrupt file, for the same reason the token store is: coming up with an
    // empty rule set would silently readmit whatever an operator blocked during
    // an incident, and would look exactly like a healthy server while doing it.
    let guard = Arc::new(guard::Guard::from_env(&cfg.guard_path));
    match guard.load(deployment::now_secs()) {
        Ok(0) => tracing::info!(path = %guard.path().display(), "no guard rules"),
        Ok(n) => tracing::info!(count = n, enforcing = guard.enforcing(), "loaded guard rules"),
        Err(e) => panic!("cannot read {}: {e}", guard.path().display()),
    }
    if !guard.enforcing() {
        tracing::warn!(
            "APP_LB_GUARD_ENFORCE=0: guard rules are matched and counted but nothing is \
             refused — this is a dry run, not protection",
        );
    }

    if !secrets.is_encrypted() {
        tracing::info!(
            path = %secrets.path().display(),
            "secrets are stored in plaintext (mode 0600); set APP_LB_SECRET_KEY to encrypt them",
        );
    }

    // Jobs run `git`, `docker` and — for a static deployment's update — whatever
    // its spec says, on this host. An ungated admin API is a remote code
    // execution surface. It was already a "boot VMs of your choosing" surface,
    // but this is worth saying out loud.
    if !cfg.admin_auth {
        tracing::warn!(
            admin = %cfg.admin_addr,
            "the deployment/secret/job API is not authenticated (set APP_LB_ADMIN_AUTH=1 \
             with APP_LB_DASHBOARD_PASSWORD); POST /deployments/:id/build runs git and \
             docker on this host, and POST /deployments/:id/update runs that deployment's \
             own commands",
        );
    }

    // The key that signs sign-in sessions. Generated on first use and kept, so
    // a restart doesn't sign every user of a gated deployment out. Loaded even
    // when no deployment is gated — one file read, and it means enabling a gate
    // later needs no restart.
    let auth_key_path = std::env::var("APP_LB_AUTH_KEY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| auth::default_key_path());
    // Attack detection over the same requests the access log describes. Built
    // here because everything downstream takes a clone of its sink, and — like
    // `obs::from_env` — it allocates a channel and nothing else, so it survives
    // the fork `run_forever` may do to daemonize.
    //
    // Independent of `obs`: it needs no collector to be useful, and is on unless
    // `APP_LB_SIEM=0`. The sink it is handed here is only for *shipping* alerts
    // onward, which is why it is `None`-tolerant.
    let siem = siem::from_env(
        obs.as_ref().and_then(|o| o.events.clone()),
        obs.as_ref()
            .and_then(|o| o.events.as_ref().or(o.access.as_ref()))
            .map(|s| s.deployment())
            .unwrap_or_else(|| Arc::from(obs::LB_DEPLOYMENT)),
    );

    let auth = Arc::new(Authenticator::new(
        Authenticator::load_key(&auth_key_path)
            .unwrap_or_else(|e| panic!("cannot read or create {}: {e}", auth_key_path.display())),
        secrets.clone(),
        Some(tokens.clone()),
        siem.as_ref().map(|s| s.sink.clone()),
    ));

    let vms = VmManager::new(cfg.daemon_url.clone());

    // The static cert pair, if configured. With ACME on it is the *fallback*,
    // served for any SNI without an issued cert of its own; with ACME off it is
    // the only certificate. Half-configured TLS stays a hard error rather than a
    // silent fallback to plaintext, so nobody thinks a listener is encrypted
    // when it isn't.
    let fallback = match (&cfg.tls_cert_path, &cfg.tls_key_path) {
        (Some(cert), Some(key)) => Some(Arc::new(
            CertStore::load_pair(cert, key)
                .unwrap_or_else(|e| panic!("failed to load TLS cert {cert} / key {key}: {e}")),
        )),
        (None, None) => None,
        _ => panic!("TLS is half-configured: set both APP_LB_TLS_CERT and APP_LB_TLS_KEY, or neither"),
    };

    // Before anything reads the cert directory: a change of ACME directory makes
    // every cached certificate and the saved account invalid, and neither
    // announces that itself.
    if cfg.acme_enabled() {
        acme::reset_if_directory_changed(
            std::path::Path::new(&cfg.acme_dir),
            &cfg.acme_directory,
        );
    }

    // Loaded from disk *before* the listener binds: the acceptor holds no
    // certificate of its own, so anything not in the store at bind time gets the
    // fallback (or a failed handshake) until ACME reissues it.
    let certs = Arc::new(CertStore::new(
        std::path::Path::new(&cfg.acme_dir).join("certs"),
        fallback,
    ));
    if cfg.tls_enabled() {
        match certs.load_from_disk() {
            0 => tracing::info!("no cached certificates"),
            n => tracing::info!(count = n, "loaded cached certificates"),
        }
    }

    // Shared between the ACME manager (which publishes challenge responses) and
    // the proxy (which serves them).
    let challenges = Arc::new(ChallengeTable::new());

    // One metrics registry, shared by the proxy (request latency), the
    // autoscaler (cold-start timing, scaling activity), and the admin API
    // (which serves the dashboard from it).
    let metrics = Arc::new(Metrics::new());

    // Note: nothing above may spawn threads or build a runtime — `run_forever`
    // may fork for daemonization and anything created before it would be lost.
    let mut server = Server::new(None).expect("failed to create server");
    server.bootstrap();

    // `background_service` hands back an Arc to the same task the service runs,
    // which is how the admin API reaches the autoscaler to tear deployments down.
    let autoscaler_svc = background_service(
        "autoscaler",
        Autoscaler::new(registry.clone(), vms.clone(), metrics.clone()),
    );
    let autoscaler = autoscaler_svc.task();

    // Disk inventory and reclamation. Optional in exactly one way: without a
    // resolvable daemon data directory there is nothing to inventory, and an LB
    // whose host keeps its VMs elsewhere should not refuse to start over it. The
    // routes then answer 503 and say why.
    let disks = match disks::DiskConfig::from_env(&cfg) {
        Ok(disk_cfg) => {
            let store = Arc::new(disks::DiskStore::new(
                disk_cfg,
                vms,
                registry.clone(),
            ));
            match store.load() {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "loaded disk retention policies"),
                // Not fatal, unlike the secret store: the worst case is that a
                // `retain` flag is missed, and the sweep's other four guards
                // (running, claimed, age, daemon reachable) still hold.
                Err(e) => tracing::error!(
                    error = %e,
                    "cannot read disk retention policies; every disk will be treated as \
                     unretained until this is fixed",
                ),
            }
            let c = store.config();
            if c.ttl_secs == 0 {
                tracing::info!(
                    data_dir = %c.data_dir.display(),
                    "disk expiry is off (APP_LB_DISK_TTL_SECS=0); disks are listed and can be \
                     purged by hand, but nothing is reclaimed automatically",
                );
            } else {
                // Counted here rather than left for the first sweep to
                // discover, because on a host that has been booting VMs for
                // months the honest answer is "most of them" — and an operator
                // who never opens /storage should still learn that from the log
                // before it happens rather than after. An upper bound: without
                // the daemon this cannot tell a running sandbox from residue.
                let (total, due, bytes) = store.expiry_preview();
                tracing::warn!(
                    data_dir = %c.data_dir.display(),
                    ttl_secs = c.ttl_secs,
                    sweep_secs = c.sweep_secs,
                    archive = c.bucket.is_some(),
                    disks = total,
                    eligible = due,
                    gib = bytes / (1 << 30),
                    "disk expiry is ON: up to {due} of {total} sandbox disks on this host are \
                     already older than the retention window and will be reclaimed by the \
                     first sweep, in {}s. Open /storage to review them, mark the ones to keep \
                     as retained, or set APP_LB_DISK_TTL_SECS=0 to turn expiry off",
                    c.sweep_secs,
                );
            }
            Some(store)
        }
        Err(e) => {
            tracing::warn!("{e}. Disk management is disabled");
            None
        }
    };

    // Where an artifact pull writes a rootfs. Resolved once, and kept as a
    // `Result` rather than unwrapped: without `HOME` or `MVM_DATA_DIR` there is
    // no way to know where heyvmd looks, but an LB whose deployments all name
    // prebuilt images never needs to know, and it should not refuse to start
    // over a directory it will not use. The error travels to the pull instead.
    let images_dir = match &cfg.images_dir {
        Some(dir) => Ok(std::path::PathBuf::from(dir)),
        None => artifact::default_images_dir(cfg.heyvm_home.as_deref()),
    };
    if let Err(e) = &images_dir {
        tracing::warn!("{e}. Artifact pulls will fail until APP_LB_IMAGES_DIR is set");
    }

    // The job runner is not a service: it has no loop of its own, it runs a task
    // per job. It needs the autoscaler because finishing an image build means
    // rewriting `vm.image` and tearing the old pool down — the same swap the
    // admin API's update path does.
    let jobs = Arc::new(Jobs::new(
        JobConfig {
            work_dir: cfg.build_dir.clone().into(),
            heyvm_bin: cfg.heyvm_bin.clone(),
            art_bin: cfg.art_bin.clone(),
            images_dir,
            git_bin: cfg.git_bin.clone(),
            shell: cfg.update_shell.clone(),
            timeout: std::time::Duration::from_secs(cfg.build_timeout_secs),
            home: cfg.heyvm_home.clone(),
        },
        registry.clone(),
        autoscaler.clone(),
        secrets.clone(),
        // Job output is app-lb's own output, so it rides the same switch as the
        // event stream (`APP_LB_OBS_EVENTS`) rather than getting a third one.
        obs.as_ref().and_then(|o| o.events.clone()),
    ));

    // ACME runs only when a contact address is configured. Its `Notify` goes to
    // the admin API so registering a deployment starts issuance immediately
    // rather than at the next 12-hour sweep.
    let acme_svc = cfg.acme_email.clone().map(|email| {
        background_service(
            "acme",
            AcmeManager::new(
                registry.clone(),
                certs.clone(),
                challenges.clone(),
                AcmeConfig {
                    email,
                    dir: cfg.acme_dir.clone().into(),
                    directory_url: cfg.acme_directory.clone(),
                    proxy_addr: cfg.proxy_addr.clone(),
                    wildcards: cfg.acme_wildcards.clone(),
                    dns: cfg
                        .route53_zone_id
                        .clone()
                        .map(|zone| dns::Route53::new(cfg.aws_bin.clone(), zone)),
                },
            ),
        )
    });
    let acme_signal = acme_svc.as_ref().map(|svc| svc.task().signal());

    let admin_svc = background_service(
        "admin",
        AdminApi::new(
            cfg.admin_addr.clone(),
            registry.clone(),
            autoscaler,
            metrics.clone(),
            cfg.name.clone(),
            cfg.dashboard_user.clone(),
            cfg.dashboard_password.clone(),
            cfg.admin_auth,
            certs.clone(),
            acme_signal,
            secrets,
            workflows,
            tokens,
            jobs,
            obs.as_ref().map(|o| o.stats.clone()),
            siem.as_ref(),
            guard.clone(),
            disks.clone(),
            admin::PublicUrl::from_config(cfg.tls_enabled(), &cfg.proxy_addr, &cfg.tls_addr),
        ),
    );

    let mut proxy_svc = pingora_proxy::http_proxy_service(
        &server.configuration,
        LbProxy::new(
            registry,
            metrics,
            challenges,
            auth,
            obs.as_ref().and_then(|o| o.access.clone()),
            siem.as_ref().map(|s| s.sink.clone()),
            guard.clone(),
        ),
    );
    proxy_svc.add_tcp(&cfg.proxy_addr);

    // HTTPS listener, alongside the plaintext one. The acceptor is built with no
    // certificate attached: `CertStore` supplies one per handshake keyed on SNI,
    // which is what lets a certificate issued moments ago serve without a
    // restart. See `src/tls.rs`.
    if cfg.tls_enabled() {
        let settings = TlsSettings::with_callbacks(Box::new(SniResolver::new(certs)))
            .expect("failed to build TLS settings");
        proxy_svc.add_tls_with_settings(&cfg.tls_addr, None, settings);
        tracing::info!(tls = %cfg.tls_addr, "HTTPS listener enabled");
    }

    tracing::info!(proxy = %cfg.proxy_addr, admin = %cfg.admin_addr, "starting app-lb");

    let autoscaler_handle = server.add_service(autoscaler_svc);
    server.add_service(admin_svc);
    // Log shipping, when `APP_LB_OBS_URL` is set. Pointedly *not* a dependency of
    // the proxy handle below: whether this service is running, and whether app-obs
    // answers it, must make no difference to serving traffic.
    if let Some(obs) = obs {
        server.add_service(background_service("obs", obs.shipper));
    }
    // Detection, on the same terms as log shipping: deliberately not a
    // dependency of the proxy handle. A stalled or failed analyzer must degrade
    // to losing findings, never to holding up traffic.
    if let Some(siem) = siem {
        server.add_service(background_service("siem", siem.engine));
    }
    if let Some(acme_svc) = acme_svc {
        server.add_service(acme_svc);
    }
    // Disk reclamation, on the same terms as the two above: never a dependency
    // of the proxy handle. It walks directories and shells out to `aws`, and
    // neither may hold up traffic.
    if let Some(disks) = disks {
        server.add_service(background_service("disks", disks::DiskSweeper::new(disks)));
    }
    let proxy_handle = server.add_service(proxy_svc);
    // Don't accept traffic until the autoscaler has adopted existing VMs and
    // built the warm pool; otherwise the first requests all eat a cold start.
    proxy_handle.add_dependency(&autoscaler_handle);

    server.run_forever();
}
