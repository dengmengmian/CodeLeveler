//! CoreGraphics input events.
//!
//! These are the fallback path, not the first one: semantic accessibility
//! actions run without stealing keyboard focus. Pointer and keyboard events
//! are used when a control does not expose an action, or when the model asks
//! for raw input.

use std::thread::sleep;
use std::time::Duration;

use crate::error::{CuaError, CuaResult};
use crate::types::{Modifier, MouseButton, Point};

use super::cf::{
    Boolean, CGEventCreateKeyboardEvent, CGEventCreateMouseEvent, CGEventCreateScrollWheelEvent2,
    CGEventKeyboardSetUnicodeString, CGEventPost, CGEventSetFlags, CGEventSetIntegerValueField,
    CGEventSetLocation, CGPointRaw, CfOwned,
};

const HID_EVENT_TAP: u32 = 0;

const EVENT_LEFT_DOWN: u32 = 1;
const EVENT_LEFT_UP: u32 = 2;
const EVENT_RIGHT_DOWN: u32 = 3;
const EVENT_RIGHT_UP: u32 = 4;
const EVENT_MOUSE_MOVED: u32 = 5;

const FIELD_CLICK_STATE: u32 = 1;

const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_ALTERNATE: u64 = 1 << 19;
const FLAG_COMMAND: u64 = 1 << 20;

const SCROLL_UNIT_LINE: u32 = 1;

pub(crate) fn flags(modifiers: &[Modifier]) -> u64 {
    modifiers.iter().fold(0, |flags, modifier| {
        flags
            | match modifier {
                Modifier::Command => FLAG_COMMAND,
                Modifier::Control => FLAG_CONTROL,
                Modifier::Alt => FLAG_ALTERNATE,
                Modifier::Shift => FLAG_SHIFT,
            }
    })
}

pub(crate) fn key_code(key: &str) -> CuaResult<u16> {
    let normalized = key.trim().to_ascii_lowercase();
    let named = match normalized.as_str() {
        "return" | "enter" => Some(36),
        "tab" => Some(48),
        "space" => Some(49),
        "backspace" | "delete" => Some(51),
        "escape" | "esc" => Some(53),
        "forward_delete" | "forwarddelete" => Some(117),
        "home" => Some(115),
        "end" => Some(119),
        "page_up" | "pageup" => Some(116),
        "page_down" | "pagedown" => Some(121),
        "left" | "left_arrow" => Some(123),
        "right" | "right_arrow" => Some(124),
        "down" | "down_arrow" => Some(125),
        "up" | "up_arrow" => Some(126),
        "f1" => Some(122),
        "f2" => Some(120),
        "f3" => Some(99),
        "f4" => Some(118),
        "f5" => Some(96),
        "f6" => Some(97),
        "f7" => Some(98),
        "f8" => Some(100),
        "f9" => Some(101),
        "f10" => Some(109),
        "f11" => Some(103),
        "f12" => Some(111),
        "a" => Some(0),
        "s" => Some(1),
        "d" => Some(2),
        "f" => Some(3),
        "h" => Some(4),
        "g" => Some(5),
        "z" => Some(6),
        "x" => Some(7),
        "c" => Some(8),
        "v" => Some(9),
        "b" => Some(11),
        "q" => Some(12),
        "w" => Some(13),
        "e" => Some(14),
        "r" => Some(15),
        "y" => Some(16),
        "t" => Some(17),
        "1" => Some(18),
        "2" => Some(19),
        "3" => Some(20),
        "4" => Some(21),
        "6" => Some(22),
        "5" => Some(23),
        "=" => Some(24),
        "9" => Some(25),
        "7" => Some(26),
        "-" => Some(27),
        "8" => Some(28),
        "0" => Some(29),
        "]" => Some(30),
        "o" => Some(31),
        "u" => Some(32),
        "[" => Some(33),
        "i" => Some(34),
        "p" => Some(35),
        "l" => Some(37),
        "j" => Some(38),
        "'" => Some(39),
        "k" => Some(40),
        ";" => Some(41),
        "\\" => Some(42),
        "," => Some(43),
        "/" => Some(44),
        "n" => Some(45),
        "m" => Some(46),
        "." => Some(47),
        "`" => Some(50),
        _ => None,
    };
    named.ok_or_else(|| {
        CuaError::InvalidAction(format!(
            "unknown key {key:?}; use a letter, digit, punctuation character, or one of return, tab, space, backspace, escape, delete, arrows, home, end, pageup, pagedown, f1..f12"
        ))
    })
}

