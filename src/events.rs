//! Counters of notable pooler events for the monitoring page's per-hour
//! charts, backed by daily-partitioned files.
//!
//! Reads are served from a bounded in-memory buffer (same shape as
//! `dashboard::history`); every recorded event is *also* appended to a
//! partition file `events-YYYY-MM-DD.tsv` (UTC) under the metrics dir, and
//! startup reloads the partitions covering the chart window — so the charts
//! survive pooler restarts, which during an incident is exactly when the
//! restore/create history matters. Partitions are plain TSV (`unix_ts \t
//! kind`), append-only, never rewritten; old partitions are deleted whole
//! ([`RETAIN_DAYS`]), which is the point of partitioning by day.
//!
//! Events are recorded from the VM layer (`vm.rs`) through a process-global so
//! the recording sites don't need state threaded through them. Recording is an
//! uncontended-mutex push plus one small `O_APPEND` write (no fsync — this is
//! a metrics trail, not an audit log); before [`init`] (and in tests) it is
//! memory-only.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

/// Something the monitoring page counts per hour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A schema was successfully restored into a VM from its S3 archive.
    RestoreS3,
    /// A schema was successfully restored from a local frozen dump file.
    RestoreLocal,
    /// A new VM was created (schema VMs and warm spares alike).
    VmCreated,
    /// A schema moved one step down the offload ladder: dump-archived to S3,
    /// frozen, compacted, image-archived, or a local dump promoted to S3.
    /// Each one frees disk (or is about to, via the kill that follows).
    OffloadDone,
    /// A VM/sandbox directory was deleted outright: an orphan-sweep removal
    /// or a purge-pass kill. The other half of "are we draining the disk".
    VmDeleted,
    /// A warm spare was claimed off the shelf by a bring-up. Compared against
    /// the replenisher's target this shows whether the pool is sized for the
    /// claim rate ("0 ready" with a tall claims chart = demand, not failure).
    SpareClaimed,
}

impl Event {
    /// Stable on-disk token — part of the partition file format, never rename.
    fn as_str(self) -> &'static str {
        match self {
            Event::RestoreS3 => "restore_s3",
            Event::RestoreLocal => "restore_local",
            Event::VmCreated => "vm_created",
            Event::OffloadDone => "offload_done",
            Event::VmDeleted => "vm_deleted",
            Event::SpareClaimed => "spare_claimed",
        }
    }

    /// Unknown tokens (from a newer/older binary's files) parse to `None` and
    /// are skipped, never an error.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "restore_s3" => Some(Event::RestoreS3),
            "restore_local" => Some(Event::RestoreLocal),
            "vm_created" => Some(Event::VmCreated),
            "offload_done" => Some(Event::OffloadDone),
            "vm_deleted" => Some(Event::VmDeleted),
            "spare_claimed" => Some(Event::SpareClaimed),
            _ => None,
        }
    }
}

/// Severity of a journal entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Error,
}

