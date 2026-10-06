"""The typed-goal overlay. Tk is injected, so no window is ever created in a test."""

from __future__ import annotations

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
