use crate::agent::TokenUsage;
use crate::cli::{ExecArgs, HistoryArgs, ReviewArgs, RunArgs};
use crate::codegraph_watch;
use crate::drivers::{self, ActiveSandbox, DriverSet, shell_quote_argv};
use crate::hooks::{Event, HookContext, HookSet};
use crate::tasks::model::Task;
use crate::tasks::store::{self, compute_ready};
use crate::{review, run};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Per-spec checkpoint root. Each spec gets its own
/// `.printer/exec/<key>.json`; running two specs concurrently in the same
/// cwd no longer collides. Older builds wrote a single file at
/// `.printer/exec.json` — see `legacy_checkpoint_path`. Stale legacy files
/// are detected and surfaced as a one-time hint, not migrated automatically.
const CHECKPOINT_DIR_REL: &str = ".printer/exec";
const LEGACY_CHECKPOINT_REL: &str = ".printer/exec.json";
const HISTORY_REL: &str = ".printer/history.json";
/// Default upper bound on review cycles (one initial review + N-1 follow-ups).
/// Each non-PASS verdict triggers a fix pass and another review, so this caps
/// how many round-trips we'll attempt before giving up.
pub const DEFAULT_MAX_REVIEW_PASSES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Planning pass has not yet finished. Set on `Action::Fresh` /
    /// `Action::FreshAfterDone` before any agent work, and cleared by
    /// `run.rs` after `planning_pass` completes — so a `--continue` from
    /// this state re-runs planning (planning may have crashed mid-way and
    /// re-running it is cheap + idempotent).
    Planning,
    /// Planning is done; the implementation loop is in progress (or
    /// crashed mid-loop). Resuming from this phase skips `planning_pass`
    /// entirely and goes straight to the exec loop.
    Running,
    /// Run completed cleanly; review has not started yet.
    ReviewPending,
    /// Review report produced; nothing left to do.
    Done,
    /// Spec was explicitly retired via `printer spec cancel` without
    /// finishing review. Treated like `Done` for resume/already-finished
    /// purposes (a bare re-`exec` archives + restarts), but kept distinct
    /// so history and listings show it was cancelled, not completed.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub spec: PathBuf,
    pub phase: Phase,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One archived exec — a Checkpoint plus the time it was retired from
/// `.printer/exec.json` (i.e. a follow-up spec replaced it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(flatten)]
    pub checkpoint: Checkpoint,
    pub archived_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    pub entries: Vec<HistoryEntry>,
}

impl History {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading history {}", path.display()))?;
        let history: History = serde_json::from_str(&raw)
            .with_context(|| format!("parsing history {}", path.display()))?;
        Ok(history)
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self).context("serializing history")?;
        std::fs::write(path, body)
            .with_context(|| format!("writing history {}", path.display()))?;
        Ok(())
    }

    fn append(path: &Path, entry: HistoryEntry) -> Result<()> {
        let mut h = Self::load(path)?;
        h.entries.push(entry);
        h.save(path)
    }
}

pub fn history_path(cwd: &Path) -> PathBuf {
    cwd.join(HISTORY_REL)
}

pub fn load_history(cwd: &Path) -> Result<History> {
    History::load(&history_path(cwd))
}

pub fn print_history(args: HistoryArgs) -> Result<()> {
    let cwd: PathBuf = match args.cwd.as_deref() {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--cwd not found: {}", p.display()))?,
        None => std::env::current_dir()?,
    };

    let history = load_history(&cwd)?;
    let current = list_checkpoints(&cwd)?;

    if args.json {
        #[derive(Serialize)]
        struct CurrentEntry<'a> {
            path: String,
            #[serde(flatten)]
            checkpoint: &'a Checkpoint,
        }
        #[derive(Serialize)]
        struct Out<'a> {
            current: Vec<CurrentEntry<'a>>,
            history: &'a History,
        }
        let out = Out {
            current: current
                .iter()
                .map(|(p, cp)| CurrentEntry {
                    path: p.display().to_string(),
                    checkpoint: cp,
                })
                .collect(),
            history: &history,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if history.entries.is_empty() && current.is_empty() {
        println!("No exec history at {}.", history_path(&cwd).display());
        return Ok(());
    }

    if !history.entries.is_empty() {
        println!("Archived execs ({}):", history.entries.len());
        for (i, e) in history.entries.iter().enumerate() {
            println!(
                "  [{}] {}  phase={:?}  started={}  finished={}  archived={}",
                i + 1,
                e.checkpoint.spec.display(),
                e.checkpoint.phase,
                e.checkpoint.started_at.to_rfc3339(),
                e.checkpoint.updated_at.to_rfc3339(),
                e.archived_at.to_rfc3339(),
            );
        }
    }

    if !current.is_empty() {
        println!("\nCurrent checkpoints ({}):", current.len());
        for (path, cp) in &current {
            println!(
                "  {}: {}  phase={:?}  started={}  updated={}",
                path.display(),
                cp.spec.display(),
                cp.phase,
                cp.started_at.to_rfc3339(),
                cp.updated_at.to_rfc3339(),
            );
        }
    }

    Ok(())
}

impl Checkpoint {
    fn new(spec: PathBuf, phase: Phase) -> Self {
        let now = Utc::now();
        Self {
            spec,
            phase,
            started_at: now,
            updated_at: now,
        }
    }

    fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading checkpoint {}", path.display()))?;
        let cp: Checkpoint = serde_json::from_str(&raw)
            .with_context(|| format!("parsing checkpoint {}", path.display()))?;
        Ok(cp)
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(self).context("serializing checkpoint")?;
        std::fs::write(path, body)
            .with_context(|| format!("writing checkpoint {}", path.display()))?;
        Ok(())
    }
}

/// Per-cwd directory holding one checkpoint file per in-flight spec.
pub fn checkpoint_dir(cwd: &Path) -> PathBuf {
    cwd.join(CHECKPOINT_DIR_REL)
}

/// Pre-rename location of the single-checkpoint file. Read-only here:
/// the new code never writes it, but we surface a hint when one exists
/// so users don't get confused by an orphaned legacy file alongside the
/// new per-spec layout.
fn legacy_checkpoint_path(cwd: &Path) -> PathBuf {
    cwd.join(LEGACY_CHECKPOINT_REL)
}

