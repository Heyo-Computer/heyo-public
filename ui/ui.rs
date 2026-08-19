//! The shared UI surface: assets, the theme cookie, and forwarded identity.
//!
//! ## Why this is a file rather than a crate
//!
//! Five apps, three axum versions — app-lb is on 0.7, app-obs, ci, heyosecret
//! and artifacts are on 0.8.x. A crate that spoke axum types could not be
//! depended on by all of them at once, and pinning them together to share a
//! header parser is the tail wagging the dog. So this module **names no
//! framework type at all**: it takes `Option<&str>` and closures, returns plain
//! data, and each app wires its own routes in its own version's idiom.
//!
//! It is included rather than depended on:
//!
//! ```ignore
//! #[path = "../../ui/ui.rs"]
//! mod heyo_ui;
//! ```
//!
//! which means no `Cargo.toml` entry, no lockfile churn, and — the reason that
//! matters here — no change to the `cache_key_files` any of these apps' CI
//! workflows fingerprint their warm VMs on. The cost is that each app compiles
//! its own copy, which for ~200 lines and six embedded fonts is nothing, and
//! that the tests at the bottom run once per app, which is a feature: five
//! independent proofs that the shared contract still holds.
//!
//! `include_str!`/`include_bytes!` below resolve against *this* file's
//! directory, not the including crate's, so the paths are `ui/`-relative and
//! stay correct from every app.

#![allow(dead_code)]

/// The stylesheet, the toggle and the six font faces, embedded at compile time.
///
/// Embedded rather than read from disk because these binaries are deployed as
/// single files — app-lb's own deploy notes are explicit that a build artifact
/// is a binary, not a directory — and a dashboard that renders unstyled because
/// somebody forgot to copy an `assets/` folder is the exact failure this avoids.
pub const CSS: &str = include_str!("heyo.css");
pub const THEME_JS: &str = include_str!("theme.js");

const FONT_PLEX_400: &[u8] = include_bytes!("fonts/ibm-plex-mono-400.woff2");
const FONT_PLEX_500: &[u8] = include_bytes!("fonts/ibm-plex-mono-500.woff2");
const FONT_PLEX_600: &[u8] = include_bytes!("fonts/ibm-plex-mono-600.woff2");
const FONT_PLEX_700: &[u8] = include_bytes!("fonts/ibm-plex-mono-700.woff2");
const FONT_SILK_400: &[u8] = include_bytes!("fonts/silkscreen-400.woff2");
const FONT_SILK_700: &[u8] = include_bytes!("fonts/silkscreen-700.woff2");

/// Where an app should mount [`asset`]. One prefix for all five, because the
/// stylesheet asks for its fonts by a path relative to itself (`fonts/*.woff2`)
/// and a browser resolves that against the URL the stylesheet came from.
pub const ASSET_PREFIX: &str = "/__ui/";

/// A served file: its bytes, its type, and whether it may be cached forever.
pub struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    /// True for the fonts, which are content-stable for the life of a binary.
    /// The CSS and JS are deliberately *not* immutable: they change with a
    /// deploy, and a year-long cache on a stylesheet is how a fleet ends up
    /// half-restyled with no way to tell people to hard-refresh.
    pub immutable: bool,
}

/// Resolve a path under [`ASSET_PREFIX`] — `"heyo.css"`, `"theme.js"`,
/// `"fonts/silkscreen-400.woff2"` — to the file it names.
///
/// An exhaustive match rather than a directory read: there is no filesystem
/// involved, so a request cannot traverse out of it, and an unknown name is a
/// plain `None` for the caller to turn into a 404.
pub fn asset(path: &str) -> Option<Asset> {
    let (bytes, content_type, immutable): (&'static [u8], &'static str, bool) = match path
        .trim_start_matches('/')
    {
        "heyo.css" => (CSS.as_bytes(), "text/css; charset=utf-8", false),
        "theme.js" => (THEME_JS.as_bytes(), "text/javascript; charset=utf-8", false),
        "fonts/ibm-plex-mono-400.woff2" => (FONT_PLEX_400, "font/woff2", true),
        "fonts/ibm-plex-mono-500.woff2" => (FONT_PLEX_500, "font/woff2", true),
        "fonts/ibm-plex-mono-600.woff2" => (FONT_PLEX_600, "font/woff2", true),
        "fonts/ibm-plex-mono-700.woff2" => (FONT_PLEX_700, "font/woff2", true),
        "fonts/silkscreen-400.woff2" => (FONT_SILK_400, "font/woff2", true),
        "fonts/silkscreen-700.woff2" => (FONT_SILK_700, "font/woff2", true),
        _ => return None,
    };
    Some(Asset { bytes, content_type, immutable })
}

