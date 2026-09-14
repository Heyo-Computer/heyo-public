//! Bounded physical migration preparation.
//!
//! `POST /api/replication/{db}/physical-prepare` creates durable source slot
//! ownership and asks the existing logical peer to seed a distinct candidate.
//! `POST /api/replication/peer/physical-replicas` durably records that
//! candidate before VM creation. Both endpoints are generation-idempotent and
//! return a sanitized [`wire::PhysicalRecordJson`]. The background worker may
//! advance only through `verified`: it never changes the ordinary database
//! binding, promotes either VM, or tears down logical replication.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use anyhow::{Context, Result, bail};
use tracing::warn;

use crate::registry::SchemaRegistry;
use super::{PhysicalPhase, PhysicalRecord, PhysicalSourceRecord, Role, State, peer::PeerClient, wire};

const SETTINGS: [&str; 5] = ["max_connections", "max_prepared_transactions", "max_locks_per_transaction", "max_wal_senders", "max_worker_processes"];

pub async fn prepare_source(reg: &Arc<SchemaRegistry>, database: &str, generation: &str) -> Result<wire::PhysicalRecordJson> {
    let _guard = reg.replication_operation(database).await;
    let logical = reg.replication().get(database).context("physical preparation requires an existing logical pairing")?;
    if logical.role != Role::Primary || !matches!(logical.state, State::Active | State::Syncing) { bail!("physical preparation must run on the active logical primary"); }
    if logical.fence.is_some() { bail!("prepare the physical candidate before fencing the source"); }
    let rcfg = reg.replication_cfg().context("replication disabled")?;
    if !reg.tls_enabled() && !rcfg.allow_insecure { bail!("physical preparation requires pooler TLS or explicitly allowed insecure transport"); }
    let peer = reg.peers().get(&logical.peer).context("logical peer record is missing")?;
    let client = PeerClient::new(peer, rcfg.peer_timeout)?;
    let info = client.node_info().await?;
    if !info.physical_prepare || info.node != logical.peer || !info.replication_enabled {
        bail!("peer identity/capability does not support this physical preparation");
    }
    if let Some(existing) = reg.physical_sources().get(database) {
        if existing.generation != generation { bail!("a different physical generation already owns this source"); }
    }
    let tenant = reg.dedicated().by_database(database).context("physical preparation requires the dedicated tenant credential")?;
    let source_vm_id = reg.bound_vm_id(database).context("source database has no durable VM binding")?;
    let (_conn_guard, db) = reg.db_client(database).await?;
    let row = db.query_one("SELECT NOT pg_is_in_recovery(), (pg_control_system()).system_identifier::text, current_setting('server_version_num')::int / 10000, pg_current_wal_flush_lsn()::text", &[]).await?;
    let primary: bool = row.get(0); if !primary { bail!("bound source VM is in recovery"); }
    if reg.bound_vm_id(database).as_deref() != Some(&source_vm_id) { bail!("source VM binding changed during validation"); }
    let extra: i64 = db.query_one("SELECT count(*) FROM pg_database WHERE datallowconn AND NOT datistemplate AND datname NOT IN ('postgres',$1)", &[&database]).await?.get(0);
    let tablespaces: i64 = db.query_one("SELECT count(*) FROM pg_tablespace WHERE spcname NOT IN ('pg_default','pg_global')", &[]).await?.get(0);
    if extra != 0 || tablespaces != 0 { bail!("physical seed does not support extra user databases or tablespaces"); }
    let system_identifier: String = row.get(1); let pg_major: i32 = row.get(2); let source_lsn: String = row.get(3);
    let mut settings = BTreeMap::new();
    for key in SETTINGS { let value: i32 = db.query_one("SELECT current_setting($1)::int", &[&key]).await?.get(0); settings.insert(key.into(), value); }
    let slot = format!("pgfc_phys_{}", generation.replace('-', "_"));
    let source = reg.physical_sources().create(PhysicalSourceRecord { database: database.into(), generation: generation.into(), source_vm_id: source_vm_id.clone(), system_identifier: system_identifier.clone(), pg_major: pg_major as u32, slot: slot.clone(), source_lsn: source_lsn.clone(), peer: logical.peer.clone(), last_error: None })?;
    if source.source_vm_id != source_vm_id || source.system_identifier != system_identifier || source.pg_major != pg_major as u32 || source.slot != slot || source.peer != logical.peer {
        bail!("durable physical source identity no longer matches the bound logical primary");
    }
    let existing_type: Option<String> = db.query_opt("SELECT slot_type FROM pg_replication_slots WHERE slot_name=$1", &[&slot]).await?.map(|r| r.get(0));
    if existing_type.as_deref().is_some_and(|kind| kind != "physical") { bail!("owned physical slot name collides with a non-physical slot"); }
    if existing_type.is_none() { db.query_one("SELECT pg_create_physical_replication_slot($1, true)", &[&slot]).await.context("creating owned physical slot")?; }
    let mut hba_env = HashMap::new(); hba_env.insert("PGFC_REPL_ROLE".into(), logical.repl_role.clone());
    // TLS terminates at the pooler; its upstream connection is plaintext.
    hba_env.insert("PGFC_HBA_KIND".into(), "host".into());
    reg.exec_bound(database, &source_vm_id, "set -eu; f=/workspace/pgdata/pg_hba.conf; line=\"$PGFC_HBA_KIND replication $PGFC_REPL_ROLE 0.0.0.0/0 scram-sha-256\"; grep -Fqx \"$line\" $f || printf '%s\\n' \"$line\" >>$f; gosu postgres pg_ctl -D /workspace/pgdata reload", hba_env).await?;
    let host = rcfg.advertise_host.as_ref().context("PG_VM_POOL_ADVERTISE_PG_HOST is required")?;
    let host = super::orchestrate::resolve_v4(host, rcfg.advertise_port).await?.to_string();
    let request = wire::PhysicalReplicaRequest { database: database.into(), generation: generation.into(), source_node: rcfg.node_name.clone(), source_vm_id: source.source_vm_id.clone(), system_identifier: source.system_identifier.clone(), pg_major: source.pg_major, source_lsn: source.source_lsn.clone(), settings, tenant: wire::Login { role: tenant.role, password: tenant.password }, repl: wire::Login { role: logical.repl_role, password: logical.repl_password }, primary: wire::PrimaryEndpoint { hostaddr: host, port: rcfg.advertise_port, sslmode: rcfg.sslmode.clone() }, slot: source.slot.clone() };
    let answer = client.provision_physical_replica(&request).await?;
    Ok(answer)
}

