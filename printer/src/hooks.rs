//! Lifecycle hooks contributed by installed plugins.
//!
//! See `HOOKS.md` for the full design and event reference. This module is
//! intentionally thin: it loads hook declarations out of plugin manifests,
//! interpolates `{vars}` into commands, and runs the matching CLI hooks.
//! Agent-hook payloads (skill paths and prompt injections) are surfaced to
//! `prompts.rs` via [`HookSet::agent_for`], not executed here.

use crate::plugins::store;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    BeforeInit,
    AfterInit,
    BeforeExec,
    AfterExec,
    BeforeRun,
    AfterRun,
    BeforeReview,
    AfterReview,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::BeforeInit => "before_init",
            Event::AfterInit => "after_init",
            Event::BeforeExec => "before_exec",
            Event::AfterExec => "after_exec",
            Event::BeforeRun => "before_run",
            Event::AfterRun => "after_run",
            Event::BeforeReview => "before_review",
            Event::AfterReview => "after_review",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "before_init" => Event::BeforeInit,
            "after_init" => Event::AfterInit,
            "before_exec" => Event::BeforeExec,
            "after_exec" => Event::AfterExec,
            "before_run" => Event::BeforeRun,
            "after_run" => Event::AfterRun,
            "before_review" => Event::BeforeReview,
            "after_review" => Event::AfterReview,
            _ => return None,
        })
    }

    fn is_before(self) -> bool {
        matches!(
            self,
            Event::BeforeInit | Event::BeforeExec | Event::BeforeRun | Event::BeforeReview
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    Fail,
    Warn,
    Ignore,
}

/// One hook entry as declared in a plugin manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    #[serde(rename = "type")]
    pub kind: HookKind,
    pub event: String,
    /// CLI command (when `kind = cli`) or agent prompt-injection text
    /// (when `kind = agent`). Mutually exclusive with `skill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Path (relative to the plugin's directory) to a SKILL.md or skill
    /// directory. Agent-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// CLI-only. Default: `fail` for `before_*`, `warn` for `after_*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<OnFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    Cli,
    Agent,
}

/// A loaded hook + the plugin it came from + its resolved on-disk root
/// (for resolving relative `skill` paths).
#[derive(Debug, Clone)]
pub struct Hook {
    pub plugin: String,
    pub plugin_dir: PathBuf,
    pub event: Event,
    pub spec: HookSpec,
}

/// Snapshot of every hook from every installed plugin.
#[derive(Debug, Clone, Default)]
pub struct HookSet {
    hooks: Vec<Hook>,
}

