//! The window inventory: which real windows exist, on which screen, and how to switch to one.
//!
//! Four rules here are each the fix for a run that went wrong:
//!
//! 1. A minimized window parks its rectangle near (-32000, -32000). That is not a place, so it is
//!    never filtered for being too small and never asked where it is — its screen comes from
//!    `monitor_of`, which asks Windows.
//! 2. This process's own windows are excluded. Without that, the program's control panel offered
//!    itself as a window to switch to and its own buttons as things to click.
//! 3. `activate_window` attaches to the foreground thread's input queue before
//!    `SetForegroundWindow` — Windows refuses the call from a process that does not own the
//!    foreground — and then *confirms* the foreground moved. Reporting success without checking is
//!    what made one run "switch to" the same window three times in a row.
//! 4. The order is deterministic: foreground first, then by monitor, then z-order as `EnumWindows`
//!    yields it. A list that reshuffles between two questions makes "the second window" a lie.

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, RECT, TRUE};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
    QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow,
    ShowWindow, SW_RESTORE,
};

/// A minimized window parks at about (-32000, -32000). Anything out that far is a parked window,
/// not a window on a screen, however many monitors sit left of the primary.
const ICONIC_RECT_EDGE: i32 = -30000;
/// How often the activation check asks whether the foreground has moved yet.
const ACTIVATE_POLL: Duration = Duration::from_millis(100);
/// The default minimum side: anything smaller is a palette or a tooltip, not a window to work in.
pub const MIN_WINDOW_SIDE_PX: i32 = 50;

/// One switchable top-level window, and which screen it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub app: String,
    pub pid: u32,
    /// left, top, right, bottom in physical pixels on the virtual desktop. Any may be negative.
    pub rect: (i32, i32, i32, i32),
    pub monitor: usize,
    pub minimized: bool,
    pub foreground: bool,
}

// ---------------------------------------------------------------- the pure decisions

/// Whether a window is minimized: Windows says so, or its rectangle is parked out in the far
/// negative corner where no screen is.
fn minimized_from(iconic: bool, rect: (i32, i32, i32, i32)) -> bool {
    iconic || rect.0 <= ICONIC_RECT_EDGE
}

/// Whether a window belongs in the inventory.
///
/// Visible, titled, not ours, and bigger than `min_side` on both sides — except a minimized
/// window, which is kept whatever its parked rectangle claims about its size.
fn is_real_window(
    visible: bool,
    title: &str,
    pid: u32,
    own_pid: u32,
    minimized: bool,
    rect: (i32, i32, i32, i32),
    min_side: i32,
) -> bool {
    if !visible || title.is_empty() || pid == own_pid {
        return false;
    }
    if minimized {
        return true;
    }
    rect.2 - rect.0 > min_side && rect.3 - rect.1 > min_side
}

/// Foreground first, then by monitor, then z-order. `z` is the position `EnumWindows` gave it,
/// which is front-to-back within a monitor.
fn order_key(info: &WindowInfo, z: usize) -> (u8, usize, usize) {
    (u8::from(!info.foreground), info.monitor, z)
}

// ---------------------------------------------------------------- Win32

fn window_title(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if written <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..written as usize])
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn thread_of(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((rect.left, rect.top, rect.right, rect.bottom))
}

/// The executable's stem, without its extension: "chrome", "explorer", "Code".
pub fn process_name(pid: u32) -> String {
    let handle: HANDLE = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
    {
        Ok(handle) => handle,
        Err(_) => return String::new(),
    };
    let mut buffer = [0u16; 1024];
    let mut size = buffer.len() as u32;
    let queried = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if queried.is_err() {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    executable_stem(&path)
}

/// A full image path reduced to the name the rest of the project speaks.
fn executable_stem(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => name[..dot].to_string(),
        _ => name.to_string(),
    }
}

/// The handle of the window that currently has the foreground, or 0 when none does.
pub fn foreground() -> isize {
    unsafe { GetForegroundWindow() }.0 as isize
}

unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> BOOL {
    let found = &mut *(param.0 as *mut Vec<HWND>);
    found.push(hwnd);
    TRUE
}

