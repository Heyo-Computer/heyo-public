# computer skill — examples

Runnable snippets for the `computer` Wayland CLI, oriented at a `printer
review` agent that needs visual or interactive evidence before issuing a
verdict. Each block is self-contained: copy, paste, run.

> **Heads up:** `computer` is a Wayland-only tool and synthesizes input via
> `uinput`. Confirm `$XDG_SESSION_TYPE=wayland`, `$WAYLAND_DISPLAY` is set,
> and the user is in the `input` group before relying on it.

---

## 1. Sanity check

```bash
computer --version
echo "session=$XDG_SESSION_TYPE display=$WAYLAND_DISPLAY runtime=$XDG_RUNTIME_DIR"
[ -w /dev/uinput ] || echo "uinput not writable — input synthesis will fail"
computer outputs --json
computer windows --json | head
```

If `outputs` errors, you are not connected to a Wayland compositor (e.g. plain
SSH without a forwarded session). If `windows --json` returns `[]`, the
compositor probably does not implement `ext-foreign-toplevel-list-v1`.

---

## 2. Screenshots as review evidence

### Capture the first (default) output

```bash
computer screenshot -o /tmp/screen.png
file /tmp/screen.png      # PNG image data, ...
```

`-o` is important — without it the PNG bytes go to stdout and pollute the
agent transcript.

### Capture a specific monitor

```bash
NAME=$(computer outputs --json | jq -r '.[0].name')
computer screenshot --output "$NAME" -o /tmp/primary.png
```

### Before/after pair around a synthesized action

```bash
computer screenshot -o /tmp/before.png
computer key chord 'ctrl+shift+t'
computer sleep 300
computer screenshot -o /tmp/after.png
```

You can attach both files in the review report or describe the visual delta in
the `## Per-item findings` section.

---

## 3. Window enumeration

### List all toplevels

```bash
computer windows --json | jq '.[] | {title, app_id}'
```

### Check for a window by title (regex via jq)

```bash
computer windows --json \
  | jq -e '.[] | select(.title | test("Settings"; "i"))' >/dev/null \
  && echo "Settings window present" \
  || echo "Settings window NOT present"
```

`jq -e` exits non-zero on no match — handy for shell-style assertions inside
a review.

### Group windows by app id

```bash
computer windows --json | jq 'group_by(.app_id) | map({app: .[0].app_id, count: length})'
```

---

## 4. Output / monitor inspection

```bash
computer outputs --json | jq '.[] | {name, make, model, width, height, refresh}'
```

If the human asked "does the new layout adapt to a 4K monitor?", confirm
geometry here and then take a screenshot of that specific output.

---

## 5. Keyboard input

### Tap a single key

```bash
computer key tap Return
computer key tap Escape
computer key tap F5
```

### Send a chord

```bash
computer key chord 'ctrl+s'
computer key chord 'ctrl+shift+t'
computer key chord 'alt+Tab'
```

### Hold a modifier across multiple actions

```bash
computer key down shift
computer key tap Right
computer key tap Right
computer key tap Right         # extends selection three chars right
computer key up shift
```

### Type a literal string into the focused field

```bash
computer type --delay-ms 20 'hello, reviewer'
```

Bump `--delay-ms` to 40+ for slow Electron / web apps that drop fast input.
Single-quote anything with shell metacharacters:

```bash
computer type --delay-ms 20 'cost: $5 (literal)'
```

---

## 6. Mouse input

### Move + click (per-output coordinates)

```bash
NAME=$(computer outputs --json | jq -r '.[0].name')
computer mouse move 960 540 --output "$NAME"
computer mouse click                          # left button (default)
computer mouse click --button right
computer mouse click --button left --count 2  # double-click
```

Forgetting `--output` on a multi-monitor box silently warps to monitor 0 — be
explicit.

### Relative move

```bash
computer mouse move-rel -50 100   # left 50, down 100
```

### Scroll

```bash
computer mouse scroll 0 5         # 5 ticks down
computer mouse scroll 0 -3        # 3 ticks up
computer mouse scroll 5 0         # 5 ticks right (horizontal)
```

### Drag

```bash
NAME=$(computer outputs --json | jq -r '.[0].name')
computer mouse move 100 100 --output "$NAME"
computer mouse down --button left
computer mouse move 400 400 --output "$NAME"
computer mouse up   --button left
```

---

## 7. Composed verification flows

### "Does the keyboard shortcut open the right window?"

```bash
before=$(computer windows --json)
computer key chord 'super+e'        # whatever the shortcut is
computer sleep 500
after=$(computer windows --json)
diff <(echo "$before" | jq -S .) <(echo "$after" | jq -S .) || echo "windowset changed"
```

### "Is the new toolbar button visible at this resolution?"

```bash
NAME=$(computer outputs --json | jq -r '.[] | select(.width==1920) | .name' | head -1)
[ -n "$NAME" ] || { echo "no 1920-wide output"; exit 1; }
computer screenshot --output "$NAME" -o /tmp/toolbar.png
# then describe /tmp/toolbar.png in the review report
```

### "Did pressing Escape dismiss the dialog?"

```bash
computer windows --json | jq -e '.[] | select(.title | test("Confirm"))' >/dev/null \
  || { echo "dialog already gone"; exit 0; }
computer key tap Escape
computer sleep 250
computer windows --json | jq -e '.[] | select(.title | test("Confirm"))' >/dev/null \
  && echo "FAIL: dialog still present" \
  || echo "PASS: dialog dismissed"
```

### Wait for a specific window to appear before acting

```bash
for _ in $(seq 1 30); do
  computer windows --json | jq -e '.[] | select(.title | test("Login"))' >/dev/null && break
  computer sleep 500
done
computer key tap Tab           # whatever the next step is
```

---

## 8. Common pitfalls

- **`screenshot` with no `-o`** dumps PNG bytes to stdout; agents will choke.
  Always pass `-o /tmp/foo.png`.
- **`mouse move` without `--output`** uses output index 0, not the focused
  monitor.
- **No `--sync` after focus changes.** Insert `computer sleep 200` (or longer
  for Electron) before sending keys after a focus-changing action.
- **Held-modifier contamination.** Defensively `key up shift`/`ctrl`/`alt`
  before sensitive sequences if a previous chord may have left state behind.
- **`windows` returns `[]`.** The compositor doesn't expose
  `ext-foreign-toplevel-list-v1`. This is a compositor capability issue, not a
  bug — escalate as a blocker rather than retrying.
- **`/dev/uinput` permission denied.** The user is not in the `input` group.
  Surface as a blocker; do not try to chmod or sudo around it.
- **Treating exit code as proof of effect.** A clean exit means the syscall
  to inject the input succeeded — not that the target app reacted. Always
  follow up with a screenshot or `windows` query when the verdict depends on
  the app's response.
- **Confusing X11 keysyms with xkb names.** They mostly overlap (`Return`,
  `Escape`, `Tab`, `F1`–`F12`), but if a key doesn't work, check
  `xkbcommon-keysyms.h` rather than the X11 header.

---

## 9. Reference

```bash
computer help               # list all subcommands
computer <subcommand> --help
```

Source of truth: `computer/src/main.rs` and `computer/src/keymap.rs` in this
repo. The `--json` outputs of `outputs` and `windows` are stable enough to
script against with `jq`.