/// Stable filesystem-safe key per spec. Combines the human-readable slug
/// (so `ls .printer/exec/` is meaningful) with an 8-hex-char hash of the
/// canonical path (so two specs that share a basename — `a/spec.md` and
/// `b/spec.md` — don't collide). Hash uses the std `DefaultHasher`,
/// which is non-cryptographic but deterministic within a Rust toolchain;
/// a Rust upgrade may invalidate in-flight checkpoints, which is fine —
/// they're per-run state, not durable artifacts.
pub fn checkpoint_key_for_spec(spec: &Path) -> String {
    let slug = drivers::make_spec_slug(spec);
    let mut h = DefaultHasher::new();
    spec.to_string_lossy().hash(&mut h);
    let hash32 = (h.finish() & 0xFFFF_FFFF) as u32;
    format!("{slug}-{hash32:08x}")
}

pub fn checkpoint_path_for_spec(cwd: &Path, spec: &Path) -> PathBuf {
    checkpoint_dir(cwd).join(format!("{}.json", checkpoint_key_for_spec(spec)))
}

/// Load every current checkpoint under `cwd`. Returns `(file_path,
/// Checkpoint)` pairs sorted by checkpoint `started_at` ascending so
/// "the only one" picks have a stable order. Empty (and `Ok`) when the
/// dir is missing — first-run on a clean repo.
pub fn list_checkpoints(cwd: &Path) -> Result<Vec<(PathBuf, Checkpoint)>> {
    let dir = checkpoint_dir(cwd);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<(PathBuf, Checkpoint)> = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match Checkpoint::load(&path) {
            Ok(cp) => out.push((path, cp)),
            Err(e) => eprintln!(
                "[printer] warning: skipping unreadable checkpoint at {}: {e}",
                path.display()
            ),
        }
    }
    out.sort_by_key(|a| a.1.started_at);
    Ok(out)
}

/// One-time hint when an old `.printer/exec.json` is sitting next to
/// the new `.printer/exec/` dir. Doesn't block; just tells the user
/// what to do with it.
fn warn_about_legacy_checkpoint(cwd: &Path) {
    let legacy = legacy_checkpoint_path(cwd);
    if legacy.is_file() {
        eprintln!(
            "[printer] note: legacy checkpoint at {} is no longer used \
             (per-spec checkpoints now live under {}/). \
             Safe to delete: rm {}",
            legacy.display(),
            CHECKPOINT_DIR_REL,
            legacy.display()
        );
    }
}

/// Decision the dispatcher makes after consulting the checkpoint(s) and
/// flags. Pulled out so it can be unit-tested without spawning agents.
/// `skip_planning` is true on resume from `Phase::Running` (planning
/// pass already completed) and false otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// No checkpoint or fresh start; create one at `spec` and run+review
    /// from the planning pass on.
    Fresh { spec: PathBuf },
    /// Resume the run phase. `skip_planning=true` when the checkpoint was
    /// already at `Phase::Running` (planning done); false when at
    /// `Phase::Planning` (planning needs to redo).
    ResumeRun { spec: PathBuf, skip_planning: bool },
    /// Skip run entirely, go straight to review.
    ResumeReview { spec: PathBuf },
    /// Already done; nothing to do.
    AlreadyDone { spec: PathBuf },
    /// Prior exec for the *same* spec finished cleanly; archive it to
    /// history and start a fresh exec for that spec. (Different specs
    /// no longer collide — they each have their own checkpoint file —
    /// so "fresh after done" only applies to re-running a spec whose
    /// checkpoint is at Phase::Done.)
    FreshAfterDone { spec: PathBuf, prior: Checkpoint },
}

/// Resolve the (action, checkpoint-path-on-disk) pair from CLI args plus
/// whatever checkpoints already exist on disk under `cwd`. Pure-ish:
/// `list_checkpoints` is a directory read but no agents spawn here.
fn decide(cwd: &Path, cli_spec: Option<&Path>, cont: bool) -> Result<(Action, PathBuf)> {
    let all = list_checkpoints(cwd)?;

    match (cont, cli_spec) {
        (true, Some(spec)) => {
            // --continue with explicit spec: load that spec's checkpoint
            // (if any) by key. If the user typo'd a spec that has no
            // checkpoint, list what we have so they can recover.
            let path = checkpoint_path_for_spec(cwd, spec);
            let cp = if path.is_file() {
                Some(Checkpoint::load(&path)?)
            } else {
                None
            };
            match cp {
                Some(cp) => Ok((action_from_phase(&cp), path)),
                None => bail!(
                    "--continue {} but no checkpoint at {} for that spec. {}",
                    spec.display(),
                    path.display(),
                    summarize_checkpoints(&all)
                ),
            }
        }
        (true, None) => {
            // --continue without a spec: pick the only in-flight
            // checkpoint, error if zero or many.
            let in_flight: Vec<_> = all
                .iter()
                .filter(|(_, cp)| !matches!(cp.phase, Phase::Done | Phase::Cancelled))
                .collect();
            match in_flight.len() {
                0 => bail!(
                    "--continue requested but no in-flight checkpoint under {}/. {}",
                    CHECKPOINT_DIR_REL,
                    summarize_checkpoints(&all)
                ),
                1 => {
                    let (path, cp) = in_flight[0];
                    Ok((action_from_phase(cp), path.clone()))
                }
                n => bail!(
                    "--continue requested but {n} in-flight checkpoints exist; \
                     pass `--continue <spec>` to disambiguate. \
                     {}",
                    summarize_checkpoints(&all)
                ),
            }
        }
        (false, Some(spec)) => {
            let path = checkpoint_path_for_spec(cwd, spec);
            let cp = if path.is_file() {
                Some(Checkpoint::load(&path)?)
            } else {
                None
            };
            match cp {
                None => Ok((
                    Action::Fresh {
                        spec: spec.to_path_buf(),
                    },
                    path,
                )),
                Some(cp) if matches!(cp.phase, Phase::Done | Phase::Cancelled) => {
                    // Re-running a finished (or cancelled) spec: archive + restart.
                    Ok((
                        Action::FreshAfterDone {
                            spec: spec.to_path_buf(),
                            prior: cp,
                        },
                        path,
                    ))
                }
                Some(cp) => {
                    // Spec has an in-flight checkpoint. Resume from
                    // recorded phase — bare `printer exec spec.md` after
                    // a crash should do the right thing without --continue.
                    Ok((action_from_phase(&cp), path))
                }
            }
        }
        (false, None) => bail!("missing spec path (required unless --continue is set)"),
    }
}

