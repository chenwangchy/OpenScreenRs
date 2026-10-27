//! Sends the viewer's pointer and keyboard to this machine.
//!
//! `SendInput` is the interface the input stack itself uses, so injected events look like a real
//! keyboard to everything above the driver: modifier state is tracked by the system rather than by
//! us, and applications that ignore the older `mouse_event`/`keybd_event` still see these. It needs
//! no COM, no window and no dependency.
//!
//! The difficulty is all in the coordinates. The viewer sends a fraction of the picture, the
//! picture is one monitor, and `SendInput` wants 0..65535 across the whole virtual desktop in
//! physical pixels. [`crate::sys::become_dpi_aware`] is what makes "physical pixels" and "the space
//! this process sees" the same space.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::capture::Duplicator;
use crate::sys;

// MOUSEEVENTF_*
const MOVE: u32 = 0x0001;
const LEFT_DOWN: u32 = 0x0002;
const LEFT_UP: u32 = 0x0004;
const RIGHT_DOWN: u32 = 0x0008;
const RIGHT_UP: u32 = 0x0010;
const MIDDLE_DOWN: u32 = 0x0020;
const MIDDLE_UP: u32 = 0x0040;
const WHEEL: u32 = 0x0800;
const VIRTUAL_DESKTOP: u32 = 0x4000;
const ABSOLUTE: u32 = 0x8000;

// KEYEVENTF_*
const EXTENDED_KEY: u32 = 0x0001;
const KEY_UP: u32 = 0x0002;
const SCANCODE: u32 = 0x0008;

/// What one wheel notch is worth to Windows.
const WHEEL_DELTA: i32 = 120;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MouseInput {
    dx: i32,
    dy: i32,
    data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Payload {
    mouse: MouseInput,
    key: KeyInput,
}

/// INPUT: a type tag and the union of all three input kinds. 40 bytes on x64, which is what the
/// `cbSize` argument has to say for the call to be accepted.
#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: u32,
    /// The union is 8 byte aligned, so the tag is followed by four bytes of padding.
    padding: u32,
    payload: Payload,
}

fn send(inputs: &[Input]) {
    type SendInputFn = unsafe extern "system" fn(u32, *const Input, i32) -> u32;

    static FUNCTION: OnceLock<Option<SendInputFn>> = OnceLock::new();

    let Some(function) = *FUNCTION.get_or_init(|| sys::export("user32.dll", b"SendInput\0")) else {
        return;
    };

    unsafe {
        function(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<Input>() as i32,
        );
    }
}

fn mouse(flags: u32, dx: i32, dy: i32, data: u32) {
    send(&[Input {
        kind: 0,
        padding: 0,
        payload: Payload {
            mouse: MouseInput {
                dx,
                dy,
                data,
                flags,
                time: 0,
                extra: 0,
            },
        },
    }]);
}

fn key(scan: u16, extended: bool, down: bool) {
    let mut flags = SCANCODE;

    if extended {
        flags |= EXTENDED_KEY;
    }

    if !down {
        flags |= KEY_UP;
    }

    send(&[Input {
        kind: 1,
        padding: 0,
        payload: Payload {
            key: KeyInput {
                vk: 0,
                scan,
                flags,
                time: 0,
                extra: 0,
            },
        },
    }]);
}

/// What a viewer has down right now, so it can all be let go when it goes away.
///
/// The monitor list is cached because enumerating it creates a D3D11 device, and a moving pointer
/// arrives sixty times a second.
pub struct Session {
    monitors: Vec<crate::capture::Output>,
    refreshed: Instant,
    keys: Vec<(u16, bool)>,
    buttons: u32,
}

impl Session {
    pub fn new() -> Self {
        Self {
            monitors: Vec::new(),
            refreshed: Instant::now(),
            keys: Vec::new(),
            buttons: 0,
        }
    }

    /// Applies one message from a viewer. Returns false for anything unrecognised, which is also
    /// how a page from an older build stays harmless.
    pub fn apply(&mut self, message: &str, shared: Option<u32>) -> bool {
        self.handle(message, shared).is_some()
    }

    fn handle(&mut self, message: &str, shared: Option<u32>) -> Option<()> {
        let mut parts = message.split(' ');

        match parts.next()? {
            // `m <x> <y>`, both a 0..10000 fraction of the shared monitor.
            "m" => {
                let x: i64 = parts.next()?.parse().ok()?;
                let y: i64 = parts.next()?.parse().ok()?;
                let (monitor, desktop) = self.geometry(shared)?;
                let (x, y) = place(monitor, x, y);

                mouse(
                    MOVE | ABSOLUTE | VIRTUAL_DESKTOP,
                    absolute(x, desktop.0, desktop.2),
                    absolute(y, desktop.1, desktop.3),
                    0,
                );
            }
            // `b <button> <down>`, button as the browser numbers them: 0 left, 1 middle, 2 right.
            "b" => {
                let button: u32 = parts.next()?.parse().ok()?;
                let down: u8 = parts.next()?.parse().ok()?;
                self.button(button, down != 0)?;
            }
            // `w <notches>`, positive is up.
            "w" => {
                let notches: i32 = parts.next()?.parse().ok()?;
                mouse(WHEEL, 0, 0, (notches * WHEEL_DELTA) as u32);
            }
            // `k <code> <down>`, code being a browser `KeyboardEvent.code`.
            "k" => {
                let code = parts.next()?;
                let down: u8 = parts.next()?.parse().ok()?;
                self.key(code, down != 0)?;
            }
            // `r`: the page lost focus and wants everything released.
            "r" => self.release(),
            _ => return None,
        }

        Some(())
    }

