//! Path globs, and the set of paths a submit changed.
//!
//! Two things live here because they are only ever used together: the matcher
//! that decides whether a path is covered by a pattern, and [`Changes`] — what a
//! submit actually touched, which is what the patterns are matched against.
//!
//! ## The glob dialect
//!
//! GitHub Actions' filter patterns, in the subset that is unambiguous:
//!
//! ```text
//! *      any run of characters within one segment; never crosses `/`
//! **     any number of whole segments, including none
//! ?      exactly one character within one segment
//! ```
//!
//! Everything else is a literal, and a pattern with no wildcard matches that one
//! path exactly. So `packages/api/**` covers everything under `packages/api`,
//! `**/*.rs` covers every Rust file at any depth, and `Cargo.lock` covers one
//! file.
//!
//! **A leading `!` is refused rather than honoured.** GitHub lets a `paths:`
//! list mix positive and negative patterns, where the last one to match wins —
//! order-dependent behaviour in a YAML list, which is a thing people get wrong
//! silently. `paths-ignore:` says the same thing in a way that cannot depend on
//! ordering, so that is the only spelling here and `!` is a parse error naming
//! it.
//!
//! ## Why an unknown change set matches everything
//!
//! [`Changes::Unknown`] is not an edge case to tidy away — it is most submits
//! that are not an ordinary push. A tree-only tarball has no history to diff, a
//! root commit has no parent, and `--dirty` packs a commit whose `after` is not
//! a resolvable object. In every one of those the honest answer is "no idea".
//!
//! The direction of the fallback is the whole safety property: an unknown set
//! **matches every pattern**, so a filtered workflow builds. The other direction
//! — unknown meaning "nothing changed" — is a CI system that quietly stops
//! building and reports success for a commit nothing ran on, which is the worst
//! failure a path filter can have.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The largest diff recorded as a real answer.
///
/// Past this the list stops being useful as a filter input and starts being a
/// row nothing wants to read: a tree-wide rename or a vendored-dependency drop
/// touches everything, so every filter would match anyway. Recording it as
/// [`Changes::Unknown`] reaches the same decision without the payload.
const MAX_CHANGED_PATHS: usize = 5_000;

/// What a submit changed, relative to the commit it came from.
///
/// Serialized onto `ci_run.changes` and read back by the scheduler, so the shape
/// is internally tagged rather than an untagged union — a row written by an
/// older build deserializes into [`Changes::Unknown`] through the `Default`
/// below rather than failing the whole run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Changes {
    /// The diff between the submitted commit and its stated parent, as
    /// `git diff --name-only` prints it: repository-relative, `/`-separated.
    Known { paths: Vec<String> },
    /// No usable diff, and why. Every pattern matches.
    Unknown { reason: String },
}

impl Default for Changes {
    fn default() -> Self {
        Self::unknown("this run predates change detection")
    }
}

impl Changes {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }

    /// A computed diff, degraded to [`Changes::Unknown`] when it is too large to
    /// be worth carrying.
    pub fn known(paths: Vec<String>) -> Self {
        if paths.len() > MAX_CHANGED_PATHS {
            return Self::unknown(format!(
                "the diff touches {} paths, over the {MAX_CHANGED_PATHS} this records; \
                 treating every path filter as matched",
                paths.len()
            ));
        }
        Self::Known { paths }
    }

    /// The paths, or an empty slice when there is no answer.
    ///
    /// Callers that make a decision must use [`Changes::matches_any`] instead:
    /// an empty slice here reads identically to "nothing changed", which is the
    /// one conclusion an unknown set must never produce.
    pub fn paths(&self) -> &[String] {
        match self {
            Self::Known { paths } => paths,
            Self::Unknown { .. } => &[],
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Known { .. } => None,
            Self::Unknown { reason } => Some(reason),
        }
    }

    /// Whether any changed path is covered by any of `patterns`.
    ///
    /// An unknown set is true, always — see the module doc. An empty pattern
    /// list is false, because "match nothing" is what an empty list says; every
    /// caller checks for that case before asking.
    pub fn matches_any(&self, patterns: &[String]) -> bool {
        let paths = match self {
            Self::Unknown { .. } => return true,
            Self::Known { paths } => paths,
        };
        patterns
            .iter()
            .any(|pat| paths.iter().any(|p| matches(pat, p)))
    }

    /// Whether *every* changed path is covered by `patterns`.
    ///
    /// What `paths-ignore:` needs: a commit is ignorable only when nothing in it
    /// is interesting. The two false cases are both deliberate and neither is
    /// what a bare `.iter().all()` would give — an unknown set is false because
    /// no answer must never skip a build, and an empty set is false because
    /// `all()` over nothing is vacuously true, which would ignore every submit
    /// the moment somebody wrote `paths-ignore:` with no diff to read.
    pub fn all_match(&self, patterns: &[String]) -> bool {
        let paths = match self {
            Self::Unknown { .. } => return false,
            Self::Known { paths } => paths,
        };
        if paths.is_empty() || patterns.is_empty() {
            return false;
        }
        paths
            .iter()
            .all(|p| patterns.iter().any(|pat| matches(pat, p)))
    }
}

