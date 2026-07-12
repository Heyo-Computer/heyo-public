//! User-level configuration at `~/.printer/config.toml`.
//!
//! Currently scoped to sandbox driver preferences: which driver to dispatch
//! through, what base image to ask it for, and (later) which env vars and
//! mounts to forward. Loaded once per `run` / `review` / `exec`; missing file
//! → defaults.
//!
//! See `HOOKS.md` for the user-facing schema.

use crate::plugins::store;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Filename inside `~/.printer/`.
const CONFIG_FILE: &str = "config.toml";

/// Seed contents written by `printer config edit` if the file does not yet
/// exist. Stays in sync with the schema documented in `HOOKS.md`.
pub const DEFAULT_CONFIG: &str = r#"# printer global config (~/.printer/config.toml)
#
# This file is optional. Anything you omit falls back to built-in defaults.

[sandbox]
# Which driver-contributing plugin to dispatch through.
#   "auto" — pick the only installed driver (errors if more than one).
#   "off"  — never sandbox, even if a driver is installed.
#   "<plugin-name>" — pick a specific driver by plugin name.
driver = "auto"

# Forwarded to the driver's templates as {base_image}. For the heyvm plugin
# this is the image string passed to `heyvm create --image …` (e.g.
# "ubuntu:24.04", "alpine:3.19", or any image heyvm knows about).
base_image = "ubuntu:24.04"

# Names of env vars to forward into the sandbox. Driver-specific.
env = []

# Extra read/write mounts (host:guest), beyond cwd which is mounted by default.
mounts = []

# Per-step overrides on top of the active driver's manifest. Any key you set
# here replaces that step's template; anything you omit falls through to the
# plugin's defaults. Same {var} interpolation as the plugin manifest, plus
# {base_image} (from above), {agent_setup} (`--agent claude` or empty), and
# {spec_slug} (the spec basename, sanitized).
[sandbox.commands]
# create = "heyvm create --name printer-{spec_slug} --image {base_image} {agent_setup} --no-ttl --needs-network --mount {cwd}:/workspace --mount $HOME/.claude:$HOME/.claude >&2 && echo printer-{spec_slug}"
# enter = "heyvm exec {handle} --session printer --env IS_SANDBOX=1 -- {child}"
# destroy = "heyvm rm -y {handle}"
# sync_in / sync_out are not used: cwd is bind-mounted, so file edits
# round-trip live and there's nothing to copy.

# Optional preflight script. Wrapped through `enter` so it runs inside the
# sandbox right after create. Failure aborts the run.
# post_create = "bash -lc 'cargo fetch || true'"
"#;

/// Top-level config. Every field is `serde(default)` so a partial config file
/// never errors at parse time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

/// Sandbox preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub driver: SandboxDriverChoice,
    #[serde(default = "default_base_image")]
    pub base_image: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<String>,
    #[serde(default)]
    pub commands: SandboxCommands,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            driver: SandboxDriverChoice::default(),
            base_image: default_base_image(),
            env: Vec::new(),
            mounts: Vec::new(),
            commands: SandboxCommands::default(),
        }
    }
}

/// Per-step overrides applied on top of the active driver's manifest. Any
/// field left as `None` falls through to the manifest's template. `{var}`
/// interpolation works the same as for plugin-declared driver templates;
/// see `HOOKS.md` for the available vars.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxCommands {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_in: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_out: Option<String>,
    /// Optional preflight script run *inside* the sandbox right after
    /// `create` succeeds. Wrapped through `enter`. Failure aborts the run;
    /// use shell short-circuits (`|| true`) if you want it to be best-effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_create: Option<String>,
}

fn default_base_image() -> String {
    "ubuntu:24.04".to_string()
}

/// What the user wants `pick_active` to return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SandboxDriverChoice {
    /// Pick the single installed driver (error if more than one).
    #[default]
    Auto,
    /// Disable sandboxing regardless of installed drivers.
    Off,
    /// Pick the driver contributed by this plugin name.
    Named(String),
}

impl Serialize for SandboxDriverChoice {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => ser.serialize_str("auto"),
            Self::Off => ser.serialize_str("off"),
            Self::Named(n) => ser.serialize_str(n),
        }
    }
}

impl<'de> Deserialize<'de> for SandboxDriverChoice {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(match s.as_str() {
            "auto" => Self::Auto,
            "off" => Self::Off,
            _ => Self::Named(s),
        })
    }
}

/// Path to the (possibly non-existent) config file.
pub fn config_path() -> Result<PathBuf> {
    Ok(store::data_dir()?.join(CONFIG_FILE))
}

/// Load `~/.printer/config.toml`, falling back to defaults if it does not
/// exist. Parse errors propagate so the user sees a clear message.
pub fn load() -> Result<GlobalConfig> {
    let path = config_path()?;
    load_from(&path)
}

