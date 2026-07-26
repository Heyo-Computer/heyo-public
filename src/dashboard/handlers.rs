//! Request handlers. Reads render maud pages; POST actions drive the SDK and
//! use Post-Redirect-Get so a refresh/back doesn't re-fire the action.

use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::response::Redirect;
use heyo_sdk::Sandbox;
use maud::Markup;
use serde::Deserialize;

use crate::config::parse_size_class;
use crate::vm;

use super::error::AppError;
use super::state::DashState;
use super::{host, logs, model, views};

/// Lifecycle actions can be slow (a cold boot re-runs init.sh), so give them a
/// generous bound — but still bound them, so one wedged VM can't hang a request.
const ACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// One-shot status banner carried across a redirect (`?msg=` / `?err=`).
#[derive(Deserialize)]
pub struct Banner {
    pub msg: Option<String>,
    pub err: Option<String>,
}

/// Query params for the Databases list. All optional so `/` bare works; junk
/// numeric input is a 400 from the extractor, which is fine for an admin tool.
#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub q: String,
    /// State filter; defaults to showing only running sandboxes.
    pub state: Option<String>,
    pub page: Option<usize>,
    pub per: Option<usize>,
}

/// The main Databases view: a paged, searchable list of every daemon sandbox.
/// Lazy by construction: one bounded daemon read, filtering happens
/// in-process, and only the visible page is joined with pooler state and
/// rendered. No guest access.
pub async fn databases(
    State(st): State<DashState>,
    Query(p): Query<ListParams>,
) -> Result<Markup, AppError> {
    let per = p.per.unwrap_or(model::DEFAULT_PER).clamp(1, model::MAX_PER);
    let page = p.page.unwrap_or(1);
    let state = p.state.as_deref().unwrap_or(model::DEFAULT_STATE);
    let pg = model::build_page(&st, &p.q, state, page, per).await?;
    Ok(views::databases_page(&st, &pg))
}

/// Host + fleet monitoring: whole-machine CPU/memory (heyvmd's sampler) and disk
/// saturation (read from the host directly), plus pooler-fleet aggregates rolled
/// up from the same VM inventory the Databases view uses. No guest access. Host
/// CPU/mem and disk are each best-effort and degrade to an "unavailable" note
/// independently, so a stale poller or a `df` hiccup never fails the page.
pub async fn monitoring(
    State(st): State<DashState>,
    Query(banner): Query<Banner>,
) -> Result<Markup, AppError> {
    let (rows, host_usage, disks) = tokio::join!(
        model::build_rows(&st),
        model::fetch_host_usage(&st),
        host::host_disks(),
    );
    let rows = rows?;
    let disks = disks.unwrap_or_else(|e| {
        tracing::warn!("reading host disks failed (hiding disk): {e:#}");
        Vec::new()
    });
    let history = st.history.snapshot();
    // Per-hour event series for the activity charts (trailing 24 clock hours).
    let activity = views::ActivitySeries {
        restores_s3: crate::events::hourly_counts(crate::events::Event::RestoreS3, 24),
        restores_local: crate::events::hourly_counts(crate::events::Event::RestoreLocal, 24),
        vms_created: crate::events::hourly_counts(crate::events::Event::VmCreated, 24),
    };
    Ok(views::monitoring_page(
        &st,
        &rows,
        host_usage.as_ref(),
        &disks,
        &history,
        &activity,
        &banner,
    ))
}

/// Recent pooler events (failures + sweep summaries) from the journal —
/// newest first, in-memory + reloaded from the daily partition files on
/// startup. Read-only; no daemon or guest access.
pub async fn events(State(st): State<DashState>) -> Markup {
    let entries = crate::events::journal_recent(200);
    views::events_page(&st, &entries)
}

/// Form for adding a webhook alert rule from the monitoring page.
#[derive(Deserialize)]
pub struct AlertForm {
    pub metric: String,
    pub threshold_pct: f64,
    pub webhook_url: String,
}

pub async fn alert_add(State(st): State<DashState>, Form(form): Form<AlertForm>) -> Redirect {
    match st
        .alerts
        .add(&form.metric, form.threshold_pct, &form.webhook_url)
    {
        Ok(()) => Redirect::to(&format!("/monitoring?msg={}", qenc("alert rule added"))),
        Err(e) => Redirect::to(&format!("/monitoring?err={}", qenc(&e.to_string()))),
    }
}

