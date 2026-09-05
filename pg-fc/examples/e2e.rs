//! End-to-end test for pg-vm-pool.
//!
//! Exercises the full path a real client (the app) hits, and times each step:
//!   1. create VM   — first connect through the pooler brings a fresh VM up
//!   2. write       — CREATE TABLE + INSERT, verified by row count
//!   then, for each of E2E_CYCLES iterations (default 3):
//!   3. stop VM     — stop the VM out-of-band via the daemon (as a manual/idle stop)
//!   4. restart VM  — reconnect through the pooler (its self-heal restarts it),
//!                    then HARD-ASSERT the restart actually came up healthy:
//!                      (a) the Firecracker config still carries the /dev/vdb
//!                          data drive  — a dropped data drive silently reverts
//!                          the VM to an empty, non-persistent rootfs PGDATA;
//!                      (b) the guest's Postgres port is directly reachable —
//!                          a "running" VM whose tap is NO-CARRIER is dead.
//!   5. query       — SELECT the rows back and verify they survived the restart.
//!
//! Why the cycles + white-box checks: a single, fast, same-process stop/start
//! usually comes back up fine, so a one-shot test passes even though a real
//! app-driven restart intermittently comes up with the data drive dropped or
//! the VM network-dead. Looping and asserting the daemon-side invariants
//! (config drives, direct guest reachability) turns those intermittent,
//! easy-to-mask failures into a deterministic, loud test failure.
//!
//! Prereqs: a running pooler (`PG_VM_POOL_LISTEN`, default 127.0.0.1:6432) and
//! the local heyvmd daemon. Run:
//!   cargo run --release --example e2e
//!
//! Env: `PG_VM_POOL_LISTEN`, `PG_VM_POOL_USER` (default postgres),
//! `E2E_ROWS` (default 1000), `E2E_CYCLES` (default 3 stop/restart cycles),
//! `E2E_STOP_MODE` = `cli` (default) | `sdk`, `E2E_KEEP=1` to skip deleting
//! the test VM.
//!
//! STOP MODE matters — it selects which daemon learns about the stop:
//!   sdk — `Sandbox::stop()` → HTTP to the API daemon; the daemon stops the VM
//!         itself, so its in-memory handle is marked not-running. This is the
//!         "cooperative" path and does NOT reproduce the manual-stop bug.
//!   cli — shell out to `heyvm stop <id>`: the CLI builds its OWN embedded
//!         SandboxManager and kills the firecracker process directly, exactly
//!         like a user stopping a sandbox from a terminal / the desktop app.
//!         The API daemon never observes the stop, so its cached handle still
//!         claims running=true — and a later start() must not trust it. This
//!         is the path that reproduces "restart silently no-ops, app times
//!         out", so it's the default.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use heyo_sdk::{HeyoClientOptions, Sandbox, DEFAULT_LOCAL_BASE_URL};
use tokio::net::TcpStream;
use tokio_postgres::{Client, NoTls};

fn local_opts() -> HeyoClientOptions {
    HeyoClientOptions {
        base_url: Some(DEFAULT_LOCAL_BASE_URL.to_string()),
        ..Default::default()
    }
}

/// Open a Postgres connection *through the pooler*. The pooler routes on the
/// database name, so `dbname` selects (and lazily brings up / restarts) the
/// schema's VM; this call blocks until the VM is ready and the connection is
/// spliced. Bounded by `timeout` so a network-dead restart fails the test
/// instead of hanging.
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
    // The connection future drives the socket; it ends when we drop the client
    // or the VM goes away (both expected in this test), so swallow its result.
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

/// Best-effort fetch of the VM's direct guest `ip:5432`. `guest_ip` is derived
/// deterministically from the sandbox id, so it's stable across restarts, but a
/// just-restarted VM can briefly drop out of the status listing — callers pass
/// a fallback captured earlier.
async fn guest_addr(sb: &Sandbox) -> Option<SocketAddr> {
    sb.info()
        .await
        .ok()
        .and_then(|i| i.guest_ip)
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 5432))
}

/// How a cycle stops the VM — see the module docs for why this matters.
#[derive(Clone, Copy, PartialEq)]
enum StopMode {
    /// Through the API daemon (SDK) — daemon knows the VM stopped.
    Sdk,
    /// Out-of-band via the `heyvm` CLI (embedded manager) — daemon does NOT
    /// know, its cached handle goes stale. Reproduces a manual stop.
    Cli,
}

/// Stop the VM according to `mode`.
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