impl HookSet {
    /// Load hook declarations from every installed plugin under
    /// `~/.printer/plugins/`. Plugins with no `[[hooks]]` contribute nothing.
    /// Plugins with malformed hooks are skipped with a warning so a single
    /// broken plugin can't take the whole pipeline down.
    pub fn load_installed() -> Result<Self> {
        let plugins_root = match store::plugins_dir() {
            Ok(p) => p,
            Err(_) => return Ok(Self::default()),
        };
        let mut hooks: Vec<Hook> = Vec::new();
        if !plugins_root.is_dir() {
            return Ok(Self::default());
        }
        for entry in std::fs::read_dir(&plugins_root)
            .with_context(|| format!("reading {}", plugins_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let manifest = match store::read_manifest(&dir) {
                Ok(m) => m,
                Err(_) => continue, // not a plugin dir
            };
            for spec in &manifest.hooks {
                match resolve_hook(&manifest.name, &dir, spec.clone()) {
                    Ok(h) => hooks.push(h),
                    Err(e) => {
                        eprintln!("[printer] skipping hook in plugin `{}`: {e}", manifest.name)
                    }
                }
            }
        }
        Ok(Self { hooks })
    }

    /// `printer hooks list` impl. `event_filter`, if given, restricts to
    /// hooks matching that event name (parses via [`Event::parse`]).
    pub fn print_list(&self, event_filter: Option<&str>) -> Result<()> {
        let filter = match event_filter {
            Some(s) => match Event::parse(s) {
                Some(e) => Some(e),
                None => bail!("unknown event `{s}`"),
            },
            None => None,
        };
        if self.hooks.is_empty() {
            println!("(no hooks registered; install a plugin with `[[hooks]]` in its manifest)");
            return Ok(());
        }
        for h in &self.hooks {
            if let Some(f) = filter
                && h.event != f
            {
                continue;
            }
            let payload = match (h.spec.kind, &h.spec.command, &h.spec.skill) {
                (HookKind::Cli, Some(c), _) => format!("cli: {c}"),
                (HookKind::Agent, Some(c), _) => format!("agent-cmd: {c}"),
                (HookKind::Agent, _, Some(s)) => format!("agent-skill: {s}"),
                _ => "(invalid)".to_string(),
            };
            println!(
                "{event:<14}  [{plugin}]  {payload}",
                event = h.event.as_str(),
                plugin = h.plugin
            );
        }
        Ok(())
    }

    /// Hooks that match a given event.
    pub fn for_event(&self, event: Event) -> impl Iterator<Item = &Hook> {
        self.hooks.iter().filter(move |h| h.event == event)
    }

    /// Run every CLI hook bound to `event`, with `ctx` interpolated into
    /// each command. Behaviour on hook failure is controlled per-hook by
    /// `on_failure`.
    pub fn run_cli(&self, event: Event, ctx: &HookContext) -> Result<()> {
        for h in self.for_event(event) {
            if h.spec.kind != HookKind::Cli {
                continue;
            }
            let Some(cmd_template) = &h.spec.command else {
                eprintln!(
                    "[printer] hook `{}/{}` has no command; skipping",
                    h.plugin,
                    event.as_str()
                );
                continue;
            };
            let interpolated = interpolate(cmd_template, ctx);
            let policy = h.spec.on_failure.unwrap_or(default_on_failure(event));
            eprintln!(
                "[printer] hook[{}] {} ({}): {}",
                h.plugin,
                event.as_str(),
                policy_str(policy),
                interpolated
            );
            let status = Command::new("sh")
                .arg("-c")
                .arg(&interpolated)
                .current_dir(&ctx.cwd)
                .envs(ctx.env_vars(&h.plugin))
                .status()
                .with_context(|| format!("spawning hook `{}` for {}", h.plugin, event.as_str()))?;
            if status.success() {
                continue;
            }
            let exit = status.code().unwrap_or(-1);
            match policy {
                OnFailure::Fail => bail!(
                    "hook `{}` ({}) failed with exit {}",
                    h.plugin,
                    event.as_str(),
                    exit
                ),
                OnFailure::Warn => eprintln!(
                    "[printer] warning: hook `{}` ({}) exited {} (continuing)",
                    h.plugin,
                    event.as_str(),
                    exit
                ),
                OnFailure::Ignore => {}
            }
        }
        Ok(())
    }

    /// Resolve agent-hook contributions for `event`: an injected prompt
    /// segment and a list of skill paths to expose to the agent. Both can be
    /// empty if no agent hooks bind to this event.
    pub fn agent_for(&self, event: Event) -> AgentContribution {
        let mut prompt_chunks: Vec<(String, String)> = Vec::new();
        let mut skills: Vec<PathBuf> = Vec::new();
        for h in self.for_event(event) {
            if h.spec.kind != HookKind::Agent {
                continue;
            }
            if let Some(text) = &h.spec.command {
                prompt_chunks.push((h.plugin.clone(), text.trim().to_string()));
            }
            if let Some(rel) = &h.spec.skill {
                skills.push(h.plugin_dir.join(rel));
            }
        }
        AgentContribution {
            prompt_chunks,
            skills,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentContribution {
    /// `(plugin_name, instruction_text)` pairs.
    pub prompt_chunks: Vec<(String, String)>,
    /// Skill paths (absolute) to be exposed to the agent.
    pub skills: Vec<PathBuf>,
}

impl AgentContribution {
    pub fn is_empty(&self) -> bool {
        self.prompt_chunks.is_empty() && self.skills.is_empty()
    }

    /// Render the prompt-injection block. Returns `None` when there's
    /// nothing to inject, so the caller can omit the heading entirely.
    pub fn render_prompt_block(&self) -> Option<String> {
        if self.prompt_chunks.is_empty() {
            return None;
        }
        let mut out = String::from("\nAdditional instructions from plugin hooks:\n");
        for (plugin, text) in &self.prompt_chunks {
            out.push_str(&format!("- ({plugin}) {text}\n"));
        }
        Some(out)
    }
}

/// Variables visible to hook commands. Construct with [`HookContext::new`]
/// and fill in optional fields (`spec`, `report_path`, `base_ref`,
/// `exit_status`) with the chained setters.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: Event,
    pub cwd: PathBuf,
    pub spec: Option<PathBuf>,
    pub phase: Option<&'static str>,
    pub exit_status: Option<&'static str>,
    pub base_ref: Option<String>,
    pub report_path: Option<PathBuf>,
}

impl HookContext {
    pub fn new(event: Event, cwd: PathBuf) -> Self {
        Self {
            event,
            cwd,
            spec: None,
            phase: phase_for(event),
            exit_status: None,
            base_ref: None,
            report_path: None,
        }
    }
    pub fn with_spec(mut self, spec: PathBuf) -> Self {
        self.spec = Some(spec);
        self
    }
    pub fn with_exit_status(mut self, ok: bool) -> Self {
        self.exit_status = Some(if ok { "ok" } else { "err" });
        self
    }
    pub fn with_base_ref(mut self, base: impl Into<String>) -> Self {
        self.base_ref = Some(base.into());
        self
    }
    pub fn with_report_path(mut self, p: PathBuf) -> Self {
        self.report_path = Some(p);
        self
    }

    fn vars(&self) -> BTreeMap<&'static str, String> {
        let mut m = BTreeMap::new();
        m.insert("event", self.event.as_str().to_string());
        m.insert("cwd", self.cwd.display().to_string());
        if let Some(s) = &self.spec {
            m.insert("spec", s.display().to_string());
        }
        if let Some(p) = self.phase {
            m.insert("phase", p.to_string());
        }
        if let Some(e) = self.exit_status {
            m.insert("exit_status", e.to_string());
        }
        if let Some(b) = &self.base_ref {
            m.insert("base_ref", b.clone());
        }
        if let Some(r) = &self.report_path {
            m.insert("report_path", r.display().to_string());
        }
        m
    }

    fn env_vars(&self, plugin: &str) -> Vec<(String, String)> {
        let mut v = Vec::new();
        for (k, val) in self.vars() {
            v.push((format!("PRINTER_HOOK_{}", k.to_ascii_uppercase()), val));
        }
        v.push(("PRINTER_PLUGIN".to_string(), plugin.to_string()));
        v
    }
}

fn phase_for(event: Event) -> Option<&'static str> {
    match event {
        Event::BeforeRun | Event::AfterRun => Some("run"),
        Event::BeforeReview | Event::AfterReview => Some("review"),
        _ => None,
    }
}

fn default_on_failure(event: Event) -> OnFailure {
    if event.is_before() {
        OnFailure::Fail
    } else {
        OnFailure::Warn
    }
}

fn policy_str(p: OnFailure) -> &'static str {
    match p {
        OnFailure::Fail => "on_failure=fail",
        OnFailure::Warn => "on_failure=warn",
        OnFailure::Ignore => "on_failure=ignore",
    }
}

