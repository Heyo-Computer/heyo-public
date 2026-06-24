use crate::agent::AgentInvocation;
use crate::cli::SpecFromFollowupsArgs;
use crate::prompts::{SENTINEL_BLOCKED, SENTINEL_PLAN_READY, spec_from_followups_prompt};
use crate::session::Session;
use crate::specs_paths::next_numbered_spec_path;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub async fn spec_from_followups(args: SpecFromFollowupsArgs) -> Result<()> {
    let cwd: PathBuf = match args.cwd.as_deref() {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--cwd not found: {}", p.display()))?,
        None => std::env::current_dir()?,
    };

    let from = match args.from.as_deref() {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("--from not found: {}", p.display()))?,
        None => latest_followups(&cwd)?,
    };
    if !from.is_file() {
        bail!("--from must be a file: {}", from.display());
    }

    let dest = next_numbered_spec_path(&cwd, &args.name)?;
    if dest.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite",
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let body = std::fs::read_to_string(&from)
        .with_context(|| format!("reading follow-ups {}", from.display()))?;
    if body.trim().is_empty() {
        bail!("follow-ups file is empty: {}", from.display());
    }

    eprintln!(
        "[printer] generating spec from {} -> {}",
        from.display(),
        dest.display()
    );

    let acp =
        crate::agents::resolve_acp_launch(&args.agent, args.acp_bin.as_deref(), &args.acp_args)?;
    let agent = AgentInvocation {
        kind: args.agent.clone(),
        model: args.model.as_deref(),
        cwd: Some(&cwd),
        permission_mode: &args.permission_mode,
        command_wrapper: None,
        verbose: args.verbose,
        acp_bin: acp.bin.as_deref(),
        acp_args: acp.args.as_slice(),
        acp_env: &acp.env,
    };
    let mut session = Session::new(agent).with_verbose(args.verbose);

    let prompt = spec_from_followups_prompt(&dest.to_string_lossy(), &body);
    let outcome = session.turn(&prompt).await?;
    let text = outcome.result_text;
    if let Some(idx) = text.find(SENTINEL_BLOCKED) {
        let line: String = text[idx..].chars().take(120).collect();
        bail!("agent reported blocked: {line}");
    }
    if !text.contains(SENTINEL_PLAN_READY) {
        bail!("agent ended turn without {SENTINEL_PLAN_READY}; the spec may not have been written");
    }
    if !dest.is_file() {
        bail!(
            "agent emitted {SENTINEL_PLAN_READY} but {} was not created",
            dest.display()
        );
    }

    println!("wrote {}", dest.display());
    println!("next: printer plan {}", dest.display());
    eprintln!("[printer] token usage: {}", session.usage_total);
    Ok(())
}

fn latest_followups(cwd: &Path) -> Result<PathBuf> {
    let dir = cwd.join(".printer").join("followups");
    if !dir.is_dir() {
        bail!(
            "no follow-ups dir at {}; run `printer review` first or pass --from",
            dir.display()
        );
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let mtime = entry
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    match best {
        Some((_, p)) => Ok(p),
        None => bail!(
            "no *.md files in {}; run `printer review` first or pass --from",
            dir.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn latest_picks_newest_md() {
        let dir = tempdir().unwrap();
        let fdir = dir.path().join(".printer/followups");
        std::fs::create_dir_all(&fdir).unwrap();
        let a = fdir.join("a.md");
        let b = fdir.join("b.md");
        std::fs::write(&a, "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&b, "new").unwrap();
        let got = latest_followups(dir.path()).unwrap();
        assert_eq!(got, b);
    }

    #[test]
    fn latest_errors_when_dir_missing() {
        let dir = tempdir().unwrap();
        let err = latest_followups(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no follow-ups dir"));
    }

    #[test]
    fn latest_errors_when_dir_empty() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".printer/followups")).unwrap();
        let err = latest_followups(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no *.md"));
    }
}
