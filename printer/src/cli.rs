use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Parser, Debug)]
#[command(
    name = "printer",
    about = "Drive a Claude/Codex/Amp/opencode session against a markdown spec"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Write a starter spec file in the canonical format.
    Init(InitArgs),
    /// Plan and execute a markdown spec.
    Run(RunArgs),
    /// Generate a detailed plan from a spec without executing any work.
    /// Writes a checkpoint to `.printer/plan.checkpoint` and lets the agent
    /// optionally ask the user clarifying questions before locking the plan in.
    Plan(PlanArgs),
    /// Review the working tree against the original spec.
    Review(ReviewArgs),
    /// Click-test a UI/web change end-to-end with the `computer` tool. Runs on
    /// the host (a display is required); exits non-PASS with guidance if no
    /// display or the `computer` CLI is unavailable.
    Test(TestArgs),
    /// Run-then-review in one shot, with crash-safe `--continue`.
    Exec(ExecArgs),
    /// Show the archive of completed execs (`.printer/history.json`).
    History(HistoryArgs),
    /// Generate a new numbered spec from a saved follow-ups file.
    /// Spawns one agent turn that converts `.printer/followups/<spec>.md`
    /// (produced by `printer review`) into `specs/NNN-<slug>.md`.
    SpecFromFollowups(SpecFromFollowupsArgs),
    /// Mark a spec's exec checkpoint as completed or cancelled without
    /// driving an agent (`printer spec complete|cancel <spec>`).
    #[command(subcommand_help_heading = "Spec subcommands")]
    Spec(SpecArgs),
    /// File-based task tracking (create / list / start / done / ...).
    #[command(subcommand_help_heading = "Task subcommands")]
    Task(crate::tasks::TaskArgs),
    /// Install a plugin into ~/.printer/plugins/.
    AddPlugin(crate::plugins::AddPluginArgs),
    /// Reinstall an installed plugin from its recorded source — refreshes the
    /// snapshot under `~/.printer/plugins/<name>/` after editing the plugin's
    /// source manifest in-tree (the common case for `path:` installs). Pass
    /// `--all` to refresh every installed plugin.
    ReinstallPlugin(ReinstallPluginArgs),
    /// List installed plugins.
    Plugins,
    /// Inspect the lifecycle hooks installed plugins have registered.
    #[command(subcommand_help_heading = "Hook subcommands")]
    Hooks(HooksArgs),
    /// Inspect or edit the global config at `~/.printer/config.toml`.
    #[command(subcommand_help_heading = "Config subcommands")]
    Config(ConfigArgs),
    /// Forward to an installed plugin: `printer <plugin> <args>...`.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Where to write the spec template.
    ///
    /// Two modes, chosen by whether `.printer/` already exists in the cwd:
    ///
    /// - **Fresh repo** (no `.printer/`): treated as a path. Defaults to
    ///   `spec.md`.
    /// - **Existing printer repo** (`.printer/` present): treated as a slug
    ///   and the spec is written to `specs/NNN-<slug>.md` with `NNN`
    ///   auto-incremented. Required in this mode.
    pub path: Option<PathBuf>,

    /// Project title used in the spec's top-level heading.
    #[arg(long, short, default_value = "New Project")]
    pub title: String,

    /// Overwrite an existing file at the target path.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Path to the markdown spec / todo file.
    pub spec: PathBuf,

    /// Which agent to drive.
    #[arg(long, default_value_t = AgentKind::Claude, value_parser = parse_agent_kind)]
    pub agent: AgentKind,

    /// Override the model (passed through to the agent).
    #[arg(long)]
    pub model: Option<String>,

    /// Hard cap on driver turns (excluding the bootstrap turn).
    #[arg(long, default_value_t = 40)]
    pub max_turns: u32,

    /// Cumulative input tokens at which we rotate to a fresh session.
    #[arg(long, default_value_t = 150_000)]
    pub compact_at: u64,

    /// Working directory for the child agent process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Permission mode passed to the child agent. Defaults to bypassPermissions
    /// because there is no human at the keyboard to approve prompts during a
    /// non-interactive driver run. Set to a stricter value if you want
    /// approvals to block the run.
    #[arg(long, default_value = "bypassPermissions")]
    pub permission_mode: String,

    /// Show a live spinner and periodic heartbeats so you can see the agent
    /// is still working during long turns.
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,

    /// Skip auto-spawning a `codegraph watch` daemon for the run. By default
    /// printer launches one (if `codegraph` is installed) so the index stays
    /// fresh as the agent edits files.
    #[arg(long, default_value_t = false)]
    pub no_codegraph_watch: bool,

    /// Skip the "no plugins installed" interactive check. Use this in CI where
    /// stdin is not a terminal and the run is intentionally plugin-free.
    #[arg(long, default_value_t = false)]
    pub skip_plugin_check: bool,

    /// Skip dispatching the agent inside a sandbox driver, even if an
    /// installed plugin contributes one. Useful for debugging on the host.
    #[arg(long, default_value_t = false)]
    pub no_sandbox: bool,

    /// Path/command to launch the ACP agent server. Required with bare
    /// `--agent acp`; optional with `--agent acp:<name>`, where it overrides
    /// the binary the plugin's `[[agent]]` block points at.
    #[arg(long)]
    pub acp_bin: Option<String>,

    /// Repeatable extra arg appended to the ACP server's argv. With
    /// `--agent acp:<name>` these append to the plugin manifest's `args`.
    #[arg(long = "acp-arg", value_name = "ARG")]
    pub acp_args: Vec<String>,

    /// Internal: skip the planning_pass turn at the start of the run.
    /// Set by `printer exec` when resuming from `Phase::Running` (planning
    /// already completed in a prior attempt). Hidden from `--help` because
    /// `printer run` users have no need to set it manually — a fresh `run`
    /// always wants planning.
    #[arg(skip)]
    pub skip_planning: bool,

    /// Internal: path to the per-spec exec checkpoint
    /// (`.printer/exec/<key>.json`). When set, run.rs writes
    /// `Phase::Running` to it after `planning_pass` completes so a later
    /// `--continue` can skip planning. `None` for standalone `printer run`
    /// invocations (those don't have an exec-level checkpoint).
    #[arg(skip)]
    pub checkpoint_path: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct PlanArgs {
    /// Path to the markdown spec / todo file.
    pub spec: PathBuf,

    /// Which agent to drive.
    #[arg(long, default_value_t = AgentKind::Claude, value_parser = parse_agent_kind)]
    pub agent: AgentKind,

    /// Override the model.
    #[arg(long)]
    pub model: Option<String>,

    /// Working directory for the child agent process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Permission mode passed to the child agent.
    #[arg(long, default_value = "bypassPermissions")]
    pub permission_mode: String,

    /// Show a live spinner / heartbeats during the planning turn.
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,

    /// Skip the optional question/answer round even if the agent asks.
    /// Useful in CI / non-interactive contexts.
    #[arg(long, default_value_t = false)]
    pub no_questions: bool,

    /// Maximum number of question/answer rounds before the plan is forced to
    /// finalize.
    #[arg(long, default_value_t = 3)]
    pub max_question_rounds: u32,

    /// Path/command to launch the ACP agent server when `--agent acp`.
    #[arg(long)]
    pub acp_bin: Option<String>,

    /// Repeatable extra arg appended to the ACP agent binary's argv.
    #[arg(long = "acp-arg", value_name = "ARG")]
    pub acp_args: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct ReviewArgs {
    /// Path to the markdown spec the implementation was driven from.
    pub spec: PathBuf,

    /// Which agent to drive.
    #[arg(long, default_value_t = AgentKind::Claude, value_parser = parse_agent_kind)]
    pub agent: AgentKind,

    /// Override the model.
    #[arg(long)]
    pub model: Option<String>,

    /// Git ref to diff against. Defaults to detected base (HEAD~ or main).
    #[arg(long)]
    pub base: Option<String>,

    /// Working directory for the child agent process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// If set, also write the review report to this path.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Permission mode passed to the child agent. Review is mostly read-only
    /// (`git diff`, file reads) so the default is `bypassPermissions` to keep
    /// the run non-interactive.
    #[arg(long, default_value = "bypassPermissions")]
    pub permission_mode: String,

    /// Make a skill available to the review agent. Accepts a path to a
    /// `SKILL.md`, a single skill directory, or a parent directory of skill
    /// directories (e.g. `skills/`). Repeatable. If omitted, `skills/` in the
    /// agent cwd is auto-discovered.
    #[arg(long = "skill", value_name = "PATH")]
    pub skills: Vec<PathBuf>,

    /// Show a live spinner and periodic heartbeats during the review turn.
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,

    /// Skip dispatching the review agent inside a sandbox driver, even if an
    /// installed plugin contributes one.
    #[arg(long, default_value_t = false)]
    pub no_sandbox: bool,

    /// Force the sandbox even when the diff touches a UI/web surface. By default
    /// such diffs run review on the host (when a display is present) so the
    /// `computer` tool can click-test; this flag disables that auto-host routing.
    #[arg(long, default_value_t = false)]
    pub no_ui_host: bool,

    /// Path/command to launch the ACP agent server when `--agent acp`.
    #[arg(long)]
    pub acp_bin: Option<String>,

    /// Repeatable extra arg appended to the ACP agent binary's argv.
    #[arg(long = "acp-arg", value_name = "ARG")]
    pub acp_args: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct TestArgs {
    /// Path to the markdown spec the change was driven from (context for the
    /// click-test).
    pub spec: PathBuf,

    /// Which agent to drive.
    #[arg(long, default_value_t = AgentKind::Claude, value_parser = parse_agent_kind)]
    pub agent: AgentKind,

    /// Override the model.
    #[arg(long)]
    pub model: Option<String>,

    /// Git ref to diff against when describing what changed. Defaults to the
    /// detected base (main/master or HEAD~1).
    #[arg(long)]
    pub base: Option<String>,

    /// Working directory for the child agent process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Local URL to open for the click-test (e.g. http://localhost:3000). If
    /// omitted, the agent infers how to start the app.
    #[arg(long)]
    pub url: Option<String>,

    /// Permission mode passed to the child agent. The test agent drives the app
    /// (input synthesis) but must not edit project files, so a permissive
    /// default keeps the run non-interactive.
    #[arg(long, default_value = "bypassPermissions")]
    pub permission_mode: String,

    /// Make a skill available to the test agent (path to a `SKILL.md`, a skill
    /// dir, or a parent dir). Repeatable. Defaults to auto-discovering `skills/`
    /// in the agent cwd.
    #[arg(long = "skill", value_name = "PATH")]
    pub skills: Vec<PathBuf>,

    /// Show a live spinner and periodic heartbeats during the test turn.
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,

    /// Path/command to launch the ACP agent server when `--agent acp`.
    #[arg(long)]
    pub acp_bin: Option<String>,

    /// Repeatable extra arg appended to the ACP agent binary's argv.
    #[arg(long = "acp-arg", value_name = "ARG")]
    pub acp_args: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct ExecArgs {
    /// Path to the markdown spec / todo file. Optional with `--continue`
    /// (the spec path is then read from `.printer/exec.json`).
    pub spec: Option<PathBuf>,

    /// Resume a previous `printer exec` from `.printer/exec.json` instead of
    /// starting fresh. If the previous run finished cleanly, jumps to review.
    /// If the previous review finished cleanly, exits without doing anything.
    #[arg(long = "continue", default_value_t = false)]
    pub r#continue: bool,

    /// Which agent to drive.
    #[arg(long, default_value_t = AgentKind::Claude, value_parser = parse_agent_kind)]
    pub agent: AgentKind,

    /// Override the model (passed through to the agent).
    #[arg(long)]
    pub model: Option<String>,

    /// Working directory for the child agent process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Permission mode passed to the child agent.
    #[arg(long, default_value = "bypassPermissions")]
    pub permission_mode: String,

    /// Live spinner / heartbeats during long turns.
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,

    // --- run-phase only ---
    /// Hard cap on driver turns during the run phase (excluding bootstrap).
    #[arg(long, default_value_t = 40)]
    pub max_turns: u32,

    /// Cumulative input tokens at which the run phase rotates the session.
    #[arg(long, default_value_t = 150_000)]
    pub compact_at: u64,

    // --- review-phase only ---
    /// Git ref to diff against during review. Defaults to detected base
    /// (`main` → `master` → `HEAD~1`).
    #[arg(long)]
    pub base: Option<String>,

    /// If set, also write the review report to this path.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// Make a skill available to the review agent. Same semantics as
    /// `printer review --skill`. Repeatable.
    #[arg(long = "skill", value_name = "PATH")]
    pub skills: Vec<PathBuf>,

    /// Skip auto-spawning a `codegraph watch` daemon for the duration of
    /// the exec (run + review). By default printer launches one if
    /// `codegraph` is installed.
    #[arg(long, default_value_t = false)]
    pub no_codegraph_watch: bool,

    /// Skip the "no plugins installed" interactive check. Use this in CI where
    /// stdin is not a terminal and the run is intentionally plugin-free.
    #[arg(long, default_value_t = false)]
    pub skip_plugin_check: bool,

    /// Maximum number of review cycles (review → fix → re-review). Each
    /// non-PASS verdict triggers a fix pass and another review, so this is
    /// the cap on round-trips before exec gives up. Defaults to
    /// `DEFAULT_MAX_REVIEW_PASSES` in `exec.rs` (currently 3). Set to 1 to
    /// disable the cycle entirely (single review, no fix pass).
    #[arg(long)]
    pub max_review_passes: Option<u32>,

    /// Skip dispatching the agent inside a sandbox driver, even if an
    /// installed plugin contributes one. Applies to both run and review
    /// phases of the exec.
    #[arg(long, default_value_t = false)]
    pub no_sandbox: bool,

    /// Enable recursive mode: iterate over open tasks in the spec and run
    /// each one in its own sandbox (requires heyvm plugin).
    #[arg(long, default_value_t = false)]
    pub recursive: bool,

    /// Path/command to launch the ACP agent server when `--agent acp`.
    #[arg(long)]
    pub acp_bin: Option<String>,

    /// Repeatable extra arg appended to the ACP agent binary's argv.
    #[arg(long = "acp-arg", value_name = "ARG")]
    pub acp_args: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct SpecArgs {
    #[command(subcommand)]
    pub command: SpecCommand,
}

#[derive(Subcommand, Debug)]
pub enum SpecCommand {
    /// Force the spec's exec checkpoint to `done`. Use when work was
    /// finished out-of-band and you want `printer exec <spec>` to treat
    /// it as complete.
    Complete(SpecTargetArgs),
    /// Mark the spec's exec checkpoint `cancelled`. Like `complete`, a
    /// bare re-`exec` of the spec archives the cancelled checkpoint and
    /// starts fresh — but listings/history show it was cancelled.
    Cancel(SpecTargetArgs),
}

#[derive(clap::Args, Debug)]
pub struct SpecTargetArgs {
    /// Path to the markdown spec whose checkpoint should be marked.
    pub spec: PathBuf,

    /// Working directory containing `.printer/`. Defaults to the cwd.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct SpecFromFollowupsArgs {
    /// Slug for the new spec (`specs/NNN-<slug>.md`).
    pub name: String,

    /// Path to the follow-ups file. Defaults to the most recently modified
    /// file in `.printer/followups/` of the cwd.
    #[arg(long)]
    pub from: Option<PathBuf>,

    /// Which agent to drive.
    #[arg(long, default_value_t = AgentKind::Claude, value_parser = parse_agent_kind)]
    pub agent: AgentKind,

    /// Override the model.
    #[arg(long)]
    pub model: Option<String>,

    /// Working directory for the child agent process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Permission mode passed to the child agent.
    #[arg(long, default_value = "bypassPermissions")]
    pub permission_mode: String,

    /// Show a live spinner / heartbeats during the turn.
    #[arg(long, short, default_value_t = false)]
    pub verbose: bool,

    /// Overwrite the destination spec if it already exists.
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Path/command to launch the ACP agent server when `--agent acp`.
    #[arg(long)]
    pub acp_bin: Option<String>,

    /// Repeatable extra arg appended to the ACP agent binary's argv.
    #[arg(long = "acp-arg", value_name = "ARG")]
    pub acp_args: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct HistoryArgs {
    /// Working directory containing `.printer/history.json`.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Emit the raw JSON instead of a human-readable summary.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(clap::Args, Debug)]
pub struct ReinstallPluginArgs {
    /// Name of the plugin to reinstall (must already be installed). Required
    /// unless `--all` is set.
    pub name: Option<String>,

    /// Reinstall every installed plugin in name order. Mutually exclusive
    /// with a positional name.
    #[arg(long, default_value_t = false)]
    pub all: bool,
}

#[derive(clap::Args, Debug)]
pub struct HooksArgs {
    #[command(subcommand)]
    pub command: HooksCommand,
}

#[derive(Subcommand, Debug)]
pub enum HooksCommand {
    /// List every hook contributed by every installed plugin.
    List(HooksListArgs),
}

#[derive(clap::Args, Debug)]
pub struct HooksListArgs {
    /// Filter to a single event (e.g. `after_review`).
    #[arg(long)]
    pub event: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Pretty-print the resolved config (defaults included if the file is
    /// missing).
    Show,
    /// Open `~/.printer/config.toml` in `$EDITOR`, seeding it from a default
    /// template if it does not yet exist.
    Edit,
}

/// Which agent backend to drive.
///
/// CLI form (case-insensitive):
/// - `claude` → built-in one-shot Claude CLI
/// - `codex` → built-in one-shot Codex CLI
/// - `amp` → built-in one-shot Amp CLI
/// - `opencode` → built-in one-shot opencode CLI
/// - `acp` → ACP server, launched from `--acp-bin` and `--acp-arg`
/// - `acp:<name>` → ACP server contributed by an installed plugin's
///   `[[agent]]` block (see HOOKS.md)
#[derive(Clone, Debug)]
pub enum AgentKind {
    Claude,
    Codex,
    Amp,
    Opencode,
    Acp { name: Option<String> },
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentKind::Claude => f.write_str("claude"),
            AgentKind::Codex => f.write_str("codex"),
            AgentKind::Amp => f.write_str("amp"),
            AgentKind::Opencode => f.write_str("opencode"),
            AgentKind::Acp { name: None } => f.write_str("acp"),
            AgentKind::Acp { name: Some(n) } => write!(f, "acp:{n}"),
        }
    }
}

/// Clap value parser. Wraps `AgentKind::from_str` so the `--agent` flag accepts
/// the same forms (`claude`, `codex`, `amp`, `opencode`, `acp`, `acp:<name>`) without needing
/// a `ValueEnum` derive — `acp:<name>` carries a payload that ValueEnum can't
/// model.
fn parse_agent_kind(s: &str) -> Result<AgentKind, String> {
    AgentKind::from_str(s)
}

impl FromStr for AgentKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "claude" => Ok(AgentKind::Claude),
            "codex" => Ok(AgentKind::Codex),
            "amp" => Ok(AgentKind::Amp),
            "opencode" => Ok(AgentKind::Opencode),
            "acp" => Ok(AgentKind::Acp { name: None }),
            other => {
                if let Some(name) = other.strip_prefix("acp:") {
                    if name.is_empty() {
                        return Err(
                            "agent `acp:` requires a plugin-contributed name (e.g. acp:poolside)"
                                .into(),
                        );
                    }
                    Ok(AgentKind::Acp {
                        name: Some(name.to_string()),
                    })
                } else {
                    Err(format!(
                        "unknown agent `{s}` (expected one of: claude, codex, amp, opencode, acp, acp:<name>)"
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_builtin_agents() {
        assert!(matches!(
            AgentKind::from_str("claude"),
            Ok(AgentKind::Claude)
        ));
        assert!(matches!(AgentKind::from_str("codex"), Ok(AgentKind::Codex)));
        assert!(matches!(AgentKind::from_str("amp"), Ok(AgentKind::Amp)));
        assert!(matches!(
            AgentKind::from_str("opencode"),
            Ok(AgentKind::Opencode)
        ));
        assert!(matches!(
            AgentKind::from_str("CLAUDE"),
            Ok(AgentKind::Claude)
        ));
        assert!(matches!(AgentKind::from_str("CODEX"), Ok(AgentKind::Codex)));
        assert!(matches!(AgentKind::from_str("AMP"), Ok(AgentKind::Amp)));
    }

    #[test]
    fn parses_bare_acp() {
        assert!(matches!(
            AgentKind::from_str("acp"),
            Ok(AgentKind::Acp { name: None })
        ));
    }

    #[test]
    fn parses_named_acp() {
        match AgentKind::from_str("acp:poolside").unwrap() {
            AgentKind::Acp { name: Some(n) } => assert_eq!(n, "poolside"),
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn rejects_empty_acp_name() {
        let err = AgentKind::from_str("acp:").unwrap_err();
        assert!(err.contains("requires a plugin-contributed name"));
    }

    #[test]
    fn rejects_unknown() {
        assert!(AgentKind::from_str("banana").is_err());
    }

    #[test]
    fn display_round_trips() {
        for s in ["claude", "codex", "amp", "opencode", "acp", "acp:poolside"] {
            let parsed = AgentKind::from_str(s).unwrap();
            assert_eq!(parsed.to_string(), s);
        }
    }
}
