//! Optional `printer-plugin.toml` shipped at a plugin's source root, declaring
//! hooks and asset files to merge into the installed manifest. See
//! `printer/HOOKS.md` ("Authoring a plugin").

use crate::{
    agents::{AgentSpec, validate_agent},
    drivers::{DriverSpec, validate_driver},
    hooks::{HookSpec, resolve_hook},
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::path::{Component, Path};

const FILE_NAME: &str = "printer-plugin.toml";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceManifest {
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub driver: Option<DriverSpec>,
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentSpec>,
}

impl SourceManifest {
    /// Load `<source_dir>/printer-plugin.toml` if it exists. Missing file →
    /// empty manifest (current behaviour). Malformed file → hard error.
    pub fn load(source_dir: &Path) -> Result<Self> {
        let path = source_dir.join(FILE_NAME);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let parsed: SourceManifest =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(parsed)
    }

    /// Validate every declared hook by round-tripping through `resolve_hook`,
    /// using the eventual install dir for skill-path display only. Returns
    /// the validated specs ready to write into `Manifest::hooks`.
    pub fn validate_hooks(&self, plugin_name: &str, plugin_dir: &Path) -> Result<Vec<HookSpec>> {
        let mut out = Vec::with_capacity(self.hooks.len());
        for (i, spec) in self.hooks.iter().enumerate() {
            // resolve_hook consumes; we validate a clone and keep the original.
            resolve_hook(plugin_name, plugin_dir, spec.clone())
                .with_context(|| format!("{FILE_NAME} hook #{} ({})", i + 1, spec.event))?;
            out.push(spec.clone());
        }
        Ok(out)
    }

    /// Validate the optional `[driver]` block. Returns the spec ready to
    /// write into `Manifest::driver`. `None` if the source declares no driver.
    pub fn validate_driver(&self) -> Result<Option<DriverSpec>> {
        let Some(spec) = &self.driver else {
            return Ok(None);
        };
        validate_driver(spec).with_context(|| format!("{FILE_NAME} [driver] block"))?;
        Ok(Some(spec.clone()))
    }

    /// Validate every declared `[[agent]]` block and check that names are
    /// unique within this manifest. Cross-plugin uniqueness is enforced at
    /// load time by `AgentSet::load_installed`.
    pub fn validate_agents(&self) -> Result<Vec<AgentSpec>> {
        let mut out = Vec::with_capacity(self.agents.len());
        for (i, spec) in self.agents.iter().enumerate() {
            validate_agent(spec).with_context(|| format!("{FILE_NAME} [[agent]] #{}", i + 1))?;
            if out.iter().any(|s: &AgentSpec| s.name == spec.name) {
                bail!(
                    "{FILE_NAME} declares two [[agent]] blocks with name `{}`",
                    spec.name
                );
            }
            out.push(spec.clone());
        }
        Ok(out)
    }
}

/// Copy each declared asset from `source_dir/<asset>` into
/// `plugin_dir/<asset>`. Files are copied directly; directories are copied
/// recursively. Asset paths are validated (relative, no `..`, no symlink
/// shenanigans). Refuses to clobber pre-existing files in the install dir.
pub fn copy_assets(source_dir: &Path, plugin_dir: &Path, assets: &[String]) -> Result<()> {
    for asset in assets {
        validate_asset_path(asset)?;
        let src = source_dir.join(asset);
        let dst = plugin_dir.join(asset);
        if !src.exists() {
            bail!(
                "{FILE_NAME} declares asset `{asset}` but {} does not exist",
                src.display()
            );
        }
        if dst.exists() {
            bail!(
                "asset `{asset}` collides with existing path {}",
                dst.display()
            );
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if src.is_dir() {
            copy_dir(&src, &dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
        } else {
            fs::copy(&src, &dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
        }
        eprintln!(
            "[printer] installed asset {} ({})",
            asset,
            describe_kind(&dst)
        );
    }
    Ok(())
}

fn describe_kind(p: &Path) -> &'static str {
    if p.is_dir() { "dir" } else { "file" }
}

fn validate_asset_path(asset: &str) -> Result<()> {
    if asset.is_empty() {
        bail!("empty asset path");
    }
    let p = Path::new(asset);
    if p.is_absolute() {
        bail!("asset path `{asset}` must be relative");
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => bail!("asset path `{asset}` may not contain `..`"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("asset path `{asset}` may not be absolute")
            }
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to)?;
        } else if ft.is_symlink() {
            // Refuse to follow symlinks inside an asset tree. Avoids
            // exfiltrating files outside the source dir if a malicious
            // plugin author symlinks `/etc/passwd` into `skills/`.
            bail!("symlink not allowed in asset: {}", from.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let m = SourceManifest::load(dir.path()).unwrap();
        assert!(m.hooks.is_empty());
        assert!(m.assets.is_empty());
    }

    #[test]
    fn loads_hooks_and_assets() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE_NAME),
            r#"
assets = ["skills"]

[[hooks]]
type = "agent"
event = "before_run"
skill = "skills/codegraph-search/SKILL.md"

[[hooks]]
type = "cli"
event = "before_run"
command = "codegraph index"
on_failure = "warn"
"#,
        )
        .unwrap();
        let m = SourceManifest::load(dir.path()).unwrap();
        assert_eq!(m.hooks.len(), 2);
        assert_eq!(m.assets, vec!["skills".to_string()]);
    }

    #[test]
    fn malformed_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), "not = [valid").unwrap();
        assert!(SourceManifest::load(dir.path()).is_err());
    }

    #[test]
    fn rejects_absolute_asset() {
        assert!(validate_asset_path("/etc/passwd").is_err());
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        assert!(validate_asset_path("../etc/passwd").is_err());
        assert!(validate_asset_path("skills/../../etc").is_err());
    }

    #[test]
    fn accepts_normal_relative_paths() {
        validate_asset_path("skills").unwrap();
        validate_asset_path("skills/codegraph-search/SKILL.md").unwrap();
        validate_asset_path("./skills").unwrap();
    }

    #[test]
    fn copy_assets_copies_files_and_dirs() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // file asset
        fs::write(src.path().join("README.md"), b"hi").unwrap();
        // nested dir asset
        fs::create_dir_all(src.path().join("skills/inner")).unwrap();
        fs::write(src.path().join("skills/inner/SKILL.md"), b"x").unwrap();

        copy_assets(
            src.path(),
            dst.path(),
            &["README.md".to_string(), "skills".to_string()],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(dst.path().join("README.md")).unwrap(),
            "hi"
        );
        assert_eq!(
            fs::read_to_string(dst.path().join("skills/inner/SKILL.md")).unwrap(),
            "x"
        );
    }

    #[test]
    fn copy_assets_rejects_collision() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.md"), b"x").unwrap();
        fs::write(dst.path().join("a.md"), b"already").unwrap();
        let err = copy_assets(src.path(), dst.path(), &["a.md".to_string()]).unwrap_err();
        assert!(err.to_string().contains("collides"));
    }

    #[test]
    fn loads_and_validates_agents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE_NAME),
            r#"