pub(crate) fn post_key(key: &str, modifiers: &[Modifier]) -> CuaResult<()> {
    let code = key_code(key)?;
    let flags = flags(modifiers);
    post_key_code(code, flags)
}

fn post_key_code(code: u16, flags: u64) -> CuaResult<()> {
    for down in [true, false] {
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), code, down as Boolean) };
        let event = unsafe { CfOwned::from_create(event) }
            .ok_or_else(|| backend("CGEventCreateKeyboardEvent returned null"))?;
        unsafe { CGEventSetFlags(event.as_ptr(), flags) };
        unsafe { CGEventPost(HID_EVENT_TAP, event.as_ptr()) };
    }
    Ok(())
}

/// Type arbitrary text without an input method. Each scalar becomes one
/// key-down/key-up pair carrying a UTF-16 string; the virtual key is unused.
pub(crate) fn post_text(text: &str) -> CuaResult<()> {
    for character in text.chars() {
        let mut buffer = [0u16; 2];
        let units = character.encode_utf16(&mut buffer);
        for down in [true, false] {
            let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), 0, down as Boolean) };
            let event = unsafe { CfOwned::from_create(event) }
                .ok_or_else(|| backend("CGEventCreateKeyboardEvent returned null"))?;
            unsafe { CGEventKeyboardSetUnicodeString(event.as_ptr(), units.len(), units.as_ptr()) };
            unsafe { CGEventPost(HID_EVENT_TAP, event.as_ptr()) };
        }
        // Applications that coalesce text input need the queue to advance
        // between scalars. This is a bounded, per-character delay, not a
        // synchronization primitive.
        sleep(Duration::from_millis(2));
    }
    Ok(())
}

pub(crate) fn post_click(point: Point, button: MouseButton, count: u8) -> CuaResult<()> {
    let count = count.clamp(1, 3);
    let (down_type, up_type, button_number) = match button {
        MouseButton::Left => (EVENT_LEFT_DOWN, EVENT_LEFT_UP, 0),
        MouseButton::Right => (EVENT_RIGHT_DOWN, EVENT_RIGHT_UP, 1),
    };
    post_mouse(EVENT_MOUSE_MOVED, point, button_number, 0)?;
    for click in 1..=count {
        post_mouse(down_type, point, button_number, i64::from(click))?;
        post_mouse(up_type, point, button_number, i64::from(click))?;
    }
    Ok(())
}

fn post_mouse(event_type: u32, point: Point, button: u32, click_state: i64) -> CuaResult<()> {
    let event =
        unsafe { CGEventCreateMouseEvent(std::ptr::null(), event_type, point.x, point.y, button) };
    let event = unsafe { CfOwned::from_create(event) }
        .ok_or_else(|| backend("CGEventCreateMouseEvent returned null"))?;
    if click_state > 0 {
        unsafe { CGEventSetIntegerValueField(event.as_ptr(), FIELD_CLICK_STATE, click_state) };
    }
    unsafe { CGEventPost(HID_EVENT_TAP, event.as_ptr()) };
    Ok(())
}

pub(crate) fn move_pointer(point: Point) -> CuaResult<()> {
    post_mouse(EVENT_MOUSE_MOVED, point, 0, 0)
}

pub(crate) fn post_scroll(point: Point, dx: f64, dy: f64) -> CuaResult<()> {
    move_pointer(point)?;
    let vertical = dy.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    let horizontal = dx.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    let event = unsafe {
        CGEventCreateScrollWheelEvent2(
            std::ptr::null(),
            SCROLL_UNIT_LINE,
            2,
            vertical,
            horizontal,
            0,
        )
    };
    let event = unsafe { CfOwned::from_create(event) }
        .ok_or_else(|| backend("CGEventCreateScrollWheelEvent2 returned null"))?;
    unsafe {
        CGEventSetLocation(
            event.as_ptr(),
            CGPointRaw {
                x: point.x,
                y: point.y,
            },
        )
    };
    unsafe { CGEventPost(HID_EVENT_TAP, event.as_ptr()) };
    Ok(())
}

fn backend(detail: &str) -> CuaError {
    CuaError::Backend {
        backend: "macos",
        detail: detail.to_string(),
    }
}
