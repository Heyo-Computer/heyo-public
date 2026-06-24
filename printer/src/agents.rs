//! Agent backends contributed by installed plugins.
//!
//! An agent is a plugin role (sibling of `[[hooks]]` and `[driver]`) that lets
//! a plugin contribute a launch command for an external agent process — today
//! only ACP servers like `claude-code-acp` or Poolside, but the schema is
//! kind-tagged so future kinds (one-shot CLIs, websocket transports, etc.)
//! can be added without a breaking change.
//!
//! Selection is by name: users pick an agent with `--agent acp:<name>`, which
//! looks up an `[[agent]]` block whose `name` matches across every installed
//! plugin's manifest.
//!
//! See `HOOKS.md` ("ACP agents") for the user-facing schema.

use crate::cli::AgentKind;
use crate::plugins::store;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Kinds of plugin-contributed agents we know about. Currently only `acp`.
/// Distinct from `cli::AgentKind`, which models the user's `--agent` choice
/// and includes built-in (claude/opencode) backends; this enum is just the
/// `kind = …` field on an `[[agent]]` manifest block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSpecKind {
    Acp,
}

/// One `[[agent]]` block as declared in a plugin manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    pub kind: AgentSpecKind,
    /// Lookup name. Must be unique across all installed plugins.
    pub name: String,
    /// Launch command (binary path or program name on `$PATH`).
    pub command: String,
    /// Argv tokens appended to `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables passed to the spawned child. Values are taken as
    /// literal strings — no shell or `${env:…}` expansion is performed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// An agent loaded off disk: spec + the plugin that owns it.
#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub plugin: String,
    /// Install root for the plugin. Reserved for future agent features (e.g.
    /// resolving relative paths in agent-contributed assets).
    #[allow(dead_code)]
    pub plugin_dir: PathBuf,
    pub spec: AgentSpec,
}

/// Snapshot of every agent from every installed plugin.
#[derive(Debug, Clone, Default)]
pub struct AgentSet {
    agents: Vec<ResolvedAgent>,
}

/// Names that may not be used by plugin-contributed agents.
///
/// "acp" is reserved because it's the namespace prefix in `--agent acp:<name>`;
/// a plugin named "acp" would parse as the bare ACP form (which expects
/// `--acp-bin`) and confuse the resolver.
///
/// "claude", "codex", "amp", and "opencode" are reserved because they are the built-in
/// `--agent` names. While plugin agents only resolve under the `acp:<name>`
/// prefix (so `--agent acp:opencode` is unambiguous from `--agent opencode`),
/// reserving them prevents confusion and keeps the natural names available
/// for the built-in one-shot backends.
const RESERVED_AGENT_NAMES: &[&str] = &["acp", "claude", "codex", "amp", "opencode"];

impl AgentSet {
    /// Load `[[agent]]` blocks from every installed plugin. Errors if two
    /// plugins contribute an agent with the same `name` — selection by name
    /// would otherwise be ambiguous.
    pub fn load_installed() -> Result<Self> {
        let plugins_root = match store::plugins_dir() {
            Ok(p) => p,
            Err(_) => return Ok(Self::default()),
        };
        if !plugins_root.is_dir() {
            return Ok(Self::default());
        }
        let mut agents: Vec<ResolvedAgent> = Vec::new();
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
                Err(_) => continue,
            };
            for spec in &manifest.agents {
                if let Some(existing) = agents.iter().find(|a| a.spec.name == spec.name) {
                    bail!(
                        "agent name `{}` is contributed by both plugin `{}` and plugin `{}`; \
                         agent names must be unique across installed plugins",
                        spec.name,
                        existing.plugin,
                        manifest.name,
                    );
                }
                agents.push(ResolvedAgent {
                    plugin: manifest.name.clone(),
                    plugin_dir: dir.clone(),
                    spec: spec.clone(),
                });
            }
        }
        Ok(Self { agents })
    }

    /// Iterate all loaded agents (used by tests; reserved for a future
    /// `printer agents list`-style command).
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &ResolvedAgent> {
        self.agents.iter()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Look up an installed agent by name. Errors if no plugin contributes one
    /// under that name; the message lists what is installed so the user can
    /// see what they typed wrong.
    pub fn resolve(&self, name: &str) -> Result<&ResolvedAgent> {
        if let Some(a) = self.agents.iter().find(|a| a.spec.name == name) {
            return Ok(a);
        }
        let installed: Vec<String> = self
            .agents
            .iter()
            .map(|a| format!("{} (from `{}`)", a.spec.name, a.plugin))
            .collect();
        let installed_msg = if installed.is_empty() {
            "none installed".to_string()
        } else {
            format!("installed: {}", installed.join(", "))
        };
        bail!("--agent acp:{name} but no installed plugin contributes that agent ({installed_msg})")
    }
}

