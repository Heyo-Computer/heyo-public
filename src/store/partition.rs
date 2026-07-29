//! Hive-style partition paths.
//!
//! ```text
//! <data>/logs/deployment=<id>/date=2026-07-28/hour=17/<ulid>.parquet
//! <data>/metrics/deployment=<id>/date=2026-07-28/<ulid>.parquet
//! ```
//!
//! The `key=value` directory convention is what lets DataFusion recover the
//! partition columns from the path and skip whole directories that a `WHERE`
//! clause excludes — the difference between a day-scoped query reading one
//! directory and reading the entire history.

use super::schema::Table;
use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use std::path::{Path, PathBuf};

/// Longest accepted deployment id. Well beyond anything app-lb produces, and
/// short enough that the assembled path stays clear of `PATH_MAX`.
const MAX_DEPLOYMENT_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartitionKey {
    pub table: Table,
    pub deployment: String,
    /// `YYYY-MM-DD`, UTC.
    pub date: String,
    /// `None` for daily tables.
    pub hour: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PartitionError {
    /// The deployment id could not be used as a directory name.
    BadDeployment(String),
    /// The timestamp did not correspond to a real instant.
    BadTimestamp(i64),
}

impl std::fmt::Display for PartitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadDeployment(d) => write!(
                f,
                "deployment id {d:?} is not usable as a path segment (allowed: \
                 letters, digits, dot, dash, underscore; 1-{MAX_DEPLOYMENT_LEN} chars)"
            ),
            Self::BadTimestamp(ts) => write!(f, "timestamp {ts} is not a valid instant"),
        }
    }
}

impl std::error::Error for PartitionError {}

/// Validate a deployment id for use as a directory name.
///
/// **Rejects rather than sanitises.** This value reaches us from two untrusted
/// places — app-lb deployment specs, whose ids are only checked for emptiness,
/// and the ingest endpoint, where the sender is a customer application. Mangling
/// a bad id into a legal one would let two distinct deployments collapse into
/// one directory and mix their logs; rejecting is unambiguous and gives the
/// sender a 400 to fix. The character set also makes `..` and `/` unrepresentable,
/// so no input can escape the data directory.
fn check_deployment(deployment: &str) -> Result<(), PartitionError> {
    let ok = !deployment.is_empty()
        && deployment.len() <= MAX_DEPLOYMENT_LEN
        && deployment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        // Belt and braces: the byte filter already excludes `/`, but a name of
        // all dots would still be a relative path component.
        && deployment.bytes().any(|b| b != b'.');
    if ok {
        Ok(())
    } else {
        Err(PartitionError::BadDeployment(deployment.to_string()))
    }
}

impl PartitionKey {
    /// Derive the partition a record belongs to from its timestamp.
    pub fn new(table: Table, deployment: &str, ts_millis: i64) -> Result<Self, PartitionError> {
        check_deployment(deployment)?;
        let ts = Utc
            .timestamp_millis_opt(ts_millis)
            .single()
            .ok_or(PartitionError::BadTimestamp(ts_millis))?;
        Ok(Self::from_datetime(table, deployment, ts))
    }

    fn from_datetime(table: Table, deployment: &str, ts: DateTime<Utc>) -> Self {
        Self {
            table,
            deployment: deployment.to_string(),
            date: format!("{:04}-{:02}-{:02}", ts.year(), ts.month(), ts.day()),
            hour: table.hourly().then(|| ts.hour()),
        }
    }

    /// The directory this partition's files live in, under `data_dir`.
    pub fn dir(&self, data_dir: &Path) -> PathBuf {
        let mut path = data_dir
            .join(self.table.name())
            .join(format!("deployment={}", self.deployment))
            .join(format!("date={}", self.date));
        if let Some(hour) = self.hour {
            path.push(format!("hour={hour:02}"));
        }
        path
    }
}

/// Recover the date from a `date=YYYY-MM-DD` directory name.
///
/// Used by the retention sweeper, which walks directories rather than reading
/// files — deciding what to drop should never require opening a parquet.
pub fn date_from_dir_name(name: &str) -> Option<chrono::NaiveDate> {
    let value = name.strip_prefix("date=")?;
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-28T17:34:56Z
    const TS: i64 = 1_785_260_096_000;

    #[test]
    fn logs_partition_by_hour_and_metrics_by_day() {
        let data = Path::new("/data");

        let logs = PartitionKey::new(Table::Logs, "demo", TS).unwrap();
        assert_eq!(
            logs.dir(data),
            Path::new("/data/logs/deployment=demo/date=2026-07-28/hour=17"),
        );

        let metrics = PartitionKey::new(Table::Metrics, "demo", TS).unwrap();
        assert_eq!(
            metrics.dir(data),
            Path::new("/data/metrics/deployment=demo/date=2026-07-28"),
        );
    }

    #[test]
    fn hours_are_zero_padded_so_directories_sort() {
        // hour=9 would sort after hour=10 lexically, which makes both a manual
        // `ls` and any path-ordered scan misleading.
        let early = PartitionKey::new(Table::Logs, "demo", 1_785_207_600_000).unwrap();
        assert_eq!(early.hour, Some(3));
        assert!(early.dir(Path::new("/d")).ends_with("hour=03"));
    }

    #[test]
    fn partitions_roll_over_at_utc_midnight() {
        // One millisecond either side of midnight must land in different days,
        // and in the last/first hour respectively.
        let before = PartitionKey::new(Table::Logs, "demo", 1_785_283_199_999).unwrap();
        let after = PartitionKey::new(Table::Logs, "demo", 1_785_283_200_000).unwrap();
        assert_eq!((before.date.as_str(), before.hour), ("2026-07-28", Some(23)));
        assert_eq!((after.date.as_str(), after.hour), ("2026-07-29", Some(0)));
    }

    #[test]
    fn deployment_ids_that_could_escape_the_data_dir_are_rejected() {
        // The ingest endpoint takes this field straight from a customer
        // application, so a traversal here would write parquet anywhere the
        // process can reach.
        for bad in [
            "../../etc/cron.d/x",
            "a/b",
            "..",
            "...",
            "",
            "a\0b",
            "a b",
            &"x".repeat(MAX_DEPLOYMENT_LEN + 1),
        ] {
            assert!(
                PartitionKey::new(Table::Logs, bad, TS).is_err(),
                "{bad:?} must be rejected, not sanitised into something legal",
            );
        }
    }

    #[test]
    fn realistic_deployment_ids_are_accepted() {
        for good in ["demo", "vault-86a37f", "app-lb-admin", "a.b_c-1", "x"] {
            assert!(
                PartitionKey::new(Table::Logs, good, TS).is_ok(),
                "{good:?} is the shape app-lb actually produces",
            );
        }
    }

    #[test]
    fn absurd_timestamps_are_rejected_rather_than_panicking() {
        assert_eq!(
            PartitionKey::new(Table::Logs, "demo", i64::MAX),
            Err(PartitionError::BadTimestamp(i64::MAX)),
        );
    }

    #[test]
    fn date_dir_names_round_trip() {
        let key = PartitionKey::new(Table::Metrics, "demo", TS).unwrap();
        let parsed = date_from_dir_name(&format!("date={}", key.date)).unwrap();
        assert_eq!(parsed.to_string(), "2026-07-28");
        assert_eq!(date_from_dir_name("hour=03"), None);
        assert_eq!(date_from_dir_name("date=not-a-date"), None);
    }
}
