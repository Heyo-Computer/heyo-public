//! Archive reconciliation: schemas whose data is in S3 but which the pooler
//! will not restore.
//!
//! THE FAILURE THIS FIXES. `checkout` decides how to serve a schema purely
//! from its registry tier (see the `match record.tier` in
//! [`crate::registry::SchemaRegistry`]): only `Archived` reaches the S3
//! restore path. A schema whose row says `Live` is reattached — or, when the
//! VM behind that row is gone, brought up on a **brand new empty VM**. The
//! client connects successfully and finds an empty database, while a perfectly
//! good archive sits in S3 that nothing will ever look at. Nothing logs an
//! error, because from the pooler's point of view a live schema was served.
//!
//! WHY THIS CANNOT BE AUTOMATIC. The obvious rule — "an S3 object exists but
//! the tier isn't Archived, so fix the tier" — is wrong, and destructively so.
//! A restore does not delete the archive it restored from (`delete_object` is
//! only ever called to clear a *stale dump* that an image upload supersedes),
//! and a successful bring-up writes `Tier::Live` through `store.put`. So
//! **every schema that has ever been archived and thawed** sits at `Live` with
//! its old archive still in the bucket. That is the healthy steady state, and
//! flipping those rows would abandon current data in favour of a stale copy.
//!
//! So this page does not decide. It gathers the evidence that actually
//! separates the two cases and puts it side by side for a human:
//!
//!   * the archive — which key a restore would choose, how big, how old;
//!   * the VM the registry currently binds — whether its `data.ext4` exists at
//!     all, and how many bytes it has actually allocated.
//!
//! The discriminator that carries the most weight is the cheapest one: a live
//! row whose disk directory is *missing* binds nothing, so there is no data to
//! lose and the archive is strictly better than what is there. Next is size —
//! a freshly-initdb'd cluster allocates a very tight ~109-118 MiB (measured
//! across a whole pooler host), so a live row backed by a disk in that band,
//! against a multi-gigabyte archive, is the "connected and got an empty
//! database" case caught in the act. Neither is proof on its own, which is why
//! the action is per-schema and the numbers are on screen next to it.

use std::time::Duration;

use axum::extract::{Form, Query, State};
use axum::response::Redirect;
use futures::stream::{self, StreamExt};
use maud::Markup;
use serde::Deserialize;

use crate::store::Tier;

use super::handlers::{qenc, Banner};
use super::state::DashState;
use super::views;

/// Allocated size at or below which a data disk is treated as "looks like a
/// fresh cluster". A blank 2GB disk that has been mkfs'd and initdb'd and
/// nothing else lands at 109-118 MiB; the headroom above that covers initdb
/// variance and a little WAL without reaching into real-workbook territory.
/// Only ever used to *sort and flag* — never to act.
const FRESH_DISK_MAX: u64 = 160 * 1024 * 1024;

/// Bound on concurrent S3 HEADs during a scan. The candidate set is the
/// non-archived rows (tens to low hundreds), so this keeps a scan to a couple
/// of seconds without opening a connection per schema.
const PROBE_CONCURRENCY: usize = 8;

/// How the evidence reads for one schema. Ordering is the display order:
/// the rows an operator must look at come first.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum Verdict {
    /// The registry binds a VM whose data disk does not exist. Nothing is
    /// being served from it; the archive is unambiguously better.
    NoDisk,
    /// The bound disk exists but has allocated no more than a fresh cluster
    /// would, while an archive holds more. The shape of "a client connected
    /// and got an empty database".
    LooksEmpty,
    /// The bound disk holds substantially more than a fresh cluster. Almost
    /// certainly real, current data — the archive is just the old copy that a
    /// thaw left behind. Shown for completeness, never flagged.
    Serving,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::NoDisk => "no disk",
            Verdict::LooksEmpty => "looks empty",
            Verdict::Serving => "serving",
        }
    }

    /// Does this row want an operator's attention?
    pub fn suspect(self) -> bool {
        matches!(self, Verdict::NoDisk | Verdict::LooksEmpty)
    }
}

