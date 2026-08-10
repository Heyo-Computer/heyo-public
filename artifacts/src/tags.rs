//! A tag file holds exactly one digest and is replaced only by rename.
//!
//! Tags are the one mutable object in the store, so unlike blobs they use the
//! temp-in-same-directory + `sync_all` + rename pattern from
//! `vault/src/store.rs:111-136`.
//!
//! They are **regular files, not symlinks**. A symlink target is a path you
//! must re-parse and re-validate, which would reintroduce the traversal
//! boundary [`crate::digest::Digest::parse`] just removed; the kernel does not
//! validate symlink targets, so a dangling tag would be indistinguishable from
//! a typo; symlinks do not affect the target's `st_nlink`, so they contribute
//! nothing to the refcount; and replacing one atomically still needs
//! symlink-to-temp + rename anyway.
//!
//! [`TagName`] uses the same charset as `vault/src/ids.rs` and is a
//! path-traversal boundary for exactly the same reason.

use crate::digest::Digest;
use serde::{Deserialize, Deserializer};
use std::fmt;

const MAX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    #[error("tag must not be empty")]
    Empty,
    #[error("tag must be at most {MAX_LEN} characters")]
    TooLong,
    #[error("tag may only contain letters, digits, '_', '-' and '.'")]
    BadChar,
    #[error("tag must not start with '-' or '.'")]
    BadStart,
    #[error("tag is a reserved name")]
    Reserved,
}

/// Case-insensitive reserved names (dot entries plus Windows devices, so a
/// store directory stays copyable to a non-Unix filesystem).
const RESERVED: &[&str] = &[
    ".", "..", "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "lpt1", "lpt2", "lpt3",
    "lpt4",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagName(String);

impl TagName {
    pub fn parse(s: &str) -> Result<TagName, TagError> {
        if s.is_empty() {
            return Err(TagError::Empty);
        }
        if s.len() > MAX_LEN {
            return Err(TagError::TooLong);
        }
        let first = s.as_bytes()[0];
        if first == b'-' || first == b'.' {
            return Err(TagError::BadStart);
        }
        // '.' is allowed after the first byte so image names like
        // "ubuntu-24.04" survive a round trip. It cannot form ".." because a
        // leading '.' is rejected and no separator is in the charset.
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            return Err(TagError::BadChar);
        }
        if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(s)) {
            return Err(TagError::Reserved);
        }
        Ok(TagName(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TagName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TagName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        TagName::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Something that resolves to a digest: either a tag or a literal digest.
///
/// Parsed digest-first, so a 64-hex tag name can never shadow a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Digest(Digest),
    Tag(TagName),
}

impl Ref {
    pub fn parse(s: &str) -> Result<Ref, TagError> {
        if let Ok(d) = Digest::parse(s) {
            return Ok(Ref::Digest(d));
        }
        TagName::parse(s).map(Ref::Tag)
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ref::Digest(d) => write!(f, "{d}"),
            Ref::Tag(t) => write!(f, "{t}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn accepts_reasonable_tags() {
        for good in [
            "debian",
            "debian-hermes",
            "ubuntu-24.04",
            "agents_v1",
            "x",
            "N0de",
        ] {
            assert!(TagName::parse(good).is_ok(), "should accept {good}");
        }
    }

    #[test]
    fn rejects_traversal_and_tricks() {
        let long = "x".repeat(65);
        let bad: [&str; 14] = [
            "",
            "..",
            ".",
            "../../etc/passwd",
            "/etc/passwd",
            "a/b",
            "a\\b",
            ".hidden",
            "-flag",
            "a b",
            "café",
            "nul",
            "CON",
            &long,
        ];
        for b in bad {
            assert!(TagName::parse(b).is_err(), "should reject {b:?}");
        }
    }

    #[test]
    fn rejects_embedded_nul() {
        assert!(TagName::parse("with\0null").is_err());
    }

    #[test]
    fn dotted_tag_cannot_form_parent_directory() {
        // ".." is Reserved and a leading '.' is BadStart, so no accepted tag
        // can traverse even though '.' is in the charset.
        assert!(TagName::parse("..").is_err());
        assert!(TagName::parse(".").is_err());
        assert!(TagName::parse("a..b").is_ok());
        assert!(!TagName::parse("a..b").unwrap().as_str().contains('/'));
    }

    #[test]
    fn ref_prefers_digest_over_tag() {
        // A 64-hex string is a digest, never a tag, so a tag can't shadow a blob.
        assert!(matches!(Ref::parse(OK_DIGEST), Ok(Ref::Digest(_))));
        assert!(matches!(Ref::parse("debian"), Ok(Ref::Tag(_))));
    }

    #[test]
    fn ref_rejects_what_neither_accepts() {
        assert!(Ref::parse("../etc").is_err());
    }
}
