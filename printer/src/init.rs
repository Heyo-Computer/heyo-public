use crate::cli::InitArgs;
use crate::hooks::{Event, HookContext, HookSet};
use crate::specs_paths::{next_numbered_spec_path, validate_slug};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn init(args: InitArgs) -> Result<()> {
    let project_root =
        std::env::current_dir().context("resolving current directory for project root")?;
    let printer_dir_exists = project_root.join(".printer").is_dir();

    let path = resolve_target(&project_root, args.path.as_deref(), printer_dir_exists)?;
    if path.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }

    let hooks = HookSet::load_installed().unwrap_or_default();
    hooks.run_cli(
        Event::BeforeInit,
        &HookContext::new(Event::BeforeInit, project_root.clone()),
    )?;

    let result: Result<()> = (|| {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir {}", parent.display()))?;
        }
        let body = template(&args.title);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("[printer] wrote {}", path.display());
        bootstrap_printer_dir(&project_root)?;
        bootstrap_codegraph_index(&project_root);
        Ok(())
    })();

    let after_ctx =
        HookContext::new(Event::AfterInit, project_root.clone()).with_exit_status(result.is_ok());
    let _ = hooks.run_cli(Event::AfterInit, &after_ctx);

    result?;
    eprintln!(
        "Edit the checklist, then run `printer plan {}` to draft the plan.",
        path.display()
    );
    Ok(())
}

/// Decide where the spec file should land based on whether the project has
/// already been initialized with printer (`.printer/` present). In a fresh
/// repo the arg is a path (default `spec.md`); in an initialized repo the arg
/// is required and treated as a slug producing `specs/NNN-<slug>.md`.
fn resolve_target(
    project_root: &Path,
    arg: Option<&Path>,
    printer_dir_exists: bool,
) -> Result<PathBuf> {
    if !printer_dir_exists {
        return Ok(arg
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("spec.md")));
    }
    // Initialized repo: arg must be a slug.
    let slug = arg.and_then(|p| p.to_str()).ok_or_else(|| {
        anyhow::anyhow!(
            "this repo already has a .printer/ directory; pass a slug like \
                 `printer init feat-deploy-assets` to write specs/NNN-<slug>.md"
        )
    })?;
    if slug.contains(std::path::MAIN_SEPARATOR) {
        bail!(
            "expected a slug (e.g. `feat-deploy-assets`) but got a path: {}",
            slug
        );
    }
    validate_slug(slug)?;
    let abs = next_numbered_spec_path(project_root, slug)?;
    // Return relative-to-cwd if possible so eprintln!s stay tidy.
    Ok(abs
        .strip_prefix(project_root)
        .map(|p| p.to_path_buf())
        .unwrap_or(abs))
}

/// Create the `.printer/` skeleton (`.printer/tasks/`) so `printer run` and
/// `printer exec` find a writable store on first invocation.
fn bootstrap_printer_dir(root: &Path) -> Result<()> {
    let tasks_dir = root.join(".printer").join("tasks");
    std::fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("creating {}", tasks_dir.display()))?;
    eprintln!("[printer] prepared {}", tasks_dir.display());
    Ok(())
}

/// Best-effort: shell out to `codegraph index` so search/snippet/outline are
/// usable from the agent's first turn. If `codegraph` is not on PATH or the
/// index fails, warn and continue — the spec is still usable without it.
fn bootstrap_codegraph_index(root: &Path) {
    let out = Command::new("codegraph")
        .args(["--text", "index"])
        .current_dir(root)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let summary = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if summary.is_empty() {
                eprintln!("[printer] codegraph index built");
            } else {
                eprintln!("[printer] {summary}");
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!(
                "[printer] codegraph index failed (exit {}): {}",
                o.status.code().unwrap_or(-1),
                stderr.trim()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "[printer] codegraph not on PATH; skipping initial index. \
                 Install with `make install-codegraph` to enable code-graph search."
            );
        }
        Err(e) => {
            eprintln!("[printer] could not invoke codegraph: {e}");
        }
    }
}

fn template(title: &str) -> String {
    format!(
        "# {title}

A short description of what this project is and why we're doing it.

## Tasks

- [ ] First task — short imperative title for one unit of work
  Optional indented description (2-space indent). Multi-line is fine.
  Blank lines inside the description are preserved.

- [ ] Second task — replace this with a real task or delete it

- [x] Use `[x]` for items that were done before this spec existed; the
  driver will create them already in the `done` state.

<!--
Spec format reference (full docs in the printer README):
  * Lines starting with `- [ ]`, `- [x]`, `* [ ]`, `+ [ ]` (etc.) at
    column 0 are tasks. The text after the checkbox is the title.
  * Lines indented by 2 spaces or one tab below a task become its
    description body.
  * Any unindented non-task line ends the current task's description.
  * Re-runs of `printer run {title_path}` are idempotent — items are
    matched to existing tasks by a stable anchor derived from this
    file's path + the task title.
-->
",
        title = title,
        // %-encode-style hint for users who renamed the file later
        title_path = "<this-file>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn legacy_mode_defaults_to_spec_md() {
        let dir = tempdir().unwrap();
        let p = resolve_target(dir.path(), None, false).unwrap();
        assert_eq!(p, PathBuf::from("spec.md"));
    }

    #[test]
    fn legacy_mode_accepts_explicit_path() {
        let dir = tempdir().unwrap();
        let arg = PathBuf::from("foo/bar.md");
        let p = resolve_target(dir.path(), Some(&arg), false).unwrap();
        assert_eq!(p, PathBuf::from("foo/bar.md"));
    }

    #[test]
    fn slug_mode_routes_to_numbered_specs() {
        let dir = tempdir().unwrap();
        let specs = dir.path().join("specs");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::write(specs.join("001-old.md"), "").unwrap();
        let arg = PathBuf::from("feat-x");
        let p = resolve_target(dir.path(), Some(&arg), true).unwrap();
        assert_eq!(p, PathBuf::from("specs/002-feat-x.md"));
    }

    #[test]
    fn slug_mode_requires_arg() {
        let dir = tempdir().unwrap();
        let err = resolve_target(dir.path(), None, true).unwrap_err();
        assert!(err.to_string().contains("slug"));
    }

    #[test]
    fn slug_mode_rejects_path_argument() {
        let dir = tempdir().unwrap();
        let arg = PathBuf::from("a/b");
        let err = resolve_target(dir.path(), Some(&arg), true).unwrap_err();
        assert!(err.to_string().contains("slug"));
    }
}
