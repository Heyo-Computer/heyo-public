//! Webhook alerts for the basic host metrics the monitoring page shows.
//!
//! An operator configures rules on the monitoring page — one metric (host CPU %,
//! host memory %, disk saturation %, or the local daemon's health check), a
//! threshold, and a webhook URL. A
//! background task ([`spawn_evaluator`]) samples the same host metrics the page
//! renders every [`DashboardConfig::alert_interval`] and POSTs a small JSON body
//! to the URL when a rule *crosses* its threshold — once on the rising edge
//! (`"triggered"`) and once when it falls back (`"resolved"`), never every tick
//! while it stays over. Delivery shells out to `curl` (already relied on for the
//! healthcheck and guest S3 streaming), so no HTTP-client dependency is added.
//!
//! The daemon-health metric is the odd one out: its "value" is not a percentage
//! but the count of *consecutive* failed `GET /health` probes against the local
//! heyvmd (the same signal its watchdog keys on), so a rule with threshold 3
//! means "alert once the daemon has been silent for 3 straight intervals". The
//! webhook payload keeps the `threshold_pct`/`value_pct` keys for wire
//! compatibility; consumers should interpret them by `metric`.
//!
//! Rules persist to a tiny TSV next to the pooler's schema registry, using the
//! same atomic temp-file+rename write as [`crate::store`]. The firing state is
//! runtime-only (starts clear on load), so a pooler restart re-evaluates from
//! scratch and won't replay a stale edge. The paused flag *is* persisted (a
//! pause should survive a redeploy); a paused rule keeps its config but is
//! skipped by the evaluator, and pausing clears any firing state so resuming
//! re-triggers on the next tick if the metric is still over.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

use super::state::DashState;
use super::{host, model};

/// Which host metric a rule watches. Serialized as the kebab labels below so the
/// on-disk file and webhook payloads are self-describing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    HostCpu,
    HostMemory,
    Disk,
    /// Consecutive failed `GET /health` probes against the local heyvmd — the
    /// same signal the supervisor watchdog keys on, so a rule here can warn
    /// (webhook) well before the watchdog escalates to a VM-killing restart.
    HeyvmdHealth,
}

impl Metric {
    /// Stable machine label (on-disk + webhook payload).
    pub fn slug(self) -> &'static str {
        match self {
            Metric::HostCpu => "host-cpu",
            Metric::HostMemory => "host-memory",
            Metric::Disk => "disk",
            Metric::HeyvmdHealth => "heyvmd-health",
        }
    }

    /// Human label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Metric::HostCpu => "host CPU",
            Metric::HostMemory => "host memory",
            Metric::Disk => "disk saturation",
            Metric::HeyvmdHealth => "daemon health check",
        }
    }

    /// Unit suffix for rendering a threshold ("≥ 90%" vs "≥ 3 failed checks").
    pub fn unit(self) -> &'static str {
        match self {
            Metric::HeyvmdHealth => " failed checks",
            _ => "%",
        }
    }

    pub fn parse(s: &str) -> Option<Metric> {
        match s {
            "host-cpu" => Some(Metric::HostCpu),
            "host-memory" => Some(Metric::HostMemory),
            "disk" => Some(Metric::Disk),
            "heyvmd-health" => Some(Metric::HeyvmdHealth),
            _ => None,
        }
    }

    /// Every metric, for populating the add-rule dropdown.
    pub fn all() -> [Metric; 4] {
        [
            Metric::HostCpu,
            Metric::HostMemory,
            Metric::Disk,
            Metric::HeyvmdHealth,
        ]
    }
}

/// A configured alert rule plus its (runtime-only) firing state.
struct Rule {
    id: String,
    metric: Metric,
    /// Fire when the metric's value is `>=` this — a percentage (0–100) for the
    /// host metrics, a consecutive-failure count for [`Metric::HeyvmdHealth`].
    threshold_pct: f64,
    webhook_url: String,
    /// True while the metric is currently over the threshold — the edge-detector
    /// that stops us re-POSTing every interval. Not persisted.
    firing: bool,
    /// A paused rule keeps its config but is skipped by the evaluator.
    /// Persisted (a pause should survive a redeploy).
    paused: bool,
}

