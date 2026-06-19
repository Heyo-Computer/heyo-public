use anyhow::{Context, Result, anyhow, bail};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::thread::sleep;
use std::time::Duration;

use super::keymap::{modifier_flag, parse_key};
use super::perms;
use crate::platform::keymap_common::split_chord;
use crate::platform::types::{Button, KeyAction, MouseAction};

const TAP: CGEventTapLocation = CGEventTapLocation::HID;

fn source() -> Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("CGEventSourceCreate(HIDSystemState) failed"))
}

fn cg_button(b: Button) -> (CGEventType, CGEventType, CGMouseButton, Option<i64>) {
    // Returns (down_type, up_type, mouse_button, button_number_override).
    match b {
        Button::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left, None),
        Button::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right, None),
        Button::Middle => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, Some(2)),
        Button::Side => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, Some(3)),
        Button::Extra => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, Some(4)),
    }
}

fn current_pointer() -> Result<CGPoint> {
    let src = source()?;
    let evt = CGEvent::new(src).map_err(|_| anyhow!("CGEventCreate failed"))?;
    Ok(evt.location())
}

fn resolve_target_point(x: i32, y: i32, output: Option<&str>) -> Result<CGPoint> {
    let Some(name) = output else {
        return Ok(CGPoint::new(x as f64, y as f64));
    };
    let want = name.strip_prefix("display-").unwrap_or(name);
    let parsed: u32 = want
        .parse()
        .with_context(|| format!("output name {name:?} is not display-<id>"))?;
    let ids = CGDisplay::active_displays()
        .map_err(|e| anyhow!("CGGetActiveDisplayList failed: {e}"))?;
    if !ids.contains(&parsed) {
        bail!("no active display with id {parsed} (try `computer outputs`)");
    }
    let bounds = CGDisplay::new(parsed).bounds();
    Ok(CGPoint::new(bounds.origin.x + x as f64, bounds.origin.y + y as f64))
}

pub fn mouse(action: MouseAction) -> Result<()> {
    perms::require_accessibility()?;
    match action {
        MouseAction::Move { x, y, output } => {
            let p = resolve_target_point(x, y, output.as_deref())?;
            move_to(p)?;
        }
        MouseAction::MoveRel { dx, dy } => {
            let p = current_pointer()?;
            move_to(CGPoint::new(p.x + dx as f64, p.y + dy as f64))?;
        }
        MouseAction::Click { button, count } => {
            let p = current_pointer()?;
            let (down, up, btn, btn_no) = cg_button(button);
            for i in 1..=count.max(1) {
                if i > 1 {
                    sleep(Duration::from_millis(40));
                }
                post_mouse(down, p, btn, btn_no, Some(i as i64))?;
                sleep(Duration::from_millis(20));
                post_mouse(up, p, btn, btn_no, Some(i as i64))?;
            }
        }
        MouseAction::Down { button } => {
            let p = current_pointer()?;
            let (down, _up, btn, btn_no) = cg_button(button);
            post_mouse(down, p, btn, btn_no, Some(1))?;
        }
        MouseAction::Up { button } => {
            let p = current_pointer()?;
            let (_down, up, btn, btn_no) = cg_button(button);
            post_mouse(up, p, btn, btn_no, Some(1))?;
        }
        MouseAction::Scroll { dx, dy } => {
            // CLI contract: positive dy = scroll down. CG convention: positive
            // wheel1 scrolls up, so negate.
            let src = source()?;
            let evt = CGEvent::new_scroll_event(src, ScrollEventUnit::LINE, 2, -dy, dx, 0)
                .map_err(|_| anyhow!("CGEventCreateScrollWheelEvent2 failed"))?;
            evt.post(TAP);
        }
    }
    sleep(Duration::from_millis(10));
    Ok(())
}

fn move_to(p: CGPoint) -> Result<()> {
    let src = source()?;
    let evt = CGEvent::new_mouse_event(src, CGEventType::MouseMoved, p, CGMouseButton::Left)
        .map_err(|_| anyhow!("CGEventCreateMouseEvent(MouseMoved) failed"))?;
    evt.post(TAP);
    Ok(())
}

fn post_mouse(
    ty: CGEventType,
    p: CGPoint,
    btn: CGMouseButton,
    btn_no: Option<i64>,
    click_state: Option<i64>,
) -> Result<()> {
    let src = source()?;
    let evt = CGEvent::new_mouse_event(src, ty, p, btn)
        .map_err(|_| anyhow!("CGEventCreateMouseEvent failed"))?;
    if let Some(n) = btn_no {
        evt.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, n);
    }
    if let Some(s) = click_state {
        evt.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, s);
    }
    evt.post(TAP);
    Ok(())
}

