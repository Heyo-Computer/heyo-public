//! End-to-end: create a KVM sandbox from a public image, verify it reaches
//! `running`, open an interactive shell and exercise it, then tear it down.
//!
//! Public-image discovery is dynamic: we list `/public-images?backend=kvm`
//! and use the first match. Override with `HEYO_KVM_IMAGE` if you need to
//! pin a specific image id/name. If no KVM public images are available the
//! test skips rather than failing — the cloud doesn't always have one
//! published.
//!
//! KVM provisioning is meaningfully slower than libvirt/firecracker today
//! (image download + ext4 mount-image build + VM boot), so we bump
//! `wait_for_ready` to 10 minutes.
//!
//! ## Targeting an environment
//!
//! The base URL is resolved from env (see `tests/common/mod.rs::base_url`):
//!
//! - `HEYO_BASE_URL=https://…` — explicit URL wins.
//! - `HEYO_ENV=production`     — `https://server.heyo.computer`.
//! - `HEYO_ENV=local` (default)— `http://localhost:4445`.
//!
//! Your API key must match the targeted environment — a prod key will get
//! 401/403 from the local server and vice versa.
//!
//! Region defaults to `US`; override with `HEYO_REGION=EU`.
//!
//! Examples:
//! ```text
//! # Production
//! HEYO_API_KEY=heyo_... HEYO_ENV=production \
//!   cargo test --test kvm_e2e -- --ignored --nocapture
//!
//! # Local dev (default)
//! HEYO_API_KEY=<local-key> \
//!   cargo test --test kvm_e2e -- --ignored --nocapture
//! ```

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use heyo_sdk::{
    HeyoError, Sandbox, SandboxCreateOptions, SandboxDriver, SandboxRegion, SandboxSize,
    SandboxStatus, ShellOptions,
};
use tokio::time::{sleep, timeout};

const READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Per-HTTP-request timeout. The SDK default is 60s (`client.rs:16`), which
/// is too tight for `/sandbox-deploy` in production: that handler does
/// account/entitlement lookups, picks a backend, writes a row, and waits
/// for the JetStream publish ack before returning. Anything that stacks up
/// on a cold path can blow past 60s. Anything beyond ~3 min is a real bug
/// on the server side, not flake.
const HTTP_TIMEOUT: Duration = Duration::from_secs(180);

#[tokio::test]
#[ignore]
async fn kvm_create_shell_and_kill() {
    common::load_dotenv();
    let Some(mut opts) = common::client_options() else {
        eprintln!("[kvm-e2e] skipping — HEYO_API_KEY not set");
        return;
    };
    opts.timeout = Some(HTTP_TIMEOUT);
    let region = resolve_region();
    println!("[kvm-e2e] base URL: {}", common::base_url());
    println!("[kvm-e2e] http timeout: {:?}", HTTP_TIMEOUT);
    println!("[kvm-e2e] region: {:?} (override with HEYO_REGION=US|EU)", region);

    let image = match resolve_kvm_image(opts.clone()).await {
        Ok(Some(image)) => image,
        Ok(None) => {
            eprintln!(
                "[kvm-e2e] skipping — no public KVM images available (set HEYO_KVM_IMAGE to override)"
            );
            return;
        }
        // The SDK reuses `HeyoError::Authentication` for both "no api key set
        // locally" and "server returned 401/403" (see client.rs:258), so the
        // default Display is misleading when the cause is the latter. Rewrite
        // it with the base URL so the user can immediately see whether the
        // key targets the right environment.
        Err(HeyoError::Authentication) => panic!(
            "list public kvm images: server at {} rejected HEYO_API_KEY (401/403). \
             Check the key matches the target env. Set HEYO_ENV=production or \
             HEYO_BASE_URL=… to switch.",
            common::base_url()
        ),
        Err(e) => panic!("list public kvm images: {}", e),
    };
    println!("[kvm-e2e] using image: {}", image);

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    println!("[kvm-e2e] creating KVM sandbox (waiting up to {:?} for ready)…", READY_TIMEOUT);
    let create_started = std::time::Instant::now();
    let sandbox = match Sandbox::create(
        SandboxCreateOptions {
            name: Some(format!("sdk-rs-kvm-e2e-{}", stamp)),
            driver: Some(SandboxDriver::Kvm),
            image: Some(image),
            region: Some(region),
            size_class: Some(SandboxSize::Micro),
            ttl_seconds: Some(600),
            wait_for_ready: Some(READY_TIMEOUT),
            ..Default::default()
        },
        opts,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            let elapsed = create_started.elapsed();
            let hint = match &e {
                HeyoError::Api { status: 504, .. } => format!(
                    " (elapsed {:?}; nginx upstream timeout — `post_sandbox_deploy` did not \
                     respond within the gateway's read budget. That handler should queue \
                     to NATS and return ~immediately, so this is a server-side bug, not a \
                     test issue. Suspect JetStream publish-ack blocking or auth-service \
                     entitlement roundtrip.)",
                    elapsed
                ),
                _ if elapsed >= HTTP_TIMEOUT - Duration::from_secs(5) => format!(
                    " (elapsed {:?} ≈ HTTP_TIMEOUT — client gave up waiting; raise \
                     HTTP_TIMEOUT or fix the server)",
                    elapsed
                ),
                _ => format!(" (elapsed {:?})", elapsed),
            };
            panic!("create kvm sandbox: {}{}", e, hint);
        }
    };
    println!(
        "[kvm-e2e] created {} in {:.1?}",
        sandbox.sandbox_id(),
        create_started.elapsed()
    );

    // Run the rest of the exercise inside a closure so we always kill on
    // exit, even if an assertion blows up.
    let outcome = run_exercise(&sandbox).await;

    println!("[kvm-e2e] killing sandbox…");
    if let Err(e) = sandbox.kill().await {
        eprintln!("[kvm-e2e]   ⚠ kill failed: {}", e);
    }
    outcome.expect("kvm e2e exercise");
    println!("[kvm-e2e] done");
}

