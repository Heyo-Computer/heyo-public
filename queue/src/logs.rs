//! What nats-server is saying, tailed from its log file.
//!
//! ## Why a file and not a subscription
//!
//! NATS does not publish its own log. `$SYS` carries connection and account
//! events, and `/varz` carries counters, but the lines that say *why* — a
//! permissions violation, a slow consumer disconnect, a stream that failed to
//! restore — exist only in the log. So this reads the file, which means log
//! collection works only where this process and nats-server share a filesystem.
//! Everything else on the dashboard works against a remote monitoring port;
//! this one panel does not, and the API says so rather than showing an empty
//! box.
//!
//! ## Rotation
//!
//! The deployment this is written for captures nats-server's stdout through
//! supervisord, which rotates by renaming. A tailer that only followed its open
//! handle would go quiet at the first rotation and stay quiet, with no error to
//! notice. So at every end-of-file the path is re-checked: a different inode is
//! a rotation, and a file shorter than the read position is a truncation. Both
//! reopen from the start.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

use serde::Serialize;

/// How long to wait at end-of-file before looking again.
///
/// A log tail is the one part of this page that should feel immediate, and a
/// healthy NATS is silent for hours — so the cost of checking often is a
/// `stat` against the page cache, not a read.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(400);

/// How long to wait before retrying a file that could not be opened. Longer,
/// because the usual cause is a path that is wrong and will stay wrong.
const RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Ceiling on a single line kept in memory. nats-server does not write lines
/// this long; something that does is not a log line, and holding a gigabyte of
/// it would take the dashboard down with it.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// One parsed line.
///
/// `text` excludes the pid prefix nats-server puts on every line. It is the
/// same pid for the life of a server run, and a run change is already legible
/// in the log itself ("Server is ready"), so a column of identical numbers
/// would cost width and say nothing.
#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    /// Monotonic within this process, and the cursor the dashboard polls with:
    /// `?since=<seq>` returns only what has arrived since. Not stable across a
    /// restart of *this* process, which is fine — the buffer is not either.
    pub seq: u64,
    /// Exactly as nats-server wrote it, so it can be pasted into a grep.
    pub ts: Option<String>,
    pub ts_ms: Option<i64>,
    /// `info`, `warn`, `error`, `debug`, `trace`, `fatal` — or `None` for a
    /// line this parser did not recognise, which is deliberately never hidden.
    pub level: Option<&'static str>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogStatus {
    /// False when no log file is configured. The dashboard says which variable
    /// would turn the panel on rather than showing an empty box.
    pub enabled: bool,
    pub file: Option<String>,
    /// Set while the file cannot be read. Kept separate from `enabled`: "not
    /// configured" and "configured but unreadable" are different mistakes.
    pub error: Option<String>,
    /// Lines currently held. Named `lines_held` rather than `lines` because
    /// the log response flattens this struct beside its own `lines` array,
    /// and two fields serializing to one key is a silent overwrite.
    pub lines_held: usize,
    /// Lines evicted because the buffer is full. This climbing steadily means
    /// the window on the page is shorter than it looks.
    pub dropped: u64,
    pub capacity: usize,
    pub latest_seq: u64,
}

/// A bounded ring of the most recent lines.
///
/// In memory and nowhere else. This is a live view, not a log store — app-obs
/// is the thing in this repository that keeps logs, and pointing nats-server's
/// output at it is the answer to "I want last Tuesday".
pub struct LogBuffer {
    lines: Mutex<VecDeque<LogLine>>,
    capacity: usize,
    next_seq: AtomicU64,
    dropped: AtomicU64,
    file: Option<String>,
    error: Mutex<Option<String>>,
}