pub fn accept_candidate(reg: &Arc<SchemaRegistry>, req: wire::PhysicalReplicaRequest) -> Result<PhysicalRecord> {
    let rcfg = reg.replication_cfg().context("replication disabled")?;
    let _: std::net::Ipv4Addr = req.primary.hostaddr.parse().context("physical source must advertise IPv4")?;
    if req.primary.port == 0 || !matches!(req.primary.sslmode.as_str(), "require" | "disable")
        || (req.primary.sslmode == "disable" && !rcfg.allow_insecure) {
        bail!("invalid or insecure physical source endpoint");
    }
    if req.settings.values().any(|value| *value < 0) { bail!("physical source settings cannot be negative"); }
    let logical = reg.replication().get(&req.database).context("physical candidate requires the existing logical replica")?;
    if logical.role != Role::Replica || logical.peer != req.source_node { bail!("physical source does not match the active logical pairing"); }
    if logical.repl_role != req.repl.role || logical.repl_password != req.repl.password { bail!("physical replication credential does not match the logical pairing"); }
    let tenant = reg.dedicated().by_database(&req.database).context("logical replica lost its tenant credential")?;
    if tenant.role != req.tenant.role || tenant.password != req.tenant.password { bail!("physical tenant credential does not match the logical replica"); }
    if SETTINGS.iter().any(|key| !req.settings.contains_key(*key)) || req.settings.len() != SETTINGS.len() { bail!("physical source settings are incomplete"); }
    let previous = reg.bound_vm_id(&req.database).context("logical replica has no durable serving VM")?;
    let rec = reg.physical().create(PhysicalRecord { database: req.database.clone(), generation: req.generation.clone(), candidate_name: PhysicalRecord::candidate_name(&req.generation), candidate_id: None, previous_vm_id: Some(previous), source_node: req.source_node.clone(), source_vm_id: req.source_vm_id.clone(), system_identifier: req.system_identifier.clone(), pg_major: req.pg_major, slot: req.slot.clone(), phase: PhysicalPhase::Intent, last_error: None })?;
    let background = rec.clone();
    let reg2 = reg.clone(); tokio::spawn(async move { if let Err(e) = seed(reg2.clone(), req).await { let _ = reg2.physical().set_error(&background.database, &background.generation, Some(format!("{e:#}").chars().take(500).collect())); warn!("physical candidate prepare failed: {e:#}"); } });
    Ok(rec)
}

