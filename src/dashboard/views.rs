//! Server-side-rendered HTML (maud). All values are interpolated through maud,
//! which HTML-escapes by default; no secrets are ever rendered.

use heyo_sdk::SandboxStatus;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::registry::{DbStats, GuestStats};

use super::alerts::{Metric, RuleView};
use super::handlers::Banner;
use super::history::Sample;
use super::host::HostDisk;
use super::model::{self, HostUsage, SandboxPage, VmRow};
use super::state::DashState;

const SIZE_CLASSES: [&str; 5] = ["micro", "mini", "small", "medium", "large"];

/// Shared page chrome: `<head>` with inline CSS, a nav bar, and the body.
fn shell(title: &str, body: Markup) -> Markup {
    shell_with_head(title, html! {}, body)
}

/// Like [`shell`] but injects `extra_head` into `<head>` (e.g. a `meta
/// http-equiv="refresh"` for the auto-refreshing log pages).
fn shell_with_head(title: &str, extra_head: Markup, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "pg-vm-pool · " (title) }
                (extra_head)
                style { (STYLE) }
            }
            body {
                header.nav {
                    a.brand href="/" { "pg-vm-pool" }
                    nav {
                        a href="/" { "Databases" }
                        a href="/monitoring" { "monitoring" }
                        a href="/events" { "events" }
                        a href="/logs/pooler" { "pooler log" }
                        a href="/logs/heyvmd" { "heyvmd log" }
                    }
                }
                main { (body) }
            }
        }
    }
}

/// Render the `?msg=`/`?err=` one-shot banner, if present.
fn banner(b: &Banner) -> Markup {
    html! {
        @if let Some(m) = &b.msg {
            div.banner.ok { (m) }
        }
        @if let Some(e) = &b.err {
            div.banner.err { "error: " (e) }
        }
    }
}

/// The main Databases view: a paged, searchable list of every daemon sandbox.
pub fn databases_page(st: &DashState, p: &SandboxPage) -> Markup {
    let first = (p.page - 1) * p.per + 1;
    let last = first + p.rows.len().saturating_sub(1);
    shell(
        "Databases",
        html! {
            div.pagehead {
                h1 { "Databases" }
                div.pagehead-actions {
                    a.button-link href=(list_href(&p.q, &p.state, p.page, p.per)) { "↻ refresh" }
                    form method="post" action="/stop-idle" class="inline-form" {
                        button.stop
                            title="Stop every running VM that has zero live client sessions (keep-alive schemas are skipped). Runs in the background; VMs restart on the next client connection."
                            onclick="return confirm('Stop ALL running VMs with no live sessions? Keep-alive schemas are skipped; stopped VMs restart automatically on the next client connection.')"
                            { "stop idle VMs" }
                    }
                }
            }
            form.search method="get" action="/" {
                input type="search" name="q" value=(p.q)
                    placeholder="filter by id, name, schema, status, image, or guest ip";
                @if p.state != model::DEFAULT_STATE {
                    input type="hidden" name="state" value=(p.state);
                }
                @if p.per != model::DEFAULT_PER {
                    input type="hidden" name="per" value=(p.per);
                }
                button type="submit" { "search" }
                @if !p.q.is_empty() { a.button-link href=(list_href("", &p.state, 1, p.per)) { "clear" } }
            }
            (state_pills(p))
            div.summary {
                @if p.matched == 0 {
                    span { "no sandboxes match" }
                } @else {
                    span { "showing " b { (first) "–" (last) } " of " b { (p.matched) } }
                }
                span.dim { (p.total) " total" }
                @if st.cfg.basic_auth.is_none() {
                    span.warn { "auth: OFF" }
                }
            }
            @if p.matched > 0 {
                (vm_table(&p.rows, st.registry.archive_enabled(),
                    &list_href(&p.q, &p.state, p.page, p.per)))
                (pager(p))
            }
        },
    )
}

/// Per-hour event series for the monitoring page's activity charts, each as
/// oldest-first `(hour_start_unix, count)` buckets from `crate::events`.
pub struct ActivitySeries {
    pub restores_s3: Vec<(u64, u32)>,
    pub restores_local: Vec<(u64, u32)>,
    pub vms_created: Vec<(u64, u32)>,
}

/// The monitoring view: whole-host CPU/memory/disk saturation plus pooler-fleet
/// aggregates rolled up from the VM inventory.
pub fn monitoring_page(
    st: &DashState,
    rows: &[VmRow],
    host: Option<&HostUsage>,
    disks: &[HostDisk],
    history: &[Sample],
    activity: &ActivitySeries,
    b: &Banner,
) -> Markup {
    let running: Vec<&VmRow> = rows.iter().filter(|r| r.is_running()).collect();
    let sessions: usize = rows.iter().filter_map(|r| r.live_sessions).sum();
    let warm = rows.iter().filter(|r| r.live_sessions.is_some()).count();
    let queueing = rows
        .iter()
        .filter(|r| matches!(r.client_slots, Some((0, _))))
        .count();
    let alloc_vcpu: u64 = running.iter().filter_map(|r| r.cpus).map(u64::from).sum();
    let alloc_mem: u64 = running.iter().filter_map(|r| r.memory_bytes).sum();
    // Sum of per-VM guest CPU (heyvmd's `top`-convention sample: 100% = one
    // core), i.e. how many cores' worth of work the fleet's guests are doing.
    let guest_cpu: f32 = running.iter().filter_map(|r| r.cpu_percent).sum();
    let archived = rows.iter().filter(|r| r.offload == Some("archived")).count();
    let frozen = rows.iter().filter(|r| r.offload == Some("frozen")).count();
    let compacted = rows.iter().filter(|r| r.offload == Some("compacted")).count();

    shell(
        "Monitoring",
        html! {
            div.pagehead {
                h1 { "Monitoring" }
                div.pagehead-actions {
                    a.button-link href="/monitoring" { "↻ refresh" }
                    @if st.registry.archive_enabled() {
                        form method="post" action="/monitoring/sweep" class="inline-form" {
                            button.reap type="submit"
                                title="Run an S3 eviction sweep now: archive every long-idle schema and free its disk, instead of waiting for the periodic timer." {
                                "sweep idle → S3 now"
                            }
                        }
                    }
                    @if st.registry.reclaim_enabled() {
                        form method="post" action="/monitoring/reclaim" class="inline-form" {
                            button.reap type="submit"
                                title="Offline-trim every stopped VM's data disk now, returning stranded free space to the host. Running VMs are skipped." {
                                "reclaim disk slack now"
                            }
                        }
                    }
                    form method="post" action="/monitoring/purge" class="inline-form" {
                        button.stop type="submit"
                            title="Permanently delete VMs that are pure waste: leftover VMs of schemas already frozen/archived (their data is durably offloaded) and unclaimed warm spares (empty by construction). Double confirmation required."
                            onclick="return confirm('Purge deletes VMs PERMANENTLY: leftovers of already-frozen/archived schemas (data is safe in their dumps) and unclaimed spares (empty). Continue?') && prompt('Type PURGE to confirm') === 'PURGE'"
                            { "purge waste VMs" }
                    }
                }
            }
            (banner(b))
            @if st.cfg.basic_auth.is_none() {
                div.summary { span.warn { "auth: OFF" } }
            }

            h2 { "host" }
            @match host {
                Some(h) => {
                    div.metrics {
                        (host_cpu_metric(h))
                        (host_mem_metric(h))
                    }
                }
                None => {
                    p.note {
                        "Host CPU/memory unavailable — heyvmd's usage poller has not "
                        "published a sample yet, or this daemon predates "
                        code { "/system/usage" } "."
                    }
                }
            }

            h3.sub-head { "disk saturation" }
            @if disks.is_empty() {
                p.note { "No host filesystems reported (" code { "df" } " unavailable or timed out)." }
            } @else {
                div.metrics {
                    @for d in disks {
                        (disk_metric(d))
                    }
                }
            }

            h2 { "pooler fleet" }
            div.stats {
                (stat("running VMs", &running.len().to_string(), Some(&format!("{} total", rows.len()))))
                (stat("warm in pooler", &warm.to_string(),
                    if queueing > 0 { Some("clients queueing") } else { None }))
                (stat("live sessions", &sessions.to_string(), None))
                (stat("allocated vCPU", &alloc_vcpu.to_string(), Some("across running VMs")))
                (stat("allocated RAM", &human_bytes(alloc_mem), Some("across running VMs")))
                (stat("guest CPU", &format!("{guest_cpu:.0}%"), Some("cores busy, top-convention")))
                @if st.registry.archive_enabled() {
                    (stat("compacted (local)", &compacted.to_string(), None))
                    (stat("frozen (local)", &frozen.to_string(), None))
                    (stat("archived (S3)", &archived.to_string(), None))
                }
            }
            p.note {
                "Whole-host CPU and memory come from heyvmd's sampler (" code { "/system/usage" }
                "); disk saturation is read on the host with " code { "df" }
                ". Fleet figures are rolled up from the same inventory the "
                a href="/" { "Databases" } " view shows — no guest access."
            }

            h3.sub-head { "live VMs over time" }
            (vm_history_chart(history, running.len()))

            h3.sub-head { "restores from S3" }
            (hourly_bar_chart(&activity.restores_s3, "restore"))

            h3.sub-head { "restores from local dumps" }
            (hourly_bar_chart(&activity.restores_local, "restore"))

            h3.sub-head { "VMs created" }
            (hourly_bar_chart(&activity.vms_created, "VM"))

            (alerts_section(st))
        },
    )
}

