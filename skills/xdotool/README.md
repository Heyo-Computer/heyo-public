# xdotool skill — examples

Runnable snippets for the most common xdotool workflows. Each block is self-contained: copy, paste, run.

> **Heads up:** xdotool only controls X11 clients. On Wayland sessions it can still drive XWayland apps but won't see native Wayland windows. Check with `echo $XDG_SESSION_TYPE`.

---

## 1. Sanity check

```bash
xdotool --version
echo "session=$XDG_SESSION_TYPE display=$DISPLAY"
xdotool getmouselocation
xdotool getactivewindow getwindowname
```

If `getactivewindow` returns nothing useful on Wayland, the focused app is a native Wayland client and xdotool can't touch it.

---

## 2. Keyboard

### Type a string into the focused field

```bash
xdotool type --delay 20 'hello, world'
```

`--delay` is in milliseconds between keystrokes. Bump to 40–60 ms for slow Electron apps.

### Press a single key

```bash
xdotool key Return
xdotool key Escape
xdotool key F5
```

### Hotkey / chord

```bash
xdotool key ctrl+s            # save
xdotool key ctrl+shift+t      # reopen closed tab
xdotool key alt+Tab           # window switch
xdotool key super+l           # lock screen (depends on WM)
```

### Hold a modifier across multiple actions

```bash
xdotool keydown shift
xdotool key Right Right Right    # extend selection 3 chars right
xdotool keyup shift
```

### Type text with shell variables (quote carefully)

```bash
name="Sam"
xdotool type --delay 15 "Hi $name"      # double quotes -> $name expands
xdotool type --delay 15 'literal $1.99' # single quotes -> no expansion
```

---

## 3. Mouse

### Move and click

```bash
xdotool mousemove 960 540        # absolute (center of 1080p)
xdotool click 1                  # left click
xdotool mousemove 200 300 click 3   # move + right-click in one go
```

### Relative movement (note the `--`)

```bash
xdotool mousemove_relative -- -50 100   # left 50, down 100
```

### Double-click / scroll

```bash
xdotool click --repeat 2 --delay 80 1   # double-click
xdotool click 4                          # scroll up
xdotool click --repeat 5 5              # scroll down 5 ticks
```

### Drag (button-down, move, button-up)

```bash
xdotool mousemove 100 100 mousedown 1 mousemove 400 400 mouseup 1
```

### Save and restore pointer position

```bash
eval "$(xdotool getmouselocation --shell)"   # exports X, Y, SCREEN, WINDOW
# ... do work that warps the mouse ...
xdotool mousemove "$X" "$Y"
```

---

## 4. Finding windows

```bash
xdotool search --name 'Firefox'                       # by visible title (regex)
xdotool search --class firefox                        # by WM_CLASS class
xdotool search --classname Navigator                  # by WM_CLASS instance
xdotool search --pid 12345                            # by PID
xdotool search --onlyvisible --limit 1 --name Slack   # first visible match
```

Inspect a window:

```bash
WID=$(xdotool search --onlyvisible --limit 1 --name Slack)
xdotool getwindowname "$WID"
xdotool getwindowgeometry "$WID"
xdotool getwindowpid "$WID"
```

---

## 5. Window manipulation

### Activate (focus + raise) — preferred for "bring to front"

```bash
xdotool search --onlyvisible --name 'Mozilla Firefox' windowactivate --sync
```

`--sync` blocks until the window manager actually finishes the action — important before sending input.

### Move + resize the active window

```bash
xdotool getactivewindow windowmove 0 0 windowsize 1280 800
```

### Tile to right half of a 1920x1080 display

```bash
xdotool getactivewindow windowmove 960 0 windowsize 960 1080
```

### Resize using percentages

```bash
xdotool getactivewindow windowsize 50% 100%
```

### Minimize / raise / close / kill

