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

use heyctl::types::{
    DiskInventory, DiskState, MetricsResponse, UpstreamTrafficStatus, WorkflowList, WorkflowView,
};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("heyctl lives inside the app-lb workspace")
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
        "heyctl does not understand these fields app-lb sends: {:?}\n\
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

#[test]
fn upstream_traffic_status_understands_every_field() {
    let status: UpstreamTrafficStatus =
        serde_json::from_str(&fixture("upstream-traffic-status")).expect("fixture parses");
    assert!(
        status.extra.is_empty(),
        "unknown upstream traffic fields: {:?}",
        status.extra.keys().collect::<Vec<_>>()
    );
    assert_eq!(status.deployment_id, "stage");
    assert_eq!(status.upstream, "us1.example.com:443");
    assert_eq!(status.state, "draining");
    assert!(status.healthy);
    assert_eq!(status.in_flight, 3);
    assert_eq!(status.reason.as_deref(), Some("regional maintenance"));
    assert_eq!(status.started_at, Some(1_722_400_000));
}

/// The disk inventory `heyctl get disks` renders.
///
/// Checked field-by-field rather than only through `extra`, because this
/// response is the one an operator reads while deciding whether gigabytes are
/// safe to delete. A blanked column here is not a cosmetic regression: `held_by`
/// going missing turns "never reclaim this" into an empty cell, and the two size
/// fields disagreeing by design means silently rendering the wrong one
/// misreports how full the host actually is.
#[test]
fn disk_inventory_understands_every_field() {
    let inv: DiskInventory = serde_json::from_str(&fixture("disks")).expect("fixture parses");
    assert!(
        inv.extra.is_empty(),
        "unknown inventory fields: {:?}",
        inv.extra.keys().collect::<Vec<_>>()
    );
    assert!(inv.complete);
    assert_eq!(inv.data_dir, "/var/lib/heyo");
    assert_eq!(inv.ttl_secs, 604_800);
    assert_eq!(inv.totals.disks, 3);
    // The free-space block, which is what turns "here are the disks" into "and
    // that is why a create just failed". A fixture below the floor, so the
    // pressure path has a shape a client can render.
    assert_eq!(inv.free_bytes, Some(12_884_901_888));
    assert_eq!(inv.filesystem_bytes, Some(536_870_912_000));
    // The orphan clock is the policy that matters here, and it must survive the
    // wire: an orphan is the disk of a VM that never created, and retaining a
    // copy of that on the seven-day TTL is the leak this separates out.
    assert!(inv.orphan_ttl_secs > 0, "the orphan TTL must survive the wire");
    assert!(
        inv.orphan_ttl_secs < inv.ttl_secs,
        "an orphan must expire sooner than a recoverable disk, not later",
    );
    assert_eq!(inv.totals.reclaimable_bytes, 2_147_483_648);
    assert!(
        inv.totals.extra.is_empty(),
        "unknown totals fields: {:?}",
        inv.totals.extra.keys().collect::<Vec<_>>()
    );

    assert_eq!(inv.disks.len(), 3);
    for d in &inv.disks {
        assert!(
            d.extra.is_empty(),
            "unknown disk fields on {}: {:?}",
            d.sandbox_id,
            d.extra.keys().collect::<Vec<_>>()
        );
        for p in &d.parts {
            assert!(
                p.extra.is_empty(),
                "unknown disk-part fields: {:?}",
                p.extra.keys().collect::<Vec<_>>()
            );
        }
    }

    // The running disk: claimed, held, and sparse — the case where the two size
    // columns differ by 8x and reporting either alone would mislead.
    let running = &inv.disks[0];
    assert_eq!(running.sandbox_id, "sb-1a2b3c4d");
    assert_eq!(running.deployment.as_deref(), Some("web"));
    assert_eq!(running.state, DiskState::Running);
    assert!(running.claimed);
    assert_eq!(running.bytes, 1_073_741_824);
    assert_eq!(running.apparent_bytes, 8_589_934_592);
    assert_eq!(running.held_by.as_deref(), Some("in use by a running sandbox"));
    assert_eq!(running.parts.len(), 2);
    assert_eq!(running.roots, vec!["run/sb-1a2b3c4d".to_string()]);

    // The orphan: no daemon record, nothing holding it, and an expiry the sweep
    // will act on. `held_by: None` is what makes it reclaimable.
    let orphan = &inv.disks[1];
    assert_eq!(orphan.state, DiskState::Orphan);
    assert_eq!(orphan.deployment, None);
    assert_eq!(orphan.held_by, None);
    assert_eq!(orphan.expires_at, Some(1_759_604_800));

    // The pinned one: an operator said never, which outranks any age.
    let pinned = &inv.disks[2];
    assert_eq!(pinned.state, DiskState::Stopped);
    assert!(pinned.retain);
    assert_eq!(pinned.note.as_deref(), Some("keeping for the incident postmortem"));
    assert_eq!(pinned.expires_at, None);
}

