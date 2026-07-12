use crate::agent::{AgentInvocation, TokenUsage};
use crate::cli::ReviewArgs;
use crate::drivers::{ActiveSandbox, DriverSet};
use crate::hooks::{Event, HookContext, HookSet};
use crate::host::{computer_on_path, host_display_available};
use crate::prompts::review_prompt_with;
use crate::session::{MetricsCtx, Session};
use crate::skills;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Verdict parsed out of the review report's `## Verdict` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Partial,
    Fail,
    /// Could not parse a verdict from the report (treat as fail-safe).
    Unknown,
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Pass => "PASS",
            Verdict::Partial => "PARTIAL",
            Verdict::Fail => "FAIL",
            Verdict::Unknown => "UNKNOWN",
        })
    }
}

/// Normalized result of a single review turn.
#[derive(Debug, Clone)]
pub struct ReviewOutcome {
    pub usage: TokenUsage,
    pub verdict: Verdict,
    pub report: String,
}

/// Extract the body of a top-level markdown section (e.g. `## Suggested follow-ups`)
/// from a review report. Header match is case-insensitive on the heading text.
/// Returns None if the section is missing, or its body trims to empty / "none".
pub fn extract_section(report: &str, heading: &str) -> Option<String> {
    let needle = heading.trim().to_ascii_lowercase();
    let mut lines = report.lines().enumerate();
    let mut start: Option<usize> = None;
    for (i, line) in lines.by_ref() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("## ")
            && rest.trim().to_ascii_lowercase() == needle.strip_prefix("## ").unwrap_or(&needle)
        {
            start = Some(i + 1);
            break;
        }
    }
    let start = start?;
    let collected: Vec<&str> = report
        .lines()
        .skip(start)
        .take_while(|l| !l.trim_start().starts_with("## "))
        .collect();
    let body = collected.join("\n").trim().to_string();
    if body.is_empty() || body.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(body)
    }
}

/// Persist the parsed `## Suggested follow-ups` section of a review report to
/// `.printer/followups/<spec-stem>.md` under `cwd`. Overwrites existing file
/// so the most recent review wins. Returns the path written.
pub fn write_followups(
    cwd: &Path,
    spec_abs: &Path,
    verdict: Verdict,
    report: &str,
) -> Result<PathBuf> {
    let stem = spec_abs
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "spec".to_string());
    let dir = cwd.join(".printer").join("followups");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let out = dir.join(format!("{stem}.md"));
    let body = extract_section(report, "## Suggested follow-ups").unwrap_or_else(|| "none".into());
    let now = chrono::Utc::now().to_rfc3339();
    let contents = format!(
        "# Follow-ups for {}\n\nGenerated: {}\nVerdict: {}\n\n## Suggested follow-ups\n\n{}\n",
        spec_abs.display(),
        now,
        verdict,
        body,
    );
    std::fs::write(&out, contents)
        .with_context(|| format!("writing followups to {}", out.display()))?;
    Ok(out)
}

/// Parse the verdict out of a review report. We look for a `## Verdict`
/// section heading and pick up the first PASS / PARTIAL / FAIL token after it.
/// Falls back to scanning the whole report if the heading is missing.
pub fn parse_verdict(report: &str) -> Verdict {
    let lower = report.to_ascii_lowercase();
    let scan = match lower.find("## verdict") {
        Some(idx) => &lower[idx..],
        None => lower.as_str(),
    };
    // Pick the first explicit verdict token.
    let pass_at = scan.find("pass");
    let partial_at = scan.find("partial");
    let fail_at = scan.find("fail");
    let mut best: Option<(usize, Verdict)> = None;
    for cand in [
        partial_at.map(|i| (i, Verdict::Partial)),
        pass_at.map(|i| (i, Verdict::Pass)),
        fail_at.map(|i| (i, Verdict::Fail)),
    ]
    .into_iter()
    .flatten()
    {
        best = Some(match best {
            None => cand,
            Some(cur) if cand.0 < cur.0 => cand,
            Some(cur) => cur,
        });
    }
    best.map(|(_, v)| v).unwrap_or(Verdict::Unknown)
}