/// Every top-level window, front to back, as Windows enumerates them.
fn top_level_windows() -> Vec<HWND> {
    let mut found: Vec<HWND> = Vec::new();
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut found as *mut _ as isize)) };
    found
}

/// Every real, switchable window on every monitor, in a stable order.
///
/// A minimized window keeps its place, flagged, with its screen asked of `monitor_of` rather than
/// read off the rectangle it parked at.
pub fn open_windows(min_side: i32) -> Vec<WindowInfo> {
    let own_pid = unsafe { GetCurrentProcessId() };
    // The panel is a different process from this core: Electron starts the core and passes its own
    // pid. Its windows are ours too, or a run reads its own panel as the app to work in.
    let ui_pid = std::env::var("POINTER_UI_PID").ok().and_then(|v| v.parse::<u32>().ok());
    let front = foreground();
    let mut found: Vec<(usize, WindowInfo)> = Vec::new();
    for (z, hwnd) in top_level_windows().into_iter().enumerate() {
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        let title = window_title(hwnd);
        let pid = pid_of(hwnd);
        let Some(rect) = window_rect(hwnd) else {
            continue;
        };
        let minimized = minimized_from(unsafe { IsIconic(hwnd) }.as_bool(), rect);
        if Some(pid) == ui_pid
            || !is_real_window(visible, &title, pid, own_pid, minimized, rect, min_side)
        {
            continue;
        }
        let info = WindowInfo {
            hwnd: hwnd.0 as isize,
            title,
            app: process_name(pid),
            pid,
            rect,
            monitor: crate::display::monitor_of(hwnd.0 as isize),
            minimized,
            foreground: hwnd.0 as isize == front,
        };
        found.push((z, info));
    }
    found.sort_by_key(|(z, info)| order_key(info, *z));
    found.into_iter().map(|(_z, info)| info).collect()
}

/// The centre of the foreground window, in physical pixels, when it is big enough to be a window
/// being worked in. Wheel events go to whatever is under the cursor, so `scroll` parks there first.
pub(crate) fn foreground_center() -> Option<(i32, i32)> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return None;
    }
    let (left, top, right, bottom) = window_rect(hwnd)?;
    let (w, h) = (right - left, bottom - top);
    if w <= MIN_WINDOW_SIDE_PX || h <= MIN_WINDOW_SIDE_PX {
        return None;
    }
    Some((left + w / 2, top + h / 2))
}

