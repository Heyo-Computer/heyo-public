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
//! Alongside the counters there is a second, *valued* stream: timing samples
//! ([`Timing`]), which answer "how long does this take" rather than "how often
//! does it happen". They live in their own `timings-YYYY-MM-DD.tsv` partitions
//! rather than as a third column on the event lines, so the event format stays
//! exactly what every older binary can still parse — a rollback keeps its
//! charts and simply ignores the timing files.
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

/// Something the monitoring page reports a *distribution* for, not a count.
///
/// Separate from [`Event`] because the two answer different questions and are
/// stored differently: an event line is `(when, what)`, a timing line is
/// `(when, what, how long)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Timing {
    /// End-to-end creation of a brand-new VM: the daemon's deploy call through
    /// the VM reporting ready. Recorded for schema VMs and warm spares alike,
    /// paired 1:1 with [`Event::VmCreated`], and only on success — a create
    /// that failed or timed out is a different measurement (it is bounded by
    /// `ready_timeout`, so including it would drag every percentile toward
    /// that ceiling and hide what a working create costs).
    ///
    /// Excludes the wait for a bring-up slot: that measures how many other
    /// creates are already in flight, not what this one costs.
    VmCreate,
}

impl Timing {
    /// Stable on-disk token — part of the partition file format, never rename.
    fn as_str(self) -> &'static str {
        match self {
            Timing::VmCreate => "vm_create",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "vm_create" => Some(Timing::VmCreate),
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

/// Hard cap on in-memory timing samples. Percentiles are computed by sorting
/// a copy of the window, so this also bounds that cost; 10k samples sort in
/// well under a millisecond and far exceed what a day of creates produces.
const TIMING_CAPACITY: usize = 10_000;

static LOG: Mutex<VecDeque<(u64, Event)>> = Mutex::new(VecDeque::new());
static JOURNAL: Mutex<VecDeque<JournalEntry>> = Mutex::new(VecDeque::new());
/// `(when, what, how many milliseconds)`. Millis rather than `Duration`
/// because that is the on-disk unit too, and no consumer wants finer.
static TIMINGS: Mutex<VecDeque<(u64, Timing, u32)>> = Mutex::new(VecDeque::new());

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
    let timings_loaded = load_timings_window(&dir, now);
    {
        let mut t = TIMINGS.lock().unwrap();
        for entry in &timings_loaded {
            push_timing_mem(&mut t, *entry);
        }
    }
    prune_partitions(&dir, now);
    info!(
        "metrics: {} event(s) + {} timing sample(s) + {} journal entrie(s) reloaded from {} \
         (daily partitions, {RETAIN_DAYS}-day retention)",
        loaded.len(),
        timings_loaded.len(),
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

/// Record how long one `kind` of operation took, timestamped now.
pub fn record_timing(kind: Timing, took: std::time::Duration) {
    record_timing_at(kind, took, now_unix());
}

fn record_timing_at(kind: Timing, took: std::time::Duration, t: u64) {
    // Saturating: a pathological sample must not wrap into a tiny one and
    // quietly drag the percentiles down. u32 millis tops out at ~49 days.
    let ms = took.as_millis().min(u32::MAX as u128) as u32;
    {
        let mut log = TIMINGS.lock().unwrap();
        push_timing_mem(&mut log, (t, kind, ms));
    }
    let Some(dir) = DIR.get() else {
        return;
    };
    if let Err(e) = append_timing(dir, t, kind, ms) {
        warn!(
            "metrics: appending to {} failed: {e}",
            timing_path(dir, t).display()
        );
    }
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

/// Push a timing sample into its bounded buffer, pruning by age and capacity —
/// the same policy as [`push_mem`], so the timing window and the chart window
/// always cover the same span.
fn push_timing_mem(log: &mut VecDeque<(u64, Timing, u32)>, entry: (u64, Timing, u32)) {
    let cutoff = entry.0.saturating_sub(RETAIN_HOURS * 3600);
    while log.front().is_some_and(|(ft, ..)| *ft < cutoff) {
        log.pop_front();
    }
    while log.len() >= TIMING_CAPACITY {
        log.pop_front();
    }
    log.push_back(entry);
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

/// Percentiles of one [`Timing`] over a trailing window. `None` from
/// [`timing_stats`] when the window holds no samples at all — "nothing has
/// been created in 24h" is a different statement from "creates take 0ms", and
/// the dashboard says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingStats {
    /// How many samples the percentiles were computed from. Shown alongside
    /// them because a p99 over four samples is not a p99, and an operator
    /// reading a latency figure needs to know which of those they have.
    pub count: usize,
    pub p50_ms: u32,
    pub p95_ms: u32,
    pub p99_ms: u32,
    /// The slowest sample in the window — the tail the percentiles clip.
    pub max_ms: u32,
}

/// Percentiles of `kind` over the trailing `hours` wall-clock hours.
///
/// Capped by the in-memory retention ([`RETAIN_HOURS`]), so asking for more
/// than that silently gets what is held rather than an error — the buffer is
/// the source of truth here, exactly as it is for the charts.
pub fn timing_stats(kind: Timing, hours: u64) -> Option<TimingStats> {
    timing_stats_at(kind, hours, now_unix())
}

fn timing_stats_at(kind: Timing, hours: u64, now: u64) -> Option<TimingStats> {
    let cutoff = now.saturating_sub(hours * 3600);
    let mut samples: Vec<u32> = {
        let log = TIMINGS.lock().unwrap();
        log.iter()
            .filter(|(t, k, _)| *k == kind && *t >= cutoff)
            .map(|(_, _, ms)| *ms)
            .collect()
    };
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    Some(TimingStats {
        count: samples.len(),
        p50_ms: percentile(&samples, 50),
        p95_ms: percentile(&samples, 95),
        p99_ms: percentile(&samples, 99),
        max_ms: *samples.last().expect("non-empty"),
    })
}

/// Nearest-rank percentile over an ascending slice: the smallest value at or
/// below which at least `p`% of the samples fall, i.e. element
/// `ceil(p/100 * n)` counting from 1.
///
/// Nearest-rank rather than an interpolating definition because every value
/// it can return is a measurement that actually happened. Interpolation would
/// invent a "p99 create time" that no create ever took, which for a latency
/// figure an operator is going to act on is worse than the small quantization
/// at low sample counts (which `TimingStats::count` already exposes).
fn percentile(sorted: &[u32], p: u32) -> u32 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    // ceil(p*n/100), clamped to a valid 1-based rank.
    let rank = ((p as usize * n).div_ceil(100)).clamp(1, n);
    sorted[rank - 1]
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

/// `timings-YYYY-MM-DD.tsv` (UTC) for the day containing `t`.
fn timing_path(dir: &Path, t: u64) -> PathBuf {
    dir.join(format!("timings-{}.tsv", date_str(t)))
}

/// Append one timing sample to its day's partition file.
fn append_timing(dir: &Path, t: u64, kind: Timing, ms: u32) -> std::io::Result<()> {
    let line = format!("{t}\t{}\t{ms}\n", kind.as_str());
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(timing_path(dir, t))
        .and_then(|mut f| f.write_all(line.as_bytes()))
}

/// Timing samples from the partitions overlapping the in-memory window,
/// oldest first. Unreadable files and unparseable lines are skipped — a
/// partition written by a binary that knew other [`Timing`] kinds contributes
/// the kinds this one understands and silently drops the rest.
fn load_timings_window(dir: &Path, now: u64) -> Vec<(u64, Timing, u32)> {
    let cutoff = now.saturating_sub(RETAIN_HOURS * 3600);
    let mut out: Vec<(u64, Timing, u32)> = Vec::new();
    for day_t in [now.saturating_sub(86_400), now] {
        let Ok(contents) = fs::read_to_string(timing_path(dir, day_t)) else {
            continue;
        };
        for line in contents.lines() {
            let mut f = line.splitn(3, '\t');
            let (Some(ts), Some(kind), Some(ms)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let (Ok(t), Some(kind), Ok(ms)) = (
                ts.trim().parse::<u64>(),
                Timing::parse(kind.trim()),
                ms.trim().parse::<u32>(),
            ) else {
                continue;
            };
            if t >= cutoff && t <= now + 3600 {
                out.push((t, kind, ms));
            }
        }
    }
    out.sort_by_key(|(t, ..)| *t);
    out
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

/// Delete `events-*.tsv` / `journal-*.tsv` / `timings-*.tsv` partitions older than
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
        let Some(date) = name.strip_suffix(".tsv").and_then(|n| {
            n.strip_prefix("events-")
                .or_else(|| n.strip_prefix("journal-"))
                .or_else(|| n.strip_prefix("timings-"))
        }) else {
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
    use std::time::Duration;

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

    /// `LOG` is a process-global that every test in this binary shares, and
    /// [`push_mem`] prunes by timestamp — so far-apart timestamp ranges do not
    /// isolate these tests, they are precisely what breaks them: whichever of
    /// the three runs last with the *highest* base evicts the others' entries.
    /// That raced silently until enough tests existed elsewhere in the binary
    /// to change the scheduling. Serialize them and start each from a clean
    /// log instead.
    ///
    /// `DIR` is never initialized in tests, so `record_at` stays memory-only;
    /// the file layer is tested through its helpers.
    static LOG_TESTS: Mutex<()> = Mutex::new(());

    /// Take the shared-log lock and empty the log. Returns the guard, which
    /// the caller must hold for the body of the test.
    fn exclusive_log() -> std::sync::MutexGuard<'static, ()> {
        // A poisoned lock only means some other test panicked; the log is
        // cleared below either way, so recover rather than cascade.
        let g = LOG_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
        g
    }

    #[test]
    fn buckets_align_to_hours_and_count_per_kind() {
        let _g = exclusive_log();
        let base = 1_000_000 * 3600; // exact hour boundary
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
        let _g = exclusive_log();
        let base = 2_000_000 * 3600;
        record_at(Event::VmCreated, base - 3600); // one hour before the window
        record_at(Event::VmCreated, base + 5);
        let c = hourly_counts_at(Event::VmCreated, 2, base + 3600 + 1);
        assert_eq!(c.iter().map(|(_, n)| n).sum::<u32>(), 1);
    }

    #[test]
    fn old_events_are_pruned_on_write() {
        let _g = exclusive_log();
        let base = 3_000_000 * 3600;
        record_at(Event::VmCreated, base);
        // A write RETAIN_HOURS+1h later prunes the first event.
        record_at(Event::VmCreated, base + (RETAIN_HOURS + 1) * 3600);
        let log = LOG.lock().unwrap();
        assert!(!log.iter().any(|(t, _)| *t == base));
    }

    /// Timing tokens are on-disk format, exactly like event tokens.
    #[test]
    // One variant today; the loop is the shape this test keeps as more are
    // added, and a missing token here silently zeroes historical data.
    #[allow(clippy::single_element_loop)]
    fn timing_tokens_roundtrip() {
        for k in [Timing::VmCreate] {
            assert_eq!(Timing::parse(k.as_str()), Some(k), "token {:?}", k.as_str());
        }
        assert_eq!(Timing::parse("from_the_future"), None);
    }

    /// Nearest-rank, checked against the definition: the p-th percentile is
    /// the smallest sample at or below which at least p% of them fall. Every
    /// answer must be a value that is actually in the input — that is the
    /// property the dashboard leans on when it says "a create that happened".
    #[test]
    fn percentiles_are_nearest_rank_and_never_interpolate() {
        // 1..=100, so the p-th percentile is exactly p.
        let hundred: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&hundred, 50), 50);
        assert_eq!(percentile(&hundred, 95), 95);
        assert_eq!(percentile(&hundred, 99), 99);

        // A single sample is every percentile of itself.
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 99), 7);

        // The classic tail case: one outlier in ten. p50 must ignore it and
        // p99 must find it — a mean would split the difference and describe
        // neither.
        let skewed = [1u32, 1, 1, 1, 1, 1, 1, 1, 1, 900];
        assert_eq!(percentile(&skewed, 50), 1);
        assert_eq!(percentile(&skewed, 99), 900);

        // Never interpolated: every output is an input.
        let odd = [3u32, 3, 10, 10, 10, 40, 40];
        for p in [50, 95, 99] {
            assert!(odd.contains(&percentile(&odd, p)), "p{p} invented a value");
        }
    }

    #[test]
    fn timing_stats_windows_by_kind_and_time() {
        let _g = exclusive_log();
        TIMINGS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let base = 6_000_000 * 3600;
        let now = base + 3600;
        for ms in [100u32, 200, 300, 400] {
            record_timing_at(Timing::VmCreate, Duration::from_millis(ms as u64), base + 10);
        }
        // Outside the window: must not move the percentiles.
        record_timing_at(
            Timing::VmCreate,
            Duration::from_millis(99_000),
            now - 5 * 3600,
        );

        let s = timing_stats_at(Timing::VmCreate, 2, now).expect("samples in window");
        assert_eq!(s.count, 4, "only the in-window samples count");
        assert_eq!(s.p50_ms, 200);
        assert_eq!(s.max_ms, 400);
        assert_eq!(s.p99_ms, 400);

        // An empty window is None, not a zeroed stat — "nothing was created"
        // must not render as "creates take 0ms".
        assert!(timing_stats_at(Timing::VmCreate, 1, now + 10 * 3600).is_none());
    }

    /// A pathological sample must saturate, never wrap into a tiny one and
    /// quietly pull the percentiles down.
    #[test]
    fn absurd_durations_saturate_rather_than_wrap() {
        let _g = exclusive_log();
        TIMINGS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let base = 7_000_000 * 3600;
        record_timing_at(Timing::VmCreate, Duration::from_secs(60 * 86_400), base);
        let s = timing_stats_at(Timing::VmCreate, 1, base + 60).unwrap();
        assert_eq!(s.max_ms, u32::MAX);
    }

    #[test]
    fn timing_partitions_write_load_and_prune() {
        let dir = std::env::temp_dir().join(format!("pgfc-timings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let day2 = 8_000_000 * 86_400;
        let now = day2 + 2 * 3600;
        append_timing(&dir, day2 + 50, Timing::VmCreate, 1_500).unwrap();
        append_timing(&dir, day2 + 60, Timing::VmCreate, 2_500).unwrap();
        // Before the window.
        append_timing(&dir, day2 - 26 * 3600, Timing::VmCreate, 9_999).unwrap();

        let loaded = load_timings_window(&dir, now);
        assert_eq!(loaded.len(), 2, "window-filtered load: {loaded:?}");
        assert_eq!(loaded[0], (day2 + 50, Timing::VmCreate, 1_500));

        // Garbage, an unknown kind and a non-numeric value are skipped.
        fs::write(
            timing_path(&dir, now),
            format!("nope
{now}	future_kind	5
{now}	vm_create	fast
{now}	vm_create	42
"),
        )
        .unwrap();
        let loaded = load_timings_window(&dir, now);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].2, 42);

        // Timing partitions age out with the others.
        let old = day2 - (RETAIN_DAYS + 2) * 86_400;
        append_timing(&dir, old, Timing::VmCreate, 1).unwrap();
        prune_partitions(&dir, day2);
        assert!(!timing_path(&dir, old).exists(), "old timing partition pruned");
        assert!(timing_path(&dir, now).exists());

        let _ = fs::remove_dir_all(&dir);
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