```bash
xdotool search --name 'Spotify' windowminimize
xdotool search --name 'Terminal' windowraise
xdotool search --name 'Calculator' windowclose    # graceful
xdotool search --name 'Frozen App' windowkill     # force
```

---

## 6. Desktops (workspaces)

```bash
xdotool get_num_desktops
xdotool get_desktop                              # current
xdotool set_desktop 2                            # switch to desktop 2
WID=$(xdotool getactivewindow)
xdotool set_desktop_for_window "$WID" 1          # send active window to desktop 1
```

---

## 7. Multi-step workflows

### Open a new tab in Firefox and search

```bash
xdotool search --onlyvisible --name 'Mozilla Firefox' windowactivate --sync
xdotool key ctrl+t
xdotool sleep 0.3
xdotool type --delay 20 'site:news.ycombinator.com xdotool'
xdotool key Return
```

### Paste clipboard into a specific window

```bash
xdotool search --onlyvisible --name 'gedit' windowactivate --sync
xdotool key --clearmodifiers ctrl+v
```

`--clearmodifiers` releases any modifier keys the user is currently holding so the chord lands cleanly.

### Capture a region by clicking opposite corners

```bash
echo "Click top-left corner..."
read -r _; eval "$(xdotool getmouselocation --shell)"; X1=$X; Y1=$Y
echo "Click bottom-right corner..."
read -r _; eval "$(xdotool getmouselocation --shell)"; X2=$X; Y2=$Y
W=$((X2 - X1)); H=$((Y2 - Y1))
import -window root -crop "${W}x${H}+${X1}+${Y1}" out.png
```

### Wait for a specific window to appear, then act

```bash
until xdotool search --onlyvisible --name 'Login' >/dev/null 2>&1; do
  sleep 0.5
done
xdotool search --name 'Login' windowactivate --sync
xdotool type --delay 20 "$USERNAME"
xdotool key Tab
xdotool type --delay 20 "$PASSWORD"
xdotool key Return
```

---

## 8. Reading state for scripts

```bash
# Active window id, name, geometry in one shot
WID=$(xdotool getactivewindow)
xdotool getwindowname "$WID"
xdotool getwindowgeometry --shell "$WID"   # exports WINDOW, X, Y, WIDTH, HEIGHT, SCREEN
```

```bash
# Current pointer
eval "$(xdotool getmouselocation --shell)"
echo "pointer at $X,$Y over window $WINDOW"
```

```bash
# Display size
xdotool getdisplaygeometry      # "1920 1080"
```

---

## 9. Common pitfalls

- **First keystroke goes to the wrong window.** Use `windowactivate --sync` (or `xdotool sleep 0.2`) before `type`/`key`.
- **Negative coordinates parsed as flags.** Use `--`: `xdotool mousemove_relative -- -10 0`.
- **`type` drops characters.** Add `--delay 20` (or higher) for Electron/Java/slow apps.
- **Shell-expanded `$` or `!` in typed text.** Single-quote the argument: `xdotool type 'cost: $5!'`.
- **Held modifiers from the user contaminate chords.** Use `xdotool key --clearmodifiers ctrl+v`.
- **Wayland session.** Native Wayland apps are invisible to xdotool. Use `ydotool`, `wtype`, or `dotool` instead, or run the target app with `GDK_BACKEND=x11` / `QT_QPA_PLATFORM=xcb` if it supports it.
- **Running under sudo.** xdotool needs the user's `$DISPLAY` and `$XAUTHORITY`; without them it fails silently or hits the wrong server. Prefer running as the desktop user.
- **`search` without `--onlyvisible`.** Returns hidden/utility windows too — usually not what scripts want.

---

## 10. Reference

```bash
xdotool help                  # list all commands
xdotool <command> --help      # per-command flags
man xdotool                   # full manual
```

Useful keysym sources for the `key` verb: `xev` (live key inspector), `/usr/include/X11/keysymdef.h`, or `xmodmap -pk`.
