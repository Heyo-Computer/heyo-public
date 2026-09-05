//! Host-machine disk metrics for the monitoring page.
//!
//! CPU and memory for the host come from heyvmd's own sampler
//! (`model::fetch_host_usage`), but its usage snapshot carries no disk data —
//! and the pooler runs directly on the host (a supervisord program alongside
//! heyvmd, not inside a VM), so the most reliable source for host disk
//! saturation is the host's own filesystem, read here via `df`. This is the
//! host counterpart to the *guest* `df` the registry runs over the PG pool.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

/// Bound the `df` fork so a wedged filesystem (e.g. a hung network mount that
/// slipped past `--local`) can't hang a monitoring-page render.
const DF_TIMEOUT: Duration = Duration::from_secs(3);

/// One real host filesystem's space usage, in bytes.
pub struct HostDisk {
    /// The backing device (`df`'s Filesystem column).
    pub source: String,
    /// Mount point.
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
}

impl HostDisk {
    /// Fraction full in `[0, 1]`, computed as `used / (used + avail)` — the same
    /// basis as `df`'s Use% (it excludes root-reserved blocks), so this lines up
    /// with what the shell reports rather than drifting a few points off `total`.
    pub fn saturation(&self) -> f64 {
        let denom = self.used + self.avail;
        if denom == 0 {
            0.0
        } else {
            self.used as f64 / denom as f64
        }
    }
}

/// Read real host filesystems' saturation via `df`. Pseudo/virtual filesystems
/// (tmpfs, devtmpfs, overlay, squashfs, …) are excluded so the view shows only
/// disks that can actually fill and take the host down with them. Bounded and
/// best-effort: a `df` failure or timeout is surfaced as an error the caller
/// renders as "unavailable", never a panic.
pub async fn host_disks() -> Result<Vec<HostDisk>> {
    // -k: 1024-byte blocks, -P: POSIX one-line-per-filesystem output (stable to
    // parse), --local: skip network mounts, -x: drop each pseudo filesystem
    // type. All GNU coreutils `df` flags (this is a Linux host).
    let out = tokio::time::timeout(
        DF_TIMEOUT,
        Command::new("df")
            .args([
                "-kP",
                "--local",
                "-x",
                "tmpfs",
                "-x",
                "devtmpfs",
                "-x",
                "overlay",
                "-x",
                "squashfs",
                "-x",
                "efivarfs",
            ])
            .output(),
    )
    .await
    .context("df timed out")?
    .context("running df")?;
    if !out.status.success() {
        anyhow::bail!(
            "df exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_df(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `df -kP` output into [`HostDisk`]s. The POSIX format is one line per
/// filesystem with columns: `Filesystem 1024-blocks Used Available Capacity
/// Mounted-on`. We read the four numeric-ish columns from the left and treat the
/// remainder of the line as the mount point (which can contain spaces); a device
/// name with spaces is vanishingly rare and would only mislabel, never crash.
/// Blocks are 1 KiB, scaled to bytes here. Malformed lines are skipped.
fn parse_df(stdout: &str) -> Vec<HostDisk> {
    stdout
        .lines()
        .skip(1) // header row
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let source = it.next()?.to_string();
            let total_k: u64 = it.next()?.parse().ok()?;
            let used_k: u64 = it.next()?.parse().ok()?;
            let avail_k: u64 = it.next()?.parse().ok()?;
            let _capacity = it.next()?; // "NN%", recomputed from bytes instead
            let mount = it.collect::<Vec<_>>().join(" ");
            if mount.is_empty() {
                return None;
            }
            Some(HostDisk {
                source,
                mount,
                total: total_k * 1024,
                used: used_k * 1024,
                avail: avail_k * 1024,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_df_posix_output() {
        // `df -kP` output: header then one line per fs. Includes a mount point
        // with a space to exercise the rest-of-line mount parse.
        let out = "\
Filesystem     1024-blocks      Used Available Capacity Mounted on
/dev/nvme0n1p2   982940500 412000000 521000000      45% /
/dev/nvme0n1p1      523248     12000    511248       3% /boot/efi
/dev/sdb1        10485760   1048576   9437184      10% /mnt/data disk
";
        let disks = parse_df(out);
        assert_eq!(disks.len(), 3);
        assert_eq!(disks[0].source, "/dev/nvme0n1p2");
        assert_eq!(disks[0].mount, "/");
        assert_eq!(disks[0].total, 982_940_500 * 1024);
        assert_eq!(disks[0].used, 412_000_000 * 1024);
        // used / (used + avail)
        let sat = disks[0].saturation();
        assert!((sat - 412_000_000.0 / (412_000_000.0 + 521_000_000.0)).abs() < 1e-9);
        // Mount point with an embedded space is preserved.
        assert_eq!(disks[2].mount, "/mnt/data disk");
    }

    #[test]
    fn skips_malformed_and_empty_lines() {
        let out = "\
Filesystem 1024-blocks Used Available Capacity Mounted on
garbage line without enough columns
/dev/sda1 1000 400 600 40% /
";
        let disks = parse_df(out);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].mount, "/");
    }

    #[test]
    fn saturation_handles_zero_capacity() {
        let d = HostDisk {
            source: "none".into(),
            mount: "/x".into(),
            total: 0,
            used: 0,
            avail: 0,
        };
        assert_eq!(d.saturation(), 0.0);
    }
}
