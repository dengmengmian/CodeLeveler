//! Win32 input injection.
//!
//! Pointer and keyboard events are the fallback path. Semantic UI Automation
//! patterns run first; these events are used when a control exposes no
//! pattern, or when the model asks for raw input.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    SendInput, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1,
    VK_HOME, VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_SPACE, VK_TAB, VK_UP, VkKeyScanW,
};
use windows::Win32::UI::WindowsAndMessaging::{SetCursorPos, WHEEL_DELTA};

use crate::error::{CuaError, CuaResult};
use crate::types::{Modifier, MouseButton, Point};

use super::NAME;

pub(super) fn click(point: Point, button: MouseButton, count: u8) -> CuaResult<()> {
    move_pointer(point)?;
    let (down, up) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
    };
    let mut inputs = Vec::with_capacity(usize::from(count) * 2);
    for _ in 0..count.clamp(1, 3) {
        inputs.push(mouse_input(down, 0));
        inputs.push(mouse_input(up, 0));
    }
    send(&inputs)
}

pub(super) fn move_pointer(point: Point) -> CuaResult<()> {
    let x = point
        .x
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    let y = point
        .y
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    unsafe { SetCursorPos(x, y) }
        .map_err(|error| backend(format!("move the pointer: {}", error.message())))
}

pub(super) fn text(text: &str) -> CuaResult<()> {
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let line = line.replace('\r', "");
        let units: Vec<u16> = line.encode_utf16().collect();
        if !units.is_empty() {
            let mut inputs = Vec::with_capacity(units.len() * 2);
            for unit in units {
                inputs.push(unicode_input(unit, false));
                inputs.push(unicode_input(unit, true));
            }
            send(&inputs)?;
        }
        if lines.peek().is_some() {
            press_vk(VK_RETURN, &[])?;
        }
    }
    Ok(())
}

pub(super) fn press(key: &str, modifiers: &[Modifier]) -> CuaResult<()> {
    let (key, implicit_shift) = key_for(key)?;
    let mut modifiers = modifiers.to_vec();
    if implicit_shift && !modifiers.contains(&Modifier::Shift) {
        modifiers.push(Modifier::Shift);
    }
    press_vk(key, &modifiers)
}

fn press_vk(key: VIRTUAL_KEY, modifiers: &[Modifier]) -> CuaResult<()> {
    let shift = modifiers.contains(&Modifier::Shift);
    let control = modifiers
        .iter()
        .any(|modifier| matches!(modifier, Modifier::Command | Modifier::Control));
    let alt = modifiers.contains(&Modifier::Alt);
    let mut held = Vec::new();
    if control {
        held.push(VK_CONTROL);
    }
    if alt {
        held.push(VK_MENU);
    }
    if shift {
        held.push(VK_SHIFT);
    }
    let mut inputs = Vec::new();
    for modifier in &held {
        inputs.push(key_input(*modifier, false));
    }
    inputs.push(key_input(key, false));
    inputs.push(key_input(key, true));
    for modifier in held.iter().rev() {
        inputs.push(key_input(*modifier, true));
    }
    send(&inputs)
}

pub(super) fn scroll(point: Point, dx: f64, dy: f64) -> CuaResult<()> {
    move_pointer(point)?;
    let vertical = (-dy.round()).clamp(-1000.0, 1000.0) as i32;
    let horizontal = dx.round().clamp(-1000.0, 1000.0) as i32;
    let mut inputs = Vec::new();
    if vertical != 0 {
        inputs.push(mouse_input(
            MOUSEEVENTF_WHEEL,
            (i64::from(vertical) * i64::from(WHEEL_DELTA)) as u32,
        ));
    }
    if horizontal != 0 {
        inputs.push(mouse_input(
            MOUSEEVENTF_HWHEEL,
            (i64::from(horizontal) * i64::from(WHEEL_DELTA)) as u32,
        ));
    }
    if inputs.is_empty() {
        return Ok(());
    }
    send(&inputs)
}

/// Translates a key name into a virtual key and whether the key produces the
/// requested character only with Shift. Named keys follow the macOS backend's
/// vocabulary, so a capability's input means the same thing on both platforms.
fn key_for(name: &str) -> CuaResult<(VIRTUAL_KEY, bool)> {
    let normalized = name.trim().to_ascii_lowercase();
    let named = match normalized.as_str() {
        "return" | "enter" => Some(VK_RETURN),
        "tab" => Some(VK_TAB),
        "space" => Some(VK_SPACE),
        "backspace" | "delete" => Some(VK_BACK),
        "forward_delete" | "forwarddelete" => Some(VK_DELETE),
        "escape" | "esc" => Some(VK_ESCAPE),
        "insert" => Some(VK_INSERT),
        "home" => Some(VK_HOME),
        "end" => Some(VK_END),
        "page_up" | "pageup" => Some(VK_PRIOR),
        "page_down" | "pagedown" => Some(VK_NEXT),
        "left" | "left_arrow" => Some(VK_LEFT),
        "right" | "right_arrow" => Some(VK_RIGHT),
        "up" | "up_arrow" => Some(VK_UP),
        "down" | "down_arrow" => Some(VK_DOWN),
        _ => None,
    };
    if let Some(key) = named {
        return Ok((key, false));
    }
    if let Some(rest) = normalized.strip_prefix('f')
        && let Ok(number) = rest.parse::<u16>()
        && (1..=24).contains(&number)
    {
        return Ok((VIRTUAL_KEY(VK_F1.0 + number - 1), false));
    }
    if normalized.len() == 1 {
        let character = normalized.as_bytes()[0];
        if character.is_ascii_lowercase() {
            return Ok((
                VIRTUAL_KEY(u16::from(b'A') + u16::from(character - b'a')),
                false,
            ));
        }
        if character.is_ascii_digit() {
            return Ok((
                VIRTUAL_KEY(u16::from(b'0') + u16::from(character - b'0')),
                false,
            ));
        }
        if character.is_ascii() && !character.is_ascii_control() {
            let scan = unsafe { VkKeyScanW(u16::from(character)) };
            if scan != -1 {
                let scan = scan as u16;
                let shift = scan & 0x0100 != 0;
                let control_or_alt = scan & 0x0600 != 0;
                if control_or_alt {
                    return Err(unknown_key(name));
                }
                return Ok((VIRTUAL_KEY(scan & 0x00ff), shift));
            }
        }
    }
    Err(unknown_key(name))
}

fn unknown_key(name: &str) -> CuaError {
    CuaError::InvalidAction(format!(
        "unknown key {name:?}; use a letter, digit, punctuation character, or one of return, \
         tab, space, backspace, escape, delete, insert, arrows, home, end, pageup, pagedown, \
         f1..f24"
    ))
}

fn key_input(key: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: if up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> CuaResult<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(backend(
            "SendInput did not accept the input; the desktop may be locked, or UIPI blocks \
             input to the foreground window",
        ))
    }
}

fn backend(detail: impl Into<String>) -> CuaError {
    CuaError::Backend {
        backend: NAME,
        detail: detail.into(),
    }
}
