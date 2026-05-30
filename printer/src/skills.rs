use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub skill_file: PathBuf,
}

/// Resolve `--skill` arguments and an optional default skills root (e.g.
/// `.claude/skills/`) into a flat list of skills. Each input may be a
/// `SKILL.md` file directly, a directory containing `SKILL.md`, or a
/// directory of skill directories. Duplicate skill files are de-duplicated
/// by canonical path.
pub fn resolve(explicit: &[PathBuf], default_root: Option<&Path>) -> Result<Vec<Skill>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for p in explicit {
        collect_skill_files(p, &mut paths)
            .with_context(|| format!("resolving --skill {}", p.display()))?;
    }
    if explicit.is_empty()
        && let Some(root) = default_root
            && root.is_dir() {
                collect_skill_files(root, &mut paths)
                    .with_context(|| format!("scanning default skills root {}", root.display()))?;
            }

    paths.sort();
    paths.dedup();

    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading skill file {}", path.display()))?;
        let (name, description) = parse_frontmatter(&raw)
            .with_context(|| format!("parsing frontmatter in {}", path.display()))?;
        out.push(Skill {
            name,
            description,
            skill_file: path,
        });
    }
    Ok(out)
}

fn collect_skill_files(p: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let canon = p
        .canonicalize()
        .with_context(|| format!("path not found: {}", p.display()))?;

    if canon.is_file() {
        out.push(canon);
        return Ok(());
    }
    if !canon.is_dir() {
        return Err(anyhow!(
            "{} is neither a file nor a directory",
            canon.display()
        ));
    }

    let direct = canon.join("SKILL.md");
    if direct.is_file() {
        out.push(direct);
        return Ok(());
    }

    let mut found_any = false;
    for entry in fs::read_dir(&canon)
        .with_context(|| format!("reading directory {}", canon.display()))?
    {
        let entry = entry?;
        let child = entry.path();
        if !child.is_dir() {
            continue;
        }
        let skill_md = child.join("SKILL.md");
        if skill_md.is_file() {
            out.push(skill_md);
            found_any = true;
        }
    }
    if !found_any && !canon.join("SKILL.md").is_file() {
        // Empty or unrelated directory — not an error; the caller may have
        // pointed at a default root that simply has no skills yet.
    }
    Ok(())
}

/// Extract `name` and `description` from a YAML-ish frontmatter block fenced
/// by `---` lines at the top of the file. We keep this tiny on purpose:
/// only top-level scalar values, no nesting, no multi-line folded scalars.
fn parse_frontmatter(raw: &str) -> Result<(String, String)> {
    let body = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("missing opening `---` frontmatter fence"))?;
    let close = body
        .find("\n---")
        .ok_or_else(|| anyhow!("missing closing `---` frontmatter fence"))?;
    let fm = &body[..close];

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    for line in fm.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = strip_yaml_quotes(v.trim());
        match key {
            "name" => name = Some(val.to_string()),
            "description" => description = Some(val.to_string()),
            _ => {}
        }
    }
    let name = name.ok_or_else(|| anyhow!("frontmatter missing `name`"))?;
    let description = description.ok_or_else(|| anyhow!("frontmatter missing `description`"))?;
    Ok((name, description))
}

fn strip_yaml_quotes(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_frontmatter() {
        let raw = "---\nname: foo\ndescription: bar baz\nversion: 0.1.0\n---\n\n# foo\n";
        let (n, d) = parse_frontmatter(raw).unwrap();
        assert_eq!(n, "foo");
        assert_eq!(d, "bar baz");
    }

    #[test]
    fn strips_quotes() {
        let raw = "---\nname: \"foo\"\ndescription: 'bar'\n---\n";
        let (n, d) = parse_frontmatter(raw).unwrap();
        assert_eq!(n, "foo");
        assert_eq!(d, "bar");
    }

    #[test]
    fn missing_fences_are_errors() {
        assert!(parse_frontmatter("name: foo\n").is_err());
    }

    #[test]
    fn discovers_skills_in_parent_dir() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .parent()
            .expect("repo root")
            .join(".claude")
            .join("skills");
        if !root.is_dir() {
            return;
        }
        let skills = resolve(&[], Some(&root)).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"computer"), "expected computer skill, got {names:?}");
    }
}