/// Cap a PASS verdict at PARTIAL when the diff has a UI/web surface but no
/// display was available to click-test it — makes unverified UI non-silent.
/// Only PASS is affected; PARTIAL/FAIL/UNKNOWN pass through unchanged.
pub fn cap_verdict_for_unverified_ui(verdict: Verdict, ui_surface: bool, display: bool) -> Verdict {
    if verdict == Verdict::Pass && ui_surface && !display {
        Verdict::Partial
    } else {
        verdict
    }
}

pub async fn review(args: ReviewArgs) -> Result<ReviewOutcome> {
    let sandbox = acquire_sandbox(&args)?;
    if let Some(sb) = sandbox.as_ref() {
        sb.sync_in()?;
    }
    let outcome = review_with_sandbox(args, sandbox.as_ref()).await;
    if let Some(sb) = sandbox.as_ref() {
        sb.sync_out();
    }
    outcome
}

/// Sandbox-aware variant. `printer exec` shares one [`ActiveSandbox`] across
/// run + review by calling this directly, so the VM lifecycle is created once
/// per exec rather than per phase.
pub async fn review_with_sandbox(
    args: ReviewArgs,
    sandbox: Option<&ActiveSandbox>,
) -> Result<ReviewOutcome> {
    let hooks = HookSet::load_installed().unwrap_or_default();

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
    let cwd_for_hook = cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let base = match args.base {
        Some(b) => b,
        None => detect_base(cwd_ref).unwrap_or_else(|| "HEAD~1".to_string()),
    };
    eprintln!("[printer] reviewing against base ref: {base}");

    // Whether this review could actually click-test a UI/web surface. A heyvm
    // sandbox is always headless, so a display is only possible when running on
    // the host (sandbox is None). Captured here while `base` is still in scope;
    // used after the verdict is parsed to cap an over-optimistic PASS.
    let ui_surface = ui_surface_changed(cwd_ref, &base);
    let display_available = sandbox.is_none() && host_display_available();

    hooks.run_cli(
        Event::BeforeReview,
        &HookContext::new(Event::BeforeReview, cwd_for_hook.clone())
            .with_spec(spec_abs.clone())
            .with_base_ref(base.clone()),
    )?;

    let agent_contrib = hooks.agent_for(Event::BeforeReview);
    let injected_block = agent_contrib.render_prompt_block();

    let default_skills_root = cwd_ref
        .map(|d| d.join("skills"))
        .unwrap_or_else(|| std::path::PathBuf::from("skills"));
    // Merge user-supplied --skill paths with hook-contributed skill paths.
    let mut all_skill_paths = args.skills.clone();
    all_skill_paths.extend(agent_contrib.skills.iter().cloned());
    let resolved_skills = skills::resolve(&all_skill_paths, Some(&default_skills_root))?;
    if !resolved_skills.is_empty() {
        let names: Vec<&str> = resolved_skills.iter().map(|s| s.name.as_str()).collect();
        eprintln!(
            "[printer] skills available to reviewer: {}",
            names.join(", ")
        );
    }

    if let Some(sb) = sandbox {
        eprintln!(
            "[printer] dispatching review agent inside sandbox driver `{}` (handle: {})",
            sb.plugin(),
            sb.handle()
        );
    }
    let wrapper = sandbox.map(|s| s.enter_template());
    let acp =
        crate::agents::resolve_acp_launch(&args.agent, args.acp_bin.as_deref(), &args.acp_args)?;
    let agent = AgentInvocation {
        kind: args.agent.clone(),
        model: args.model.as_deref(),
        cwd: cwd_ref,
        permission_mode: &args.permission_mode,
        command_wrapper: wrapper.as_deref(),
        verbose: args.verbose,
        acp_bin: acp.bin.as_deref(),
        acp_args: acp.args.as_slice(),
        acp_env: &acp.env,
    };
    let metrics_cwd = cwd_ref
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut session = Session::new(agent)
        .with_verbose(args.verbose)
        .with_metrics_context(MetricsCtx {
            cwd: metrics_cwd,
            spec: spec_abs.to_string_lossy().into_owned(),
            agent: args.agent.to_string(),
            model: args.model.clone(),
        });

    let spec_arg = spec_abs.to_string_lossy().into_owned();
    let result: Result<String> = async {
        let outcome = session
            .turn(&review_prompt_with(
                &spec_arg,
                &base,
                &resolved_skills,
                injected_block.as_deref(),
            ))
            .await?;
        let report = outcome.result_text.trim().to_string();
        println!("{report}");

        if let Some(out) = args.out.as_deref() {
            std::fs::write(out, format!("{report}\n"))
                .with_context(|| format!("failed to write review report to {}", out.display()))?;
            eprintln!("[printer] review written to {}", out.display());
        }
        Ok(report)
    }
    .await;

    let mut after_ctx = HookContext::new(Event::AfterReview, cwd_for_hook)
        .with_spec(spec_abs.clone())
        .with_base_ref(base)
        .with_exit_status(result.is_ok());
    if let Some(out) = args.out.as_deref() {
        after_ctx = after_ctx.with_report_path(out.to_path_buf());
    }
    let _ = hooks.run_cli(Event::AfterReview, &after_ctx);

    let usage = session.usage_total;
    let report = result?;
    let parsed = parse_verdict(&report);
    let verdict = cap_verdict_for_unverified_ui(parsed, ui_surface, display_available);
    if verdict != parsed {
        eprintln!(
            "[printer] verdict capped {parsed}->{verdict}: UI/web surface changed but no display was available to click-test"
        );
    }
    eprintln!("[printer] review verdict: {verdict}");
    eprintln!("[printer] review token usage: {usage}");
    // Verbose: surface the verification commands the reviewer claims to have
    // run as a distinct stderr block (the full report already went to stdout).
    if args.verbose
        && let Some(verification) = extract_section(&report, "## Verification")
    {
        eprintln!("[printer] review verification:\n{}", verification.trim());
    }

    let followups_cwd = cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    match write_followups(&followups_cwd, &spec_abs, verdict, &report) {
        Ok(p) => eprintln!("[printer] follow-ups written to {}", p.display()),
        Err(e) => eprintln!("[printer] failed to persist follow-ups: {e}"),
    }
    crate::metrics::record(
        &followups_cwd,
        &spec_abs.to_string_lossy(),
        "review",
        args.agent.to_string(),
        args.model.clone(),
        usage,
    );

    Ok(ReviewOutcome {
        usage,
        verdict,
        report,
    })
}

