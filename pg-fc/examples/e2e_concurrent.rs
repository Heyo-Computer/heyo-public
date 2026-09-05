//! Concurrency e2e for pg-vm-pool: N simultaneous VMs (default 5).
//!
//! Proves supported concurrency > 1 across the whole stack — pooler routing,
//! daemon create/stop/restart, and per-VM persistent disks — by running every
//! phase for all VMs **at the same time**:
//!   1. create   — N concurrent cold connects → N distinct VMs come up in parallel
//!   2. write    — each VM gets VERIFIABLY UNIQUE data: a distinct row count
//!                 AND rows whose content embeds its own schema name, so any
//!                 cross-wiring (schema A's connection landing on schema B's
//!                 VM) fails loudly on both count and content
//!   3. stop     — all VMs stopped concurrently, out-of-band by default
//!                 (`heyvm stop`, the manual-stop path the daemon can't observe)
//!   4. restart  — N concurrent reconnects → pooler restarts all VMs in
//!                 parallel; per-VM asserts: data drive still attached in the
//!                 Firecracker config + guest ip:5432 directly reachable
//!   5. verify   — each schema's count, marker row, and spot-check note must
//!                 match exactly what THAT schema wrote before the stop
//!
//! Distinctness asserts: all sandbox ids unique, all guest IPs unique.
//! The timing table reports per-phase wall time vs the sum of per-VM times —
//! wall << sum is the direct evidence the phases actually ran in parallel.
//!
//! Prereqs: a running pooler (`PG_VM_POOL_LISTEN`, default 127.0.0.1:6432) and
//! the local heyvmd daemon. Run:
//!   cargo run --release --example e2e_concurrent
//!
//! Env: `PG_VM_POOL_LISTEN`, `PG_VM_POOL_USER` (default postgres),
//! `E2E_VMS` (default 5), `E2E_ROWS` (base row count, default 400 — VM i gets
//! base + i*137 rows), `E2E_STOP_MODE` = `cli` (default) | `sdk`,
//! `E2E_KEEP=1` to skip deleting the test VMs.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use futures::future::join_all;
use heyo_sdk::{HeyoClientOptions, Sandbox, DEFAULT_LOCAL_BASE_URL};
use tokio::net::TcpStream;
use tokio_postgres::{Client, NoTls};

fn local_opts() -> HeyoClientOptions {
    HeyoClientOptions {
        base_url: Some(DEFAULT_LOCAL_BASE_URL.to_string()),
        ..Default::default()
    }
}

/// One VM-under-test: its schema (pooler routing key) and its unique dataset.
struct Case {
    schema: String,
    vm_name: String,
    /// Unique per case — a count collision can't mask cross-wiring.
    rows: i64,
}

/// Open a Postgres connection *through the pooler*; bounded by `timeout` so a
/// stuck bring-up fails the test instead of hanging it.
async fn pg_connect(
    host: &str,
    port: u16,
    dbname: &str,
    user: &str,
    timeout: Duration,
) -> Result<Client> {
    let secs = timeout.as_secs().max(1);
    let conn_str =
        format!("host={host} port={port} dbname={dbname} user={user} connect_timeout={secs}");
    let connect = tokio_postgres::connect(&conn_str, NoTls);
    let (client, connection) = tokio::time::timeout(timeout, connect)
        .await
        .with_context(|| format!("pooler connect to {dbname} timed out after {timeout:?}"))?
        .context("connecting through pooler")?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn find_sandbox(vm_name: &str) -> Result<Sandbox> {
    let info = Sandbox::list(local_opts())
        .await
        .context("listing sandboxes")?
        .into_iter()
        .find(|s| s.name == vm_name)
        .with_context(|| format!("VM {vm_name} not found"))?;
    Sandbox::connect(info.id, local_opts()).context("connecting to sandbox")
}

async fn guest_addr(sb: &Sandbox) -> Option<SocketAddr> {
    sb.info()
        .await
        .ok()
        .and_then(|i| i.guest_ip)
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 5432))
}

