//! Setting up, promoting and tearing down a pairing.
//!
//! Every flow here obeys one rule: **the durable record is written before the
//! thing it describes exists**, and every step is idempotent. A crash can
//! therefore leave a row describing work that never happened — which the
//! operator can retry or abandon — but never work that happened with no row,
//! which would be an orphaned replication slot pinning WAL on a primary with
//! nothing naming it.
//!
//! The ordering of the two sides matters for the same reason. The primary
//! creates its publication and login, but **not** the slot: `CREATE
//! SUBSCRIPTION ... create_slot = true` on the replica is what creates it. So
//! a setup that dies after the primary's half leaves nothing pinning WAL — the
//! publication and the login are inert on their own.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::config::ReplicationConfig;
use crate::registry::SchemaRegistry;

use super::{ReplRecord, Role, State, peer::PeerClient, sql, wire};

/// Length of a generated replication password. Same 144 bits of entropy as a
/// dedicated database's, and generated the same way.
const PASSWORD_LEN: usize = 24;

/// The replication settings, or a message saying how to turn the feature on.
fn cfg(reg: &Arc<SchemaRegistry>) -> Result<&ReplicationConfig> {
    reg.replication_cfg()
        .context("replication is not enabled on this node (set PG_VM_POOL_REPLICATION=1)")
}

/// Resolve a peer's advertised host to an IPv4 address **on this host**.
///
/// The guest microVMs ship with an empty `/etc/resolv.conf`, so a hostname
/// handed to one simply never resolves — the same constraint that makes the S3
/// path pin IPs with `curl --resolve`. IPv4 only, because the guest tap/NAT is.
async fn resolve_v4(host: &str, port: u16) -> Result<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    let addrs = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .with_context(|| format!("resolving {host} timed out"))?
    .with_context(|| format!("resolving {host}"))?;
    addrs
        .filter_map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            std::net::IpAddr::V6(_) => None,
        })
        .find(|ip| !ip.is_loopback())
        .with_context(|| {
            format!(
                "{host} has no non-loopback IPv4 address — a guest VM reaches a peer over the \
                 host's IPv4 NAT, and cannot use a hostname or a loopback address"
            )
        })
}

fn generate_password() -> Result<String> {
    // Reuse the dedicated store's generator: same alphabet, same entropy, same
    // guarantee that the result passes the shared password validation.
    let p = crate::dedicated::generate_password()?;
    debug_assert_eq!(p.chars().count(), PASSWORD_LEN);
    Ok(p)
}

// ---------------------------------------------------------------------------
// Primary side
// ---------------------------------------------------------------------------

