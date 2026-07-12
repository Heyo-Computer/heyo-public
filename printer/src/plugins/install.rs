use super::cli::AddPluginArgs;
use super::registry::{self, KnownInstaller};
use super::source::{self, SourceManifest};
use super::store::{self, Manifest, Source};
use crate::tasks::model::now_iso;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Top-level entry point for `printer add-plugin`.
pub fn add_plugin(args: AddPluginArgs) -> Result<()> {
    let resolved = resolve_spec(&args)?;

    let name = resolved.name.clone();
    let plugin_dir = store::plugin_dir(&name)?;

    if store::installed(&name)? && !args.force {
        bail!(
            "plugin `{name}` is already installed at {}; pass --force to reinstall",
            plugin_dir.display()
        );
    }
    if args.force && plugin_dir.exists() {
        eprintln!(
            "[printer] --force: removing existing {}",
            plugin_dir.display()
        );
        fs::remove_dir_all(&plugin_dir)
            .with_context(|| format!("removing {}", plugin_dir.display()))?;
    }
    fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("creating {}", plugin_dir.display()))?;

    let installed = match resolved.kind {
        ResolvedKind::CargoGit { url, subdir } => {
            install_cargo_git(&plugin_dir, &url, args.rev.as_deref(), subdir.as_deref())?
        }
        ResolvedKind::CargoPath { path } => install_cargo_path(&plugin_dir, &path)?,
        ResolvedKind::Shell { command, binary } => install_shell(&command, &binary)?,
    };

    // Read optional `printer-plugin.toml` from the source dir, validate its
    // hooks, and copy any declared asset files alongside the binary so paths
    // referenced by hooks resolve at runtime.
    let (declared_hooks, declared_assets, declared_driver, declared_agents) =
        match installed.source_dir.as_deref() {
            Some(src) => {
                let sm = SourceManifest::load(src)?;
                let hooks = sm.validate_hooks(&name, &plugin_dir)?;
                let driver = sm.validate_driver()?;
                let agents = sm.validate_agents()?;
                (hooks, sm.assets, driver, agents)
            }
            None => (Vec::new(), Vec::new(), None, Vec::new()),
        };
    if let Some(src) = installed.source_dir.as_deref()
        && !declared_assets.is_empty()
    {
        source::copy_assets(src, &plugin_dir, &declared_assets)?;
    }
    let hook_count = declared_hooks.len();
    let has_driver = declared_driver.is_some();
    let agent_count = declared_agents.len();

    let manifest = Manifest {
        name: name.clone(),
        version: installed.version,
        binary: installed.binary,
        installed_at: now_iso(),
        source: installed.source,
        hooks: declared_hooks,
        driver: declared_driver,
        agents: declared_agents,
    };
    store::write_manifest(&plugin_dir, &manifest)?;
    if hook_count > 0 {
        eprintln!("[printer] registered {hook_count} hook(s) for `{name}`");
    }
    if has_driver {
        eprintln!("[printer] registered sandbox driver for `{name}`");
    }
    if agent_count > 0 {
        eprintln!("[printer] registered {agent_count} agent(s) for `{name}`");
    }

    if manifest.binary.is_empty() {
        println!(
            "installed plugin `{name}` v{} (skill-only; contributes hooks/assets)",
            manifest.version
        );
    } else {
        println!(
            "installed plugin `{name}` v{} -> {}",
            manifest.version, manifest.binary
        );
        println!("invoke with: printer {name} <args...>");
    }
    Ok(())
}

/// Re-snapshot a single installed plugin from its recorded `Source`. Used by
/// `printer reinstall-plugin <name>` to refresh a plugin after the user edits
/// the source manifest in-tree (the common case: `path:` installs from a
/// local checkout). Equivalent to running `add-plugin <recorded-spec> --force`
/// without making the user remember the original spec.
pub fn reinstall_plugin(name: &str) -> Result<()> {
    let dir = store::plugin_dir(name)?;
    if !store::installed(name)? {
        bail!(
            "plugin `{name}` is not installed at {}; use `printer add-plugin` to install it",
            dir.display()
        );
    }
    let manifest = store::read_manifest(&dir)?;
    let args = args_from_manifest(&manifest)?;
    eprintln!(
        "[printer] reinstalling `{name}` from {}",
        source_label(&manifest.source)
    );
    add_plugin(args)
}