/// One schema's reconciliation evidence.
pub struct ArchiveRow {
    pub schema: String,
    pub tier: Tier,
    pub last_active: u64,
    pub sandbox_id: String,
    /// `None` when the bound VM has no `data.ext4` on this host.
    pub disk_bytes: Option<u64>,
    pub archive_kind: &'static str,
    /// The exact S3 key a restore would read, so an operator can verify it
    /// out-of-band (`aws s3 ls`) before acting on this page's say-so.
    pub archive_key: String,
    pub archive_bytes: u64,
    pub archive_modified: String,
    pub verdict: Verdict,
}

/// How one schema's evidence reads. Pure, so the thresholds can be exercised
/// without S3 or a run dir — this single decision is what the page flags, and
/// what an operator's eye is drawn to.
///
/// Note the `archive_bytes > b` guard on `LooksEmpty`: a fresh-looking disk
/// next to an archive that is *smaller still* is not the empty-database case,
/// and restoring it would trade one near-empty database for another while
/// throwing away whichever had more. Only flag when the archive is the bigger
/// of the two.
fn verdict_for(disk_bytes: Option<u64>, archive_bytes: u64) -> Verdict {
    match disk_bytes {
        None => Verdict::NoDisk,
        Some(b) if b <= FRESH_DISK_MAX && archive_bytes > b => Verdict::LooksEmpty,
        Some(_) => Verdict::Serving,
    }
}

