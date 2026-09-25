//! Mirrors `sdk-ts/scripts/smoke-test.ts`: create sandbox → wait ready →
//! commands.run → files.write/read → kill.
//!
//! Run with: `HEYO_API_KEY=... cargo test --test smoke -- --ignored --nocapture`

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use heyo_sdk::{
    CommandRunOptions, FileOptions, Sandbox, SandboxCreateOptions, SandboxSize,
};

#[tokio::test]
#[ignore]
async fn smoke_sandbox_round_trip() {
    common::load_dotenv();
    let Some(opts) = common::client_options() else {
        eprintln!("[smoke] skipping — HEYO_API_KEY not set");
        return;
    };

    println!("[smoke] base URL: {}", common::base_url());
    println!("[smoke] listing sandboxes…");
    let existing = Sandbox::list(opts.clone()).await.expect("list");
    println!("[smoke] found {} sandbox(es)", existing.len());
    for s in existing.iter().take(5) {
        println!("  - {}  {:?}  {}", s.id, s.status, s.name);
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    println!("[smoke] creating sandbox…");
    let sandbox = Sandbox::create(
        SandboxCreateOptions {
            name: Some(format!("sdk-rs-smoke-{}", stamp)),
            image: Some("ubuntu:24.04".into()),
            size_class: Some(SandboxSize::Micro),
            ttl_seconds: Some(300),
            ..Default::default()
        },
        opts,
    )
    .await
    .expect("create");
    println!("[smoke] created {}", sandbox.sandbox_id());

    let result = sandbox
        .commands()
        .run("echo hello && uname -a", CommandRunOptions::default())
        .await;
    match result {
        Ok(r) => {
            println!("[smoke] exit={}", r.exit_code);
            println!("[smoke] stdout: {}", r.stdout.trim());
            if !r.stderr.trim().is_empty() {
                println!("[smoke] stderr: {}", r.stderr.trim());
            }
            assert_eq!(r.exit_code, 0, "echo should succeed");
            assert!(r.stdout.contains("hello"), "stdout should contain 'hello'");
        }
        Err(e) => {
            // Try cleanup before failing.
            let _ = sandbox.kill().await;
            panic!("commands.run failed: {}", e);
        }
    }

    // Files round-trip.
    println!("[smoke] write+read /workspace/sdk-rs-smoke.txt…");
    let payload = "from sdk-rs smoke test\n";
    sandbox
        .files()
        .write("sdk-rs-smoke.txt", payload, FileOptions::default())
        .await
        .expect("files.write");
    let back = sandbox
        .files()
        .read_text("sdk-rs-smoke.txt", FileOptions::default())
        .await
        .expect("files.read");
    assert_eq!(back, payload, "round-tripped file should match");
    println!("[smoke] files round-trip ok");

    println!("[smoke] killing sandbox…");
    sandbox.kill().await.expect("kill");
    println!("[smoke] done");
}