/// Reinstall every plugin under `~/.printer/plugins/`. Iterates in name order
/// for deterministic output. Continues past individual failures and returns a
/// summary error at the end so one broken plugin doesn't block refreshing the
/// rest — useful right after an across-the-board manifest edit.
pub fn reinstall_all() -> Result<()> {
    let plugins_dir = store::plugins_dir()?;
    let mut names: Vec<String> = Vec::new();
    for entry in
        fs::read_dir(&plugins_dir).with_context(|| format!("reading {}", plugins_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if store::installed(&name)? {
            names.push(name);
        }
    }
    names.sort();
    if names.is_empty() {
        eprintln!(
            "[printer] no plugins installed under {}",
            plugins_dir.display()
        );
        return Ok(());
    }
    let mut failed: Vec<String> = Vec::new();
    for name in &names {
        eprintln!("\n[printer] reinstalling `{name}`...");
        if let Err(e) = reinstall_plugin(name) {
            eprintln!("[printer] reinstall `{name}` failed: {e:#}");
            failed.push(name.clone());
        }
    }
    if !failed.is_empty() {
        bail!(
            "{}/{} plugin(s) failed to reinstall: {}",
            failed.len(),
            names.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

/// Reconstruct an `AddPluginArgs` from a recorded manifest. The constructed
/// args round-trip through `add_plugin` with `force=true`, re-running the
/// install for whichever `Source` variant the plugin was originally installed
/// from. Pure (no I/O) so it's directly testable.
fn args_from_manifest(manifest: &Manifest) -> Result<AddPluginArgs> {
    let force = true;
    let name = Some(manifest.name.clone());
    Ok(match &manifest.source {
        Source::Path { path } => AddPluginArgs {
            spec: format!("path:{path}"),
            name,
            rev: None,
            subdir: None,
            install_cmd: None,
            binary: None,
            force,
        },
        Source::Git { url, rev, subdir } => AddPluginArgs {
            spec: url.clone(),
            name,
            rev: rev.clone(),
            subdir: subdir.clone(),
            install_cmd: None,
            binary: None,
            force,
        },
        Source::Shell { command } => {
            if manifest.binary.is_empty() {
                bail!(
                    "plugin `{}` was installed via a shell installer but its \
                     manifest has no `binary` recorded — cannot reconstruct \
                     the install args. Reinstall manually with \
                     `printer add-plugin {} --install-cmd … --binary …`.",
                    manifest.name,
                    manifest.name
                );
            }
            AddPluginArgs {
                spec: manifest.name.clone(),
                name,
                rev: None,
                subdir: None,
                install_cmd: Some(command.clone()),
                binary: Some(manifest.binary.clone()),
                force,
            }
        }
    })
}

fn source_label(s: &Source) -> String {
    match s {
        Source::Path { path } => format!("path {path}"),
        Source::Git { url, rev, subdir } => {
            let head = match rev {
                Some(r) => format!("git {url}@{}", &r[..r.len().min(8)]),
                None => format!("git {url}"),
            };
            match subdir {
                Some(sd) => format!("{head} (subdir={sd})"),
                None => head,
            }
        }
        Source::Shell { command } => {
            let trimmed = command.trim();
            let cap = trimmed.len().min(40);
            if trimmed.len() > cap {
                format!("shell `{}…`", &trimmed[..cap])
            } else {
                format!("shell `{trimmed}`")
            }
        }
    }
}

struct Installed {
    /// Absolute path (or `bin/<name>` relative to plugin_dir, for cargo).
    binary: String,
    version: String,
    source: Source,
    /// Absolute path to the plugin's source directory, if there is one
    /// (cargo-git / cargo-path). Shell-installer plugins return `None`.
    /// Used to look up an optional `printer-plugin.toml`.
    source_dir: Option<PathBuf>,
}

fn install_cargo_git(
    plugin_dir: &Path,
    url: &str,
    rev: Option<&str>,
    subdir: Option<&Path>,
) -> Result<Installed> {
    let clone_dir = plugin_dir.join("src");
    eprintln!("[printer] cloning {url} -> {}", clone_dir.display());
    run(Command::new("git").args(["clone", url]).arg(&clone_dir)).context("git clone failed")?;
    if let Some(rev) = rev {
        eprintln!("[printer] checking out {rev}");
        run(Command::new("git")
            .current_dir(&clone_dir)
            .args(["checkout", rev]))
        .context("git checkout failed")?;
    }
    let head = read_head_sha(&clone_dir).ok();

    let source_dir = match subdir {
        Some(sd) => {
            let joined = clone_dir.join(sd);
            if !joined.is_dir() {
                bail!(
                    "--subdir {} not found in cloned repo (expected {})",
                    sd.display(),
                    joined.display()
                );
            }
            joined
        }
        None => clone_dir,
    };

    let (binary, version) = if source_dir.join("Cargo.toml").is_file() {
        cargo_install_to(&source_dir, plugin_dir)?
    } else {
        // Skill-only / driver-only plugin shipped inside a git repo.
        eprintln!(
            "[printer] no Cargo.toml at {}; installing as skill-only plugin (no binary)",
            source_dir.display()
        );
        (String::new(), "0.0.0".to_string())
    };

    Ok(Installed {
        binary,
        version,
        source: Source::Git {
            url: url.to_string(),
            rev: head,
            subdir: subdir.and_then(|p| p.to_str().map(|s| s.to_string())),
        },
        source_dir: Some(source_dir),
    })
}

fn install_cargo_path(plugin_dir: &Path, path: &Path) -> Result<Installed> {
    let canon = path
        .canonicalize()
        .with_context(|| format!("resolving local plugin path {}", path.display()))?;
    let (binary, version) = if canon.join("Cargo.toml").is_file() {
        cargo_install_to(&canon, plugin_dir)?
    } else {
        // Skill-only plugin: no binary to build, just hooks/assets. The
        // dispatcher will refuse `printer <name>` invocations on it (see
        // exec_external); contributed hooks/skills still flow through.
        eprintln!(
            "[printer] no Cargo.toml at {}; installing as skill-only plugin (no binary)",
            canon.display()
        );
        (String::new(), "0.0.0".to_string())
    };
    Ok(Installed {
        binary,
        version,
        source: Source::Path {
            path: canon.to_string_lossy().into_owned(),
        },
        source_dir: Some(canon),
    })
}

/// Run `cargo install --path <src> --root <plugin_dir>` and resolve the
/// produced binary's path + version. Returns (relative `bin/<name>`, version).
fn cargo_install_to(src: &Path, plugin_dir: &Path) -> Result<(String, String)> {
    let cargo_toml =
        read_cargo_toml(src).with_context(|| format!("reading {}/Cargo.toml", src.display()))?;
    let package_name = cargo_toml.package.name.clone();
    let version = cargo_toml.package.version.clone();

    let binary_name = match cargo_toml.bin.as_deref() {
        None | Some([]) => package_name.clone(),
        Some([single]) => single.name.clone(),
        Some(many) => bail!(
            "plugin crate declares {} binaries; multi-bin support is not yet implemented",
            many.len()
        ),
    };

    eprintln!(
        "[printer] cargo install --path {} --root {}",
        src.display(),
        plugin_dir.display()
    );
    run(Command::new("cargo")
        .arg("install")
        .arg("--path")
        .arg(src)
        .arg("--root")
        .arg(plugin_dir))
    .context("cargo install failed")?;

    let bin_rel = format!("bin/{binary_name}");
    let bin_abs = plugin_dir.join(&bin_rel);
    if !bin_abs.is_file() {
        bail!(
            "cargo install completed but {} does not exist",
            bin_abs.display()
        );
    }
    // Drop cargo's bookkeeping that we don't need.
    for noise in [".crates.toml", ".crates2.json"] {
        let p = plugin_dir.join(noise);
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
    Ok((bin_rel, version))
}

fn install_shell(command: &str, binary: &str) -> Result<Installed> {
    let resolved_binary = expand_tilde(binary)?;
    eprintln!("[printer] running install command: {command}");
    eprintln!("[printer] expecting binary at: {resolved_binary}");
    run(Command::new("sh").arg("-c").arg(command)).context("install command failed")?;
    let bin_path = PathBuf::from(&resolved_binary);
    if !bin_path.is_file() {
        bail!(
            "install command completed but {} is not a regular file — \
            check that the installer landed the binary where you expected, \
            then re-run with the right --binary path",
            bin_path.display()
        );
    }
    let version = detect_version(&bin_path).unwrap_or_else(|_| "unknown".to_string());
    Ok(Installed {
        binary: resolved_binary,
        version,
        source: Source::Shell {
            command: command.to_string(),
        },
        source_dir: None,
    })
}

fn detect_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin).arg("--version").output()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let line = raw.lines().next().unwrap_or("").trim();
    // Heuristic: take the last whitespace-separated token, strip a leading `v`.
    let tok = line
        .split_whitespace()
        .next_back()
        .unwrap_or("")
        .trim_start_matches('v');
    if tok.is_empty() || tok.chars().any(|c| !c.is_ascii_graphic()) {
        bail!("could not parse version from `{bin:?} --version` output");
    }
    Ok(tok.to_string())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with status {status}", cmd.get_program());
    }
    Ok(())
}

fn read_head_sha(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("git rev-parse")?;
    if !out.status.success() {
        bail!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn expand_tilde(s: &str) -> Result<String> {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").context("$HOME is not set")?;
        Ok(format!("{home}/{rest}"))
    } else if s == "~" {
        std::env::var("HOME").context("$HOME is not set")
    } else {
        Ok(s.to_string())
    }
}

#[derive(Debug)]
struct ResolvedSpec {
    name: String,
    kind: ResolvedKind,
}

#[derive(Debug)]
enum ResolvedKind {
    CargoGit {
        url: String,
        subdir: Option<PathBuf>,
    },
    CargoPath {
        path: PathBuf,
    },
    Shell {
        command: String,
        binary: String,
    },
}

fn resolve_spec(args: &AddPluginArgs) -> Result<ResolvedSpec> {
    let name_override = args.name.as_deref();

    // Validate --subdir up front and convert to PathBuf. Only meaningful for
    // git specs; reject for path:/registry/--install-cmd to avoid silent drops.
    let subdir = match args.subdir.as_deref() {
        Some(s) => Some(validate_subdir(s)?),
        None => None,
    };

    // 1. Explicit --install-cmd wins over everything; spec is just the name.
    if let Some(cmd) = &args.install_cmd {
        if subdir.is_some() {
            bail!("--subdir is not supported with --install-cmd");
        }
        let binary = args
            .binary
            .clone()
            .ok_or_else(|| anyhow!("--install-cmd requires --binary <PATH>"))?;
        let name = name_override
            .map(|s| s.to_string())
            .unwrap_or_else(|| args.spec.clone());
        return Ok(ResolvedSpec {
            name,
            kind: ResolvedKind::Shell {
                command: cmd.clone(),
                binary,
            },
        });
    }

    // 2. Local path?
    if let Some(rest) = args.spec.strip_prefix("path:") {
        if subdir.is_some() {
            bail!(
                "--subdir is not supported with path: specs (point path: at the plugin dir directly)"
            );
        }
        let path = PathBuf::from(rest);
        if !path.exists() {
            let abs = path
                .canonicalize()
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|c| c.join(&path).display().to_string())
                        .unwrap_or_else(|_| path.display().to_string())
                });
            bail!(
                "path:{} does not exist (resolved to {}). \
                 path: specs are relative to your current working directory — \
                 cd to the printer repo root or pass an absolute path",
                path.display(),
                abs
            );
        }
        let inferred = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("cannot infer name from path {}", path.display()))?
            .to_string();
        let name = name_override.unwrap_or(&inferred).to_string();
        return Ok(ResolvedSpec {
            name,
            kind: ResolvedKind::CargoPath { path },
        });
    }

    // 3. Registry name?
    if let Some(known) = registry::lookup(&args.spec) {
        let name = name_override.unwrap_or(known.name).to_string();
        let kind = match &known.installer {
            KnownInstaller::Cargo { git } => ResolvedKind::CargoGit {
                url: git.to_string(),
                subdir: subdir.clone(),
            },
            KnownInstaller::Shell { command, binary } => {
                if subdir.is_some() {
                    bail!(
                        "--subdir is not supported with the `{}` registry entry (shell installer)",
                        args.spec
                    );
                }
                ResolvedKind::Shell {
                    command: command.to_string(),
                    binary: binary.to_string(),
                }
            }
        };
        return Ok(ResolvedSpec { name, kind });
    }

    // 4. Otherwise treat as a git URL. Heuristic: contains "://", "@", or ends in .git.
    if args.spec.contains("://") || args.spec.contains('@') || args.spec.ends_with(".git") {
        // Prefer the subdir basename for the inferred name when it's set —
        // otherwise installing two plugins from one monorepo would collide
        // on the repo basename.
        let inferred = subdir
            .as_deref()
            .and_then(subdir_basename)
            .or_else(|| git_url_basename(&args.spec))
            .ok_or_else(|| anyhow!("cannot infer plugin name from `{}`; pass --name", args.spec))?;
        let name = name_override.unwrap_or(&inferred).to_string();
        return Ok(ResolvedSpec {
            name,
            kind: ResolvedKind::CargoGit {
                url: args.spec.clone(),
                subdir,
            },
        });
    }

    bail!(
        "could not resolve `{}` as a registry name, git URL, or path: spec — \
        pass a known name, a path:… spec, a git URL, or use --install-cmd + --binary",
        args.spec
    );
}