/// Gather the evidence. Only rows that are **not** already `Archived` are
/// probed: an archived row already restores correctly, and probing the whole
/// registry (tens of thousands of rows on a mature host) would be a HEAD storm
/// for no information.
///
/// Rows with no archive in S3 are dropped entirely — there is nothing to
/// reconcile and they are the overwhelming majority.
pub async fn scan(st: &DashState) -> Result<Vec<ArchiveRow>, String> {
    let Some(s3) = st.registry.archive_s3() else {
        return Err("the S3 archive tier is not configured (PG_VM_POOL_S3_* unset)".into());
    };
    let run_dir = st.registry.run_dir();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("building the S3 HTTP client: {e}"))?;

    let candidates: Vec<(String, crate::store::StoreRecord)> = st
        .registry
        .store_records()
        .into_iter()
        .filter(|(_, r)| r.tier != Tier::Archived)
        .collect();

    let rows = stream::iter(candidates)
        .map(|(schema, rec)| {
            let (s3, http, run_dir) = (s3.clone(), http.clone(), run_dir.clone());
            async move {
                let probe = crate::imgarchive::probe_archive(&s3, &http, &schema).await?;
                // Filesystem-only: the daemon is not consulted. A missing
                // directory is the same evidence whether heyvmd has forgotten
                // the VM or never had it, and asking costs a round-trip per
                // schema that can only agree with what the disk already says.
                let disk_bytes = run_dir.as_ref().and_then(|d| {
                    let p = d.join(&rec.sandbox_id).join("data.ext4");
                    p.exists().then(|| crate::orphans::file_allocated_bytes(&p))
                });
                let verdict = verdict_for(disk_bytes, probe.bytes);
                Some(ArchiveRow {
                    schema,
                    tier: rec.tier,
                    last_active: rec.last_active,
                    sandbox_id: rec.sandbox_id,
                    disk_bytes,
                    archive_kind: match probe.kind {
                        crate::imgarchive::RestoreKind::Dump => "dump",
                        crate::imgarchive::RestoreKind::Image => "image",
                    },
                    archive_key: probe.key,
                    archive_bytes: probe.bytes,
                    archive_modified: probe.last_modified,
                    verdict,
                })
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .filter_map(|r| async move { r })
        .collect::<Vec<_>>()
        .await;

    let mut rows = rows;
    // Suspect first, then biggest archive at risk — the operator's reading
    // order, not the registry's.
    rows.sort_by(|a, b| {
        a.verdict
            .cmp(&b.verdict)
            .then(b.archive_bytes.cmp(&a.archive_bytes))
    });
    Ok(rows)
}

/// What a lookup of one workbook id found, on both sides at once.
pub struct Lookup {
    pub schema: String,
    /// The object a restore would actually read, or `None` when S3 holds
    /// nothing usable under either key.
    pub archive: Option<Found>,
    /// This server's registry row — `None` is the interesting case: a workbook
    /// the pooler has no record of will get a brand new empty database on every
    /// connect, no matter what is in S3, because nothing tells `checkout` to
    /// look there.
    pub record: Option<crate::store::StoreRecord>,
    /// Allocated bytes of the bound VM's data disk, when the row binds one that
    /// exists on this host.
    pub disk_bytes: Option<u64>,
}

/// The archive object behind a [`Lookup`].
pub struct Found {
    pub kind: &'static str,
    pub key: String,
    pub bytes: u64,
    pub modified: String,
}

impl Lookup {
    /// Is there anything to align to?
    pub fn actionable(&self) -> bool {
        self.archive.is_some()
    }

    /// Would acting on this replace a disk that looks like it holds real data?
    /// The one case where the operator is trading something away rather than
    /// recovering something lost.
    pub fn would_displace_data(&self) -> Option<u64> {
        self.disk_bytes.filter(|b| *b > FRESH_DISK_MAX)
    }
}

/// Look one workbook id up on both sides: what S3 holds for it, and what this
/// server thinks it is. Read-only — two HEADs and a `stat`.
pub async fn lookup(st: &DashState, schema: &str) -> Result<Lookup, String> {
    if !crate::is_valid_schema(schema) {
        return Err(format!(
            "{schema:?} is not a usable schema name (1-63 chars, no control characters)"
        ));
    }
    let Some(s3) = st.registry.archive_s3() else {
        return Err("the S3 archive tier is not configured (PG_VM_POOL_S3_* unset)".into());
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("building the S3 HTTP client: {e}"))?;
    let archive = crate::imgarchive::probe_archive(&s3, &http, schema)
        .await
        .map(|p| Found {
            kind: p.kind_str(),
            key: p.key,
            bytes: p.bytes,
            modified: p.last_modified,
        });
    let record = st.registry.schema_record(schema);
    let disk_bytes = match (&record, st.registry.run_dir()) {
        (Some(rec), Some(dir)) => {
            let p = dir.join(&rec.sandbox_id).join("data.ext4");
            p.exists().then(|| crate::orphans::file_allocated_bytes(&p))
        }
        _ => None,
    };
    Ok(Lookup {
        schema: schema.to_string(),
        archive,
        record,
        disk_bytes,
    })
}

/// Query string for the page: an optional workbook id to look up, an optional
/// request to run the full scan, and the usual one-shot banner.
#[derive(Deserialize)]
pub struct PageQuery {
    pub lookup: Option<String>,
    pub scan: Option<String>,
    pub msg: Option<String>,
    pub err: Option<String>,
}

/// `GET /archives` — the recovery page.
///
/// The lookup is the primary tool and always cheap (two HEADs). The full scan
/// is opt-in behind `?scan=1` on purpose: it HEADs every non-archived row, so
/// on a mature host it is hundreds of round-trips that an operator who came
/// here to fix one known workbook has no reason to pay.
pub async fn page(State(st): State<DashState>, Query(q): Query<PageQuery>) -> Markup {
    let banner = Banner {
        msg: q.msg,
        err: q.err,
    };
    let looked = match q.lookup.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => Some(lookup(&st, id).await),
        None => None,
    };
    let scanned = match q.scan.as_deref() {
        Some("1") => Some(scan(&st).await),
        _ => None,
    };
    views::archives_page(&st, looked.as_ref(), scanned.as_ref(), &banner)
}

/// The workbook id an operator typed (or a row's hidden field). A form field
/// rather than a path segment: these ids come from a human and a path would
/// mangle anything containing a slash or a percent.
#[derive(Deserialize)]
pub struct RestoreForm {
    pub schema: String,
}

/// `POST /archives/restore` — align this workbook with its archive.
///
/// Two steps, in this order. First
/// [`crate::registry::SchemaRegistry::adopt_archive`] re-probes S3 and writes
/// the archived tier, creating the registry row when the server had none —
/// that alone is enough for the *next* connect to restore. Then a background
/// `checkout` performs that restore immediately, so the on-disk VM is aligned
/// now rather than whenever a client next happens to arrive, and the operator
/// gets a result on the events page instead of having to go and test it.
///
/// The checkout runs detached for the same reason the per-VM restore action
/// does: an image restore is a download, a decompress, an fsck and a boot —
/// minutes of work that must not be holding an HTTP request open.
pub async fn restore(State(st): State<DashState>, Form(f): Form<RestoreForm>) -> Redirect {
    let schema = f.schema.trim().to_string();
    let q = match st.registry.adopt_archive(&schema).await {
        Ok(msg) => {
            let registry = st.registry.clone();
            let s = schema.clone();
            tokio::spawn(async move {
                match registry.checkout(&s).await {
                    Ok(guard) => {
                        let vm = guard.entry().sandbox_id().to_string();
                        drop(guard);
                        crate::events::journal_info(
                            "archive-fix",
                            format!("schema {s}: restored from its archive into VM {vm}"),
                        );
                    }
                    Err(e) => crate::events::journal_error(
                        "archive-fix",
                        format!("schema {s}: restoring from its archive failed: {e:#}"),
                    ),
                }
            });
            format!(
                "msg={}",
                qenc(&format!("{msg}. Restoring now — outcome lands on the events page."))
            )
        }
        Err(e) => format!("err={}", qenc(&e.to_string())),
    };
    Redirect::to(&format!("/archives?lookup={}&{q}", qenc(&schema)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    /// The page's whole value is that it flags the right rows. Getting this
    /// wrong in the safe direction hides a schema serving an empty database;
    /// getting it wrong in the unsafe direction invites an operator to
    /// abandon a live one.
    #[test]
    fn verdict_separates_empty_rebinds_from_live_databases() {
        // A row binding a VM with no disk on this host serves nothing at all.
        assert_eq!(verdict_for(None, 2 * GIB), Verdict::NoDisk);

        // The case this tool exists for: a client connected, got a fresh VM,
        // and the real data is still sitting in S3. 109-118 MiB is the
        // measured allocation of a mkfs'd + initdb'd 2GB disk.
        assert_eq!(verdict_for(Some(109 * MIB), 2 * GIB), Verdict::LooksEmpty);
        assert_eq!(verdict_for(Some(118 * MIB), 2 * GIB), Verdict::LooksEmpty);

        // A disk carrying real data is never flagged, however big the archive
        // it was once restored from — that is the healthy post-thaw state.
        assert_eq!(verdict_for(Some(8 * GIB), 2 * GIB), Verdict::Serving);
        assert_eq!(verdict_for(Some(FRESH_DISK_MAX + 1), 2 * GIB), Verdict::Serving);

        // A near-empty disk against an even smaller archive is NOT the empty
        // case: restoring would discard the larger of two near-empty copies.
        assert_eq!(verdict_for(Some(110 * MIB), 50 * MIB), Verdict::Serving);
        // ...including exactly equal, where a restore gains nothing.
        assert_eq!(verdict_for(Some(110 * MIB), 110 * MIB), Verdict::Serving);
    }

    /// Rows an operator must act on have to be at the top; within them, the
    /// biggest archive at risk comes first.
    #[test]
    fn suspect_rows_sort_above_serving_ones() {
        let mut v = vec![
            (Verdict::Serving, 9 * GIB),
            (Verdict::LooksEmpty, 1 * GIB),
            (Verdict::NoDisk, 2 * GIB),
            (Verdict::LooksEmpty, 5 * GIB),
        ];
        v.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        assert_eq!(
            v,
            vec![
                (Verdict::NoDisk, 2 * GIB),
                (Verdict::LooksEmpty, 5 * GIB),
                (Verdict::LooksEmpty, 1 * GIB),
                (Verdict::Serving, 9 * GIB),
            ],
            "no-disk first, then looks-empty by descending archive size"
        );
    }
}
