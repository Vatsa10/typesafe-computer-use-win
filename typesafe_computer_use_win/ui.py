"""The control panel: a WebView2 window driven by the daemon behind it.

Tkinter drew this before, and the ceiling was obvious — no rounded corners, no shadows, no icons,
and a feed that could only be one colour. WebView2 ships with Windows, so an HTML panel costs one
pure-Python dependency and renders properly. It is still a desktop window, not a browser tab,
which matters here: a dashboard living in the browser this program drives would be read by its own
OCR and could be clicked by its own run.

Two rules shape the code:

- The page polls, Python never pushes. The run loop logs from a worker thread, and a poll is the
  one shape that cannot race the WebView2 message pump: the page asks when it is ready and gets
  whatever has queued up since last time.
- The window hides the instant a job is queued, not when a poll notices the run started. The old
  panel hid on a 100 ms timer and lost that race, so the first capture of a run read the panel's
  own buttons and offered them as things to click.
"""

from __future__ import annotations

import base64
import queue
from pathlib import Path

from . import config, dotenv_io, runs_index
from . import daemon as daemon_module

PANEL = Path(__file__).parent / "panel"
DOTENV = Path.cwd() / ".env"
RUNS = Path.cwd() / "runs"
WINDOW = {"width": 1120, "height": 780, "min_size": (900, 600)}
MAX_QUEUED_LINES = 4000  # a long run logs a lot; the page only ever shows the tail

# What the settings tab edits. API keys are deliberately absent: a panel should never display or
# write a secret, and these are the knobs anyone actually turns.
SETTINGS: list[tuple[str, str, str]] = [
    ("CLICKER_BROWSER", "Browser", config.DEFAULT_BROWSER),
    ("CLICKER_EMAIL", "Email for type_email", "unset: the action is not offered"),
    ("CLICKER_HOTKEY_TALK", "Hotkey: push to talk", config.DEFAULT_HOTKEYS["talk"]),
    ("CLICKER_HOTKEY_GOAL", "Hotkey: typed goal", config.DEFAULT_HOTKEYS["goal"]),
    ("CLICKER_HOTKEY_PAUSE", "Hotkey: pause or resume", config.DEFAULT_HOTKEYS["pause"]),
    ("CLICKER_HOTKEY_ABORT", "Hotkey: abort the run", config.DEFAULT_HOTKEYS["abort"]),
    ("CLICKER_HOTKEY_QUIT", "Hotkey: quit", config.DEFAULT_HOTKEYS["quit"]),
    ("CLICKER_WHISPER_MODEL", "Whisper model", config.DEFAULT_WHISPER_MODEL),
    ("CLICKER_VOICE_MAX_SECONDS", "Longest utterance (s)", str(config.DEFAULT_VOICE_MAX_SECONDS)),
    ("CLICKER_VOICE_MIN_CONFIDENCE", "Voice confidence floor", str(config.DEFAULT_VOICE_MIN_CONFIDENCE)),
    ("CLICKER_CATALOG", "Site catalog from browser", "1 (0 = pinned sites only)"),
    ("CLICKER_CATALOG_TITLES", "Page titles as labels", "1 (0 = bare domains)"),
    ("CLICKER_CATALOG_LIMIT", "Sites offered per goal", str(config.DEFAULT_CATALOG_LIMIT)),
    ("CLICKER_OCR_ENGINE", "OCR engine", "windows"),
    ("CLICKER_OCR_LANGUAGE", "OCR language", "en-US"),
    ("CLICKER_WRITER_PROVIDER", "Writer provider", "openai when its key is set"),
    ("CLICKER_WRITER_MODEL", "Writer model", config.DEFAULT_OPENAI_WRITER_MODEL),
    ("CLICKER_ANSWER_MODEL", "Answer model", config.DEFAULT_OPENAI_ANSWER_MODEL),
]


def hotkey_hint() -> str:
    """The bindings, for the corner of the title bar. The window is optional; these are not."""
    return "   ".join(f"{config.hotkey(name)} {name}" for name in ("talk", "goal", "pause", "abort"))


def line_queue(messages: queue.Queue, limit: int = MAX_QUEUED_LINES) -> list[str]:
    """Everything said since the last poll. Bounded, because a page that was closed for a minute
    must not be handed a minute of backlog in one frame."""
    drained: list[str] = []
    while len(drained) < limit:
        try:
            drained.append(str(messages.get_nowait()))
        except queue.Empty:
            break
    return drained


def settings_rows(current: dict[str, str]) -> list[dict]:
    return [
        {"key": key, "label": label, "fallback": fallback, "value": current.get(key, "")} for key, label, fallback in SETTINGS
    ]


def run_rows(runs) -> list[dict]:
    return [
        {
            "name": run.name,
            "goal": run.goal,
            "outcome": run.outcome,
            "answer": run.answer,
            "goal_achieved": run.goal_achieved,
            "seconds": run.seconds,
            "steps_taken": run.steps_taken,
            "acted": run.acted,
        }
        for run in runs
    ]


