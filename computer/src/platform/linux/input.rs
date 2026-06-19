use super::keymap::{char_to_key, parse_key};
use super::outputs;
use crate::platform::keymap_common::split_chord;
use crate::platform::types::{Button, KeyAction, MouseAction};
use anyhow::{Context, Result, bail};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode,
    UinputAbsSetup, uinput::VirtualDevice,
};
use std::thread::sleep;
use std::time::Duration;

const ABS_MAX: i32 = 65535;

/// Build a single virtual device with keyboard + mouse capabilities.
/// Combining them keeps focus on whichever surface receives the synthesized
/// pointer event, which is what tests usually want.
fn build_device() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    // Cover the full A–Z, 0–9, modifiers, F-keys, navigation, and mouse buttons we use.
    let key_codes = [
        KeyCode::KEY_A, KeyCode::KEY_B, KeyCode::KEY_C, KeyCode::KEY_D, KeyCode::KEY_E,
        KeyCode::KEY_F, KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_I, KeyCode::KEY_J,
        KeyCode::KEY_K, KeyCode::KEY_L, KeyCode::KEY_M, KeyCode::KEY_N, KeyCode::KEY_O,
        KeyCode::KEY_P, KeyCode::KEY_Q, KeyCode::KEY_R, KeyCode::KEY_S, KeyCode::KEY_T,
        KeyCode::KEY_U, KeyCode::KEY_V, KeyCode::KEY_W, KeyCode::KEY_X, KeyCode::KEY_Y,
        KeyCode::KEY_Z,
        KeyCode::KEY_0, KeyCode::KEY_1, KeyCode::KEY_2, KeyCode::KEY_3, KeyCode::KEY_4,
        KeyCode::KEY_5, KeyCode::KEY_6, KeyCode::KEY_7, KeyCode::KEY_8, KeyCode::KEY_9,
        KeyCode::KEY_SPACE, KeyCode::KEY_ENTER, KeyCode::KEY_TAB, KeyCode::KEY_ESC,
        KeyCode::KEY_BACKSPACE, KeyCode::KEY_DELETE, KeyCode::KEY_INSERT,
        KeyCode::KEY_HOME, KeyCode::KEY_END, KeyCode::KEY_PAGEUP, KeyCode::KEY_PAGEDOWN,
        KeyCode::KEY_UP, KeyCode::KEY_DOWN, KeyCode::KEY_LEFT, KeyCode::KEY_RIGHT,
        KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL,
        KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT,
        KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT,
        KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA,
        KeyCode::KEY_CAPSLOCK,
        KeyCode::KEY_F1, KeyCode::KEY_F2, KeyCode::KEY_F3, KeyCode::KEY_F4,
        KeyCode::KEY_F5, KeyCode::KEY_F6, KeyCode::KEY_F7, KeyCode::KEY_F8,
        KeyCode::KEY_F9, KeyCode::KEY_F10, KeyCode::KEY_F11, KeyCode::KEY_F12,
        KeyCode::KEY_MINUS, KeyCode::KEY_EQUAL, KeyCode::KEY_LEFTBRACE, KeyCode::KEY_RIGHTBRACE,
        KeyCode::KEY_SEMICOLON, KeyCode::KEY_APOSTROPHE, KeyCode::KEY_GRAVE,
        KeyCode::KEY_BACKSLASH, KeyCode::KEY_COMMA, KeyCode::KEY_DOT, KeyCode::KEY_SLASH,
        KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE,
        KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA,
    ];
    for k in key_codes {
        keys.insert(k);
    }

    let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
    rel_axes.insert(RelativeAxisCode::REL_X);
    rel_axes.insert(RelativeAxisCode::REL_Y);
    rel_axes.insert(RelativeAxisCode::REL_WHEEL);
    rel_axes.insert(RelativeAxisCode::REL_HWHEEL);
    rel_axes.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rel_axes.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);

    let abs_info = AbsInfo::new(0, 0, ABS_MAX, 0, 0, 1);
    let abs_x = UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, abs_info);
    let abs_y = UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, abs_info);

    let dev = VirtualDevice::builder()
        .context("open /dev/uinput (need r/w access)")?
        .name(b"computer-cli")
        .with_keys(&keys)?
        .with_relative_axes(&rel_axes)?
        .with_absolute_axis(&abs_x)?
        .with_absolute_axis(&abs_y)?
        .build()
        .context("build uinput device")?;

    // Compositors usually need a moment to discover the new device.
    sleep(Duration::from_millis(60));
    Ok(dev)
}

