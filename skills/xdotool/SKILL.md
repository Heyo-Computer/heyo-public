---
name: xdotool
description: This skill should be used when the user asks to "use xdotool", "automate the desktop", "simulate a keypress", "send keystrokes", "move the mouse", "click on screen", "find a window", "activate a window", "resize a window", "move a window", "type text into an app", or any X11 GUI automation on Linux. Provides patterns for keyboard, mouse, and window control via the xdotool CLI.
version: 0.1.0
---

# xdotool

Drive X11 GUIs from the shell: synthesize key/mouse input, locate and manipulate windows, and read window/pointer state. Useful for desktop automation, scripted demos, repetitive UI flows, and accessibility helpers.

## When to Use

Reach for xdotool when the user wants to:

- Send keystrokes or text to a focused or specific window (`type`, `key`).
- Move/click the pointer at absolute or relative coordinates (`mousemove`, `mousemove_relative`, `click`).
- Find a window by name/class/PID and act on it (`search`, `windowactivate`, `windowfocus`).
- Move, resize, minimize, raise, or close windows (`windowmove`, `windowsize`, `windowminimize`, `windowraise`, `windowclose`).
- Read current pointer/window/desktop state (`getmouselocation`, `getactivewindow`, `getwindowname`, `getwindowgeometry`, `get_desktop`).
- Switch virtual desktops or move windows across desktops.

## Critical: Display Server Compatibility

xdotool **only drives X11**. Behavior depends on the session:

- **X11 session** (`echo $XDG_SESSION_TYPE` → `x11`): full functionality.
- **Wayland session** (`$XDG_SESSION_TYPE` → `wayland`): xdotool can only see and control **XWayland** clients (legacy X11 apps running under Wayland). Native Wayland apps (most GNOME/KDE apps on modern distros, Firefox/Chromium when built for Wayland) are invisible to xdotool — `search` won't find them, `type`/`key` won't reach them, and global pointer warps may misbehave.
- Detect the session before scripting: `echo $XDG_SESSION_TYPE`. If wayland, warn the user and suggest alternatives like `ydotool`, `wtype`, or `dotool` for native Wayland.
- When xdotool **must** target a real X11 display, set `DISPLAY` explicitly: `DISPLAY=:0 xdotool ...`.

## Core Command Patterns

### Keyboard input

- Single key or chord: `xdotool key Return`, `xdotool key ctrl+s`, `xdotool key ctrl+shift+t`.
- Stream of characters as if typed: `xdotool type --delay 20 'hello world'`. The `--delay` (ms between keys) avoids dropped chars in slow apps.
- Hold/release a key: `xdotool keydown shift` … `xdotool keyup shift`.
- Key names follow X11 keysyms: `Return`, `Tab`, `Escape`, `BackSpace`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `Page_Up`, `F1`–`F12`, `super`, `alt`, `ctrl`, `shift`. Look up unusual ones with `xev` or `/usr/include/X11/keysymdef.h`.

### Mouse input

- Absolute move: `xdotool mousemove X Y` (origin is top-left of the screen).
- Relative move: `xdotool mousemove_relative -- -50 100` (use `--` so negatives aren't parsed as flags).
- Click: `xdotool click 1` (1=left, 2=middle, 3=right, 4=scroll up, 5=scroll down). Repeat: `xdotool click --repeat 2 --delay 100 1` for a double-click.
- Read current position: `xdotool getmouselocation` → `x:… y:… screen:… window:…`. Add `--shell` to get eval-able output.

### Window targeting

- Active window: `xdotool getactivewindow`.
- Search by visible title (regex): `xdotool search --name 'Mozilla Firefox'`.
- Search by WM_CLASS: `xdotool search --class 'firefox'` or `--classname`.
- Search by PID: `xdotool search --pid 12345`.
- Restrict to mapped/visible windows and pick the first: `xdotool search --onlyvisible --limit 1 --name 'Term'`.
- Most window verbs accept either a numeric window id or `%@` (last result of the previous `search`), so chain like:

```bash
xdotool search --onlyvisible --name 'Slack' windowactivate --sync
```

The trailing verb after a `search` reuses the matched window(s). `--sync` blocks until the WM has actually applied the change — almost always what you want before sending input.

### Window manipulation

- Activate (give focus + raise): `xdotool windowactivate --sync $WID`.
- Focus only (no raise): `xdotool windowfocus $WID`.
- Move: `xdotool windowmove $WID 100 200` (use `x y`; `current` keeps that axis).
- Resize: `xdotool windowsize $WID 1280 720` (append `%` for percentage of display).
- Minimize / raise / close / kill: `windowminimize`, `windowraise`, `windowclose` (graceful), `windowkill` (force).

### Desktops

- Count / current: `xdotool get_num_desktops`, `xdotool get_desktop`.
- Switch: `xdotool set_desktop 2`.
- Move window to desktop: `xdotool set_desktop_for_window $WID 1`.

## Composing Reliable Automations

1. **Always pause for the WM** when doing focus-then-input. After `windowactivate`, either pass `--sync` or insert `xdotool sleep 0.2` before sending keys, otherwise the first keystrokes can land in the previously focused window.
2. **Quote shell metacharacters** passed to `type`. Single-quote strings containing `$`, backticks, or `!` to avoid shell expansion: `xdotool type 'price=$5'`.
3. **Use `--delay`** with `type` for any nontrivial string (10–30 ms) — terminals and Electron apps frequently drop characters at full speed.
4. **Prefer `search ... <verb>` chaining** over capturing a window id into a variable. It's atomic and avoids races where the window vanishes between commands.
5. **Restore the user's pointer** if a script warps the mouse: capture `xdotool getmouselocation --shell`, do work, then `mousemove` back.
6. **Don't combine xdotool with `sudo`** for input injection — it talks to the user's X server via `$DISPLAY` and `$XAUTHORITY`. If a root context is unavoidable, pass both env vars through explicitly.
7. **Test interactively first.** When writing a new script, run each xdotool command standalone in a terminal and watch the result before stitching them together.

## Quick Decision Table

| Goal | Command |
|---|---|
| Type text into focused window | `xdotool type --delay 20 'text'` |
| Press a hotkey | `xdotool key ctrl+shift+t` |
| Click at coordinates | `xdotool mousemove 500 400 click 1` |
| Find + focus an app | `xdotool search --onlyvisible --name 'Pat' windowactivate --sync` |
| Read active window title | `xdotool getactivewindow getwindowname` |
| Move window to (0,0), 800x600 | `xdotool getactivewindow windowmove 0 0 windowsize 800 600` |
| Switch to desktop 2 | `xdotool set_desktop 2` |

## Examples & Reference

See `README.md` in this skill directory for runnable, copy-pasteable examples organized by use case (keystrokes, mouse, window discovery, multi-window workflows, common pitfalls).

For exotic options and the full command list, run `xdotool help` or `xdotool <command> --help`.