/// A self-contained inline-SVG line chart of the live-VM count over the sampler's
/// retained history — no client JS, no external libs, so it renders under the
/// dashboard's strict-by-default CSP. `current` is the running count right now,
/// shown in the caption. The SVG uses a fixed internal coordinate system and is
/// scaled to the container by CSS, so it stays crisp at any width.
fn vm_history_chart(samples: &[Sample], current: usize) -> Markup {
    // Internal coordinate system (CSS scales the whole thing to fit).
    const W: f64 = 640.0;
    const H: f64 = 160.0;
    const PAD_L: f64 = 32.0;
    const PAD_R: f64 = 12.0;
    const PAD_T: f64 = 12.0;
    const PAD_B: f64 = 20.0;
    let plot_w = W - PAD_L - PAD_R;
    let plot_h = H - PAD_T - PAD_B;
    let baseline = PAD_T + plot_h;

    if samples.is_empty() {
        return html! {
            div.chart-wrap {
                p.note.chart-empty {
                    "Collecting live-VM history — the first sample lands within a minute."
                }
            }
        };
    }

    let n = samples.len();
    let max_live = samples.iter().map(|s| s.live).max().unwrap_or(0);
    let axis_max = f64::from(max_live.max(1));

    let x = |i: usize| -> f64 {
        if n > 1 {
            PAD_L + (i as f64 / (n - 1) as f64) * plot_w
        } else {
            PAD_L + plot_w
        }
    };
    let y = |v: u32| -> f64 { PAD_T + (1.0 - f64::from(v) / axis_max) * plot_h };

    let line_pts: String = samples
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{:.1},{:.1}", x(i), y(s.live)))
        .collect::<Vec<_>>()
        .join(" ");
    // Close the area down to the baseline under the first and last points.
    let area_pts = format!(
        "{:.1},{:.1} {line_pts} {:.1},{:.1}",
        x(0),
        baseline,
        x(n - 1),
        baseline
    );

    let last = samples[n - 1];
    let (lx, ly) = (x(n - 1), y(last.live));
    let span = last.t.saturating_sub(samples[0].t);

    // Y gridline values: 0, max, and a midpoint when the range is tall enough.
    let ticks: Vec<u32> = if max_live >= 2 {
        vec![0, max_live / 2, max_live]
    } else {
        vec![0, 1]
    };

    html! {
        div.chart-wrap {
            svg.chart-svg viewBox="0 0 640 160" role="img"
                aria-label=(format!("Live VM count over the last {}", fmt_span(span))) {
                // Horizontal gridlines + y-axis labels.
                @for t in &ticks {
                    @let gy = y(*t);
                    // Empty `{}` (not `;`) so maud emits a closing tag: an SVG
                    // `<line>` is not an HTML void element, and a bare `<line>`
                    // would swallow every following shape as an unrendered child.
                    line.chart-grid x1=(fmt(PAD_L)) y1=(fmt(gy)) x2=(fmt(W - PAD_R)) y2=(fmt(gy)) {}
                    text.chart-ylabel x=(fmt(PAD_L - 5.0)) y=(fmt(gy + 3.0)) text-anchor="end" {
                        (t)
                    }
                }
                // Filled area under the line, then the line itself.
                polygon.chart-area points=(area_pts) {}
                polyline.chart-line points=(line_pts) {}
                // Marker + value at the latest sample.
                circle.chart-dot cx=(fmt(lx)) cy=(fmt(ly)) r="3" {}
                text.chart-now x=(fmt(lx)) y=(fmt((ly - 6.0).max(10.0))) text-anchor="end" {
                    (last.live)
                }
            }
            p.chart-caption.dim {
                (n) " sample" (if n == 1 { "" } else { "s" })
                " · spanning " (fmt_span(span))
                " · now " (current) " running"
            }
        }
    }
}

/// The events view: the pooler's journal of failures and sweep summaries,
/// newest first — the page that answers "why are there N running VMs with no
/// sessions all started at the same minute" (a sweep's worth of boot-to-dump
/// attempts that failed and were never stopped shows up here as a cluster of
/// `archive`/`freeze` errors around one timestamp).
pub fn events_page(st: &DashState, entries: &[crate::events::JournalEntry]) -> Markup {
    use crate::events::{Level, fmt_ts};
    let errors = entries.iter().filter(|e| e.level == Level::Error).count();
    shell(
        "Events",
        html! {
            div.pagehead {
                h1 { "Events" }
                a.button-link href="/events" { "↻ refresh" }
            }
            @if st.cfg.basic_auth.is_none() {
                div.summary { span.warn { "auth: OFF" } }
            }
            div.summary {
                span { "showing " b { (entries.len()) } " most recent" }
                @if errors > 0 { span.warn { (errors) " error(s)" } }
            }
            @if entries.is_empty() {
                p.note {
                    "Nothing journaled yet. Failures (bring-ups, archives, freezes) and "
                    "archive/freeze sweep summaries land here as they happen; entries are "
                    "persisted in daily partitions under the metrics dir and survive restarts."
                }
            } @else {
                table.events {
                    thead {
                        tr { th { "time (UTC)" } th { "level" } th { "kind" } th { "what happened" } }
                    }
                    tbody {
                        @for e in entries {
                            tr {
                                td.dim { (fmt_ts(e.t)) }
                                td {
                                    @match e.level {
                                        Level::Error => { span.lvl.lvl-err { "error" } }
                                        Level::Info => { span.lvl.lvl-info { "info" } }
                                    }
                                }
                                td { code { (e.kind) } }
                                td.ev-msg { (e.msg) }
                            }
                        }
                    }
                }
                p.note.dim {
                    "Failures include bring-ups, archives (S3), freezes (local), and frozen-dump "
                    "promotions; sweep rows summarize each archive/freeze/pressure pass that had "
                    "work to do. Backed by daily " code { "journal-YYYY-MM-DD.tsv" }
                    " partitions (14-day retention)."
                }
            }
        },
    )
}