impl Level {
    /// Stable on-disk token — part of the journal file format, never rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Error => "error",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Level::Info),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

/// One journal entry for the dashboard's events page: a timestamped, leveled,
/// kind-tagged human-readable line (an operation failure, a sweep summary).
#[derive(Clone, Debug)]
pub struct JournalEntry {
    pub t: u64,
    pub level: Level,
    /// Short dotted category ("archive", "freeze", "bring-up", "sweep.freeze").
    pub kind: String,
    pub msg: String,
}

/// In-memory retention: everything older than this many hours is pruned on
/// write. Kept a bit past the 24h the charts show so a bucket that's about to
/// scroll off is still complete.
const RETAIN_HOURS: u64 = 25;

/// Max journal entries held in memory (and shown on the events page's source
/// buffer). The daily partition files retain more ([`RETAIN_DAYS`]).
const JOURNAL_CAPACITY: usize = 1_000;

/// Hard cap on in-memory events, independent of age — a runaway create/restore
/// loop must not grow this without bound. 10k events far exceeds anything a
/// real hour sees.
const CAPACITY: usize = 10_000;

/// How many daily partition files to keep on disk. Generous relative to the
/// 24h charts so the files double as a greppable recent-activity record.
const RETAIN_DAYS: u64 = 14;

static LOG: Mutex<VecDeque<(u64, Event)>> = Mutex::new(VecDeque::new());
static JOURNAL: Mutex<VecDeque<JournalEntry>> = Mutex::new(VecDeque::new());

/// Metrics directory, set once by [`init`]. Unset (tests, or before init in
/// startup) means memory-only operation.
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// UTC day (days since epoch) of the last file append, for prune-on-rotation.
static LAST_DAY: AtomicU64 = AtomicU64::new(0);

/// Wire up the file backing: create `dir`, reload the partitions covering the
/// in-memory window, and prune expired ones. Call once at startup, before the
/// dashboard serves; recording works (memory-only) even if this is never
/// called or fails.
pub fn init(dir: PathBuf) {
    if let Err(e) = fs::create_dir_all(&dir) {
        warn!(
            "metrics: cannot create {} ({e}); event charts will not survive restarts",
            dir.display()
        );
        return;
    }
    let now = now_unix();
    let loaded = load_window(&dir, now);
    {
        let mut log = LOG.lock().unwrap();
        for entry in &loaded {
            push_mem(&mut log, *entry);
        }
    }
    let journal_loaded = load_journal_window(&dir, now);
    {
        let mut j = JOURNAL.lock().unwrap();
        for entry in journal_loaded.iter().cloned() {
            push_journal_mem(&mut j, entry);
        }
    }
    prune_partitions(&dir, now);
    info!(
        "metrics: {} event(s) + {} journal entrie(s) reloaded from {} \
         (daily partitions, {RETAIN_DAYS}-day retention)",
        loaded.len(),
        journal_loaded.len(),
        dir.display()
    );
    let _ = DIR.set(dir);
}

/// Record one occurrence of `event`, timestamped now.
pub fn record(event: Event) {
    record_at(event, now_unix());
}

fn record_at(event: Event, t: u64) {
    {
        let mut log = LOG.lock().unwrap();
        push_mem(&mut log, (t, event));
    }
    let Some(dir) = DIR.get() else {
        return;
    };
    let day = t / 86_400;
    // First write of a new UTC day starts a fresh partition; take the moment
    // to drop expired ones. swap() makes exactly one writer per rotation do it.
    if LAST_DAY.swap(day, Ordering::Relaxed) != day {
        prune_partitions(dir, t);
    }
    if let Err(e) = append_partition(dir, t, event) {
        warn!(
            "metrics: appending to {} failed: {e}",
            partition_path(dir, t).display()
        );
    }
}

/// Append one event line to its day's partition file.
fn append_partition(dir: &Path, t: u64, event: Event) -> std::io::Result<()> {
    let line = format!("{t}\t{}\n", event.as_str());
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(partition_path(dir, t))
        .and_then(|mut f| f.write_all(line.as_bytes()))
}

/// Journal an informational entry (a sweep summary, a completed offload).
pub fn journal_info(kind: &str, msg: impl Into<String>) {
    journal_at(Level::Info, kind, msg.into(), now_unix());
}

/// Journal a failure (a bring-up, archive, or freeze that errored).
pub fn journal_error(kind: &str, msg: impl Into<String>) {
    journal_at(Level::Error, kind, msg.into(), now_unix());
}

fn journal_at(level: Level, kind: &str, msg: String, t: u64) {
    let entry = JournalEntry {
        t,
        level,
        kind: kind.to_string(),
        // One line per entry is part of the file format; also keeps the
        // events page rows sane for multi-line anyhow chains.
        msg: sanitize(&msg),
    };
    {
        let mut j = JOURNAL.lock().unwrap();
        push_journal_mem(&mut j, entry.clone());
    }
    let Some(dir) = DIR.get() else {
        return;
    };
    if let Err(e) = append_journal(dir, &entry) {
        warn!(
            "metrics: appending to {} failed: {e}",
            journal_path(dir, entry.t).display()
        );
    }
}

/// Newest-first recent journal entries for the events page.
pub fn journal_recent(limit: usize) -> Vec<JournalEntry> {
    let j = JOURNAL.lock().unwrap();
    j.iter().rev().take(limit).cloned().collect()
}

fn push_journal_mem(j: &mut VecDeque<JournalEntry>, entry: JournalEntry) {
    while j.len() >= JOURNAL_CAPACITY {
        j.pop_front();
    }
    j.push_back(entry);
}

/// Tabs/newlines would break the one-line-per-entry TSV format.
fn sanitize(msg: &str) -> String {
    msg.replace(['\t', '\n', '\r'], " ")
}

/// Push into the bounded in-memory buffer, pruning by age and capacity.
fn push_mem(log: &mut VecDeque<(u64, Event)>, entry: (u64, Event)) {
    let cutoff = entry.0.saturating_sub(RETAIN_HOURS * 3600);
    while log.front().is_some_and(|(ft, _)| *ft < cutoff) {
        log.pop_front();
    }
    while log.len() >= CAPACITY {
        log.pop_front();
    }
    log.push_back(entry);
}

/// Per-hour counts of `event` for the trailing `buckets` wall-clock hours,
/// oldest first. Each entry is `(hour_start_unix, count)`; the last bucket is
/// the current (partial) hour. Buckets are aligned to whole UTC hours so bars
/// read as clock hours, not sliding windows.
pub fn hourly_counts(event: Event, buckets: usize) -> Vec<(u64, u32)> {
    hourly_counts_at(event, buckets, now_unix())
}

fn hourly_counts_at(event: Event, buckets: usize, now: u64) -> Vec<(u64, u32)> {
    let current_hour = now / 3600 * 3600;
    let start = current_hour.saturating_sub((buckets.saturating_sub(1) as u64) * 3600);
    let mut out: Vec<(u64, u32)> = (0..buckets as u64)
        .map(|i| (start + i * 3600, 0))
        .collect();
    let log = LOG.lock().unwrap();
    for (t, e) in log.iter() {
        if *e != event || *t < start {
            continue;
        }
        let idx = ((*t - start) / 3600) as usize;
        if let Some(slot) = out.get_mut(idx) {
            slot.1 += 1;
        }
    }
    out
}

// ---- daily partition files ------------------------------------------------

/// `YYYY-MM-DD` (UTC) of the day containing `t` — the partition date key.
fn date_str(t: u64) -> String {
    let (y, m, d) = civil_from_unix(t);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `events-YYYY-MM-DD.tsv` (UTC) for the day containing `t`.
fn partition_path(dir: &Path, t: u64) -> PathBuf {
    dir.join(format!("events-{}.tsv", date_str(t)))
}

/// `journal-YYYY-MM-DD.tsv` (UTC) for the day containing `t`.
fn journal_path(dir: &Path, t: u64) -> PathBuf {
    dir.join(format!("journal-{}.tsv", date_str(t)))
}

/// Append one journal entry to its day's partition file.
fn append_journal(dir: &Path, e: &JournalEntry) -> std::io::Result<()> {
    let line = format!("{}\t{}\t{}\t{}\n", e.t, e.level.as_str(), e.kind, e.msg);
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path(dir, e.t))
        .and_then(|mut f| f.write_all(line.as_bytes()))
}

/// Journal entries from the partitions overlapping the in-memory buffer
/// (today and yesterday in UTC), oldest first, capped at [`JOURNAL_CAPACITY`]
/// newest. Unreadable files and unparseable lines are skipped.
fn load_journal_window(dir: &Path, now: u64) -> Vec<JournalEntry> {
    let mut out: Vec<JournalEntry> = Vec::new();
    for day_t in [now.saturating_sub(86_400), now] {
        let Ok(contents) = fs::read_to_string(journal_path(dir, day_t)) else {
            continue;
        };
        for line in contents.lines() {
            let mut f = line.splitn(4, '\t');
            let (Some(ts), Some(level), Some(kind), Some(msg)) =
                (f.next(), f.next(), f.next(), f.next())
            else {
                continue;
            };
            let (Ok(t), Some(level)) = (ts.trim().parse::<u64>(), Level::parse(level)) else {
                continue;
            };
            out.push(JournalEntry {
                t,
                level,
                kind: kind.to_string(),
                msg: msg.to_string(),
            });
        }
    }
    out.sort_by_key(|e| e.t);
    if out.len() > JOURNAL_CAPACITY {
        out.drain(..out.len() - JOURNAL_CAPACITY);
    }
    out
}

/// Events from the partitions overlapping the in-memory window (today and
/// yesterday in UTC — [`RETAIN_HOURS`] ≤ 48h), oldest first, already filtered
/// to the window. Unreadable files and unparseable lines are skipped.
fn load_window(dir: &Path, now: u64) -> Vec<(u64, Event)> {
    let cutoff = now.saturating_sub(RETAIN_HOURS * 3600);
    let mut out: Vec<(u64, Event)> = Vec::new();
    for day_t in [now.saturating_sub(86_400), now] {
        let path = partition_path(dir, day_t);
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            let Some((ts, kind)) = line.split_once('\t') else {
                continue;
            };
            let (Ok(t), Some(e)) = (ts.trim().parse::<u64>(), Event::parse(kind.trim())) else {
                continue;
            };
            if t >= cutoff && t <= now + 3600 {
                out.push((t, e));
            }
        }
    }
    // Appends are chronological within a file, but be robust to clock steps.
    out.sort_by_key(|(t, _)| *t);
    out
}