fn syn() -> InputEvent {
    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0)
}

fn key_event(code: KeyCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, value)
}

fn rel_event(axis: RelativeAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::RELATIVE.0, axis.0, value)
}

fn abs_event(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
}

fn button_code(b: Button) -> KeyCode {
    match b {
        Button::Left => KeyCode::BTN_LEFT,
        Button::Right => KeyCode::BTN_RIGHT,
        Button::Middle => KeyCode::BTN_MIDDLE,
        Button::Side => KeyCode::BTN_SIDE,
        Button::Extra => KeyCode::BTN_EXTRA,
    }
}

pub fn mouse(action: MouseAction) -> Result<()> {
    let mut dev = build_device()?;
    match action {
        MouseAction::Move { x, y, output } => {
            let (sx, sy) = pixel_to_abs(x, y, output.as_deref())?;
            dev.emit(&[
                abs_event(AbsoluteAxisCode::ABS_X, sx),
                abs_event(AbsoluteAxisCode::ABS_Y, sy),
                syn(),
            ])?;
        }
        MouseAction::MoveRel { dx, dy } => {
            dev.emit(&[
                rel_event(RelativeAxisCode::REL_X, dx),
                rel_event(RelativeAxisCode::REL_Y, dy),
                syn(),
            ])?;
        }
        MouseAction::Click { button, count } => {
            let code = button_code(button);
            for i in 0..count {
                if i > 0 {
                    sleep(Duration::from_millis(40));
                }
                dev.emit(&[key_event(code, 1), syn()])?;
                sleep(Duration::from_millis(20));
                dev.emit(&[key_event(code, 0), syn()])?;
            }
        }
        MouseAction::Down { button } => {
            dev.emit(&[key_event(button_code(button), 1), syn()])?;
        }
        MouseAction::Up { button } => {
            dev.emit(&[key_event(button_code(button), 0), syn()])?;
        }
        MouseAction::Scroll { dx, dy } => {
            // Positive dy scrolls down per CLI convention; REL_WHEEL is negative for down.
            let wheel = -dy;
            let hwheel = dx;
            let mut events = Vec::new();
            if wheel != 0 {
                events.push(rel_event(RelativeAxisCode::REL_WHEEL, wheel));
                events.push(rel_event(RelativeAxisCode::REL_WHEEL_HI_RES, wheel * 120));
            }
            if hwheel != 0 {
                events.push(rel_event(RelativeAxisCode::REL_HWHEEL, hwheel));
                events.push(rel_event(RelativeAxisCode::REL_HWHEEL_HI_RES, hwheel * 120));
            }
            if events.is_empty() {
                return Ok(());
            }
            events.push(syn());
            dev.emit(&events)?;
        }
    }
    // Tiny delay so the event reaches the compositor before we drop the device.
    sleep(Duration::from_millis(20));
    Ok(())
}

/// Click-and-drag `button` from `from` to `to`, all on a SINGLE device so the
/// press survives the move. Separate `mouse down`/`up` calls can't hold a
/// button because `build_device` tears the device down on return; a drag must
/// press, move, and release within one device's lifetime. Coordinates are
/// pixels on the chosen output, same as `mouse move`.
pub fn drag(from: (i32, i32), to: (i32, i32), button: Button, output: Option<&str>) -> Result<()> {
    let mut dev = build_device()?;
    let code = button_code(button);
    let (fx, fy) = pixel_to_abs(from.0, from.1, output)?;
    let (tx, ty) = pixel_to_abs(to.0, to.1, output)?;
    dev.emit(&[
        abs_event(AbsoluteAxisCode::ABS_X, fx),
        abs_event(AbsoluteAxisCode::ABS_Y, fy),
        syn(),
    ])?;
    sleep(Duration::from_millis(20));
    dev.emit(&[key_event(code, 1), syn()])?;
    sleep(Duration::from_millis(20));
    dev.emit(&[
        abs_event(AbsoluteAxisCode::ABS_X, tx),
        abs_event(AbsoluteAxisCode::ABS_Y, ty),
        syn(),
    ])?;
    sleep(Duration::from_millis(20));
    dev.emit(&[key_event(code, 0), syn()])?;
    sleep(Duration::from_millis(20));
    Ok(())
}

