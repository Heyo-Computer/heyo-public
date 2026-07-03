---
name: computer
description: This skill should be used when a review-mode agent needs to verify desktop/UI behavior on a Wayland session — list monitors and toplevel windows, capture screenshots, or synthesize keyboard/mouse/scroll input via uinput. Reach for this skill any time the user asks to "use computer", "screenshot the desktop", "list windows on Wayland", "click at coordinates", "type into the focused app", "send a key chord", or to confirm by visual evidence that a UI change actually landed. Native Wayland equivalent of the xdotool skill — use this instead of xdotool when `$XDG_SESSION_TYPE` is `wayland`.
version: 0.1.0
---

# computer

`computer` is a small Wayland desktop-automation CLI (Rust, in this repo at
`computer/`). It exposes exactly what a headless reviewer needs to confirm
visual outcomes on a Wayland session: enumerate outputs and toplevel windows,
grab per-output PNG screenshots, and synthesize keyboard / mouse / scroll
events through `uinput`. There are no window-manipulation verbs — the
foreign-toplevel protocol is read-only.

## When to use this skill

You are most likely in a `printer review` turn and need to **verify** a UI
change rather than make one. Reach for `computer` when the verdict depends on
something only the screen can answer:

- "Did the new dialog actually render?" → `screenshot` the relevant output.
- "Is the right window in the foreground?" → `windows --json` and inspect titles.
- "Does the keyboard shortcut still work?" → `key chord ctrl+shift+t`.
- "Does typing into the field do the right thing?" → `type 'hello'`.
- "Is multi-monitor layout sane?" → `outputs --json`.

It is also the right tool any time `xdotool` would be reached for but the
session is Wayland (`echo $XDG_SESSION_TYPE` → `wayland`) — most modern GNOME
and KDE installs. xdotool only sees XWayland clients there; `computer` sees
all native Wayland windows and drives input below the compositor.

## Do not modify state with this skill during review

