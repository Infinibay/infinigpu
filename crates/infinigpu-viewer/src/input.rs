//! Guest-input encoding: turn the viewer's winit events into the compact infiniPixel input
//! protocol (JSON text messages) the master relay injects over QMP.
//!
//! Wire shape (one JSON object per WebSocket text message):
//!   {"t":"m","x":0.0..1.0,"y":0.0..1.0}     mouse absolute move (window-normalized)
//!   {"t":"b","b":"l"|"r"|"m","d":0|1}        mouse button up/down
//!   {"t":"w","d":+1|-1}                      wheel notch
//!   {"t":"k","q":"<qcode>","d":0|1}          key up/down (QEMU qcode)
//!
//! Messages are built by hand (no serde) — they are tiny and fixed-shape, and the viewer
//! avoids a JSON dependency. QEMU `abs` axes want 0..32767; we send a normalized fraction
//! and let the relay scale, so the viewer never needs the guest's resolution.

use winit::event::MouseButton;
use winit::keyboard::KeyCode;

/// Mouse absolute move, `x`/`y` normalized to the window (0.0 = left/top, 1.0 = right/bottom).
pub fn mouse_move(x: f64, y: f64) -> String {
    let x = x.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    format!("{{\"t\":\"m\",\"x\":{x:.4},\"y\":{y:.4}}}")
}

/// Mouse button press/release. Returns `None` for buttons we don't forward.
pub fn mouse_button(button: MouseButton, down: bool) -> Option<String> {
    let b = match button {
        MouseButton::Left => "l",
        MouseButton::Right => "r",
        MouseButton::Middle => "m",
        _ => return None,
    };
    Some(format!("{{\"t\":\"b\",\"b\":\"{b}\",\"d\":{}}}", down as u8))
}

/// A wheel notch. `dy` > 0 scrolls up, < 0 scrolls down; 0 is ignored.
pub fn wheel(dy: f32) -> Option<String> {
    if dy == 0.0 {
        return None;
    }
    let d = if dy > 0.0 { 1 } else { -1 };
    Some(format!("{{\"t\":\"w\",\"d\":{d}}}"))
}

/// A key press/release. Returns `None` for keys with no QEMU qcode mapping.
pub fn key(code: KeyCode, down: bool) -> Option<String> {
    let q = qcode(code)?;
    Some(format!("{{\"t\":\"k\",\"q\":\"{q}\",\"d\":{}}}", down as u8))
}

/// Map a winit physical [`KeyCode`] to the QEMU `QKeyCode` string (qapi/ui.json). Covers the
/// standard PC keyboard; unmapped keys return `None` and are dropped.
fn qcode(code: KeyCode) -> Option<&'static str> {
    use KeyCode::*;
    Some(match code {
        KeyA => "a", KeyB => "b", KeyC => "c", KeyD => "d", KeyE => "e", KeyF => "f",
        KeyG => "g", KeyH => "h", KeyI => "i", KeyJ => "j", KeyK => "k", KeyL => "l",
        KeyM => "m", KeyN => "n", KeyO => "o", KeyP => "p", KeyQ => "q", KeyR => "r",
        KeyS => "s", KeyT => "t", KeyU => "u", KeyV => "v", KeyW => "w", KeyX => "x",
        KeyY => "y", KeyZ => "z",
        Digit0 => "0", Digit1 => "1", Digit2 => "2", Digit3 => "3", Digit4 => "4",
        Digit5 => "5", Digit6 => "6", Digit7 => "7", Digit8 => "8", Digit9 => "9",
        Enter => "ret", Escape => "esc", Space => "spc", Backspace => "backspace",
        Tab => "tab", CapsLock => "caps_lock",
        ShiftLeft => "shift", ShiftRight => "shift_r",
        ControlLeft => "ctrl", ControlRight => "ctrl_r",
        AltLeft => "alt", AltRight => "alt_r",
        SuperLeft => "meta_l", SuperRight => "meta_r",
        ContextMenu => "menu",
        Minus => "minus", Equal => "equal",
        BracketLeft => "bracket_left", BracketRight => "bracket_right",
        Backslash => "backslash", Semicolon => "semicolon", Quote => "apostrophe",
        Comma => "comma", Period => "dot", Slash => "slash", Backquote => "grave_accent",
        ArrowUp => "up", ArrowDown => "down", ArrowLeft => "left", ArrowRight => "right",
        Home => "home", End => "end", PageUp => "pgup", PageDown => "pgdn",
        Insert => "insert", Delete => "delete",
        F1 => "f1", F2 => "f2", F3 => "f3", F4 => "f4", F5 => "f5", F6 => "f6",
        F7 => "f7", F8 => "f8", F9 => "f9", F10 => "f10", F11 => "f11", F12 => "f12",
        Numpad0 => "kp_0", Numpad1 => "kp_1", Numpad2 => "kp_2", Numpad3 => "kp_3",
        Numpad4 => "kp_4", Numpad5 => "kp_5", Numpad6 => "kp_6", Numpad7 => "kp_7",
        Numpad8 => "kp_8", Numpad9 => "kp_9",
        NumpadAdd => "kp_add", NumpadSubtract => "kp_subtract",
        NumpadMultiply => "kp_multiply", NumpadDivide => "kp_divide",
        NumpadEnter => "kp_enter", NumpadDecimal => "kp_decimal",
        _ => return None,
    })
}