/// Does `path` (a repo-relative path) look like a UI/web surface? Pure matcher
/// so it is unit-testable without a git tree. Unambiguous front-end / markup /
/// style extensions always count; bare `.ts`/`.js` only count under a
/// front-end-ish directory to avoid flagging backend TypeScript/Node code.
fn is_ui_path(path: &str) -> bool {
    let p = path.trim().to_ascii_lowercase();
    const UI_EXTS: &[&str] = &[
        ".tsx", ".jsx", ".vue", ".svelte", ".html", ".htm", ".css", ".scss", ".sass",
    ];
    if UI_EXTS.iter().any(|e| p.ends_with(e)) {
        return true;
    }
    if [".ts", ".js", ".mjs", ".cjs"]
        .iter()
        .any(|e| p.ends_with(e))
    {
        const UI_DIRS: &[&str] = &[
            "web/",
            "frontend/",
            "ui/",
            "client/",
            "src/components/",
            "app/",
        ];
        return UI_DIRS.iter().any(|d| p.contains(d));
    }
    false
}

/// Run `git <args>` in `cwd` and return stdout lines, or None on failure.
fn git_lines(cwd: Option<&Path>, args: &[&str]) -> Option<Vec<String>> {
    let mut cmd = Command::new("git");
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.args(args);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect(),
    )
}