[[agent]]
kind = "acp"
name = "poolside"
command = "pool"
args = ["acp"]

[[agent]]
kind = "acp"
name = "claude-code"
command = "claude-code-acp"
"#,
        )
        .unwrap();
        let m = SourceManifest::load(dir.path()).unwrap();
        let validated = m.validate_agents().unwrap();
        assert_eq!(validated.len(), 2);
        assert_eq!(validated[0].name, "poolside");
        assert_eq!(validated[1].command, "claude-code-acp");
    }

    #[test]
    fn validate_agents_rejects_dupes_in_same_manifest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE_NAME),
            r#"
[[agent]]
kind = "acp"
name = "dup"
command = "x"

[[agent]]
kind = "acp"
name = "dup"
command = "y"
"#,
        )
        .unwrap();
        let m = SourceManifest::load(dir.path()).unwrap();
        let err = m.validate_agents().unwrap_err();
        assert!(err.to_string().contains("dup"));
    }

    #[test]
    fn validate_agents_rejects_reserved_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(FILE_NAME),
            r#"
[[agent]]
kind = "acp"
name = "claude"
command = "x"
"#,
        )
        .unwrap();
        let m = SourceManifest::load(dir.path()).unwrap();
        let err = m.validate_agents().unwrap_err();
        assert!(format!("{err:#}").contains("reserved"));
    }

    #[test]
    fn copy_assets_rejects_missing_source() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let err = copy_assets(src.path(), dst.path(), &["does-not-exist".to_string()]).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }
}