pub fn key(action: KeyAction) -> Result<()> {
    let mut dev = build_device()?;
    match action {
        KeyAction::Tap { key } => {
            let code = parse_key(&key)?;
            dev.emit(&[key_event(code, 1), syn()])?;
            sleep(Duration::from_millis(15));
            dev.emit(&[key_event(code, 0), syn()])?;
        }
        KeyAction::Down { key } => {
            let code = parse_key(&key)?;
            dev.emit(&[key_event(code, 1), syn()])?;
        }
        KeyAction::Up { key } => {
            let code = parse_key(&key)?;
            dev.emit(&[key_event(code, 0), syn()])?;
        }
        KeyAction::Chord { combo } => {
            let parts: Vec<KeyCode> = split_chord(&combo)
                .into_iter()
                .map(parse_key)
                .collect::<Result<_>>()?;
            if parts.is_empty() {
                bail!("empty chord");
            }
            // Press all in order.
            for c in &parts {
                dev.emit(&[key_event(*c, 1), syn()])?;
                sleep(Duration::from_millis(8));
            }
            sleep(Duration::from_millis(15));
            // Release in reverse.
            for c in parts.iter().rev() {
                dev.emit(&[key_event(*c, 0), syn()])?;
                sleep(Duration::from_millis(8));
            }
        }
    }
    sleep(Duration::from_millis(20));
    Ok(())
}

pub fn type_text(text: &str, delay_ms: u64) -> Result<()> {
    let mut dev = build_device()?;
    let mut shift_held = false;
    for ch in text.chars() {
        let Some((code, needs_shift)) = char_to_key(ch) else {
            bail!("character {ch:?} not in US-layout keymap; pass a chord via `key chord` instead");
        };
        if needs_shift && !shift_held {
            dev.emit(&[key_event(KeyCode::KEY_LEFTSHIFT, 1), syn()])?;
            shift_held = true;
        } else if !needs_shift && shift_held {
            dev.emit(&[key_event(KeyCode::KEY_LEFTSHIFT, 0), syn()])?;
            shift_held = false;
        }
        dev.emit(&[key_event(code, 1), syn()])?;
        sleep(Duration::from_millis(delay_ms.max(1)));
        dev.emit(&[key_event(code, 0), syn()])?;
        sleep(Duration::from_millis(delay_ms.max(1)));
    }
    if shift_held {
        dev.emit(&[key_event(KeyCode::KEY_LEFTSHIFT, 0), syn()])?;
    }
    sleep(Duration::from_millis(20));
    Ok(())
}

/// Convert pixel coordinates on a chosen wl_output to the device-wide ABS_MAX
/// range. We pick the bounding box of all known outputs (or a specific one) so
/// the synthesized pointer lands at the right global screen pixel.
fn pixel_to_abs(x: i32, y: i32, output_name: Option<&str>) -> Result<(i32, i32)> {
    let outputs = outputs::collect()?;
    if outputs.is_empty() {
        bail!("no wl_outputs available; run `computer outputs` to confirm");
    }

    let (min_x, min_y, max_x, max_y) = if let Some(name) = output_name {
        let o = outputs
            .iter()
            .find(|o| o.name == name)
            .with_context(|| format!("no output named {name:?}"))?;
        (o.x, o.y, o.x + o.width_px, o.y + o.height_px)
    } else {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for o in &outputs {
            min_x = min_x.min(o.x);
            min_y = min_y.min(o.y);
            max_x = max_x.max(o.x + o.width_px);
            max_y = max_y.max(o.y + o.height_px);
        }
        (min_x, min_y, max_x, max_y)
    };

    let span_x = (max_x - min_x).max(1) as f64;
    let span_y = (max_y - min_y).max(1) as f64;
    let nx = ((x - min_x) as f64 / span_x).clamp(0.0, 1.0);
    let ny = ((y - min_y) as f64 / span_y).clamp(0.0, 1.0);
    Ok(((nx * ABS_MAX as f64) as i32, (ny * ABS_MAX as f64) as i32))
}
