/* ==========================================================================
   Theme toggle — shared by app-lb, app-obs, ci, heyosecret and artifacts.
   ==========================================================================

   The choice lives in a **cookie**, not in localStorage, and that is the whole
   point of this file. localStorage is per-origin: a fleet on
   `ci.example.com`, `obs.example.com` and `secrets.example.com` is three
   origins, so a person who picks light mode picks it three times and it comes
   back dark on the fourth app they open. A cookie scoped to the parent domain
   is one choice for all of them — the same trick app-lb's session cookie uses,
   for the same reason.

   It is also what lets the server render the right palette on the *first* byte:
   the app reads the cookie, stamps `data-theme` on `<html>`, and no page ever
   flashes the wrong ground. JavaScript that applied the theme on load could not
   do that, and a dashboard that blinks white on every navigation is a dashboard
   people notice.

   ## The contract

       <html data-theme="dark|light|system"
             data-cookie-domain=".example.com"    <- optional; omit for host-only
             data-cookie-name="heyo_theme">       <- optional; this is the default

   Both attributes are written by the server from its own configuration, which
   is what keeps this file free of any particular deployment's domain. See
   ui/README.md.

   No framework, no build step, no dependencies: it is loaded with a plain
   `<script defer src="/__ui/theme.js">` and touches nothing but the root
   element and the toggle buttons.
   ========================================================================== */

(function () {
  "use strict";

  var root = document.documentElement;
  var DEFAULT_NAME = "heyo_theme";

  /* dark is first because it is the default, and `system` is last because it is
     the escape hatch rather than a destination most people want. */
  var ORDER = ["dark", "light", "system"];

  var LABEL = { dark: "dark", light: "light", system: "auto" };

  function cookieName() {
    return root.getAttribute("data-cookie-name") || DEFAULT_NAME;
  }

  /* The domain the cookie is written for. Absent means a host-only cookie,
     which is correct for a single-app install and for anyone running this
     locally — a `Domain` naming something the page is not served from is
     discarded by the browser, silently, and the toggle would appear to do
     nothing on the next page load. */
  function cookieDomain() {
    var d = root.getAttribute("data-cookie-domain");
    return d && d.trim() ? d.trim() : null;
  }

  function readCookie(name) {
    var parts = document.cookie ? document.cookie.split(";") : [];
    for (var i = 0; i < parts.length; i++) {
      var p = parts[i].trim();
      if (p.indexOf(name + "=") === 0) {
        return decodeURIComponent(p.slice(name.length + 1));
      }
    }
    return null;
  }

  function writeCookie(value) {
    var parts = [
      cookieName() + "=" + encodeURIComponent(value),
      "path=/",
      /* A year. A theme is a preference, not a session — expiring it would mean
         the fleet quietly goes dark again on people who chose otherwise. */
      "max-age=31536000",
      /* Lax, not None: this cookie is never needed on a cross-site POST, and
         None would require Secure and invite it into requests it has no
         business in. */
      "SameSite=Lax",
    ];
    var domain = cookieDomain();
    if (domain) parts.push("domain=" + domain);
    /* Secure only over https, because a local http install must be able to set
       it too — a Secure cookie on http is dropped without a word. */
    if (location.protocol === "https:") parts.push("Secure");
    document.cookie = parts.join("; ");
  }

  function current() {
    var attr = root.getAttribute("data-theme");
    if (ORDER.indexOf(attr) !== -1) return attr;
    var fromCookie = readCookie(cookieName());
    return ORDER.indexOf(fromCookie) !== -1 ? fromCookie : "dark";
  }

  function apply(theme) {
    root.setAttribute("data-theme", theme);
    var buttons = document.querySelectorAll("[data-theme-toggle]");
    for (var i = 0; i < buttons.length; i++) {
      var b = buttons[i];
      var label = b.querySelector("[data-theme-label]") || b;
      label.textContent = LABEL[theme] || theme;
      b.setAttribute("aria-label", "Theme: " + (LABEL[theme] || theme) + ". Click to change.");
      b.setAttribute("title", "Theme: " + (LABEL[theme] || theme));
    }
  }

  function next() {
    var i = ORDER.indexOf(current());
    return ORDER[(i + 1) % ORDER.length];
  }

  function set(theme) {
    if (ORDER.indexOf(theme) === -1) theme = "dark";
    writeCookie(theme);
    apply(theme);
  }

  /* Bound on the container rather than per button, so a page that renders its
     toggle after this script runs — or renders several — still works. */
  document.addEventListener("click", function (ev) {
    var btn = ev.target.closest && ev.target.closest("[data-theme-toggle]");
    if (!btn) return;
    ev.preventDefault();
    set(next());
  });

  /* The server should have stamped `data-theme` already. This re-applies the
     same value to label the buttons, and is the fallback for a page served
     without server-side templating: correct, but it flashes, so the fix is
     always to stamp it server-side rather than to lean on this. */
  apply(current());

  /* Exposed for pages that want their own control, e.g. a settings screen. */
  window.heyoTheme = { get: current, set: set, cycle: function () { set(next()); } };
})();