/// Click-and-drag `button` from `from` to `to`: press at the start, post a
/// dragged event to the end, release. One logical gesture so the press is held
/// across the move (mirrors the Linux single-device drag).
pub fn drag(from: (i32, i32), to: (i32, i32), button: Button, output: Option<&str>) -> Result<()> {
    perms::require_accessibility()?;
    let p_from = resolve_target_point(from.0, from.1, output)?;
    let p_to = resolve_target_point(to.0, to.1, output)?;
    let (down, up, btn, btn_no) = cg_button(button);
    let dragged = match button {
        Button::Left => CGEventType::LeftMouseDragged,
        Button::Right => CGEventType::RightMouseDragged,
        _ => CGEventType::OtherMouseDragged,
    };
    move_to(p_from)?;
    post_mouse(down, p_from, btn, btn_no, Some(1))?;
    sleep(Duration::from_millis(20));
    post_mouse(dragged, p_to, btn, btn_no, None)?;
    sleep(Duration::from_millis(20));
    post_mouse(up, p_to, btn, btn_no, Some(1))?;
    sleep(Duration::from_millis(10));
    Ok(())
}

pub fn key(action: KeyAction) -> Result<()> {
    perms::require_accessibility()?;
    match action {
        KeyAction::Tap { key } => {
            let code = parse_key(&key)?;
            tap_key(code, CGEventFlags::empty())?;
        }
        KeyAction::Down { key } => {
            let code = parse_key(&key)?;
            post_key(code, true, CGEventFlags::empty())?;
        }
        KeyAction::Up { key } => {
            let code = parse_key(&key)?;
            post_key(code, false, CGEventFlags::empty())?;
        }
        KeyAction::Chord { combo } => {
            let parts = split_chord(&combo);
            if parts.is_empty() {
                bail!("empty chord");
            }
            let codes: Vec<u16> = parts.into_iter().map(parse_key).collect::<Result<_>>()?;
            // Split into modifiers (all but last) and the final key. For chords
            // like "shift" alone, treat the only key as the "final" so the user
            // gets a clean tap.
            let (final_key, mods) = if codes.len() == 1 {
                (codes[0], Vec::<u16>::new())
            } else {
                (*codes.last().unwrap(), codes[..codes.len() - 1].to_vec())
            };
            let mut mask = CGEventFlags::empty();
            for m in &mods {
                if let Some(f) = modifier_flag(*m) {
                    mask |= f;
                }
                post_key(*m, true, CGEventFlags::empty())?;
                sleep(Duration::from_millis(8));
            }
            sleep(Duration::from_millis(10));
            tap_key(final_key, mask)?;
            sleep(Duration::from_millis(10));
            for m in mods.iter().rev() {
                post_key(*m, false, CGEventFlags::empty())?;
                sleep(Duration::from_millis(8));
            }
        }
    }
    sleep(Duration::from_millis(10));
    Ok(())
}

fn tap_key(code: u16, mask: CGEventFlags) -> Result<()> {
    post_key(code, true, mask)?;
    sleep(Duration::from_millis(15));
    post_key(code, false, mask)?;
    Ok(())
}

fn post_key(code: u16, keydown: bool, mask: CGEventFlags) -> Result<()> {
    let src = source()?;
    let evt = CGEvent::new_keyboard_event(src, code, keydown)
        .map_err(|_| anyhow!("CGEventCreateKeyboardEvent failed"))?;
    if !mask.is_empty() {
        evt.set_flags(mask);
    }
    evt.post(TAP);
    Ok(())
}

pub fn type_text(text: &str, delay_ms: u64) -> Result<()> {
    perms::require_accessibility()?;
    for ch in text.chars() {
        let utf16: Vec<u16> = ch.encode_utf16(&mut [0u16; 2]).to_vec();
        let src = source()?;
        let down = CGEvent::new_keyboard_event(src, 0, true)
            .map_err(|_| anyhow!("CGEventCreateKeyboardEvent(down) failed"))?;
        down.set_string_from_utf16_unchecked(&utf16);
        down.post(TAP);
        sleep(Duration::from_millis(delay_ms.max(1)));

        let src = source()?;
        let up = CGEvent::new_keyboard_event(src, 0, false)
            .map_err(|_| anyhow!("CGEventCreateKeyboardEvent(up) failed"))?;
        up.set_string_from_utf16_unchecked(&utf16);
        up.post(TAP);
        sleep(Duration::from_millis(delay_ms.max(1)));
    }
    sleep(Duration::from_millis(10));
    Ok(())
}
