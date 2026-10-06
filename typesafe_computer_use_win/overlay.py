"""The command bar: one hotkey, speak or type, and it runs.

Two implementations of one thing, because the program runs two ways. With the panel open there is
already a webview loop on the main thread, so the bar is a second window in it and looks like
everything else. The headless daemon has no loop and cannot start one from a worker thread, so it
falls back to Tk, which ships with Python.

Where it appears matters more than it sounds: the bar opens at the mouse, on whichever of the three
monitors the mouse is on. A bar that always opened on the primary display would be somewhere else
entirely from the thing the user is looking at.

The caller blocks either way: the input worker asks for a goal and waits for one.
"""

from __future__ import annotations

import os
import threading
from collections.abc import Callable
from pathlib import Path

from . import config, voice, windows

PANEL = Path(__file__).parent / "panel"
WIDTH, HEIGHT = 680, 96
WAIT_SECONDS = 300.0  # a bar left open holds the input worker, and the hotkeys with it
MARGIN = 24  # keeps the bar off the very edge of a display

# Tk fallback colours, kept close to the panel so the two do not look like different programs.
BACKGROUND = "#111c30"
FOREGROUND = "#f8fafc"


def listens_on_open() -> bool:
    """Whether the bar starts recording the moment it opens.

    On by default, because the point of the hotkey is to press it, say the thing and be done. Set
    CLICKER_BAR_LISTEN=0 to open into the text box instead.
    """
    return os.environ.get("CLICKER_BAR_LISTEN", "1").strip().lower() not in {"0", "false", "no", "off"}


def at_cursor(width: int = WIDTH, height: int = HEIGHT) -> tuple[int, int]:
    """Where to put the bar: under the mouse, clamped to the display the mouse is on.

    Clamping is the part that matters on a multi-monitor desk. Centring the bar on the pointer
    near a screen edge would push half of it onto the next monitor, or off the desktop entirely.
    """
    try:
        x, y = windows.mouse_location()
        screens = windows.monitors()
        index = windows.monitor_at(x, y)
        screen = screens[index] if 0 <= index < len(screens) else None
    except Exception:  # no desktop to ask, which is only ever a test or a locked session
        return 0, 0
    left = int(x) - width // 2
    top = int(y) + 28  # just below the pointer, not under it
    if screen is None:
        return max(0, left), max(0, top)
    left = min(max(left, screen.left + MARGIN), screen.right - width - MARGIN)
    top = min(max(top, screen.top + MARGIN), screen.bottom - height - MARGIN)
    return int(left), int(top)


class _Bar:
    """What the bar's page can call.

    Private attributes only: pywebview builds the page's JS proxy by walking the public ones, and
    an Event is not something to hand a browser.
    """

    def __init__(self, listen_now: bool = True) -> None:
        self._done = threading.Event()
        self._text: str | None = None
        self._listen_on_open = listen_now
        self._recording = threading.Event()
        self._frames: list[bytes] = []
        self._capture: threading.Thread | None = None

    # ------------------------------------------------------------------ typed

    def submit(self, value: str) -> None:
        self._text = (value or "").strip() or None
        self._done.set()

    def cancel(self, _value: str = "") -> None:
        self._text = None
        self._done.set()

    def wait(self, seconds: float = WAIT_SECONDS) -> str | None:
        self._done.wait(seconds)
        self._recording.clear()
        return self._text

    # ------------------------------------------------------------------ spoken

    def listen_on_open(self) -> bool:
        return self._listen_on_open

    def start_listening(self) -> bool:
        """Begin recording on a thread of its own, so the page stays answerable while it runs."""
        if self._recording.is_set():
            return True
        self._frames = []
        self._recording.set()

        def capture() -> None:
            try:
                self._frames = voice.record_until(
                    stop=lambda: not self._recording.is_set(),
                    max_seconds=config.voice_max_seconds(),
                    silence_seconds=config.bar_silence_seconds(),
                )
            except Exception:
                self._frames = []
            finally:
                self._recording.clear()

        self._capture = threading.Thread(target=capture, name="bar-recording", daemon=True)
        self._capture.start()
        return True

    def stop_listening(self) -> str:
        """Stop, transcribe, and hand the page what was said. Empty means nothing usable."""
        self._recording.clear()
        if self._capture is not None:
            self._capture.join(timeout=5.0)
        if not self._frames:
            return ""
        try:
            return voice.transcribe_frames(self._frames, config.whisper_model())
        except Exception:
            return ""


def _webview_bar(prompt: str) -> tuple[bool, str | None]:
    """The bar as a frameless window in the panel's own webview loop.

    Returns (handled, goal). Not handled means no loop is running — the daemon without a panel —
    and the caller falls back rather than trying to start a GUI from a worker thread, which is not
    a thing a webview will do.
    """
    try:
        import webview
    except ImportError:
        return False, None
    if not getattr(webview, "windows", None):
        return False, None
    bar = _Bar(listen_now=listens_on_open())
    left, top = at_cursor()
    window = webview.create_window(
        prompt,
        str(PANEL / "overlay.html"),
        js_api=bar,
        frameless=True,
        easy_drag=True,
        on_top=True,
        width=WIDTH,
        height=HEIGHT,
        x=left,
        y=top,
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
    left, top = at_cursor()
    root.geometry(f"{WIDTH}x{HEIGHT}+{left}+{top}")
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
