//! Minimal isolation test: does the SDK's stop→start hang the way the pooler
//! sees, with no deadpool/pooler context? Creates a VM, stops it, times start().
//!   cargo run --release --example sdk_restart

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use deadpool_postgres::{Config as PgConfig, Runtime};
use heyo_sdk::{
    HeyoClientOptions, Sandbox, SandboxCreateOptions, SandboxDriver, SandboxSize,
    DEFAULT_LOCAL_BASE_URL,
};

fn opts() -> HeyoClientOptions {
    HeyoClientOptions {
        base_url: Some(DEFAULT_LOCAL_BASE_URL.to_string()),
        ..Default::default()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let name = format!("pg-sdkr{secs}");
    println!("creating {name} …");
    let sb = Sandbox::create(
        SandboxCreateOptions {
            name: Some(name.clone()),
            image: Some("pg".to_string()),
            driver: Some(SandboxDriver::Firecracker),
            open_ports: vec![5432],
            size_class: Some(SandboxSize::Micro),
            disk_size_gb: Some(4),
            ttl_seconds: Some(0),
            wait_for_ready: Some(Duration::from_secs(120)),
            ..Default::default()
        },
        opts(),
    )
    .await?;
    println!("created id={}", sb.sandbox_id());

    // Replicate the pooler: build a deadpool pool to the VM's guest_ip and use
    // it, so when we stop the VM the pool has connections to a now-dead address.
    let guest_ip = sb.get().await?.guest_ip.expect("guest_ip");
    println!("guest_ip={guest_ip}");
    let mut pg = PgConfig::new();
    pg.host = Some(guest_ip.clone());
    pg.port = Some(5432);
    pg.dbname = Some("postgres".to_string());
    pg.user = Some("postgres".to_string());
    let pool = pg.create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)?;
    {
        let client = pool.get().await?;
        client.simple_query("SELECT 1").await?;
        println!("pool query OK");
    }

    let t = Instant::now();
    sb.stop().await?;
    println!("stop  {:?}", t.elapsed());

    // Poke the pool at the now-dead guest_ip (as the pooler's liveness probe
    // does), which is what seems to poison later SDK calls.
    let t = Instant::now();
    let probe = tokio::time::timeout(Duration::from_secs(3), async {
        let c = pool.get().await?;
        c.simple_query("SELECT 1").await?;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    println!("probe-after-stop {:?} -> {:?}", t.elapsed(), probe.is_ok());

    let t = Instant::now();
    sb.start().await?;
    println!("start {:?}", t.elapsed());

    let t = Instant::now();
    sb.get().await?;
    println!("get   {:?}", t.elapsed());

    println!("cleanup …");
    sb.kill().await?;
    println!("done");
    Ok(())
}
