//! Synthetic input: the mouse, the keyboard, and the wheel.
//!
//! Every rule here was learned by watching the Python implementation fail without it:
//!
//! * Absolute mouse coordinates are normalised over the **whole virtual desktop**, not the primary
//!   monitor. `SendInput`'s 0..65535 range spans every screen, so normalising against
//!   `SM_CXSCREEN` puts every click on the wrong display the moment a second monitor exists — and
//!   this machine has three, one of them above the primary, so the desktop origin is negative.
//! * Text is typed as UTF-16 code units with `KEYEVENTF_UNICODE`, so it lands whatever keyboard
//!   layout is active, and an astral character goes as its two surrogate halves.
//! * A small sleep follows every event. Apps drop synthetic events sent faster than this.
//! * `key_held` reads `GetAsyncKeyState`, because push-to-talk needs the key *release* that
//!   `RegisterHotKey` never reports.

use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

/// `SendInput`'s absolute coordinate space, per axis, across the whole virtual desktop.
const ABSOLUTE_RANGE: i64 = 65535;
/// The pause after each synthetic event. Apps drop events sent faster than this.
const POST_EVENT_SLEEP: Duration = Duration::from_millis(40);
/// A wheel notch is 120 and scrolls three lines, so one "line" is 40.
const WHEEL_PER_LINE: i32 = 40;
/// Control, the modifier every shortcut in this project hangs off on Windows.
const VK_CONTROL: u16 = 0x11;
/// `GetAsyncKeyState`'s "currently down" bit. The low bit means "pressed since last asked", which
/// is not the question push-to-talk is asking.
const KEY_DOWN_BIT: i16 = -0x8000; // 0x8000 as a signed 16-bit value

/// The virtual desktop: its origin on the desktop and its size, all in physical pixels. The origin
/// is negative whenever a monitor sits left of or above the primary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// A physical pixel as the 0..65535 coordinate `SendInput` wants, relative to the virtual desktop.
///
/// Pure, so the arithmetic is tested without a desktop: this is the calculation that decides which
/// screen a click lands on.
fn absolute_coords(x: i32, y: i32, desktop: VirtualDesktop) -> (i32, i32) {
    let width = if desktop.width > 0 { desktop.width } else { 1 } as i64;
    let height = if desktop.height > 0 {
        desktop.height
    } else {
        1
    } as i64;
    let scale = |value: i64, span: i64| -> i32 {
        // Round half away from zero, as Python's round-to-even does not and the half-pixel case is
        // not worth a difference between the two implementations.
        let numerator = value * ABSOLUTE_RANGE;
        let rounded = if numerator >= 0 {
            (numerator + span / 2) / span
        } else {
            (numerator - span / 2) / span
        };
        rounded as i32
    };
    (
        scale((x - desktop.left) as i64, width),
        scale((y - desktop.top) as i64, height),
    )
}