/// `{var}` substitution. Unknown vars are left in place — that way a hook
/// command that uses `{X}` for its own purpose isn't mangled.
fn interpolate(template: &str, ctx: &HookContext) -> String {
    let vars = ctx.vars();
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = template[i + 1..].find('}')
        {
            let name = &template[i + 1..i + 1 + end];
            if let Some(val) = vars.get(name) {
                out.push_str(val);
                i += end + 2;
                continue;
            }
            // Unknown var — emit `{name}` literally and keep going.
            out.push_str(&template[i..i + end + 2]);
            i += end + 2;
            continue;
        }
        // Not a `{var}` — push one char and advance.
        let ch_len = template[i..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        out.push_str(&template[i..i + ch_len]);
        i += ch_len;
    }
    out
}

pub(crate) fn resolve_hook(plugin: &str, plugin_dir: &Path, spec: HookSpec) -> Result<Hook> {
    let event = Event::parse(&spec.event)
        .ok_or_else(|| anyhow::anyhow!("unknown event `{}`", spec.event))?;
    match spec.kind {
        HookKind::Cli => {
            if spec.command.is_none() {
                bail!("CLI hook is missing `command`");
            }
            if spec.skill.is_some() {
                bail!("CLI hooks cannot declare `skill`");
            }
        }
        HookKind::Agent => {
            if spec.command.is_none() && spec.skill.is_none() {
                bail!("agent hook must declare either `command` or `skill`");
            }
            if spec.command.is_some() && spec.skill.is_some() {
                bail!("agent hook cannot declare both `command` and `skill`");
            }
        }
    }
    Ok(Hook {
        plugin: plugin.to_string(),
        plugin_dir: plugin_dir.to_path_buf(),
        event,
        spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> HookContext {
        HookContext::new(Event::AfterReview, PathBuf::from("/work"))
            .with_spec(PathBuf::from("/work/spec.md"))
            .with_exit_status(true)
            .with_base_ref("main")
            .with_report_path(PathBuf::from("/work/review.md"))
    }

    #[test]
    fn interpolates_known_vars() {
        let s = interpolate("{event} {phase} {spec} {exit_status} {base_ref}", &ctx());
        assert_eq!(s, "after_review review /work/spec.md ok main");
    }

    #[test]
    fn leaves_unknown_vars_intact() {
        let s = interpolate("notify {channel} that {event} fired", &ctx());
        assert_eq!(s, "notify {channel} that after_review fired");
    }

    #[test]
    fn handles_empty_braces_and_trailing_brace() {
        let s = interpolate("{} stays, trailing { also", &ctx());
        assert_eq!(s, "{} stays, trailing { also");
    }

    #[test]
    fn event_round_trip() {
        for e in [
            Event::BeforeInit,
            Event::AfterInit,
            Event::BeforeExec,
            Event::AfterExec,
            Event::BeforeRun,
            Event::AfterRun,
            Event::BeforeReview,
            Event::AfterReview,
        ] {
            assert_eq!(Event::parse(e.as_str()), Some(e));
        }
    }

    #[test]
    fn defaults_on_failure_match_phase() {
        assert_eq!(default_on_failure(Event::BeforeRun), OnFailure::Fail);
        assert_eq!(default_on_failure(Event::AfterRun), OnFailure::Warn);
    }

    #[test]
    fn agent_hook_requires_command_or_skill() {
        let dir = PathBuf::from("/p");
        let s = HookSpec {
            kind: HookKind::Agent,
            event: "before_run".to_string(),
            command: None,
            skill: None,
            on_failure: None,
        };
        assert!(resolve_hook("p", &dir, s).is_err());
    }

    #[test]
    fn agent_hook_rejects_both_command_and_skill() {
        let dir = PathBuf::from("/p");
        let s = HookSpec {
            kind: HookKind::Agent,
            event: "before_run".to_string(),
            command: Some("x".into()),
            skill: Some("y".into()),
            on_failure: None,
        };
        assert!(resolve_hook("p", &dir, s).is_err());
    }

    #[test]
    fn cli_hook_rejects_skill() {
        let dir = PathBuf::from("/p");
        let s = HookSpec {
            kind: HookKind::Cli,
            event: "after_run".to_string(),
            command: Some("x".into()),
            skill: Some("y".into()),
            on_failure: None,
        };
        assert!(resolve_hook("p", &dir, s).is_err());
    }

    #[test]
    fn agent_contribution_renders_only_when_nonempty() {
        let mut a = AgentContribution::default();
        assert!(a.render_prompt_block().is_none());
        a.prompt_chunks.push(("p1".into(), "do the thing".into()));
        let s = a.render_prompt_block().unwrap();
        assert!(s.contains("p1"));
        assert!(s.contains("do the thing"));
    }
}
