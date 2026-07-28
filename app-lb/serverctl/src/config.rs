//! The serverctl config file: named contexts, kubeconfig-style.
//!
//! One file, `~/.config/serverctl/config.json`, holding a set of named contexts
//! (server + credentials) and which one is current. Written `0600` because it
//! can hold a password — app-lb's admin API authenticates with HTTP Basic, and
//! there is no token endpoint to trade it for something shorter-lived.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_context: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, ContextEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextEntry {
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Stored in the clear. `password_command` exists for anyone who would
    /// rather keep it in a keychain and shell out for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// A shell command whose stdout is the password. Takes precedence over a
    /// stored `password`; run via `sh -c` on every request that needs auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_command: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub insecure_skip_tls_verify: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Where the credentials for the current invocation came from — reported by
/// `whoami` so "why am I getting a 401" has an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSource {
    None,
    Flag,
    Command,
    Stored,
}

impl PasswordSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Flag => "--password / SERVERCTL_PASSWORD",
            Self::Command => "password_command",
            Self::Stored => "stored in the config file",
        }
    }
}

/// A context resolved against the command line: what this invocation will use.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The context name, or `"(none)"` when nothing is configured and the
    /// built-in default server is in play.
    pub name: String,
    pub server: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub password_source: PasswordSource,
    pub insecure_skip_tls_verify: bool,
}

impl Config {
    pub fn path(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        if let Some(dir) = dirs::config_dir() {
            return Ok(dir.join("serverctl").join("config.json"));
        }
        // No XDG config dir (an unusual container, say) — fall back to $HOME.
        let home = dirs::home_dir().context("no config directory and no home directory")?;
        Ok(home.join(".serverctl").join("config.json"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            // An empty file is the same as no file: a half-finished write, or
            // /dev/null passed deliberately to ignore the stored contexts.
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing the config file {}", path.display())),
            // A missing config is the normal first-run state, not an error: the
            // built-in default server makes `serverctl get deployments` work
            // against a local app-lb with no setup at all.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            restrict(parent, 0o700)?;
        }
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        // Written after the fact rather than with `OpenOptions::mode`, so an
        // existing file that was too permissive gets tightened too.
        restrict(path, 0o600)?;
        Ok(())
    }

    /// The context this invocation should use: `--context`, else
    /// `current_context`, else the only context if there is exactly one.
    pub fn resolve(&self, requested: Option<&str>) -> Result<Option<(String, ContextEntry)>> {
        if let Some(name) = requested {
            let entry = self
                .contexts
                .get(name)
                .with_context(|| format!("no context named {name:?} — `serverctl config get-contexts` lists them"))?;
            return Ok(Some((name.to_string(), entry.clone())));
        }
        if let Some(name) = &self.current_context
            && let Some(entry) = self.contexts.get(name)
        {
            return Ok(Some((name.clone(), entry.clone())));
        }
        if self.contexts.len() == 1 {
            let (name, entry) = self.contexts.iter().next().expect("len == 1");
            return Ok(Some((name.clone(), entry.clone())));
        }
        if self.current_context.is_some() {
            bail!(
                "current context {:?} is not in the config file — pick one with \
                 `serverctl config use-context`",
                self.current_context.as_deref().unwrap_or_default()
            );
        }
        Ok(None)
    }
}

/// Merge a stored context with the command-line/env overrides.
pub fn resolve_endpoint(
    config: &Config,
    context: Option<&str>,
    server: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
    insecure: bool,
) -> Result<Endpoint> {
    let resolved = config.resolve(context)?;
    let (name, entry) = match resolved {
        Some((n, e)) => (n, e),
        None => ("(none)".to_string(), ContextEntry::default()),
    };

    // Precedence, highest first: an explicit flag (which clap has already
    // merged the SERVERCTL_* env vars into), then the context, then the
    // built-in default.
    let (password, password_source) = match password {
        Some(p) => (Some(p.to_string()), PasswordSource::Flag),
        None => match &entry.password_command {
            Some(cmd) => (Some(run_password_command(cmd)?), PasswordSource::Command),
            None => match &entry.password {
                Some(p) => (Some(p.clone()), PasswordSource::Stored),
                None => (None, PasswordSource::None),
            },
        },
    };

    Ok(Endpoint {
        name,
        server: server
            .map(str::to_string)
            .or_else(|| (!entry.server.is_empty()).then(|| entry.server.clone()))
            .unwrap_or_else(|| crate::client::DEFAULT_SERVER.to_string()),
        user: user.map(str::to_string).or(entry.user),
        password,
        password_source,
        insecure_skip_tls_verify: insecure || entry.insecure_skip_tls_verify,
    })
}

fn run_password_command(cmd: &str) -> Result<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("running password_command: {cmd}"))?;
    if !out.status.success() {
        bail!(
            "password_command failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let password = String::from_utf8(out.stdout)
        .context("password_command produced output that is not UTF-8")?;
    Ok(password.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting {mode:o} on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(names: &[&str], current: Option<&str>) -> Config {
        Config {
            current_context: current.map(str::to_string),
            contexts: names
                .iter()
                .map(|n| {
                    (
                        n.to_string(),
                        ContextEntry {
                            server: format!("http://{n}:9090"),
                            user: Some("admin".into()),
                            password: Some("pw".into()),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn a_lone_context_is_used_without_being_selected() {
        let cfg = cfg_with(&["prod"], None);
        let (name, _) = cfg.resolve(None).unwrap().unwrap();
        assert_eq!(name, "prod");
    }

    #[test]
    fn an_empty_config_resolves_to_nothing_rather_than_failing() {
        assert!(Config::default().resolve(None).unwrap().is_none());
    }

    #[test]
    fn flags_outrank_the_stored_context() {
        let cfg = cfg_with(&["prod"], Some("prod"));
        let ep = resolve_endpoint(&cfg, None, Some("http://other:1"), None, Some("flag"), false)
            .unwrap();
        assert_eq!(ep.server, "http://other:1");
        assert_eq!(ep.password.as_deref(), Some("flag"));
        assert_eq!(ep.password_source, PasswordSource::Flag);
        assert_eq!(ep.user.as_deref(), Some("admin"), "not overridden, so kept");
    }

    #[test]
    fn no_config_at_all_still_points_at_a_local_app_lb() {
        let ep = resolve_endpoint(&Config::default(), None, None, None, None, false).unwrap();
        assert_eq!(ep.server, crate::client::DEFAULT_SERVER);
        assert_eq!(ep.password_source, PasswordSource::None);
    }

    #[test]
    fn an_unknown_context_is_an_error() {
        assert!(cfg_with(&["prod"], None).resolve(Some("staging")).is_err());
    }
}
