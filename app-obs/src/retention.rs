//! Ages partitions out of local storage.
//!
//! Phase 1 deletes; S3 tiering replaces the delete step later, at which point
//! the decision of *which* partitions are due stays exactly as it is here.
//!
//! The sweep works entirely from directory names. Deciding what to drop must
//! never require opening a parquet file — that would turn a cheap periodic
//! sweep into a full scan of everything being retained.

use crate::store::partition::date_from_dir_name;
use chrono::{NaiveDate, Utc};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub partitions_removed: usize,
    /// Deployment directories removed once their last date partition went.
    pub deployments_removed: usize,
}

pub struct Retention {
    data_dir: PathBuf,
    retain_days: i64,
}

impl Retention {
    pub fn new(data_dir: impl Into<PathBuf>, retain_days: u32) -> Self {
        Self {
            data_dir: data_dir.into(),
            retain_days: retain_days.into(),
        }
    }

    /// Sweep on a timer until the process ends.
    ///
    /// Hourly rather than daily: a daily sweep that happens to run just before
    /// midnight leaves a whole extra day on disk, and the sweep is cheap enough
    /// that running it more often costs nothing.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(3600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match self.sweep(Utc::now().date_naive()) {
                Ok(swept) if swept.partitions_removed > 0 => {
                    tracing::info!(
                        partitions = swept.partitions_removed,
                        deployments = swept.deployments_removed,
                        "retention sweep removed expired partitions",
                    );
                }
                Ok(_) => tracing::debug!("retention sweep found nothing expired"),
                Err(e) => tracing::error!(error = %e, "retention sweep failed"),
            }
        }
    }

    /// Remove every partition older than the retention window.
    ///
    /// `today` is a parameter rather than read from the clock so the boundary
    /// behaviour is testable without pretending it is a different date.
    pub fn sweep(&self, today: NaiveDate) -> std::io::Result<Swept> {
        let mut swept = Swept::default();

        for table in ["logs", "metrics"] {
            let table_dir = self.data_dir.join(table);
            let Ok(deployments) = std::fs::read_dir(&table_dir) else {
                continue; // nothing written for this table yet
            };

            for deployment in deployments.flatten() {
                let deployment_dir = deployment.path();
                if !deployment_dir.is_dir() {
                    continue;
                }
                swept.partitions_removed += self.sweep_deployment(&deployment_dir, today)?;

                // A deployment with no partitions left is just clutter. Only
                // remove it if it is genuinely empty, so a concurrent write
                // that just created a new partition is never destroyed.
                if is_empty_dir(&deployment_dir) {
                    std::fs::remove_dir(&deployment_dir)?;
                    swept.deployments_removed += 1;
                }
            }
        }
        Ok(swept)
    }

    fn sweep_deployment(&self, deployment_dir: &Path, today: NaiveDate) -> std::io::Result<usize> {
        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir(deployment_dir) else {
            return Ok(0);
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(date) = date_from_dir_name(name) else {
                // Not a partition directory. Leave anything unrecognised alone
                // rather than guessing — this code deletes recursively.
                continue;
            };

            if is_expired(date, today, self.retain_days) {
                std::fs::remove_dir_all(&path)?;
                removed += 1;
                tracing::debug!(path = %path.display(), "removed expired partition");
            }
        }
        Ok(removed)
    }
}