impl LogBuffer {
    pub fn new(capacity: usize, file: Option<String>) -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            capacity: capacity.max(1),
            next_seq: AtomicU64::new(1),
            dropped: AtomicU64::new(0),
            file,
            error: Mutex::new(None),
        }
    }

    fn lines(&self) -> std::sync::MutexGuard<'_, VecDeque<LogLine>> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push_raw(&self, raw: &str) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut line = parse(raw);
        line.seq = seq;
        let mut lines = self.lines();
        lines.push_back(line);
        while lines.len() > self.capacity {
            lines.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn set_error(&self, error: Option<String>) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = error;
    }

    pub fn status(&self) -> LogStatus {
        let lines = self.lines();
        LogStatus {
            enabled: self.file.is_some(),
            file: self.file.clone(),
            error: self.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            lines_held: lines.len(),
            dropped: self.dropped.load(Ordering::Relaxed),
            capacity: self.capacity,
            latest_seq: self.next_seq.load(Ordering::Relaxed).saturating_sub(1),
        }
    }

    /// The most recent `limit` lines matching the filter, oldest first.
    ///
    /// Filtered before truncating, so "the last 50 errors" means fifty errors
    /// rather than however many of the last fifty lines happened to be errors.
    pub fn query(&self, filter: &LogFilter) -> Vec<LogLine> {
        let lines = self.lines();
        let matched = lines.iter().filter(|line| filter.matches(line));
        // Take from the end, then restore reading order.
        let mut out: Vec<LogLine> = matched.rev().take(filter.limit).cloned().collect();
        out.reverse();
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Only lines newer than this sequence — the incremental poll.
    pub since: Option<u64>,
    /// A severity floor, not an exact match: `warn` means warn and worse.
    pub min_level: Option<&'static str>,
    /// Case-insensitive substring over the message text.
    pub contains: Option<String>,
    pub limit: usize,
}

impl LogFilter {
    fn matches(&self, line: &LogLine) -> bool {
        if let Some(since) = self.since
            && line.seq <= since
        {
            return false;
        }
        if let Some(min) = self.min_level {
            // A line whose level could not be parsed is never hidden by a
            // severity filter. The lines this parser does not recognise are
            // panics and stack traces — exactly what somebody filtering for
            // "warn and above" is looking for.
            if let Some(level) = line.level
                && severity(level) < severity(min)
            {
                return false;
            }
        }
        if let Some(needle) = &self.contains
            && !line.text.to_lowercase().contains(&needle.to_lowercase())
        {
            return false;
        }
        true
    }
}

/// Rank for the severity floor. Only the ordering matters.
pub fn severity(level: &str) -> u8 {
    match level {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        "fatal" => 5,
        _ => 2,
    }
}

/// Map a level name the API was given onto one this module uses, so an
/// unknown value is rejected rather than silently filtering everything out.
pub fn normalize_level(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "trace" | "trc" => Some("trace"),
        "debug" | "dbg" => Some("debug"),
        "info" | "inf" => Some("info"),
        "warn" | "warning" | "wrn" => Some("warn"),
        "error" | "err" => Some("error"),
        "fatal" | "ftl" => Some("fatal"),
        _ => None,
    }
}