/// Bring a window to the front, the documented way, and confirm it got there.
///
/// `SetForegroundWindow` is refused outright when the caller does not own the foreground, so this
/// borrows the foreground thread's input queue with `AttachThreadInput` first. A minimized window
/// is restored before it is raised, or it comes forward still an icon.
pub fn activate_window(hwnd: isize, timeout_ms: u64) -> bool {
    let target = HWND(hwnd as *mut core::ffi::c_void);
    if unsafe { IsIconic(target) }.as_bool() {
        unsafe {
            let _ = ShowWindow(target, SW_RESTORE);
        }
    }
    let current = unsafe { GetCurrentThreadId() };
    let owner = thread_of(unsafe { GetForegroundWindow() });
    let attached = owner != 0
        && current != owner
        && unsafe { AttachThreadInput(current, owner, true) }.as_bool();
    unsafe {
        let _ = SetForegroundWindow(target);
        let _ = BringWindowToTop(target);
    }
    if attached {
        unsafe {
            let _ = AttachThreadInput(current, owner, false);
        }
    }
    // Windows may refuse quietly, so the only honest answer comes from looking.
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if foreground() == hwnd {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(ACTIVATE_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARKED: (i32, i32, i32, i32) = (-32000, -32000, -31840, -31972);
    const ON_SCREEN: (i32, i32, i32, i32) = (100, 100, 1000, 800);
    const OWN: u32 = 4242;

    fn info(monitor: usize, foreground: bool) -> WindowInfo {
        WindowInfo {
            hwnd: 1,
            title: "t".into(),
            app: "a".into(),
            pid: 1,
            rect: ON_SCREEN,
            monitor,
            minimized: false,
            foreground,
        }
    }

    #[test]
    fn a_parked_rectangle_means_minimized_even_when_isiconic_lies() {
        assert!(minimized_from(false, PARKED));
        assert!(minimized_from(true, ON_SCREEN));
        assert!(!minimized_from(false, ON_SCREEN));
    }

    #[test]
    fn a_monitor_left_of_the_primary_is_not_mistaken_for_a_parked_window() {
        // A real second monitor can start at -1920. The sentinel has to sit far past that.
        assert!(!minimized_from(false, (-1920, 0, -20, 1080)));
        const { assert!(ICONIC_RECT_EDGE < -20000) };
    }

    #[test]
    fn a_minimized_window_survives_the_size_filter() {
        // Its parked rectangle is 160x28: far under min_side, and meaningless.
        assert!(is_real_window(true, "Mail", 7, OWN, true, PARKED, 50));
    }

    #[test]
    fn a_small_window_is_dropped_but_only_when_it_is_not_minimized() {
        let tiny = (0, 0, 40, 40);
        assert!(!is_real_window(true, "tip", 7, OWN, false, tiny, 50));
        assert!(is_real_window(true, "tip", 7, OWN, true, tiny, 50));
    }

    #[test]
    fn min_side_is_exclusive_on_both_sides() {
        assert!(!is_real_window(
            true,
            "t",
            7,
            OWN,
            false,
            (0, 0, 50, 500),
            50
        ));
        assert!(!is_real_window(
            true,
            "t",
            7,
            OWN,
            false,
            (0, 0, 500, 50),
            50
        ));
        assert!(is_real_window(true, "t", 7, OWN, false, (0, 0, 51, 51), 50));
    }

    #[test]
    fn invisible_and_untitled_windows_are_not_windows() {
        assert!(!is_real_window(false, "Mail", 7, OWN, false, ON_SCREEN, 50));
        assert!(!is_real_window(true, "", 7, OWN, false, ON_SCREEN, 50));
    }

    #[test]
    fn our_own_windows_are_never_offered_as_targets() {
        // The panel and the overlay are ours; offering them made the program click itself.
        assert!(!is_real_window(
            true, "Clicker", OWN, OWN, false, ON_SCREEN, 50
        ));
        assert!(!is_real_window(true, "Clicker", OWN, OWN, true, PARKED, 50));
    }

    #[test]
    fn the_foreground_window_sorts_first_whatever_monitor_it_is_on() {
        let mut rows = [
            (0usize, info(0, false)),
            (1usize, info(2, true)),
            (2usize, info(1, false)),
        ];
        rows.sort_by_key(|(z, w)| order_key(w, *z));
        assert_eq!(
            rows.iter()
                .map(|(_, w)| (w.monitor, w.foreground))
                .collect::<Vec<_>>(),
            vec![(2, true), (0, false), (1, false)]
        );
    }

    #[test]
    fn within_a_monitor_the_order_is_z_order() {
        let mut rows = [(5usize, info(1, false)), (2usize, info(1, false))];
        rows.sort_by_key(|(z, w)| order_key(w, *z));
        assert_eq!(rows.iter().map(|(z, _)| *z).collect::<Vec<_>>(), vec![2, 5]);
    }

    #[test]
    fn an_executable_path_reduces_to_its_stem() {
        assert_eq!(executable_stem(r"C:\Windows\explorer.exe"), "explorer");
        assert_eq!(
            executable_stem(r"C:\Program Files\Chrome\chrome.exe"),
            "chrome"
        );
        assert_eq!(executable_stem("Code.exe"), "Code");
        assert_eq!(executable_stem(r"C:\bin\noext"), "noext");
        assert_eq!(executable_stem(""), "");
    }

    /// Live: enumerate the real desktop and print it, to compare against the Python list.
    #[test]
    #[ignore]
    fn live_window_list() {
        crate::display::declare_dpi_aware();
        let windows = open_windows(MIN_WINDOW_SIDE_PX);
        for w in &windows {
            let title: String = w.title.chars().take(40).collect();
            println!(
                "{} {} {} {} {}",
                w.hwnd, w.app, w.monitor, w.minimized, title
            );
        }
        assert!(!windows.is_empty(), "no windows found at all");
    }
}
