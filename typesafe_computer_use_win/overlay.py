"""A command bar for typing a goal without leaving whatever app is in front.

Two implementations of one thing, and the reason is the two ways this program runs. With the panel
open there is already a webview loop on the main thread, so the bar is a second window in it and
matches everything else. The headless daemon has no such loop and cannot start one from a worker
thread, so it falls back to Tk, which ships with Python and needs no loop of its own.

The caller blocks either way: the input worker asks for a goal and waits for one.
"""

from __future__ import annotations

import threading
from collections.abc import Callable
from pathlib import Path

PANEL = Path(__file__).parent / "panel"
WIDTH, HEIGHT = 680, 96
WAIT_SECONDS = 180.0  # a bar left open forever would hold the input worker, and with it the hotkeys

# Tk fallback colours, kept close to the panel so the two do not look like different programs.
BACKGROUND = "#111c30"
FOREGROUND = "#f8fafc"


class _Bar:
    """What the bar's page can call. Private attributes only: pywebview builds the page's JS proxy
    by walking the public ones, and an Event is not something to hand a browser."""

    def __init__(self) -> None:
        self._done = threading.Event()
        self._text: str | None = None

    def submit(self, value: str) -> None:
        self._text = (value or "").strip() or None
        self._done.set()

    def cancel(self, _value: str = "") -> None:
        self._text = None
        self._done.set()

    def wait(self, seconds: float = WAIT_SECONDS) -> str | None:
        self._done.wait(seconds)
        return self._text


def _webview_bar(prompt: str) -> tuple[bool, str | None]:
    """The bar as a frameless window in the panel's own webview loop.

    Returns (handled, goal). Not handled means no loop is running — the daemon without a panel —
    and the caller should fall back rather than trying to start a second GUI from a worker thread,
    which is not a thing a webview will do.
    """
    try:
        import webview
    except ImportError:
        return False, None
    if not getattr(webview, "windows", None):
        return False, None
    bar = _Bar()
    window = webview.create_window(
        prompt,
        str(PANEL / "overlay.html"),
        js_api=bar,
        frameless=True,
        easy_drag=True,
        on_top=True,
        width=WIDTH,
        height=HEIGHT,
        background_color="#0b1120",
    )
    try:
        return True, bar.wait()
    finally:
        window.destroy()


def _default_factory(prompt: str):
    import tkinter as tk

    root = tk.Tk()
    root.title(prompt)
    root.overrideredirect(True)  # no title bar: this is a command bar, not a window to manage
    root.attributes("-topmost", True)
    root.configure(bg=BACKGROUND)
    entry = tk.Entry(root, font=("Segoe UI", 15), bg=BACKGROUND, fg=FOREGROUND, insertbackground=FOREGROUND, relief="flat")
    entry.pack(fill="both", expand=True, padx=16, pady=14)
    entry.focus_set()
    root.update_idletasks()
    x = (root.winfo_screenwidth() - WIDTH) // 2
    y = (root.winfo_screenheight() - HEIGHT) // 3
    root.geometry(f"{WIDTH}x{HEIGHT}+{x}+{y}")
    return root, entry


def ask_for_goal(prompt: str = "goal", tk_factory: Callable[[], tuple] | None = None) -> str | None:
    """Show the bar and block until the user submits or cancels. None means nothing to run."""
    if tk_factory is None:
        handled, goal = _webview_bar(prompt)
        if handled:
            return goal
    root, entry = _default_factory(prompt) if tk_factory is None else tk_factory()
    typed: list[str] = []

    def submit(_event=None):
        typed.append(entry.get())
        root.destroy()

    def cancel(_event=None):
        root.destroy()

    entry.bind("<Return>", submit)
    entry.bind("<Escape>", cancel)
    root.mainloop()
    text = typed[0].strip() if typed else ""
    return text or None