/// Parse one nats-server log line:
///
/// ```text
/// [1] 2026/08/30 16:17:44.254017 [INF] Server is ready
///  ^pid ^date      ^time          ^lvl ^message
/// ```
///
/// Anything that does not match is kept whole as the message with no timestamp
/// and no level. Dropping it would lose the panic that follows a fatal, which
/// is the one thing in a log nobody can afford to have filtered out by a
/// parser.
fn parse(raw: &str) -> LogLine {
    let line = raw.trim_end_matches(['\n', '\r']);
    let unparsed = |text: &str| LogLine {
        seq: 0,
        ts: None,
        ts_ms: None,
        level: None,
        text: text.to_string(),
    };

    // The pid prefix, if present.
    let rest = match line.strip_prefix('[') {
        Some(after) => match after.find(']') {
            Some(close) => after[close + 1..].trim_start(),
            None => return unparsed(line),
        },
        None => line,
    };

    let mut parts = rest.splitn(3, ' ');
    let (Some(date), Some(time), Some(tail)) = (parts.next(), parts.next(), parts.next()) else {
        return unparsed(line);
    };
    // A date is the only thing that can be here; if it is not one, this is not
    // a line this parser understands, and guessing would mislabel it.
    if date.len() != 10 || !date.contains('/') {
        return unparsed(line);
    }

    let tail = tail.trim_start();
    let (level, text) = match tail.strip_prefix('[') {
        Some(after) => match after.find(']') {
            Some(close) => (
                level_from_tag(&after[..close]),
                after[close + 1..].trim_start(),
            ),
            None => (None, tail),
        },
        None => (None, tail),
    };

    let ts = format!("{date} {time}");
    LogLine {
        seq: 0,
        ts_ms: local_to_ms(&ts),
        ts: Some(ts),
        level,
        text: text.to_string(),
    }
}

fn level_from_tag(tag: &str) -> Option<&'static str> {
    match tag {
        "TRC" => Some("trace"),
        "DBG" => Some("debug"),
        "INF" => Some("info"),
        "WRN" => Some("warn"),
        "ERR" => Some("error"),
        "FTL" => Some("fatal"),
        _ => None,
    }
}

/// `2026/08/30 16:17:44.254017` to epoch milliseconds.
///
/// nats-server writes its log in the host's local time with no offset, so the
/// only way to place it on a timeline is a timezone assumption. This one is
/// sound rather than convenient: log collection requires reading the file, which
/// requires sharing a filesystem with nats-server, which means sharing its
/// clock and its `TZ`. An ambiguous local time — the hour a DST fall-back
/// repeats — yields `None`, and the page falls back to the raw string, which
/// was never wrong.
fn local_to_ms(raw: &str) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y/%m/%d %H:%M:%S%.f").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|t| t.timestamp_millis())
}

/// Follow the file until shutdown.
///
/// Returns immediately when no file is configured, so the caller can spawn this
/// unconditionally and the "disabled" case is one branch rather than two.
pub async fn run(
    buffer: std::sync::Arc<LogBuffer>,
    prime_bytes: u64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let Some(path) = buffer.file.clone() else {
        tracing::info!("log collection is off (set QUEUE_NATS_LOG_FILE to nats-server's log file)",);
        return;
    };
    tracing::info!(file = %path, "tailing the nats-server log");

    loop {
        let opened = tokio::select! {
            opened = open_at_tail(&path, prime_bytes) => opened,
            _ = shutdown.changed() => return,
        };
        let (file, mut position, inode) = match opened {
            Ok(v) => {
                buffer.set_error(None);
                v
            }
            Err(e) => {
                // Warn once per failure rather than once per retry: a wrong
                // path would otherwise fill the dashboard's own log.
                let message = format!("cannot read {path}: {e}");
                if buffer
                    .error
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_deref()
                    != Some(message.as_str())
                {
                    tracing::warn!(file = %path, error = %e, "cannot read the nats-server log");
                }
                buffer.set_error(Some(message));
                tokio::select! {
                    _ = tokio::time::sleep(RETRY) => continue,
                    _ = shutdown.changed() => return,
                }
            }
        };

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            let read = tokio::select! {
                read = reader.read_line(&mut line) => read,
                _ = shutdown.changed() => return,
            };
            match read {
                Ok(0) => {
                    // End of file: either nothing new, or the file we are
                    // holding is no longer the file at this path.
                    match rotated(&path, inode, position).await {
                        Some(reason) => {
                            tracing::info!(file = %path, reason, "log file rotated; reopening");
                            break;
                        }
                        None => {
                            tokio::select! {
                                _ = tokio::time::sleep(IDLE_POLL) => {}
                                _ = shutdown.changed() => return,
                            }
                        }
                    }
                }
                Ok(n) => {
                    position += n as u64;
                    // A line still being written arrives without its newline;
                    // `read_line` returns it anyway. Publishing it now would
                    // show a half line and then a duplicate. Rewind and wait.
                    if !line.ends_with('\n') {
                        position -= n as u64;
                        if let Err(e) = reader.seek(std::io::SeekFrom::Start(position)).await {
                            tracing::warn!(error = %e, "cannot rewind a partial log line");
                            break;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(IDLE_POLL) => {}
                            _ = shutdown.changed() => return,
                        }
                        continue;
                    }
                    if line.len() > MAX_LINE_BYTES {
                        line.truncate(MAX_LINE_BYTES);
                        line.push_str(" …[truncated]");
                    }
                    buffer.push_raw(&line);
                }
                Err(e) => {
                    tracing::warn!(file = %path, error = %e, "log read failed; reopening");
                    buffer.set_error(Some(format!("read failed: {e}")));
                    break;
                }
            }
        }
    }
}