async fn seed(reg: Arc<SchemaRegistry>, req: wire::PhysicalReplicaRequest) -> Result<()> {
    let _guard = reg.replication_operation(&req.database).await;
    let mut rec = reg.physical().get(&req.database).context("physical intent disappeared")?;
    let sandbox = if let Some(id) = &rec.candidate_id { crate::vm::connect_physical_candidate(reg.cfg(), &rec.candidate_name, id).await? } else {
        let allow_create = rec.phase == PhysicalPhase::Intent;
        if allow_create {
            reg.physical().advance(&req.database, &req.generation, PhysicalPhase::Intent, PhysicalPhase::Creating, None)?;
        }
        let own = |id: &str| { reg.physical().advance(&req.database, &req.generation, PhysicalPhase::Creating, PhysicalPhase::Candidate, Some(id.into())).map(|_| ()) };
        let sb = crate::vm::physical_candidate(reg.cfg(), &rec.candidate_name, allow_create, &own).await?;
        rec = reg.physical().get(&req.database).context("physical candidate ownership disappeared")?; sb
    };
    if rec.phase == PhysicalPhase::Candidate { rec = reg.physical().advance(&req.database, &req.generation, PhysicalPhase::Candidate, PhysicalPhase::Seeding, None)?; }
    if rec.phase == PhysicalPhase::Verified { return Ok(()); }
    let mut connection = super::sql::primary_conninfo(req.primary.hostaddr.parse()?, req.primary.port, &req.database, &req.primary.sslmode, &req.generation);
    connection.user = req.repl.role.clone();
    connection.password = req.repl.password.clone();
    let conninfo = connection.to_libpq();
    let plan = serde_json::json!({"generation":req.generation,"system_identifier":req.system_identifier,"pg_major":req.pg_major,"conninfo":conninfo,"slot":req.slot,"settings":req.settings});
    let mut env = HashMap::new(); env.insert("PGFC_PLAN".into(), serde_json::to_string(&plan)?);
    let launched = crate::vm::physical_exec(reg.cfg(), &sandbox, INSTALL_SEED_PLAN, env, "starting physical seed").await?;
    if launched.exit_code != 0 { bail!("physical seed plan installation failed; existing plan was not replaced"); }
    let deadline = tokio::time::Instant::now() + reg.replication_cfg().context("replication disabled")?.setup_deadline;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let status = crate::vm::physical_exec(reg.cfg(), &sandbox, READ_STATUS, HashMap::new(), "probing physical seed").await?;
        if status.exit_code != 0 { bail!("physical seed status probe failed"); }
        let value: serde_json::Value = serde_json::from_str(if status.stdout.is_empty() { status.output.trim() } else { status.stdout.trim() })?;
        match value["phase"].as_str() { Some("active") => break, Some("failed") => bail!("guest seed failed: {}", value["error"].as_str().unwrap_or("unknown error")), _ if tokio::time::Instant::now() >= deadline => bail!("physical seed still running after setup deadline; retry will resume under guest lock"), _ => {} }
    }
    let mut verify_env = HashMap::new();
    verify_env.insert("PGFC_DB".into(), req.database.clone()); verify_env.insert("PGFC_ROLE".into(), req.tenant.role.clone());
    verify_env.insert("PGFC_SYSTEM_ID".into(), req.system_identifier.clone()); verify_env.insert("PGFC_SLOT".into(), req.slot.clone()); verify_env.insert("PGFC_LSN".into(), req.source_lsn.clone());
    verify_env.insert("PGFC_HOST".into(), req.primary.hostaddr.clone());
    verify_env.insert("PGFC_PORT".into(), req.primary.port.to_string());
    // Guest activation starts Postgres; it does not wait for the WAL receiver
    // to connect and replay the source barrier. Only a true probe verifies it.
    wait_for_runtime(deadline, || async {
        let verify = crate::vm::physical_exec(reg.cfg(), &sandbox, VERIFY_RUNTIME, verify_env.clone(), "verifying physical candidate runtime").await?;
        if verify.exit_code != 0 { bail!("physical candidate runtime probe failed"); }
        let result = if verify.stdout.is_empty() { verify.output.trim() } else { verify.stdout.trim() };
        match result {
            "t" => Ok(true),
            "f" => Ok(false),
            _ => bail!("physical candidate runtime probe returned an invalid result"),
        }
    }).await?;
    reg.physical().advance(&req.database, &req.generation, PhysicalPhase::Seeding, PhysicalPhase::Verified, None)?;
    Ok(())
}