    /// The shared monitor as a rectangle, and the virtual desktop positions are measured against.
    fn geometry(
        &mut self,
        shared: Option<u32>,
    ) -> Option<((i32, i32, u32, u32), (i32, i32, i32, i32))> {
        if self.monitors.is_empty() || self.refreshed.elapsed() > Duration::from_secs(2) {
            self.monitors = Duplicator::outputs();
            self.refreshed = Instant::now();
        }

        let monitor = shared
            .and_then(|index| self.monitors.iter().find(|output| output.index == index))
            .or_else(|| self.monitors.iter().find(|output| output.primary))
            .or_else(|| self.monitors.first())?;

        let monitor = (monitor.left, monitor.top, monitor.width, monitor.height);

        Some((monitor, sys::virtual_screen()))
    }

    fn button(&mut self, button: u32, down: bool) -> Option<()> {
        let bit = 1u32.checked_shl(button).filter(|_| button < 3)?;

        let flags = match (button, down) {
            (0, true) => LEFT_DOWN,
            (0, false) => LEFT_UP,
            (1, true) => MIDDLE_DOWN,
            (1, false) => MIDDLE_UP,
            (2, true) => RIGHT_DOWN,
            (2, false) => RIGHT_UP,
            _ => return None,
        };

        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }

        mouse(flags, 0, 0, 0);
        Some(())
    }

    fn key(&mut self, code: &str, down: bool) -> Option<()> {
        let (scan, extended) = scancode(code)?;

        if down {
            if !self.keys.contains(&(scan, extended)) {
                self.keys.push((scan, extended));
            }
        } else {
            self.keys.retain(|entry| *entry != (scan, extended));
        }

        key(scan, extended, down);
        Some(())
    }

    /// Lets go of everything still held.
    ///
    /// Called when the viewer leaves, when control is turned off and when the page loses focus: a
    /// Ctrl that nobody is holding any more is the difference between help and a locked keyboard.
    pub fn release(&mut self) {
        for (scan, extended) in std::mem::take(&mut self.keys) {
            key(scan, extended, false);
        }

        let buttons = std::mem::take(&mut self.buttons);

        for button in 0..3 {
            if buttons & (1 << button) != 0 {
                self.button(button, false);
            }
        }
    }
}

/// The pixel inside the virtual desktop that a 0..10000 fraction of the shared monitor means.
fn place(monitor: (i32, i32, u32, u32), x: i64, y: i64) -> (i64, i64) {
    let (left, top, width, height) = monitor;

    (
        left as i64 + (x.clamp(0, 10_000) * width as i64) / 10_000,
        top as i64 + (y.clamp(0, 10_000) * height as i64) / 10_000,
    )
}

/// The number `SendInput` wants for an absolute position: 0..65535 across the virtual desktop.
fn absolute(value: i64, origin: i32, size: i32) -> i32 {
    let span = (size.max(2) as i64) - 1;

    (((value - origin as i64) * 65535) / span).clamp(0, 65535) as i32
}

