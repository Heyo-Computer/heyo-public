//! Server-rendered dashboard, gated on admin credentials.
//!
//! Zero JavaScript and zero external assets. Not minimalism for its own sake:
//! this runs inside a Firecracker microVM with no route to a CDN, so a remote
//! font or script would simply fail to load, and a dashboard that needs the
//! network to render is useless exactly when you most want to look at it.
//!
//! Everything here is read-only. Deleting a blob, moving a tag, and running a
//! sweep all stay in the CLI — a dashboard is for answering "what is in there
//! and what is it costing me", and a misplaced click should not be able to
//! collect a blob a VM is booting from. Garbage collection is doubly excluded:
//! even a dry run takes the store lock exclusively, so a page load would stall
//! every writer on the host.
//!
//! ## Visual encoding
//!
//! The numbers that matter here are single values, so the forms are a hero
//! figure and stat tiles rather than charts. Two exceptions carry a bar:
//!
//! - **The capacity meter** is a status encoding (good / warning / critical).
//!   Status colour never carries meaning alone, so it always ships beside a
//!   glyph and a written percentage — which is also the required relief for the
//!   warning step sitting below 3:1 on the light surface.
//! - **Sparsity bars** are one sequential hue, light→dark, on a lighter step of
//!   the same ramp for the track. A single series needs no legend; the column
//!   header names it.

