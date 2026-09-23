"""The multi-monitor window inventory: which screens exist, what is open on them, and switching.

None of this may touch the real desktop, because CI has one virtual display and this machine has
three. The win32 layer is monkeypatched the way the other adapter tests do it, so what is under
test is the ordering, the filtering and the confirmation loop.
"""

from __future__ import annotations

import os
import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import windows

# A desktop shaped like the real machine: a primary in the middle, one screen left of it whose
# coordinates are negative, and one to the right that reaches past 5000.
LEFT = (-1920, -80, 0, 1000, False)
PRIMARY = (0, 0, 1920, 1080, True)
RIGHT = (1920, 0, 5112, 1800, False)


def fake_desktop(monkeypatch, screens=(RIGHT, LEFT, PRIMARY)):
    """Enumerate the given monitors in the given (deliberately unsorted) order."""
    handles = list(range(1, len(screens) + 1))
    monkeypatch.setattr(windows, "_monitor_handles", lambda: handles)
    monkeypatch.setattr(windows, "_monitor_info", lambda h: screens[h - 1])
    return screens


def test_monitors_put_the_primary_first_then_left_to_right(monkeypatch):
    fake_desktop(monkeypatch)
    found = windows.monitors()
    assert [m.bounds for m in found] == [(0, 0, 1920, 1080), (-1920, -80, 0, 1000), (1920, 0, 5112, 1800)]
    assert [m.index for m in found] == [0, 1, 2]
    assert [m.primary for m in found] == [True, False, False]


def test_a_monitor_left_of_the_primary_keeps_its_negative_origin(monkeypatch):
    fake_desktop(monkeypatch)
    left = windows.monitors()[1]
    assert (left.left, left.top) == (-1920, -80)
    assert (left.width, left.height) == (1920, 1080)


def test_monitor_at_finds_the_screen_under_a_point_on_each_one(monkeypatch):
    fake_desktop(monkeypatch)
    assert windows.monitor_at(960, 540) == 0
    assert windows.monitor_at(-11, 12) == 1
    assert windows.monitor_at(5000, 900) == 2


def test_monitor_at_falls_back_to_the_primary_for_a_point_on_no_screen(monkeypatch):
    fake_desktop(monkeypatch)
    assert windows.monitor_at(9999, 9999) == 0
    assert windows.monitor_at(-5000, -5000) == 0


def test_monitor_of_a_minimized_window_ignores_its_parked_rectangle(monkeypatch):
    """A minimized window sits at about (-32000, -32000), which is on no screen at all. Windows
    still knows where it belongs, so ask it rather than the rectangle."""
    fake_desktop(monkeypatch)
    monkeypatch.setattr(windows.user32, "MonitorFromWindow", lambda hwnd, flags: 2)  # handle 2 is the left screen
    assert windows.monitor_at(-32000, -32000) == 0
    assert windows.monitor_of(0xABC) == 1


def test_monitor_of_reports_the_primary_when_windows_names_no_screen(monkeypatch):
    fake_desktop(monkeypatch)
    monkeypatch.setattr(windows.user32, "MonitorFromWindow", lambda hwnd, flags: 0)
    assert windows.monitor_of(0xABC) == 0


# ------------------------------------------------------------------ open_windows

OWN_PID = 4242


def fake_windows(monkeypatch, rows, foreground=0):
    """Stand in for the desktop with a list of rows: hwnd, title, pid, rect, visible, iconic, monitor."""
    by_hwnd = {r["hwnd"]: r for r in rows}
    monkeypatch.setattr(windows, "_top_level_windows", lambda: [r["hwnd"] for r in rows])
    monkeypatch.setattr(windows, "_window_title", lambda h: by_hwnd[h]["title"])
    monkeypatch.setattr(windows, "_pid_of", lambda h: by_hwnd[h]["pid"])
    monkeypatch.setattr(windows, "_process_name", lambda pid: f"app{pid}")
    monkeypatch.setattr(windows, "_window_rect", lambda h: by_hwnd[h]["rect"])
    monkeypatch.setattr(windows, "monitor_of", lambda h: by_hwnd[h].get("monitor", 0))
    monkeypatch.setattr(windows, "_foreground_hwnd", lambda: foreground)
    monkeypatch.setattr(windows.user32, "IsWindowVisible", lambda h: int(by_hwnd[h].get("visible", True)))
    monkeypatch.setattr(windows.user32, "IsIconic", lambda h: int(by_hwnd[h].get("iconic", False)))
    monkeypatch.setattr(windows.kernel32, "GetCurrentProcessId", lambda: OWN_PID)


def row(hwnd, title="Window", pid=10, rect=(0, 0, 800, 600), **kw):
    return {"hwnd": hwnd, "title": title, "pid": pid, "rect": rect, **kw}