/// The virtual desktop as Windows currently reports it.
fn virtual_desktop() -> VirtualDesktop {
    unsafe {
        VirtualDesktop {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn send(event: INPUT) {
    unsafe { SendInput(&[event], std::mem::size_of::<INPUT>() as i32) };
    sleep(POST_EVENT_SLEEP);
}

fn mouse_event(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) {
    send(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

fn key_event(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) {
    send(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    });
}

/// Where the cursor is, in physical pixels on the virtual desktop. May be negative.
pub fn mouse_location() -> (i32, i32) {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// Put the cursor on a physical pixel anywhere on the virtual desktop.
pub fn move_mouse(x: i32, y: i32) {
    let (ax, ay) = absolute_coords(x, y, virtual_desktop());
    mouse_event(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, ax, ay, 0);
}

/// Move, then press and release the left button there. Two separate events: a down and an up sent
/// as one batch arrive too close together for some apps to read as a click.
pub fn click_at(x: i32, y: i32) {
    move_mouse(x, y);
    mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0);
    mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0);
}

/// Tap a virtual key, optionally with Control held around it.
pub fn press(vk: u16, ctrl: bool) {
    if ctrl {
        key_event(VK_CONTROL, 0, KEYBD_EVENT_FLAGS(0));
    }
    key_event(vk, 0, KEYBD_EVENT_FLAGS(0));
    key_event(vk, 0, KEYEVENTF_KEYUP);
    if ctrl {
        key_event(VK_CONTROL, 0, KEYEVENTF_KEYUP);
    }
}

/// Type text as Unicode, one event pair per UTF-16 code unit.
///
/// `KEYEVENTF_UNICODE` carries the code unit in the scan code, so the text arrives whatever
/// keyboard layout is active instead of being translated through it. A character outside the BMP
/// goes as its two surrogate halves, which is exactly what iterating UTF-16 code units gives.
pub fn type_text(text: &str) {
    for unit in text.encode_utf16() {
        key_event(0, unit, KEYEVENTF_UNICODE);
        key_event(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }
}

/// Scroll by a number of lines. Positive scrolls up, as Windows means it.
///
/// A wheel event goes to whatever sits under the cursor, not to the focused window, so the cursor
/// is parked over the centre of the frontmost window first — otherwise the scroll lands in whatever
/// the pointer happened to be resting on.
pub fn scroll(lines: i32) {
    if let Some((x, y)) = crate::winlist::foreground_center() {
        move_mouse(x, y);
    }
    mouse_event(MOUSEEVENTF_WHEEL, 0, 0, lines * WHEEL_PER_LINE);
}

/// Let whichever process asks next take the foreground.
///
/// Windows' foreground lock refuses `SetForegroundWindow` from a process that did not receive the
/// last input. A global hotkey is delivered to this core, not to the Electron shell, so the shell
/// cannot focus the command bar it opens in answer to one: the bar appears behind the window the
/// user was in, and what they type goes there instead. Granting the right to any process just
/// before the shell hears about the hotkey is what lets the bar take focus. The grant lasts until
/// the next input or the next foreground change, so it hands over nothing that lingers.
pub fn allow_foreground_handoff() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
    // SAFETY: no pointers; fails only when this process holds no foreground right to give.
    unsafe { AllowSetForegroundWindow(ASFW_ANY) }.is_ok()
}

/// Seconds between the last input's tick and now, across the 49.7-day wrap of the tick counter.
pub fn idle_from(now_ms: u32, last_input_ms: u32) -> f64 {
    now_ms.wrapping_sub(last_input_ms) as f64 / 1000.0
}

/// Seconds since the user last moved the mouse or pressed a key, system-wide.
pub fn idle_seconds() -> Option<f64> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: a correctly sized out struct.
    let ok = unsafe { GetLastInputInfo(&mut info) }.as_bool();
    // SAFETY: no preconditions.
    ok.then(|| idle_from(unsafe { GetTickCount() }, info.dwTime))
}

/// True while a virtual key is physically down.
///
/// Push-to-talk needs the release, and `RegisterHotKey` reports only presses, so this is the only
/// way to see the key come back up.
pub fn key_held(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) & KEY_DOWN_BIT != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_survives_the_tick_counter_wrapping() {
        assert_eq!(idle_from(5_000, 2_000), 3.0);
        assert_eq!(idle_from(1_000, u32::MAX - 999), 2.0);
        assert_eq!(idle_from(7, 7), 0.0);
    }

    /// This machine: three monitors, the desktop spanning x from 0 and y from -1440.
    const THIS_MACHINE: VirtualDesktop = VirtualDesktop {
        left: 0,
        top: -1440,
        width: 5120,
        height: 2880,
    };

    #[test]
    fn corners_of_a_negative_origin_desktop_map_to_the_full_range() {
        assert_eq!(absolute_coords(0, -1440, THIS_MACHINE), (0, 0));
        assert_eq!(
            absolute_coords(5120, 1440, THIS_MACHINE),
            (ABSOLUTE_RANGE as i32, ABSOLUTE_RANGE as i32)
        );
    }

    #[test]
    fn the_primary_origin_is_not_the_desktop_origin() {
        // (0, 0) is the top-left of the primary monitor, which on this machine sits halfway down
        // the virtual desktop. Normalising against the primary would call this y = 0 and put the
        // pointer on the monitor above.
        let (x, y) = absolute_coords(0, 0, THIS_MACHINE);
        assert_eq!(x, 0);
        assert_eq!(y, 32768); // 1440/2880 of the range
        assert!(
            y > 0,
            "a point below the desktop top must not normalise to 0"
        );
    }

    #[test]
    fn a_point_above_the_desktop_origin_goes_negative_rather_than_clamping() {
        // Out of range is the caller's problem; silently clamping would hide a bad coordinate.
        let (_, y) = absolute_coords(0, -2000, THIS_MACHINE);
        assert!(y < 0, "expected a negative normalised y, got {y}");
    }

    #[test]
    fn a_single_screen_desktop_at_the_origin_is_a_plain_proportion() {
        let one = VirtualDesktop {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(absolute_coords(960, 540, one), (32768, 32768));
        assert_eq!(absolute_coords(1920, 1080, one), (65535, 65535));
    }

    #[test]
    fn a_zero_sized_desktop_does_not_divide_by_zero() {
        let none = VirtualDesktop {
            left: 0,
            top: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(absolute_coords(0, 0, none), (0, 0));
    }

    #[test]
    fn utf16_units_cover_surrogate_pairs() {
        // What type_text sends, as the sequence of scan codes. An emoji is two events' worth.
        let units: Vec<u16> = "a😀".encode_utf16().collect();
        assert_eq!(units, vec![0x0061, 0xD83D, 0xDE00]);
    }

    #[test]
    fn a_wheel_line_is_a_third_of_a_notch() {
        assert_eq!(3 * WHEEL_PER_LINE, 120);
        assert_eq!(-3 * WHEEL_PER_LINE, -120);
    }

    #[test]
    fn the_key_down_bit_is_the_high_bit_of_a_signed_short() {
        assert_eq!(KEY_DOWN_BIT, i16::MIN);
        let every_bit: i16 = -1; // the state of a key that is down and was pressed since asked
        assert_ne!(every_bit & KEY_DOWN_BIT, 0); // held
        let toggled: i16 = 1; // only "pressed since last asked"
        assert_eq!(toggled & KEY_DOWN_BIT, 0); // not held
    }
}
