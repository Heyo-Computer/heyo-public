//! Compile `migrations/*.sql` into the binary.
//!
//! The schema used to be a runtime dependency: `ci` read `CI_MIGRATIONS_DIR`
//! at startup and re-executed whatever `.sql` files it found there. That put
//! the binary and its schema in two places that had to be deployed together,
//! and the failure when they were not was the worst kind — a binary querying a
//! column its own migration would have added, refused by Postgres on the first
//! request rather than at startup (`column p.size_class does not exist`, on
//! 2026-08-25, from a build installed without its `010_*.sql`). A binary that
//! carries its migrations cannot be separated from them.
//!
//! The list is generated here rather than written by hand as a row of
//! `include_str!`s, because a hand-written list is the same drift one file
//! later: a migration added to the directory and forgotten from the list is
//! silently never applied. Every `.sql` in the directory, in filename order,
//! or the build fails.

use std::fs;
use std::path::Path;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    // The directory for additions and removals, and each file for its content:
    // cargo's directory watch does not see edits inside existing files.
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no .sql files in {} — the binary would start with no schema",
        dir.display()
    );

    let mut out = String::from(
        "/// Every `migrations/*.sql`, in filename order: `(file name, SQL)`.\n\
         pub const EMBEDDED_MIGRATIONS: &[(&str, &str)] = &[\n",
    );
    for path in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path.file_name().unwrap().to_str().expect("utf-8 file name");
        out.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("migrations.rs");
    fs::write(&dest, out).unwrap_or_else(|e| panic!("writing {}: {e}", dest.display()));
}
