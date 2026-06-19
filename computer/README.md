# Computer CLI

A CLI for providing mouse, keyboard, screenshot, and window/display enumeration
to agents. Supports Wayland-based Linux desktops and macOS.

## Example usage

### Discover the desktop

```sh
computer outputs                    # list connected monitors
computer windows                    # list visible toplevel windows
```

### Screenshots

```sh
computer screenshot -o /tmp/desk.png             # capture the first output
computer screenshot --output DP-1 -o ./hidpi.png # specific monitor (Linux)
computer screenshot --output display-5 -o ./mac.png # specific display (macOS)
computer screenshot > /tmp/desk.png              # PNG to stdout
```

### Mouse

```sh
computer mouse move 960 540                    # absolute position on the first output
computer mouse move 100 100 --output HDMI-A-1  # absolute on a named output
computer mouse move-rel 50 -20                 # relative dx,dy from current position
computer mouse click                           # left click (default)
computer mouse click --button right            # right click
computer mouse click --count 2                 # double-click
computer mouse down --button left              # press and hold (for drag)
computer mouse up   --button left              # release
computer mouse scroll 0 5                      # scroll down 5 ticks
computer mouse scroll -3 0                     # scroll left 3 ticks
```

### Keyboard

```sh
computer key tap Return                  # press + release
computer key tap Escape
computer key chord "ctrl+c"              # send a chord (Linux)
computer key chord "cmd+c"               # send a chord (macOS)
computer key chord "ctrl+shift+t"        # reopen last tab
computer key down ctrl                   # hold a modifier
computer key up   ctrl                   # release it
```

### Typing text

```sh
computer type "hello, world"             # literal string with default 8ms inter-key delay
computer type --delay-ms 30 "slower for picky inputs"
```

On macOS `type` posts the text via `CGEventKeyboardSetUnicodeString`, so any
Unicode is supported regardless of the active keyboard layout. On Linux the
synthesizer uses a US-layout keymap.

### Pacing automations

```sh
computer key chord "super+t"             # open a terminal (Linux)
computer key chord "cmd+space"           # Spotlight (macOS)
computer sleep 250                       # let it settle
computer type "echo hi"
computer key tap Return
```

### Drag-and-drop pattern

```sh
computer mouse move 200 300
computer mouse down --button left
computer mouse move 600 300              # drag
computer mouse up   --button left
```

## macOS quick start

1. Build: `cargo build --release` (or `make install-computer` from the repo
   root).
2. Grant **Accessibility** to the binary so input synthesis is delivered:
   System Settings → Privacy & Security → Accessibility → add the path to
   `target/release/computer` (or `~/.local/bin/computer`) and toggle it on.
3. Grant **Screen Recording** so `screenshot` and the title fields of `windows`
   work: System Settings → Privacy & Security → Screen Recording.
4. After every rebuild the binary's signing identity changes, which makes
   macOS re-prompt for both permissions. To keep a stable identity during
   development, sign the binary ad-hoc once:

   ```sh
   codesign --force --sign - target/release/computer
   ```

   Subsequent rebuilds at the same path will be recognized and the toggle
   stays granted.

## Behavioral differences between platforms

- `mouse move x y` uses **points** on macOS (Retina-aware global coordinate
  space) and **pixels** on Linux. On a Retina display 1 point ≈ 2 pixels.
- `--output NAME` is a `wl_output` name like `HDMI-A-1` on Linux, and
  `display-<CGDirectDisplayID>` (e.g. `display-5`) on macOS — see the output
  of `computer outputs` on the host.
- `windows` returns the same JSON shape on both platforms, but on macOS the
  `title` field is empty until Screen Recording is granted. A warning is
  printed to stderr in that case.
- macOS "natural scrolling" inverts perceived scroll direction at the OS
  layer; `mouse scroll 0 5` always emits "5 ticks toward the bottom of the
  page" but the OS may flip it before reaching the application.
- The `screenshot` path on macOS uses `CGDisplayCreateImage`, which Apple
  deprecated in macOS 14. It still functions on current macOS; a future
  migration to ScreenCaptureKit is planned.

### Serve as an MCP server

```sh
computer mcp
```

Speaks newline-delimited JSON-RPC 2.0 over stdin/stdout (the MCP stdio
transport) and exposes the desktop commands as native tools: `screenshot`,
`outputs`, `windows`, `mouse_move`, `mouse_click`, `mouse_scroll`,
`mouse_drag`, `key`, `type`, `browse`. The headline difference from shelling
out: **`screenshot` returns an inline PNG image content block**, so an agent
host sees the pixels in the same turn it called the tool (no temp file + read).
By default the screenshot's long edge is downscaled to 1568px to bound the
payload; pass `max_width` to override.

Intended to be wired into an agent host, not run by hand — e.g. for Claude:

```sh
claude --mcp-config '{"mcpServers":{"computer":{"type":"stdio","command":"computer","args":["mcp"]}}}' \
       --strict-mcp-config \
       --allowedTools mcp__computer__screenshot,mcp__computer__mouse_click,mcp__computer__type
```

The Printer CLI wires this automatically for the Claude backend when the
`computer` binary is installed **and** a real display is present **and** the run
is not sandboxed (the heyvm microVM is headless), using the absolute binary
path. Input tools return a short text ack; `outputs`/`windows` return JSON text.

## Notes

- On Linux this targets the active Wayland session. For X11 automation,
  additional tools may be required as Wayland does not expose X11-level access.
- On macOS coordinates for `mouse move` are absolute points on the chosen
  display in the global coordinate space; `--output` is optional.
- `screenshot` defaults to the first output if `--output` is omitted.
- `type` synthesises one keystroke per character; for long strings consider
  raising `--delay-ms` if the receiving app drops events.