/// A self-contained inline-SVG bar chart of per-hour event counts — same
/// no-JS/no-libs idiom as [`vm_history_chart`]. `buckets` is oldest-first
/// `(hour_start_unix, count)` pairs (the events module emits whole UTC clock
/// hours; the last bucket is the current, partial hour). One series, so the
/// section heading is the only legend; each bar carries a native SVG `<title>`
/// tooltip and the peak bar is direct-labeled. `noun` names the unit in the
/// caption ("restore", "VM").
fn hourly_bar_chart(buckets: &[(u64, u32)], noun: &str) -> Markup {
    const W: f64 = 640.0;
    const H: f64 = 160.0;
    const PAD_L: f64 = 32.0;
    const PAD_R: f64 = 12.0;
    const PAD_T: f64 = 12.0;
    const PAD_B: f64 = 20.0;
    let plot_w = W - PAD_L - PAD_R;
    let plot_h = H - PAD_T - PAD_B;
    let baseline = PAD_T + plot_h;

    if buckets.is_empty() {
        return html! {
            div.chart-wrap {
                p.note.chart-empty { "No event history yet." }
            }
        };
    }

    let n = buckets.len();
    let max = buckets.iter().map(|(_, c)| *c).max().unwrap_or(0);
    let total: u64 = buckets.iter().map(|(_, c)| u64::from(*c)).sum();
    let axis_max = f64::from(max.max(1));
    let slot_w = plot_w / n as f64;
    // 2px surface gap between adjacent bars, and never thinner than 1px.
    let bar_w = (slot_w - 2.0).max(1.0);
    let bar_x = |i: usize| PAD_L + i as f64 * slot_w + 1.0;
    let y = |c: u32| PAD_T + (1.0 - f64::from(c) / axis_max) * plot_h;
    let hour_of = |t: u64| (t / 3600) % 24;

    let ticks: Vec<u32> = if max >= 2 { vec![0, max / 2, max] } else { vec![0, 1] };
    // Index of the tallest bar, for the one direct label.
    let peak = buckets
        .iter()
        .enumerate()
        .max_by_key(|(_, (_, c))| *c)
        .map(|(i, _)| i)
        .filter(|_| max > 0);

    html! {
        div.chart-wrap {
            svg.chart-svg viewBox="0 0 640 160" role="img"
                aria-label=(format!("{noun}s per hour over the last {n} hours")) {
                @for t in &ticks {
                    @let gy = y(*t);
                    // Empty `{}` (not `;`) — see vm_history_chart: SVG shapes
                    // are not HTML void elements under maud.
                    line.chart-grid x1=(fmt(PAD_L)) y1=(fmt(gy)) x2=(fmt(W - PAD_R)) y2=(fmt(gy)) {}
                    text.chart-ylabel x=(fmt(PAD_L - 5.0)) y=(fmt(gy + 3.0)) text-anchor="end" {
                        (t)
                    }
                }
                @for (i, (t, count)) in buckets.iter().enumerate() {
                    // Clock labels at 00/06/12/18 UTC so ticks stay put as the
                    // window slides.
                    @if hour_of(*t) % 6 == 0 {
                        text.chart-xlabel x=(fmt(bar_x(i) + bar_w / 2.0)) y=(fmt(H - 6.0))
                            text-anchor="middle" {
                            (format!("{:02}:00", hour_of(*t)))
                        }
                    }
                    @if *count > 0 {
                        rect.chart-bar x=(fmt(bar_x(i))) y=(fmt(y(*count)))
                            width=(fmt(bar_w)) height=(fmt(baseline - y(*count))) rx="2" {
                            title {
                                (format!("{:02}:00–{:02}:00 UTC · {count} {noun}{}",
                                    hour_of(*t), (hour_of(*t) + 1) % 24,
                                    if *count == 1 { "" } else { "s" }))
                            }
                        }
                        @if peak == Some(i) {
                            text.chart-now x=(fmt(bar_x(i) + bar_w / 2.0))
                                y=(fmt((y(*count) - 4.0).max(10.0))) text-anchor="middle" {
                                (count)
                            }
                        }
                    }
                }
            }
            p.chart-caption.dim {
                (total) " " (noun) (if total == 1 { "" } else { "s" })
                " in the last " (n) "h · one bar per UTC clock hour"
            }
        }
    }
}

/// Format an SVG coordinate to one decimal place.
fn fmt(v: f64) -> String {
    format!("{v:.1}")
}

