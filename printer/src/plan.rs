use crate::agent::{AgentInvocation, TokenUsage};
use crate::cli::PlanArgs;
use crate::prompts::{
    interactive_planning_prompt, plan_resume_with_answers_prompt, SENTINEL_BLOCKED,
    SENTINEL_PLAN_READY, SENTINEL_QUESTIONS_CLOSE, SENTINEL_QUESTIONS_OPEN,
};
use crate::session::Session;
use crate::tasks::spec;
use anyhow::{Context, Result};
use chrono::Utc;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Entry point for `printer plan <spec>`.
pub async fn plan(args: PlanArgs) -> Result<TokenUsage> {
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
    let printer_dir = cwd.join(".printer");
    let tasks_dir = printer_dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("creating task store at {}", tasks_dir.display()))?;

    let printer_bin = std::env::current_exe()
        .context("resolving printer binary path for the agent prompt")?;
    let printer_bin_str = printer_bin.to_string_lossy().into_owned();

    // Sync spec → task store so the agent has tasks to refine.
    let report = sync_spec(&spec_abs, &tasks_dir)?;
    eprintln!(
        "[printer] spec sync: {} new, {} existing, {} closed-from-spec",
        report.created, report.existing, report.closed
    );

    let acp = crate::agents::resolve_acp_launch(
        &args.agent,
        args.acp_bin.as_deref(),
        &args.acp_args,
    )?;
    let agent = AgentInvocation {
        kind: args.agent.clone(),
        model: args.model.as_deref(),
        cwd: Some(&cwd),
        permission_mode: &args.permission_mode,
        command_wrapper: None,
        verbose: args.verbose,
        acp_bin: acp.bin.as_deref(),
        acp_args: acp.args.as_slice(),
        acp_env: &acp.env,
    };
    let mut session = Session::new(agent).with_verbose(args.verbose);

    let mut prompt = interactive_planning_prompt(&printer_bin_str, &spec_abs.to_string_lossy());
    let mut rounds: u32 = 0;
    let final_text;
    loop {
        let outcome = session.turn(&prompt).await?;
        let text = outcome.result_text;
        if let Some(reason) = blocked_reason(&text) {
            anyhow::bail!("agent reported blocked during planning: {reason}");
        }
        if let Some(qs) = extract_questions(&text) {
            if args.no_questions {
                eprintln!(
                    "[printer] agent asked questions but --no-questions is set; instructing it to finalize"
                );
                prompt = plan_resume_with_answers_prompt(
                    &printer_bin_str,
                    "(no answers — finalize the plan with your best inference)",
                );
                rounds += 1;
                if rounds >= args.max_question_rounds {
                    anyhow::bail!(
                        "exceeded --max-question-rounds {}; aborting plan",
                        args.max_question_rounds
                    );
                }
                continue;
            }
            rounds += 1;
            if rounds > args.max_question_rounds {
                anyhow::bail!(
                    "exceeded --max-question-rounds {}; aborting plan",
                    args.max_question_rounds
                );
            }
            let answers = collect_answers(&qs)?;
            prompt = plan_resume_with_answers_prompt(&printer_bin_str, &answers);
            continue;
        }
        if text.contains(SENTINEL_PLAN_READY) {
            final_text = text;
            break;
        }
        anyhow::bail!(
            "agent ended planning turn without {SENTINEL_PLAN_READY} or a question block"
        );
    }

    write_checkpoint(&printer_dir, &spec_abs, &final_text)?;
    eprintln!("[printer] plan checkpoint written to {}", printer_dir.join("plan.checkpoint").display());
    Ok(session.usage_total)
}

fn sync_spec(spec_abs: &Path, tasks_dir: &Path) -> Result<spec::SyncReport> {
    let text = std::fs::read_to_string(spec_abs)
        .with_context(|| format!("reading spec {}", spec_abs.display()))?;
    let items = spec::parse_spec(&text);
    spec::sync_to_store(&items, spec_abs, tasks_dir)
}

fn blocked_reason(result_text: &str) -> Option<String> {
    let idx = result_text.find(SENTINEL_BLOCKED)?;
    let after = &result_text[idx + SENTINEL_BLOCKED.len()..];
    let end = after.find('\n').unwrap_or(after.len());
    let line = &after[..end];
    Some(line.trim_end_matches('>').trim().to_string())
}

/// Pull the body between `<<QUESTIONS>>` and `<<END_QUESTIONS>>`. Returns
/// `None` if the block is absent or malformed.
fn extract_questions(text: &str) -> Option<String> {
    let open_idx = text.find(SENTINEL_QUESTIONS_OPEN)?;
    let after_open = &text[open_idx + SENTINEL_QUESTIONS_OPEN.len()..];
    let close_rel = after_open.find(SENTINEL_QUESTIONS_CLOSE)?;
    let body = after_open[..close_rel].trim();
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
}

/// Print the questions to the user and read their answers. Reads until EOF
/// (Ctrl-D on a TTY) or a line consisting solely of `.`.
fn collect_answers(questions: &str) -> Result<String> {
    println!();
    println!("--- The agent has questions ---");
    println!("{questions}");
    println!("--- End of questions ---");
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        println!("Type your answers below. End with a single line containing only `.` or press Ctrl-D when done.");
    }
    print!("> ");
    std::io::stdout().flush().ok();

    let mut answers = String::new();
    let lock = stdin.lock();
    for line in lock.lines() {
        let line = line.context("reading answer from stdin")?;
        if line.trim() == "." {
            break;
        }
        answers.push_str(&line);
        answers.push('\n');
        if std::io::stdin().is_terminal() {
            print!("> ");
            std::io::stdout().flush().ok();
        }
    }
    if answers.trim().is_empty() {
        anyhow::bail!("no answers received from user; aborting plan");
    }
    Ok(answers)
}

fn write_checkpoint(printer_dir: &Path, spec_abs: &Path, agent_tail: &str) -> Result<()> {
    let path = printer_dir.join("plan.checkpoint");
    let mut body = String::new();
    body.push_str("# printer plan checkpoint\n");
    body.push_str(&format!("spec: {}\n", spec_abs.display()));
    body.push_str(&format!("created_at: {}\n", Utc::now().to_rfc3339()));
    body.push_str("\n## Final agent output\n\n");
    body.push_str(agent_tail.trim());
    body.push('\n');
    std::fs::write(&path, body)
        .with_context(|| format!("writing plan checkpoint to {}", path.display()))?;
    Ok(())
}
