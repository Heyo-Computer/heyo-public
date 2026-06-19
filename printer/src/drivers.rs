//! Sandbox / VM drivers contributed by installed plugins.
//!
//! A driver is a plugin role (sibling of `[[hooks]]`) that lets printer
//! dispatch the agent inside an isolated environment — typically a heyvm
//! worktree — instead of the host cwd. The framework is transport-agnostic;
//! every lifecycle step is a shell template the plugin author writes.
//!
//! See `HOOKS.md` ("Sandbox drivers") for the user-facing schema.
//!
//! Lifecycle:
//!   1. `create`   — provision the sandbox; must print the handle on stdout.
//!   2. `sync_in`  — (optional) push the host cwd into the sandbox.
//!   3. `enter`    — wrap each child command (the agent CLI) so it executes
//!      inside the sandbox. `{child}` is the shell-quoted argv of the
//!      original command.
//!   4. `sync_out` — (optional) pull artifacts back to the host.
//!   5. `destroy`  — (optional) tear the sandbox down. Runs from `Drop` so
//!      it fires even on panic / early return.

use crate::config::{SandboxCommands, SandboxDriverChoice};
use crate::plugins::store;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Kinds of drivers we know about. Currently only `vm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    Vm,
}

/// Driver block as declared in a plugin manifest. Templates use `{var}`
/// substitution with the same syntax as hook commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverSpec {
    pub kind: DriverKind,
    /// Provision a sandbox. Must print the handle (id / name / path) on stdout.
    pub create: String,
    /// Wrap a child command. Must include the literal `{child}` placeholder;
    /// the agent's argv is shell-quoted and substituted in.
    pub enter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_in: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_out: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<String>,
    /// Preflight command run inside the sandbox immediately after `create`.
    /// Wrapped through `enter`. Failure aborts the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_create: Option<String>,
}

/// A driver loaded off disk: spec + the plugin that owns it.
#[derive(Debug, Clone)]
pub struct ResolvedDriver {
    pub plugin: String,
    /// Install root for the plugin. Reserved for future driver features (e.g.
    /// resolving relative paths in driver-contributed assets) — see T-016.
    #[allow(dead_code)]
    pub plugin_dir: PathBuf,
    pub spec: DriverSpec,
}

impl ResolvedDriver {
    /// Apply per-step overrides from the user's global config on top of the
    /// plugin's manifest. Returns a freshly merged driver; the original is
    /// untouched. The result is re-validated so a malformed override is
    /// caught before any sandbox is created.
    pub fn with_overrides(&self, c: &SandboxCommands) -> Result<Self> {
        let mut spec = self.spec.clone();
        if let Some(s) = &c.create {
            spec.create = s.clone();
        }
        if let Some(s) = &c.enter {
            spec.enter = s.clone();
        }
        if let Some(s) = &c.destroy {
            spec.destroy = Some(s.clone());
        }
        if let Some(s) = &c.sync_in {
            spec.sync_in = Some(s.clone());
        }
        if let Some(s) = &c.sync_out {
            spec.sync_out = Some(s.clone());
        }
        if let Some(s) = &c.post_create {
            spec.post_create = Some(s.clone());
        }
        validate_driver(&spec)
            .with_context(|| format!("sandbox.commands override for driver `{}`", self.plugin))?;
        Ok(Self {
            plugin: self.plugin.clone(),
            plugin_dir: self.plugin_dir.clone(),
            spec,
        })
    }
}

/// Snapshot of every driver from every installed plugin.
#[derive(Debug, Clone, Default)]
pub struct DriverSet {
    drivers: Vec<ResolvedDriver>,
}

impl DriverSet {
    /// Load `[driver]` blocks from every installed plugin under
    /// `~/.printer/plugins/`. A plugin whose manifest has no driver
    /// contributes nothing.
    pub fn load_installed() -> Result<Self> {
        let plugins_root = match store::plugins_dir() {
            Ok(p) => p,
            Err(_) => return Ok(Self::default()),
        };
        if !plugins_root.is_dir() {
            return Ok(Self::default());
        }
        let mut drivers: Vec<ResolvedDriver> = Vec::new();
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
            if let Some(spec) = manifest.driver.clone() {
                drivers.push(ResolvedDriver {
                    plugin: manifest.name.clone(),
                    plugin_dir: dir,
                    spec,
                });
            }
        }
        Ok(Self { drivers })
    }

    /// Iterate all loaded drivers (used by `printer drivers list`-style code
    /// and tests).
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &ResolvedDriver> {
        self.drivers.iter()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    /// Pick the single active driver. Returns `None` if no plugin contributes
    /// one. Errors if multiple do — selection between competing drivers is
    /// the global config's job (see [`Self::resolve`]); this framework just
    /// refuses to guess.
    pub fn pick_active(&self) -> Result<Option<&ResolvedDriver>> {
        match self.drivers.as_slice() {
            [] => Ok(None),
            [only] => Ok(Some(only)),
            many => {
                let names: Vec<&str> = many.iter().map(|d| d.plugin.as_str()).collect();
                bail!(
                    "multiple installed plugins declare a [driver] block ({}); \
                     set `sandbox.driver` in ~/.printer/config.toml to pick one",
                    names.join(", ")
                );
            }
        }
    }

    /// Pick the active driver honoring the user's `sandbox.driver` choice:
    ///
    /// - `Auto` falls through to [`Self::pick_active`].
    /// - `Off` always returns `None`.
    /// - `Named(n)` looks up by plugin name; errors if no installed plugin
    ///   contributes a driver under that name.
    pub fn resolve(&self, choice: &SandboxDriverChoice) -> Result<Option<&ResolvedDriver>> {
        match choice {
            SandboxDriverChoice::Off => Ok(None),
            SandboxDriverChoice::Auto => self.pick_active(),
            SandboxDriverChoice::Named(name) => {
                let Some(d) = self.drivers.iter().find(|d| d.plugin == *name) else {
                    let installed: Vec<&str> =
                        self.drivers.iter().map(|d| d.plugin.as_str()).collect();
                    let installed_msg = if installed.is_empty() {
                        "none installed".to_string()
                    } else {
                        format!("installed: {}", installed.join(", "))
                    };
                    bail!(
                        "sandbox.driver = \"{name}\" but no installed plugin contributes that driver ({installed_msg})"
                    );
                };
                Ok(Some(d))
            }
        }
    }
}

/// Validate a `[driver]` spec at install time. Returns the validated spec.
/// Same shape as `hooks::resolve_hook` — runs in the install path so a broken
/// plugin can't even land on disk.
pub fn validate_driver(spec: &DriverSpec) -> Result<()> {
    if spec.create.trim().is_empty() {
        bail!("driver `create` template is empty");
    }
    if spec.enter.trim().is_empty() {
        bail!("driver `enter` template is empty");
    }
    if !spec.enter.contains("{child}") {
        bail!("driver `enter` template must contain `{{child}}` placeholder");
    }
    Ok(())
}

/// Variables visible to driver templates. Built once per sandbox; the
/// resulting map feeds [`interpolate`] for each lifecycle step.
#[derive(Debug, Clone, Default)]
pub struct DriverContext {
    pub cwd: PathBuf,
    pub spec: Option<PathBuf>,
    pub handle: Option<String>,
    pub base_image: Option<String>,
    pub spec_slug: Option<String>,
    pub task_id: Option<String>,
}

impl DriverContext {
    fn vars(&self) -> BTreeMap<&'static str, String> {
        let mut m = BTreeMap::new();
        m.insert("cwd", self.cwd.display().to_string());
        if let Some(s) = &self.spec {
            m.insert("spec", s.display().to_string());
        }
        if let Some(h) = &self.handle {
            m.insert("handle", h.clone());
        }
        if let Some(b) = &self.base_image {
            m.insert("base_image", b.clone());
        }
        if let Some(s) = &self.spec_slug {
            m.insert("spec_slug", s.clone());
        }
        if let Some(t) = &self.task_id {
            m.insert("task_id", t.clone());
        }
        m
    }
}

/// Derive a sandbox-name-safe slug from a spec path. Keeps ASCII alphanumerics,
/// `-` and `_`; everything else (including slashes and the file extension) is
/// either stripped or replaced with `-`. Empty input → `"spec"`.
pub fn make_spec_slug(spec_path: &Path) -> String {
    let stem = spec_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut out = String::with_capacity(stem.len());
    let mut prev_dash = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "spec".to_string()
    } else {
        out
    }
}

/// Substitute `{var}` in `template`. Unknown vars are left as-is (matches
/// hook interpolation semantics so plugins can use `{name}` for their own
/// purposes without risk of mangling).
pub fn interpolate(template: &str, vars: &BTreeMap<&'static str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = template[i + 1..].find('}') {
                let name = &template[i + 1..i + 1 + end];
                if let Some(val) = vars.get(name) {
                    out.push_str(val);
                    i += end + 2;
                    continue;
                }
                out.push_str(&template[i..i + end + 2]);
                i += end + 2;
                continue;
            }
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

/// POSIX shell single-quote: wrap the string in `'…'` and escape any embedded
/// single quotes via the `'\''` idiom. Used to build a safe `{child}` argv.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Quote each token of `argv` and join with spaces. Empty argv → empty string.
pub fn shell_quote_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A live sandbox: a created handle plus the driver to drive it. Holds the
/// destroy command in `Drop` so the sandbox is torn down even on panic / early
/// return.
pub struct ActiveSandbox {
    driver: ResolvedDriver,
    handle: String,
    ctx: DriverContext,
    /// Suppress `destroy` if the user has asked for the sandbox to persist
    /// (debugging hook). `Drop` consults this.
    keep: bool,
}

impl ActiveSandbox {
    /// Provision a sandbox by running `create`. Captures stdout as the handle.
    /// Errors if the driver's create command fails or prints an empty handle.
    /// If the driver has a `post_create` step, it runs (wrapped via `enter`)
    /// inside the freshly created sandbox; failure tears the sandbox down.
    pub fn create(
        driver: ResolvedDriver,
        cwd: PathBuf,
        spec: Option<PathBuf>,
        base_image: Option<String>,
        task_id: Option<String>,
    ) -> Result<Self> {
        let spec_slug = spec.as_deref().map(make_spec_slug);
        let mut ctx = DriverContext {
            cwd,
            spec,
            handle: None,
            base_image,
            spec_slug,
            task_id,
        };
        let create_cmd = interpolate(&driver.spec.create, &ctx.vars());
        eprintln!(
            "[printer] driver[{}] create: {}",
            driver.plugin, create_cmd
        );
        let out = Command::new("sh")
            .arg("-c")
            .arg(&create_cmd)
            .current_dir(&ctx.cwd)
            .output()
            .with_context(|| format!("spawning driver `{}` create", driver.plugin))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "driver `{}` create failed (exit {}): {}",
                driver.plugin,
                out.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }
        let handle = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if handle.is_empty() {
            bail!(
                "driver `{}` create succeeded but printed no handle on stdout",
                driver.plugin
            );
        }
        ctx.handle = Some(handle.clone());
        eprintln!(
            "[printer] driver[{}] handle: {}",
            driver.plugin, handle
        );
        let sandbox = Self {
            driver,
            handle,
            ctx,
            keep: false,
        };
        sandbox.run_post_create()?;
        Ok(sandbox)
    }

    /// Run the optional `post_create` template inside the sandbox, wrapped via
    /// `enter`. The sandbox is already live, so any failure here triggers
    /// `Drop` (which fires `destroy`) — the user's view is "create failed; the
    /// sandbox is gone".
    fn run_post_create(&self) -> Result<()> {
        let Some(tmpl) = self.driver.spec.post_create.as_deref() else {
            return Ok(());
        };
        let cmd = interpolate(tmpl, &self.ctx.vars());
        let wrapped = self.wrap_child(&cmd);
        eprintln!(
            "[printer] driver[{}] post_create: {}",
            self.driver.plugin, cmd
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(&wrapped)
            .current_dir(&self.ctx.cwd)
            .status()
            .with_context(|| format!("spawning driver `{}` post_create", self.driver.plugin))?;
        if !status.success() {
            bail!(
                "driver `{}` post_create failed (exit {})",
                self.driver.plugin,
                status.code().unwrap_or(-1)
            );
        }
        Ok(())
    }

    /// Substitute `{child}` in the resolved `enter` template with the
    /// shell-quoted form of `cmd` so the resulting string can be handed to
    /// `sh -c` and run inside the sandbox.
    fn wrap_child(&self, cmd: &str) -> String {
        let mut vars = self.ctx.vars();
        vars.insert("child", shell_quote(cmd));
        interpolate(&self.driver.spec.enter, &vars)
    }

    pub fn handle(&self) -> &str {
        &self.handle
    }

    pub fn plugin(&self) -> &str {
        &self.driver.plugin
    }

    /// If set, the sandbox is *not* destroyed on drop. Useful for debugging.
    /// Wired up to a CLI flag in a follow-up; the field is consumed in `Drop`.
    #[allow(dead_code)]
    pub fn set_keep(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// Resolve the `enter` template with `{handle}`/`{cwd}`/`{spec}` baked in.
    /// `{child}` is left untouched — call sites substitute it with the
    /// shell-quoted argv of the agent process.
    pub fn enter_template(&self) -> String {
        interpolate(&self.driver.spec.enter, &self.ctx.vars())
    }

    /// Run the optional `sync_in` template. No-op when unset.
    pub fn sync_in(&self) -> Result<()> {
        self.run_optional("sync_in", self.driver.spec.sync_in.as_deref())
    }

    /// Run the optional `sync_out` template. Failures are logged and
    /// swallowed — sync-out is best-effort cleanup.
    pub fn sync_out(&self) {
        if let Err(e) = self.run_optional("sync_out", self.driver.spec.sync_out.as_deref()) {
            eprintln!(
                "[printer] driver[{}] sync_out: {e} (ignored)",
                self.driver.plugin
            );
        }
    }

    fn run_optional(&self, label: &str, tmpl: Option<&str>) -> Result<()> {
        let Some(tmpl) = tmpl else { return Ok(()) };
        let cmd_str = interpolate(tmpl, &self.ctx.vars());
        eprintln!(
            "[printer] driver[{}] {label}: {}",
            self.driver.plugin, cmd_str
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(&self.ctx.cwd)
            .status()
            .with_context(|| {
                format!(
                    "spawning driver `{}` {label}",
                    self.driver.plugin
                )
            })?;
        if !status.success() {
            bail!(
                "driver `{}` {label} failed (exit {})",
                self.driver.plugin,
                status.code().unwrap_or(-1)
            );
        }
        Ok(())
    }
}

impl Drop for ActiveSandbox {
    fn drop(&mut self) {
        if self.keep {
            eprintln!(
                "[printer] driver[{}] keeping sandbox {} (--keep-sandbox)",
                self.driver.plugin, self.handle
            );
            return;
        }
        let Some(tmpl) = self.driver.spec.destroy.as_deref() else {
            return;
        };
        let cmd_str = interpolate(tmpl, &self.ctx.vars());
        eprintln!(
            "[printer] driver[{}] destroy: {}",
            self.driver.plugin, cmd_str
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(&self.ctx.cwd)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!(
                "[printer] driver[{}] destroy exited {} (sandbox {} may be orphaned)",
                self.driver.plugin,
                s.code().unwrap_or(-1),
                self.handle
            ),
            Err(e) => eprintln!(
                "[printer] driver[{}] destroy failed to spawn: {e}",
                self.driver.plugin
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dspec(create: &str, enter: &str) -> DriverSpec {
        DriverSpec {
            kind: DriverKind::Vm,
            create: create.into(),
            enter: enter.into(),
            sync_in: None,
            sync_out: None,
            destroy: None,
            post_create: None,
        }
    }

    fn rd(name: &str, spec: DriverSpec) -> ResolvedDriver {
        ResolvedDriver {
            plugin: name.into(),
            plugin_dir: PathBuf::from(format!("/p/{name}")),
            spec,
        }
    }

    #[test]
    fn validate_rejects_missing_child() {
        let s = dspec("vm-create", "ssh vm exec --");
        assert!(validate_driver(&s).is_err());
    }

    #[test]
    fn validate_accepts_minimal() {
        let s = dspec("vm-create", "ssh vm -- {child}");
        validate_driver(&s).unwrap();
    }

    #[test]
    fn validate_rejects_empty_create() {
        let s = dspec("", "wrap {child}");
        assert!(validate_driver(&s).is_err());
    }

    #[test]
    fn pick_active_with_zero() {
        let set = DriverSet::default();
        assert!(set.pick_active().unwrap().is_none());
    }

    #[test]
    fn pick_active_with_one() {
        let set = DriverSet {
            drivers: vec![rd("heyvm", dspec("c", "e {child}"))],
        };
        let active = set.pick_active().unwrap().unwrap();
        assert_eq!(active.plugin, "heyvm");
    }

    #[test]
    fn pick_active_with_many_errors() {
        let set = DriverSet {
            drivers: vec![
                rd("heyvm", dspec("c", "e {child}")),
                rd("other", dspec("c", "e {child}")),
            ],
        };
        let err = set.pick_active().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("multiple"));
        assert!(msg.contains("heyvm"));
        assert!(msg.contains("other"));
    }

    #[test]
    fn resolve_off_short_circuits() {
        let set = DriverSet {
            drivers: vec![rd("heyvm", dspec("c", "e {child}"))],
        };
        assert!(set.resolve(&SandboxDriverChoice::Off).unwrap().is_none());
    }

    #[test]
    fn resolve_auto_with_many_errors() {
        let set = DriverSet {
            drivers: vec![
                rd("heyvm", dspec("c", "e {child}")),
                rd("other", dspec("c", "e {child}")),
            ],
        };
        assert!(set.resolve(&SandboxDriverChoice::Auto).is_err());
    }

    #[test]
    fn resolve_named_picks_specific() {
        let set = DriverSet {
            drivers: vec![
                rd("heyvm", dspec("c", "e {child}")),
                rd("other", dspec("c", "e {child}")),
            ],
        };
        let active = set
            .resolve(&SandboxDriverChoice::Named("other".into()))
            .unwrap()
            .unwrap();
        assert_eq!(active.plugin, "other");
    }

    #[test]
    fn resolve_named_missing_errors() {
        let set = DriverSet {
            drivers: vec![rd("heyvm", dspec("c", "e {child}"))],
        };
        let err = set
            .resolve(&SandboxDriverChoice::Named("missing".into()))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"));
        assert!(msg.contains("heyvm"));
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("a b c"), "'a b c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_argv_joins() {
        let argv = vec!["claude".into(), "--print".into(), "hello world".into()];
        assert_eq!(shell_quote_argv(&argv), "'claude' '--print' 'hello world'");
    }

    #[test]
    fn interpolate_substitutes_handle_and_cwd() {
        let mut vars = BTreeMap::new();
        vars.insert("handle", "vm-123".to_string());
        vars.insert("cwd", "/work".to_string());
        let s = interpolate("ssh {handle} cd {cwd} && {child}", &vars);
        // {child} is unknown to this map — left intact.
        assert_eq!(s, "ssh vm-123 cd /work && {child}");
    }

    #[test]
    fn interpolate_leaves_unknown_intact() {
        let vars = BTreeMap::new();
        let s = interpolate("hello {nope}", &vars);
        assert_eq!(s, "hello {nope}");
    }

    #[test]
    fn spec_slug_strips_path_and_extension() {
        assert_eq!(
            make_spec_slug(Path::new("specs/004-heyvm-plugin-hooks.md")),
            "004-heyvm-plugin-hooks"
        );
    }

    #[test]
    fn spec_slug_replaces_unsafe_chars() {
        assert_eq!(make_spec_slug(Path::new("foo bar.baz.md")), "foo-bar-baz");
    }

    #[test]
    fn spec_slug_falls_back_when_empty() {
        assert_eq!(make_spec_slug(Path::new("")), "spec");
        assert_eq!(make_spec_slug(Path::new("....md")), "spec");
    }

    #[test]
    fn overrides_replace_create_and_post_create() {
        let driver = rd("heyvm", dspec("plugin-create", "wrap {child}"));
        let cmds = SandboxCommands {
            create: Some("override-create".into()),
            post_create: Some("setup".into()),
            ..Default::default()
        };
        let merged = driver.with_overrides(&cmds).unwrap();
        assert_eq!(merged.spec.create, "override-create");
        assert_eq!(merged.spec.enter, "wrap {child}");
        assert_eq!(merged.spec.post_create.as_deref(), Some("setup"));
    }

    #[test]
    fn overrides_fall_through_when_unset() {
        let driver = rd("heyvm", dspec("plugin-create", "wrap {child}"));
        let merged = driver.with_overrides(&SandboxCommands::default()).unwrap();
        assert_eq!(merged.spec.create, "plugin-create");
        assert_eq!(merged.spec.enter, "wrap {child}");
    }

    #[test]
    fn overrides_revalidate_merged_spec() {
        let driver = rd("heyvm", dspec("plugin-create", "wrap {child}"));
        // override `enter` with a template missing `{child}` — must error.
        let cmds = SandboxCommands {
            enter: Some("ssh vm exec --".into()),
            ..Default::default()
        };
        let err = driver.with_overrides(&cmds).unwrap_err();
        assert!(err.to_string().contains("sandbox.commands override"));
    }

    #[test]
    fn driver_context_exposes_base_image_and_slug() {
        let ctx = DriverContext {
            cwd: PathBuf::from("/work"),
            spec: None,
            handle: Some("vm-1".into()),
            base_image: Some("alpine:3.19".into()),
            spec_slug: Some("foo".into()),
            task_id: None,
        };
        let s = interpolate(
            "create --base {base_image} --name printer-{spec_slug} on {handle}",
            &ctx.vars(),
        );
        assert_eq!(s, "create --base alpine:3.19 --name printer-foo on vm-1");
    }
}
