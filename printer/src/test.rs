//! `printer test` — drive one agent turn that click-tests a UI/web change with
//! the `computer` tool. Unlike `printer review`, this always runs on the host
//! (a real display is required to synthesize input) and never acquires a
//! sandbox. It reuses review's display/UI detection helpers so the two stay in
//! sync about what "a display is available" means.

use crate::agent::AgentInvocation;
use crate::cli::TestArgs;
use crate::host::{computer_on_path, host_display_available};
use crate::prompts::test_prompt;
use crate::review::{Verdict, detect_base, parse_verdict};
use crate::session::Session;
use crate::skills;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub async fn test(args: TestArgs) -> Result<()> {
    let spec_abs = args
        .spec
        .canonicalize()
        .with_context(|| format!("spec file not found: {}", args.spec.display()))?;
    let cwd = match args.cwd.as_deref() {
        Some(p) => Some(
            p.canonicalize()
                .with_context(|| format!("--cwd not found: {}", p.display()))?,
        ),
        None => None,
    };
    let cwd_ref = cwd.as_deref();

    // Preconditions: click-testing needs a real display AND the `computer`
    // binary. If either is missing, emit the same actionable guidance the
    // review host-routing path uses and exit non-PASS without spending a turn.
    if !host_display_available() {
        anyhow::bail!(
            "no usable display for click-testing: set WAYLAND_DISPLAY/XDG_SESSION_TYPE and ensure \
             /dev/uinput exists (a headless sandbox cannot run `printer test`). \
             Run on a host with a graphical session."
        );
    }
    if !computer_on_path() {
        anyhow::bail!(
            "computer CLI not found on PATH; UI click-testing cannot run. \
             Install it: re-run install.sh (or PRINTER_BINS=computer ./install.sh)."
        );
    }

    let base = match &args.base {
        Some(b) => b.clone(),
        None => detect_base(cwd_ref).unwrap_or_else(|| "HEAD~1".to_string()),
    };
    eprintln!("[printer] click-testing change against base ref: {base}");

    let default_skills_root = cwd_ref
        .map(|d| d.join("skills"))
        .unwrap_or_else(|| PathBuf::from("skills"));
    let resolved_skills = skills::resolve(&args.skills, Some(&default_skills_root))?;
    if !resolved_skills.is_empty() {
        let names: Vec<&str> = resolved_skills.iter().map(|s| s.name.as_str()).collect();
        eprintln!("[printer] skills available to tester: {}", names.join(", "));
    }

    let acp =
        crate::agents::resolve_acp_launch(&args.agent, args.acp_bin.as_deref(), &args.acp_args)?;
    let agent = AgentInvocation {
        kind: args.agent.clone(),
        model: args.model.as_deref(),
        cwd: cwd_ref,
        permission_mode: &args.permission_mode,
        command_wrapper: None,
        verbose: args.verbose,
        acp_bin: acp.bin.as_deref(),
        acp_args: acp.args.as_slice(),
        acp_env: &acp.env,
    };
    let mut session = Session::new(agent).with_verbose(args.verbose);

    let spec_arg = spec_abs.to_string_lossy().into_owned();
    let outcome = session
        .turn(&test_prompt(
            &spec_arg,
            &base,
            args.url.as_deref(),
            &resolved_skills,
            None,
        ))
        .await?;
    let report = outcome.result_text.trim().to_string();
    println!("{report}");

    let usage = session.usage_total;
    let verdict = parse_verdict(&report);
    eprintln!("[printer] test verdict: {verdict}");
    eprintln!("[printer] test token usage: {usage}");

    let metrics_cwd = cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    crate::metrics::record(
        &metrics_cwd,
        &spec_abs.to_string_lossy(),
        "test",
        args.agent.to_string(),
        args.model.clone(),
        usage,
    );

    // Exit code mirrors the verdict: 0 only on a clean PASS so `printer test`
    // is usable as a CI/gate check.
    if verdict != Verdict::Pass {
        anyhow::bail!("click-test did not pass (verdict: {verdict})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use clap::Parser;

    #[test]
    fn test_subcommand_parses_spec_and_url() {
        let cli = Cli::try_parse_from([
            "printer",
            "test",
            "specs/011-observability.md",
            "--url",
            "http://localhost:3000",
            "-v",
        ])
        .expect("`printer test` args should parse");
        match cli.command {
            Command::Test(args) => {
                assert_eq!(args.spec.to_str().unwrap(), "specs/011-observability.md");
                assert_eq!(args.url.as_deref(), Some("http://localhost:3000"));
                assert!(args.verbose);
            }
            other => panic!("expected Test command, got {other:?}"),
        }
    }
}
