//! Log tailing: host files (pooler, heyvmd) by seeking near the end, and the
//! per-VM Postgres log by running `tail` inside the guest.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use heyo_sdk::{CommandRunOptions, Sandbox};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::vm;

/// Never read more than this much off the end of a log file — the pooler/heyvmd
/// logs rotate at 20 MB, and an ops view only needs the recent tail.
const TAIL_CAP: u64 = 128 * 1024;
const GUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Return the last `max_lines` lines of `path`. Seeks to `len - TAIL_CAP` so a
/// large log isn't read whole; a missing file renders a friendly note.
pub async fn tail_file(path: &Path, max_lines: usize) -> Result<String> {
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("(log file not found at {})", path.display()));
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = f.metadata().await?.len();
    let start = len.saturating_sub(TAIL_CAP);
    if start > 0 {
        f.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    // If we seeked into the middle of a line, drop that partial first line.
    let text: &str = if start > 0 {
        match text.find('\n') {
            Some(i) => &text[i + 1..],
            None => &text,
        }
    } else {
        &text
    };
    let lines: Vec<&str> = text.lines().collect();
    let tail = if lines.len() > max_lines {
        &lines[lines.len() - max_lines..]
    } else {
        &lines[..]
    };
    Ok(tail.join("\n"))
}

/// Tail the guest's Postgres log (`$PGDATA/log/postgresql-<weekday>.log`). Only
/// works while the VM is running; `max_lines` is a number, safe to interpolate.
pub async fn tail_vm_log(id: &str, max_lines: usize) -> Result<String> {
    let sandbox =
        Sandbox::connect(id.to_string(), vm::local_opts()).context("connecting to VM")?;
    let cmd = format!("tail -n {max_lines} /workspace/pgdata/log/postgresql-*.log 2>/dev/null");
    let opts = CommandRunOptions {
        timeout: Some(GUEST_TIMEOUT),
        ..Default::default()
    };
    let res = tokio::time::timeout(
        GUEST_TIMEOUT + Duration::from_secs(2),
        sandbox.commands().run(&cmd, opts),
    )
    .await
    .context("guest log tail timed out")?
    .context("running guest log tail")?;
    let out = res.output;
    if out.trim().is_empty() {
        Ok("(no Postgres log output — VM may be stopped or the log not yet created)".to_string())
    } else {
        Ok(out)
    }
}
