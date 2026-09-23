"""What is open right now, across every monitor.

The loop used to know exactly one thing about the machine: whichever window happened to be in
front. Everything else it had to infer from pixels, so "open whatsapp" while WhatsApp sat on the
second monitor read as "nothing on this screen helps" — and the honest fix is not better OCR, it
is telling the classifier what is already running.

This module turns the window inventory into two things: records for the state packet, and the
criteria for a `window` question. It takes the inventory as an argument rather than fetching it,
so the ranking rules are testable without a desktop.
"""

from __future__ import annotations

MAX_WINDOWS = 18  # options compete for probability mass, so this is a budget, not a limit
TITLE_CHARS = 70

# Windows that exist on every desktop and are never what someone means by "switch to". Program
# Manager is the desktop itself; the shell experience host and the input host are chrome.
SHELL_TITLES = frozenset({"program manager", "windows input experience", "windows shell experience host"})

# A packaged app is hosted inside ApplicationFrameHost, so Settings and the like show up twice:
# once as the app, once as the frame around it. The frame is the duplicate worth dropping, but only
# when the real one is also listed -- for some apps the frame is all there is.
FRAME_HOST = "applicationframehost"


def interesting(window) -> bool:
    """Whether a window is a plausible thing to switch to.

    The inventory already drops untitled, invisible and tiny windows. What is left to reject is
    the shell's own furniture, which is always open and never meant.
    """
    return window.title.strip().lower() not in SHELL_TITLES


def drop_frame_duplicates(windows) -> list:
    """Remove the ApplicationFrameHost copy of a window that is already listed under its own app.

    Offering both halves of one window wastes an option and splits the probability between two
    answers that do the same thing, which reads as doubt and can stop a run.
    """
    real = {w.title.strip().lower() for w in windows if w.app.lower() != FRAME_HOST}
    return [w for w in windows if w.app.lower() != FRAME_HOST or w.title.strip().lower() not in real]


def rank(windows, limit: int = MAX_WINDOWS) -> list:
    """The windows worth offering, best first.

    The foreground window comes first because it is the one being worked in, then the rest in the
    order the inventory gave them, which is already monitor then z-order. A minimized window sorts
    last: it is reachable, but it is not what someone means when several copies of a thing are
    open.
    """
    kept = [w for w in windows if interesting(w)]
    kept = drop_frame_duplicates(kept)
    kept.sort(key=lambda w: (not w.foreground, w.minimized, w.monitor))
    return kept[:limit]


def summary(window) -> str:
    """One window as a line a classifier can reason about: the app, its title, and where it is."""
    title = window.title.strip()
    if len(title) > TITLE_CHARS:
        title = title[: TITLE_CHARS - 1].rstrip() + "…"
    where = f"monitor {window.monitor + 1}"
    state = " (minimized)" if window.minimized else (" (in front)" if window.foreground else "")
    return f"{window.app}: {title!r} on {where}{state}"


def records(windows) -> list[dict]:
    """The inventory as state. Keyed by position, which is what the window question answers with."""
    return [
        {
            "k": i,
            "app": w.app,
            "title": w.title[:TITLE_CHARS],
            "monitor": w.monitor + 1,
            "minimized": w.minimized,
            "in_front": w.foreground,
        }
        for i, w in enumerate(windows)
    ]


def criteria(windows) -> dict[str, str]:
    """The window question's options, one per open window."""
    return {str(i): summary(w) for i, w in enumerate(windows)}


def monitor_summary(monitors) -> list[dict]:
    """The displays themselves, so the classifier knows how much machine there is. Only the
    captured one is ever read, which is why a run says which that was."""
    return [{"monitor": m.index + 1, "size": f"{m.width}x{m.height}", "primary": m.primary} for m in monitors]


def already_open(windows, name: str) -> object | None:
    """The open window that best matches a name, or None.

    Used to answer the question the loop could never answer before: is the thing being asked for
    already running somewhere? A match on the process name is stronger than one in a title, since
    a title mentions all sorts of things.
    """
    wanted = name.strip().lower()
    if not wanted:
        return None
    for window in windows:
        if window.app.lower() == wanted:
            return window
    for window in windows:
        if wanted in window.app.lower():
            return window
    for window in windows:
        if wanted in window.title.lower():
            return window
    return None