async fn run_exercise(sandbox: &Sandbox) -> Result<(), String> {
    // 1. Verify the cloud reports `running`.
    let info = sandbox.info().await.map_err(|e| format!("info: {}", e))?;
    if info.status != SandboxStatus::Running {
        return Err(format!(
            "expected status=running after create+wait, got {:?} (error_message={:?})",
            info.status, info.error_message
        ));
    }
    println!("[kvm-e2e]   ✓ status=running");

    // 2. Open a shell and exercise it. Mirrors `tests/shell_protocol.rs`
    // but lighter — we only need to prove a live shell, not the full
    // protocol surface.
    println!("[kvm-e2e] opening shell…");
    let shell = sandbox
        .shell(ShellOptions {
            cols: 100,
            rows: 30,
            ..Default::default()
        })
        .await
        .map_err(|e| format!("shell open: {}", e))?;
    let session_id = shell
        .session_id()
        .await
        .ok_or_else(|| "session_id should be set after open()".to_string())?;
    println!("[kvm-e2e]   ✓ shell ready (session_id={})", session_id);

    use std::sync::{Arc, Mutex};
    let buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let collector_buf = buffer.clone();
    let mut output = shell.output();
    let collector = tokio::spawn(async move {
        while let Some(chunk) = output.next().await {
            if let Ok(text) = std::str::from_utf8(&chunk) {
                let cleaned = strip_ansi(text);
                collector_buf.lock().unwrap().push_str(&cleaned);
            }
        }
    });

    // Drain initial prompt/MOTD.
    sleep(Duration::from_millis(500)).await;
    drain(&buffer);

    // Sentinel echo proves the shell is actually executing commands inside
    // the guest, not just connected.
    let sentinel = format!("kvm-e2e-sentinel-{}", session_id);
    let cmd = format!("echo {} && uname -a", sentinel);
    write_line(&shell, &cmd).await?;
    // KVM guests can be a bit slow to flush over the PTY on first command.
    sleep(Duration::from_secs(2)).await;
    let out = drain(&buffer);
    if !out.contains(&sentinel) {
        return Err(format!(
            "shell did not echo sentinel; got: {:?}",
            out
        ));
    }
    if !out.contains("Linux") {
        return Err(format!(
            "uname -a should report Linux; got: {:?}",
            out
        ));
    }
    println!("[kvm-e2e]   ✓ shell executed `echo` + `uname -a` inside the guest");

    // 3. Graceful close.
    shell.close().await.map_err(|e| format!("shell close: {}", e))?;
    let _ = timeout(Duration::from_secs(5), async {
        while !shell.is_closed() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if !shell.is_closed() {
        return Err("shell did not close within 5s".into());
    }
    println!("[kvm-e2e]   ✓ shell closed (exit={:?})", shell.exit_code().await);

    collector.abort();
    let _ = collector.await;
    Ok(())
}

/// Pick the deploy region. Default `US`. Override via `HEYO_REGION=EU` (or
/// `=US` for explicitness). Unknown values fall back to US with a warning.
fn resolve_region() -> SandboxRegion {
    match std::env::var("HEYO_REGION").ok().as_deref() {
        Some(v) => match v.trim().to_ascii_uppercase().as_str() {
            "US" => SandboxRegion::US,
            "EU" => SandboxRegion::EU,
            other => {
                eprintln!(
                    "[kvm-e2e] HEYO_REGION={:?} not recognized; falling back to US",
                    other
                );
                SandboxRegion::US
            }
        },
        None => SandboxRegion::US,
    }
}

/// Pick a KVM public image. `HEYO_KVM_IMAGE` wins if set; otherwise we list
/// `/public-images?backend=kvm` and take the first.
async fn resolve_kvm_image(
    opts: heyo_sdk::HeyoClientOptions,
) -> Result<Option<String>, heyo_sdk::HeyoError> {
    if let Ok(forced) = std::env::var("HEYO_KVM_IMAGE") {
        if !forced.is_empty() {
            return Ok(Some(forced));
        }
    }
    let images = Sandbox::list_public_images(Some("kvm"), opts).await?;
    // Prefer image `name` (human-friendly slug callers can also pass) over
    // the raw id; fall back to id.
    Ok(images
        .into_iter()
        .next()
        .map(|img| img.name.unwrap_or(img.id)))
}

async fn write_line(shell: &heyo_sdk::ShellSession, cmd: &str) -> Result<(), String> {
    let mut bytes = cmd.as_bytes().to_vec();
    bytes.push(b'\n');
    shell.write(&bytes).await.map_err(|e| e.to_string())
}

fn drain(buf: &std::sync::Arc<std::sync::Mutex<String>>) -> String {
    let mut g = buf.lock().unwrap();
    std::mem::take(&mut *g)
}

/// Strip CSI/OSC sequences so substring assertions survive prompt redraws.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    chars.next();
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    continue;
                } else if next == ']' {
                    chars.next();
                    for d in chars.by_ref() {
                        if d == 0x07 as char {
                            break;
                        }
                    }
                    continue;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}
