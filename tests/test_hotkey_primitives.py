"""The win32 primitives the hotkey manager sits on."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import windows


def test_modifier_flags_match_the_win32_values():
    assert (windows.MOD_ALT, windows.MOD_CONTROL, windows.MOD_SHIFT, windows.MOD_WIN) == (1, 2, 4, 8)
    assert windows.MOD_NOREPEAT == 0x4000


def test_sleep_watching_polls_the_check_it_is_given(monkeypatch):
    calls = []
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    ticks = iter([0.0, 0.05, 0.2])
    monkeypatch.setattr(windows.time, "monotonic", lambda: next(ticks, 99.0))
    windows.sleep_watching(0.1, check=lambda: calls.append("checked"))
    assert calls, "the caller's check must be polled while waiting"


def test_sleep_watching_defaults_to_the_corner_escape_hatch(monkeypatch):
    seen = []
    monkeypatch.setattr(windows, "check_abort", lambda: seen.append("corner"))
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    ticks = iter([0.0, 0.05, 0.2])
    monkeypatch.setattr(windows.time, "monotonic", lambda: next(ticks, 99.0))
    windows.sleep_watching(0.1)
    assert seen == ["corner"]


def test_a_refused_registration_reports_false_rather_than_raising(monkeypatch):
    monkeypatch.setattr(windows.user32, "RegisterHotKey", lambda hwnd, i, m, k: 0)
    assert windows.register_hotkey(1, windows.MOD_ALT, 0x20) is False


def test_pump_stops_when_the_stop_callback_says_so(monkeypatch):
    monkeypatch.setattr(windows.user32, "PeekMessageW", lambda *a: 0)
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    stops = iter([False, False, True])
    windows.pump_messages(on_hotkey=lambda i: None, stop=lambda: next(stops))
    # returning at all is the assertion: an unstoppable pump hangs the test