/// A read-only view of a rule for rendering.
pub struct RuleView {
    pub id: String,
    pub metric: Metric,
    pub threshold_pct: f64,
    pub webhook_url: String,
    pub firing: bool,
    pub paused: bool,
}

/// A webhook to send: the URL and a pre-rendered JSON body. Produced by
/// [`AlertStore::evaluate`] on an edge, consumed by [`deliver`].
pub struct Delivery {
    pub url: String,
    pub body: String,
}

/// The current host-metric sample the evaluator compares rules against. Each is
/// a percentage in `[0, 100]`, `None` when that metric couldn't be read this
/// tick (so a rule on it is left untouched rather than falsely resolved).
#[derive(Default)]
pub struct Sample {
    pub host_cpu_pct: Option<f64>,
    pub host_mem_pct: Option<f64>,
    /// Highest saturation across the host's real filesystems.
    pub max_disk_pct: Option<f64>,
    /// Mount point of the fullest filesystem, for the disk payload's `detail`.
    pub worst_mount: Option<String>,
    /// Consecutive failed heyvmd `/health` probes; 0 while healthy. The counter
    /// lives in the evaluator loop (it must survive across ticks).
    pub heyvmd_fail_count: Option<f64>,
    /// The latest probe's error, for the health payload's `detail`.
    pub heyvmd_error: Option<String>,
}

/// Max rules — a sanity cap so a runaway form can't grow the file unboundedly.
const MAX_RULES: usize = 64;

pub struct AlertStore {
    path: PathBuf,
    rules: Mutex<Vec<Rule>>,
}

impl AlertStore {
    /// Load rules from `path`; a missing/corrupt file starts empty (never fatal —
    /// alerts are an optional convenience, not on the data path).
    pub fn load(path: PathBuf) -> Self {
        let rules = match std::fs::read_to_string(&path) {
            Ok(s) => parse(&s),
            Err(_) => Vec::new(),
        };
        if !rules.is_empty() {
            info!("loaded {} alert rule(s) from {}", rules.len(), path.display());
        }
        AlertStore {
            path,
            rules: Mutex::new(rules),
        }
    }

    /// All rules, for rendering the monitoring page.
    pub fn list(&self) -> Vec<RuleView> {
        self.rules
            .lock()
            .unwrap()
            .iter()
            .map(|r| RuleView {
                id: r.id.clone(),
                metric: r.metric,
                threshold_pct: r.threshold_pct,
                webhook_url: r.webhook_url.clone(),
                firing: r.firing,
                paused: r.paused,
            })
            .collect()
    }

    /// Add a rule and persist. Validates the metric, threshold range, and that
    /// the URL is a plausible http(s) webhook; returns a user-facing error string
    /// on rejection (surfaced in the redirect banner).
    pub fn add(&self, metric: &str, threshold_pct: f64, webhook_url: &str) -> Result<()> {
        let (metric, url) = validate(metric, threshold_pct, webhook_url)?;
        let snapshot = {
            let mut rules = self.rules.lock().unwrap();
            if rules.len() >= MAX_RULES {
                bail!("too many alert rules (max {MAX_RULES})");
            }
            rules.push(Rule {
                id: new_id(),
                metric,
                threshold_pct,
                webhook_url: url,
                firing: false,
                paused: false,
            });
            serialize(&rules)
        };
        write_atomic(&self.path, &snapshot)
            .with_context(|| format!("persisting alerts to {}", self.path.display()))?;
        Ok(())
    }