/// `Cache-Control` for an asset. Fonts are immutable for the life of the
/// binary; the CSS and JS are revalidated so a deploy actually lands.
pub fn cache_control(a: &Asset) -> &'static str {
    if a.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=300"
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// The default cookie name. Overridable per installation so two unrelated Heyo
/// fleets on one parent domain do not fight over one cookie.
pub const THEME_COOKIE: &str = "heyo_theme";

/// The environment variable every app reads for the parent domain to widen the
/// theme cookie to — the theme half of what `auth.cookie_domain` does for the
/// session in app-lb's deployment spec.
///
/// Unset means a host-only cookie, which is the right default: it works for a
/// single app, for `localhost`, and for anyone who has not deployed a fleet.
/// It is a variable rather than a constant because **no domain belongs in this
/// source tree** — `us2.heyo.work` is one installation's, and this is an open
/// source project that other people deploy.
pub const COOKIE_DOMAIN_ENV: &str = "HEYO_UI_COOKIE_DOMAIN";
pub const COOKIE_NAME_ENV: &str = "HEYO_UI_COOKIE_NAME";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// The canonical palette, and what the marketing and retail sites are.
    #[default]
    Dark,
    Light,
    /// Defer to `prefers-color-scheme`.
    System,
}

impl Theme {
    /// The value for `<html data-theme="…">`.
    pub fn as_attr(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::System => "system",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "system" | "auto" => Some(Theme::System),
            _ => None,
        }
    }
}

/// Read the theme out of a raw `Cookie:` header.
///
/// Takes the header rather than a parsed jar because three of these apps have
/// no cookie library at all, and the parse is six lines. An unknown or absent
/// value is [`Theme::Dark`] — the default is a decision, not a fallback to
/// whatever the OS happens to prefer.
///
/// Reading this *server-side* is the whole point: the app stamps the answer on
/// `<html>` before sending a byte, so no page flashes the wrong ground. The
/// script only handles clicks.
pub fn theme_from_cookie_header(header: Option<&str>, name: &str) -> Theme {
    let Some(header) = header else {
        return Theme::Dark;
    };
    for part in header.split(';') {
        let part = part.trim();
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim() == name {
            // Last-wins is deliberate: a host-only cookie and a domain-wide one
            // can both be sent, and the browser sends the more specific first.
            // The fleet-wide choice is the one a person made most recently on
            // any app, so it is the one to honour.
            if let Some(t) = Theme::parse(v.trim().trim_matches('"')) {
                return t;
            }
        }
    }
    Theme::Dark
}

/// Canonicalize a cookie domain, or reject it.
///
/// The same shape app-lb's `normalize_cookie_domain` enforces, and for the same
/// reason: a `Domain` the page is not served from is discarded by the browser
/// *silently*, so the toggle would appear to do nothing and the fleet would
/// never agree. A leading dot is accepted and dropped — it has been meaningless
/// since RFC 6265, but everybody still writes it.
pub fn normalize_cookie_domain(raw: &str) -> Option<String> {
    let d = raw.trim().trim_start_matches('.').trim_end_matches('.');
    if d.is_empty() || d.len() > 253 || !d.contains('.') {
        return None;
    }
    let ok = d.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });
    ok.then(|| d.to_ascii_lowercase())
}