impl fmt::Display for Changes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known { paths } => write!(f, "{} path(s) changed", paths.len()),
            Self::Unknown { reason } => write!(f, "changed paths unknown: {reason}"),
        }
    }
}

/// Reject a pattern this dialect cannot represent, naming the alternative.
///
/// Called at workflow parse time so a filter that would never behave as written
/// is a rejected submit rather than a workflow that quietly builds too much or
/// too little.
pub fn validate(pattern: &str) -> Result<(), String> {
    let p = pattern.trim();
    if p.is_empty() {
        return Err("a path pattern is empty".to_string());
    }
    if let Some(rest) = p.strip_prefix('!') {
        return Err(format!(
            "path pattern {p:?} starts with `!`. Negation is spelled with a \
             separate `paths-ignore:` list here, because a mixed list makes the \
             result depend on the order the patterns happen to be written in. \
             Write {rest:?} under `paths-ignore:`."
        ));
    }
    if p.starts_with('/') {
        return Err(format!(
            "path pattern {p:?} is absolute. Patterns are matched against \
             repository-relative paths, so write {:?}.",
            p.trim_start_matches('/')
        ));
    }
    Ok(())
}

/// Whether `path` is covered by `pattern`.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.trim().trim_start_matches("./").split('/').collect();
    let seg: Vec<&str> = path.trim_start_matches("./").split('/').collect();
    match_segments(&pat, &seg)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        // A pattern that ran out matches only a path that ran out too;
        // `src/lib.rs` must not be covered by `src`.
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // Trailing `**` takes whatever is left, including nothing.
            if rest.is_empty() {
                return true;
            }
            // Otherwise try every split point. The pattern segment count bounds
            // the recursion, and patterns are short.
            (0..=path.len()).any(|i| match_segments(rest, &path[i..]))
        }
        Some((first, rest)) => match path.split_first() {
            Some((head, tail)) if match_segment(first, head) => match_segments(rest, tail),
            _ => false,
        },
    }
}