    /// Replace the rule `id`'s metric/threshold/URL and persist. Same validation
    /// as [`Self::add`]. Firing state resets (the rule's meaning changed, so the
    /// next tick re-evaluates from a clean edge); the paused flag is kept.
    pub fn update(&self, id: &str, metric: &str, threshold_pct: f64, webhook_url: &str) -> Result<()> {
        let (metric, url) = validate(metric, threshold_pct, webhook_url)?;
        let snapshot = {
            let mut rules = self.rules.lock().unwrap();
            let Some(r) = rules.iter_mut().find(|r| r.id == id) else {
                bail!("no such alert rule");
            };
            r.metric = metric;
            r.threshold_pct = threshold_pct;
            r.webhook_url = url;
            r.firing = false;
            serialize(&rules)
        };
        write_atomic(&self.path, &snapshot)
            .with_context(|| format!("persisting alerts to {}", self.path.display()))?;
        Ok(())
    }

    /// Pause or resume the rule `id`, persisting the flag. Pausing clears any
    /// firing state — no "resolved" webhook will be sent for a fire that was
    /// alive at pause time, and resuming re-triggers on the next tick if the
    /// metric is still over. Returns whether the rule exists.
    pub fn set_paused(&self, id: &str, paused: bool) -> bool {
        let snapshot = {
            let mut rules = self.rules.lock().unwrap();
            let Some(r) = rules.iter_mut().find(|r| r.id == id) else {
                return false;
            };
            r.paused = paused;
            if paused {
                r.firing = false;
            }
            serialize(&rules)
        };
        if let Err(e) = write_atomic(&self.path, &snapshot) {
            warn!("failed to persist alerts to {}: {e:#}", self.path.display());
        }
        true
    }

    /// Remove the rule with `id`, persisting if it existed. Returns whether a rule
    /// was removed.
    pub fn remove(&self, id: &str) -> bool {
        let snapshot = {
            let mut rules = self.rules.lock().unwrap();
            let before = rules.len();
            rules.retain(|r| r.id != id);
            if rules.len() == before {
                return false;
            }
            serialize(&rules)
        };
        if let Err(e) = write_atomic(&self.path, &snapshot) {
            warn!("failed to persist alerts to {}: {e:#}", self.path.display());
        }
        true
    }

    /// Compare every rule against `sample`, flip firing state on a crossing, and
    /// return the webhooks to send for this tick (rising and falling edges only).
    /// A metric that's `None` this tick leaves its rules' state alone.
    pub fn evaluate(&self, sample: &Sample) -> Vec<Delivery> {
        let mut out = Vec::new();
        let mut rules = self.rules.lock().unwrap();
        for r in rules.iter_mut() {
            if r.paused {
                continue;
            }
            let (value, detail) = match r.metric {
                Metric::HostCpu => (sample.host_cpu_pct, None),
                Metric::HostMemory => (sample.host_mem_pct, None),
                Metric::Disk => (sample.max_disk_pct, sample.worst_mount.as_deref()),
                Metric::HeyvmdHealth => {
                    (sample.heyvmd_fail_count, sample.heyvmd_error.as_deref())
                }
            };
            let Some(value) = value else { continue };
            let over = value >= r.threshold_pct;
            let state = if over && !r.firing {
                r.firing = true;
                "triggered"
            } else if !over && r.firing {
                r.firing = false;
                "resolved"
            } else {
                continue;
            };
            out.push(Delivery {
                url: r.webhook_url.clone(),
                body: payload(r, state, value, detail),
            });
        }
        out
    }
}

