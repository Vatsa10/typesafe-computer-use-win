"""Hotkey specs, and the table that turns a WM_HOTKEY id back into a callback."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import hotkeys, windows


def test_a_spec_becomes_modifier_flags_and_a_virtual_key():
    mods, vk = hotkeys.parse_hotkey("ctrl+alt+space")
    assert mods == windows.MOD_CONTROL | windows.MOD_ALT
    assert vk == 0x20


def test_a_spec_is_case_and_space_insensitive():
    assert hotkeys.parse_hotkey(" CTRL + Alt + G ") == hotkeys.parse_hotkey("ctrl+alt+g")


def test_a_letter_or_digit_needs_no_table_entry():
    assert hotkeys.parse_hotkey("ctrl+alt+7")[1] == ord("7")


def test_an_unknown_key_is_refused_by_name():
    with pytest.raises(ValueError, match="frobnicate"):
        hotkeys.parse_hotkey("ctrl+frobnicate")


def test_a_spec_without_a_key_is_refused():
    with pytest.raises(ValueError, match="no key"):
        hotkeys.parse_hotkey("ctrl+alt")


def test_registered_hotkeys_dispatch_to_their_callback(monkeypatch):
    fired = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: True)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: fired.append("talk"))
    keys.add("quit", "ctrl+alt+q", lambda: fired.append("quit"))
    assert keys.register() == []
    keys.dispatch(keys.id_of("quit"))
    assert fired == ["quit"]


def test_a_hotkey_another_app_owns_is_reported_by_name(monkeypatch):
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: i != 2)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.add("taken", "ctrl+alt+p", lambda: None)
    assert keys.register() == ["taken"]


def test_an_unknown_hotkey_id_is_ignored_rather_than_raising():
    keys = hotkeys.Hotkeys()
    keys.dispatch(999)  # a stale WM_HOTKEY from a hotkey just unregistered


def test_stop_unregisters_everything_it_registered(monkeypatch):
    removed = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: True)
    monkeypatch.setattr(windows, "unregister_hotkey", removed.append)
    monkeypatch.setattr(windows, "post_quit_message", lambda tid: None)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.register()
    keys.stop()
    assert removed == [keys.id_of("talk")]


def test_serve_registers_before_it_pumps(monkeypatch):
    order = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: order.append("register") or True)
    monkeypatch.setattr(windows, "current_thread_id", lambda: 1)
    monkeypatch.setattr(windows, "pump_messages", lambda on_hotkey, stop: order.append("pump"))
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.serve(on_ready=lambda refused: order.append("ready"))
    assert order == ["register", "ready", "pump"], "the pump must start only once every key is claimed"


def test_serve_hands_on_ready_the_refused_names(monkeypatch):
    seen = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: i != 2)
    monkeypatch.setattr(windows, "current_thread_id", lambda: 1)
    monkeypatch.setattr(windows, "pump_messages", lambda on_hotkey, stop: None)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.add("taken", "ctrl+alt+p", lambda: None)
    keys.serve(on_ready=seen.append)
    assert seen == [["taken"]]


def test_serve_without_a_callback_still_registers_and_pumps(monkeypatch):
    pumped = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: True)
    monkeypatch.setattr(windows, "current_thread_id", lambda: 1)
    monkeypatch.setattr(windows, "pump_messages", lambda on_hotkey, stop: pumped.append(True))
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.serve()
    assert pumped == [True]


def test_stop_from_another_thread_ends_a_serving_pump(monkeypatch):
    import threading
    import time

    def pump(on_hotkey, stop):
        while not stop():
            time.sleep(0.01)

    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: True)
    monkeypatch.setattr(windows, "current_thread_id", lambda: 1)
    monkeypatch.setattr(windows, "unregister_hotkey", lambda i: None)
    monkeypatch.setattr(windows, "post_quit_message", lambda tid: None)
    monkeypatch.setattr(windows, "pump_messages", pump)
    ready = threading.Event()
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    worker = threading.Thread(target=lambda: keys.serve(on_ready=lambda r: ready.set()), daemon=True)
    worker.start()
    assert ready.wait(2.0)
    keys.stop()
    worker.join(2.0)
    assert not worker.is_alive()
