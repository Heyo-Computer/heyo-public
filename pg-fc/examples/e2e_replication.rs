//! Two-node cross-host logical replication, end to end, against real poolers.
//!
//! Unlike `e2e.rs` this needs **two** running pg-fc nodes, because the thing
//! under test is the pairing between them. It drives them exactly as an
//! operator would — over the admin API and through the poolers' Postgres
//! ports — and asserts both what replication does and, deliberately, what it
//! does **not** do.
//!
//! ```sh
//! A_DASH=https://a.example:34199 A_USER=admin A_PASS=... A_PG=a.example:6432 \
//! B_DASH=https://b.example:34199 B_USER=admin B_PASS=... B_PG=b.example:6432 \
//! PEER_PG_HOST=198.51.100.20 \
//!     cargo run --release --example e2e_replication
//! ```
//!
//! `PEER_PG_HOST[:PORT]` is what a guest on node A dials to reach node B's
//! pooler — the peer record's Postgres endpoint. It is separate from `B_PG`,
//! which is what *this test process* dials, because the two are frequently
//! different addresses.
//!
//! Steps, and why each one is here:
//!
//!  1. provision a dedicated database on A and write rows through A's pooler;
//!  2. add B as a peer and start replication;
//!  3. poll until the pairing reports `active` with every table copied;
//!  4. read the rows back from **B's** pooler using the *same* role and
//!     password — the credential is mirrored, and that is the property that
//!     makes a failover a DNS change rather than a re-credentialing;
//!  5. write more rows on A and watch them arrive on B — the streaming half,
//!     as distinct from the initial copy;
//!  6. `ALTER TABLE` on A and assert the new column does **not** appear on B.
//!     The DDL limitation is asserted rather than only documented, so it can
//!     never quietly start working and then quietly stop;
//!  7. promote B, and assert its sequence was re-seeded past the replicated
//!     rows — the collision every promote would otherwise hit on its first
//!     insert;
//!  8. detach on A and assert the replication slot is gone. An orphaned slot
//!     pins WAL on the primary's data disk indefinitely, so this is the
//!     assertion that matters most for anything long-running.
//!
//! `E2E_KEEP=1` leaves the database and the pairing in place for inspection.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, NoTls};

/// How long the initial copy may take before the test gives up. Generous: it
/// includes a cold VM bring-up on the replica plus a schema copy across the
/// link.
const SYNC_DEADLINE: Duration = Duration::from_secs(600);
/// How long a handful of rows may take to stream once the pairing is active.
const STREAM_DEADLINE: Duration = Duration::from_secs(120);

struct Node {
    name: &'static str,
    dash: String,
    user: String,
    pass: String,
    pg_host: String,
    pg_port: u16,
}

impl Node {
    fn from_env(name: &'static str, prefix: &str) -> Result<Self> {
        let var = |k: &str| -> Result<String> {
            std::env::var(format!("{prefix}_{k}"))
                .with_context(|| format!("{prefix}_{k} is required"))
        };
        let pg = var("PG")?;
        let (host, port) = split_hostport(&pg, 6432)?;
        Ok(Self {
            name,
            dash: var("DASH")?.trim_end_matches('/').to_string(),
            user: var("USER")?,
            pass: var("PASS")?,
            pg_host: host,
            pg_port: port,
        })
    }
}

fn split_hostport(s: &str, default_port: u16) -> Result<(String, u16)> {
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), p.parse().context("port")?)),
        None => Ok((s.to_string(), default_port)),
    }
}

/// One admin-API call, with the node's Basic credentials.
async fn api(
    http: &reqwest::Client,
    node: &Node,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let url = format!("{}{path}", node.dash);
    let mut req = http
        .request(method.clone(), &url)
        .basic_auth(&node.user, Some(&node.pass));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req
        .send()
        .await
        .with_context(|| format!("{method} {url} on {}", node.name))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{method} {url} on {} -> {status}: {text}", node.name);
    }
    Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
}

