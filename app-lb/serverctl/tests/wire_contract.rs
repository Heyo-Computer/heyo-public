//! The wire contract between app-lb and this crate.
//!
//! `src/types.rs` promises these tests exist; until now they did not, and the
//! `extra` maps were carried without anything asserting on them. The fixtures in
//! `../testdata/wire/*.json` are written by app-lb's *own* response types
//! (`app-lb/src/wire_golden.rs`), so parsing one here and finding `extra` empty
//! is the only check that this crate still understands every field the server
//! sends.
//!
//! Leniency is what makes the check necessary rather than redundant: to a
//! defaulting deserializer an unknown field and an absent one are identical, so
//! a field that disappears from this crate's structs breaks nothing and blanks a
//! column. `extra` turns that into a failing test.
//!
//! Regenerate the fixtures with:
//!   `UPDATE_GOLDEN=1 cargo test -p app-lb wire_golden`

use serverctl::types::{WorkflowList, WorkflowView};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("serverctl lives inside the app-lb workspace")
        .join("testdata")
        .join("wire")
        .join(format!("{name}.json"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}\nrun `UPDATE_GOLDEN=1 cargo test -p app-lb wire_golden`",
            path.display()
        )
    })
}

/// The check that matters: every field app-lb sends has a name here.
#[test]
fn workflow_view_understands_every_field() {
    let w: WorkflowView = serde_json::from_str(&fixture("workflow")).expect("fixture parses");

    assert!(
        w.extra.is_empty(),
        "serverctl does not understand these fields app-lb sends: {:?}\n\
         Add them to WorkflowView in src/types.rs.",
        w.extra.keys().collect::<Vec<_>>()
    );

    // And the values actually landed, rather than defaulting past a rename.
    assert_eq!(w.id, "build");
    assert_eq!(w.git_ref, "main", "`ref` on the wire, `git_ref` in Rust");
    assert_eq!(w.path, ".ci/workflows/*.yml");
    assert_eq!(w.network, "prod-runners");
    assert!(w.auth.is_some(), "a SecretRef must survive as an opaque value");
    assert_eq!(w.secrets_prefix.as_deref(), Some("ci/app"));
    assert!(w.enabled);
}

/// The listing the `ci` orchestrator polls. Enveloped, so the envelope needs its
/// own check — a bare-array assumption here would fail at runtime, not compile
/// time.
#[test]
fn workflow_list_understands_every_field() {
    let list: WorkflowList = serde_json::from_str(&fixture("workflow-list")).expect("parses");
    assert!(
        list.extra.is_empty(),
        "unknown fields on the list envelope: {:?}",
        list.extra.keys().collect::<Vec<_>>()
    );
    assert_eq!(list.workflows.len(), 1);
    assert!(list.workflows[0].extra.is_empty());
    assert_eq!(list.workflows[0].id, "build");
}

/// A minimal object gets its defaults filled server-side, so the client must see
/// them rather than empty strings.
#[test]
fn a_minimal_workflow_arrives_with_its_defaults_filled() {
    let w: WorkflowView = serde_json::from_str(&fixture("workflow-minimal")).expect("parses");
    assert!(w.extra.is_empty(), "{:?}", w.extra.keys().collect::<Vec<_>>());
    assert_eq!(w.git_ref, "main");
    assert_eq!(w.path, ".ci/workflows/*.yml");
    assert!(w.enabled);
    assert!(w.auth.is_none());
}

/// Leniency itself: a *newer* server sending a field this build has no name for
/// must still parse, and the unknown field must be reachable rather than
/// discarded. That is the property the `extra` map buys.
#[test]
fn an_unknown_field_parses_and_is_reachable() {
    let mut value: serde_json::Value =
        serde_json::from_str(&fixture("workflow")).expect("fixture parses");
    value["concurrency_group"] = serde_json::json!("prod");

    let w: WorkflowView = serde_json::from_value(value).expect("a newer server still parses");
    assert_eq!(w.id, "build", "the known fields still land");
    assert_eq!(
        w.extra.get("concurrency_group").and_then(|v| v.as_str()),
        Some("prod"),
        "an unknown field must be reachable, not dropped"
    );
}
