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
    /// Artifact stores, kept apart from `contexts` on purpose.
    ///
    /// A store is not an app-lb: it is a different service, on a different host
    /// more often than not, authenticated by a shared key rather than by a
    /// username and password. Folding it into `ContextEntry` would mean every
    /// context carrying four fields most of them never use, and `--server`
    /// silently pointing the registry commands at a load balancer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub registries: BTreeMap<String, RegistryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_registry: Option<String>,
}

/// One artifact store: where it is and how to prove you may write to it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub url: String,
    /// `ART_API_KEY`, presented as `Authorization: Bearer`. Stored in the clear
    /// in a `0600` file, exactly as a context's password is; `api_key_command`
    /// is here for anyone who would rather it live in a keychain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// A shell command whose stdout is the key. Takes precedence over a stored
    /// `api_key`; run via `sh -c` on every request that needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_command: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub insecure_skip_tls_verify: bool,
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

    /// The same question for an artifact store's key, which arrives by different
    /// flags. Two methods rather than a format argument because the answer is
    /// what somebody reads when a `401` surprises them, and naming the wrong
    /// flag there sends them to edit something that was never in play.
    pub fn describe_api_key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Flag => "--api-key / SERVERCTL_ART_API_KEY",
            Self::Command => "api_key_command",
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

    /// The artifact store this invocation should use, by the same rules as
    /// [`resolve`](Self::resolve): `--registry`, else `current_registry`, else
    /// the only one if there is exactly one.
    ///
    /// Unlike a context there is no built-in default. An app-lb answers on a
    /// known loopback port; a store does not, and guessing one would mean
    /// `artifact push` could upload a rootfs to whatever happens to be
    /// listening.
    pub fn resolve_registry(&self, requested: Option<&str>) -> Result<Option<(String, RegistryEntry)>> {
        if let Some(name) = requested {
            let entry = self.registries.get(name).with_context(|| {
                format!("no registry named {name:?} — `serverctl artifact registries` lists them")
            })?;
            return Ok(Some((name.to_string(), entry.clone())));
        }
        if let Some(name) = &self.current_registry
            && let Some(entry) = self.registries.get(name)
        {
            return Ok(Some((name.clone(), entry.clone())));
        }
        if self.registries.len() == 1 {
            let (name, entry) = self.registries.iter().next().expect("len == 1");
            return Ok(Some((name.clone(), entry.clone())));
        }
        if self.current_registry.is_some() {
            bail!(
                "current registry {:?} is not in the config file — pick one with \
                 `serverctl artifact use`",
                self.current_registry.as_deref().unwrap_or_default()
            );
        }
        Ok(None)
    }
}

/// A registry resolved against the command line: what this invocation will use.
#[derive(Debug, Clone)]
pub struct RegistryEndpoint {
    pub name: String,
    pub url: String,
    pub api_key: Option<String>,
    pub api_key_source: PasswordSource,
    pub insecure_skip_tls_verify: bool,
}

/// Merge a stored registry with the command-line/env overrides.
///
/// Errors rather than defaulting when nothing names a store: see
/// [`Config::resolve_registry`] for why there is no fallback URL.
pub fn resolve_registry_endpoint(
    config: &Config,
    registry: Option<&str>,
    url: Option<&str>,
    api_key: Option<&str>,
    insecure: bool,
) -> Result<RegistryEndpoint> {
    let resolved = config.resolve_registry(registry)?;
    let (name, entry) = match resolved {
        Some((n, e)) => (n, e),
        None if url.is_some() => ("(none)".to_string(), RegistryEntry::default()),
        None => bail!(
            "no artifact store configured — run `serverctl artifact login <url>` first, \
             or pass --registry-url"
        ),
    };

    let (api_key, api_key_source) = match api_key {
        Some(k) => (Some(k.to_string()), PasswordSource::Flag),
        None => match &entry.api_key_command {
            Some(cmd) => (Some(run_password_command(cmd)?), PasswordSource::Command),
            None => match &entry.api_key {
                Some(k) => (Some(k.clone()), PasswordSource::Stored),
                None => (None, PasswordSource::None),
            },
        },
    };

    Ok(RegistryEndpoint {
        name,
        url: url
            .map(str::to_string)
            .or_else(|| (!entry.url.is_empty()).then(|| entry.url.clone()))
            .ok_or_else(|| anyhow::anyhow!("the stored registry has no url"))?,
        api_key,
        api_key_source,
        insecure_skip_tls_verify: insecure || entry.insecure_skip_tls_verify,
    })
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
            ..Default::default()
        }
    }

    fn cfg_with_registries(names: &[&str], current: Option<&str>) -> Config {
        Config {
            current_registry: current.map(str::to_string),
            registries: names
                .iter()
                .map(|n| {
                    (
                        n.to_string(),
                        RegistryEntry {
                            url: format!("http://{n}:8080"),
                            api_key: Some("k".into()),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_lone_registry_is_used_without_being_selected() {
        let cfg = cfg_with_registries(&["store"], None);
        let ep = resolve_registry_endpoint(&cfg, None, None, None, false).unwrap();
        assert_eq!(ep.name, "store");
        assert_eq!(ep.url, "http://store:8080");
        assert_eq!(ep.api_key_source, PasswordSource::Stored);
    }

    #[test]
    fn no_registry_is_an_error_rather_than_a_guessed_url() {
        // A push has to go somewhere deliberate: there is no conventional port
        // for an artifact store the way there is for an app-lb admin listener.
        assert!(resolve_registry_endpoint(&Config::default(), None, None, None, false).is_err());
        // ...unless the URL is given outright, which is deliberate enough.
        let ep = resolve_registry_endpoint(
            &Config::default(),
            None,
            Some("http://art:8080"),
            Some("key"),
            false,
        )
        .unwrap();
        assert_eq!(ep.url, "http://art:8080");
        assert_eq!(ep.api_key_source, PasswordSource::Flag);
    }

    #[test]
    fn an_unknown_registry_is_an_error() {
        assert!(cfg_with_registries(&["prod"], None).resolve_registry(Some("dev")).is_err());
    }

    #[test]
    fn registries_and_contexts_are_stored_side_by_side_without_colliding() {
        let mut cfg = cfg_with(&["prod"], Some("prod"));
        cfg.registries = cfg_with_registries(&["store"], None).registries;
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.contexts["prod"].server, "http://prod:9090");
        assert_eq!(back.registries["store"].url, "http://store:8080");
    }

    #[test]
    fn a_config_written_before_registries_existed_still_parses() {
        let old = r#"{"current_context":"prod","contexts":{"prod":{"server":"http://prod:9090"}}}"#;
        let cfg: Config = serde_json::from_str(old).unwrap();
        assert!(cfg.registries.is_empty());
        assert!(cfg.current_registry.is_none());
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