Review-mode work is **read-only by contract** (see the review prompt — "Do
not modify any files."). The same spirit applies to the desktop:

- Prefer `outputs`, `windows --json`, and `screenshot` — they observe.
- Use `mouse`, `key`, `type`, and `scroll` only when *observation alone*
  cannot answer the verification question (e.g. you must open a menu to
  confirm a label, or focus a field to confirm validation behavior).
- After any input synthesis that warps the pointer or types, leave the
  desktop in a sane state (close the menu, scroll back). Treat synthesized
  input the way you treat editing a file you read for context: minimize the
  footprint, and never do destructive actions (closing windows, dismissing
  unsaved-changes dialogs, clicking arbitrary buttons in unknown apps).

## Critical: environment requirements

`computer` talks to two subsystems and will fail loudly if either is missing.

1. **Wayland compositor**, reached via `$WAYLAND_DISPLAY` (and `$XDG_RUNTIME_DIR`).
   - Listing toplevels requires `ext-foreign-toplevel-list-v1`. Most wlroots-based
     compositors (Sway, Hyprland, river, niri, COSMIC) implement this. GNOME
     Mutter and KWin support varies by version — if `windows` returns an empty
     list under those, the protocol is unavailable, not a bug.
   - Screenshots use `wlr-screencopy-unstable-v1`. Same compatibility caveat.
2. **`uinput`** for input synthesis. The user's account must be in the `input`
   group (or otherwise have write access to `/dev/uinput`). On a fresh box:
   ```bash
   ls -l /dev/uinput          # crw-rw---- root input
   id -nG | tr ' ' '\n' | grep -x input || echo "NOT IN input GROUP"
   ```
   If not in the group, `mouse`, `key`, and `type` will return permission errors.
   Do not attempt to fix this from inside a review run — surface it as a
   blocker in the report.

If `$WAYLAND_DISPLAY` is unset (e.g. running over SSH without a forwarded
display), every subcommand except `--help`/`sleep` will fail. Detect first:

```bash
[ -n "$WAYLAND_DISPLAY" ] && [ -n "$XDG_RUNTIME_DIR" ] || echo "no Wayland"
```

## Command surface

```
computer outputs     [--json]           # list monitors (wl_output)
computer windows     [--json]           # list toplevel windows (foreign-toplevel-list)
computer screenshot  [--output NAME] [-o FILE]   # capture an output to PNG (default: stdout)
computer mouse move      <X> <Y> [--output NAME]     # absolute, per-output coords
computer mouse move-rel  <DX> <DY>
computer mouse click     [--button B] [--count N]    # B in {left,right,middle,side,extra}
computer mouse down      [--button B]
computer mouse up        [--button B]
computer mouse scroll    <DX> <DY>                   # +y = scroll down
computer key tap         <KEY>            # e.g. Return, Escape, F5
computer key down        <KEY>
computer key up          <KEY>
computer key chord       "ctrl+shift+t"
computer type            [--delay-ms N] "literal text"
computer sleep           <MS>
```

Notes that bite if missed:

- **`mouse move` is per-output**, not global. Coordinates are pixels within
  the chosen `--output`. Without `--output`, the **first** output is used —
  which on a multi-monitor box is rarely the focused one. Always pass
  `--output` when more than one is connected.
- **`screenshot` defaults to stdout** as raw PNG. Always pass `-o file.png`
  unless you are intentionally piping to another tool — otherwise you flood
  the terminal / agent transcript with binary garbage.
- **No `--clearmodifiers`-equivalent.** If a chord lands wrong, sleep first
  to let any held modifier be released by the user, or send `key up <mod>`
  defensively. There are no held-modifier shortcuts during a headless review.
- **`type --delay-ms` defaults to 8.** Bump to 20–40 for slow Electron / web
  apps; otherwise characters drop.
- **`key tap`/`chord` keysyms** follow xkbcommon names (`Return`, `Escape`,
  `Tab`, `BackSpace`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`,
  `Page_Up`, `F1`–`F12`, `super`, `alt`, `ctrl`, `shift`). Same vocabulary
  as `xdotool key`.

## Composing reliable observations

1. **Always sanity-check first.** `outputs --json` and `windows --json`
   verify the compositor is reachable before you trust later commands.
2. **Screenshot before synthesizing input**, then again after, so you can
   diff state explicitly. Save with timestamped names: `before.png`,
   `after.png`.
3. **Wait for the WM after focusing.** There is no `--sync`; insert
   `computer sleep 200` (ms) after any action that changes focus before
   sending keys.
4. **Quote shell metacharacters** for `type`. Single-quote anything with
   `$`, backticks, or `!`: `computer type 'price=$5'`.
5. **Don't rely on window IDs being stable across runs.** `windows --json`
   gives a snapshot; re-query if you need to act on a window after a delay.
6. **Report what you saw, not what you sent.** A successful `key chord`
   exit code only means the keys were synthesized — it does not mean the
   target app received or acted on them. Confirm with a follow-up screenshot
   or window-state query.

## Quick decision table

| Goal | Command |
|---|---|
| Confirm a window exists by title | `computer windows --json \| jq -e '.[] \| select(.title \| test("Settings"))'` |
| Capture the primary monitor to a PNG | `computer screenshot --output "$(computer outputs --json \| jq -r '.[0].name')" -o /tmp/screen.png` |
| Press a global hotkey | `computer key chord 'ctrl+alt+t'` |
| Type a literal string | `computer type --delay-ms 20 'hello, reviewer'` |
| Click at a known location | `computer mouse move 800 400 --output DP-1 && computer mouse click` |
| Scroll a document down | `computer mouse scroll 0 5` |
| Verify multi-monitor geometry | `computer outputs --json` |

## Examples & reference

See `README.md` in this skill directory for runnable, copy-pasteable examples
organized by use case (sanity checks, screenshots for review evidence,
window enumeration, input synthesis, common pitfalls).

For exotic options run `computer help` or `computer <command> --help`. The
source of truth is the binary itself at `computer/src/main.rs` in this repo.
