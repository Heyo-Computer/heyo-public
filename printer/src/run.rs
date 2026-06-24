use crate::agent::{AgentInvocation, TokenUsage};
use crate::cli::RunArgs;
use crate::codegraph_watch;
use crate::drivers::{ActiveSandbox, DriverSet};
use crate::hooks::{AgentContribution, Event, HookContext, HookSet};
use crate::prompts::{
    SENTINEL_BLOCKED, SENTINEL_DONE, bootstrap_prompt, fix_from_review_prompt, nudge_prompt_with,
    planning_prompt, rotation_prompt, unstall_prompt,
};
use crate::session::{MetricsCtx, Session};
use crate::skills;
use crate::tasks::model::{Status, Task};
use crate::tasks::{spec, store};
use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub async fn run(args: RunArgs) -> Result<TokenUsage> {
    run_with_feedback(args, None).await
}

/// Same as [`run`], but if `review_feedback` is `Some(report)`, the agent is
/// shown the prior review report on its first turn and asked to add or reopen
/// tasks to address the findings. Used by the exec review-cycle to feed
/// reviewer output back into the coding agent.
pub async fn run_with_feedback(args: RunArgs, review_feedback: Option<&str>) -> Result<TokenUsage> {
    // Public entry path: acquire our own sandbox (if any plugin contributes
    // one) and then dispatch. The CLI flag `--no-sandbox` short-circuits.
    let sandbox = acquire_sandbox(&args)?;
    run_with_sandbox(args, review_feedback, sandbox.as_ref()).await
}

/// Sandbox-aware entry. Used by `printer exec` to share one sandbox between
/// the run and review phases — exec creates the [`ActiveSandbox`] once and
/// hands a reference to both phases instead of letting each one create its
/// own. CLI entries above wrap this with their own per-call sandbox.
pub async fn run_with_sandbox(
    args: RunArgs,
    review_feedback: Option<&str>,
    sandbox: Option<&ActiveSandbox>,
) -> Result<TokenUsage> {
    crate::plugins::prompt_if_no_plugins(args.skip_plugin_check)?;
    let hooks = HookSet::load_installed().unwrap_or_default();
    let spec_abs = args
        .spec
        .canonicalize()
        .with_context(|| format!("spec file not found: {}", args.spec.display()))?;
    if !spec_abs.is_file() {
        anyhow::bail!("spec must be a file: {}", spec_abs.display());
    }

    let cwd: PathBuf = match args.cwd.as_deref() {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--cwd not found: {}", p.display()))?,
        None => std::env::current_dir()?,
    };
    let tasks_dir = cwd.join(".printer").join("tasks");
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("creating task store at {}", tasks_dir.display()))?;

    let _watch_guard = if args.no_codegraph_watch {
        None
    } else {
        codegraph_watch::try_spawn(&cwd).unwrap_or_else(|e| {
            eprintln!("[printer] codegraph watch spawn failed: {e}; continuing without daemon");
            None
        })
    };

    hooks.run_cli(
        Event::BeforeRun,
        &HookContext::new(Event::BeforeRun, cwd.clone()).with_spec(spec_abs.clone()),
    )?;
    let agent_contrib = hooks.agent_for(Event::BeforeRun);
    let resolved_skills = resolve_agent_skills(&agent_contrib)?;
    if !agent_contrib.is_empty() {
        eprintln!(
            "[printer] before_run hooks: {} prompt chunk(s), {} skill(s)",
            agent_contrib.prompt_chunks.len(),
            resolved_skills.len()
        );
    }
    let injected_block = agent_contrib.render_prompt_block();

    if let Some(sb) = sandbox {
        eprintln!(
            "[printer] dispatching run agent inside sandbox driver `{}` (handle: {})",
            sb.plugin(),
            sb.handle()
        );
    }
    let wrapper_template = sandbox.map(|s| s.enter_template());

    // Standalone CLI entry: drive sync_in/sync_out around the agent loop. When
    // exec drives us, it handles these once for both phases (and we receive
    // an already-set-up sandbox via run_with_sandbox).
    let owned_sandbox = if !args.no_sandbox && sandbox.is_some() {
        // We were called via the public entry that constructed the sandbox
        // itself; sync_in fires here. (`run_with_sandbox` from exec passes a
        // borrowed sandbox and `args.no_sandbox` is true, so this branch is
        // only taken on the standalone path.)
        if let Some(sb) = sandbox {
            sb.sync_in()?;
        }
        true
    } else {
        false
    };

    let inner = run_inner(
        &args,
        &hooks,
        &spec_abs,
        &cwd,
        &tasks_dir,
        injected_block.as_deref(),
        &resolved_skills,
        review_feedback,
        wrapper_template.as_deref(),
    )
    .await;

    let after_ctx = HookContext::new(Event::AfterRun, cwd.clone())
        .with_spec(spec_abs.clone())
        .with_exit_status(inner.is_ok());
    let _ = hooks.run_cli(Event::AfterRun, &after_ctx);

    if owned_sandbox && let Some(sb) = sandbox {
        sb.sync_out();
    }

    if let Ok(usage) = &inner {
        eprintln!("[printer] run token usage: {usage}");
        crate::metrics::record(
            &cwd,
            &spec_abs.to_string_lossy(),
            "run",
            args.agent.to_string(),
            args.model.clone(),
            *usage,
        );
    }
    inner
}

