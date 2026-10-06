//! Monitors, and the DPI awareness that has to come before any of them are asked about.
//!
//! Everything here reads the desktop as it actually is: several monitors, any of which may sit left
//! of or above the primary and so carry negative coordinates. Nothing may assume the desktop starts
//! at (0, 0), and nothing may assume there is only one screen.

use std::sync::Once;

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_CXSCREEN, SM_CYSCREEN,
};

/// Declare per-monitor DPI awareness. Without this every rectangle Windows reports is a lie on a
/// scaled display, and a click computed from one lands short. It must happen before the first
/// window or monitor query, which is why it is not optional and not lazy.
pub fn declare_dpi_aware() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() }
}

static DPI: Once = Once::new();

/// Declare DPI awareness exactly once per process, from whichever entry point gets there first.
///
/// Every public query in this module calls it, so there is no order of calls that can ask Windows
/// about a rectangle before the process has told Windows it understands scaling.
pub fn ensure_dpi_aware() {
    DPI.call_once(|| {
        declare_dpi_aware();
    });
}

/// The primary display's size in physical pixels, once awareness is declared.
pub fn primary_size() -> (i32, i32) {
    ensure_dpi_aware();
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// One physical display, in the virtual desktop's physical pixels. Any edge may be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub index: usize,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

impl Monitor {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// The four edges, in the order a capture rectangle wants them.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        (self.left, self.top, self.right, self.bottom)
    }

    /// Whether a point falls on this display. Half-open on the right and bottom, so two monitors
    /// that meet along an edge never both claim the same pixel.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// One monitor as Windows reports it, before an index is assigned. The pure half of `monitors()`,
/// kept separate so the ordering below can be tested without a desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMonitor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

/// Order the displays and number them: primary first, then left to right, so an index means the
/// same thing twice. `left` can be negative, so this is a signed comparison and never a distance.
pub fn ordered(mut raw: Vec<RawMonitor>) -> Vec<Monitor> {
    raw.sort_by_key(|m| (if m.primary { 0 } else { 1 }, m.left, m.top));
    raw.into_iter()
        .enumerate()
        .map(|(index, m)| Monitor {
            index,
            left: m.left,
            top: m.top,
            right: m.right,
            bottom: m.bottom,
            primary: m.primary,
        })
        .collect()
}

/// The display containing a point, or 0 when the point falls on no display at all.
///
/// Pure, so the containment rule is testable against a hand-built list.
pub fn index_at(list: &[Monitor], x: i32, y: i32) -> usize {
    list.iter()
        .find(|m| m.contains(x, y))
        .map(|m| m.index)
        .unwrap_or(0)
}

/// The display whose rectangle matches these bounds, or 0 when none does.
pub fn index_of_bounds(list: &[Monitor], bounds: (i32, i32, i32, i32)) -> usize {
    list.iter()
        .find(|m| m.bounds() == bounds)
        .map(|m| m.index)
        .unwrap_or(0)
}

unsafe extern "system" fn collect(
    handle: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    param: LPARAM,
) -> BOOL {
    let found = &mut *(param.0 as *mut Vec<HMONITOR>);
    found.push(handle);
    TRUE
}

/// Every display device handle, in whatever order Windows enumerates them.
fn monitor_handles() -> Vec<HMONITOR> {
    let mut found: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(collect),
            LPARAM(&mut found as *mut Vec<HMONITOR> as isize),
        );
    }
    found
}