/// True if the review diff (committed `base...HEAD` plus uncommitted changes)
/// touches any UI/web surface — i.e. a change a reviewer would want to
/// click-test rather than just read.
pub(crate) fn ui_surface_changed(cwd: Option<&Path>, base: &str) -> bool {
    let mut paths: Vec<String> = Vec::new();
    if let Some(lines) = git_lines(cwd, &["diff", "--name-only", &format!("{base}...HEAD")]) {
        paths.extend(lines);
    }
    if let Some(lines) = git_lines(cwd, &["status", "--porcelain"]) {
        for l in lines {
            // porcelain format: two status chars + space + path.
            let p = l.get(3..).unwrap_or("").trim();
            if !p.is_empty() {
                paths.push(p.to_string());
            }
        }
    }
    paths.iter().any(|p| is_ui_path(p))
}

fn acquire_sandbox(args: &ReviewArgs) -> Result<Option<ActiveSandbox>> {
    if args.no_sandbox {
        return Ok(None);
    }
    // UI/web review needs a real display to click-test, but a heyvm sandbox is a
    // headless microVM with no Wayland/uinput. When the diff touches a UI
    // surface AND the host has a usable display, run review on the host (no
    // sandbox) so the `computer` tool can drive the app. `--no-ui-host` forces
    // the sandbox even for UI diffs. (Note: `printer exec` shares one sandbox
    // across run+review via review_with_sandbox and bypasses this — exec UI
    // review needs `--no-sandbox`.)
    if !args.no_ui_host {
        let cwd = args.cwd.as_deref();
        let base = match &args.base {
            Some(b) => b.clone(),
            None => detect_base(cwd).unwrap_or_else(|| "HEAD~1".to_string()),
        };
        if ui_surface_changed(cwd, &base) && host_display_available() {
            eprintln!(
                "[printer] UI/web surface detected + host display present; running review on host (no sandbox) for click-testing"
            );
            if !computer_on_path() {
                eprintln!(
                    "[printer] computer CLI not found on PATH; UI click-testing will be skipped. \
                     Install it: re-run install.sh (or PRINTER_BINS=computer ./install.sh)."
                );
            }
            return Ok(None);
        }
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
    let cwd: PathBuf = match args.cwd.as_deref() {
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

pub(crate) fn detect_base(cwd: Option<&Path>) -> Option<String> {
    // Try `main`, then `master`, then HEAD~1.
    for candidate in ["main", "master"] {
        let mut cmd = Command::new("git");
        cmd.args(["rev-parse", "--verify", "--quiet", candidate]);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        if let Ok(out) = cmd.output()
            && out.status.success()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_path_matches_frontend_extensions() {
        assert!(is_ui_path("src/App.tsx"));
        assert!(is_ui_path("components/Button.jsx"));
        assert!(is_ui_path("pages/Home.vue"));
        assert!(is_ui_path("widget.svelte"));
        assert!(is_ui_path("index.html"));
        assert!(is_ui_path("styles/main.css"));
        assert!(is_ui_path("theme.scss"));
    }

    #[test]
    fn ui_path_rejects_backend_sources() {
        assert!(!is_ui_path("src/lib.rs"));
        assert!(!is_ui_path("printer/src/review.rs"));
        assert!(!is_ui_path("README.md"));
        // bare .ts/.js outside a front-end dir does not count
        assert!(!is_ui_path("server/db.ts"));
        assert!(!is_ui_path("scripts/build.js"));
    }

    #[test]
    fn caps_pass_to_partial_when_ui_unverified() {
        use Verdict::*;
        // PASS + UI surface + no display -> capped to PARTIAL.
        assert_eq!(cap_verdict_for_unverified_ui(Pass, true, false), Partial);
        // PASS + UI surface + display present -> PASS stands.
        assert_eq!(cap_verdict_for_unverified_ui(Pass, true, true), Pass);
        // PASS + no UI surface -> PASS stands regardless of display.
        assert_eq!(cap_verdict_for_unverified_ui(Pass, false, false), Pass);
        // Non-PASS verdicts are never altered.
        assert_eq!(cap_verdict_for_unverified_ui(Fail, true, false), Fail);
        assert_eq!(cap_verdict_for_unverified_ui(Partial, true, false), Partial);
        assert_eq!(cap_verdict_for_unverified_ui(Unknown, true, false), Unknown);
    }

    #[test]
    fn ui_path_matches_ts_js_under_frontend_dirs() {
        assert!(is_ui_path("web/app.js"));
        assert!(is_ui_path("frontend/main.ts"));
        assert!(is_ui_path("src/components/list.ts"));
        assert!(is_ui_path("client/index.mjs"));
    }

    #[test]
    fn parses_pass_verdict() {
        let report = "## Verdict\nPASS\n\n## Per-item findings\n- foo MET\n";
        assert_eq!(parse_verdict(report), Verdict::Pass);
    }

    #[test]
    fn parses_partial_verdict() {
        let report = "## Verdict\nPARTIAL — two items missing.\n";
        assert_eq!(parse_verdict(report), Verdict::Partial);
    }

    #[test]
    fn parses_fail_verdict() {
        let report = "## Verdict\nFAIL\nseveral items missing.\n";
        assert_eq!(parse_verdict(report), Verdict::Fail);
    }

    #[test]
    fn unknown_when_no_verdict_section() {
        let report = "lots of prose with no verdict here\n";
        assert_eq!(parse_verdict(report), Verdict::Unknown);
    }

    #[test]
    fn extracts_followups_section() {
        let report = "## Verdict\nPARTIAL\n\n## Suggested follow-ups\n- add tests for X\n- handle empty input\n\n## Out-of-scope\n- big refactor\n";
        let body = extract_section(report, "## Suggested follow-ups").unwrap();
        assert!(body.contains("add tests for X"));
        assert!(body.contains("handle empty input"));
        assert!(!body.contains("big refactor"));
    }

    #[test]
    fn extract_missing_section_returns_none() {
        let report = "## Verdict\nPASS\n\n## Per-item\n- foo MET\n";
        assert!(extract_section(report, "## Suggested follow-ups").is_none());
    }

    #[test]
    fn extract_none_body_returns_none() {
        let report = "## Suggested follow-ups\nnone\n";
        assert!(extract_section(report, "## Suggested follow-ups").is_none());
    }

    #[test]
    fn extract_empty_body_returns_none() {
        let report = "## Suggested follow-ups\n\n## Next\nstuff\n";
        assert!(extract_section(report, "## Suggested follow-ups").is_none());
    }

    #[test]
    fn write_followups_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("specs").join("003-followups.md");
        std::fs::create_dir_all(spec.parent().unwrap()).unwrap();
        std::fs::write(&spec, "spec body").unwrap();
        let report = "## Verdict\nPARTIAL\n\n## Suggested follow-ups\n- thing one\n- thing two\n";
        let out = write_followups(dir.path(), &spec, Verdict::Partial, report).unwrap();
        assert_eq!(out, dir.path().join(".printer/followups/003-followups.md"));
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("Verdict: PARTIAL"));
        assert!(body.contains("- thing one"));
        assert!(body.contains("- thing two"));
    }

    #[test]
    fn write_followups_writes_none_when_section_missing() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("foo.md");
        std::fs::write(&spec, "x").unwrap();
        let out = write_followups(dir.path(), &spec, Verdict::Pass, "## Verdict\nPASS\n").unwrap();
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("Verdict: PASS"));
        assert!(body.trim_end().ends_with("none"));
    }

    #[test]
    fn ignores_pass_in_per_item_findings() {
        // No `## Verdict` section, but the body uses MET / MISSING tokens.
        // We still scan the whole text — accept the first hit, prefer PARTIAL
        // over FAIL/PASS when ambiguous.
        let report = "## Per-item findings\n- foo MET\n- bar MISSING (partial coverage)\n";
        assert_eq!(parse_verdict(report), Verdict::Partial);
    }
}