def step_rows(steps) -> list[dict]:
    return [{"number": step.number, "has_shot": bool(step.annotated or step.raw)} for step in steps]


def data_url(path: Path | None) -> str:
    """A capture as something an <img> can show. The panel is local and the files are already on
    this disk, so inlining avoids standing up a file server just to look at a screenshot."""
    if path is None or not path.is_file():
        return ""
    return "data:image/png;base64," + base64.b64encode(path.read_bytes()).decode()


class Api:
    """What the page can call. Every method is reached from the WebView2 thread, so nothing here
    may block: the slow things go to the daemon's own workers."""

    def __init__(self, service, messages: queue.Queue) -> None:
        # Every one of these is private, and that is load-bearing rather than tidiness: pywebview
        # builds the page's JS proxy by walking this object's public attributes, so a public
        # reference to anything holding a native handle, a lock or a thread either serialises
        # something it should not or recurses until the stack ends. Only the methods are the API.
        self._service = service
        self._daemon = service.daemon
        self._messages = messages
        self._window = None
        self._hide_while_acting = True
        self._hidden = False

    # ------------------------------------------------------------------ run

    def attach(self, window) -> None:
        """Hold the native window. Private on purpose: see _window."""
        self._window = window

    def poll(self) -> dict:
        running = self._daemon.running
        if not running and self._hidden:
            self._show()
        return {
            "lines": line_queue(self._messages),
            "running": running,
            "paused": self._daemon.control.paused,
            "hotkeys": hotkey_hint(),
        }

    def start(self, goal: str) -> bool:
        goal = (goal or "").strip()
        if not goal:
            return False
        self._hide_for_run()  # before the job exists, so the first capture cannot include this window
        self._daemon.queue_goal(goal)
        return True

    def pause(self) -> bool:
        self._daemon.on_pause()
        return self._daemon.control.paused

    def abort(self) -> bool:
        self._daemon.on_abort()
        return True

    def test_voice(self) -> bool:
        """Hear one line and report what it was taken for, without running it. Recording blocks for
        as long as the key is held, so it goes to the input worker rather than this thread."""
        if not self._daemon.voice_enabled:
            self._messages.put("voice is off")
            return False
        self._messages.put("test: hold the talk hotkey and say something")
        self._service.post(self._daemon.probe_voice)
        return True

    def set_mode(self, act: bool, voice: bool, hide: bool) -> dict:
        self._daemon.act = bool(act)
        self._daemon.voice_enabled = bool(voice)
        self._hide_while_acting = bool(hide)
        return {"act": self._daemon.act, "voice": self._daemon.voice_enabled, "hide": self._hide_while_acting}

    # -------------------------------------------------------------- history

    def runs(self) -> list[dict]:
        return run_rows(runs_index.list_runs(RUNS))

    def steps(self, name: str) -> list[dict]:
        return step_rows(runs_index.steps_of(RUNS / name))

    def shot(self, name: str, number: int) -> str:
        for step in runs_index.steps_of(RUNS / name):
            if step.number == number:
                return data_url(step.annotated or step.raw)
        return ""

    # ------------------------------------------------------------- settings

    def settings(self) -> list[dict]:
        return settings_rows(dotenv_io.read_env(DOTENV))

    def save_settings(self, values: dict) -> bool:
        wanted = {key for key, _, _ in SETTINGS}
        dotenv_io.write_env(DOTENV, {k: str(v).strip() for k, v in values.items() if k in wanted})
        return True

    # --------------------------------------------------------- the window

    def _hide_for_run(self) -> None:
        """Get out of the way before the run looks at the screen.

        This is a correctness fix, not a courtesy. The panel is on screen, so OCR reads its buttons
        and the accessibility tree lists them; a run that captured while it was visible was offered
        its own controls as things to click, and took them.
        """
        if self._window is not None and self._hide_while_acting and not self._hidden:
            self._hidden = True
            self._window.hide()

    def _show(self) -> None:
        self._hidden = False
        if self._window is not None:
            self._window.show()


def launch() -> None:
    """Build the daemon, start its threads, and hand this thread to the webview."""
    import webview

    messages: queue.Queue = queue.Queue()
    worker = daemon_module.build(log=messages.put)
    worker.act = False  # a panel that opens ready to click things is a panel nobody trusts
    service = daemon_module.Service(worker, log=messages.put)
    service.start()

    api = Api(service, messages)
    window = webview.create_window(
        "winclicker",
        str(PANEL / "index.html"),
        js_api=api,
        width=WINDOW["width"],
        height=WINDOW["height"],
        min_size=WINDOW["min_size"],
        background_color="#0b1120",
    )
    api.attach(window)
    # A hotkey can start a run while the window is hidden, so the hide has to happen there too.
    _hook_hotkey_runs(worker, api)
    try:
        webview.start()
    finally:
        service.stop()


def _hook_hotkey_runs(worker, api: Api) -> None:
    """Hide the panel for a run started by voice or the overlay, not only by the Start button."""
    original = worker.queue_goal

    def queue_goal(goal: str) -> None:
        api._hide_for_run()
        original(goal)

    worker.queue_goal = queue_goal