/// The two `<html>` attributes the toggle reads, rendered ready to interpolate.
///
/// Returns e.g. `data-theme="dark" data-cookie-domain="example.com"`. The
/// domain is omitted entirely when there is none, because an empty attribute
/// and an absent one mean different things to the script.
pub fn html_attrs(theme: Theme, cookie_domain: Option<&str>, cookie_name: &str) -> String {
    let mut s = format!(r#"data-theme="{}""#, theme.as_attr());
    if let Some(d) = cookie_domain.and_then(|d| normalize_cookie_domain(d)) {
        s.push_str(&format!(r#" data-cookie-domain="{}""#, escape(&d)));
    }
    if cookie_name != THEME_COOKIE {
        s.push_str(&format!(r#" data-cookie-name="{}""#, escape(cookie_name)));
    }
    s
}

/// The `<head>` tags every page needs: the stylesheet and the toggle script.
pub fn head_tags() -> String {
    format!(
        concat!(
            r#"<link rel="stylesheet" href="{p}heyo.css">"#,
            r#"<script defer src="{p}theme.js"></script>"#,
        ),
        p = ASSET_PREFIX
    )
}

/// The toggle button. `defer`red script, so it is wired by the time anyone
/// clicks; the label is filled in by the script from the live theme.
pub fn theme_toggle_html() -> String {
    r#"<button class="theme-toggle" type="button" data-theme-toggle><span data-theme-label>theme</span></button>"#
        .to_string()
}

/// One installation's cookie settings, resolved from the environment.
///
/// Two variables, each with a per-app override so a fleet can be configured
/// once and one app can differ:
///
/// ```text
/// <APP>_UI_COOKIE_DOMAIN, else HEYO_UI_COOKIE_DOMAIN   parent domain, or host-only
/// <APP>_UI_COOKIE_NAME,   else HEYO_UI_COOKIE_NAME     defaults to `heyo_theme`
/// ```
///
/// The same precedence app-lb and ci already use for their shared services
/// (`CI_HEYOSECRET_URL` before `HEYOSECRET_URL`), so an operator learns it once.
pub struct CookieConfig {
    /// Already normalized: a value the browser would discard never gets this
    /// far, because a silently dropped cookie is a toggle that appears to work
    /// and then forgets.
    pub domain: Option<String>,
    pub name: String,
}

impl CookieConfig {
    /// Read `<APP>_UI_COOKIE_*`, falling back to the fleet-wide `HEYO_UI_*`.
    pub fn from_env(app_prefix: &str) -> Self {
        let var = |name: String| -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.trim().is_empty())
        };
        Self::resolve(
            var(format!("{app_prefix}_UI_COOKIE_DOMAIN"))
                .or_else(|| var(COOKIE_DOMAIN_ENV.to_string())),
            var(format!("{app_prefix}_UI_COOKIE_NAME"))
                .or_else(|| var(COOKIE_NAME_ENV.to_string())),
        )
    }

    /// The half of [`from_env`](Self::from_env) that has no environment in it,
    /// which is the half worth testing: mutating the process environment from a
    /// test is a race against every other test in the binary, and on edition
    /// 2024 it is `unsafe` besides.
    pub fn resolve(domain: Option<String>, name: Option<String>) -> Self {
        let normalized = domain.as_deref().and_then(normalize_cookie_domain);
        if normalized.is_none() {
            if let Some(raw) = &domain {
                // A warning rather than a startup failure: the theme is not
                // worth refusing to boot over, and host-only is a working
                // fallback. But it is said out loud, because the symptom
                // otherwise is "the toggle does not stick on the other apps"
                // with nothing to search for.
                eprintln!(
                    "warning: UI cookie domain {raw:?} is not a domain a cookie can be scoped \
                     to; the theme will be remembered per host instead"
                );
            }
        }
        Self {
            domain: normalized,
            name: name.unwrap_or_else(|| THEME_COOKIE.to_string()),
        }
    }

    /// The theme this request asked for.
    pub fn theme(&self, cookie_header: Option<&str>) -> Theme {
        theme_from_cookie_header(cookie_header, &self.name)
    }

    /// The `<html>` attributes for this request.
    pub fn attrs(&self, cookie_header: Option<&str>) -> String {
        html_attrs(self.theme(cookie_header), self.domain.as_deref(), &self.name)
    }
}

impl Default for CookieConfig {
    fn default() -> Self {
        Self { domain: None, name: THEME_COOKIE.to_string() }
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The three headers app-lb's gate sets on a deployment with
/// `"forward_identity": true` (`app-lb/src/proxy.rs`).
pub const HEADER_EMAIL: &str = "x-auth-request-email";
pub const HEADER_USER: &str = "x-auth-request-user";
pub const HEADER_NAME: &str = "x-auth-request-name";

/// Who is making this request, according to the gate in front of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The provider's stable subject — Google's `sub`. The primary key for a
    /// person, because an email address can change under the same human and
    /// keying on it silently creates a second account.
    pub subject: String,
    pub email: String,
    pub name: Option<String>,
}

impl Identity {
    /// What to show in a UI: the display name if the gate had one, else the
    /// address.
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
}

/// Read an identity from request headers via a caller-supplied getter.
///
/// A closure rather than a `HeaderMap` so this compiles against every axum
/// version in the fleet — the caller writes `|n| headers.get(n).and_then(|v|
/// v.to_str().ok())` in whichever one it has.
///
/// **This is only as trustworthy as the gate in front of it.** app-lb strips all
/// three headers unconditionally before setting them, which is what makes them
/// unspoofable — on a *gated* deployment. An app reachable without that gate
/// must not call this, which is why every caller here takes it from a config
/// flag that says so out loud rather than trusting headers by default.
///
/// Both `subject` and `email` must be present: a half-set identity means
/// something upstream is misconfigured, and anonymous is the safe reading.
pub fn identity_from<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Identity> {
    let field = |name: &str| -> Option<String> {
        get(name)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Identity {
        subject: field(HEADER_USER)?,
        email: field(HEADER_EMAIL)?,
        name: field(HEADER_NAME),
    })
}

/// Minimal HTML escaping for values interpolated into the shell — an email or a
/// display name arrives from an identity provider and is not ours to trust.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The shared top bar, as HTML.
///
/// A string rather than a template type, because two of these apps render with
/// maud and three substitute into an `include_str!`d file — a `String` is the
/// only shape both can take. maud injects it with `PreEscaped`; the HTML files
/// replace a placeholder.
///
/// `nav` is `(label, href, is_current)`. `who` is the signed-in display name, or
/// `None` for an ungated deployment, where the bar simply carries no identity
/// rather than inventing an "anonymous" one.
pub fn topbar_html(app: &str, nav: &[(&str, &str, bool)], who: Option<&str>) -> String {
    let mut s = String::with_capacity(512);
    s.push_str(r#"<header class="topbar">"#);
    s.push_str(&format!(
        r#"<a class="topbar-brand" href="/"><span>heyo</span><span class="topbar-app">{}</span></a>"#,
        escape(app)
    ));
    s.push_str(r#"<nav class="topbar-nav">"#);
    for (label, href, current) in nav {
        s.push_str(&format!(
            r#"<a href="{}"{}>{}</a>"#,
            escape(href),
            if *current { r#" aria-current="page""# } else { "" },
            escape(label)
        ));
    }
    s.push_str("</nav>");
    s.push_str(r#"<div class="topbar-right">"#);
    if let Some(who) = who {
        s.push_str(&format!(
            r#"<span class="topbar-user" title="{0}">{0}</span>"#,
            escape(who)
        ));
    }
    s.push_str(&theme_toggle_html());
    s.push_str("</div></header>");
    s
}

#[cfg(test)]
mod heyo_ui_tests {
    use super::*;

    /// The stylesheet asks for its fonts by a path relative to itself, so every
    /// `url(fonts/…)` in it must be a name [`asset`] answers. A rename in one
    /// place and not the other is a dashboard in the fallback font, which looks
    /// like a design decision rather than a 404.
    #[test]
    fn every_font_the_css_asks_for_is_served() {
        let mut checked = 0;
        for (i, _) in CSS.match_indices("url(\"fonts/") {
            let rest = &CSS[i + 5..];
            let end = rest.find('"').expect("unterminated url()");
            let path = &rest[..end];
            assert!(asset(path).is_some(), "{path} is in the CSS but not served");
            checked += 1;
        }
        assert!(checked >= 6, "expected six faces, found {checked}");
    }

    #[test]
    fn assets_outside_the_set_are_not_served() {
        assert!(asset("../ui.rs").is_none());
        assert!(asset("fonts/../../etc/passwd").is_none());
        assert!(asset("").is_none());
    }

    /// Every token the stylesheet defines for dark must also exist for light.
    /// A missing one does not fail loudly — it inherits the dark value and
    /// produces one unreadable element on a page nobody was looking at.
    #[test]
    fn both_themes_define_the_same_tokens() {
        let names = |block: &str| -> Vec<String> {
            block
                .lines()
                .filter_map(|l| l.trim().strip_prefix("--"))
                .filter_map(|l| l.split(':').next())
                .map(|s| s.trim().to_string())
                .collect()
        };
        let dark_block = CSS
            .split(":root {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("dark :root block");
        let light_block = CSS
            .split(r#":root[data-theme="light"] {"#)
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("light block");
        let (dark, light) = (names(dark_block), names(light_block));
        // Type, spacing and radius are theme-independent on purpose, so they
        // are listed here rather than duplicated into the light block.
        let shared_only = [
            "font-display", "font-body", "radius",
            "gap-1", "gap-2", "gap-3", "gap-4", "gap-5", "gap-6",
        ];
        for token in dark {
            if shared_only.contains(&token.as_str()) {
                continue;
            }
            assert!(
                light.contains(&token),
                "--{token} is defined for dark but not for light"
            );
        }
    }

    #[test]
    fn a_missing_or_unknown_cookie_is_dark() {
        assert_eq!(theme_from_cookie_header(None, THEME_COOKIE), Theme::Dark);
        assert_eq!(
            theme_from_cookie_header(Some("other=1; heyo_theme=chartreuse"), THEME_COOKIE),
            Theme::Dark
        );
    }

    #[test]
    fn the_theme_cookie_is_read_from_a_crowded_header() {
        assert_eq!(
            theme_from_cookie_header(Some("applb_session=abc; heyo_theme=light"), THEME_COOKIE),
            Theme::Light
        );
        assert_eq!(
            theme_from_cookie_header(Some("heyo_theme=\"system\"; x=y"), THEME_COOKIE),
            Theme::System
        );
    }

    /// A cookie name is per-installation, and reading somebody else's cookie
    /// because the names collide is exactly what the override exists to avoid.
    #[test]
    fn only_the_configured_name_is_read() {
        assert_eq!(
            theme_from_cookie_header(Some("heyo_theme=light"), "acme_theme"),
            Theme::Dark
        );
        assert_eq!(
            theme_from_cookie_header(Some("acme_theme=light"), "acme_theme"),
            Theme::Light
        );
    }

    #[test]
    fn a_domain_that_cannot_work_is_refused_rather_than_emitted() {
        assert_eq!(normalize_cookie_domain(".Example.COM"), Some("example.com".into()));
        assert_eq!(normalize_cookie_domain("us2.heyo.work"), Some("us2.heyo.work".into()));
        // No dot: a browser would drop the cookie, so it never reaches the page.
        assert_eq!(normalize_cookie_domain("localhost"), None);
        assert_eq!(normalize_cookie_domain(""), None);
        assert_eq!(normalize_cookie_domain("bad_underscore.com"), None);
    }

    #[test]
    fn html_attrs_omit_what_is_not_configured() {
        let a = html_attrs(Theme::Dark, None, THEME_COOKIE);
        assert_eq!(a, r#"data-theme="dark""#);
        let b = html_attrs(Theme::Light, Some("example.com"), THEME_COOKIE);
        assert!(b.contains(r#"data-cookie-domain="example.com""#));
        // An unusable domain is dropped rather than emitted broken.
        let c = html_attrs(Theme::Dark, Some("localhost"), THEME_COOKIE);
        assert!(!c.contains("data-cookie-domain"));
    }

    /// Resolution is the part with the decisions in it: an unusable domain
    /// becomes host-only rather than being emitted broken, and an unset name
    /// falls back to the shared default.
    #[test]
    fn cookie_config_resolves_to_something_a_browser_accepts() {
        let c = CookieConfig::resolve(Some(".Example.com".into()), None);
        assert_eq!(c.domain.as_deref(), Some("example.com"));
        assert_eq!(c.name, THEME_COOKIE);
        assert!(c.attrs(Some("heyo_theme=light")).contains(r#"data-theme="light""#));

        // `localhost` has no dot, so a browser drops the cookie. Host-only is
        // the recoverable direction, and it is what a local run wants anyway.
        let local = CookieConfig::resolve(Some("localhost".into()), None);
        assert_eq!(local.domain, None);
        assert!(!local.attrs(None).contains("data-cookie-domain"));

        // A per-installation name is carried into the markup so the script
        // writes the cookie the server will read back.
        let named = CookieConfig::resolve(None, Some("acme_theme".into()));
        assert!(named.attrs(None).contains(r#"data-cookie-name="acme_theme""#));
        assert_eq!(named.theme(Some("acme_theme=system")), Theme::System);
    }

    #[test]
    fn identity_needs_both_halves() {
        let full = |n: &str| match n {
            HEADER_USER => Some("sub-1"),
            HEADER_EMAIL => Some("a@b.c"),
            HEADER_NAME => Some("A B"),
            _ => None,
        };
        let id = identity_from(full).expect("complete identity");
        assert_eq!(id.subject, "sub-1");
        assert_eq!(id.display(), "A B");

        // Email only: anonymous, not an account keyed on the mutable half.
        let half = |n: &str| (n == HEADER_EMAIL).then_some("a@b.c");
        assert!(identity_from(half).is_none());

        // Present but empty is the same as absent — a gate that sets a blank
        // header is misconfigured, not authenticating somebody called "".
        let blank = |n: &str| match n {
            HEADER_USER => Some("  "),
            HEADER_EMAIL => Some("a@b.c"),
            _ => None,
        };
        assert!(identity_from(blank).is_none());
    }

    #[test]
    fn a_display_name_cannot_inject_markup() {
        let bar = topbar_html("ci", &[("Runs", "/runs", true)], Some("<script>x</script>"));
        assert!(!bar.contains("<script>x"));
        assert!(bar.contains("&lt;script&gt;"));
        assert!(bar.contains(r#"aria-current="page""#));
    }
}