/// A state this build has never heard of must degrade, not blank the listing.
#[test]
fn an_unknown_disk_state_degrades_to_unknown() {
    let mut value: serde_json::Value =
        serde_json::from_str(&fixture("disks")).expect("fixture parses");
    value["disks"][0]["state"] = serde_json::json!("quiescing");

    let inv: DiskInventory = serde_json::from_value(value).expect("a newer server still parses");
    assert_eq!(inv.disks[0].state, DiskState::Unknown);
    assert_eq!(inv.disks[1].state, DiskState::Orphan, "the other rows are unaffected");
}

/// `GET /metrics`, which had no contract test at all until now.
///
/// That gap was not theoretical. `AutoscaleCounts` was missing `boot_timeouts`
/// while app-lb had been sending it, so `heyctl describe` could not distinguish
/// the two ways a pool gets stuck at `ready: 0` — a guest that never becomes
/// healthy, and a VM that is never created. Both render as silence, and the
/// second one sends you to debug an image that never ran.
///
/// Asserting `extra` is empty at every level is what keeps that from recurring:
/// a lenient deserializer cannot tell an absent field from an unknown one, so
/// without this a field app-lb adds is simply never shown.
#[test]
fn metrics_response_understands_every_field() {
    let m: MetricsResponse =
        serde_json::from_str(&fixture("metrics-response")).expect("fixture parses");
    assert!(
        m.extra.is_empty(),
        "unknown metrics fields: {:?}",
        m.extra.keys().collect::<Vec<_>>()
    );
    assert!(
        m.global.extra.is_empty(),
        "unknown global-metrics fields: {:?}",
        m.global.extra.keys().collect::<Vec<_>>()
    );

    // The daemon block: the one field that says whether everything else here is
    // live or frozen. The fixture is the unreachable case on purpose.
    assert!(
        m.daemon.extra.is_empty(),
        "unknown daemon fields: {:?}",
        m.daemon.extra.keys().collect::<Vec<_>>()
    );
    assert!(!m.daemon.reachable, "the fixture records an unreachable daemon");
    assert!(
        m.daemon.last_error.as_deref().is_some_and(|e| e.contains("deployed-sandboxes")),
        "the reason must survive the wire, not just the boolean",
    );

    let a = &m.global.autoscale;
    assert!(
        a.extra.is_empty(),
        "unknown autoscale fields: {:?}",
        a.extra.keys().collect::<Vec<_>>()
    );
    assert_eq!(a.vms_created, 2);
    assert_eq!(a.boot_timeouts, 1, "the field that had gone missing");
    assert_eq!(a.create_failures, 1);
    assert_eq!(
        a.last_create_error.as_deref(),
        Some("api error (500): No space left on device"),
        "the reason a pool is stuck must survive the wire",
    );

    for d in &m.deployments {
        assert!(
            d.metrics.autoscale.extra.is_empty(),
            "unknown per-deployment autoscale fields: {:?}",
            d.metrics.autoscale.extra.keys().collect::<Vec<_>>()
        );
    }
}