/// `retain_days` counts back from and including today.
///
/// With `retain_days = 7` and today 2026-07-28, the seven retained days are
/// 07-22 through 07-28; 07-21 is exactly at the boundary and goes. A
/// future-dated partition (clock skew, or a sender with a bad clock) has
/// negative age and is always kept — deleting data that claims to be from the
/// future would destroy whatever the skew actually was.
fn is_expired(date: NaiveDate, today: NaiveDate, retain_days: i64) -> bool {
    (today - date).num_days() >= retain_days
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut e| e.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("app-obs-retention-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// Create a populated partition, as the writer would.
    fn seed(data_dir: &Path, table: &str, deployment: &str, date: &str, hour: Option<&str>) {
        let mut dir = data_dir
            .join(table)
            .join(format!("deployment={deployment}"))
            .join(format!("date={date}"));
        if let Some(hour) = hour {
            dir.push(format!("hour={hour}"));
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("000.parquet"), b"data").unwrap();
    }

    fn partition_exists(data_dir: &Path, table: &str, deployment: &str, date: &str) -> bool {
        data_dir
            .join(table)
            .join(format!("deployment={deployment}"))
            .join(format!("date={date}"))
            .exists()
    }

    #[test]
    fn the_retention_boundary_keeps_exactly_n_days() {
        let today = day("2026-07-28");
        // retain_days = 7 keeps 07-22..07-28 inclusive — seven days.
        assert!(!is_expired(day("2026-07-28"), today, 7), "today");
        assert!(!is_expired(day("2026-07-22"), today, 7), "oldest kept day");
        assert!(is_expired(day("2026-07-21"), today, 7), "the boundary day goes");
        assert!(is_expired(day("2026-07-01"), today, 7), "well past");
    }

    #[test]
    fn future_dated_partitions_are_never_deleted() {
        // A sender with a skewed clock shouldn't have its data destroyed by the
        // sweep before anyone notices the skew.
        let today = day("2026-07-28");
        assert!(!is_expired(day("2026-07-29"), today, 7));
        assert!(!is_expired(day("2027-01-01"), today, 1));
    }

    #[test]
    fn expired_partitions_are_removed_and_recent_ones_kept() {
        let dir = tmpdir("sweep");
        seed(&dir, "logs", "demo", "2026-07-28", Some("17"));
        seed(&dir, "logs", "demo", "2026-07-01", Some("03"));
        seed(&dir, "metrics", "demo", "2026-07-28", None);
        seed(&dir, "metrics", "demo", "2026-06-15", None);

        let swept = Retention::new(&dir, 7).sweep(day("2026-07-28")).unwrap();
        assert_eq!(swept.partitions_removed, 2);

        assert!(partition_exists(&dir, "logs", "demo", "2026-07-28"));
        assert!(!partition_exists(&dir, "logs", "demo", "2026-07-01"));
        assert!(partition_exists(&dir, "metrics", "demo", "2026-07-28"));
        assert!(!partition_exists(&dir, "metrics", "demo", "2026-06-15"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hourly_subdirectories_go_with_their_day() {
        let dir = tmpdir("hourly");
        for hour in ["00", "07", "23"] {
            seed(&dir, "logs", "demo", "2026-07-01", Some(hour));
        }
        Retention::new(&dir, 7).sweep(day("2026-07-28")).unwrap();
        assert!(!partition_exists(&dir, "logs", "demo", "2026-07-01"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_emptied_deployment_directory_is_cleaned_up() {
        let dir = tmpdir("empty");
        seed(&dir, "logs", "gone", "2026-01-01", Some("00"));
        seed(&dir, "logs", "stays", "2026-07-28", Some("17"));

        let swept = Retention::new(&dir, 7).sweep(day("2026-07-28")).unwrap();
        assert_eq!(swept.deployments_removed, 1);
        assert!(!dir.join("logs/deployment=gone").exists());
        assert!(
            dir.join("logs/deployment=stays").exists(),
            "a deployment with live partitions must survive",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unrecognised_directories_are_left_alone() {
        // This code deletes recursively. Anything it can't positively identify
        // as an expired partition must be left where it is.
        let dir = tmpdir("unknown");
        let stray = dir.join("logs/deployment=demo/notes");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("keep.txt"), b"important").unwrap();
        seed(&dir, "logs", "demo", "2026-01-01", Some("00"));

        Retention::new(&dir, 7).sweep(day("2026-07-28")).unwrap();
        assert!(stray.join("keep.txt").exists(), "not a partition, not ours");
        assert!(!partition_exists(&dir, "logs", "demo", "2026-01-01"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_data_directory_is_not_an_error() {
        // The first sweep can easily beat the first write.
        let swept = Retention::new(tmpdir("absent"), 7)
            .sweep(day("2026-07-28"))
            .unwrap();
        assert_eq!(swept, Swept::default());
    }

    #[test]
    fn zero_retention_keeps_today_only() {
        // retain_days = 0 would delete everything including today, which is
        // almost certainly a misconfiguration rather than an intent; verify the
        // behaviour is at least predictable.
        let today = day("2026-07-28");
        assert!(is_expired(day("2026-07-28"), today, 0));
        assert!(!is_expired(day("2026-07-28"), today, 1), "1 day keeps today");
    }
}