use crate::admin::{AdminAuth, SESSION_COOKIE};
use crate::digest::Digest;
use crate::manifest::Manifest;
use crate::store::{BlobInfo, Store, Usage};
use crate::tags::TagName;
use axum::extract::{Form, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;
use std::sync::Arc;

/// Rows before the overview stops listing and points at the full page.
const OVERVIEW_ROWS: usize = 12;

#[derive(Clone)]
pub struct WebState {
    pub store: Store,
    pub auth: Arc<AdminAuth>,
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LoginForm {
    user: String,
    password: String,
}

pub async fn login_page(State(st): State<WebState>, jar: CookieJar) -> Response {
    if session_ok(&st, &jar) {
        return Redirect::to("/dashboard").into_response();
    }
    page_bare("sign in", login_body(false)).into_response()
}

pub async fn login_submit(
    State(st): State<WebState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    if !st.auth.verify_login(&form.user, &form.password) {
        tracing::warn!(user = %form.user, "dashboard login failed");
        // Same page, same status shape, no hint about which field was wrong.
        return (
            StatusCode::UNAUTHORIZED,
            page_bare("sign in", login_body(true)),
        )
            .into_response();
    }
    let cookie = Cookie::build((SESSION_COOKIE, st.auth.session_token().to_string()))
        // HttpOnly: script must never be able to read it. SameSite=Strict: the
        // dashboard has no cross-site flows, so nothing legitimate is lost and
        // CSRF against the login POST is closed off.
        .http_only(true)
        .same_site(SameSite::Strict)
        .path("/")
        .build();
    (jar.add(cookie), Redirect::to("/dashboard")).into_response()
}

pub async fn logout(jar: CookieJar) -> Response {
    let mut removal = Cookie::from(SESSION_COOKIE);
    removal.set_path("/");
    (jar.remove(removal), Redirect::to("/login")).into_response()
}

fn session_ok(st: &WebState, jar: &CookieJar) -> bool {
    jar.get(SESSION_COOKIE)
        .map(|c| st.auth.verify_session(c.value()))
        .unwrap_or(false)
}

/// Authorize a dashboard request: session cookie, or Basic auth for `curl -u`.
pub fn dashboard_authorized(st: &WebState, headers: &header::HeaderMap, jar: &CookieJar) -> bool {
    if session_ok(st, jar) {
        return true;
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|h| st.auth.verify_basic(h))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

pub async fn index() -> Redirect {
    Redirect::to("/dashboard")
}

pub async fn overview(State(st): State<WebState>) -> Result<Markup, WebError> {
    let usage = st.store.usage().await?;
    let tags = st.store.list_tags().await?;
    let blobs = st.store.list_blobs().await?;

    let pinned = blobs.iter().filter(|b| b.nlink > 1).count();
    let shown: Vec<_> = blobs.iter().take(OVERVIEW_ROWS).collect();

    Ok(page(
        "overview",
        "/dashboard",
        html! {
            // Hero: the one number this store exists to improve. Exactly one
            // per view, proportional figures (tabular-nums is for columns).
            section.hero {
                div.hero-value { (format!("{:.1}×", dedup_ratio(&usage))) }
                div.hero-label {
                    "effective capacity — "
                    (human(usage.logical)) " of content stored in " (human(usage.allocated))
                }
            }

            section.tiles {
                (tile("Blobs", &usage.blobs.to_string(), None))
                (tile("Logical", &human(usage.logical), None))
                (tile("On disk", &human(usage.allocated), None))
                (tile("Manifests", &usage.manifests.to_string(), None))
                (tile("Tags", &usage.tags.to_string(), None))
                (tile("Pinned", &pinned.to_string(), Some("materialized elsewhere")))
            }

            (capacity_meter(&usage))

            section {
                (section_head("Tags", tags.len(), None))
                @if tags.is_empty() {
                    p.empty { "No tags. " code { "art tag <name> <ref>" } }
                } @else {
                    table {
                        thead { tr { th { "Tag" } th { "Target" } } }
                        tbody {
                            @for (t, d) in &tags {
                                tr {
                                    td.name { (t.as_str()) }
                                    td { a.mono href=(format!("/dashboard/blob/{d}")) { (short(d)) } }
                                }
                            }
                        }
                    }
                }
            }

            section {
                (section_head("Largest blobs", blobs.len(), Some("/dashboard/blobs")))
                (blob_table(&shown))
            }
        },
    ))
}

pub async fn blobs_page(State(st): State<WebState>) -> Result<Markup, WebError> {
    let blobs = st.store.list_blobs().await?;
    let refs: Vec<&BlobInfo> = blobs.iter().collect();
    Ok(page(
        "blobs",
        "/dashboard/blobs",
        html! {
            section {
                (section_head("All blobs", blobs.len(), None))
                (blob_table(&refs))
            }
        },
    ))
}

pub async fn blob_page(
    State(st): State<WebState>,
    Path(digest): Path<String>,
) -> Result<Markup, WebError> {
    let d = Digest::parse(&digest).map_err(crate::Error::from)?;
    let info = st.store.stat(&d).await?;

    // Which tags and manifests point here. Both lists are small enough to scan
    // directly; an index would be a second source of truth for something the
    // filesystem already answers.
    let tags: Vec<TagName> = st
        .store
        .list_tags()
        .await?
        .into_iter()
        .filter(|(_, td)| *td == d)
        .map(|(t, _)| t)
        .collect();

    let mut manifests = Vec::new();
    for md in st.store.list_manifests().await? {
        if let Ok(m) = st.store.get_manifest(&md).await
            && m.entries.iter().any(|e| e.digest == d)
        {
            manifests.push((md, m));
        }
    }

    let saved = info.size.saturating_sub(info.allocated);
    Ok(page(
        "blob",
        "/dashboard/blobs",
        html! {
            section {
                h2 { "Blob" }
                p.digest.mono { (info.digest.as_str()) }
                section.tiles {
                    (tile("Logical", &human(info.size), None))
                    (tile("On disk", &human(info.allocated), None))
                    (tile("Reclaimed", &human(saved), Some("zero runs punched out")))
                    (tile("Links", &info.nlink.to_string(),
                          Some(if info.nlink > 1 { "pinned by a materialization" }
                               else { "store entry only" })))
                }
                (sparsity_bar(&info, true))
            }

            section {
                (section_head("Referenced by", tags.len() + manifests.len(), None))
                @if tags.is_empty() && manifests.is_empty() {
                    p.empty {
                        "Nothing references this blob. "
                        @if info.nlink > 1 {
                            "It survives collection only because something outside the store holds a hardlink."
                        } @else {
                            "It is eligible for collection once it is older than the grace window."
                        }
                    }
                }
                @if !tags.is_empty() {
                    table {
                        thead { tr { th { "Tag" } } }
                        tbody { @for t in &tags { tr { td.name { (t.as_str()) } } } }
                    }
                }
                @if !manifests.is_empty() {
                    table {
                        thead { tr { th { "Manifest" } th { "Kind" } th { "Entry" } } }
                        tbody {
                            @for (md, m) in &manifests {
                                tr {
                                    td { a.mono href=(format!("/dashboard/manifest/{md}")) { (short(md)) } }
                                    td { (m.kind) }
                                    td.name {
                                        (m.entries.iter().find(|e| e.digest == d)
                                          .map(|e| e.name.as_str()).unwrap_or("—"))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    ))
}

pub async fn manifests_page(State(st): State<WebState>) -> Result<Markup, WebError> {
    let mut rows = Vec::new();
    for d in st.store.list_manifests().await? {
        let m = st.store.get_manifest(&d).await.ok();
        rows.push((d, m));
    }
    Ok(page(
        "manifests",
        "/dashboard/manifests",
        html! {
            section {
                (section_head("Manifests", rows.len(), None))
                @if rows.is_empty() {
                    p.empty { "No manifests." }
                } @else {
                    table {
                        thead { tr { th { "Digest" } th { "Kind" } th.r { "Entries" } th.r { "Total" } } }
                        tbody {
                            @for (d, m) in &rows {
                                tr {
                                    td { a.mono href=(format!("/dashboard/manifest/{d}")) { (short(d)) } }
                                    td { (m.as_ref().map(|m| m.kind.as_str()).unwrap_or("unreadable")) }
                                    td.r { (m.as_ref().map(|m| m.entries.len()).unwrap_or(0)) }
                                    td.r { (m.as_ref().map(|m| human(m.total_size())).unwrap_or_else(|| "—".into())) }
                                }
                            }
                        }
                    }
                }
            }
        },
    ))
}

pub async fn manifest_page(
    State(st): State<WebState>,
    Path(digest): Path<String>,
) -> Result<Markup, WebError> {
    let d = Digest::parse(&digest).map_err(crate::Error::from)?;
    let m: Manifest = st.store.get_manifest(&d).await?;
    Ok(page(
        "manifest",
        "/dashboard/manifests",
        html! {
            section {
                h2 { "Manifest" }
                p.digest.mono { (d.as_str()) }
                section.tiles {
                    (tile("Kind", &m.kind, None))
                    (tile("Schema", &m.schema.to_string(), None))
                    (tile("Entries", &m.entries.len().to_string(), None))
                    (tile("Total", &human(m.total_size()), None))
                }
            }
            section {
                (section_head("Entries", m.entries.len(), None))
                table {
                    thead { tr { th { "Name" } th { "Digest" } th.r { "Size" } } }
                    tbody {
                        @for e in &m.entries {
                            tr {
                                td.name { (e.name) }
                                td { a.mono href=(format!("/dashboard/blob/{}", e.digest)) { (short(&e.digest)) } }
                                td.r { (human(e.size)) }
                            }
                        }
                    }
                }
            }
            @if !m.annotations.is_empty() {
                section {
                    (section_head("Annotations", m.annotations.len(), None))
                    table {
                        tbody {
                            @for (k, v) in &m.annotations {
                                tr { td.name { (k) } td.mono { (v) } }
                            }
                        }
                    }
                }
            }
        },
    ))
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

fn section_head(title: &str, count: usize, more: Option<&str>) -> Markup {
    html! {
        div.sec-head {
            h2 { (title) " " span.count { (count) } }
            @if let Some(href) = more {
                a.more href=(href) { "view all →" }
            }
        }
    }
}

/// Stat tile: label in sentence case with no trailing colon, value semibold.
fn tile(label: &str, value: &str, note: Option<&str>) -> Markup {
    html! {
        div.tile {
            div.tile-label { (label) }
            div.tile-value { (value) }
            @if let Some(n) = note { div.tile-note { (n) } }
        }
    }
}

/// Filesystem capacity, as a status meter.
///
/// The fill carries severity and the track is a lighter step of the same ramp,
/// so state reads across the whole bar. Status colour never stands alone: the
/// glyph and the written percentage carry the same information, which is what
/// makes this legible to a colourblind reader, in forced-colors mode, and on
/// the light surface where the warning step falls below 3:1.
fn capacity_meter(u: &Usage) -> Markup {
    let used = u.fs_total.saturating_sub(u.fs_available);
    let pct = if u.fs_total > 0 {
        (used as f64 / u.fs_total as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let (level, glyph, word) = match pct {
        p if p >= 95.0 => ("critical", "!!", "critical"),
        p if p >= 85.0 => ("warning", "! ", "low"),
        _ => ("good", "OK", "healthy"),
    };
    html! {
        section.meter-wrap {
            div.meter-head {
                span.meter-title { "Filesystem" }
                span.(format!("badge badge-{level}")) {
                    span.badge-glyph aria-hidden="true" { (glyph) }
                    " " (word)
                }
            }
            div.meter role="img"
                aria-label=(format!("{:.0}% of the filesystem used, {} free of {}", pct, human(u.fs_available), human(u.fs_total))) {
                div.(format!("meter-fill meter-{level}")) style=(format!("width:{pct:.1}%")) {}
            }
            div.meter-foot {
                span { (format!("{pct:.0}% used")) }
                span { (human(u.fs_available)) " free of " (human(u.fs_total)) }
            }
        }
    }
}

/// One sequential hue, light→dark, on a lighter step of the same ramp.
///
/// A single series, so no legend — the column header names it.
fn sparsity_bar(b: &BlobInfo, wide: bool) -> Markup {
    let pct = if b.size > 0 {
        (b.allocated as f64 / b.size as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let label = format!(
        "{} stored of {} logical ({:.0}%)",
        human(b.allocated),
        human(b.size),
        pct
    );
    html! {
        div.(if wide { "spark spark-wide" } else { "spark" })
            role="img" aria-label=(label) title=(label) {
            div.spark-fill style=(format!("width:{:.1}%", pct.max(1.0))) {}
        }
    }
}

fn blob_table(blobs: &[&BlobInfo]) -> Markup {
    html! {
        @if blobs.is_empty() {
            p.empty { "No blobs yet. " code { "art put <file>" } }
        } @else {
            div.scroll {
                table {
                    thead {
                        tr {
                            th { "Digest" }
                            th.r { "Logical" }
                            th.r { "On disk" }
                            th { "Stored / logical" }
                            th.r { "Links" }
                        }
                    }
                    tbody {
                        @for b in blobs {
                            tr {
                                td { a.mono href=(format!("/dashboard/blob/{}", b.digest)) { (short(&b.digest)) } }
                                td.r.num { (human(b.size)) }
                                td.r.num { (human(b.allocated)) }
                                td.barcell { (sparsity_bar(b, false)) }
                                td.r.num {
                                    (b.nlink)
                                    @if b.nlink > 1 { span.pin title="pinned by a materialization" { " ●" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn dedup_ratio(u: &Usage) -> f64 {
    if u.allocated > 0 {
        u.logical as f64 / u.allocated as f64
    } else {
        1.0
    }
}

fn short(d: &Digest) -> String {
    format!("{}…", &d.as_str()[..12])
}

fn human(n: u64) -> String {
    crate::cli::human(n)
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

fn page(title: &str, active: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "artifacts · " (title) }
                style { (PreEscaped(STYLE)) }
            }
            body {
                header.top {
                    a.brand href="/dashboard" { "artifacts" }
                    nav {
                        (nav_link("Overview", "/dashboard", active))
                        (nav_link("Blobs", "/dashboard/blobs", active))
                        (nav_link("Manifests", "/dashboard/manifests", active))
                    }
                    form method="post" action="/logout" { button.signout type="submit" { "sign out" } }
                }
                main { (body) }
            }
        }
    }
}

/// The login page has no nav and no sign-out.
fn page_bare(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "artifacts · " (title) }
                style { (PreEscaped(STYLE)) }
            }
            body.centered { main { (body) } }
        }
    }
}

fn login_body(failed: bool) -> Markup {
    html! {
        div.login {
            h1 { "artifacts" }
            p.sub { "sign in to the dashboard" }
            @if failed {
                // One message for both fields: which one was wrong is not the
                // visitor's business to learn by probing.
                p.err role="alert" { "Incorrect username or password." }
            }
            form method="post" action="/login" {
                label for="user" { "Username" }
                input #user name="user" type="text" autocomplete="username" required autofocus;
                label for="password" { "Password" }
                input #password name="password" type="password" autocomplete="current-password" required;
                button type="submit" { "sign in" }
            }
        }
    }
}

fn nav_link(label: &str, href: &str, active: &str) -> Markup {
    let cls = if href == active { "nav-link active" } else { "nav-link" };
    html! { a.(cls) href=(href) { (label) } }
}

/// Colours are the validated reference palette: a sequential blue ramp for
/// magnitude and the fixed status trio for state. Dark mode is stepped for the
/// dark surface rather than flipped, and the viewer's theme toggle wins over the
/// OS preference in both directions.
const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --surface:      #fcfcfb;
  --plane:        #f9f9f7;
  --ink:          #0b0b0b;
  --ink-2:        #52514e;
  --muted:        #898781;
  --grid:         #e1e0d9;
  --rule:         #c3c2b7;
  /* sequential blue, light->dark: fill is step 450, track is step 150 */
  --seq-fill:     #2a78d6;
  --seq-track:    #b7d3f6;
  --good:         #0ca30c;
  --warning:      #fab219;
  --critical:     #d03b3b;
  --link:         #256abf;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --surface:    #1a1a19;
    --plane:      #0d0d0d;
    --ink:        #ffffff;
    --ink-2:      #c3c2b7;
    --muted:      #898781;
    --grid:       #2c2c2a;
    --rule:       #383835;
    --seq-fill:   #3987e5;
    --seq-track:  #184f95;
    --link:       #86b6ef;
  }
}
:root[data-theme="dark"] {
  --surface:    #1a1a19;
  --plane:      #0d0d0d;
  --ink:        #ffffff;
  --ink-2:      #c3c2b7;
  --grid:       #2c2c2a;
  --rule:       #383835;
  --seq-fill:   #3987e5;
  --seq-track:  #184f95;
  --link:       #86b6ef;
}

* { box-sizing: border-box; }
body {
  margin: 0; background: var(--plane); color: var(--ink);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 14px; line-height: 1.55;
}
body.centered { display: grid; place-items: center; min-height: 100vh; padding: 1rem; }
main { max-width: 62rem; margin-inline: auto; padding: 1.5rem 1.25rem 4rem; }

/* header */
.top {
  display: flex; align-items: center; gap: 1rem; flex-wrap: wrap;
  padding: 0.85rem 1.25rem; background: var(--surface);
  border-bottom: 1px solid var(--rule);
}
.brand { font-weight: 700; letter-spacing: 0.04em; color: var(--ink); text-decoration: none; }
nav { display: flex; gap: 0.25rem; flex: 1; flex-wrap: wrap; }
.nav-link {
  color: var(--ink-2); text-decoration: none; padding: 0.2rem 0.6rem;
  border: 1px solid transparent;
}
.nav-link:hover { color: var(--ink); border-color: var(--grid); }
.nav-link.active { color: var(--ink); border-color: var(--rule); background: var(--plane); }
.signout {
  font: inherit; color: var(--ink-2); background: none;
  border: 1px solid var(--grid); padding: 0.2rem 0.6rem; cursor: pointer;
}
.signout:hover { color: var(--ink); border-color: var(--rule); }

/* hero — exactly one per view, proportional figures */
.hero { padding: 1.5rem 0 1rem; }
.hero-value { font-size: 52px; font-weight: 600; line-height: 1; letter-spacing: -0.02em; }
.hero-label { color: var(--ink-2); margin-top: 0.4rem; }

/* stat tiles */
.tiles {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(8.5rem, 1fr));
  gap: 0.6rem; margin: 1rem 0 1.5rem;
}
.tile { background: var(--surface); border: 1px solid var(--grid); padding: 0.7rem 0.8rem; }
.tile-label { color: var(--muted); font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.09em; }
.tile-value { font-size: 1.3rem; font-weight: 600; margin-top: 0.15rem; }
.tile-note { color: var(--muted); font-size: 0.7rem; margin-top: 0.15rem; }

/* capacity meter */
.meter-wrap { background: var(--surface); border: 1px solid var(--grid); padding: 0.85rem; margin-bottom: 1.75rem; }
.meter-head { display: flex; justify-content: space-between; align-items: center; gap: 0.5rem; }
.meter-title { color: var(--muted); font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.09em; }
.badge { font-size: 0.72rem; font-weight: 600; padding: 0.1rem 0.45rem; border: 1px solid currentColor; }
.badge-glyph { font-weight: 700; }
.badge-good { color: var(--good); }
.badge-warning { color: var(--warning); }
.badge-critical { color: var(--critical); }
.meter { height: 10px; background: var(--seq-track); margin: 0.55rem 0 0.4rem; overflow: hidden; }
.meter-fill { height: 100%; border-radius: 0 4px 4px 0; }
.meter-good { background: var(--good); }
.meter-warning { background: var(--warning); }
.meter-critical { background: var(--critical); }
.meter-foot { display: flex; justify-content: space-between; color: var(--ink-2); font-size: 0.78rem; }

/* sections & tables */
.sec-head { display: flex; align-items: baseline; justify-content: space-between; gap: 1rem; margin: 1.75rem 0 0.5rem; }
h2 { font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.11em; color: var(--muted); margin: 0; font-weight: 600; }
.count { color: var(--rule); font-weight: 400; }
.more { color: var(--link); text-decoration: none; font-size: 0.78rem; }
.more:hover { text-decoration: underline; }
.scroll { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; background: var(--surface); }
th, td { text-align: left; padding: 0.4rem 0.6rem; border-bottom: 1px solid var(--grid); }
th { color: var(--muted); font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.08em; font-weight: 600; }
tbody tr:hover { background: var(--plane); }
.r { text-align: right; }
/* tabular figures only in columns, where digits must line up */
.num { font-variant-numeric: tabular-nums; white-space: nowrap; }
.mono, .name { white-space: nowrap; }
.name { color: var(--ink); }
a.mono { color: var(--link); text-decoration: none; }
a.mono:hover { text-decoration: underline; }
.pin { color: var(--seq-fill); }
.digest { word-break: break-all; color: var(--ink-2); font-size: 0.82rem; margin: 0.25rem 0 1rem; }
.empty { color: var(--muted); }
code { background: var(--surface); border: 1px solid var(--grid); padding: 0.05rem 0.3rem; }

/* sparsity bar: 2px surface gap keeps the fill off the track edge */
.spark { width: 9rem; height: 8px; background: var(--seq-track); overflow: hidden; }
.spark-wide { width: 100%; height: 10px; margin-top: 0.9rem; }
.spark-fill { height: 100%; background: var(--seq-fill); border-radius: 0 4px 4px 0; }
.barcell { width: 10rem; }

/* login */
.login { background: var(--surface); border: 1px solid var(--rule); padding: 1.75rem; width: min(22rem, 92vw); }
.login h1 { margin: 0; font-size: 1.35rem; letter-spacing: 0.04em; }
.sub { color: var(--muted); margin: 0.2rem 0 1.25rem; }
.login label { display: block; color: var(--muted); font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.09em; margin-bottom: 0.2rem; }
.login input {
  font: inherit; width: 100%; padding: 0.45rem 0.55rem; margin-bottom: 0.9rem;
  background: var(--plane); color: var(--ink); border: 1px solid var(--rule);
}
.login input:focus-visible, .signout:focus-visible, .login button:focus-visible, a:focus-visible {
  outline: 2px solid var(--link); outline-offset: 2px;
}
.login button {
  font: inherit; font-weight: 600; width: 100%; padding: 0.5rem;
  background: var(--seq-fill); color: #fff; border: 1px solid var(--seq-fill); cursor: pointer;
}
.login button:hover { filter: brightness(1.08); }
.err { color: var(--critical); border: 1px solid currentColor; padding: 0.4rem 0.55rem; margin-bottom: 1rem; font-size: 0.82rem; }

@media (prefers-reduced-motion: no-preference) { .nav-link, .more, .signout { transition: color 0.12s; } }
@media (max-width: 40rem) {
  th, td { padding: 0.35rem 0.4rem; font-size: 0.8rem; }
  .hero-value { font-size: 40px; }
  .barcell, .spark { width: 5.5rem; }
}
@media (forced-colors: active) {
  .meter-fill, .spark-fill { background: CanvasText; }
  .tile, .meter-wrap, table { border-color: CanvasText; }
}
"#;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Dashboard errors render as HTML, never JSON — the format follows the
/// handler's return type, so a mismatch is impossible.
#[derive(Debug)]
pub struct WebError(crate::Error);

impl From<crate::Error> for WebError {
    fn from(e: crate::Error) -> Self {
        WebError(e)
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            crate::Error::NotFound(_) | crate::Error::TagNotFound(_) => StatusCode::NOT_FOUND,
            crate::Error::Digest(_) | crate::Error::TagName(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self.0, "dashboard request failed");
        }
        let body = page_bare(
            "error",
            html! {
                div.login {
                    h1 { (status.as_u16()) }
                    p.sub { (self.0.to_string()) }
                    p { a.more href="/dashboard" { "← back to the dashboard" } }
                }
            },
        );
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::AdminAuth;

    /// A fresh store per test. The `TempDir` is returned, not dropped: tests run
    /// in parallel threads of one process, so anything keyed on the process id
    /// would have them deleting each other's store out from under themselves.
    fn state() -> (WebState, tempfile::TempDir) {
        let dir = match std::env::var_os("ART_TEST_DIR").map(std::path::PathBuf::from) {
            Some(b) => tempfile::tempdir_in(b).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let store = Store::open(&crate::Config {
            root: dir.path().join("store"),
            min_free_bytes: 0,
            gc_min_age: std::time::Duration::ZERO,
            heyvm_images_dir: std::path::PathBuf::from("/nonexistent"),
        })
        .unwrap();
        (
            WebState {
                store,
                auth: Arc::new(AdminAuth::new("admin".into(), "pw".into())),
            },
            dir,
        )
    }

    fn usage(logical: u64, allocated: u64, avail: u64, total: u64) -> Usage {
        Usage {
            blobs: 1,
            logical,
            allocated,
            manifests: 0,
            tags: 0,
            fs_available: avail,
            fs_total: total,
        }
    }

    #[test]
    fn dedup_ratio_handles_an_empty_store() {
        // No division by zero on a store that has never been written to.
        assert_eq!(dedup_ratio(&usage(0, 0, 1, 2)), 1.0);
        assert!((dedup_ratio(&usage(1000, 100, 1, 2)) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn capacity_meter_escalates_and_always_carries_text() {
        for (avail, total, level, word) in [
            (50u64, 100u64, "good", "healthy"),
            (10, 100, "warning", "low"),
            (2, 100, "critical", "critical"),
        ] {
            let html = capacity_meter(&usage(0, 0, avail, total)).into_string();
            assert!(html.contains(level), "expected {level} in {html}");
            // Status colour never stands alone: the word and the percentage are
            // what a colourblind or forced-colors reader relies on.
            assert!(html.contains(word), "missing status word {word}");
            assert!(html.contains("% used"), "missing written percentage");
            assert!(html.contains("aria-label"), "meter needs an accessible name");
        }
    }

    #[test]
    fn capacity_meter_survives_a_zero_sized_filesystem() {
        let html = capacity_meter(&usage(0, 0, 0, 0)).into_string();
        assert!(html.contains("0% used"));
    }

    #[test]
    fn sparsity_bar_is_labelled_and_clamped() {
        let b = BlobInfo {
            digest: Digest::parse(&hex::encode([1u8; 32])).unwrap(),
            size: 1000,
            allocated: 100,
            nlink: 1,
            created: std::time::SystemTime::UNIX_EPOCH,
            deduped: false,
        };
        let html = sparsity_bar(&b, false).into_string();
        assert!(html.contains("aria-label"));
        assert!(html.contains("10%"));

        // A zero-length blob must not divide by zero or overflow the track.
        let empty = BlobInfo { size: 0, allocated: 0, ..b.clone() };
        let html = sparsity_bar(&empty, false).into_string();
        assert!(html.contains("width:1.0%"), "{html}");

        // Allocation above logical size (a dense small file with metadata
        // overhead) must clamp rather than run past 100%.
        let over = BlobInfo { size: 100, allocated: 4096, ..b.clone() };
        assert!(sparsity_bar(&over, false).into_string().contains("width:100.0%"));
    }

    #[test]
    fn login_page_has_no_external_assets_and_no_script() {
        let html = page_bare("sign in", login_body(false)).into_string();
        // The VM has no route to a CDN, so anything remote would not load.
        assert!(!html.contains("http://"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("<script"));
        assert!(html.contains("type=\"password\""));
        assert!(html.contains("autocomplete=\"current-password\""));
    }

    #[test]
    fn a_failed_login_does_not_say_which_field_was_wrong() {
        let html = page_bare("sign in", login_body(true)).into_string();
        assert!(html.contains("Incorrect username or password"));
        assert!(!html.to_lowercase().contains("no such user"));
        assert!(!html.to_lowercase().contains("wrong password"));
    }

    #[test]
    fn page_shell_marks_the_active_nav_item() {
        let html = page("overview", "/dashboard", html! {}).into_string();
        assert!(html.contains("nav-link active"));
        assert!(html.contains("/dashboard/blobs"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn dark_mode_is_stepped_not_flipped() {
        // The dark values are their own steps from the same ramps, and the
        // theme toggle must beat the OS preference in both directions.
        assert!(STYLE.contains("prefers-color-scheme: dark"));
        assert!(STYLE.contains("[data-theme=\"dark\"]"));
        assert!(STYLE.contains(":not([data-theme=\"light\"])"));
        assert!(STYLE.contains("forced-colors: active"));
    }

    #[tokio::test]
    async fn overview_renders_on_an_empty_store() {
        let (st, _dir) = state();
        let html = overview(State(st)).await.unwrap().into_string();
        assert!(html.contains("No blobs yet"));
        assert!(html.contains("effective capacity"));
    }

    #[tokio::test]
    async fn blob_page_reports_what_references_a_blob() {
        let (st, _dir) = state();
        let blob = st.store.insert_bytes(b"referenced".to_vec()).await.unwrap();
        let m = Manifest::new(crate::manifest::KIND_GENERIC).with_entry(
            "rootfs.ext4",
            blob.digest.clone(),
            blob.size,
        );
        let md = st.store.put_manifest(&m).await.unwrap();
        st.store
            .set_tag(&TagName::parse("live").unwrap(), &md)
            .await
            .unwrap();

        let html = blob_page(State(st), Path(blob.digest.to_string()))
            .await
            .unwrap()
            .into_string();
        assert!(html.contains("rootfs.ext4"));
        assert!(html.contains(&short(&md)));
    }

    #[tokio::test]
    async fn an_unreferenced_blob_says_so() {
        let (st, _dir) = state();
        let blob = st.store.insert_bytes(b"orphan".to_vec()).await.unwrap();
        let html = blob_page(State(st), Path(blob.digest.to_string()))
            .await
            .unwrap()
            .into_string();
        assert!(html.contains("Nothing references this blob"));
        assert!(html.contains("eligible for collection"));
    }

    #[tokio::test]
    async fn a_bad_digest_renders_an_html_error_not_json() {
        let (st, _dir) = state();
        let resp = blob_page(State(st), Path("not-a-digest".into()))
            .await
            .unwrap_err()
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