/// Compact human duration for the chart caption: `3h 12m`, `42m`, `30s`.
fn fmt_span(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// The webhook-alerts panel: existing rules — editable in place, with live
/// firing state and pause/delete controls — plus a form to add a new rule.
/// Each row's inputs bind to a per-rule edit form in its actions cell via the
/// HTML5 `form` attribute (forms can't nest, and pause/delete are their own
/// one-button forms in the same cell).
fn alerts_section(st: &DashState) -> Markup {
    let rules = st.alerts.list();
    let interval = st.cfg.alert_interval.as_secs();
    html! {
        section.controls {
            h2 { "alerts" }
            @if rules.is_empty() {
                p.dim { "No alert rules configured." }
            } @else {
                table.alerts {
                    thead {
                        tr {
                            th { "metric" }
                            th { "threshold" }
                            th { "webhook" }
                            th { "state" }
                            th {}
                        }
                    }
                    tbody {
                        @for r in &rules {
                            @let edit_id = format!("alert-edit-{}", r.id);
                            tr {
                                td {
                                    select name="metric" form=(edit_id) {
                                        @for m in Metric::all() {
                                            option value=(m.slug()) selected[m == r.metric] {
                                                (m.label())
                                            }
                                        }
                                    }
                                }
                                td {
                                    "≥ "
                                    input.threshold type="number" name="threshold_pct"
                                        form=(edit_id) min="0" max="100" step="0.1"
                                        value=(fmt_pct(r.threshold_pct)) required;
                                    span.dim { (r.metric.unit()) }
                                }
                                td {
                                    input.webhook type="url" name="webhook_url" form=(edit_id)
                                        value=(r.webhook_url) required;
                                }
                                td { (alert_state_badge(r)) }
                                td.actions {
                                    form id=(edit_id) method="post"
                                        action={ "/monitoring/alerts/" (r.id) "/update" } {
                                        button { "save" }
                                    }
                                    @if r.paused {
                                        form method="post"
                                            action={ "/monitoring/alerts/" (r.id) "/resume" } {
                                            button { "resume" }
                                        }
                                    } @else {
                                        form method="post"
                                            action={ "/monitoring/alerts/" (r.id) "/pause" } {
                                            button { "pause" }
                                        }
                                    }
                                    form method="post"
                                        action={ "/monitoring/alerts/" (r.id) "/delete" } {
                                        button.stop
                                            onclick="return confirm('Delete this alert rule?')"
                                            { "delete" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            h3.sub-head { "add rule" }
            form.alert-add method="post" action="/monitoring/alerts" {
                label {
                    span { "metric" }
                    select name="metric" {
                        @for m in Metric::all() {
                            option value=(m.slug()) { (m.label()) }
                        }
                    }
                }
                label {
                    span { "fire at ≥ (% — or failed checks for daemon health)" }
                    input type="number" name="threshold_pct" min="0" max="100" step="1"
                        value="90" required;
                }
                label.grow {
                    span { "webhook URL" }
                    input type="url" name="webhook_url" placeholder="https://hooks.example.com/…"
                        required;
                }
                button type="submit" { "add alert" }
            }
            p.note {
                "The evaluator samples host CPU, memory, the fullest disk, and the "
                "daemon's health endpoint every "
                (interval) "s and POSTs a JSON body (" code { "state" } " of "
                code { "triggered" } "/" code { "resolved" }
                ") to the URL when a rule crosses its threshold — once per crossing, "
                "not every interval. The disk rule watches the fullest host filesystem. "
                "For the daemon health check the threshold counts CONSECUTIVE failed "
                code { "GET /health" } " probes (e.g. 3 = silent for 3 straight "
                "intervals) — set it below the watchdog's restart threshold to get "
                "warned before a restart. A paused rule keeps its config but is "
                "skipped until resumed."
            }
        }
    }
}

fn alert_state_badge(r: &RuleView) -> Markup {
    html! {
        @if r.paused {
            span.badge.s-stopped { "paused" }
        } @else if r.firing {
            span.badge.s-failed { "firing" }
        } @else {
            span.badge.s-running { "ok" }
        }
    }
}

/// Format a threshold percent without a needless trailing `.0`.
fn fmt_pct(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Host CPU meter. `cpu_percent` is whole-machine 0–100.
fn host_cpu_metric(h: &HostUsage) -> Markup {
    match h.cpu_percent {
        Some(cpu) => {
            let frac = (cpu as f64 / 100.0).clamp(0.0, 1.0);
            let sub = h.cpu_count.map(|n| format!("{n} cores"));
            metric("CPU", frac, &format!("{cpu:.1}%"), sub.as_deref())
        }
        None => metric_unavailable("CPU"),
    }
}

/// Host memory meter, used / total.
fn host_mem_metric(h: &HostUsage) -> Markup {
    match (h.memory_used_bytes, h.memory_total_bytes) {
        (Some(used), Some(total)) if total > 0 => {
            let frac = (used as f64 / total as f64).clamp(0.0, 1.0);
            let val = format!("{:.0}%", frac * 100.0);
            let sub = format!("{} of {}", human_bytes(used), human_bytes(total));
            metric("Memory", frac, &val, Some(&sub))
        }
        _ => metric_unavailable("Memory"),
    }
}

fn disk_metric(d: &HostDisk) -> Markup {
    let frac = d.saturation();
    let val = format!("{:.0}%", frac * 100.0);
    let sub = format!(
        "{} of {} · {} free",
        human_bytes(d.used),
        human_bytes(d.total),
        human_bytes(d.avail)
    );
    // Label by mount point; the device is the fine-print second line.
    metric(&d.mount, frac, &val, Some(&format!("{} · {}", d.source, sub)))
}

/// A labeled saturation meter: a big percent, a colored bar, and a caption. The
/// bar's color escalates ok→warn→crit as it fills so a hot resource reads at a
/// glance.
fn metric(label: &str, frac: f64, value: &str, sub: Option<&str>) -> Markup {
    let pct = (frac * 100.0).clamp(0.0, 100.0);
    let cls = meter_level(frac);
    html! {
        div.metric {
            div.metric-head {
                span.metric-label { (label) }
                span.metric-val { (value) }
            }
            div.meter {
                div class={ "meter-fill " (cls) } style=(format!("width:{pct:.1}%")) {}
            }
            @if let Some(s) = sub { div.metric-sub { (s) } }
        }
    }
}

fn metric_unavailable(label: &str) -> Markup {
    html! {
        div.metric {
            div.metric-head {
                span.metric-label { (label) }
                span.metric-val.dim { "—" }
            }
            div.meter { div class="meter-fill ok" style="width:0%" {} }
            div.metric-sub { "unavailable" }
        }
    }
}

/// Color band for a meter by fill fraction: calm below 70%, amber to 90%, red
/// above — the usual "getting full" escalation.
fn meter_level(frac: f64) -> &'static str {
    if frac >= 0.90 {
        "crit"
    } else if frac >= 0.70 {
        "warn"
    } else {
        "ok"
    }
}

/// A compact aggregate stat card: a number, a label, and an optional caption.
fn stat(label: &str, value: &str, sub: Option<&str>) -> Markup {
    html! {
        div.stat {
            div.stat-val { (value) }
            div.stat-label { (label) }
            @if let Some(s) = sub { div.stat-sub { (s) } }
        }
    }
}

/// State filter pills: "all" plus every state with matches (and the selected
/// state even at zero, so the active filter stays visible/escapable). Counts
/// are within the current search, so they show where the matches live.
fn state_pills(p: &SandboxPage) -> Markup {
    let all: usize = p.state_counts.iter().map(|(_, n)| n).sum();
    html! {
        div.pills {
            a.pill.selected[p.state == model::STATE_ALL]
                href=(list_href(&p.q, model::STATE_ALL, 1, p.per)) {
                "all " span.count { (all) }
            }
            @for (label, n) in &p.state_counts {
                @if *n > 0 || p.state == *label {
                    a.pill.selected[p.state == *label]
                        href=(list_href(&p.q, label, 1, p.per)) {
                        (label) " " span.count { (n) }
                    }
                }
            }
        }
    }
}

/// Prev/next page controls; hidden when everything fits on one page.
fn pager(p: &SandboxPage) -> Markup {
    html! {
        @if p.pages > 1 {
            div.pager {
                @if p.page > 1 {
                    a.button-link href=(list_href(&p.q, &p.state, p.page - 1, p.per)) { "← prev" }
                }
                span { "page " (p.page) " of " (p.pages) }
                @if p.page < p.pages {
                    a.button-link href=(list_href(&p.q, &p.state, p.page + 1, p.per)) { "next →" }
                }
            }
        }
    }
}

/// Build a Databases-list link, omitting params that hold their default value.
fn list_href(q: &str, state: &str, page: usize, per: usize) -> String {
    let mut href = String::from("/?");
    if page != 1 {
        href.push_str(&format!("page={page}&"));
    }
    if state != model::DEFAULT_STATE {
        href.push_str(&format!("state={}&", urlenc(state)));
    }
    if !q.is_empty() {
        href.push_str(&format!("q={}&", urlenc(q)));
    }
    if per != model::DEFAULT_PER {
        href.push_str(&format!("per={per}&"));
    }
    href.truncate(href.len() - 1); // trailing '&' or the bare '?'
    href
}

/// Minimal RFC 3986 percent-encoder for a query value (search text is
/// user-typed, so it can contain anything).
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn vm_detail_page(
    st: &DashState,
    r: &VmRow,
    db: Option<&DbStats>,
    guest: Option<&GuestStats>,
    b: &Banner,
) -> Markup {
    shell(
        &r.name,
        html! {
            p { a href="/" { "← Databases" } }
            (banner(b))
            div.pagehead {
                h1 { (r.name) " " (status_badge_row(r)) }
                // A plain GET back to the canonical URL: re-reads daemon +
                // pooler state and drops any one-shot ?msg/?err banner.
                a.button-link href={ "/vm/" (r.id) } { "↻ refresh" }
            }

            h2 { "configuration" }
            dl.detail {
                dt { "id" }          dd { code { (r.id) } }
                dt { "schema" }      dd { (r.schema.as_deref().unwrap_or("—")) }
                dt { "pooler-managed" } dd { (yesno(r.pool_managed)) }
                dt { "vCPUs" } dd { (r.cpus.map(|c| c.to_string()).unwrap_or_else(|| "—".into())) }
                dt { "memory" } dd { (r.memory_bytes.map(human_bytes).unwrap_or_else(|| "—".into())) }
                dt { "disk" } dd { (r.disk_size_gb.map(|g| format!("{g} GB")).unwrap_or_else(|| "—".into())) }
                @if let Some(sc) = &r.size_class { dt { "size class" } dd { (sc) } }
                @if let Some((free, total)) = r.client_slots {
                    dt { "client slots" }
                    dd {
                        @if free == 0 {
                            span.badge.active { (format!("0 / {total} — clients queueing")) }
                        } @else {
                            (format!("{free} / {total} free"))
                        }
                    }
                }
                @if let Some(img) = &r.image { dt { "image" } dd { (img) } }
                @if let Some(reg) = &r.region { dt { "region" } dd { (reg) } }
                dt { "uptime" } dd { (if r.is_running() { human_secs(r.uptime_secs) } else { "—".into() }) }
                dt { "guest ip" } dd { (r.guest_ip.as_deref().unwrap_or("—")) }
                @if let Some(t) = r.ttl_seconds {
                    dt { "ttl" } dd { (if t == 0 { "0 (pinned)".to_string() } else { human_secs(t) }) }
                }
                @if let Some(c) = &r.status_changed_at { dt { "status changed" } dd.dim { (c) } }
                dt { "keep-alive" } dd { (yesno(r.keepalive)) }
                @if let Some(target) = r.target {
                    dt { "splice target" }
                    dd { code { (target.to_string()) } (if r.tunneled == Some(true) { " (tunnel)" } else { " (direct)" }) }
                }
                @if let Some(err) = &r.error_message { dt { "error" } dd.err { (err) } }
            }

            h2 { "resource usage" }
            dl.detail {
                dt { "allocated" }
                dd { (allocated_str(r).unwrap_or_else(|| "—".into())) }
                @if let Some(cpu) = r.cpu_percent {
                    dt { "cpu" }
                    dd {
                        (format!("{cpu:.1}%"))
                        @if let Some(c) = r.cpus {
                            span.dim { " of " (c as u64 * 100) "% (" (c) " vCPU)" }
                        } @else {
                            span.dim { " of one core per vCPU" }
                        }
                    }
                }
                dt { "live sessions" } dd { (sessions_cell(r)) }
                @if let Some(idle) = r.idle_secs {
                    dt { "idle for" }
                    dd {
                        (human_secs(idle))
                        @if let Some(t) = st.registry.idle_timeout() {
                            span.dim { " (reaped after " (human_secs(t.as_secs())) ")" }
                        }
                    }
                }
                @if let Some(s) = db {
                    dt { "database size" } dd { (human_bytes(s.db_size_bytes.max(0) as u64)) }
                    dt { "backend conns" } dd { (s.backends) }
                } @else if r.pool_managed && r.is_running() {
                    dt { "database" } dd.dim { "(VM not warm in the pooler — no live DB stats)" }
                }
                @if let Some(g) = guest {
                    @if let Some((total, avail)) = g.mem {
                        dt { "guest memory" }
                        dd {
                            (human_bytes(total.saturating_sub(avail))) " used"
                            span.dim { " of " (human_bytes(total)) }
                        }
                    }
                    @if let Some((l1, l5, l15)) = g.load {
                        dt { "load average" }
                        dd { (format!("{l1:.2} / {l5:.2} / {l15:.2}")) span.dim { " (1 / 5 / 15 min)" } }
                    }
                    @if let Some((total, used, avail)) = g.disk {
                        dt { "data disk" }
                        dd {
                            (human_bytes(used)) " used"
                            span.dim { " of " (human_bytes(total)) " · " (human_bytes(avail)) " free" }
                        }
                    }
                }
            }
            @if guest.is_some() || r.cpu_percent.is_some() {
                p.note {
                    @if r.cpu_percent.is_some() {
                        "CPU is heyvmd's host-side sample of the VM's "
                        "process(es), " code { "top" } " convention: 100% = one "
                        "core, so a busy guest can exceed 100%. "
                    }
                    @if guest.is_some() {
                        "Guest memory/load/disk are read over the pooler's Postgres "
                        "connection (" code { "pg_read_file" } " on " code { "/proc" } ", "
                        code { "df" } " via " code { "COPY FROM PROGRAM" } ") — "
                        "no guest-console access."
                    }
                }
            }

            section.controls {
                h2 { "controls" }
                div.actions { (action_buttons(r, st.registry.archive_enabled(), None)) }
                @if st.registry.image_archive_enabled()
                    && r.pool_managed
                    && r.schema.is_some()
                    && r.offload.is_none()
                {
                    div.actions {
                        form method="post" action={ "/vm/" (r.id) "/archive-image" } {
                            button.reap
                                onclick="return confirm('Archive this VM as a raw disk image? No pg_dump runs: the VM is stopped, its disk is trimmed, compressed, and uploaded to S3, then the VM is deleted. Meant for schemas whose Postgres will not boot — a healthy schema is better served by reap → S3.')"
                                { "archive disk image → S3" }
                        }
                    }
                    p.note {
                        "The image archive needs no working Postgres — it uploads the raw "
                        code { "data.ext4" }
                        " — but it preserves the pgdata version, so restoring it requires a "
                        "rootfs with a matching Postgres major."
                    }
                }
                form.resize method="post" action={ "/vm/" (r.id) "/resize" } {
                    label { "resize to " }
                    select name="size_class" {
                        @for s in SIZE_CLASSES {
                            option value=(s) selected[r.size_class.as_deref() == Some(s)] { (s) }
                        }
                    }
                    button type="submit" { "resize" }
                }
                p.note {
                    "Pooler-managed VMs stopped here auto-restart on the next client "
                    "connection; a resize takes effect on the VM's next boot."
                    @if st.registry.archive_enabled() {
                        " Reaping to S3 dumps the database, deletes the VM and its disk, "
                        "and restores the data into a fresh VM on the next connection."
                    }
                }
            }

            section {
                h2 { "logs" }
                p {
                    a.button-link href={ "/logs/vm/" (r.id) } { "view Postgres log →" }
                }
                p.note {
                    "Opening the VM log runs "
                    code { "tail" }
                    " inside the guest — a deliberate action, kept off this page so "
                    "simply viewing a VM never touches it."
                }
            }
        },
    )
}

/// A tailed log. `href_base` is the page's own path (e.g. `/logs/pooler`), used
/// to build the auto-refresh toggle links so switching intervals keeps you on the
/// same log. `refresh` is the current interval in seconds (`None`/0 = off).
///
/// Refresh is a plain `meta http-equiv="refresh"`: the browser reloads the whole
/// URL — query string included — so `?refresh=N` survives each reload without any
/// client state. A small script filters lines client-side (no server round-trip,
/// so it costs the pooler nothing) and jumps to the newest lines after each load.
pub fn log_page(
    title: &str,
    href_base: &str,
    source: &str,
    text: &str,
    refresh: Option<u64>,
) -> Markup {
    let secs = refresh.filter(|&s| s > 0).map(|s| s.clamp(REFRESH_MIN, REFRESH_MAX));
    let extra_head = html! {
        @if let Some(s) = secs {
            meta http-equiv="refresh" content=(s);
        }
    };
    shell_with_head(
        title,
        extra_head,
        html! {
            p { a href="/" { "← Databases" } }
            h1 { "log · " (title) }
            p.dim { code { (source) } }
            (refresh_controls(href_base, secs))
            div.log-search {
                input #logfilter type="text" placeholder="filter lines (client-side)…"
                    autocomplete="off" spellcheck="false";
                span.dim #logcount {}
            }
            pre.log #logbody { (text) }
            script { (PreEscaped(LOG_SCRIPT)) }
        },
    )
}

/// Client-side log filter + scroll-to-newest. Captures the rendered lines once,
/// then on each keystroke shows only lines containing the query (case-insensitive)
/// — all in the browser, so the pooler is never queried. The filter text is kept
/// in `sessionStorage` keyed by path, so it survives the `meta` auto-refresh
/// reload and per-log tabs don't share a query.
const LOG_SCRIPT: &str = r#"
(function () {
  var pre = document.getElementById('logbody');
  var input = document.getElementById('logfilter');
  var count = document.getElementById('logcount');
  if (!pre || !input) return;
  var lines = pre.textContent.split('\n');
  var key = 'logfilter:' + location.pathname;
  function apply(scroll) {
    var q = input.value.toLowerCase();
    var shown = q ? lines.filter(function (l) { return l.toLowerCase().indexOf(q) !== -1; }) : lines;
    pre.textContent = shown.join('\n');
    if (count) count.textContent = q ? (shown.length + ' / ' + lines.length + ' lines') : '';
    if (scroll) window.scrollTo(0, document.body.scrollHeight);
  }
  try { input.value = sessionStorage.getItem(key) || ''; } catch (e) {}
  input.addEventListener('input', function () {
    try { sessionStorage.setItem(key, input.value); } catch (e) {}
    apply(false);
  });
  apply(true);
})();
"#;

/// Bounds on the honored auto-refresh interval, so a hand-edited `?refresh=`
/// can't ask for a hammering 0.1s reload or an effectively-off multi-hour one.
const REFRESH_MIN: u64 = 1;
const REFRESH_MAX: u64 = 3600;

/// The auto-refresh toggle row for a log page: preset intervals plus a manual
/// reload. The active preset renders as plain text; the rest as links that set
/// `?refresh=N` on `base` (N=0 clears it).
fn refresh_controls(base: &str, active: Option<u64>) -> Markup {
    const PRESETS: [(&str, u64); 5] = [("off", 0), ("2s", 2), ("5s", 5), ("10s", 10), ("30s", 30)];
    let current = active.unwrap_or(0);
    let href = |secs: u64| {
        if secs == 0 {
            base.to_string()
        } else {
            format!("{base}?refresh={secs}")
        }
    };
    html! {
        div.log-controls {
            span.dim { "auto-refresh:" }
            @for (label, secs) in PRESETS {
                @if secs == current {
                    span.refresh-opt.active { (label) }
                } @else {
                    a.refresh-opt href=(href(secs)) { (label) }
                }
            }
            a.button-link href=(href(current)) { "↻ reload now" }
        }
    }
}

pub fn not_found_page(id: &str) -> Markup {
    shell(
        "not found",
        html! {
            p { a href="/" { "← Databases" } }
            h1 { "VM not found" }
            p { "No sandbox with id " code { (id) } " is known to the daemon." }
        },
    )
}

pub fn error_page(err: &anyhow::Error) -> Markup {
    shell(
        "error",
        html! {
            p { a href="/" { "← Databases" } }
            h1 { "Something went wrong" }
            pre.log { (format!("{err:#}")) }
        },
    )
}

// ---- fragments -------------------------------------------------------------

/// The VM list table, shared by the index and the paged all-sandboxes view.
fn vm_table(rows: &[VmRow], archive_enabled: bool, next: &str) -> Markup {
    html! {
        table {
            thead {
                tr {
                    th { "VM" }
                    th { "schema" }
                    th { "status" }
                    th { "size" }
                    th { "cpu" }
                    th { "uptime" }
                    th { "sessions" }
                    th { "actions" }
                }
            }
            tbody {
                @for r in rows {
                    tr {
                        td { a href={ "/vm/" (r.id) } { (r.name) } }
                        td.dim { (r.schema.as_deref().unwrap_or("—")) }
                        td { (status_badge_row(r)) }
                        td { (size_cell(r)) }
                        td { (cpu_cell(r)) }
                        td { (if r.is_running() { human_secs(r.uptime_secs) } else { "—".into() }) }
                        td { (sessions_cell(r)) }
                        td.actions { (action_buttons(r, archive_enabled, Some(next))) }
                    }
                }
            }
        }
    }
}

/// `next`: where the action should land afterwards. `Some(list URL)` from the
/// Databases table (stay on the list — stopping 40 VMs must not mean 40 detail
/// pages); `None` from the detail page (stay on the detail page).
fn action_buttons(r: &VmRow, archive_enabled: bool, next: Option<&str>) -> Markup {
    let running = r.status == SandboxStatus::Running;
    // Offer manual reap only for an idle, pooler-managed, running schema VM —
    // archiving one with live sessions is refused server-side, so don't tempt it.
    let can_reap = archive_enabled
        && running
        && r.pool_managed
        && r.offload.is_none()
        && r.schema.is_some()
        && r.live_sessions.unwrap_or(0) == 0;
    let next_input = || {
        html! {
            @if let Some(n) = next {
                input type="hidden" name="next" value=(n);
            }
        }
    };
    html! {
        @if running {
            form method="post" action={ "/vm/" (r.id) "/stop" } { (next_input()) button.stop { "stop" } }
            form method="post" action={ "/vm/" (r.id) "/reboot" } { (next_input()) button { "reboot" } }
        } @else {
            form method="post" action={ "/vm/" (r.id) "/start" } { (next_input()) button.start { "start" } }
        }
        @if can_reap {
            form method="post" action={ "/vm/" (r.id) "/reap" } {
                (next_input())
                button.reap
                    onclick="return confirm('Reap this VM to S3? The VM and its data disk will be deleted; the data is restored from S3 on the next connection.')"
                    { "reap → S3" }
            }
        }
    }
}

fn size_cell(r: &VmRow) -> Markup {
    // Prefer the concrete cpus/memory the daemon reports; fall back to a size
    // class label, then to nothing.
    html! {
        @match (r.cpus, r.memory_bytes) {
            (Some(c), Some(m)) => {
                (format!("{c} vCPU"))
                span.dim.sub { (human_bytes(m)) }
            }
            _ => @match r.size_class.as_deref() {
                Some(s) => { (s) }
                None => span.dim { "—" },
            },
        }
    }
}

/// Human-readable allocated resources for the detail page ("4 vCPU · 8.0 GiB ·
/// 4 GB disk"), from whatever the daemon reported.
fn allocated_str(r: &VmRow) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = r.cpus {
        parts.push(format!("{c} vCPU"));
    }
    if let Some(m) = r.memory_bytes {
        parts.push(human_bytes(m));
    }
    if let Some(g) = r.disk_size_gb {
        parts.push(format!("{g} GB disk"));
    }
    if parts.is_empty() {
        r.size_class.clone()
    } else {
        Some(parts.join(" · "))
    }
}

/// Daemon-sampled CPU (`top` convention: 100% = one core), same source as the
/// detail page's cpu row.
fn cpu_cell(r: &VmRow) -> Markup {
    html! {
        @match r.cpu_percent {
            Some(cpu) => { (format!("{cpu:.1}%")) }
            None => span.dim { "—" },
        }
    }
}

fn sessions_cell(r: &VmRow) -> Markup {
    html! {
        @match r.live_sessions {
            Some(n) if n > 0 => span.badge.active { (n) },
            Some(n) => span.dim { (n) },
            None => span.dim { "—" },
        }
    }
}

/// Status badge that accounts for the offloaded tiers: an offloaded schema has
/// no live sandbox, so its daemon status is meaningless — show where the data
/// actually lives instead.
fn status_badge_row(r: &VmRow) -> Markup {
    match r.offload {
        Some("frozen") => html! { span.badge class="s-archived" { "frozen (local)" } },
        Some("compacted") => html! { span.badge class="s-archived" { "compacted (local)" } },
        Some(_) => html! { span.badge class="s-archived" { "archived (S3)" } },
        None => status_badge(&r.status),
    }
}

fn status_badge(status: &SandboxStatus) -> Markup {
    let class = match status {
        SandboxStatus::Running => "s-running",
        SandboxStatus::Provisioning => "s-prov",
        SandboxStatus::Stopped | SandboxStatus::Paused | SandboxStatus::ColdStored => "s-stopped",
        SandboxStatus::Failed => "s-failed",
        SandboxStatus::Unknown => "s-unknown",
    };
    // Label comes from the model so searching e.g. "running" matches the badge.
    html! { span.badge class=(class) { (model::status_str(status)) } }
}

fn yesno(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn human_secs(s: u64) -> String {
    let d = s / 86_400;
    let h = (s % 86_400) / 3_600;
    let m = (s % 3_600) / 60;
    let sec = s % 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_level_bands() {
        assert_eq!(meter_level(0.0), "ok");
        assert_eq!(meter_level(0.699), "ok");
        assert_eq!(meter_level(0.70), "warn");
        assert_eq!(meter_level(0.899), "warn");
        assert_eq!(meter_level(0.90), "crit");
        assert_eq!(meter_level(1.0), "crit");
    }

    #[test]
    fn fmt_pct_trims_whole_numbers() {
        assert_eq!(fmt_pct(90.0), "90");
        assert_eq!(fmt_pct(85.5), "85.5");
    }

    #[test]
    fn fmt_span_scales_units() {
        assert_eq!(fmt_span(45), "45s");
        assert_eq!(fmt_span(600), "10m");
        assert_eq!(fmt_span(3 * 3600 + 12 * 60), "3h 12m");
    }

    #[test]
    fn vm_history_chart_handles_empty_single_and_many() {
        // Empty: no SVG, just the collecting note.
        let empty = vm_history_chart(&[], 0).into_string();
        assert!(!empty.contains("<svg"), "empty history should not draw a chart");
        assert!(empty.contains("Collecting"));

        // A single sample must not divide by zero (n-1 == 0) and still renders.
        let one = vm_history_chart(&[Sample { t: 100, live: 3 }], 3).into_string();
        assert!(one.contains("<svg"));
        assert!(one.contains("1 sample "), "singular caption: {one}");

        // Many samples: polyline with one point per sample, caption pluralized,
        // and the y-axis labelled up to the max.
        let samples: Vec<Sample> = (0..5).map(|i| Sample { t: 100 + i * 60, live: i as u32 }).collect();
        let many = vm_history_chart(&samples, 4).into_string();
        assert!(many.contains("polyline"));
        assert!(many.matches(',').count() >= 5, "one point per sample");
        assert!(many.contains("5 samples"));
        assert!(many.contains("now 4 running"));
        // SVG shapes must be explicitly closed. A bare `<line>`/`<polyline>`
        // (maud void syntax) nests every following shape inside it as an
        // unrendered child, so the chart draws nothing — regression guard.
        assert!(many.contains("</polyline>"), "polyline must be closed: {many}");
        assert!(many.contains("</line>"), "gridlines must be closed");
        assert!(many.contains("</polygon>"), "area must be closed");
    }

    #[test]
    fn hourly_bar_chart_draws_bars_tooltips_and_peak_label() {
        // Empty input: a note, no SVG.
        let empty = hourly_bar_chart(&[], "restore").into_string();
        assert!(!empty.contains("<svg"), "empty series should not draw a chart");

        // All-zero window: the frame renders (axes prove the chart is alive)
        // but no bars, and the caption says 0.
        let base = 1_753_600 * 3600; // an exact hour boundary
        let zeroes: Vec<(u64, u32)> = (0..24).map(|i| (base + i * 3600, 0)).collect();
        let flat = hourly_bar_chart(&zeroes, "restore").into_string();
        assert!(flat.contains("<svg"));
        assert!(!flat.contains("chart-bar"), "no bars for zero counts");
        assert!(flat.contains("0 restores in the last 24h"));

        // Real counts: one rect per non-zero bucket, closed (maud void-element
        // trap, as above), a native <title> tooltip, and exactly one direct
        // label on the peak bar.
        let counts: Vec<(u64, u32)> = vec![(base, 1), (base + 3600, 0), (base + 7200, 3)];
        let bars = hourly_bar_chart(&counts, "VM").into_string();
        assert_eq!(bars.matches("<rect").count(), 2, "one bar per non-zero hour");
        assert!(bars.contains("</rect>"), "bars must be closed");
        assert!(bars.contains("<title>"), "bars carry native tooltips");
        assert!(bars.contains("4 VMs in the last 3h"));
        assert_eq!(
            bars.matches("chart-now").count(),
            1,
            "exactly one peak label: {bars}"
        );
        assert!(bars.contains(">3</text>"), "peak label shows the max count");
    }

    #[test]
    fn metric_renders_clamped_bar_and_band() {
        // A hot disk: fill clamps to 100%, gets the crit color, shows the caption.
        let html = metric("/data", 1.4, "140%", Some("/dev/sda1")).into_string();
        assert!(html.contains("width:100.0%"), "bar should clamp: {html}");
        assert!(html.contains("meter-fill crit"), "band should be crit: {html}");
        assert!(html.contains("/data"));
        assert!(html.contains("/dev/sda1"));

        // A calm metric: partial fill, ok color.
        let html = metric("CPU", 0.25, "25.0%", None).into_string();
        assert!(html.contains("width:25.0%"));
        assert!(html.contains("meter-fill ok"));
    }

    #[test]
    fn disk_metric_shows_saturation_and_free() {
        let d = HostDisk {
            source: "/dev/sda1".into(),
            mount: "/".into(),
            total: 100 * 1024 * 1024 * 1024,
            used: 88 * 1024 * 1024 * 1024,
            avail: 12 * 1024 * 1024 * 1024,
        };
        let html = disk_metric(&d).into_string();
        assert!(html.contains("meter-fill warn"), "88% → warn band: {html}");
        assert!(html.contains("free"));
        assert!(html.contains("/dev/sda1"));
    }
}

// Light/dark via CSS custom properties; every color is defined for both schemes
// so contrast holds up in dark mode (the earlier version left light badge/pre
// colors on a dark page).
const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --bg:#f6f7f9; --fg:#1a1a1a; --dim:#6b7280; --muted:#8a8f98;
  --card:#ffffff; --border:#e3e5e9; --th-bg:#f0f2f5; --row-hover:#f6f8fb;
  --link:#2563eb; --code-bg:#eef0f3; --pre-bg:#0f1115; --pre-fg:#d4d7dd;
  --btn-bg:#ffffff; --btn-border:#c7ccd3; --btn-hover:#9aa1ab;
  --ok-bg:#e3f6e5; --ok-fg:#166534; --ok-border:#a9e0b3;
  --err-bg:#fde4e4; --err-fg:#b00020; --err-border:#e6a9a9;
  --warn:#b00020;
  --run-bg:#dcfce7; --run-fg:#166534;
  --stop-bg:#e5e7eb; --stop-fg:#4b5563;
  --prov-bg:#fef3c7; --prov-fg:#92600a;
  --fail-bg:#fee2e2; --fail-fg:#b00020;
  --sess-bg:#dbeafe; --sess-fg:#1e40af;
  --meter-track:#e6e8ec; --meter-ok:#16a34a; --meter-warn:#d97706; --meter-crit:#dc2626;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg:#15171c; --fg:#e6e8eb; --dim:#9aa1ab; --muted:#7b828c;
    --card:#1e2129; --border:#2f333c; --th-bg:#252a33; --row-hover:#252a33;
    --link:#7ab7ff; --code-bg:#0f1115; --pre-bg:#0d0f13; --pre-fg:#cdd3db;
    --btn-bg:#252a33; --btn-border:#3a404b; --btn-hover:#565e6b;
    --ok-bg:#123020; --ok-fg:#6ee7a8; --ok-border:#1f5136;
    --err-bg:#3a1618; --err-fg:#ff9ba0; --err-border:#5e2529;
    --warn:#ff9ba0;
    --run-bg:#123020; --run-fg:#5edb93;
    --stop-bg:#2a2f38; --stop-fg:#aab1bd;
    --prov-bg:#3a2f12; --prov-fg:#f2cd6b;
    --fail-bg:#3a1618; --fail-fg:#ff9ba0;
    --sess-bg:#16294a; --sess-fg:#9cc4ff;
    --meter-track:#2f333c; --meter-ok:#3ec07a; --meter-warn:#e0a13a; --meter-crit:#f0685f;
  }
}
* { box-sizing: border-box; }
body { margin:0; font:14px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;
       color:var(--fg); background:var(--bg); }
a { color:var(--link); text-decoration:none; }
a:hover { text-decoration:underline; }
.nav { display:flex; align-items:center; gap:1.5rem; padding:.7rem 1.2rem;
       background:var(--card); border-bottom:1px solid var(--border); }
.nav .brand { font-weight:700; color:var(--fg); }
.nav nav { display:flex; gap:1rem; }
main { padding:1.2rem; max-width:1200px; margin:0 auto; }
h1 { font-size:1.3rem; }
.pagehead { display:flex; align-items:center; justify-content:space-between; gap:1rem; }
.pagehead h1 { margin:.6rem 0; }
.pagehead-actions { display:flex; align-items:center; gap:.6rem; }
.inline-form { display:inline; margin:0; }
.chart-wrap { background:var(--card); border:1px solid var(--border); border-radius:8px;
         padding:.6rem .6rem .2rem; margin:.25rem 0 1rem; max-width:680px; }
.chart-svg { display:block; width:100%; height:auto; }
.chart-empty { margin:.4rem .2rem .6rem; }
.chart-grid { stroke:var(--border); stroke-width:1; }
.chart-ylabel, .chart-now { fill:var(--muted); font-size:11px; }
.chart-now { fill:var(--link); font-weight:600; }
.chart-area { fill:var(--link); fill-opacity:.12; stroke:none; }
.chart-line { fill:none; stroke:var(--link); stroke-width:2; stroke-linejoin:round; stroke-linecap:round; }
.chart-dot { fill:var(--link); stroke:var(--card); stroke-width:1.5; }
.chart-bar { fill:var(--link); fill-opacity:.8; }
.chart-bar:hover { fill-opacity:1; }
.chart-xlabel { fill:var(--muted); font-size:10px; }
.lvl { display:inline-block; padding:.05rem .45rem; border-radius:999px; font-size:.75rem;
        font-weight:600; border:1px solid; }
.lvl-err { background:var(--err-bg); color:var(--err-fg); border-color:var(--err-border); }
.lvl-info { background:var(--code-bg); color:var(--dim); border-color:var(--border); }
table.events td { vertical-align:top; }
table.events td.dim { white-space:nowrap; }
td.ev-msg { word-break:break-word; }
.chart-caption { margin:.2rem .2rem .5rem; font-size:.8rem; }
h2 { font-size:1rem; margin:1.4rem 0 .5rem; }
.summary { display:flex; gap:1.5rem; margin:.5rem 0 1rem; color:var(--dim); }
.summary b { color:var(--fg); }
.summary .warn { color:var(--warn); font-weight:600; }
table { width:100%; border-collapse:collapse; background:var(--card);
        border:1px solid var(--border); border-radius:8px; overflow:hidden; }
th, td { text-align:left; padding:.5rem .7rem; border-bottom:1px solid var(--border); white-space:nowrap; }
th { background:var(--th-bg); font-weight:600; font-size:.78rem; text-transform:uppercase; letter-spacing:.03em; color:var(--dim); }
tr:last-child td { border-bottom:0; }
tr:hover td { background:var(--row-hover); }
td.dim, .dim { color:var(--muted); }
.sub { display:block; font-size:.78rem; }
.badge { display:inline-block; padding:.05rem .5rem; border-radius:999px; font-size:.78rem; font-weight:600; }
.s-running { background:var(--run-bg); color:var(--run-fg); }
.s-stopped { background:var(--stop-bg); color:var(--stop-fg); }
.s-prov { background:var(--prov-bg); color:var(--prov-fg); }
.s-failed { background:var(--fail-bg); color:var(--fail-fg); }
.s-unknown { background:var(--stop-bg); color:var(--stop-fg); }
.s-archived { background:var(--sess-bg); color:var(--sess-fg); }
.badge.active { background:var(--sess-bg); color:var(--sess-fg); }
td.actions { display:flex; gap:.35rem; }
.actions form { display:inline; margin:0; }
button, .button-link { font:inherit; padding:.25rem .6rem; border:1px solid var(--btn-border);
         border-radius:6px; background:var(--btn-bg); cursor:pointer; color:var(--fg); display:inline-block; }
button:hover, .button-link:hover { border-color:var(--btn-hover); text-decoration:none; }
button.stop { color:var(--err-fg); border-color:var(--err-border); }
button.start { color:var(--ok-fg); border-color:var(--ok-border); }
button.reap { color:var(--sess-fg); border-color:var(--sess-fg); }
.log-controls { display:flex; align-items:center; gap:.5rem; flex-wrap:wrap; margin:.5rem 0 1rem; }
.log-search { display:flex; align-items:center; gap:.6rem; margin:.25rem 0 .75rem; }
.log-search input { flex:1; max-width:32rem; padding:.35rem .55rem; font:inherit;
         border:1px solid var(--btn-border); border-radius:6px; background:var(--btn-bg); color:var(--fg); }
.refresh-opt { padding:.15rem .5rem; border-radius:6px; border:1px solid transparent; }
a.refresh-opt:hover { border-color:var(--btn-hover); text-decoration:none; }
.refresh-opt.active { background:var(--btn-bg); border:1px solid var(--btn-border); color:var(--fg); }
.banner { padding:.55rem .8rem; border-radius:6px; margin-bottom:1rem; border:1px solid; }
.banner.ok { background:var(--ok-bg); border-color:var(--ok-border); color:var(--ok-fg); }
.banner.err { background:var(--err-bg); border-color:var(--err-border); color:var(--err-fg); }
dl.detail { display:grid; grid-template-columns:max-content 1fr; gap:.35rem 1rem;
            background:var(--card); border:1px solid var(--border); border-radius:8px; padding:1rem; max-width:680px; }
dl.detail dt { color:var(--muted); }
dl.detail dd { margin:0; }
dd.err, .err { color:var(--err-fg); }
section.controls { background:var(--card); border:1px solid var(--border); border-radius:8px;
                   padding:1rem; margin:1.2rem 0; max-width:680px; }
section.controls h2 { margin-top:0; }
.controls .actions { display:flex; gap:.5rem; margin-bottom:.8rem; }
form.resize { display:flex; align-items:center; gap:.4rem; }
select { font:inherit; padding:.2rem; background:var(--btn-bg); color:var(--fg);
         border:1px solid var(--btn-border); border-radius:6px; }
form.search { display:flex; align-items:center; gap:.4rem; margin:.8rem 0; }
form.search input[type=search] { flex:1; max-width:420px; font:inherit; padding:.3rem .6rem;
        background:var(--btn-bg); color:var(--fg); border:1px solid var(--btn-border); border-radius:6px; }
.pager { display:flex; align-items:center; gap:1rem; margin:1rem 0; color:var(--dim); }
.pills { display:flex; flex-wrap:wrap; gap:.4rem; margin:.6rem 0; }
.pill { display:inline-block; padding:.15rem .65rem; border-radius:999px; font-size:.85rem;
        border:1px solid var(--btn-border); background:var(--btn-bg); color:var(--fg); }
.pill:hover { border-color:var(--btn-hover); text-decoration:none; }
.pill.selected { background:var(--sess-bg); color:var(--sess-fg); border-color:var(--sess-fg); font-weight:600; }
.pill .count { color:var(--dim); font-weight:400; }
.pill.selected .count { color:var(--sess-fg); }
.note { color:var(--muted); font-size:.85rem; }
code { background:var(--code-bg); padding:.1rem .3rem; border-radius:4px; font-size:.85em; }
pre.log { background:var(--pre-bg); color:var(--pre-fg); padding:1rem; border-radius:8px; overflow-x:auto;
          font:12px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace; white-space:pre; max-height:70vh; }
h3.sub-head { font-size:.82rem; text-transform:uppercase; letter-spacing:.04em; color:var(--dim);
              margin:1.2rem 0 .5rem; }
.metrics { display:grid; grid-template-columns:repeat(auto-fill,minmax(240px,1fr)); gap:.8rem; margin:.6rem 0; }
.metric { background:var(--card); border:1px solid var(--border); border-radius:8px; padding:.8rem .9rem; }
.metric-head { display:flex; align-items:baseline; justify-content:space-between; gap:.5rem; }
.metric-label { font-weight:600; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.metric-val { font-variant-numeric:tabular-nums; font-weight:700; font-size:1.05rem; }
.meter { height:8px; background:var(--meter-track); border-radius:999px; overflow:hidden; margin:.5rem 0 .4rem; }
.meter-fill { height:100%; border-radius:999px; transition:width .2s ease; }
.meter-fill.ok { background:var(--meter-ok); }
.meter-fill.warn { background:var(--meter-warn); }
.meter-fill.crit { background:var(--meter-crit); }
.metric-sub { color:var(--muted); font-size:.8rem; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.stats { display:grid; grid-template-columns:repeat(auto-fill,minmax(150px,1fr)); gap:.8rem; margin:.6rem 0; }
.stat { background:var(--card); border:1px solid var(--border); border-radius:8px; padding:.8rem .9rem; }
.stat-val { font-size:1.5rem; font-weight:700; font-variant-numeric:tabular-nums; }
.stat-label { color:var(--dim); font-size:.82rem; margin-top:.15rem; }
.stat-sub { color:var(--muted); font-size:.75rem; margin-top:.2rem; }
table.alerts { margin:.4rem 0 .8rem; }
table.alerts td code { white-space:normal; word-break:break-all; }
table.alerts input.threshold { width:4.5rem; }
table.alerts input.webhook { width:100%; min-width:16rem; box-sizing:border-box; }
table.alerts td:nth-child(3) { width:40%; }
form.alert-add { display:flex; flex-wrap:wrap; align-items:end; gap:.6rem 1rem; margin:.5rem 0; }
form.alert-add label { display:flex; flex-direction:column; gap:.2rem; font-size:.82rem; color:var(--dim); }
form.alert-add label.grow { flex:1; min-width:220px; }
form.alert-add input, form.alert-add select { font:inherit; padding:.3rem .5rem; color:var(--fg);
        background:var(--btn-bg); border:1px solid var(--btn-border); border-radius:6px; }
form.alert-add input[type=number] { width:6rem; }
"#;