async fn wait_for_runtime<F, Fut>(deadline: tokio::time::Instant, mut probe: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    loop {
        if probe().await? { return Ok(()); }
        if tokio::time::Instant::now() >= deadline {
            bail!("physical candidate runtime identity/streaming verification did not become ready before setup deadline");
        }
        tokio::time::sleep_until(std::cmp::min(deadline, tokio::time::Instant::now() + Duration::from_secs(5))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_wait_requires_a_true_probe_after_startup() {
        let mut probes = 0;
        wait_for_runtime(tokio::time::Instant::now() + Duration::from_secs(10), || {
            probes += 1;
            std::future::ready(Ok(probes == 2))
        }).await.unwrap();
        assert_eq!(probes, 2);
    }

    #[tokio::test]
    async fn runtime_wait_preserves_deadline_and_probe_errors() {
        let mut probes = 0;
        let error = wait_for_runtime(tokio::time::Instant::now(), || {
            probes += 1;
            std::future::ready(Ok(false))
        }).await.unwrap_err();
        assert!(error.to_string().contains("setup deadline"));
        assert_eq!(probes, 1);
        let error = wait_for_runtime(tokio::time::Instant::now() + Duration::from_secs(10), || {
            std::future::ready(Err(anyhow::anyhow!("capture failed")))
        }).await.unwrap_err();
        assert_eq!(error.to_string(), "capture failed");
    }
}

// Command substitution gives jq a pipe instead of the serial TTY, disabling
// ANSI colors even in already-created guests. Preserve a failed probe's exit.
const READ_STATUS: &str = r#"set -e
status=$(/usr/local/bin/pg-fc-physical status)
printf '%s\n' "$status"
"#;

const INSTALL_SEED_PLAN: &str = r#"set -eu
umask 077
test -x /usr/local/bin/pg-fc-physical
mountpoint -q /workspace
install -d -o postgres -g postgres -m 700 /workspace/pg-fc-physical
if test -f /workspace/pg-fc-physical/plan.json; then
    printf '%s' "$PGFC_PLAN" | cmp -s - /workspace/pg-fc-physical/plan.json
else
    # Initial HEYVM_READY precedes postmaster readiness; do not race its first start.
    gosu postgres pg_isready -q -d postgres
    printf '%s' "$PGFC_PLAN" > /workspace/pg-fc-physical/plan.json.tmp
    chown postgres:postgres /workspace/pg-fc-physical/plan.json.tmp
    chmod 600 /workspace/pg-fc-physical/plan.json.tmp
    mv /workspace/pg-fc-physical/plan.json.tmp /workspace/pg-fc-physical/plan.json
fi
touch /etc/pg-fc-physical-persistent-required
sync
setsid nohup /usr/local/bin/pg-fc-physical seed >>/workspace/pg-fc-physical/controller.log 2>&1 </dev/null &
echo launched
"#;

// psql expands variables in input scripts, not the argument to -c.
const VERIFY_RUNTIME: &str = r#"set -eu
gosu postgres psql -AtX postgres -v ON_ERROR_STOP=1 -v db="$PGFC_DB" -v role="$PGFC_ROLE" -v sid="$PGFC_SYSTEM_ID" -v slot="$PGFC_SLOT" -v lsn="$PGFC_LSN" -v host="$PGFC_HOST" -v port="$PGFC_PORT" <<'SQL'
SELECT pg_is_in_recovery()
AND current_setting('transaction_read_only') = 'on'
AND (pg_control_system()).system_identifier::text = :'sid'
AND EXISTS(SELECT FROM pg_database WHERE datname = :'db')
AND EXISTS(SELECT FROM pg_roles WHERE rolname = :'role' AND rolcanlogin)
AND EXISTS(SELECT FROM pg_stat_wal_receiver WHERE status = 'streaming' AND slot_name = :'slot' AND sender_host = :'host' AND sender_port = :'port'::int)
AND pg_last_wal_replay_lsn() >= :'lsn'::pg_lsn;
SQL
"#;
