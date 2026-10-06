"""Point at things on the screen instead of clicking them.

The loop's other mode drives the machine. This one draws on it: a transparent window over the
desktop with a box around the control that matters and a label saying what it is for. Showing
somebody the button teaches them where it is, and unlike pressing it, it cannot press the wrong
one — which this program has done, to a Like button in a window nobody was looking at.

The window is click-through by construction. `WS_EX_TRANSPARENT` makes the desktop underneath
receive every click, `WS_EX_NOACTIVATE` keeps it from stealing focus from whatever is being
explained, and `WS_EX_TOOLWINDOW` keeps it out of alt-tab. Without those three it would be a pane
of glass nailed over the thing it is trying to point at.
"""

from __future__ import annotations

import atexit
import contextlib
import threading
import tkinter as tk
from dataclasses import dataclass

from . import windows

MARGIN = 28  # room around the marks for a label that sits above the box
DEFAULT_SECONDS = 5.0
MAX_MARKS = 6  # more than a handful of boxes stops being a hint and becomes a diagram
TITLE = "winclicker-highlight"


@dataclass(frozen=True)
class Mark:
    """One thing worth pointing at, in virtual-desktop points — the same coordinates a click uses."""

    x: float
    y: float
    w: float
    h: float
    label: str = ""
    tone: str = "point"  # point (the thing to use) | note (context worth seeing)


def bounds_of(marks: list[Mark]) -> tuple[int, int, int, int]:
    """The rectangle the overlay has to cover: every mark, plus room for labels above them."""
    left = min(m.x for m in marks) - MARGIN
    top = min(m.y for m in marks) - MARGIN * 2  # labels sit above their box
    right = max(m.x + m.w for m in marks) + MARGIN
    bottom = max(m.y + m.h for m in marks) + MARGIN
    return int(left), int(top), int(right), int(bottom)


def clamped_bounds(marks: list[Mark]) -> tuple[int, int, int, int]:
    """The same rectangle, kept on the monitors that exist.

    A control can report a frame that runs past the edge of its display, and a window placed off
    the desktop is a window nobody sees.
    """
    left, top, right, bottom = bounds_of(marks)
    try:
        screens = windows.monitors()
    except Exception:
        return left, top, right, bottom
    if not screens:
        return left, top, right, bottom
    index = windows.monitor_at((left + right) / 2, (top + bottom) / 2)
    screen = screens[index] if 0 <= index < len(screens) else screens[0]
    left = max(left, screen.left)
    top = max(top, screen.top)
    right = min(right, screen.right)
    bottom = min(bottom, screen.bottom)
    return left, top, max(right, left + 1), max(bottom, top + 1)


def placed(marks: list[Mark], origin: tuple[int, int]) -> list[dict]:
    """The marks as the overlay draws them: relative to its own corner rather than the desktop."""
    left, top = origin
    return [
        {
            "x": round(m.x - left),
            "y": round(m.y - top),
            "w": round(m.w),
            "h": round(m.h),
            "label": m.label[:80],
            "tone": "note" if m.tone == "note" else "point",
        }
        for m in marks[:MAX_MARKS]
    ]


# The colour the window paints where it should not exist. Tk on Windows takes one colour as fully
# transparent, and those pixels also stop receiving clicks, which is what makes this an overlay
# rather than a pane of glass over the thing it is pointing at. A colour nothing else would draw.
CHROMA = "#ff00ff"
POINT, NOTE, INK, PANEL = "#38bdf8", "#fbbf24", "#0b1120", "#7dd3fc"
BORDER = 3
LABEL_FONT = ("Cascadia Mono", 9, "bold")


def draw(canvas, boxes: list[dict]) -> None:
    """Put the boxes and their labels on the canvas. Pure drawing, so it can be tested on a fake."""
    for box in boxes:
        colour = NOTE if box["tone"] == "note" else POINT
        x, y, w, h = box["x"], box["y"], box["w"], box["h"]
        canvas.create_rectangle(x, y, x + w, y + h, outline=colour, width=BORDER, dash=(6, 4) if box["tone"] == "note" else ())
        if not box["label"]:
            continue
        # Above the box when there is room, otherwise just inside it: a label off the top of the
        # overlay is a label nobody reads.
        label_y = y - 13 if y > 24 else y + 13
        text = canvas.create_text(x + 8, label_y, text=box["label"], fill=PANEL, font=LABEL_FONT, anchor="w")
        left, top, right, bottom = canvas.bbox(text)
        plate = canvas.create_rectangle(left - 7, top - 4, right + 7, bottom + 4, fill=INK, outline=colour)
        canvas.tag_raise(text, plate)


class Overlay:
    """The drawing window. One at a time: a second set of boxes replaces the first."""

    def __init__(self) -> None:
        self._thread: threading.Thread | None = None
        self._close = threading.Event()
        self._lock = threading.Lock()

    def show(self, marks: list[Mark], seconds: float = DEFAULT_SECONDS) -> bool:
        """Draw the marks over the desktop. False when there is nothing to draw."""
        if not marks:
            return False
        left, top, right, bottom = clamped_bounds(marks)
        boxes = placed(marks, (left, top))
        with self._lock:
            self._stop_locked()
            self._close = threading.Event()
            closing = self._close
            self._thread = threading.Thread(
                target=self._run,
                args=(boxes, (left, top, right - left, bottom - top), seconds, closing),
                name="highlight",
                daemon=True,
            )
            self._thread.start()
        return True

    def _run(self, boxes, geometry, seconds, closing) -> None:
        """The window lives entirely on this thread: Tk objects belong to the thread that made them."""
        left, top, width, height = geometry
        try:
            root = tk.Tk()
            root.overrideredirect(True)
            root.attributes("-topmost", True)
            root.attributes("-transparentcolor", CHROMA)
            root.configure(bg=CHROMA)
            root.geometry(f"{max(width, 1)}x{max(height, 1)}+{left}+{top}")
            canvas = tk.Canvas(root, bg=CHROMA, highlightthickness=0, borderwidth=0)
            canvas.pack(fill="both", expand=True)
            draw(canvas, boxes)
            root.title(TITLE)
            root.update_idletasks()
            windows.make_click_through(TITLE)
            deadline = root.after(int(seconds * 1000), root.destroy)

            def watch() -> None:
                if closing.is_set():
                    with contextlib.suppress(Exception):
                        root.after_cancel(deadline)
                    root.destroy()
                    return
                root.after(100, watch)

            root.after(100, watch)
            root.mainloop()
        except Exception:  # a locked session, no display, a Tk that will not start
            return

    def clear(self) -> None:
        with self._lock:
            self._stop_locked()

    def _stop_locked(self) -> None:
        self._close.set()
        if self._thread is not None and self._thread.is_alive():
            self._thread.join(timeout=2.0)
        self._thread = None


_overlay = Overlay()


def show(marks: list[Mark], seconds: float = DEFAULT_SECONDS) -> bool:
    return _overlay.show(marks, seconds)


def clear() -> None:
    _overlay.clear()


# Tcl objects belong to the thread that made them, and an interpreter still alive at shutdown is
# torn down from the main thread, which Tcl complains about loudly. Closing first avoids it.
atexit.register(clear)