/// Force an S3 eviction sweep now instead of waiting for the periodic timer.
/// Runs in the background (a big backlog can take a while) and redirects
/// immediately — watch the pooler log and the VMs' status for progress.
pub async fn action_sweep_now(State(st): State<DashState>) -> Redirect {
    match st.registry.spawn_sweep_now() {
        Ok(()) => Redirect::to(&format!(
            "/monitoring?msg={}",
            qenc("eviction sweep started; watch the pooler log")
        )),
        Err(e) => Redirect::to(&format!("/monitoring?err={}", qenc(&e.to_string()))),
    }
}

/// Run a disk-reclaim pass now instead of waiting for the periodic timer:
/// offline-trim every stopped VM's data disk so its stranded free space returns
/// to the host. Runs in the background and redirects immediately — the outcome
/// lands in the pooler log.
pub async fn action_reclaim_now(State(st): State<DashState>) -> Redirect {
    match st.registry.spawn_reclaim_now() {
        Ok(()) => Redirect::to(&format!(
            "/monitoring?msg={}",
            qenc("disk reclaim started; watch the pooler log")
        )),
        Err(e) => Redirect::to(&format!("/monitoring?err={}", qenc(&e.to_string()))),
    }
}

pub async fn alert_delete(State(st): State<DashState>, Path(id): Path<String>) -> Redirect {
    let msg = if st.alerts.remove(&id) {
        format!("?msg={}", qenc("alert rule removed"))
    } else {
        format!("?err={}", qenc("no such alert rule"))
    };
    Redirect::to(&format!("/monitoring{msg}"))
}

pub async fn vm_detail(
    State(st): State<DashState>,
    Path(id): Path<String>,
    Query(banner): Query<Banner>,
) -> Result<Markup, AppError> {
    let Some(row) = model::find_row(&st, &id).await? else {
        return Ok(views::not_found_page(&id));
    };
    // Live DB usage and guest-OS stats over the pooler's warm PG pool (safe
    // TCP path, no guest-console access) — only meaningful for a warm,
    // pooler-managed VM. Fetched concurrently; each is independently bounded.
    let (db, guest) = match &row.schema {
        Some(schema) if row.is_running() => tokio::join!(
            st.registry.db_stats(&id, schema),
            st.registry.guest_stats(&id),
        ),
        _ => (None, None),
    };
    Ok(views::vm_detail_page(&st, &row, db.as_ref(), guest.as_ref(), &banner))
}

/// Auto-refresh control for the log pages: `?refresh=<secs>` (absent/0 = off).
/// The view clamps the honored value; the extractor just carries it.
#[derive(Deserialize)]
pub struct RefreshParams {
    pub refresh: Option<u64>,
}

pub async fn logs_pooler(
    State(st): State<DashState>,
    Query(p): Query<RefreshParams>,
) -> Result<Markup, AppError> {
    let text = logs::tail_file(&st.cfg.pooler_log, st.cfg.log_lines).await?;
    let src = st.cfg.pooler_log.display().to_string();
    Ok(views::log_page("pooler", "/logs/pooler", &src, &text, p.refresh))
}

pub async fn logs_heyvmd(
    State(st): State<DashState>,
    Query(p): Query<RefreshParams>,
) -> Result<Markup, AppError> {
    let text = logs::tail_file(&st.cfg.heyvmd_log, st.cfg.log_lines).await?;
    let src = st.cfg.heyvmd_log.display().to_string();
    Ok(views::log_page("heyvmd", "/logs/heyvmd", &src, &text, p.refresh))
}

pub async fn logs_vm(
    State(st): State<DashState>,
    Path(id): Path<String>,
    Query(p): Query<RefreshParams>,
) -> Result<Markup, AppError> {
    let text = logs::tail_vm_log(&id, st.cfg.log_lines).await?;
    let title = format!("vm {id}");
    let src = format!("{id}:/workspace/pgdata/log/postgresql-*.log");
    Ok(views::log_page(&title, &format!("/logs/vm/{id}"), &src, &text, p.refresh))
}

// ---- control actions -------------------------------------------------------

enum Lifecycle {
    Start,
    Stop,
    Reboot,
}