async fn reachable(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

async fn wait_unreachable(addr: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if !reachable(addr).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("VM at {addr} still reachable after {timeout:?} — stop didn't take effect");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_reachable(addr: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if reachable(addr).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "guest Postgres at {addr} NOT reachable within {timeout:?} after restart — \
                 VM came up network-dead"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum StopMode {
    Sdk,
    Cli,
}

/// Stop the VM. `Cli` shells out to `heyvm stop` (embedded manager — the
/// out-of-band path the API daemon can't observe), `Sdk` goes through the
/// daemon. See examples/e2e.rs for the full rationale.
async fn stop_vm(sb: &Sandbox, mode: StopMode) -> Result<()> {
    match mode {
        StopMode::Sdk => sb.stop().await.context("stopping VM via SDK"),
        StopMode::Cli => {
            let out = tokio::process::Command::new("heyvm")
                .args(["stop", sb.sandbox_id()])
                .output()
                .await
                .context("running `heyvm stop` (is heyvm on PATH?)")?;
            if !out.status.success() {
                bail!(
                    "`heyvm stop {}` failed: {}{}",
                    sb.sandbox_id(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
            }
            Ok(())
        }
    }
}

/// The restarted VM's Firecracker config must still declare the persistent
/// data drive; a dropped /dev/vdb means an empty ephemeral PGDATA.
fn assert_data_drive_reattached(sandbox_id: &str) -> Result<()> {
    let path = format!("/tmp/firecracker-configs/heyo-{sandbox_id}.json");
    let cfg = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // non-Firecracker backend / config elsewhere
    };
    if !cfg.contains(r#""drive_id": "data""#) {
        bail!("Firecracker config {path} has NO data drive after restart");
    }
    Ok(())
}

/// Collect per-case results, failing with every error listed (not just the
/// first) so one broken VM doesn't hide the state of the other four.
fn collect<T>(phase: &str, cases: &[Case], results: Vec<Result<T>>) -> Result<Vec<T>> {
    let mut oks = Vec::with_capacity(results.len());
    let mut errs = Vec::new();
    for (case, r) in cases.iter().zip(results) {
        match r {
            Ok(v) => oks.push(v),
            Err(e) => errs.push(format!("  {}: {e:#}", case.schema)),
        }
    }
    if !errs.is_empty() {
        bail!("{phase} failed for {} VM(s):\n{}", errs.len(), errs.join("\n"));
    }
    Ok(oks)
}

/// Record a concurrent phase: wall time of the whole join, plus per-case times
/// (returned by the closures). wall << sum(per-case) proves parallelism.
struct PhaseTiming {
    label: &'static str,
    wall: Duration,
    per_case: Vec<Duration>,
}

async fn run(
    host: &str,
    port: u16,
    user: &str,
    cases: &[Case],
    stop_mode: StopMode,
    timings: &mut Vec<PhaseTiming>,
) -> Result<()> {
    let n = cases.len();

    // 1. Create: N concurrent cold connects. Each is a distinct schema, so the
    //    pooler brings up N VMs at the same time.
    println!("[1/5] creating {n} VMs via concurrent cold connects …");
    let wall = Instant::now();
    let results = join_all(cases.iter().map(|c| async {
        let t = Instant::now();
        let client = pg_connect(host, port, &c.schema, user, Duration::from_secs(300)).await?;
        client
            .simple_query("SELECT 1")
            .await
            .context("initial ping")?;
        Ok::<_, anyhow::Error>((client, t.elapsed()))
    }))
    .await;
    let created = collect("create", cases, results)?;
    let (mut clients, per_case): (Vec<Client>, Vec<Duration>) = created.into_iter().unzip();
    timings.push(PhaseTiming { label: "create VMs", wall: wall.elapsed(), per_case });

    // 2. Write unique data concurrently. Content embeds the schema name and the
    //    row count differs per VM — either alone would already catch a swap.
    println!("[2/5] writing unique data to each VM concurrently …");
    let wall = Instant::now();
    let results = join_all(clients.iter().zip(cases.iter()).map(|(client, c)| async move {
        let t = Instant::now();
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS e2e_items (id bigint primary key, note text); \
                 TRUNCATE e2e_items; \
                 CREATE TABLE IF NOT EXISTS e2e_marker (schema_name text); \
                 TRUNCATE e2e_marker;",
            )
            .await
            .context("create tables")?;
        // schema/rows are trusted values we generated — inline them (avoids
        // generate_series int4/int8 param inference; schema is [a-z0-9] only).
        client
            .execute(
                &format!(
                    "INSERT INTO e2e_items (id, note) \
                     SELECT g, '{schema}-row-' || g FROM generate_series(1, {rows}) AS g",
                    schema = c.schema,
                    rows = c.rows
                ),
                &[],
            )
            .await
            .context("insert rows")?;
        client
            .execute("INSERT INTO e2e_marker (schema_name) VALUES ($1)", &[&c.schema])
            .await
            .context("insert marker")?;
        let count: i64 = client
            .query_one("SELECT count(*) FROM e2e_items", &[])
            .await?
            .get(0);
        if count != c.rows {
            bail!("insert verification failed: {count} != {}", c.rows);
        }
        Ok::<_, anyhow::Error>(t.elapsed())
    }))
    .await;
    let per_case = collect("write", cases, results)?;
    timings.push(PhaseTiming { label: "write unique data", wall: wall.elapsed(), per_case });

    // Close all clients before stopping the VMs.
    clients.clear();

    // Resolve sandboxes + guest addrs, and assert the VMs are genuinely
    // distinct (N unique sandbox ids, N unique guest IPs) — the direct proof
    // that N schemas got N VMs rather than sharing.
    let results = join_all(cases.iter().map(|c| async {
        let sb = find_sandbox(&c.vm_name).await?;
        let guest = guest_addr(&sb).await;
        Ok::<_, anyhow::Error>((sb, guest))
    }))
    .await;
    let resolved = collect("resolve sandboxes", cases, results)?;
    {
        let mut ids: Vec<&str> = resolved.iter().map(|(sb, _)| sb.sandbox_id()).collect();
        ids.sort();
        ids.dedup();
        if ids.len() != n {
            bail!("expected {n} distinct sandbox ids, got {}: {ids:?}", ids.len());
        }
        let mut ips: Vec<String> = resolved
            .iter()
            .filter_map(|(_, g)| g.map(|a| a.ip().to_string()))
            .collect();
        ips.sort();
        ips.dedup();
        if ips.len() != n {
            bail!("expected {n} distinct guest IPs, got {}: {ips:?}", ips.len());
        }
    }
    for (c, (sb, guest)) in cases.iter().zip(resolved.iter()) {
        println!(
            "      {} → id={} guest={:?} rows={}",
            c.schema,
            sb.sandbox_id(),
            guest,
            c.rows
        );
    }

    // 3. Stop all VMs concurrently (out-of-band in cli mode), and wait for the
    //    ground-truth down signal on each.
    let mode_label = if stop_mode == StopMode::Cli { "cli (out-of-band)" } else { "sdk" };
    println!("[3/5] stopping all {n} VMs concurrently via {mode_label} …");
    let wall = Instant::now();
    let results = join_all(resolved.iter().map(|(sb, guest)| async move {
        let t = Instant::now();
        stop_vm(sb, stop_mode).await?;
        match guest {
            Some(addr) => wait_unreachable(*addr, Duration::from_secs(60)).await?,
            None => tokio::time::sleep(Duration::from_secs(3)).await,
        }
        Ok::<_, anyhow::Error>(t.elapsed())
    }))
    .await;
    let per_case = collect("stop", cases, results)?;
    timings.push(PhaseTiming { label: "stop VMs", wall: wall.elapsed(), per_case });

    // 4. Restart: N concurrent reconnects; the pooler restarts every VM in
    //    parallel. Per-VM health asserts catch a silently-broken restart.
    println!("[4/5] reconnecting to all {n} schemas concurrently (pooler restarts) …");
    let wall = Instant::now();
    let results = join_all(cases.iter().zip(resolved.iter()).map(
        |(c, (sb, guest))| async move {
            let t = Instant::now();
            let client = pg_connect(host, port, &c.schema, user, Duration::from_secs(120))
                .await
                .context("pooler failed to restart the VM")?;
            client
                .simple_query("SELECT 1")
                .await
                .context("post-restart ping")?;
            let elapsed = t.elapsed();
            assert_data_drive_reattached(sb.sandbox_id())?;
            if let Some(addr) = guest {
                wait_reachable(*addr, Duration::from_secs(30)).await?;
            }
            Ok::<_, anyhow::Error>((client, elapsed))
        },
    ))
    .await;
    let restarted = collect("restart", cases, results)?;
    let (clients, per_case): (Vec<Client>, Vec<Duration>) = restarted.into_iter().unzip();
    timings.push(PhaseTiming { label: "restart VMs", wall: wall.elapsed(), per_case });

    // 5. Verify each schema still holds exactly ITS data: its unique count,
    //    its marker row, and a spot-checked note embedding its own schema name.
    println!("[5/5] verifying each VM's unique data survived …");
    let wall = Instant::now();
    let results = join_all(clients.iter().zip(cases.iter()).map(|(client, c)| async move {
        let t = Instant::now();
        let count: i64 = client
            .query_one("SELECT count(*) FROM e2e_items", &[])
            .await?
            .get(0);
        let marker: String = client
            .query_one("SELECT schema_name FROM e2e_marker", &[])
            .await
            .context("marker row missing")?
            .get(0);
        let last_note: String = client
            .query_one("SELECT note FROM e2e_items WHERE id = $1", &[&c.rows])
            .await
            .context("spot-check row missing")?
            .get(0);
        let want_note = format!("{}-row-{}", c.schema, c.rows);
        if count != c.rows || marker != c.schema || last_note != want_note {
            bail!(
                "data mismatch (cross-wired or lost): count={count} (want {}), \
                 marker={marker:?} (want {:?}), note={last_note:?} (want {want_note:?})",
                c.rows,
                c.schema
            );
        }
        Ok::<_, anyhow::Error>(t.elapsed())
    }))
    .await;
    let per_case = collect("verify", cases, results)?;
    timings.push(PhaseTiming { label: "verify unique data", wall: wall.elapsed(), per_case });

    println!(
        "\n✓ {n} VMs ran concurrently with distinct ids/IPs; every schema kept exactly \
         its own data across the stop/restart"
    );
    Ok(())
}

async fn cleanup(cases: &[Case], keep: bool) {
    if keep {
        println!("cleanup: E2E_KEEP set — leaving test VMs in place");
        return;
    }
    join_all(cases.iter().map(|c| async {
        if let Ok(sb) = find_sandbox(&c.vm_name).await {
            match sb.kill().await {
                Ok(()) => println!("cleanup: deleted {}", c.vm_name),
                Err(e) => eprintln!("cleanup: failed to delete {}: {e}", c.vm_name),
            }
        }
    }))
    .await;
}

#[tokio::main]
async fn main() -> Result<()> {
    let listen = std::env::var("PG_VM_POOL_LISTEN").unwrap_or_else(|_| "127.0.0.1:6432".into());
    let (host, port_str) = listen
        .rsplit_once(':')
        .context("PG_VM_POOL_LISTEN must be host:port")?;
    let port: u16 = port_str.parse().context("invalid port in PG_VM_POOL_LISTEN")?;
    let user = std::env::var("PG_VM_POOL_USER").unwrap_or_else(|_| "postgres".into());
    let vms: usize = std::env::var("E2E_VMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v >= 2) // < 2 can't prove concurrency
        .unwrap_or(5);
    let base_rows: i64 = std::env::var("E2E_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let keep = std::env::var("E2E_KEEP").is_ok();
    let stop_mode = match std::env::var("E2E_STOP_MODE").as_deref() {
        Ok("sdk") => StopMode::Sdk,
        Ok("cli") | Err(_) => StopMode::Cli,
        Ok(other) => bail!("E2E_STOP_MODE must be 'cli' or 'sdk', got {other:?}"),
    };

    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let cases: Vec<Case> = (0..vms)
        .map(|i| {
            let schema = format!("e2e{secs}v{i}");
            Case {
                vm_name: format!("pg-{schema}"),
                schema,
                // Unique count per VM: a swap can't produce a matching count.
                rows: base_rows + (i as i64) * 137,
            }
        })
        .collect();

    let mode_str = if stop_mode == StopMode::Cli { "cli" } else { "sdk" };
    println!(
        "pg-vm-pool concurrent e2e — pooler={listen} user={user} vms={vms} \
         base_rows={base_rows} stop_mode={mode_str}\n"
    );

    let mut timings: Vec<PhaseTiming> = Vec::new();
    let result = run(host, port, &user, &cases, stop_mode, &mut timings).await;

    println!("\n=== timings (wall = whole concurrent phase; sum = per-VM serial cost) ===");
    for t in &timings {
        let sum: Duration = t.per_case.iter().sum();
        let max = t.per_case.iter().max().copied().unwrap_or_default();
        println!(
            "  {:<20} wall {:>7.2}s | per-VM sum {:>7.2}s, max {:>6.2}s | parallelism ×{:.1}",
            t.label,
            t.wall.as_secs_f64(),
            sum.as_secs_f64(),
            max.as_secs_f64(),
            if t.wall.as_secs_f64() > 0.0 { sum.as_secs_f64() / t.wall.as_secs_f64() } else { 1.0 },
        );
    }

    cleanup(&cases, keep).await;

    match result {
        Ok(()) => {
            println!("\nRESULT: PASS");
            Ok(())
        }
        Err(e) => {
            println!("\nRESULT: FAIL — {e:#}");
            Err(e)
        }
    }
}