def test_open_windows_drops_untitled_invisible_tiny_and_own_process_windows(monkeypatch):
    fake_windows(
        monkeypatch,
        [
            row(1, title="Real window"),
            row(2, title=""),
            row(3, title="Hidden", visible=False),
            row(4, title="Tooltip", rect=(0, 0, 40, 40)),
            row(5, title="Control panel", pid=OWN_PID),
        ],
    )
    assert [w.hwnd for w in windows.open_windows()] == [1]


def test_open_windows_reports_what_the_window_is(monkeypatch):
    fake_windows(monkeypatch, [row(7, title="Inbox", pid=99, rect=(-11, 12, 900, 700), monitor=1)], foreground=7)
    (info,) = windows.open_windows()
    assert (info.hwnd, info.title, info.app, info.pid) == (7, "Inbox", "app99", 99)
    assert info.rect == (-11, 12, 900, 700)
    assert (info.monitor, info.minimized, info.foreground) == (1, False, True)


def test_a_minimized_window_is_kept_and_marked(monkeypatch):
    fake_windows(monkeypatch, [row(8, title="Notes", rect=(-32000, -32000, -31840, -31972), iconic=True, monitor=2)])
    (info,) = windows.open_windows()
    assert info.minimized is True
    assert info.monitor == 2, "a parked rectangle must not decide the monitor"


def test_open_windows_puts_the_foreground_first_then_orders_by_monitor_then_z(monkeypatch):
    fake_windows(
        monkeypatch,
        [
            row(1, title="Right A", monitor=2),
            row(2, title="Primary A", monitor=0),
            row(3, title="Middle", monitor=1),
            row(4, title="Primary B", monitor=0),
            row(5, title="Front", monitor=2),
        ],
        foreground=5,
    )
    assert [w.hwnd for w in windows.open_windows()] == [5, 2, 4, 3, 1]


def test_min_side_is_the_callers_to_choose(monkeypatch):
    fake_windows(monkeypatch, [row(1, title="Small", rect=(0, 0, 60, 60))])
    assert [w.hwnd for w in windows.open_windows()] == [1]
    assert windows.open_windows(min_side=100) == []


# ------------------------------------------------------------------ activating and capturing


def test_activate_window_raises_then_confirms_the_foreground_moved(monkeypatch):
    raised = []
    monkeypatch.setattr(windows, "_raise", raised.append)
    monkeypatch.setattr(windows, "_foreground_hwnd", lambda: 77)
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    assert windows.activate_window(77) is True
    assert raised == [77], "_raise already does the AttachThreadInput dance, so it must be reused"


def test_activate_window_is_false_when_the_foreground_never_changes(monkeypatch):
    monkeypatch.setattr(windows, "_raise", lambda hwnd: None)
    monkeypatch.setattr(windows, "_foreground_hwnd", lambda: 1)
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    ticks = iter([0.0, 0.0, 1.0, 2.0, 9.0])
    monkeypatch.setattr(windows.time, "monotonic", lambda: next(ticks, 99.0))
    assert windows.activate_window(77, timeout=0.3) is False


class _Grab:
    def convert(self, mode):
        return self


def test_screenshot_with_no_argument_still_grabs_the_primary_display(monkeypatch):
    calls = []
    monkeypatch.setattr(windows.ImageGrab, "grab", lambda **kw: calls.append(kw) or _Grab())
    windows.screenshot()
    assert calls == [{"all_screens": False}]


def test_screenshot_with_bounds_grabs_that_rectangle_of_the_virtual_desktop(monkeypatch):
    calls = []
    monkeypatch.setattr(windows.ImageGrab, "grab", lambda **kw: calls.append(kw) or _Grab())
    windows.screenshot((-1920, -80, 0, 1000))
    assert calls == [{"bbox": (-1920, -80, 0, 1000), "all_screens": True}]


# ------------------------------------------------------------------ the real desktop
#
# CI has one virtual display, so this runs only where someone asked for it: set
# CLICKER_REAL_DESKTOP_TESTS=1 locally.

_REAL = pytest.mark.skipif(
    not os.environ.get("CLICKER_REAL_DESKTOP_TESTS"),
    reason="needs a real desktop; set CLICKER_REAL_DESKTOP_TESTS=1",
)


@_REAL
def test_the_real_desktop_enumerates_and_agrees_with_itself():
    screens = windows.monitors()
    assert screens, "a running desktop has at least one monitor"
    assert screens[0].primary
    assert [m.index for m in screens] == list(range(len(screens)))
    assert all(m.width > 0 and m.height > 0 for m in screens)
    for monitor in screens:
        centre = (monitor.left + monitor.width / 2, monitor.top + monitor.height / 2)
        assert windows.monitor_at(*centre) == monitor.index
    for info in windows.open_windows():
        assert info.title
        assert 0 <= info.monitor < len(screens)
        assert info.pid != os.getpid()