async fn run_lifecycle(id: &str, act: Lifecycle) -> anyhow::Result<()> {
    use anyhow::Context;
    let sb = Sandbox::connect(id.to_string(), vm::local_opts()).context("connecting to VM")?;
    let fut = async move {
        match act {
            // Start/reboot open a stopped disk — mutually exclusive with
            // reclaim passes (see reclaim::boot_permit).
            Lifecycle::Start => {
                let _permit = crate::reclaim::boot_permit().await;
                sb.start().await
            }
            Lifecycle::Stop => sb.stop().await,
            Lifecycle::Reboot => {
                let _permit = crate::reclaim::boot_permit().await;
                sb.restart().await
            }
        }
    };
    tokio::time::timeout(ACTION_TIMEOUT, fut)
        .await
        .context("action timed out")??;
    Ok(())
}

fn redirect(id: &str, result: anyhow::Result<()>, ok_msg: &str) -> Redirect {
    match result {
        Ok(()) => Redirect::to(&format!("/vm/{id}?msg={}", qenc(ok_msg))),
        Err(e) => Redirect::to(&format!("/vm/{id}?err={}", qenc(&e.to_string()))),
    }
}

pub async fn action_start(Path(id): Path<String>) -> Redirect {
    let r = run_lifecycle(&id, Lifecycle::Start).await;
    redirect(&id, r, "started")
}

pub async fn action_stop(Path(id): Path<String>) -> Redirect {
    let r = run_lifecycle(&id, Lifecycle::Stop).await;
    redirect(&id, r, "stopped")
}

pub async fn action_reboot(Path(id): Path<String>) -> Redirect {
    let r = run_lifecycle(&id, Lifecycle::Reboot).await;
    redirect(&id, r, "rebooting")
}

#[derive(Deserialize)]
pub struct ResizeForm {
    pub size_class: String,
}

pub async fn action_resize(Path(id): Path<String>, Form(form): Form<ResizeForm>) -> Redirect {
    use anyhow::Context;
    let size = match parse_size_class(&form.size_class) {
        Ok(s) => s,
        Err(_) => return Redirect::to(&format!("/vm/{id}?err={}", qenc("invalid size class"))),
    };
    let result = async {
        let sb =
            Sandbox::connect(id.clone(), vm::local_opts()).context("connecting to VM")?;
        // Firecracker may reject a resize on a running VM; surface that verbatim
        // rather than appearing to succeed.
        tokio::time::timeout(ACTION_TIMEOUT, sb.resize(size))
            .await
            .context("resize timed out")??;
        anyhow::Ok(())
    }
    .await;
    redirect(&id, result, &format!("resized to {}", size.as_str()))
}

/// Manually reap a pool-managed VM to S3: dump its database and kill the VM to
/// reclaim its disk. The next client connection restores it transparently. The
/// dump+kill can take minutes for a large database, so it runs in the background
/// and this redirects immediately — watch the pooler log (and the VM's status,
/// which flips to "Archived (S3)") for the outcome.
pub async fn action_reap(State(st): State<DashState>, Path(id): Path<String>) -> Redirect {
    if !st.registry.archive_enabled() {
        return Redirect::to(&format!(
            "/vm/{id}?err={}",
            qenc("S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*)")
        ));
    }
    let schema = match model::find_row(&st, &id).await {
        Ok(Some(row)) => row.schema,
        Ok(None) => None,
        Err(e) => return Redirect::to(&format!("/vm/{id}?err={}", qenc(&e.to_string()))),
    };
    let Some(schema) = schema else {
        return Redirect::to(&format!(
            "/vm/{id}?err={}",
            qenc("not a pooler-managed schema VM — nothing to archive")
        ));
    };
    let registry = st.registry.clone();
    tokio::spawn(async move {
        if let Err(e) = registry.archive_schema(&schema).await {
            tracing::warn!("manual reap of schema {schema} to S3 failed: {e:#}");
        }
    });
    Redirect::to(&format!(
        "/vm/{id}?msg={}",
        qenc("reap to S3 started; watch the pooler log")
    ))
}

/// Minimal query-value encoder: keep readable chars, map space→`+`, drop the
/// rest, and cap length. `+` decodes back to a space via `Query`/serde_urlencoded.
fn qenc(s: &str) -> String {
    s.chars()
        .map(|c| if c == ' ' { '+' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || "+.,:_-()".contains(*c))
        .take(200)
        .collect()
}