/// Wire `database` on this node up as the primary of a pairing with `peer`.
///
/// Synchronous through the peer call, so an operator gets a real answer rather
/// than a spinner: everything that can be refused is refused before anything
/// is created, and the only long step (the replica's schema copy) happens in
/// the *peer's* background.
pub async fn enable_primary(
    reg: &Arc<SchemaRegistry>,
    database: &str,
    peer_name: &str,
) -> Result<ReplRecord> {
    let rcfg = cfg(reg)?;

    // The tenant credential has to be mirrored onto the replica so the same
    // connection string works against either node after a promote — which is
    // what makes failover a DNS change. Without one there is nothing to
    // mirror, so this is a dedicated-database-only feature.
    let tenant = reg.dedicated().by_database(database).with_context(|| {
        format!(
            "{database:?} is not a dedicated database — replication mirrors its role and \
             password onto the replica, so provision it on /dedicated first"
        )
    })?;

    let peer = reg
        .peers()
        .get(peer_name)
        .with_context(|| format!("no peer named {peer_name:?} — add it on /peers first"))?;

    let host = rcfg.advertise_host.as_deref().context(
        "PG_VM_POOL_ADVERTISE_PG_HOST is not set — a replica's guest needs an address to \
         dial this node's pooler, and it cannot be derived from PG_VM_POOL_LISTEN",
    )?;
    // Refuse rather than silently ship a cleartext credential across the
    // network. `sslmode=require` on the replica is only meaningful if this
    // side actually terminates TLS.
    if !reg.tls_enabled() && !rcfg.allow_insecure {
        bail!(
            "this node has no TLS configured (PG_VM_POOL_TLS_CERT/KEY), so the replica's \
             connection would carry the replication password in cleartext; configure TLS \
             or set PG_VM_POOL_REPL_ALLOW_INSECURE=1 if both nodes share a trusted link"
        );
    }
    let hostaddr = resolve_v4(host, rcfg.advertise_port).await?;

    // Handshake before anything is created on either side.
    let client = PeerClient::new(peer.clone(), rcfg.peer_timeout)?;
    let info = client
        .node_info()
        .await
        .with_context(|| format!("reaching peer {peer_name}"))?;
    if info.node == rcfg.node_name {
        bail!(
            "peer {peer_name:?} reports its node name as {:?}, which is this node — a database \
             cannot replicate to itself",
            info.node
        );
    }
    if !info.replication_enabled {
        bail!("peer {peer_name:?} does not have replication enabled (PG_VM_POOL_REPLICATION)");
    }

    // Durable first. From here a crash leaves a visible, retryable row.
    let repl_password = generate_password()?;
    let rec = ReplRecord::new(database, Role::Primary, peer_name, &repl_password);
    let rec = reg
        .replication()
        .create(rec, &|role| reg.dedicated().by_role(role).is_some())?;

    // `Syncing` before touching the VM, not after: it is `State::pins` that
    // keeps the idle reaper and the offload ladder off this VM, and the very
    // next step restarts its Postgres. `Pending` exists only to mark the
    // window between the row landing and this line.
    reg.replication()
        .set_state(database, State::Syncing, "preparing the primary")?;

    match prepare_primary(reg, &rec, &tenant, hostaddr, rcfg, &client).await {
        Ok(()) => {
            info!("replication: {database} is now a primary replicating to {peer_name}");
            Ok(reg.replication().get(database).unwrap_or(rec))
        }
        Err(e) => {
            // Deliberately not rolled back. The peer call may have succeeded
            // with only its *response* lost, and tearing the publication down
            // would then break a live subscriber. The record is left `Failed`
            // for the operator to reconcile, retry (every step is idempotent)
            // or abandon via detach.
            let _ = reg
                .replication()
                .set_state(database, State::Failed, &format!("{e:#}"));
            crate::events::journal_error(
                "replication",
                format!("enabling replication for {database} to {peer_name} failed: {e:#}"),
            );
            Err(e)
        }
    }
}