/// Map a checkpoint's `Phase` to the action we'd take to resume from it.
fn action_from_phase(cp: &Checkpoint) -> Action {
    match cp.phase {
        Phase::Planning => Action::ResumeRun {
            spec: cp.spec.clone(),
            skip_planning: false,
        },
        Phase::Running => Action::ResumeRun {
            spec: cp.spec.clone(),
            skip_planning: true,
        },
        Phase::ReviewPending => Action::ResumeReview {
            spec: cp.spec.clone(),
        },
        Phase::Done | Phase::Cancelled => Action::AlreadyDone {
            spec: cp.spec.clone(),
        },
    }
}

/// Render a list of existing checkpoints for inclusion in error messages
/// — helps the user pick the right `--continue <spec>` when there's
/// ambiguity (or none, in which case we tell them so).
fn summarize_checkpoints(all: &[(PathBuf, Checkpoint)]) -> String {
    if all.is_empty() {
        return "(no checkpoints found)".to_string();
    }
    let mut s = String::from("Existing checkpoints:");
    for (_, cp) in all {
        s.push_str(&format!(
            "\n  {} (phase={:?}, started={})",
            cp.spec.display(),
            cp.phase,
            cp.started_at.to_rfc3339(),
        ));
    }
    s
}

pub async fn exec(args: ExecArgs) -> Result<()> {
    crate::plugins::prompt_if_no_plugins(args.skip_plugin_check)?;
    let cwd: PathBuf = match args.cwd.as_deref() {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--cwd not found: {}", p.display()))?,
        None => std::env::current_dir()?,
    };
    warn_about_legacy_checkpoint(&cwd);

    if args.recursive {
        return exec_recursive(args, cwd).await;
    }

    let cli_spec_abs: Option<PathBuf> = match args.spec.as_deref() {
        Some(p) => Some(
            p.canonicalize()
                .with_context(|| format!("spec file not found: {}", p.display()))?,
        ),
        None => None,
    };

    let (action, checkpoint_path) = decide(&cwd, cli_spec_abs.as_deref(), args.r#continue)?;
    // We need the existing checkpoint for ResumeRun's bookkeeping (bumping
    // updated_at). Re-load from the resolved path — it's the same file
    // `decide` looked at.
    let existing = if checkpoint_path.is_file() {
        Some(Checkpoint::load(&checkpoint_path)?)
    } else {
        None
    };

    // Spawn the codegraph watch daemon at the exec level so a single daemon
    // covers both run and review. The inner run is configured (via
    // build_run_args) with no_codegraph_watch=true so it won't double-spawn.
    let _watch_guard = if args.no_codegraph_watch {
        None
    } else {
        codegraph_watch::try_spawn(&cwd).unwrap_or_else(|e| {
            eprintln!("[printer] codegraph watch spawn failed: {e}; continuing without daemon");
            None
        })
    };

    let hooks = HookSet::load_installed().unwrap_or_default();
    let exec_spec: Option<PathBuf> = match &action {
        Action::Fresh { spec }
        | Action::ResumeRun { spec, .. }
        | Action::ResumeReview { spec }
        | Action::AlreadyDone { spec }
        | Action::FreshAfterDone { spec, .. } => Some(spec.clone()),
    };

    // Provision the sandbox once at the exec level so a single VM covers both
    // run and review. The inner phases are passed `no_sandbox=true` (via
    // build_run_args / build_review_args) so they won't try to create their
    // own; instead they receive a borrowed reference to this one.
    let sandbox = if args.no_sandbox {
        None
    } else {
        acquire_exec_sandbox(&cwd, exec_spec.clone(), &args.agent, None)?
    };
    if let Some(sb) = sandbox.as_ref() {
        sb.sync_in()?;
    }

    {
        let mut ctx = HookContext::new(Event::BeforeExec, cwd.clone());
        if let Some(s) = &exec_spec {
            ctx = ctx.with_spec(s.clone());
        }
        hooks.run_cli(Event::BeforeExec, &ctx)?;
    }

    let run_result = run_action(
        &args,
        &cwd,
        &checkpoint_path,
        action,
        existing,
        sandbox.as_ref(),
    )
    .await;
    let run_success = run_result.is_ok();
    if let Some(sb) = sandbox.as_ref() {
        sb.sync_out();
    }

    {
        let mut ctx = HookContext::new(Event::AfterExec, cwd.clone()).with_exit_status(run_success);
        if let Some(s) = &exec_spec {
            ctx = ctx.with_spec(s.clone());
        }
        let _ = hooks.run_cli(Event::AfterExec, &ctx);
    }

    run_result.map(|_| ())
}

/// Run `printer exec` for each ready task, spawning a sandbox for each.
/// Ready tasks are open tasks whose dependencies are satisfied, sorted by
/// priority (highest first) then by id.
async fn exec_recursive(args: ExecArgs, cwd: PathBuf) -> Result<()> {
    // Load all tasks and compute the ready queue
    let tasks_dir = store::tasks_dir(None)?;
    let all_tasks = store::list_all(&tasks_dir)?;
    let ready_tasks = compute_ready(&all_tasks);

    if ready_tasks.is_empty() {
        eprintln!("[printer] no ready tasks to run in recursive mode");
        return Ok(());
    }

    eprintln!(
        "[printer] recursive exec: {} ready task(s)",
        ready_tasks.len()
    );

    for task in ready_tasks {
        eprintln!("[printer] processing task: {}", task.meta.id);
        exec_for_task(&args, &cwd, task).await?;
    }

    Ok(())
}

/// Execute a single task by spawning a printer subprocess in its worktree sandbox.
/// This provides process isolation per task as required by the spec.
async fn exec_for_task(args: &ExecArgs, cwd: &Path, task: &Task) -> Result<TokenUsage> {
    use tokio::process::Command as TokioCommand;

    let worktree_path = cwd.join(".printer").join("worktrees").join(&task.meta.id);
    let worktree_abs = worktree_path
        .canonicalize()
        .unwrap_or_else(|_| worktree_path.clone());

    // Check if we have a git repo for stacked PR pattern
    let has_git = std::process::Command::new("sh")
        .arg("-c")
        .arg("git rev-parse --git-dir 2>/dev/null")
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Create worktree via git worktree add (or mkdir fallback)
    ensure_worktree(cwd, &worktree_path, &task.meta.id, has_git)?;

    // Create the task spec file first - needed for sandbox creation
    let task_spec_path = worktree_abs.join(format!("{}.md", task.meta.id));
    let task_spec_content = create_task_spec_content(task);
    std::fs::write(&task_spec_path, &task_spec_content)
        .with_context(|| format!("writing task spec at {}", task_spec_path.display()))?;

    // Acquire a sandbox for this task using the heyvm driver
    // This creates an isolated sandbox per task as required by the spec
    let sandbox = if args.no_sandbox {
        None
    } else {
        acquire_exec_sandbox(
            &worktree_abs,
            Some(task_spec_path.clone()),
            &args.agent,
            Some(task.meta.id.clone()),
        )
        .unwrap_or_else(|e| {
            eprintln!(
                "[printer] failed to acquire sandbox for task {}: {e}; continuing without sandbox",
                task.meta.id
            );
            None
        })
    };

    // Spawn a codegraph watch instance in the worktree directory
    // This ensures the codegraph index picks up changes for each agent
    let codegraph_guard = if !args.no_codegraph_watch {
        codegraph_watch::try_spawn(&worktree_abs).unwrap_or_else(|e| {
            eprintln!("[printer] codegraph watch spawn failed for task {}: {e}; continuing without daemon", task.meta.id);
            None
        })
    } else {
        None
    };

    // Spawn a subprocess to run printer for this task
    // This provides process isolation per task as required by the spec
    let printer_bin =
        std::env::current_exe().context("resolving printer binary path for subprocess")?;

    let mut cmd = TokioCommand::new(&printer_bin);
    cmd.args(["exec", task_spec_path.to_str().unwrap_or(".")]);
    cmd.arg("--cwd").arg(&worktree_abs);

    if args.verbose {
        cmd.arg("--verbose");
    }
    // Always disable inner codegraph-watch; we already have one for this worktree
    cmd.arg("--no-codegraph-watch");
    // Always disable inner sandbox; we already have one for this task
    cmd.arg("--no-sandbox");
    if args.skip_plugin_check {
        cmd.arg("--skip-plugin-check");
    }

    // Wrap the subprocess through the sandbox if we have one
    if let Some(sb) = sandbox.as_ref() {
        // Sync in before running
        sb.sync_in()?;

        // Build the command with sandbox wrapper
        let enter_template = sb.enter_template();
        let mut task_argv = vec![
            printer_bin.to_string_lossy().to_string(),
            "exec".to_string(),
            task_spec_path.to_string_lossy().to_string(),
            "--cwd".to_string(),
            worktree_abs.to_string_lossy().to_string(),
        ];
        if args.verbose {
            task_argv.push("--verbose".to_string());
        }
        task_argv.push("--no-codegraph-watch".to_string());
        task_argv.push("--no-sandbox".to_string());
        if args.skip_plugin_check {
            task_argv.push("--skip-plugin-check".to_string());
        }

        let child_cmd = shell_quote_argv(&task_argv);
        let wrapped = enter_template.replace("{child}", &child_cmd);

        eprintln!(
            "[printer] spawning agent subprocess for task {} in {} (sandboxed)",
            task.meta.id,
            worktree_abs.display()
        );

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&wrapped)
            .current_dir(&worktree_abs)
            .output()
            .with_context(|| {
                format!(
                    "spawning sandboxed printer subprocess for task {}",
                    task.meta.id
                )
            })?;

        if !output.status.success() {
            eprintln!(
                "[printer] task {} subprocess failed: {}",
                task.meta.id,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Sync out after the run
        sb.sync_out();
    } else {
        eprintln!(
            "[printer] spawning agent subprocess for task {} in {}",
            task.meta.id,
            worktree_abs.display()
        );

        let output = cmd
            .output()
            .await
            .with_context(|| format!("spawning printer subprocess for task {}", task.meta.id))?;

        if !output.status.success() {
            eprintln!(
                "[printer] task {} subprocess failed: {}",
                task.meta.id,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // Drop the codegraph guard to stop the watch daemon before committing
    drop(codegraph_guard);

    // Post-task: commit changes to the task-specific branch. This MUST happen
    // before the worktree is removed — the commit runs inside the worktree, so
    // removing it first would leave nothing to commit (the previous ordering
    // silently lost every task's work via the `|| true` fallback). Supports the
    // stacked-PR pattern: each task lands on its own `task-<id>` branch which
    // can later be merged/squashed.
    if has_git && worktree_abs.exists() {
        // Stage everything including new files (`commit --all` would miss
        // untracked files the agent created), then commit.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree_abs)
            .args(["add", "-A"])
            .output();
        let commit_result = std::process::Command::new("git")
            .arg("-C")
            .arg(&worktree_abs)
            .arg("commit")
            .arg("-m")
            .arg(format!("T-{}: {}", task.meta.id, task.meta.title))
            .output();

        match commit_result {
            Ok(output) if output.status.success() => {
                eprintln!("[printer] task {} committed to task branch", task.meta.id);
            }
            Ok(output) => {
                // Non-zero is expected when there is nothing to commit; surface
                // anything else so a real failure isn't hidden.
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("nothing to commit") {
                    eprintln!(
                        "[printer] task {} commit produced no commit: {}",
                        task.meta.id,
                        stderr.trim()
                    );
                }
            }
            Err(e) => eprintln!("[printer] task {} commit failed to run: {e}", task.meta.id),
        }
    }

    // Clean up the worktree after committing (removes working tree, keeps branch).
    if has_git && worktree_path.exists() {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("worktree")
            .arg("remove")
            .arg("-f")
            .arg(&worktree_path)
            .output();
    }

    eprintln!("[printer] task {} complete", task.meta.id);
    Ok(TokenUsage::default())
}

/// Create the spec content for a task-specific spec file.
/// Create a git worktree for the task, or fall back to a plain directory.
fn ensure_worktree(cwd: &Path, worktree_path: &Path, task_id: &str, has_git: bool) -> Result<()> {
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating worktree parent {}", parent.display()))?;
    }
    if has_git && !worktree_path.exists() {
        let branch = format!("task-{}", task_id);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("worktree")
            .arg("add")
            .arg("-B")
            .arg(&branch)
            .arg(worktree_path)
            .output()
            .with_context(|| format!("git worktree add for task {}", task_id))?;
        if !output.status.success() {
            eprintln!(
                "[printer] git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::create_dir_all(worktree_path).with_context(|| {
                format!(
                    "creating worktree directory (fallback) {}",
                    worktree_path.display()
                )
            })?;
        }
    } else {
        std::fs::create_dir_all(worktree_path)
            .with_context(|| format!("creating worktree directory {}", worktree_path.display()))?;
    }
    Ok(())
}

fn create_task_spec_content(task: &Task) -> String {
    let mut content = format!("# {}\n\n", task.meta.title);
    if !task.body.is_empty() {
        content.push_str(&task.body);
        content.push('\n');
    }
    content
}

async fn run_action(
    args: &ExecArgs,
    cwd: &Path,
    checkpoint_path: &Path,
    action: Action,
    existing: Option<Checkpoint>,
    sandbox: Option<&ActiveSandbox>,
) -> Result<TokenUsage> {
    let metrics_spec: PathBuf = match &action {
        Action::Fresh { spec }
        | Action::ResumeRun { spec, .. }
        | Action::ResumeReview { spec }
        | Action::AlreadyDone { spec }
        | Action::FreshAfterDone { spec, .. } => spec.clone(),
    };
    let total = match action {
        Action::AlreadyDone { spec } => {
            eprintln!(
                "[printer] exec already finalized for {} (checkpoint is done/cancelled). \
                 Remove {} to start over.",
                spec.display(),
                checkpoint_path.display()
            );
            return Ok(TokenUsage::default());
        }
        Action::Fresh { spec } => {
            let cp = Checkpoint::new(spec.clone(), Phase::Planning);
            cp.save(checkpoint_path)?;
            do_run_then_review(args, &spec, checkpoint_path, false, sandbox).await?
        }
        Action::FreshAfterDone { spec, prior } => {
            let history_file = cwd.join(HISTORY_REL);
            let entry = HistoryEntry {
                checkpoint: prior.clone(),
                archived_at: Utc::now(),
            };
            History::append(&history_file, entry)?;
            eprintln!(
                "[printer] archived prior exec for {} to {} (phase={:?}); starting fresh",
                prior.spec.display(),
                history_file.display(),
                prior.phase,
            );
            let cp = Checkpoint::new(spec.clone(), Phase::Planning);
            cp.save(checkpoint_path)?;
            do_run_then_review(args, &spec, checkpoint_path, false, sandbox).await?
        }
        Action::ResumeRun {
            spec,
            skip_planning,
        } => {
            // Bump updated_at so the file reflects this resume.
            let mut cp = existing.unwrap();
            cp.updated_at = Utc::now();
            cp.save(checkpoint_path)?;
            if skip_planning {
                eprintln!(
                    "[printer] resuming run phase for {} (planning already complete)",
                    spec.display()
                );
            } else {
                eprintln!(
                    "[printer] resuming run phase for {} (planning was incomplete; restarting it)",
                    spec.display()
                );
            }
            do_run_then_review(args, &spec, checkpoint_path, skip_planning, sandbox).await?
        }
        Action::ResumeReview { spec } => {
            eprintln!("[printer] resuming at review phase for {}", spec.display());
            do_review(args, &spec, checkpoint_path, sandbox).await?
        }
    };

    eprintln!("[printer] exec token usage (run + review): {total}");
    crate::metrics::record(
        cwd,
        &metrics_spec.to_string_lossy(),
        "exec-total",
        args.agent.to_string(),
        args.model.clone(),
        total,
    );
    Ok(total)
}

async fn do_run_then_review(
    args: &ExecArgs,
    spec: &Path,
    cp_path: &Path,
    skip_planning: bool,
    sandbox: Option<&ActiveSandbox>,
) -> Result<TokenUsage> {
    let mut total = run::run_with_sandbox(
        build_run_args(args, spec, cp_path, skip_planning),
        None,
        sandbox,
    )
    .await?;
    write_phase(cp_path, spec, Phase::ReviewPending)?;
    let review_total = do_review(args, spec, cp_path, sandbox).await?;
    total.add(&review_total);
    Ok(total)
}

/// Drive the review phase, with up to `max_review_passes` cycles of
/// review → fix → re-review. Each iteration:
///   1. runs the review agent and parses its verdict
///   2. if PASS, we're done
///   3. otherwise, feeds the report to the coding agent (which queues fix
///      tasks and works them) and loops back to (1)
///
/// Stops early if the verdict is PASS or the cap is hit.
async fn do_review(
    args: &ExecArgs,
    spec: &Path,
    cp_path: &Path,
    sandbox: Option<&ActiveSandbox>,
) -> Result<TokenUsage> {
    let max_passes = args
        .max_review_passes
        .unwrap_or(DEFAULT_MAX_REVIEW_PASSES)
        .max(1);
    let mut total = TokenUsage::default();

    for pass in 1..=max_passes {
        eprintln!("[printer] review pass {pass}/{max_passes}");
        let outcome = review::review_with_sandbox(build_review_args(args, spec), sandbox).await?;
        total.add(&outcome.usage);

        // In verbose mode echo the fed-back findings into the live log so the
        // reviewer's verdict + suggested follow-ups are visible inline, not
        // only in the written followups file.
        if args.verbose {
            match review::extract_section(&outcome.report, "## Suggested follow-ups") {
                Some(followups) => eprintln!(
                    "[printer] review pass {pass} verdict {} — suggested follow-ups:\n{}",
                    outcome.verdict,
                    followups.trim()
                ),
                None => eprintln!(
                    "[printer] review pass {pass} verdict {} (no follow-ups section in report)",
                    outcome.verdict
                ),
            }
        }

        if outcome.verdict.is_pass() {
            eprintln!("[printer] review verdict PASS on pass {pass}; finishing");
            break;
        }

        if pass == max_passes {
            eprintln!(
                "[printer] review verdict {} on final pass {pass}/{max_passes}; \
                 stopping cycle without a PASS",
                outcome.verdict
            );
            break;
        }

        eprintln!(
            "[printer] review verdict {} on pass {pass}; feeding report back to coding agent",
            outcome.verdict
        );
        // Fix passes run after planning is conceptually complete — we
        // don't want each review→fix loop to re-run planning. Pass
        // skip_planning=true to short-circuit it.
        let fix_usage = run::run_with_sandbox(
            build_run_args(args, spec, cp_path, true),
            Some(outcome.report.as_str()),
            sandbox,
        )
        .await
        .with_context(|| format!("fix pass after review pass {pass} failed"))?;
        total.add(&fix_usage);
    }

    write_phase(cp_path, spec, Phase::Done)?;
    Ok(total)
}

fn write_phase(cp_path: &Path, spec: &Path, phase: Phase) -> Result<()> {
    let mut cp = if cp_path.exists() {
        Checkpoint::load(cp_path)?
    } else {
        Checkpoint::new(spec.to_path_buf(), phase)
    };
    cp.phase = phase;
    cp.spec = spec.to_path_buf();
    cp.updated_at = Utc::now();
    cp.save(cp_path)
}

/// Advance `cp_path`'s checkpoint from `Phase::Planning` to
/// `Phase::Running`. Idempotent: if the checkpoint is already past
/// Planning, it stays put (we never *regress* a phase, since regressing
/// would re-run already-finished work on a subsequent `--continue`).
/// Called by `run.rs` once `planning_pass` returns successfully.
pub fn write_phase_planning_done(cp_path: &Path, spec: &Path) -> Result<()> {
    if !cp_path.exists() {
        // No exec-level checkpoint (standalone `printer run`); nothing
        // to advance. Caller already handles this by not calling us, but
        // be defensive.
        return Ok(());
    }
    let mut cp = Checkpoint::load(cp_path)?;
    if cp.phase == Phase::Planning {
        cp.phase = Phase::Running;
        cp.spec = spec.to_path_buf();
        cp.updated_at = Utc::now();
        cp.save(cp_path)?;
    }
    Ok(())
}

/// Force a spec's exec checkpoint to a terminal `phase` (`Done` for
/// "completed" or `Cancelled` for "cancelled") without driving an agent.
/// Backs `printer spec complete|cancel`. Creates the checkpoint if the
/// spec has never been exec'd. The spec path is canonicalized so the
/// checkpoint key matches what `printer exec` computes; if the file no
/// longer exists on disk we fall back to the path as given. A later
/// `printer exec <spec>` treats both terminal phases as "already
/// finished" and archives + restarts on an explicit re-run.
pub fn mark_spec_phase(cwd: Option<&Path>, spec: &Path, phase: Phase) -> Result<()> {
    let cwd: PathBuf = match cwd {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--cwd not found: {}", p.display()))?,
        None => std::env::current_dir()?,
    };
    let spec_abs = spec.canonicalize().unwrap_or_else(|_| spec.to_path_buf());
    let path = checkpoint_path_for_spec(&cwd, &spec_abs);
    let mut cp = if path.exists() {
        Checkpoint::load(&path)?
    } else {
        Checkpoint::new(spec_abs.clone(), phase)
    };
    cp.phase = phase;
    cp.spec = spec_abs;
    cp.updated_at = Utc::now();
    cp.save(&path)?;
    println!(
        "[printer] marked {} as {:?} ({})",
        cp.spec.display(),
        phase,
        path.display()
    );
    Ok(())
}

fn build_run_args(
    args: &ExecArgs,
    spec: &Path,
    checkpoint_path: &Path,
    skip_planning: bool,
) -> RunArgs {
    RunArgs {
        spec: spec.to_path_buf(),
        agent: args.agent.clone(),
        model: args.model.clone(),
        max_turns: args.max_turns,
        compact_at: args.compact_at,
        cwd: args.cwd.clone(),
        permission_mode: args.permission_mode.clone(),
        verbose: args.verbose,
        // Exec already owns the daemon (or chose not to spawn one); never
        // double-spawn from the inner run.
        no_codegraph_watch: true,
        // Exec already ran the plugin check up front; suppress it inside the
        // nested run so we don't re-prompt the user mid-exec.
        skip_plugin_check: true,
        // Exec acquires one sandbox covering both phases and passes it down
        // explicitly via run_with_sandbox; the inner run must not create its
        // own.
        no_sandbox: true,
        acp_bin: args.acp_bin.clone(),
        acp_args: args.acp_args.clone(),
        // Resume-aware: when the planning pass already completed in a
        // prior exec attempt, skip it now and jump to the exec loop. The
        // checkpoint path is plumbed so run.rs can advance phase
        // (Planning → Running) once planning_pass returns.
        skip_planning,
        checkpoint_path: Some(checkpoint_path.to_path_buf()),
    }
}

fn build_review_args(args: &ExecArgs, spec: &Path) -> ReviewArgs {
    ReviewArgs {
        spec: spec.to_path_buf(),
        agent: args.agent.clone(),
        model: args.model.clone(),
        base: args.base.clone(),
        cwd: args.cwd.clone(),
        out: args.out.clone(),
        permission_mode: args.permission_mode.clone(),
        skills: args.skills.clone(),
        verbose: args.verbose,
        no_sandbox: true,
        no_ui_host: false,
        acp_bin: args.acp_bin.clone(),
        acp_args: args.acp_args.clone(),
    }
}

fn acquire_exec_sandbox(
    cwd: &Path,
    spec: Option<PathBuf>,
    agent: &crate::cli::AgentKind,
    task_id: Option<String>,
) -> Result<Option<ActiveSandbox>> {
    let cfg = match crate::config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[printer] failed to load config ({e}); using defaults");
            crate::config::GlobalConfig::default()
        }
    };
    let drivers = match DriverSet::load_installed() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[printer] failed to load drivers ({e}); continuing without sandbox");
            return Ok(None);
        }
    };
    let Some(active) = drivers.resolve(&cfg.sandbox.driver)? else {
        return Ok(None);
    };
    let merged = active.with_overrides(&cfg.sandbox.commands)?;
    Ok(Some(ActiveSandbox::create(
        merged,
        cwd.to_path_buf(),
        spec,
        Some(crate::drivers::base_image_for_agent(
            agent,
            cfg.sandbox.base_image.clone(),
        )),
        crate::drivers::agent_setup_arg(agent),
        task_id,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a Checkpoint for `spec` at `phase` and seed it into the
    /// per-spec checkpoint file under `cwd`. Returns the file path.
    fn seed(cwd: &Path, spec: &str, phase: Phase) -> PathBuf {
        let cp = Checkpoint::new(PathBuf::from(spec), phase);
        let path = checkpoint_path_for_spec(cwd, Path::new(spec));
        cp.save(&path).unwrap();
        path
    }

    fn empty_cwd() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn checkpoint_round_trips() {
        let dir = empty_cwd();
        let cp = Checkpoint::new(PathBuf::from("/tmp/spec.md"), Phase::ReviewPending);
        let path = checkpoint_path_for_spec(dir.path(), Path::new("/tmp/spec.md"));
        cp.save(&path).unwrap();
        let loaded = Checkpoint::load(&path).unwrap();
        assert_eq!(loaded.spec, cp.spec);
        assert_eq!(loaded.phase, cp.phase);
    }

    #[test]
    fn checkpoint_key_distinguishes_same_basename_in_different_dirs() {
        let k1 = checkpoint_key_for_spec(Path::new("/projects/a/spec.md"));
        let k2 = checkpoint_key_for_spec(Path::new("/projects/b/spec.md"));
        assert_ne!(
            k1, k2,
            "same-basename specs in different dirs must hash apart"
        );
        assert!(k1.starts_with("spec-"));
        assert!(k2.starts_with("spec-"));
    }

    #[test]
    fn fresh_with_spec_no_checkpoint() {
        let dir = empty_cwd();
        let (action, _path) = decide(dir.path(), Some(Path::new("/tmp/a.md")), false).unwrap();
        assert_eq!(
            action,
            Action::Fresh {
                spec: PathBuf::from("/tmp/a.md")
            }
        );
    }

    #[test]
    fn fresh_without_spec_errors() {
        let dir = empty_cwd();
        let err = decide(dir.path(), None, false).unwrap_err();
        assert!(err.to_string().contains("missing spec"));
    }

    #[test]
    fn continue_without_any_checkpoint_errors() {
        let dir = empty_cwd();
        let err = decide(dir.path(), None, true).unwrap_err();
        assert!(err.to_string().contains("no in-flight checkpoint"));
    }

    #[test]
    fn continue_with_planning_phase_redoes_planning() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Planning);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), true).unwrap();
        assert_eq!(
            action,
            Action::ResumeRun {
                spec: PathBuf::from("/tmp/a.md"),
                skip_planning: false,
            }
        );
    }

    #[test]
    fn continue_with_running_phase_skips_planning() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Running);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), true).unwrap();
        assert_eq!(
            action,
            Action::ResumeRun {
                spec: PathBuf::from("/tmp/a.md"),
                skip_planning: true,
            }
        );
    }

    #[test]
    fn continue_with_review_pending_yields_resume_review() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::ReviewPending);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), true).unwrap();
        assert_eq!(
            action,
            Action::ResumeReview {
                spec: PathBuf::from("/tmp/a.md")
            }
        );
    }

    #[test]
    fn continue_with_done_yields_already_done() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Done);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), true).unwrap();
        assert_eq!(
            action,
            Action::AlreadyDone {
                spec: PathBuf::from("/tmp/a.md")
            }
        );
    }

    #[test]
    fn continue_with_cancelled_yields_already_done() {
        // A cancelled spec resumes the same as a done one: nothing to do.
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Cancelled);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), true).unwrap();
        assert_eq!(
            action,
            Action::AlreadyDone {
                spec: PathBuf::from("/tmp/a.md")
            }
        );
    }

    #[test]
    fn continue_with_no_spec_and_cancelled_only_errors() {
        // Cancelled is terminal, not in-flight — `--continue` (no spec)
        // has nothing to pick up.
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Cancelled);
        let err = decide(dir.path(), None, true).unwrap_err();
        assert!(err.to_string().contains("no in-flight checkpoint"));
    }

    #[test]
    fn fresh_with_cancelled_checkpoint_archives_and_restarts() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Cancelled);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), false).unwrap();
        match action {
            Action::FreshAfterDone { spec, prior } => {
                assert_eq!(spec, PathBuf::from("/tmp/a.md"));
                assert_eq!(prior.phase, Phase::Cancelled);
            }
            other => panic!("expected FreshAfterDone, got {other:?}"),
        }
    }

    #[test]
    fn mark_spec_phase_writes_terminal_phase() {
        let dir = empty_cwd();
        // No prior checkpoint: cancel creates one at Cancelled.
        mark_spec_phase(Some(dir.path()), Path::new("/tmp/a.md"), Phase::Cancelled).unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let path = checkpoint_path_for_spec(&cwd, Path::new("/tmp/a.md"));
        assert_eq!(Checkpoint::load(&path).unwrap().phase, Phase::Cancelled);

        // Re-marking the same spec as complete overwrites the phase.
        mark_spec_phase(Some(dir.path()), Path::new("/tmp/a.md"), Phase::Done).unwrap();
        assert_eq!(Checkpoint::load(&path).unwrap().phase, Phase::Done);
    }

    #[test]
    fn continue_unknown_spec_errors_with_summary() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Running);
        let err = decide(dir.path(), Some(Path::new("/tmp/b.md")), true).unwrap_err();
        // Tells the user what they asked for is missing AND lists what
        // checkpoints DO exist so they can pick one.
        let msg = err.to_string();
        assert!(msg.contains("/tmp/b.md"));
        assert!(msg.contains("Existing checkpoints"));
        assert!(msg.contains("/tmp/a.md"));
    }

    #[test]
    fn continue_with_no_spec_picks_only_in_flight() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Running);
        let (action, _) = decide(dir.path(), None, true).unwrap();
        assert_eq!(
            action,
            Action::ResumeRun {
                spec: PathBuf::from("/tmp/a.md"),
                skip_planning: true,
            }
        );
    }

    #[test]
    fn continue_with_no_spec_and_done_only_errors() {
        // A Done checkpoint isn't "in flight" — there's nothing to resume,
        // and `--continue` without a spec means "pick the in-flight one".
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Done);
        let err = decide(dir.path(), None, true).unwrap_err();
        assert!(err.to_string().contains("no in-flight checkpoint"));
    }

    #[test]
    fn continue_with_no_spec_and_multiple_in_flight_errors() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Running);
        seed(dir.path(), "/tmp/b.md", Phase::Planning);
        let err = decide(dir.path(), None, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 in-flight checkpoints"));
        assert!(msg.contains("/tmp/a.md"));
        assert!(msg.contains("/tmp/b.md"));
    }

    #[test]
    fn fresh_with_done_checkpoint_archives_and_restarts() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Done);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), false).unwrap();
        match action {
            Action::FreshAfterDone { spec, prior } => {
                assert_eq!(spec, PathBuf::from("/tmp/a.md"));
                assert_eq!(prior.spec, PathBuf::from("/tmp/a.md"));
                assert_eq!(prior.phase, Phase::Done);
            }
            other => panic!("expected FreshAfterDone, got {other:?}"),
        }
    }

    #[test]
    fn fresh_with_existing_checkpoint_same_spec_resumes() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::ReviewPending);
        let (action, _) = decide(dir.path(), Some(Path::new("/tmp/a.md")), false).unwrap();
        assert_eq!(
            action,
            Action::ResumeReview {
                spec: PathBuf::from("/tmp/a.md")
            }
        );
    }

    #[test]
    fn different_specs_dont_collide() {
        // The whole point of per-spec checkpoint files: starting an exec
        // for spec B in a cwd that has an in-flight checkpoint for spec A
        // does not error and does not touch A's checkpoint.
        let dir = empty_cwd();
        let a_path = seed(dir.path(), "/tmp/a.md", Phase::Running);
        let (action, b_path) = decide(dir.path(), Some(Path::new("/tmp/b.md")), false).unwrap();
        assert_eq!(
            action,
            Action::Fresh {
                spec: PathBuf::from("/tmp/b.md")
            }
        );
        assert_ne!(a_path, b_path, "per-spec files must use different paths");
        // A's checkpoint untouched.
        assert!(a_path.exists());
    }

    #[test]
    fn write_phase_planning_done_advances_only_from_planning() {
        let dir = empty_cwd();
        let path = seed(dir.path(), "/tmp/a.md", Phase::Planning);
        write_phase_planning_done(&path, Path::new("/tmp/a.md")).unwrap();
        assert_eq!(Checkpoint::load(&path).unwrap().phase, Phase::Running);

        // Already past Planning — must not regress (or progress).
        let path2 = seed(dir.path(), "/tmp/b.md", Phase::ReviewPending);
        write_phase_planning_done(&path2, Path::new("/tmp/b.md")).unwrap();
        assert_eq!(
            Checkpoint::load(&path2).unwrap().phase,
            Phase::ReviewPending
        );
    }

    #[test]
    fn list_checkpoints_returns_per_spec_files() {
        let dir = empty_cwd();
        seed(dir.path(), "/tmp/a.md", Phase::Running);
        seed(dir.path(), "/tmp/b.md", Phase::Done);
        let all = list_checkpoints(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        let specs: Vec<_> = all.iter().map(|(_, c)| c.spec.clone()).collect();
        assert!(specs.contains(&PathBuf::from("/tmp/a.md")));
        assert!(specs.contains(&PathBuf::from("/tmp/b.md")));
    }

    #[test]
    fn history_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let entry = HistoryEntry {
            checkpoint: Checkpoint::new(PathBuf::from("/tmp/a.md"), Phase::Done),
            archived_at: Utc::now(),
        };
        History::append(&path, entry.clone()).unwrap();

        let entry2 = HistoryEntry {
            checkpoint: Checkpoint::new(PathBuf::from("/tmp/b.md"), Phase::Done),
            archived_at: Utc::now(),
        };
        History::append(&path, entry2).unwrap();

        let loaded = History::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(
            loaded.entries[0].checkpoint.spec,
            PathBuf::from("/tmp/a.md")
        );
        assert_eq!(
            loaded.entries[1].checkpoint.spec,
            PathBuf::from("/tmp/b.md")
        );
    }

    #[test]
    /// Create a git worktree for the task, or fall back to a plain directory.
    fn create_task_spec_content_formats_title_and_body() {
        use crate::tasks::model::{Status, TaskMeta};

        let task = Task {
            meta: TaskMeta {
                id: "T-001".to_string(),
                title: "Implement feature X".to_string(),
                status: Status::Open,
                priority: 2,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                owner: String::new(),
                labels: vec!["feature".to_string()],
                depends_on: Vec::new(),
                blocked_reason: String::new(),
                spec_anchor: String::new(),
            },
            body: "Implementation details here.".to_string(),
        };

        let content = create_task_spec_content(&task);
        assert!(content.starts_with("# Implement feature X"));
        assert!(content.contains("Implementation details here."));
    }

    #[test]
    /// Create a git worktree for the task, or fall back to a plain directory.
    fn create_task_spec_content_handles_empty_body() {
        use crate::tasks::model::{Status, TaskMeta};

        let task = Task {
            meta: TaskMeta {
                id: "T-002".to_string(),
                title: "Simple task".to_string(),
                status: Status::Open,
                priority: 3,
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                owner: String::new(),
                labels: Vec::new(),
                depends_on: Vec::new(),
                blocked_reason: String::new(),
                spec_anchor: String::new(),
            },
            body: String::new(),
        };

        let content = create_task_spec_content(&task);
        assert_eq!(content, "# Simple task\n\n");
    }
}