/// A monitor handle as its rectangle and whether it is the primary. Any edge may be negative.
fn monitor_info(handle: HMONITOR) -> Option<RawMonitor> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetMonitorInfoW(handle, &mut info) };
    if !ok.as_bool() {
        return None;
    }
    let r = info.rcMonitor;
    Some(RawMonitor {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

/// Every display, primary first and then left to right.
pub fn monitors() -> Vec<Monitor> {
    ensure_dpi_aware();
    ordered(
        monitor_handles()
            .into_iter()
            .filter_map(monitor_info)
            .collect(),
    )
}

/// The display containing a point, or 0 when the point falls on no display at all.
pub fn monitor_at(x: i32, y: i32) -> usize {
    index_at(&monitors(), x, y)
}

/// The display a window lives on, asked of Windows rather than derived from its rectangle: a
/// minimized window's rectangle is off at about (-32000, -32000) and names no real screen.
pub fn monitor_of(hwnd: isize) -> usize {
    ensure_dpi_aware();
    let handle = unsafe { MonitorFromWindow(HWND(hwnd as *mut _), MONITOR_DEFAULTTONEAREST) };
    if handle.is_invalid() {
        return 0;
    }
    match monitor_info(handle) {
        Some(raw) => index_of_bounds(&monitors(), (raw.left, raw.top, raw.right, raw.bottom)),
        None => 0,
    }
}

/// The virtual desktop's bounding rectangle, which may start left of or above the origin.
pub fn virtual_bounds() -> (i32, i32, i32, i32) {
    let list = monitors();
    if list.is_empty() {
        let (w, h) = primary_size();
        return (0, 0, w, h);
    }
    (
        list.iter().map(|m| m.left).min().unwrap_or(0),
        list.iter().map(|m| m.top).min().unwrap_or(0),
        list.iter().map(|m| m.right).max().unwrap_or(0),
        list.iter().map(|m| m.bottom).max().unwrap_or(0),
    )
}

/// The cursor, in the virtual desktop's physical pixels. Signed: the pointer can sit at y = -900.
pub fn cursor_position() -> (i32, i32) {
    ensure_dpi_aware();
    let mut point = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine's real layout: a monitor above the primary at y = -1440 and one to the right at
    /// x = 2561. Hand-built so the pure rules are checked without a desktop.
    fn layout() -> Vec<Monitor> {
        ordered(vec![
            RawMonitor {
                left: 2561,
                top: -496,
                right: 4481,
                bottom: 584,
                primary: false,
            },
            RawMonitor {
                left: 1,
                top: -1440,
                right: 2561,
                bottom: 0,
                primary: false,
            },
            RawMonitor {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1600,
                primary: true,
            },
        ])
    }

    #[test]
    fn primary_comes_first_then_left_to_right() {
        let list = layout();
        assert_eq!(list[0].bounds(), (0, 0, 2560, 1600));
        assert!(list[0].primary);
        assert_eq!(list[1].bounds(), (1, -1440, 2561, 0));
        assert_eq!(list[2].bounds(), (2561, -496, 4481, 584));
        assert_eq!(
            list.iter().map(|m| m.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn a_monitor_left_of_and_above_the_primary_still_follows_it() {
        // Left of the primary means a negative `left`; the primary must stay index 0 anyway, and
        // the rest must sort by signed `left`, not by magnitude.
        let list = ordered(vec![
            RawMonitor {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
                primary: true,
            },
            RawMonitor {
                left: -1920,
                top: -200,
                right: 0,
                bottom: 880,
                primary: false,
            },
            RawMonitor {
                left: -3840,
                top: 0,
                right: -1920,
                bottom: 1080,
                primary: false,
            },
        ]);
        assert!(list[0].primary);
        assert_eq!(list[1].left, -3840);
        assert_eq!(list[2].left, -1920);
    }

    #[test]
    fn monitors_at_the_same_left_sort_by_top() {
        let list = ordered(vec![
            RawMonitor {
                left: 100,
                top: 500,
                right: 1000,
                bottom: 1000,
                primary: false,
            },
            RawMonitor {
                left: 100,
                top: -500,
                right: 1000,
                bottom: 0,
                primary: false,
            },
        ]);
        assert_eq!(list[0].top, -500);
        assert_eq!(list[1].top, 500);
    }

    #[test]
    fn width_and_height_are_signed_differences() {
        let list = layout();
        assert_eq!((list[0].width(), list[0].height()), (2560, 1600));
        assert_eq!((list[1].width(), list[1].height()), (2560, 1440));
        assert_eq!((list[2].width(), list[2].height()), (1920, 1080));
    }

    #[test]
    fn index_at_finds_the_negative_origin_monitors() {
        let list = layout();
        assert_eq!(index_at(&list, 1280, 800), 0);
        assert_eq!(index_at(&list, 1280, -700), 1); // above the primary
        assert_eq!(index_at(&list, 3500, 100), 2); // right of the primary, negative top
        assert_eq!(index_at(&list, 1, -1440), 1); // its exact top-left corner
    }

    #[test]
    fn index_at_is_half_open_so_an_edge_belongs_to_one_monitor() {
        let list = layout();
        // y = 0 is the primary's first row, not the top monitor's last.
        assert_eq!(index_at(&list, 100, 0), 0);
        // x = 2561 is the right monitor's first column, not the top one's last.
        assert_eq!(index_at(&list, 2561, 0), 2);
    }

    #[test]
    fn a_point_outside_every_monitor_falls_back_to_zero() {
        let list = layout();
        assert_eq!(index_at(&list, -5000, -5000), 0); // nowhere at all
        assert_eq!(index_at(&list, 2560, -800), 0); // the one-pixel gap left of the top monitor
        assert_eq!(index_at(&list, -32000, -32000), 0); // a minimized window's parked corner
    }

    #[test]
    fn bounds_lookup_names_the_monitor_or_falls_back() {
        let list = layout();
        assert_eq!(index_of_bounds(&list, (2561, -496, 4481, 584)), 2);
        assert_eq!(index_of_bounds(&list, (1, -1440, 2561, 0)), 1);
        assert_eq!(index_of_bounds(&list, (7, 7, 8, 8)), 0);
    }

    #[test]
    fn an_empty_desktop_answers_zero_rather_than_panicking() {
        assert_eq!(index_at(&[], 10, 10), 0);
        assert_eq!(index_of_bounds(&[], (0, 0, 1, 1)), 0);
    }
}