/// Delete `events-*.tsv` / `journal-*.tsv` partitions older than
/// [`RETAIN_DAYS`]. Best-effort; anything not matching a partition name
/// pattern is left alone.
fn prune_partitions(dir: &Path, now: u64) {
    let cutoff = date_str(now.saturating_sub(RETAIN_DAYS * 86_400));
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date) = name
            .strip_suffix(".tsv")
            .and_then(|n| n.strip_prefix("events-").or_else(|| n.strip_prefix("journal-")))
        else {
            continue;
        };
        // Zero-padded ISO dates sort lexicographically = chronologically.
        if date < cutoff.as_str()
            && let Err(e) = fs::remove_file(entry.path())
        {
            warn!("metrics: pruning {} failed: {e}", entry.path().display());
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS` (UTC), for the events page.
pub fn fmt_ts(t: u64) -> String {
    let s = t % 86_400;
    format!(
        "{} {:02}:{:02}:{:02}",
        date_str(t),
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

/// Unix seconds → (year, month, day) in UTC. Howard Hinnant's civil-from-days
/// algorithm; exact for the entire u64-seconds range we can encounter.
fn civil_from_unix(t: u64) -> (i64, u32, u32) {
    let z = (t / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant's on-disk token must survive a write/reload cycle —
    /// a rename here silently zeroes historical chart data.
    #[test]
    fn event_tokens_roundtrip() {
        for e in [
            Event::RestoreS3,
            Event::RestoreLocal,
            Event::VmCreated,
            Event::OffloadDone,
            Event::VmDeleted,
            Event::SpareClaimed,
        ] {
            assert_eq!(Event::parse(e.as_str()), Some(e), "token {:?}", e.as_str());
        }
        assert_eq!(Event::parse("from_the_future"), None);
    }

    /// The global log is shared across tests in one binary, so tests use
    /// far-apart timestamp ranges instead of clearing it. `DIR` is never
    /// initialized in tests, so `record_at` stays memory-only; the file layer
    /// is tested through its helpers.
    #[test]
    fn buckets_align_to_hours_and_count_per_kind() {
        let base = 1_000_000 * 3600; // exact hour boundary, far from other tests
        let now = base + 3 * 3600 + 120; // 3 buckets later, 2 min in
        record_at(Event::RestoreS3, base + 10);
        record_at(Event::RestoreS3, base + 3599);
        record_at(Event::RestoreLocal, base + 20); // other kind: not counted
        record_at(Event::RestoreS3, base + 3 * 3600 + 60); // current hour

        let c = hourly_counts_at(Event::RestoreS3, 4, now);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], (base, 2));
        assert_eq!(c[1], (base + 3600, 0));
        assert_eq!(c[2], (base + 2 * 3600, 0));
        assert_eq!(c[3], (base + 3 * 3600, 1));

        let l = hourly_counts_at(Event::RestoreLocal, 4, now);
        assert_eq!(l.iter().map(|(_, n)| n).sum::<u32>(), 1);
    }

    #[test]
    fn events_before_the_window_are_ignored() {
        let base = 2_000_000 * 3600;
        record_at(Event::VmCreated, base - 3600); // one hour before the window
        record_at(Event::VmCreated, base + 5);
        let c = hourly_counts_at(Event::VmCreated, 2, base + 3600 + 1);
        assert_eq!(c.iter().map(|(_, n)| n).sum::<u32>(), 1);
    }

    #[test]
    fn old_events_are_pruned_on_write() {
        let base = 3_000_000 * 3600;
        record_at(Event::VmCreated, base);
        // A write RETAIN_HOURS+1h later prunes the first event.
        record_at(Event::VmCreated, base + (RETAIN_HOURS + 1) * 3600);
        let log = LOG.lock().unwrap();
        assert!(!log.iter().any(|(t, _)| *t == base));
    }

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        assert_eq!(civil_from_unix(86_399), (1970, 1, 1));
        assert_eq!(civil_from_unix(86_400), (1970, 1, 2));
        // Leap day 2024-02-29 12:00:00 UTC.
        assert_eq!(civil_from_unix(1_709_208_000), (2024, 2, 29));
        // 2026-07-26 (this feature's era) and a year rollover.
        assert_eq!(civil_from_unix(1_784_678_400), (2026, 7, 22));
        assert_eq!(civil_from_unix(1_767_225_599), (2025, 12, 31));
        assert_eq!(civil_from_unix(1_767_225_600), (2026, 1, 1));
    }

    #[test]
    fn partitions_write_load_and_prune() {
        let dir = std::env::temp_dir().join(format!("pgfc-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Two days of events written the way record_at writes them.
        let day2 = 4_000_000 * 86_400 / 86_400 * 86_400; // exact day boundary
        let day1 = day2 - 86_400;
        for (t, e) in [
            (day1 + 100, Event::RestoreS3),
            (day2 + 50, Event::VmCreated),
            (day2 + 60, Event::RestoreLocal),
        ] {
            append_partition(&dir, t, e).unwrap();
        }
        // Partition naming: one file per day.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);

        // Load at day2+2h: the day1 event is outside the 25h window's… no —
        // day1+100 is 26h+ before day2+2h? day2+2h - (day1+100) = 26h-100s,
        // > RETAIN_HOURS → filtered; both day2 events load.
        let now = day2 + 2 * 3600;
        let loaded = load_window(&dir, now);
        assert_eq!(loaded.len(), 2, "window-filtered load: {loaded:?}");
        assert_eq!(loaded[0], (day2 + 50, Event::VmCreated));
        assert_eq!(loaded[1], (day2 + 60, Event::RestoreLocal));

        // A garbage line and an unknown kind are skipped, not fatal.
        fs::write(
            partition_path(&dir, now),
            format!("not a line\n{}\tfuture_kind\n{}\tvm_created\n", now, now),
        )
        .unwrap();
        let loaded = load_window(&dir, now);
        assert_eq!(loaded.len(), 1);

        // Pruning: partitions (of both series) older than RETAIN_DAYS go;
        // recent ones stay.
        let old = day2 - (RETAIN_DAYS + 2) * 86_400;
        fs::write(partition_path(&dir, old), "1\tvm_created\n").unwrap();
        fs::write(journal_path(&dir, old), "1\tinfo\tx\ty\n").unwrap();
        // An unrelated file is never touched.
        fs::write(dir.join("notes.txt"), "keep me").unwrap();
        prune_partitions(&dir, day2);
        assert!(!partition_path(&dir, old).exists(), "old partition pruned");
        assert!(!journal_path(&dir, old).exists(), "old journal pruned");
        assert!(partition_path(&dir, day2).exists());
        assert!(dir.join("notes.txt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_writes_load_and_sanitizes() {
        let dir = std::env::temp_dir().join(format!("pgfc-journal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let day = 5_000_000 * 86_400 / 86_400 * 86_400;
        let entries = [
            JournalEntry {
                t: day + 10,
                level: Level::Error,
                kind: "archive".into(),
                msg: sanitize("schema x: boom\nline2\ttabbed"),
            },
            JournalEntry {
                t: day + 20,
                level: Level::Info,
                kind: "sweep.freeze".into(),
                msg: "froze 3/5".into(),
            },
        ];
        for e in &entries {
            append_journal(&dir, e).unwrap();
        }
        // Sanitized message stays one TSV line with the full text intact.
        let raw = fs::read_to_string(journal_path(&dir, day + 10)).unwrap();
        assert_eq!(raw.lines().count(), 2);
        assert!(raw.contains("schema x: boom line2 tabbed"));

        let loaded = load_journal_window(&dir, day + 3600);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].level, Level::Error);
        assert_eq!(loaded[0].msg, "schema x: boom line2 tabbed");
        assert_eq!(loaded[1].kind, "sweep.freeze");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_recent_is_newest_first() {
        // The global JOURNAL is shared across parallel tests, so assert only
        // on entries with our own distinctive kind, by relative order.
        for i in 0..5 {
            journal_at(Level::Info, "test.recent", format!("m{i}"), 6_000_000 + i);
        }
        let ours: Vec<_> = journal_recent(JOURNAL_CAPACITY)
            .into_iter()
            .filter(|e| e.kind == "test.recent")
            .collect();
        assert_eq!(ours.len(), 5);
        assert_eq!(ours[0].msg, "m4", "newest first");
        assert_eq!(ours[4].msg, "m0");
    }

    #[test]
    fn fmt_ts_is_utc_iso_like() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00");
        assert_eq!(fmt_ts(1_709_208_000), "2024-02-29 12:00:00");
    }
}