/// Sample the host metrics an evaluation compares against — the same sources the
/// monitoring page renders (heyvmd's usage snapshot for CPU/memory, host `df`
/// for disk). Best-effort per metric.
async fn sample(st: &DashState) -> Sample {
    let (host_usage, disks) = tokio::join!(model::fetch_host_usage(st), host::host_disks());
    let host_cpu_pct = host_usage
        .as_ref()
        .and_then(|h| h.cpu_percent)
        .map(|c| c as f64);
    let host_mem_pct = host_usage.as_ref().and_then(|h| {
        match (h.memory_used_bytes, h.memory_total_bytes) {
            (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
            _ => None,
        }
    });
    let (max_disk_pct, worst_mount) = match disks {
        Ok(disks) => disks
            .iter()
            .max_by(|a, b| a.saturation().total_cmp(&b.saturation()))
            .map(|d| (Some(d.saturation() * 100.0), Some(d.mount.clone())))
            .unwrap_or((None, None)),
        Err(e) => {
            warn!("alert evaluator: reading host disks failed: {e:#}");
            (None, None)
        }
    };
    Sample {
        host_cpu_pct,
        host_mem_pct,
        max_disk_pct,
        worst_mount,
        // Filled in by the evaluator loop, which owns the cross-tick counter.
        heyvmd_fail_count: None,
        heyvmd_error: None,
    }
}

/// Probe the local daemon's `/health` — the same lock-free endpoint the
/// supervisor watchdog polls, with a similar per-probe bound so a wedged daemon
/// costs one timeout per tick, not a hang. `Err` carries a short reason for the
/// webhook payload's `detail`.
async fn probe_heyvmd() -> std::result::Result<(), String> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    let url = format!("{}/health", heyo_sdk::DEFAULT_LOCAL_BASE_URL);
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return Err(format!("building probe client: {e}")),
    };
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!("HTTP {}", resp.status())),
        Err(e) => Err(e.to_string()),
    }
}

/// Spawn the background evaluator: every `interval`, sample host metrics, flip
/// rule edges, and deliver any resulting webhooks. Cheap and self-healing — a
/// failed sample or delivery is logged and the loop continues. The
/// consecutive-failure counter for the daemon-health metric lives here: it must
/// survive across ticks, and counting even while no rule watches it means a
/// rule added mid-outage fires on the very next tick.
pub fn spawn_evaluator(st: DashState, interval: Duration) {
    tokio::spawn(async move {
        info!(
            "alert evaluator running every {}s ({} rule(s) loaded)",
            interval.as_secs(),
            st.alerts.list().len()
        );
        let mut heyvmd_fails: u64 = 0;
        loop {
            tokio::time::sleep(interval).await;
            let mut sample = sample(&st).await;
            match probe_heyvmd().await {
                Ok(()) => heyvmd_fails = 0,
                Err(e) => {
                    heyvmd_fails += 1;
                    warn!("alert evaluator: heyvmd /health probe failed ({heyvmd_fails} consecutive): {e}");
                    sample.heyvmd_error = Some(e);
                }
            }
            sample.heyvmd_fail_count = Some(heyvmd_fails as f64);
            for d in st.alerts.evaluate(&sample) {
                deliver(&d).await;
            }
        }
    });
}

/// POST a webhook body via `curl`, bounded so a hung endpoint can't wedge the
/// evaluator. The body is fed on stdin (`--data-binary @-`) so an arbitrary URL
/// or payload never lands on the argv. Best-effort: failures are logged.
async fn deliver(d: &Delivery) {
    const DELIVER_TIMEOUT: Duration = Duration::from_secs(10);
    let spawn = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "--data-binary",
            "@-",
            &d.url,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            warn!("alert webhook: spawning curl failed: {e:#}");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(d.body.as_bytes()).await;
        // Drop closes the pipe so curl sees EOF and sends.
    }
    match tokio::time::timeout(DELIVER_TIMEOUT + Duration::from_secs(1), child.wait_with_output())
        .await
    {
        Ok(Ok(out)) if out.status.success() => {}
        Ok(Ok(out)) => warn!(
            "alert webhook to {} failed: curl {} {}",
            d.url,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Ok(Err(e)) => warn!("alert webhook to {}: curl error: {e:#}", d.url),
        Err(_) => warn!("alert webhook to {} timed out", d.url),
    }
}

