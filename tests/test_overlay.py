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