/// The steps that actually change something, split out so the caller's error
/// path is one place.
async fn prepare_primary(
    reg: &Arc<SchemaRegistry>,
    rec: &ReplRecord,
    tenant: &crate::dedicated::Credential,
    hostaddr: Ipv4Addr,
    rcfg: &ReplicationConfig,
    client: &PeerClient,
) -> Result<()> {
    let database = &rec.database;

    // Marker + Postgres restart, so the cluster is actually at
    // `wal_level = logical`. This also mints the REPLICATION login, because
    // the record now pins and `bring_up_for` picks it up.
    reg.apply_replication_mode(database)
        .await
        .context("switching the primary to wal_level=logical")?;

    let (_guard, db) = reg.db_client(database).await?;

    // Pre-flight, reported rather than enforced: a table with no primary key
    // and no REPLICA IDENTITY replicates INSERTs but errors on UPDATE/DELETE
    // at the publisher. Choosing an identity is the tenant's call, not ours.
    let offenders: Vec<String> = db
        .query(sql::NO_REPLICA_IDENTITY_SQL, &[])
        .await
        .context("checking replica identities")?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    if !offenders.is_empty() {
        let msg = format!(
            "{database}: {} table(s) have no primary key and no REPLICA IDENTITY, so their \
             UPDATEs and DELETEs will error at the publisher: {}",
            offenders.len(),
            offenders.join(", ")
        );
        warn!("{msg}");
        crate::events::journal_error("replication", msg);
    }

    // `CREATE PUBLICATION` has no IF NOT EXISTS.
    let exists = db
        .query_opt(
            "SELECT 1 FROM pg_publication WHERE pubname = $1",
            &[&rec.publication],
        )
        .await
        .context("checking pg_publication")?
        .is_some();
    if !exists {
        db.batch_execute(&sql::create_publication(&rec.publication))
            .await
            .with_context(|| format!("creating publication {}", rec.publication))?;
        info!("{database}: created publication {}", rec.publication);
    }

    let server_version_num: Option<i32> = db
        .query_one("SELECT current_setting('server_version_num')::int4", &[])
        .await
        .ok()
        .map(|r| r.get(0));
    drop(db);

    let req = wire::ProvisionReplica {
        database: database.clone(),
        peer: rcfg.node_name.clone(),
        tenant: wire::Login {
            role: tenant.role.clone(),
            password: tenant.password.clone(),
        },
        repl: wire::Login {
            role: rec.repl_role.clone(),
            password: rec.repl_password.clone(),
        },
        primary: wire::PrimaryEndpoint {
            hostaddr: hostaddr.to_string(),
            port: rcfg.advertise_port,
            sslmode: rcfg.sslmode.clone(),
        },
        publication: rec.publication.clone(),
        subscription: rec.subscription.clone(),
        slot: rec.slot.clone(),
        copy_data: true,
        streaming: true,
        primary_server_version_num: server_version_num,
    };
    client
        .provision_replica(&req)
        .await
        .with_context(|| format!("asking peer {} to build the replica", client.name()))?;
    reg.replication()
        .set_state(database, State::Syncing, "the replica is seeding")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Replica side
// ---------------------------------------------------------------------------

/// Validate a peer's provisioning request and record it. Returns as soon as
/// the record is durable; the VM work runs in the background, exactly like
/// `SchemaRegistry::spawn_provision`.
pub fn accept_replica(
    reg: &Arc<SchemaRegistry>,
    req: wire::ProvisionReplica,
) -> Result<ReplRecord> {
    let rcfg = cfg(reg)?;
    if req.peer == rcfg.node_name {
        bail!("refusing to replicate from a peer reporting this node's own name");
    }
    // The publisher-newer-than-subscriber check needs this node's guest
    // version, which needs a warm VM — so it happens in `build_replica`, not
    // here. This path stays synchronous and cheap on purpose: it is what the
    // peer waits on.
    let database = crate::dedicated::validate_identifier(&req.database, "database")?;

    // The name must be free, or already be exactly this credential. Refusing
    // here is the whole protection against a replica landing on top of a
    // database that holds someone else's data.
    match reg.dedicated().by_database(&database) {
        Some(existing) if existing.role != req.tenant.role => bail!(
            "database {database:?} already exists on this node with a different role \
             ({:?}) — refusing to overwrite it",
            existing.role
        ),
        Some(_) => {}
        None => {
            reg.create_dedicated(&database, &req.tenant.role, &req.tenant.password)
                .with_context(|| format!("mirroring the tenant credential for {database}"))?;
        }
    }

    // Names come from the request, not re-derived: both sides must agree on
    // the slot and publication even if the two nodes run different builds
    // whose naming rules have drifted.
    let mut rec = ReplRecord::new(&database, Role::Replica, &req.peer, &req.repl.password);
    rec.publication = req.publication.clone();
    rec.subscription = req.subscription.clone();
    rec.slot = req.slot.clone();
    rec.repl_role = req.repl.role.clone();
    let rec = reg.replication().create(rec, &|role| {
        // The replication login lives on the *primary*; only a local
        // collision matters here.
        reg.dedicated().by_role(role).is_some()
    })?;
    reg.replication()
        .set_state(&database, State::Syncing, "seeding from the primary")?;

    let registry = reg.clone();
    let request = req;
    tokio::spawn(async move {
        let db = request.database.clone();
        if let Err(e) = build_replica(&registry, &request).await {
            warn!("replication: building the replica for {db} failed: {e:#}");
            let _ = registry
                .replication()
                .set_state(&db, State::Failed, &format!("{e:#}"));
            crate::events::journal_error(
                "replication",
                format!("building the replica for {db} failed: {e:#}"),
            );
        }
    });
    Ok(rec)
}

/// The replica's background half: bring the VM up in replica mode, seed the
/// schema, subscribe.
///
/// Every step is idempotent so a resumed or retried run converges: the marker
/// write is a no-op when it matches, the schema copy runs in one transaction
/// against an empty database, and the subscription is skipped when it exists.
async fn build_replica(reg: &Arc<SchemaRegistry>, req: &wire::ProvisionReplica) -> Result<()> {
    let rcfg = cfg(reg)?;
    let database = &req.database;

    // Marker + (for a replica) the worker budget its next boot needs.
    reg.apply_replication_mode(database)
        .await
        .context("putting the replica's VM into replica mode")?;

    let guard = reg.checkout(database).await?;
    let entry = guard.entry();

    let hostaddr: Ipv4Addr = req.primary.hostaddr.parse().with_context(|| {
        format!(
            "the primary sent {:?}, which is not an IPv4 address",
            req.primary.hostaddr
        )
    })?;
    let mut conninfo = sql::primary_conninfo(
        hostaddr,
        req.primary.port,
        database,
        &req.primary.sslmode,
        &rcfg.node_name,
    );
    conninfo.user = req.repl.role.clone();
    conninfo.password = req.repl.password.clone();

    let db = crate::vm::db_client(reg.cfg(), &entry.target, database).await?;

    // A publisher newer than its subscriber can emit protocol messages and
    // types the subscriber cannot apply, and the failure surfaces much later
    // as an apply error nobody connects to the version gap. The reverse
    // (subscriber newer) is fine and common. Checked here rather than in
    // `accept_replica` because it needs this node's guest version, which needs
    // a VM; an unreadable version skips the check rather than blocking.
    if let Some(theirs) = req.primary_server_version_num
        && let Ok(row) = db
            .query_one("SELECT current_setting('server_version_num')::int4", &[])
            .await
    {
        let ours: i32 = row.get(0);
        if theirs / 10_000 > ours / 10_000 {
            bail!(
                "the primary runs Postgres {} but this node's image is {} — a subscriber must \
                 not be older than its publisher; rebuild this node's guest image first",
                theirs / 10_000,
                ours / 10_000
            );
        }
    }

    let already = db
        .query_opt(
            "SELECT 1 FROM pg_subscription WHERE subname = $1",
            &[&req.subscription],
        )
        .await
        .context("checking pg_subscription")?
        .is_some();
    if already {
        info!(
            "{database}: subscription {} already exists",
            req.subscription
        );
        reg.replication()
            .set_state(database, State::Syncing, "subscription already present")?;
        return Ok(());
    }

    // Logical replication carries no DDL, so the tables have to exist before
    // the subscription's initial copy can land anything.
    crate::vm::copy_schema_from_primary(
        reg.cfg(),
        &entry.sandbox,
        database,
        &conninfo,
        rcfg.setup_deadline,
    )
    .await
    .context("copying the primary's schema")?;

    // `create_slot = true` cannot run inside a transaction block, so it goes
    // through `batch_execute` as a lone statement — the same reason
    // `vm::ensure_database` issues `CREATE DATABASE` that way. It also blocks
    // on a round trip to the primary, hence the statement timeout.
    db.batch_execute(&format!(
        "SET statement_timeout = {}",
        rcfg.setup_deadline.as_millis().min(i32::MAX as u128)
    ))
    .await
    .ok();
    db.batch_execute(&sql::create_subscription(
        &req.subscription,
        &conninfo,
        &req.publication,
        &req.slot,
        req.copy_data,
        req.streaming,
    ))
    .await
    .with_context(|| {
        format!(
            "creating subscription {} against {}",
            req.subscription,
            conninfo.redacted()
        )
    })?;
    info!(
        "{database}: subscribed to {} on the primary via slot {}",
        req.publication, req.slot
    );
    reg.replication()
        .set_state(database, State::Syncing, "initial copy in progress")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Promote / detach
// ---------------------------------------------------------------------------

/// Cut a replica loose: stop applying, drop the subscription, and re-seed the
/// sequences logical replication never carried.
pub async fn promote(reg: &Arc<SchemaRegistry>, database: &str) -> Result<wire::PromoteResponse> {
    let rcfg = cfg(reg)?;
    let rec = reg
        .replication()
        .get(database)
        .with_context(|| format!("{database} is not replicating"))?;
    if rec.role != Role::Replica {
        bail!("{database} is a replication primary on this node, not a replica");
    }

    let (_guard, db) = reg.db_client(database).await?;
    for stmt in sql::promote_statements(&rec.subscription) {
        db.batch_execute(&stmt)
            .await
            .with_context(|| format!("promoting {database}: {stmt}"))?;
    }

    // Logical replication does not replicate sequence values, so every
    // serial/identity column here is still at its initial value and the first
    // insert would collide with a replicated row.
    let mut sequences_fixed = 0i64;
    if rcfg.fix_sequences {
        sequences_fixed = db
            .query_one(sql::COUNT_SEQUENCES_SQL, &[])
            .await
            .map(|r| r.get::<_, i64>(0))
            .unwrap_or(0);
        db.batch_execute(sql::FIX_SEQUENCES_SQL)
            .await
            .context("re-seeding sequences after the promote")?;
        info!("{database}: re-seeded {sequences_fixed} sequence(s) after promote");
    }
    drop(db);

    reg.replication()
        .set_state(database, State::Promoted, "promoted by an operator")?;
    // The record no longer pins, so this clears the marker and returns the VM
    // to the cheap WAL profile on its next start.
    if let Err(e) = reg.apply_replication_mode(database).await {
        warn!("{database}: promoted, but clearing the replication marker failed: {e:#}");
    }
    let rec = reg.replication().get(database).unwrap_or(rec);
    Ok(wire::PromoteResponse {
        record: (&rec).into(),
        sequences_fixed,
    })
}

/// Tear this node's half of a pairing down.
///
/// On a primary that means dropping the publication and the slot — the slot
/// especially, because an orphaned one pins WAL until
/// `max_slot_wal_keep_size` invalidates it. On a replica it is the same work
/// as a promote minus the sequence re-seed.
pub async fn detach(reg: &Arc<SchemaRegistry>, database: &str) -> Result<wire::DetachResponse> {
    let rcfg = cfg(reg)?;
    let rec = reg
        .replication()
        .get(database)
        .with_context(|| format!("{database} is not replicating"))?;

    // Best-effort: tell the peer first, so its subscriber lets go of the slot
    // and the drop below finds it inactive. A peer that cannot be reached is
    // not a reason to leave this side pinned.
    if let Some(peer) = reg.peers().get(&rec.peer)
        && let Ok(client) = PeerClient::new(peer, rcfg.peer_timeout)
        && let Err(e) = client.teardown(database).await
    {
        warn!(
            "{database}: peer {} did not confirm teardown ({e:#}); continuing with this side",
            rec.peer
        );
    }

    let (_guard, db) = reg.db_client(database).await?;
    let mut slot_dropped = false;
    let mut publication_dropped = false;
    if rec.role == Role::Primary {
        // Order matters: drop the slot before the publication, so a subscriber
        // that is still attached fails to stream rather than silently
        // reconnecting to a publication that no longer publishes anything.
        match db
            .execute(&sql::drop_slot_if_inactive(&rec.slot), &[])
            .await
        {
            Ok(n) => slot_dropped = n > 0,
            Err(e) => warn!("{database}: dropping slot {} failed: {e}", rec.slot),
        }
        db.batch_execute(&sql::drop_publication(&rec.publication))
            .await
            .with_context(|| format!("dropping publication {}", rec.publication))?;
        publication_dropped = true;
        for stmt in sql::drop_repl_role(&rec.repl_role) {
            if let Err(e) = db.batch_execute(&stmt).await {
                warn!("{database}: cleaning up {}: {e}", rec.repl_role);
            }
        }
    } else {
        for stmt in sql::promote_statements(&rec.subscription) {
            if let Err(e) = db.batch_execute(&stmt).await {
                warn!("{database}: dropping subscription: {e}");
            }
        }
    }
    drop(db);

    reg.replication()
        .set_state(database, State::Detached, "detached by an operator")?;
    if let Err(e) = reg.apply_replication_mode(database).await {
        warn!("{database}: detached, but clearing the replication marker failed: {e:#}");
    }
    if !slot_dropped && rec.role == Role::Primary {
        warn!(
            "{database}: slot {} was not dropped (still active, or already gone) — if a \
             subscriber is still attached it will keep pinning WAL",
            rec.slot
        );
    }
    let rec = reg.replication().get(database).unwrap_or(rec);
    Ok(wire::DetachResponse {
        record: (&rec).into(),
        slot_dropped,
        publication_dropped,
    })
}

/// Pick up tables added to the publication since the subscription was created.
/// Logical replication publishes new tables automatically but the subscriber
/// only notices on a refresh — and the table still has to exist here.
pub async fn refresh(reg: &Arc<SchemaRegistry>, database: &str) -> Result<()> {
    let rec = reg
        .replication()
        .get(database)
        .with_context(|| format!("{database} is not replicating"))?;
    if rec.role != Role::Replica {
        bail!("only a replica has a subscription to refresh");
    }
    let (_guard, db) = reg.db_client(database).await?;
    db.batch_execute(&sql::refresh_subscription(&rec.subscription))
        .await
        .with_context(|| format!("refreshing subscription {}", rec.subscription))
}

/// Read whichever side of the link this node can see.
pub async fn local_status(
    reg: &Arc<SchemaRegistry>,
    rec: &ReplRecord,
) -> Result<(Option<wire::PrimaryStatus>, Option<wire::ReplicaStatus>)> {
    let (_guard, db) = reg.db_client(&rec.database).await?;
    match rec.role {
        Role::Primary => {
            let row = db
                .query_opt(sql::PRIMARY_STATUS_SQL, &[&rec.slot])
                .await
                .context("reading pg_replication_slots")?;
            Ok((
                row.map(|r| wire::PrimaryStatus {
                    slot_active: r.get("active"),
                    wal_status: r.get("wal_status"),
                    behind_bytes: r.get("behind_bytes"),
                    current_lsn: r.get("current_lsn"),
                    confirmed_flush_lsn: r.get("confirmed_flush_lsn"),
                    sender_state: r.get("sender_state"),
                    write_lag_s: r.get("write_lag_s"),
                    flush_lag_s: r.get("flush_lag_s"),
                    replay_lag_s: r.get("replay_lag_s"),
                }),
                None,
            ))
        }
        Role::Replica => {
            let row = db
                .query_opt(sql::REPLICA_STATUS_SQL, &[&rec.subscription])
                .await
                .context("reading pg_stat_subscription")?;
            Ok((
                None,
                row.map(|r| wire::ReplicaStatus {
                    enabled: r.get("subenabled"),
                    worker_running: r.get::<_, Option<bool>>("worker_running").unwrap_or(false),
                    received_lsn: r.get("received_lsn"),
                    latest_end_lsn: r.get("latest_end_lsn"),
                    last_msg_age_s: r.get("last_msg_age_s"),
                    tables_total: r.get::<_, Option<i64>>("tables_total").unwrap_or(0),
                    tables_ready: r.get::<_, Option<i64>>("tables_ready").unwrap_or(0),
                }),
            ))
        }
    }
}