/// `*` and `?` within one segment, with backtracking on `*`.
///
/// Iterative rather than recursive: a segment is attacker-adjacent input (it
/// comes from a workflow file in a submitted tree) and a pattern like `a*a*a*a*`
/// against a long name is the classic way to make a naive recursive matcher
/// blow up.
fn match_segment(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have taken too little.
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some('?') => {
                pi += 1;
                ti += 1;
            }
            Some(c) if *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, ti));
                }
                None => return false,
            },
        }
    }
    // Trailing `*`s may match nothing.
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_literal_pattern_matches_one_path() {
        assert!(matches("Cargo.lock", "Cargo.lock"));
        assert!(!matches("Cargo.lock", "sub/Cargo.lock"));
        assert!(!matches("Cargo.lock", "Cargo.locks"));
    }

    /// The monorepo case the whole feature exists for.
    #[test]
    fn a_double_star_covers_a_whole_subtree() {
        for path in [
            "packages/api/src/main.rs",
            "packages/api/Cargo.toml",
            "packages/api/a/b/c/d.txt",
        ] {
            assert!(matches("packages/api/**", path), "{path}");
        }
        assert!(!matches("packages/api/**", "packages/web/src/main.rs"));
        assert!(!matches("packages/api/**", "packages/apidocs/x"));
    }

    #[test]
    fn a_single_star_does_not_cross_a_separator() {
        assert!(matches("src/*.rs", "src/main.rs"));
        assert!(!matches("src/*.rs", "src/web/mod.rs"));
        assert!(matches("packages/*/Cargo.toml", "packages/api/Cargo.toml"));
        assert!(!matches("packages/*/Cargo.toml", "packages/a/b/Cargo.toml"));
    }

    #[test]
    fn a_double_star_in_the_middle_spans_any_depth() {
        assert!(matches("**/*.rs", "main.rs"));
        assert!(matches("**/*.rs", "src/web/mod.rs"));
        assert!(matches("packages/**/test/*.rs", "packages/a/b/test/x.rs"));
        assert!(!matches("packages/**/test/*.rs", "packages/a/b/test/x.py"));
        // `**` matching zero segments is what makes the middle form usable.
        assert!(matches("packages/**/Cargo.toml", "packages/Cargo.toml"));
    }

    #[test]
    fn a_question_mark_is_exactly_one_character() {
        assert!(matches("v?.txt", "v1.txt"));
        assert!(!matches("v?.txt", "v10.txt"));
        assert!(!matches("v?.txt", "v.txt"));
    }

    /// A pattern must not match a prefix of a path: a filter for the file
    /// `deploy` must not fire on everything under a `deploy/` directory.
    #[test]
    fn a_pattern_does_not_match_a_longer_path() {
        assert!(!matches("deploy", "deploy/image/Dockerfile"));
        assert!(matches("deploy/**", "deploy/image/Dockerfile"));
    }

    /// The backtracking case a naive left-to-right matcher gets wrong.
    #[test]
    fn stars_backtrack() {
        assert!(matches("*a*b*c", "xxaxxbxxc"));
        assert!(!matches("*a*b*c", "xxaxxbxxd"));
        assert!(matches("*.tar.gz", "release.tar.gz"));
        assert!(matches("**", "anything/at/all"));
    }

    /// The headline property: no answer must never read as "nothing changed".
    #[test]
    fn an_unknown_change_set_matches_every_pattern() {
        let c = Changes::unknown("a tarball carries no history");
        assert!(c.matches_any(&v(&["packages/api/**"])));
        assert!(c.matches_any(&v(&["nothing/like/this"])));
        assert!(!c.is_known());
        assert_eq!(c.paths(), &[] as &[String]);
        assert!(c.reason().unwrap().contains("no history"));
    }

    #[test]
    fn a_known_set_matches_only_what_it_contains() {
        let c = Changes::known(v(&["packages/api/src/main.rs", "README.md"]));
        assert!(c.matches_any(&v(&["packages/api/**"])));
        assert!(!c.matches_any(&v(&["packages/web/**"])));
        // Any pattern matching is enough.
        assert!(c.matches_any(&v(&["packages/web/**", "*.md"])));
        // An empty list says "match nothing", and callers check for it first.
        assert!(!c.matches_any(&[]));
    }

    /// A tree-wide diff is recorded as no answer rather than as a huge row —
    /// and the fallback direction still builds.
    #[test]
    fn an_enormous_diff_degrades_to_unknown() {
        let huge: Vec<String> = (0..MAX_CHANGED_PATHS + 1)
            .map(|i| format!("f{i}"))
            .collect();
        let c = Changes::known(huge);
        assert!(!c.is_known());
        assert!(c.reason().unwrap().contains("over the"));
        assert!(c.matches_any(&v(&["packages/api/**"])));

        // One under the cap is still a real answer.
        let ok: Vec<String> = (0..MAX_CHANGED_PATHS).map(|i| format!("f{i}")).collect();
        assert!(Changes::known(ok).is_known());
    }

    /// Written to `ci_run.changes` and read back by the scheduler in another
    /// process, so the round trip is the contract.
    #[test]
    fn changes_round_trip_through_json() {
        for c in [
            Changes::known(v(&["a/b.rs"])),
            Changes::known(vec![]),
            Changes::unknown("no parent"),
        ] {
            let json = serde_json::to_value(&c).unwrap();
            let back: Changes = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(c, back, "{json}");
        }
        // An empty *known* set is a real answer — a commit that changed nothing
        // — and must not deserialize as unknown.
        let empty = Changes::known(vec![]);
        assert!(empty.is_known());
        assert!(!empty.matches_any(&v(&["**"])));
    }

    /// A row written before this column existed, and anything unparseable,
    /// falls back to "no answer" rather than to "nothing changed".
    #[test]
    fn the_default_is_unknown() {
        assert!(!Changes::default().is_known());
        assert!(Changes::default().matches_any(&v(&["anything"])));
    }

    #[test]
    fn negation_and_absolute_patterns_are_refused_by_name() {
        let e = validate("!packages/docs/**").unwrap_err();
        assert!(e.contains("paths-ignore"), "{e}");
        assert!(e.contains("packages/docs/**"), "{e}");

        let e = validate("/etc/passwd").unwrap_err();
        assert!(e.contains("repository-relative"), "{e}");

        assert!(validate("  ").is_err());
        assert!(validate("packages/api/**").is_ok());
    }
}