/// Resolved ACP launch parameters: command, argv tail, and env. Built by
/// `resolve_acp_launch` from the `--agent`/`--acp-bin`/`--acp-arg` triple plus
/// the installed plugin manifests.
#[derive(Debug, Default, Clone)]
pub struct AcpLaunch {
    pub bin: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Resolve the launch command for the chosen `--agent`. Returns an empty
/// `AcpLaunch` for non-ACP backends. For `--agent acp:<name>` the plugin
/// manifest's `command`/`args`/`env` provide the defaults; explicit `--acp-bin`
/// overrides the binary, and `--acp-arg` tokens are appended after the
/// manifest's argv.
pub fn resolve_acp_launch(
    kind: &AgentKind,
    explicit_bin: Option<&str>,
    explicit_args: &[String],
) -> Result<AcpLaunch> {
    match kind {
        AgentKind::Claude | AgentKind::Codex | AgentKind::Amp | AgentKind::Opencode => {
            Ok(AcpLaunch::default())
        }
        AgentKind::Acp { name: None } => Ok(AcpLaunch {
            bin: explicit_bin.map(|s| s.to_string()),
            args: explicit_args.to_vec(),
            env: BTreeMap::new(),
        }),
        AgentKind::Acp { name: Some(name) } => {
            let set = AgentSet::load_installed()?;
            let resolved = set.resolve(name)?;
            let bin = explicit_bin
                .map(|s| s.to_string())
                .unwrap_or_else(|| resolved.spec.command.clone());
            let mut args = resolved.spec.args.clone();
            args.extend(explicit_args.iter().cloned());
            Ok(AcpLaunch {
                bin: Some(bin),
                args,
                env: resolved.spec.env.clone(),
            })
        }
    }
}

/// Validate one `[[agent]]` spec at install time. Same shape as
/// `validate_driver` — runs in the install path so a malformed manifest
/// cannot land on disk.
pub fn validate_agent(spec: &AgentSpec) -> Result<()> {
    if spec.name.trim().is_empty() {
        bail!("agent `name` is empty");
    }
    if RESERVED_AGENT_NAMES.contains(&spec.name.as_str()) {
        bail!(
            "agent name `{}` is reserved (it would shadow a built-in `--agent` choice)",
            spec.name
        );
    }
    if spec.command.trim().is_empty() {
        bail!("agent `{}` has empty `command`", spec.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aspec(name: &str, command: &str) -> AgentSpec {
        AgentSpec {
            kind: AgentSpecKind::Acp,
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn ra(plugin: &str, spec: AgentSpec) -> ResolvedAgent {
        ResolvedAgent {
            plugin: plugin.into(),
            plugin_dir: PathBuf::from(format!("/p/{plugin}")),
            spec,
        }
    }

    #[test]
    fn validate_rejects_empty_command() {
        let s = aspec("foo", "");
        assert!(validate_agent(&s).is_err());
    }

    #[test]
    fn validate_rejects_reserved_name() {
        let s = aspec("acp", "x");
        let err = validate_agent(&s).unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn validate_allows_built_in_agent_names_for_acp_plugins() {
        // Plugin agents resolve under the `acp:<name>` prefix, so they
        // don't shadow the built-in one-shot backends. But the names are
        // still reserved to prevent confusion.
        assert!(validate_agent(&aspec("claude", "claude-code-acp")).is_err());
        assert!(validate_agent(&aspec("codex", "codex")).is_err());
        assert!(validate_agent(&aspec("amp", "amp")).is_err());
        assert!(validate_agent(&aspec("opencode", "opencode")).is_err());
    }

    #[test]
    fn validate_rejects_empty_name() {
        let s = aspec("", "x");
        assert!(validate_agent(&s).is_err());
    }

    #[test]
    fn validate_accepts_minimal() {
        let s = aspec("poolside", "pool");
        validate_agent(&s).unwrap();
    }

    #[test]
    fn resolve_picks_named() {
        let set = AgentSet {
            agents: vec![ra("p1", aspec("a", "x")), ra("p2", aspec("b", "y"))],
        };
        let r = set.resolve("b").unwrap();
        assert_eq!(r.spec.name, "b");
        assert_eq!(r.plugin, "p2");
    }

    #[test]
    fn resolve_missing_lists_installed() {
        let set = AgentSet {
            agents: vec![ra("p1", aspec("a", "x"))],
        };
        let err = set.resolve("missing").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"));
        assert!(msg.contains("a (from `p1`)"));
    }

    #[test]
    fn resolve_missing_when_empty() {
        let set = AgentSet::default();
        let err = set.resolve("any").unwrap_err();
        assert!(err.to_string().contains("none installed"));
    }
}