fn resolve_agent_skills(contrib: &AgentContribution) -> Result<Vec<skills::Skill>> {
    if contrib.skills.is_empty() {
        return Ok(Vec::new());
    }
    skills::resolve(&contrib.skills, None)
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    args: &RunArgs,
    _hooks: &HookSet,
    spec_abs: &std::path::Path,
    cwd: &std::path::Path,
    tasks_dir: &std::path::Path,
    injected_block: Option<&str>,
    injected_skills: &[skills::Skill],
    review_feedback: Option<&str>,
    command_wrapper: Option<&str>,
) -> Result<TokenUsage> {
    let printer_bin =
        std::env::current_exe().context("resolving printer binary path for the agent prompt")?;
    let printer_bin_str = printer_bin.to_string_lossy().into_owned();

    let acp =
        crate::agents::resolve_acp_launch(&args.agent, args.acp_bin.as_deref(), &args.acp_args)?;
    let agent = AgentInvocation {
        kind: args.agent.clone(),
        model: args.model.as_deref(),
        cwd: Some(cwd),
        permission_mode: &args.permission_mode,
        command_wrapper,
        verbose: args.verbose,
        acp_bin: acp.bin.as_deref(),
        acp_args: acp.args.as_slice(),
        acp_env: &acp.env,
    };
    let mut session = Session::new(agent)
        .with_verbose(args.verbose)
        .with_metrics_context(MetricsCtx {
            cwd: cwd.to_path_buf(),
            spec: spec_abs.to_string_lossy().into_owned(),
            agent: args.agent.to_string(),
            model: args.model.clone(),
        });

    // Initial sync: parse spec and reconcile with the task store.
    let report = sync_spec(spec_abs, tasks_dir)?;
    eprintln!(
        "[printer] spec sync: {} new, {} existing, {} closed-from-spec",
        report.created, report.existing, report.closed
    );

    // If the spec has no checklist items at all, ask the agent to write one
    // into the spec. Then re-sync. If still empty, bail.
    if !any_tasks(tasks_dir)? {
        eprintln!(
            "[printer] spec has no checklist items; asking agent to bootstrap one into the spec"
        );
        let outcome = session
            .turn(&bootstrap_prompt(&spec_abs.to_string_lossy()))
            .await?;
        print_result_tail(&outcome.result_text);
        if let Some(reason) = blocked_reason(&outcome.result_text) {
            anyhow::bail!("agent reported blocked during bootstrap: {reason}");
        }
        let report = sync_spec(spec_abs, tasks_dir)?;
        eprintln!(
            "[printer] post-bootstrap sync: {} new, {} existing",
            report.created, report.existing
        );
        if !any_tasks(tasks_dir)? {
            anyhow::bail!(
                "agent did not write any checklist items into `{}`",
                spec_abs.display()
            );
        }
    }

    // Planning pass: before any code work begins, ask the agent to refine
    // the parsed tasks into a detailed actionable plan (notes, splits,
    // deps). Skipped on resume (`args.skip_planning`) when the exec
    // checkpoint already moved past `Phase::Planning` — a `--continue`
    // after a crash post-planning shouldn't redo planning, since it's
    // expensive and the existing plan is already in the task store.
    // Also skipped if every task is already done — no point planning
    // finished work.
    if args.skip_planning {
        eprintln!(
            "[printer] planning pass: skipped (resuming from a checkpoint where planning already completed)"
        );
    } else {
        let tasks = store::list_all(tasks_dir)?;
        if !all_done(&tasks) {
            eprintln!(
                "[printer] planning pass: refining {} task(s) into actionable plan entries",
                tasks.len()
            );
            let outcome = session
                .turn(&planning_prompt(
                    &printer_bin_str,
                    &spec_abs.to_string_lossy(),
                ))
                .await?;
            print_result_tail(&outcome.result_text);
            if let Some(reason) = blocked_reason(&outcome.result_text) {
                anyhow::bail!("agent reported blocked during planning: {reason}");
            }
        }
        // Planning is durably complete — advance the exec checkpoint
        // (if exec.rs gave us one) so a later `--continue` can skip
        // straight to the run loop instead of redoing this turn. No-op
        // for standalone `printer run` invocations: they have no
        // checkpoint to advance.
        if let Some(cp_path) = args.checkpoint_path.as_deref()
            && let Err(e) = crate::exec::write_phase_planning_done(cp_path, spec_abs)
        {
            eprintln!(
                "[printer] warning: failed to advance checkpoint at {} to Running: {e}",
                cp_path.display()
            );
        }
    }

    // If the caller supplied review feedback, give the agent one turn to
    // ingest it and queue follow-up tasks before the normal loop starts. We
    // do this *after* spec sync so the agent can see the existing task store,
    // and we re-list tasks afterwards so the loop's stall detection has a
    // fresh baseline.
    if let Some(feedback) = review_feedback {
        eprintln!("[printer] feeding review report back to coding agent");
        let outcome = session
            .turn(&fix_from_review_prompt(&printer_bin_str, feedback))
            .await?;
        print_result_tail(&outcome.result_text);
        if let Some(reason) = blocked_reason(&outcome.result_text) {
            anyhow::bail!("agent reported blocked while ingesting review: {reason}");
        }
    }

    // Execute loop.
    let mut tasks = store::list_all(tasks_dir)?;
    let mut prev_state_hash = state_hash(&tasks);
    let mut stalls: u32 = 0;

    if all_done(&tasks) {
        eprintln!("[printer] all tasks already done; nothing to do.");
        return Ok(session.usage_total);
    }

    for _ in 0..args.max_turns {
        // Compaction check.
        if session.cumulative_input_tokens >= args.compact_at {
            eprintln!(
                "[printer] non-cached input tokens {} >= {}; rotating session",
                session.cumulative_input_tokens, args.compact_at
            );
            session.rotate().await;
            // First turn of the new session: orient the agent to the world.
            let outcome = session
                .turn(&rotation_prompt(
                    &printer_bin_str,
                    &spec_abs.to_string_lossy(),
                ))
                .await?;
            print_result_tail(&outcome.result_text);
            if let Some(reason) = blocked_reason(&outcome.result_text) {
                anyhow::bail!("agent reported blocked: {reason}");
            }
            tasks = store::list_all(tasks_dir)?;
            if all_done(&tasks) {
                eprintln!("[printer] all tasks done.");
                return Ok(session.usage_total);
            }
            // Replan before resuming code work: a fresh session has no memory
            // of the prior plan, so we ask it to refresh task notes against
            // the current state of the store + tree before the nudge loop
            // resumes.
            eprintln!(
                "[printer] post-rotation planning pass: refreshing plan against current state"
            );
            let outcome = session
                .turn(&planning_prompt(
                    &printer_bin_str,
                    &spec_abs.to_string_lossy(),
                ))
                .await?;
            print_result_tail(&outcome.result_text);
            if let Some(reason) = blocked_reason(&outcome.result_text) {
                anyhow::bail!("agent reported blocked during post-rotation planning: {reason}");
            }
            tasks = store::list_all(tasks_dir)?;
            prev_state_hash = state_hash(&tasks);
            if all_done(&tasks) {
                eprintln!("[printer] all tasks done.");
                return Ok(session.usage_total);
            }
            continue;
        }

        let prompt = if stalls > 0 {
            unstall_prompt(&printer_bin_str)
        } else {
            nudge_prompt_with(&printer_bin_str, injected_block, injected_skills)
        };
        let outcome = session.turn(&prompt).await?;
        print_result_tail(&outcome.result_text);

        if let Some(reason) = blocked_reason(&outcome.result_text) {
            anyhow::bail!("agent reported blocked: {reason}");
        }

        tasks = store::list_all(tasks_dir)?;

        if outcome.result_text.contains(SENTINEL_DONE) {
            if all_done(&tasks) {
                eprintln!("[printer] all tasks done.");
                return Ok(session.usage_total);
            } else {
                eprintln!(
                    "[printer] agent emitted {SENTINEL_DONE} but the task store still has unfinished work; nudging once more"
                );
            }
        }
        if all_done(&tasks) {
            eprintln!("[printer] all tasks done.");
            return Ok(session.usage_total);
        }

        // Stall detection: did any task transition this turn?
        let new_hash = state_hash(&tasks);
        if new_hash == prev_state_hash {
            stalls += 1;
            if stalls >= 3 {
                anyhow::bail!("agent stalled for 3 consecutive turns; aborting");
            }
        } else {
            stalls = 0;
            prev_state_hash = new_hash;
        }
    }

    anyhow::bail!("--max-turns {} exhausted", args.max_turns);
}