fn load_from(path: &Path) -> Result<GlobalConfig> {
    if !path.exists() {
        return Ok(GlobalConfig::default());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: GlobalConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

/// CLI: `printer config show` — pretty-print the resolved config so users
/// can see what's actually in effect (including defaults).
pub fn cli_show() -> Result<()> {
    let path = config_path()?;
    let cfg = load_from(&path)?;
    let exists = path.exists();
    let body = toml::to_string_pretty(&cfg).context("serializing config")?;
    if exists {
        println!("# from {}", path.display());
    } else {
        println!("# (no config file at {}; showing defaults)", path.display());
    }
    print!("{body}");
    Ok(())
}

/// CLI: `printer config edit` — open the config in `$EDITOR`, seeding it
/// from [`DEFAULT_CONFIG`] when the file does not yet exist.
pub fn cli_edit() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        std::fs::write(&path, DEFAULT_CONFIG)
            .with_context(|| format!("seeding {}", path.display()))?;
        eprintln!("[printer] seeded {}", path.display());
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("spawning $EDITOR ({editor})"))?;
    if !status.success() {
        anyhow::bail!("$EDITOR ({editor}) exited with {}", status);
    }
    // Re-parse so the user gets immediate feedback if they broke the file.
    load_from(&path).with_context(|| format!("re-parsing {} after edit", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn defaults_when_file_missing() {
        let d = tmp();
        let cfg = load_from(&d.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.sandbox.driver, SandboxDriverChoice::Auto);
        assert_eq!(cfg.sandbox.base_image, "ubuntu:24.04");
        assert!(cfg.sandbox.env.is_empty());
        assert!(cfg.sandbox.mounts.is_empty());
    }

    #[test]
    fn parses_partial_file() {
        let d = tmp();
        let p = d.path().join("config.toml");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "[sandbox]\ndriver = \"off\"").unwrap();
        let cfg = load_from(&p).unwrap();
        assert_eq!(cfg.sandbox.driver, SandboxDriverChoice::Off);
        assert_eq!(cfg.sandbox.base_image, "ubuntu:24.04");
    }

    #[test]
    fn parses_named_driver() {
        let d = tmp();
        let p = d.path().join("config.toml");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, "[sandbox]\ndriver = \"heyvm\"").unwrap();
        let cfg = load_from(&p).unwrap();
        assert_eq!(
            cfg.sandbox.driver,
            SandboxDriverChoice::Named("heyvm".into())
        );
    }

    #[test]
    fn parses_full_file() {
        let d = tmp();
        let p = d.path().join("config.toml");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(
            f,
            "[sandbox]\n\
             driver = \"auto\"\n\
             base_image = \"alpine:3.19\"\n\
             env = [\"FOO\", \"BAR\"]\n\
             mounts = [\"/cache:/cache\"]"
        )
        .unwrap();
        let cfg = load_from(&p).unwrap();
        assert_eq!(cfg.sandbox.driver, SandboxDriverChoice::Auto);
        assert_eq!(cfg.sandbox.base_image, "alpine:3.19");
        assert_eq!(cfg.sandbox.env, vec!["FOO", "BAR"]);
        assert_eq!(cfg.sandbox.mounts, vec!["/cache:/cache"]);
        assert!(cfg.sandbox.commands.create.is_none());
        assert!(cfg.sandbox.commands.post_create.is_none());
    }

    #[test]
    fn parses_sandbox_commands() {
        let d = tmp();
        let p = d.path().join("config.toml");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(
            f,
            "[sandbox.commands]\n\
             create = \"vm create --name printer-{{spec_slug}}\"\n\
             enter = \"vm exec {{handle}} -- {{child}}\"\n\
             post_create = \"setup.sh\""
        )
        .unwrap();
        let cfg = load_from(&p).unwrap();
        assert_eq!(
            cfg.sandbox.commands.create.as_deref(),
            Some("vm create --name printer-{spec_slug}")
        );
        assert_eq!(
            cfg.sandbox.commands.enter.as_deref(),
            Some("vm exec {handle} -- {child}")
        );
        assert_eq!(
            cfg.sandbox.commands.post_create.as_deref(),
            Some("setup.sh")
        );
        assert!(cfg.sandbox.commands.destroy.is_none());
    }

    #[test]
    fn rejects_invalid_toml() {
        let d = tmp();
        let p = d.path().join("config.toml");
        fs::write(&p, "this is = not = valid").unwrap();
        assert!(load_from(&p).is_err());
    }

    #[test]
    fn default_template_round_trips() {
        let d = tmp();
        let p = d.path().join("config.toml");
        fs::write(&p, DEFAULT_CONFIG).unwrap();
        let cfg = load_from(&p).unwrap();
        assert_eq!(cfg.sandbox.driver, SandboxDriverChoice::Auto);
        assert_eq!(cfg.sandbox.base_image, "ubuntu:24.04");
    }
}