/// Connect through a node's pooler. The dbname selects the schema's VM, so
/// this blocks until that VM is up and the connection is spliced.
async fn pg(node: &Node, dbname: &str, user: &str, password: &str) -> Result<Client> {
    let conn_str = format!(
        "host={} port={} dbname={dbname} user={user} password={password} connect_timeout=30",
        node.pg_host, node.pg_port
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls)
        .await
        .with_context(|| format!("connecting to {} as {user}/{dbname}", node.name))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

/// Poll `f` until it returns true or `deadline` elapses.
async fn until<F, Fut>(what: &str, deadline: Duration, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let start = Instant::now();
    let mut last = String::new();
    loop {
        match f().await {
            Ok(true) => {
                println!("  ✓ {what} after {:?}", start.elapsed());
                return Ok(());
            }
            Ok(false) => {}
            // Transient failures are expected while a VM is coming up; only
            // the deadline decides.
            Err(e) => last = format!("{e:#}"),
        }
        if start.elapsed() >= deadline {
            bail!("timed out after {deadline:?} waiting for {what}; last error: {last}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn count(c: &Client) -> Result<i64> {
    Ok(c.query_one("SELECT count(*) FROM widgets", &[])
        .await?
        .get(0))
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Node::from_env("A", "A")?;
    let b = Node::from_env("B", "B")?;
    let (peer_host, peer_port) = split_hostport(
        &std::env::var("PEER_PG_HOST").context("PEER_PG_HOST is required")?,
        6432,
    )?;
    let keep = std::env::var("E2E_KEEP").is_ok();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let db = format!("e2e_repl_{stamp}");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    println!("== 1. provision {db} on node A and write rows");
    let created = api(
        &http,
        &a,
        reqwest::Method::POST,
        "/api/databases",
        Some(serde_json::json!({ "database": db })),
    )
    .await?;
    let role = created["username"]
        .as_str()
        .context("username")?
        .to_string();
    let password = created["password"]
        .as_str()
        .context("password")?
        .to_string();

    let ca = pg(&a, &db, &role, &password).await?;
    ca.batch_execute("CREATE TABLE widgets (id bigserial PRIMARY KEY, label text NOT NULL)")
        .await?;
    for i in 0..1000 {
        ca.execute(
            "INSERT INTO widgets (label) VALUES ($1)",
            &[&format!("w{i}")],
        )
        .await?;
    }
    assert_eq!(count(&ca).await?, 1000);

    println!("== 2. add B as a peer on A and start replicating");
    let _ = api(
        &http,
        &a,
        reqwest::Method::POST,
        "/api/peers",
        Some(serde_json::json!({
            "name": "e2e_peer", "base_url": b.dash, "user": b.user,
            "password": b.pass, "pg_host": peer_host, "pg_port": peer_port,
        })),
    )
    .await;
    api(
        &http,
        &a,
        reqwest::Method::POST,
        "/api/replication",
        Some(serde_json::json!({ "database": db, "peer": "e2e_peer" })),
    )
    .await?;

    println!("== 3. wait for the initial copy");
    until(
        "the pairing reports active with every table copied",
        SYNC_DEADLINE,
        || {
            let (http, a, db) = (&http, &a, &db);
            async move {
                let s = api(
                    http,
                    a,
                    reqwest::Method::GET,
                    &format!("/api/replication/{db}?fresh=1"),
                    None,
                )
                .await?;
                if s["record"]["state"] == "failed" {
                    bail!("the pairing failed: {}", s["record"]["message"]);
                }
                let ready = s["replica"]["tables_ready"].as_i64().unwrap_or(-1);
                let total = s["replica"]["tables_total"].as_i64().unwrap_or(-1);
                Ok(s["record"]["state"] == "active" && ready >= 0 && ready == total)
            }
        },
    )
    .await?;

    println!("== 4. read the rows back from B with the SAME credential");
    let cb = pg(&b, &db, &role, &password)
        .await
        .context("the tenant credential must be mirrored onto the replica")?;
    assert_eq!(
        count(&cb).await?,
        1000,
        "the initial copy must land every row"
    );

    println!("== 5. write more on A and watch them stream to B");
    for i in 1000..2000 {
        ca.execute(
            "INSERT INTO widgets (label) VALUES ($1)",
            &[&format!("w{i}")],
        )
        .await?;
    }
    until("2000 rows on the replica", STREAM_DEADLINE, || {
        let cb = &cb;
        async move { Ok(count(cb).await? == 2000) }
    })
    .await?;

    println!("== 6. assert DDL is NOT replicated (the documented limitation)");
    ca.batch_execute("ALTER TABLE widgets ADD COLUMN note text")
        .await?;
    // Give it more than enough time to be wrong.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let has_note: bool = cb
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_name = 'widgets' AND column_name = 'note')",
            &[],
        )
        .await?
        .get(0);
    if has_note {
        bail!(
            "the replica grew the new column on its own — logical replication is documented \
             as not carrying DDL, and the README's limitation list (and the refresh button) \
             are built on that. If this ever changes, both must change with it."
        );
    }
    println!("  ✓ the new column did not appear on the replica, as documented");

    println!("== 7. promote B and check its sequence was re-seeded");
    let promoted = api(
        &http,
        &b,
        reqwest::Method::POST,
        &format!("/api/replication/{db}/promote"),
        None,
    )
    .await?;
    println!(
        "  promoted; {} sequence(s) re-seeded",
        promoted["sequences_fixed"]
    );
    // The point of the re-seed: without it this insert collides with an id
    // that arrived by replication.
    cb.batch_execute("ALTER TABLE widgets ADD COLUMN note text")
        .await?;
    cb.execute("INSERT INTO widgets (label) VALUES ('after-promote')", &[])
        .await
        .context("a promoted replica must be writable without a primary-key collision")?;
    assert_eq!(count(&cb).await?, 2001);

    println!("== 8. detach on A and assert the slot is gone");
    let detached = api(
        &http,
        &a,
        reqwest::Method::POST,
        &format!("/api/replication/{db}/detach"),
        None,
    )
    .await?;
    if detached["publication_dropped"] != serde_json::Value::Bool(true) {
        bail!("detach left the publication behind: {detached}");
    }
    let slots: i64 = ca
        .query_one(
            "SELECT count(*) FROM pg_replication_slots WHERE slot_name LIKE 'pgfc_%'",
            &[],
        )
        .await?
        .get(0);
    if slots != 0 {
        bail!(
            "detach left {slots} replication slot(s) behind — an orphaned slot retains WAL on \
             the primary's data disk until max_slot_wal_keep_size invalidates it"
        );
    }
    println!("  ✓ no pgfc replication slots remain on the primary");

    if !keep {
        println!("== cleanup");
        for node in [&a, &b] {
            let _ = api(
                &http,
                node,
                reqwest::Method::DELETE,
                &format!("/api/replication/{db}"),
                None,
            )
            .await;
            let _ = api(
                &http,
                node,
                reqwest::Method::DELETE,
                &format!("/api/databases/{db}"),
                None,
            )
            .await;
        }
        let _ = api(
            &http,
            &a,
            reqwest::Method::DELETE,
            "/api/peers/e2e_peer",
            None,
        )
        .await;
    } else {
        println!("E2E_KEEP=1: leaving {db} and the pairing in place");
    }

    println!("\nOK — replication, streaming, the DDL limitation, promote and detach all held.");
    Ok(())
}
