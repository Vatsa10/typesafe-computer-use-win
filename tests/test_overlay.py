"""The typed-goal overlay. Tk is injected, so no window is ever created in a test."""

from __future__ import annotations

import pytest

from typesafe_computer_use_win import overlay


class FakeEntry:
    def __init__(self, text):
        self.text = text
        self.bindings = {}

    def bind(self, sequence, handler):
        self.bindings[sequence] = handler

    def get(self):
        return self.text

    def pack(self, **kw):
        pass

    def focus_set(self):
        pass


class FakeRoot:
    def __init__(self, entry, submit_with="<Return>"):
        self.entry = entry
        self.submit_with = submit_with
        self.destroyed = False

    def title(self, *a):
        pass

    def attributes(self, *a):
        pass

    def overrideredirect(self, *a):
        pass

    def geometry(self, *a):
        pass

    def configure(self, **kw):
        pass

    def mainloop(self):
        handler = self.entry.bindings.get(self.submit_with)
        if handler is not None:
            handler(None)

    def destroy(self):
        self.destroyed = True

    def update_idletasks(self):
        pass

    def winfo_screenwidth(self):
        return 2560

    def winfo_screenheight(self):
        return 1440


def test_enter_returns_the_typed_goal():
    entry = FakeEntry("open the console")
    root = FakeRoot(entry)
    assert overlay.ask_for_goal(tk_factory=lambda: (root, entry)) == "open the console"
    assert root.destroyed, "the window must close after a submit"


def test_escape_returns_nothing():
    entry = FakeEntry("half a thought")
    root = FakeRoot(entry, submit_with="<Escape>")
    assert overlay.ask_for_goal(tk_factory=lambda: (root, entry)) is None


def test_an_empty_entry_counts_as_a_cancel():
    entry = FakeEntry("   ")
    root = FakeRoot(entry)
    assert overlay.ask_for_goal(tk_factory=lambda: (root, entry)) is None


# ------------------------------------------------------- the webview command bar


def test_the_bar_returns_what_was_typed():
    bar = overlay._Bar()
    bar.submit("  open the console  ")
    assert bar.wait(0.1) == "open the console"


def test_an_empty_submit_is_a_cancel():
    bar = overlay._Bar()
    bar.submit("   ")
    assert bar.wait(0.1) is None


def test_cancelling_returns_nothing():
    bar = overlay._Bar()
    bar.cancel()
    assert bar.wait(0.1) is None


def test_the_bar_does_not_wait_for_ever():
    """A bar left open would hold the input worker, and the hotkeys with it."""
    assert overlay._Bar().wait(0.05) is None


def test_the_page_is_offered_methods_and_nothing_else():
    """Same rule as the panel: pywebview walks public attributes, and an Event is not something to
    hand a browser."""
    assert {name for name in vars(overlay._Bar()) if not name.startswith("_")} == set()


def test_without_a_webview_loop_it_falls_back_rather_than_starting_one(monkeypatch):
    """A worker thread cannot start a GUI loop, so the headless daemon gets the Tk bar instead."""
    import sys
    from types import SimpleNamespace

    monkeypatch.setitem(sys.modules, "webview", SimpleNamespace(windows=[]))
    assert overlay._webview_bar("goal") == (False, None)


def test_a_running_webview_loop_is_used(monkeypatch):
    import sys
    from types import SimpleNamespace

    made = {}

    class FakeWindow:
        def destroy(self):
            made["destroyed"] = True

    def create_window(prompt, url, js_api, **kw):
        made["kw"] = kw
        js_api.submit("open youtube")  # the page answering
        return FakeWindow()

    monkeypatch.setitem(sys.modules, "webview", SimpleNamespace(windows=["panel"], create_window=create_window))
    assert overlay._webview_bar("goal") == (True, "open youtube")
    assert made["kw"]["frameless"] is True and made["kw"]["on_top"] is True
    assert made["destroyed"] is True, "the bar must not outlive the answer"