/// Can we open a TCP connection to `addr` within 2s?
async fn reachable(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Poll until the VM's Postgres port stops accepting connections — the ground-
/// truth signal that the VM is actually down, independent of daemon status
/// strings (a freshly-stopped VM can briefly drop out of the status listing).
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

/// Poll until the VM's Postgres port accepts connections again. A restarted VM
/// that never becomes reachable (tap NO-CARRIER / kernel panic / init failure)
/// trips this — the "running, but network-dead" failure the app hits.
async fn wait_reachable(addr: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if reachable(addr).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "guest Postgres at {addr} NOT reachable within {timeout:?} after restart — \
                 VM came up network-dead (tap NO-CARRIER / boot failure)"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// White-box check against the local Firecracker daemon: after a restart the
/// generated VM config MUST still declare the persistent data drive. If it's
/// missing, init.sh finds no /dev/vdb, falls back to the ephemeral rootfs for
/// /workspace, and Postgres comes up empty — data silently gone. This is the
/// exact regression that a coincidentally-passing row count can hide, so we
/// assert it directly. Returns Ok(()) (with a note) when the config file isn't
/// present — e.g. a non-Firecracker backend where this check doesn't apply.
fn assert_data_drive_reattached(sandbox_id: &str) -> Result<()> {
    let path = format!("/tmp/firecracker-configs/heyo-{sandbox_id}.json");
    let cfg = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            println!("      (no Firecracker config at {path} — skipping data-drive check)");
            return Ok(());
        }
    };
    if !cfg.contains(r#""drive_id": "data""#) {
        bail!(
            "Firecracker config {path} has NO data drive after restart — the persistent \
             /dev/vdb was dropped, so the VM booted onto an empty ephemeral rootfs. \
             (disk_size_gb lost on the restart path.)"
        );
    }
    Ok(())
}

/// One create + write, then `cycles` stop/restart/verify iterations.
async fn run(
    host: &str,
    port: u16,
    user: &str,
    schema: &str,
    vm_name: &str,
    rows: i64,
    cycles: u32,
    stop_mode: StopMode,
    timings: &mut Vec<(String, Duration)>,
) -> Result<()> {
    // 1. Create the VM: the first connect through the pooler brings it up.
    println!("[1] creating VM via cold connect to schema {schema} …");
    let t = Instant::now();
    let client = pg_connect(host, port, schema, user, Duration::from_secs(300)).await?;
    client
        .simple_query("SELECT 1")
        .await
        .context("initial ping")?;
    timings.push(("create VM (cold connect)".into(), t.elapsed()));

    // 2. Create a table and insert data.
    println!("[2] creating table + inserting {rows} rows …");
    let t = Instant::now();
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS e2e_items (id bigint primary key, note text); \
             TRUNCATE e2e_items;",
        )
        .await
        .context("create table")?;
    // `rows` is a trusted i64 we parsed ourselves — inline it so we don't hit
    // generate_series' int4/int8 parameter-type inference.
    client
        .execute(
            &format!(
                "INSERT INTO e2e_items (id, note) \
                 SELECT g, 'row-' || g FROM generate_series(1, {rows}) AS g"
            ),
            &[],
        )
        .await
        .context("insert rows")?;
    let count: i64 = client
        .query_one("SELECT count(*) FROM e2e_items", &[])
        .await?
        .get(0);
    if count != rows {
        bail!("insert verification failed: {count} != {rows}");
    }
    timings.push(("write (table + insert)".into(), t.elapsed()));

    // Close the client so its connection is gone before we stop the VM.
    drop(client);

    // Grab a handle to the VM and its stable direct address once; reuse across
    // cycles. guest_ip is derived from the sandbox id, so it doesn't change on
    // restart.
    let sb = find_sandbox(vm_name).await?;
    let sandbox_id = sb.sandbox_id().to_string();
    let guest = guest_addr(&sb).await;
    println!("    VM {vm_name} id={sandbox_id} guest={guest:?}");

    for cycle in 1..=cycles {
        println!("\n--- cycle {cycle}/{cycles} ---");

        // 3. Stop the VM out-of-band (as if manually stopped / idled out).
        let mode_label = match stop_mode {
            StopMode::Sdk => "sdk",
            StopMode::Cli => "cli (out-of-band)",
        };
        println!("[3] stopping VM {vm_name} via {mode_label} …");
        let t = Instant::now();
        stop_vm(&sb, stop_mode).await?;
        match guest {
            Some(addr) => wait_unreachable(addr, Duration::from_secs(60)).await?,
            None => tokio::time::sleep(Duration::from_secs(3)).await,
        }
        timings.push((format!("cycle {cycle}: stop VM"), t.elapsed()));

        // 4. Restart by reconnecting through the pooler: it probes the dead
        //    entry, evicts it, and re-inits — which starts the stopped VM back
        //    up. Bounded so a network-dead VM fails fast instead of hanging.
        println!("[4] reconnecting (pooler restarts the VM) …");
        let t = Instant::now();
        let client = pg_connect(host, port, schema, user, Duration::from_secs(120))
            .await
            .with_context(|| format!("cycle {cycle}: pooler failed to restart the VM"))?;
        client
            .simple_query("SELECT 1")
            .await
            .context("post-restart ping")?;
        let restart_elapsed = t.elapsed();

        // 4a. HARD ASSERT: the persistent data drive is still attached. Catches
        //     the "data drive dropped on restart" daemon bug even if the query
        //     below would coincidentally pass.
        assert_data_drive_reattached(&sandbox_id)
            .with_context(|| format!("cycle {cycle}: data drive check"))?;

        // 4b. HARD ASSERT: the guest is directly reachable (network is up).
        //     Catches the "running but tap NO-CARRIER" network-dead VM. Uses
        //     the stable guest addr; if we never resolved one, re-fetch now.
        let addr = match guest {
            Some(a) => Some(a),
            None => guest_addr(&sb).await,
        };
        if let Some(addr) = addr {
            wait_reachable(addr, Duration::from_secs(30))
                .await
                .with_context(|| format!("cycle {cycle}: guest reachability check"))?;
        } else {
            println!("      (no guest_ip resolved — skipping direct reachability check)");
        }
        timings.push((format!("cycle {cycle}: restart VM (reconnect)"), restart_elapsed));

        // 5. Query the data back and verify it survived the stop/start.
        println!("[5] querying data back …");
        let t = Instant::now();
        let count: i64 = client
            .query_one("SELECT count(*) FROM e2e_items", &[])
            .await?
            .get(0);
        let last_note: String = client
            .query_one("SELECT note FROM e2e_items WHERE id = $1", &[&rows])
            .await?
            .get(0);
        if count != rows || last_note != format!("row-{rows}") {
            bail!(
                "cycle {cycle}: data did NOT survive restart: count={count} (want {rows}), \
                 last_note={last_note:?} — persistent disk was not reattached"
            );
        }
        timings.push((format!("cycle {cycle}: query (verify persisted)"), t.elapsed()));
        println!("    ✓ cycle {cycle}: {count} rows intact, guest reachable, data drive attached");
        drop(client);
    }

    println!("\n✓ data survived {cycles} stop/start cycle(s): {rows} rows intact throughout");
    Ok(())
}