/// Acquire a sandbox via the active driver, if any. Returns `Ok(None)` when
/// `--no-sandbox` is set or no plugin contributes a driver.
fn acquire_sandbox(args: &RunArgs) -> Result<Option<ActiveSandbox>> {
    if args.no_sandbox {
        return Ok(None);
    }
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
    let cwd: std::path::PathBuf = match args.cwd.as_deref() {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--cwd not found: {}", p.display()))?,
        None => std::env::current_dir()?,
    };
    let spec = args
        .spec
        .canonicalize()
        .with_context(|| format!("spec file not found: {}", args.spec.display()))?;
    Ok(Some(ActiveSandbox::create(
        merged,
        cwd,
        Some(spec),
        Some(crate::drivers::base_image_for_agent(
            &args.agent,
            cfg.sandbox.base_image.clone(),
        )),
        crate::drivers::agent_setup_arg(&args.agent),
        None,
    )?))
}

fn sync_spec(spec_abs: &std::path::Path, tasks_dir: &std::path::Path) -> Result<spec::SyncReport> {
    let text = std::fs::read_to_string(spec_abs)
        .with_context(|| format!("reading spec {}", spec_abs.display()))?;
    let items = spec::parse_spec(&text);
    spec::sync_to_store(&items, spec_abs, tasks_dir)
}

fn any_tasks(tasks_dir: &std::path::Path) -> Result<bool> {
    Ok(!store::list_all(tasks_dir)?.is_empty())
}

fn all_done(tasks: &[Task]) -> bool {
    !tasks.is_empty() && tasks.iter().all(|t| t.meta.status == Status::Done)
}

fn state_hash(tasks: &[Task]) -> u64 {
    let mut h = DefaultHasher::new();
    // Include id + status + updated_at so any state transition changes the hash.
    for t in tasks {
        t.meta.id.hash(&mut h);
        t.meta.status.hash(&mut h);
        t.meta.updated_at.hash(&mut h);
    }
    h.finish()
}

fn print_result_tail(result_text: &str) {
    let trimmed = result_text.trim();
    if trimmed.is_empty() {
        return;
    }
    println!("{}", trimmed);
}

fn blocked_reason(result_text: &str) -> Option<String> {
    let idx = result_text.find(SENTINEL_BLOCKED)?;
    let after = &result_text[idx + SENTINEL_BLOCKED.len()..];
    let end = after.find('\n').unwrap_or(after.len());
    let line = &after[..end];
    let reason = line.trim_end_matches('>').trim();
    Some(reason.to_string())
}
