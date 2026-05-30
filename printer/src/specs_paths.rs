//! Helpers for the numbered `specs/NNN-<slug>.md` layout used by `printer init`
//! (when `.printer/` already exists) and `printer spec-from-followups`.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Compute the next `specs/NNN-<slug>.md` path under `root`, where `NNN` is one
/// greater than the highest existing 3-digit prefix in `root/specs/`. Starts at
/// `001` if the directory is missing or empty. Does not create the directory.
pub fn next_numbered_spec_path(root: &Path, slug: &str) -> Result<PathBuf> {
    validate_slug(slug)?;
    let dir = root.join("specs");
    let mut max_n: u32 = 0;
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = parse_leading_number(&name)
                && n > max_n {
                    max_n = n;
                }
        }
    }
    let next = max_n + 1;
    Ok(dir.join(format!("{next:03}-{slug}.md")))
}

fn parse_leading_number(filename: &str) -> Option<u32> {
    let (prefix, rest) = filename.split_once('-')?;
    if prefix.len() != 3 {
        return None;
    }
    if !rest.ends_with(".md") {
        return None;
    }
    prefix.parse::<u32>().ok()
}

/// Reject slugs that would escape `specs/` or contain whitespace. Allows
/// alphanumeric plus `-`, `_`, `.`.
pub fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() {
        bail!("slug must not be empty");
    }
    if slug.starts_with('.') || slug.starts_with('-') {
        bail!("slug must not start with '.' or '-': {slug}");
    }
    for ch in slug.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.';
        if !ok {
            bail!("slug contains invalid character {ch:?}: {slug}");
        }
    }
    if slug.contains("..") {
        bail!("slug must not contain '..': {slug}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn empty_specs_dir_returns_001() {
        let dir = tempdir().unwrap();
        let p = next_numbered_spec_path(dir.path(), "foo").unwrap();
        assert_eq!(p, dir.path().join("specs/001-foo.md"));
    }

    #[test]
    fn missing_specs_dir_returns_001() {
        let dir = tempdir().unwrap();
        // do not create specs/
        let p = next_numbered_spec_path(dir.path(), "bar").unwrap();
        assert_eq!(p, dir.path().join("specs/001-bar.md"));
    }

    #[test]
    fn picks_max_plus_one() {
        let dir = tempdir().unwrap();
        let specs = dir.path().join("specs");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::write(specs.join("001-a.md"), "").unwrap();
        std::fs::write(specs.join("002-b.md"), "").unwrap();
        std::fs::write(specs.join("003-c.md"), "").unwrap();
        let p = next_numbered_spec_path(dir.path(), "next").unwrap();
        assert_eq!(p, specs.join("004-next.md"));
    }

    #[test]
    fn ignores_non_numbered_files() {
        let dir = tempdir().unwrap();
        let specs = dir.path().join("specs");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::write(specs.join("README.md"), "").unwrap();
        std::fs::write(specs.join("notes.md"), "").unwrap();
        std::fs::write(specs.join("01-short.md"), "").unwrap(); // wrong digit count
        let p = next_numbered_spec_path(dir.path(), "x").unwrap();
        assert_eq!(p, specs.join("001-x.md"));
    }

    #[test]
    fn validates_slug() {
        assert!(validate_slug("feat-deploy_assets.v2").is_ok());
        assert!(validate_slug("").is_err());
        assert!(validate_slug("..").is_err());
        assert!(validate_slug("foo/bar").is_err());
        assert!(validate_slug("foo bar").is_err());
        assert!(validate_slug(".hidden").is_err());
        assert!(validate_slug("-leading").is_err());
    }

    #[test]
    fn rejects_invalid_slug_in_next_path() {
        let dir = tempdir().unwrap();
        assert!(next_numbered_spec_path(dir.path(), "../escape").is_err());
    }
}