# ------------------------------------------------------- speaking into the bar


def test_the_bar_records_on_a_thread_so_the_page_stays_answerable(monkeypatch):
    """start_listening must return at once. If it recorded inline, the page could not call
    stop_listening, and the recording would never end."""
    import threading

    started = threading.Event()
    monkeypatch.setattr(overlay.voice, "record_until", lambda **kw: (started.set(), [b"\x10\x00"])[1])
    bar = overlay._Bar()
    assert bar.start_listening() is True
    assert started.wait(2.0), "the recording runs, just not on the caller's thread"


def test_stopping_transcribes_what_was_recorded(monkeypatch):
    monkeypatch.setattr(overlay.voice, "record_until", lambda **kw: [b"\x10\x00", b"\x20\x00"])
    monkeypatch.setattr(overlay.voice, "transcribe_frames", lambda frames, model: "open youtube")
    bar = overlay._Bar()
    bar.start_listening()
    assert bar.stop_listening() == "open youtube"


def test_a_recording_with_no_audio_transcribes_to_nothing(monkeypatch):
    monkeypatch.setattr(overlay.voice, "record_until", lambda **kw: [])
    monkeypatch.setattr(overlay.voice, "transcribe_frames", lambda frames, model: pytest.fail("nothing to transcribe"))
    bar = overlay._Bar()
    bar.start_listening()
    assert bar.stop_listening() == ""


def test_a_microphone_that_fails_does_not_take_the_bar_with_it(monkeypatch):
    def boom(**kw):
        raise RuntimeError("no input device")

    monkeypatch.setattr(overlay.voice, "record_until", boom)
    bar = overlay._Bar()
    assert bar.start_listening() is True
    assert bar.stop_listening() == ""


def test_the_bar_can_open_straight_into_listening(monkeypatch):
    monkeypatch.delenv("CLICKER_BAR_LISTEN", raising=False)
    assert overlay.listens_on_open() is True
    monkeypatch.setenv("CLICKER_BAR_LISTEN", "0")
    assert overlay.listens_on_open() is False


# ------------------------------------------------------------ where it appears


def test_the_bar_opens_under_the_mouse(monkeypatch):
    from dataclasses import dataclass

    @dataclass(frozen=True)
    class M:
        index: int
        left: int
        top: int
        right: int
        bottom: int
        primary: bool = False

    monkeypatch.setattr(overlay.windows, "mouse_location", lambda: (3000.0, 700.0))
    monkeypatch.setattr(overlay.windows, "monitors", lambda: [M(0, 0, 0, 2560, 1440, True), M(1, 2560, 0, 5120, 1440)])
    monkeypatch.setattr(overlay.windows, "monitor_at", lambda x, y: 1)
    left, top = overlay.at_cursor(width=680, height=96)
    assert 2560 <= left <= 5120 - 680, "it belongs on the monitor the mouse is on"
    assert top > 700, "and below the pointer rather than under it"


def test_the_bar_never_hangs_off_the_edge_of_a_display(monkeypatch):
    """Centring on the pointer near an edge would push half the bar onto the next monitor."""
    from dataclasses import dataclass

    @dataclass(frozen=True)
    class M:
        index: int
        left: int
        top: int
        right: int
        bottom: int
        primary: bool = True

    monkeypatch.setattr(overlay.windows, "mouse_location", lambda: (2550.0, 1430.0))  # bottom-right corner
    monkeypatch.setattr(overlay.windows, "monitors", lambda: [M(0, 0, 0, 2560, 1440)])
    monkeypatch.setattr(overlay.windows, "monitor_at", lambda x, y: 0)
    left, top = overlay.at_cursor(width=680, height=96)
    assert left + 680 <= 2560 and top + 96 <= 1440


def test_a_desktop_that_will_not_answer_still_gives_a_position(monkeypatch):
    def boom():
        raise OSError("no desktop")

    monkeypatch.setattr(overlay.windows, "mouse_location", boom)
    assert overlay.at_cursor() == (0, 0)