fn git_url_basename(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next()?;
    let stripped = last.strip_suffix(".git").unwrap_or(last);
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Validate `--subdir`: must be a relative path with no `..` components and
/// no absolute prefix. Returns the parsed `PathBuf` on success. Same shape
/// as `source::validate_asset_path` so the rules feel consistent.
fn validate_subdir(s: &str) -> Result<PathBuf> {
    if s.is_empty() {
        bail!("--subdir is empty");
    }
    let p = Path::new(s);
    if p.is_absolute() {
        bail!("--subdir `{s}` must be relative to the clone root");
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => bail!("--subdir `{s}` may not contain `..`"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("--subdir `{s}` may not be absolute")
            }
        }
    }
    Ok(p.to_path_buf())
}

fn subdir_basename(p: &Path) -> Option<String> {
    p.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[derive(Debug, Deserialize)]
struct CargoToml {
    package: CargoPackage,
    #[serde(default)]
    bin: Option<Vec<CargoBin>>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    #[serde(default = "default_version")]
    version: String,
}

#[derive(Debug, Deserialize)]
struct CargoBin {
    name: String,
}

fn default_version() -> String {
    "0.0.0".to_string()
}

fn read_cargo_toml(dir: &Path) -> Result<CargoToml> {
    let path = dir.join("Cargo.toml");
    let raw = fs::read_to_string(&path)?;
    let parsed: CargoToml = toml::from_str(&raw)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdir_accepts_relative_paths() {
        assert_eq!(
            validate_subdir("plugins/heyvm").unwrap(),
            PathBuf::from("plugins/heyvm")
        );
        assert_eq!(
            validate_subdir("./skills").unwrap(),
            PathBuf::from("./skills")
        );
    }

    #[test]
    fn subdir_rejects_traversal_and_absolute() {
        assert!(validate_subdir("").is_err());
        assert!(validate_subdir("/etc").is_err());
        assert!(validate_subdir("../escape").is_err());
        assert!(validate_subdir("plugins/../../etc").is_err());
    }

    #[test]
    fn subdir_basename_takes_last_component() {
        assert_eq!(
            subdir_basename(Path::new("plugins/heyvm")),
            Some("heyvm".to_string())
        );
        assert_eq!(
            subdir_basename(Path::new("heyvm")),
            Some("heyvm".to_string())
        );
        assert_eq!(subdir_basename(Path::new("")), None);
    }

    fn manifest_with(source: Source, name: &str, binary: &str) -> Manifest {
        Manifest {
            name: name.into(),
            version: "0.1.0".into(),
            binary: binary.into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            source,
            hooks: Vec::new(),
            driver: None,
            agents: Vec::new(),
        }
    }

    #[test]
    fn args_from_manifest_path_round_trips() {
        let m = manifest_with(
            Source::Path {
                path: "/abs/plugins/heyvm".into(),
            },
            "heyvm",
            "",
        );
        let args = args_from_manifest(&m).unwrap();
        assert_eq!(args.spec, "path:/abs/plugins/heyvm");
        assert_eq!(args.name.as_deref(), Some("heyvm"));
        assert!(args.force);
        assert!(args.install_cmd.is_none());
        assert!(args.subdir.is_none());
    }

    #[test]
    fn args_from_manifest_git_preserves_rev_and_subdir() {
        let m = manifest_with(
            Source::Git {
                url: "https://github.com/heyo-computer/printer".into(),
                rev: Some("abc1234".into()),
                subdir: Some("plugins/heyvm".into()),
            },
            "heyvm",
            "",
        );
        let args = args_from_manifest(&m).unwrap();
        assert_eq!(args.spec, "https://github.com/heyo-computer/printer");
        assert_eq!(args.rev.as_deref(), Some("abc1234"));
        assert_eq!(args.subdir.as_deref(), Some("plugins/heyvm"));
        assert_eq!(args.name.as_deref(), Some("heyvm"));
        assert!(args.force);
    }

    #[test]
    fn args_from_manifest_shell_carries_install_cmd_and_binary() {
        let m = manifest_with(
            Source::Shell {
                command: "curl … | sh".into(),
            },
            "vendor",
            "/home/u/.local/bin/vendor",
        );
        let args = args_from_manifest(&m).unwrap();
        assert_eq!(args.spec, "vendor");
        assert_eq!(args.install_cmd.as_deref(), Some("curl … | sh"));
        assert_eq!(args.binary.as_deref(), Some("/home/u/.local/bin/vendor"));
        assert!(args.force);
    }

    #[test]
    fn args_from_manifest_shell_without_binary_errors() {
        let m = manifest_with(
            Source::Shell {
                command: "x".into(),
            },
            "vendor",
            "",
        );
        let err = args_from_manifest(&m).unwrap_err();
        assert!(err.to_string().contains("no `binary` recorded"));
    }

    #[test]
    fn source_label_renders_each_variant() {
        assert!(source_label(&Source::Path { path: "/p".into() }).contains("path /p"));
        let g = source_label(&Source::Git {
            url: "https://x/y".into(),
            rev: Some("0123456789abcdef".into()),
            subdir: Some("plugins/y".into()),
        });
        assert!(g.contains("git https://x/y@01234567"));
        assert!(g.contains("subdir=plugins/y"));
        let s = source_label(&Source::Shell {
            command: "x".into(),
        });
        assert!(s.contains("shell `x`"));
    }

    #[test]
    fn git_basename_strips_dot_git_and_trailing_slash() {
        assert_eq!(
            git_url_basename("https://github.com/heyo-computer/printer.git"),
            Some("printer".to_string())
        );
        assert_eq!(
            git_url_basename("git@github.com:heyo-computer/printer/"),
            Some("printer".to_string())
        );
    }
}
