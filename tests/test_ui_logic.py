"""The panel's logic, without a window.

Everything the page can call goes through `Api`, so these build one over fakes. The widgets
themselves are HTML and are not tested here, the same way the Tk widgets never were.
"""

from __future__ import annotations

import queue
from dataclasses import dataclass

import pytest

from typesafe_computer_use_win import ui


class FakeControl:
    def __init__(self) -> None:
        self.paused = False
        self.aborted = False

    def toggle_pause(self) -> bool:
        self.paused = not self.paused
        return self.paused

    def abort(self, reason: str = "x") -> None:
        self.aborted = True


class FakeDaemon:
    def __init__(self) -> None:
        self.running = False
        self.control = FakeControl()
        self.queued: list[str] = []
        self.act = False
        self.voice_enabled = True
        self.probed = 0

    def queue_goal(self, goal: str) -> None:
        self.queued.append(goal)

    def on_pause(self) -> None:
        self.control.toggle_pause()

    def on_abort(self) -> None:
        if self.running:
            self.control.abort("panel")

    def probe_voice(self) -> None:
        self.probed += 1


class FakeService:
    def __init__(self) -> None:
        self.daemon = FakeDaemon()
        self.posted: list = []
        self.stopped = False

    def post(self, handler) -> None:
        self.posted.append(handler)

    def stop(self) -> None:
        self.stopped = True


class FakeWindow:
    def __init__(self) -> None:
        self.visible = True
        self.events: list[str] = []

    def hide(self) -> None:
        self.visible = False
        self.events.append("hide")

    def show(self) -> None:
        self.visible = True
        self.events.append("show")


@pytest.fixture
def api():
    service = FakeService()
    panel = ui.Api(service, queue.Queue())
    panel.attach(FakeWindow())
    return panel


# ------------------------------------------------------------------ the feed


def test_the_page_is_handed_everything_said_since_it_last_asked(api):
    api._messages.put("heard: 'open youtube'")
    api._messages.put("  queued: 'open youtube'")
    assert api.poll()["lines"] == ["heard: 'open youtube'", "  queued: 'open youtube'"]
    assert api.poll()["lines"] == [], "a second poll must not repeat what was already delivered"


def test_a_backlog_is_capped_so_one_frame_cannot_be_handed_a_minute_of_log():
    messages: queue.Queue = queue.Queue()
    for i in range(50):
        messages.put(f"line {i}")
    assert len(ui.line_queue(messages, limit=10)) == 10


def test_the_state_the_page_paints_comes_from_the_daemon(api):
    api._daemon.running = True
    api._daemon.control.paused = True
    state = api.poll()
    assert state["running"] is True and state["paused"] is True
    assert "talk" in state["hotkeys"]


# ------------------------------------------------------- hiding for a run


def test_the_window_hides_before_the_job_exists(api):
    """The old panel hid on a timer and lost the race, so a run's first capture read its own
    buttons and offered them as things to click. The hide has to happen first, not eventually."""
    api.start("open youtube")
    assert api._window.events == ["hide"]
    assert api._daemon.queued == ["open youtube"], "and the goal still has to be queued"
    assert api._window.events.index("hide") == 0


def test_the_window_comes_back_when_the_run_finishes(api):
    api.start("open youtube")
    api._daemon.running = True
    api.poll()
    assert api._window.visible is False
    api._daemon.running = False
    api.poll()
    assert api._window.visible is True


def test_hiding_can_be_turned_off(api):
    api.set_mode(act=True, voice=True, hide=False)
    api.start("open youtube")
    assert api._window.events == [] and api._daemon.queued == ["open youtube"]


def test_an_empty_goal_starts_nothing_and_does_not_hide(api):
    assert api.start("   ") is False
    assert api._daemon.queued == [] and api._window.events == []


# ----------------------------------------------------------------- controls


def test_the_mode_switches_reach_the_daemon(api):
    api.set_mode(act=True, voice=False, hide=True)
    assert api._daemon.act is True and api._daemon.voice_enabled is False


def test_testing_voice_goes_to_the_input_worker_not_this_thread(api):
    """Recording blocks until the key comes up; doing that on the webview thread freezes the UI."""
    api.test_voice()
    assert api._service.posted == [api._daemon.probe_voice]


def test_testing_voice_while_voice_is_off_says_so_and_records_nothing(api):
    api.set_mode(act=False, voice=False, hide=True)
    assert api.test_voice() is False
    assert api._service.posted == []
    assert "voice is off" in api.poll()["lines"]


# ----------------------------------------------------------------- settings


def test_every_setting_carries_its_current_value_and_its_default():
    rows = ui.settings_rows({"CLICKER_BROWSER": "Firefox"})
    browser = next(r for r in rows if r["key"] == "CLICKER_BROWSER")
    assert browser["value"] == "Firefox" and browser["fallback"]
    assert all(row["label"] and row["fallback"] for row in rows)


def test_no_api_key_is_ever_shown_or_written():
    """A panel that displays a secret is a panel that leaks one into a screenshot."""
    keys = {row["key"] for row in ui.settings_rows({})}
    assert not any("API_KEY" in key for key in keys)


def test_saving_ignores_a_key_the_panel_does_not_own(api, tmp_path, monkeypatch):
    written = {}
    monkeypatch.setattr(ui.dotenv_io, "write_env", lambda path, values: written.update(values))
    api.save_settings({"CLICKER_BROWSER": "Firefox", "OPENAI_API_KEY": "sk-leak"})
    assert written == {"CLICKER_BROWSER": "Firefox"}


# ------------------------------------------------------------------ history


@dataclass(frozen=True)
class FakeRun:
    name: str = "20260101-120000"
    goal: str = "open youtube"
    outcome: str = "done"
    answer: str | None = "it is open"
    goal_achieved: bool | None = True
    seconds: float | None = 4.2
    steps_taken: int = 3
    acted: bool = True
    path: str = ""


def test_a_run_reaches_the_page_as_plain_data():
    row = ui.run_rows([FakeRun()])[0]
    assert row["name"] == "20260101-120000" and row["goal_achieved"] is True
    assert set(row) == {"name", "goal", "outcome", "answer", "goal_achieved", "seconds", "steps_taken", "acted"}


def test_a_missing_capture_is_an_empty_string_rather_than_a_broken_image(tmp_path):
    assert ui.data_url(None) == ""
    assert ui.data_url(tmp_path / "gone.png") == ""


def test_a_capture_becomes_something_an_img_can_show(tmp_path):
    from PIL import Image

    shot = tmp_path / "step-001.png"
    Image.new("RGB", (8, 6)).save(shot)
    assert ui.data_url(shot).startswith("data:image/png;base64,")


# ------------------------------------------------- what the page is allowed to see


def test_the_page_is_offered_methods_and_nothing_else(api):
    """pywebview builds the JS proxy by walking this object's public attributes.

    A public reference to the native window recursed through .NET font families until the stack
    ended and filled the console with GenericSansSerif.GenericSansSerif...; a public reference to
    the service or the daemon would hand the page threads, queues and locks to serialise. So the
    rule is that only methods are public, and this test is the rule.
    """
    public = {name for name in vars(api) if not name.startswith("_")}
    assert public == set(), f"these would be walked and serialised: {sorted(public)}"


def test_every_method_the_page_calls_is_still_reachable(api):
    for name in ("poll", "start", "pause", "abort", "test_voice", "set_mode", "runs", "steps", "shot", "settings"):
        assert callable(getattr(api, name)), f"the page calls {name}"