/// The Set 1 scan code for a browser `KeyboardEvent.code`, and whether it is an extended key.
///
/// `code` names a physical key rather than a character, which is what a remote machine needs: the
/// two computers need not share a keyboard layout. Sending the scan code instead of a virtual key
/// also keeps the far side's own layout, AltGr and dead keys working.
fn scancode(code: &str) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];

    if let Some(letter) = code.strip_prefix("Key").filter(|rest| rest.len() == 1) {
        let index = (letter.as_bytes()[0].to_ascii_uppercase() as usize).wrapping_sub('A' as usize);

        return LETTERS.get(index).map(|scan| (*scan, false));
    }

    if let Some(digit) = code.strip_prefix("Digit").filter(|rest| rest.len() == 1) {
        let value = digit.as_bytes()[0];

        if !value.is_ascii_digit() {
            return None;
        }

        // The digit row runs 1..9 and then 0, so this is not a subtraction.
        let scan = if value == b'0' {
            0x0B
        } else {
            0x02 + (value - b'1') as u16
        };

        return Some((scan, false));
    }

    if let Some(number) = code.strip_prefix('F').and_then(|rest| rest.parse::<u16>().ok()) {
        let scan = match number {
            1..=10 => 0x3A + number,
            11 => 0x57,
            12 => 0x58,
            _ => return None,
        };

        return Some((scan, false));
    }

    // Keys without the extended prefix. NumpadEnter and the navigation keys are not here: they
    // only exist as E0 prefixed scan codes.
    let scan = match code {
        "Escape" => 0x01,
        "Backspace" => 0x0E,
        "Tab" => 0x0F,
        "Enter" => 0x1C,
        "CapsLock" => 0x3A,
        "Space" => 0x39,
        "Backquote" => 0x29,
        "Minus" => 0x0C,
        "Equal" => 0x0D,
        "BracketLeft" => 0x1A,
        "BracketRight" => 0x1B,
        "Backslash" => 0x2B,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "IntlBackslash" => 0x56,
        "ShiftLeft" => 0x2A,
        "ShiftRight" => 0x36,
        "ControlLeft" => 0x1D,
        "AltLeft" => 0x38,
        "ScrollLock" => 0x46,
        "NumpadMultiply" => 0x37,
        "NumpadSubtract" => 0x4A,
        "NumpadAdd" => 0x4E,
        "NumpadDecimal" => 0x53,
        "Numpad0" => 0x52,
        "Numpad1" => 0x4F,
        "Numpad2" => 0x50,
        "Numpad3" => 0x51,
        "Numpad4" => 0x4B,
        "Numpad5" => 0x4C,
        "Numpad6" => 0x4D,
        "Numpad7" => 0x47,
        "Numpad8" => 0x48,
        "Numpad9" => 0x49,
        _ => return extended_scancode(code),
    };

    Some((scan, false))
}

/// Keys that only exist with the E0 prefix in front of their scan code.
fn extended_scancode(code: &str) -> Option<(u16, bool)> {
    let scan = match code {
        "ControlRight" => 0x1D,
        "NumpadEnter" => 0x1C,
        "NumpadDivide" => 0x35,
        "PrintScreen" => 0x37,
        "AltRight" => 0x38,
        "NumLock" => 0x45,
        "Home" => 0x47,
        "ArrowUp" => 0x48,
        "PageUp" => 0x49,
        "ArrowLeft" => 0x4B,
        "ArrowRight" => 0x4D,
        "End" => 0x4F,
        "ArrowDown" => 0x50,
        "PageDown" => 0x51,
        "Insert" => 0x52,
        "Delete" => 0x53,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "ContextMenu" => 0x5D,
        _ => return None,
    };

    Some((scan, true))
}

#[cfg(test)]
mod tests {
    use super::{absolute, place, scancode};

    #[test]
    fn a_fraction_of_the_picture_lands_inside_the_shared_monitor() {
        // The second monitor: its own 0..10000 runs from 2560 onwards, not from zero.
        assert_eq!(place((2560, 0, 2560, 1440), 0, 0), (2560, 0));
        assert_eq!(place((2560, 0, 2560, 1440), 5000, 5000), (3840, 720));
        assert_eq!(place((2560, 0, 2560, 1440), 10_000, 10_000), (5120, 1440));
        // A page that sends nonsense must not be able to put the pointer off the desktop.
        assert_eq!(place((2560, 0, 2560, 1440), -500, 99_999), (2560, 1440));
    }

    #[test]
    fn an_absolute_position_spans_the_whole_virtual_desktop() {
        assert_eq!(absolute(0, 0, 5120), 0);
        assert_eq!(absolute(5119, 0, 5120), 65535);
        assert_eq!(absolute(-300, 0, 5120), 0, "left of the desktop is clamped");

        let middle = absolute(2560, 0, 5120);
        assert!(
            (32760..=32775).contains(&middle),
            "the middle of the desktop should be the middle of the range, got {middle}"
        );
    }

    #[test]
    fn physical_keys_map_to_the_scan_codes_windows_expects() {
        assert_eq!(scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(scancode("KeyZ"), Some((0x2C, false)));
        assert_eq!(scancode("Digit1"), Some((0x02, false)));
        assert_eq!(scancode("Digit0"), Some((0x0B, false)));
        assert_eq!(scancode("F10"), Some((0x44, false)));
        assert_eq!(scancode("F12"), Some((0x58, false)));
        assert_eq!(scancode("Enter"), Some((0x1C, false)));
        assert_eq!(scancode("NumpadEnter"), Some((0x1C, true)));
        assert_eq!(scancode("Numpad5"), Some((0x4C, false)));
        assert_eq!(scancode("ArrowUp"), Some((0x48, true)));
        assert_eq!(scancode("ControlRight"), Some((0x1D, true)));
    }

    #[test]
    fn a_key_this_program_does_not_know_is_ignored() {
        // Injecting some plausible neighbour would be worse than doing nothing.
        assert_eq!(scancode("F13"), None);
        assert_eq!(scancode("Key\u{a3}"), None);
        assert_eq!(scancode("AudioVolumeUp"), None);
        assert_eq!(scancode(""), None);
    }
}