async fn cleanup(vm_name: &str, keep: bool) {
    if keep {
        println!("cleanup: E2E_KEEP set — leaving {vm_name} in place");
        return;
    }
    match find_sandbox(vm_name).await {
        Ok(sb) => match sb.kill().await {
            Ok(()) => println!("cleanup: deleted {vm_name}"),
            Err(e) => eprintln!("cleanup: failed to delete {vm_name}: {e}"),
        },
        Err(_) => {} // already gone
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let listen = std::env::var("PG_VM_POOL_LISTEN").unwrap_or_else(|_| "127.0.0.1:6432".into());
    let (host, port_str) = listen
        .rsplit_once(':')
        .context("PG_VM_POOL_LISTEN must be host:port")?;
    let port: u16 = port_str.parse().context("invalid port in PG_VM_POOL_LISTEN")?;
    let user = std::env::var("PG_VM_POOL_USER").unwrap_or_else(|_| "postgres".into());
    let rows: i64 = std::env::var("E2E_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let cycles: u32 = std::env::var("E2E_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&c| c >= 1)
        .unwrap_or(3);
    let keep = std::env::var("E2E_KEEP").is_ok();
    let stop_mode = match std::env::var("E2E_STOP_MODE").as_deref() {
        Ok("sdk") => StopMode::Sdk,
        Ok("cli") | Err(_) => StopMode::Cli,
        Ok(other) => bail!("E2E_STOP_MODE must be 'cli' or 'sdk', got {other:?}"),
    };

    // Unique schema per run so step 1 is always a fresh create; cleaned up at end.
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let schema = format!("e2e{secs}");
    let vm_name = format!("pg-{schema}");

    let mode_str = if stop_mode == StopMode::Cli { "cli" } else { "sdk" };
    println!(
        "pg-vm-pool e2e — pooler={listen} user={user} schema={schema} rows={rows} \
         cycles={cycles} stop_mode={mode_str}\n"
    );

    let mut timings: Vec<(String, Duration)> = Vec::new();
    let result = run(
        host, port, &user, &schema, &vm_name, rows, cycles, stop_mode, &mut timings,
    )
    .await;

    // Always report timing for whatever completed, and always clean up.
    println!("\n=== timings ===");
    let mut total = Duration::ZERO;
    for (label, d) in &timings {
        println!("  {label:<34} {:>8.2}s", d.as_secs_f64());
        total += *d;
    }
    println!("  {:<34} {:>8.2}s", "TOTAL", total.as_secs_f64());

    cleanup(&vm_name, keep).await;

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