/// Open the file and position at the tail, priming the buffer with the last
/// `prime_bytes` so the panel is not blank until nats-server next speaks.
async fn open_at_tail(
    path: &str,
    prime_bytes: u64,
) -> std::io::Result<(tokio::fs::File, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut file = tokio::fs::File::open(path).await?;
    let meta = file.metadata().await?;
    let len = meta.len();
    let start = len.saturating_sub(prime_bytes);
    let mut position = start;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
        // The seek almost certainly landed mid-line. Discard to the next
        // newline rather than publishing a fragment.
        let mut reader = BufReader::new(&mut file);
        let mut partial = String::new();
        let skipped = reader.read_line(&mut partial).await?;
        position += skipped as u64;
        file.seek(std::io::SeekFrom::Start(position)).await?;
    }
    Ok((file, position, meta.ino()))
}

/// Whether the file at `path` is no longer the one being read.
async fn rotated(path: &str, inode: u64, position: u64) -> Option<&'static str> {
    use std::os::unix::fs::MetadataExt;
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.ino() != inode => Some("renamed"),
        Ok(meta) if meta.len() < position => Some("truncated"),
        Ok(_) => None,
        // A missing file is a rotation caught mid-flight; reopening retries it.
        Err(_) => Some("missing"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from a running nats-server.
    const REAL_LINE: &str =
        "[1] 2026/08/30 16:17:44.264470 [INF] Listening for client connections on 0.0.0.0:4222";

    #[test]
    fn a_real_nats_log_line_splits_into_a_timestamp_a_level_and_a_message() {
        let line = parse(REAL_LINE);
        assert_eq!(line.ts.as_deref(), Some("2026/08/30 16:17:44.264470"));
        assert_eq!(line.level, Some("info"));
        assert_eq!(
            line.text,
            "Listening for client connections on 0.0.0.0:4222"
        );
    }

    #[test]
    fn every_level_nats_writes_is_recognised() {
        for (tag, expected) in [
            ("TRC", "trace"),
            ("DBG", "debug"),
            ("INF", "info"),
            ("WRN", "warn"),
            ("ERR", "error"),
            ("FTL", "fatal"),
        ] {
            let raw = format!("[7] 2026/08/30 16:17:44.264470 [{tag}] something happened");
            assert_eq!(parse(&raw).level, Some(expected), "tag {tag}");
        }
    }

    /// The line that matters most is the one after a fatal, and it has none of
    /// this format. Dropping it — or filtering it out by severity — would lose
    /// the traceback.
    #[test]
    fn a_line_this_parser_does_not_understand_is_kept_whole() {
        let line = parse("panic: runtime error: index out of range");
        assert_eq!(line.level, None);
        assert_eq!(line.ts, None);
        assert_eq!(line.text, "panic: runtime error: index out of range");
    }

    #[test]
    fn an_unrecognised_line_survives_a_severity_filter() {
        let filter = LogFilter {
            min_level: Some("error"),
            limit: 10,
            ..Default::default()
        };
        assert!(
            filter.matches(&parse("goroutine 1 [running]:")),
            "a line with no parsed level must not be hidden by a severity floor",
        );
        assert!(!filter.matches(&parse(REAL_LINE)), "info is below error");
    }

    #[test]
    fn a_severity_filter_is_a_floor_rather_than_an_exact_match() {
        let filter = LogFilter {
            min_level: Some("warn"),
            limit: 10,
            ..Default::default()
        };
        let warn = parse("[1] 2026/08/30 16:17:44.1 [WRN] slow consumer detected");
        let err = parse("[1] 2026/08/30 16:17:44.1 [ERR] authorization violation");
        let info = parse("[1] 2026/08/30 16:17:44.1 [INF] Server is ready");
        assert!(filter.matches(&warn));
        assert!(filter.matches(&err), "error is above the warn floor");
        assert!(!filter.matches(&info));
    }

    #[test]
    fn the_buffer_drops_the_oldest_line_and_counts_it() {
        let buffer = LogBuffer::new(2, Some("/var/log/nats/nats-server.log".into()));
        for i in 0..5 {
            buffer.push_raw(&format!("[1] 2026/08/30 16:17:44.1 [INF] line {i}"));
        }
        let status = buffer.status();
        assert_eq!(status.lines_held, 2);
        assert_eq!(status.dropped, 3);
        assert_eq!(status.latest_seq, 5);

        let held = buffer.query(&LogFilter {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].text, "line 3", "oldest first, newest kept");
        assert_eq!(held[1].text, "line 4");
    }

    /// The dashboard polls with the sequence it last saw, so an idle server
    /// costs an empty array rather than the whole buffer every few seconds.
    #[test]
    fn since_returns_only_what_arrived_after_the_cursor() {
        let buffer = LogBuffer::new(100, Some("x".into()));
        for i in 0..4 {
            buffer.push_raw(&format!("[1] 2026/08/30 16:17:44.1 [INF] line {i}"));
        }
        let new = buffer.query(&LogFilter {
            since: Some(2),
            limit: 100,
            ..Default::default()
        });
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].text, "line 2");
    }

    /// Filtering before truncating: "the last 2 errors" has to mean two errors,
    /// not however many of the last two lines were errors.
    #[test]
    fn the_limit_applies_after_the_filter_not_before_it() {
        let buffer = LogBuffer::new(100, Some("x".into()));
        buffer.push_raw("[1] 2026/08/30 16:17:44.1 [ERR] first failure");
        buffer.push_raw("[1] 2026/08/30 16:17:44.2 [ERR] second failure");
        for i in 0..20 {
            buffer.push_raw(&format!("[1] 2026/08/30 16:17:45.{i} [INF] chatter {i}"));
        }
        let errors = buffer.query(&LogFilter {
            min_level: Some("error"),
            limit: 2,
            ..Default::default()
        });
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].text, "first failure");
    }

    #[test]
    fn a_substring_filter_ignores_case() {
        let buffer = LogBuffer::new(100, Some("x".into()));
        buffer.push_raw("[1] 2026/08/30 16:17:44.1 [ERR] Authorization Violation for user qfn");
        let hit = buffer.query(&LogFilter {
            contains: Some("authorization".into()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(hit.len(), 1);
    }

    #[test]
    fn an_unknown_level_name_is_rejected_rather_than_filtering_everything_out() {
        assert_eq!(normalize_level("WARN"), Some("warn"));
        assert_eq!(normalize_level("wrn"), Some("warn"));
        assert_eq!(normalize_level("catastrophe"), None);
    }

    #[test]
    fn no_configured_file_reads_as_disabled_rather_than_broken() {
        let status = LogBuffer::new(10, None).status();
        assert!(!status.enabled);
        assert!(status.error.is_none(), "not configured is not an error");
    }
}