/// Build the JSON body for a fired rule. Values are numbers and known labels
/// except the disk `detail` (a mount point) and the URL is not included, so the
/// only free-form string is `detail`, which [`json_str`] escapes.
fn payload(r: &Rule, state: &str, value: f64, detail: Option<&str>) -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_default();
    let mut s = String::from("{");
    s.push_str(&format!(r#""source":"pg-vm-pool","host":{},"#, json_str(&host)));
    s.push_str(&format!(r#""rule_id":{},"#, json_str(&r.id)));
    s.push_str(&format!(r#""metric":"{}","#, r.metric.slug()));
    s.push_str(&format!(r#""state":"{state}","#));
    s.push_str(&format!(r#""threshold_pct":{},"#, num(r.threshold_pct)));
    s.push_str(&format!(r#""value_pct":{}"#, num(value)));
    if let Some(d) = detail {
        s.push_str(&format!(r#","detail":{}"#, json_str(d)));
    }
    s.push('}');
    s
}

/// Format a percentage compactly without a trailing `.0` explosion.
fn num(v: f64) -> String {
    format!("{:.1}", v)
}

/// Minimal JSON string encoder for the few free-form fields (hostname, mount,
/// rule id). Escapes the characters JSON requires; control chars become `\uXXXX`.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A short, unique-enough rule id from the current time in nanoseconds (base-36).
/// Two adds within the same nanosecond via a web form is not a real scenario.
fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    to_base36(nanos)
}

fn to_base36(mut n: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// Shared add/update validation: the metric must be known, the threshold in
/// range, and the URL a plausible single-line http(s) webhook. Returns the
/// parsed metric and trimmed URL.
fn validate(metric: &str, threshold_pct: f64, webhook_url: &str) -> Result<(Metric, String)> {
    let metric = Metric::parse(metric).context("unknown metric")?;
    if !(0.0..=100.0).contains(&threshold_pct) {
        bail!("threshold must be between 0 and 100");
    }
    let url = webhook_url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("webhook URL must start with http:// or https://");
    }
    // URLs are single-line by nature; refuse control chars so a value can't
    // break the TSV or smuggle headers into the curl invocation.
    if url.chars().any(|c| c.is_control()) {
        bail!("webhook URL must not contain control characters");
    }
    Ok((metric, url.to_string()))
}

/// Parse the alert TSV: one `id\tmetric\tthreshold\turl[\tstatus]` line per
/// rule, where the optional fifth column is `paused` (anything else — including
/// its absence in files written by older builds — means active). Unknown
/// metrics, bad thresholds, or short lines are skipped (a corrupt line only
/// drops that rule).
fn parse(s: &str) -> Vec<Rule> {
    s.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let id = f.next()?;
            let metric = Metric::parse(f.next()?)?;
            let threshold_pct: f64 = f.next()?.parse().ok()?;
            let url = f.next()?;
            let paused = f.next() == Some("paused");
            if id.is_empty() || url.is_empty() {
                return None;
            }
            Some(Rule {
                id: id.to_string(),
                metric,
                threshold_pct,
                webhook_url: url.to_string(),
                firing: false,
                paused,
            })
        })
        .collect()
}

fn serialize(rules: &[Rule]) -> String {
    let mut out = String::new();
    for r in rules {
        out.push_str(&r.id);
        out.push('\t');
        out.push_str(r.metric.slug());
        out.push('\t');
        out.push_str(&num(r.threshold_pct));
        out.push('\t');
        out.push_str(&r.webhook_url);
        out.push('\t');
        // A word, not a bool, so the column is self-describing in the file —
        // and an older build's parser (4 columns) simply ignores it.
        out.push_str(if r.paused { "paused" } else { "active" });
        out.push('\n');
    }
    out
}

/// Atomic temp-file + rename write, matching [`crate::store`] so a crash
/// mid-write can't corrupt the rules file.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(metric: Metric, threshold: f64) -> Rule {
        Rule {
            id: "r1".into(),
            metric,
            threshold_pct: threshold,
            webhook_url: "https://example.com/hook".into(),
            firing: false,
            paused: false,
        }
    }

    #[test]
    fn parse_round_trips() {
        let rules = vec![
            rule(Metric::HostCpu, 90.0),
            Rule {
                id: "r2".into(),
                metric: Metric::Disk,
                threshold_pct: 85.5,
                webhook_url: "http://localhost:9000/a".into(),
                firing: true, // not persisted
                paused: true, // persisted
            },
        ];
        let reparsed = parse(&serialize(&rules));
        assert_eq!(reparsed.len(), 2);
        assert_eq!(reparsed[0].metric, Metric::HostCpu);
        assert_eq!(reparsed[0].threshold_pct, 90.0);
        assert!(!reparsed[0].paused);
        assert_eq!(reparsed[1].metric, Metric::Disk);
        assert_eq!(reparsed[1].webhook_url, "http://localhost:9000/a");
        // Firing state is runtime-only; a reloaded rule starts clear. The
        // paused flag survives the round trip.
        assert!(!reparsed[1].firing);
        assert!(reparsed[1].paused);
    }

    #[test]
    fn parse_accepts_legacy_four_column_lines() {
        // A file written before the paused column existed: rule loads, active.
        let rules = parse("r1\thost-cpu\t90\thttps://ok\n");
        assert_eq!(rules.len(), 1);
        assert!(!rules[0].paused);
    }

    #[test]
    fn parse_skips_corrupt_lines() {
        let rules = parse("r1\thost-cpu\t90\thttps://ok\ngarbage\nr2\tbogus-metric\t5\thttps://x\nr3\thost-memory\tNaNpct\thttps://y\n");
        // Only the first line is a valid, known-metric, numeric-threshold rule.
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "r1");
    }

    #[test]
    fn evaluate_edge_triggers_once_then_resolves() {
        let store = AlertStore {
            path: PathBuf::from("/nonexistent/should-not-write"),
            rules: Mutex::new(vec![rule(Metric::HostCpu, 90.0)]),
        };
        // Below threshold: nothing.
        let below = Sample {
            host_cpu_pct: Some(50.0),
            ..Default::default()
        };
        assert!(store.evaluate(&below).is_empty());

        // Crosses up: one "triggered".
        let over = Sample {
            host_cpu_pct: Some(95.0),
            ..Default::default()
        };
        let ev = store.evaluate(&over);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].body.contains(r#""state":"triggered""#));
        assert!(ev[0].body.contains(r#""metric":"host-cpu""#));

        // Still over: no repeat.
        assert!(store.evaluate(&over).is_empty());

        // Falls back: one "resolved".
        let ev = store.evaluate(&below);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].body.contains(r#""state":"resolved""#));

        // A None sample leaves state alone (no spurious resolve).
        store.evaluate(&over); // re-arm
        let unknown = Sample::default();
        assert!(store.evaluate(&unknown).is_empty());
    }

    #[test]
    fn disk_uses_worst_mount_in_detail() {
        let store = AlertStore {
            path: PathBuf::from("/nonexistent"),
            rules: Mutex::new(vec![rule(Metric::Disk, 80.0)]),
        };
        let s = Sample {
            max_disk_pct: Some(91.0),
            worst_mount: Some("/data".into()),
            ..Default::default()
        };
        let ev = store.evaluate(&s);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].body.contains(r#""detail":"/data""#));
        assert!(ev[0].body.contains(r#""value_pct":91.0"#));
    }

    #[test]
    fn json_str_escapes() {
        assert_eq!(json_str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_str("tab\there"), r#""tab\there""#);
    }

    #[test]
    fn paused_rule_is_skipped_and_resume_refires() {
        let store = AlertStore {
            path: PathBuf::from("/nonexistent/should-not-write"),
            rules: Mutex::new(vec![rule(Metric::HostCpu, 90.0)]),
        };
        let over = Sample {
            host_cpu_pct: Some(95.0),
            ..Default::default()
        };
        // Fire it, then pause mid-fire: firing state clears, nothing delivers.
        assert_eq!(store.evaluate(&over).len(), 1);
        assert!(store.set_paused("r1", true));
        assert!(store.evaluate(&over).is_empty());
        assert!(!store.list()[0].firing, "pausing clears firing state");
        // Resume while still over: a fresh rising edge, one "triggered".
        assert!(store.set_paused("r1", false));
        let ev = store.evaluate(&over);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].body.contains(r#""state":"triggered""#));
        // Unknown id: reported, not invented.
        assert!(!store.set_paused("nope", true));
    }

    #[test]
    fn update_replaces_fields_and_validates() {
        let store = AlertStore {
            path: PathBuf::from("/nonexistent/should-not-write"),
            rules: Mutex::new(vec![rule(Metric::HostCpu, 90.0)]),
        };
        assert!(store.update("nope", "host-cpu", 50.0, "https://ok").is_err());
        assert!(store.update("r1", "bogus", 50.0, "https://ok").is_err());
        assert!(store.update("r1", "host-cpu", 150.0, "https://ok").is_err());
        // A write to /nonexistent fails at persist — but only after the
        // in-memory rule is updated, which is what we assert on here.
        let _ = store.update("r1", "disk", 80.0, "https://new.example/hook");
        let v = &store.list()[0];
        assert_eq!(v.metric, Metric::Disk);
        assert_eq!(v.threshold_pct, 80.0);
        assert_eq!(v.webhook_url, "https://new.example/hook");
    }

    #[test]
    fn heyvmd_health_fires_on_consecutive_failures() {
        let store = AlertStore {
            path: PathBuf::from("/nonexistent"),
            rules: Mutex::new(vec![rule(Metric::HeyvmdHealth, 3.0)]),
        };
        let fails = |n: f64, err: Option<&str>| Sample {
            heyvmd_fail_count: Some(n),
            heyvmd_error: err.map(str::to_string),
            ..Default::default()
        };
        // 1–2 consecutive failures: under threshold, quiet.
        assert!(store.evaluate(&fails(1.0, Some("timeout"))).is_empty());
        assert!(store.evaluate(&fails(2.0, Some("timeout"))).is_empty());
        // Third: fires once, carrying the probe error as detail.
        let ev = store.evaluate(&fails(3.0, Some("connection refused")));
        assert_eq!(ev.len(), 1);
        assert!(ev[0].body.contains(r#""metric":"heyvmd-health""#));
        assert!(ev[0].body.contains(r#""detail":"connection refused""#));
        // Still failing: no repeat.
        assert!(store.evaluate(&fails(4.0, Some("connection refused"))).is_empty());
        // Recovery: one "resolved".
        let ev = store.evaluate(&fails(0.0, None));
        assert_eq!(ev.len(), 1);
        assert!(ev[0].body.contains(r#""state":"resolved""#));
    }

    #[test]
    fn add_rejects_bad_input() {
        let dir = std::env::temp_dir().join(format!("pgvmpool-alerts-{}", std::process::id()));
        let path = dir.join("alerts.tsv");
        let _ = std::fs::remove_file(&path);
        let store = AlertStore::load(path.clone());
        assert!(store.add("host-cpu", 150.0, "https://ok").is_err()); // threshold range
        assert!(store.add("bogus", 50.0, "https://ok").is_err()); // metric
        assert!(store.add("host-cpu", 50.0, "ftp://nope").is_err()); // scheme
        assert!(store.add("host-cpu", 50.0, "https://ok").is_ok());
        assert_eq!(store.list().len(), 1);
        // Survives reload.
        assert_eq!(AlertStore::load(path.clone()).list().len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
