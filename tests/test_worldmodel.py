"""What is open, ranked and described for the classifier. No desktop involved."""

from __future__ import annotations

from dataclasses import dataclass

from typesafe_computer_use_win import worldmodel


@dataclass(frozen=True)
class FakeWindow:
    hwnd: int = 1
    title: str = "Window"
    app: str = "app"
    pid: int = 100
    rect: tuple = (0, 0, 800, 600)
    monitor: int = 0
    minimized: bool = False
    foreground: bool = False


def test_the_window_in_front_is_offered_first():
    windows = [
        FakeWindow(hwnd=1, app="chrome", title="GitHub", monitor=1),
        FakeWindow(hwnd=2, app="code", title="main.py", foreground=True),
    ]
    assert [w.hwnd for w in worldmodel.rank(windows)] == [2, 1]


def test_a_minimized_window_sorts_behind_a_visible_one():
    windows = [FakeWindow(hwnd=1, app="whatsapp", minimized=True), FakeWindow(hwnd=2, app="slack")]
    assert [w.hwnd for w in worldmodel.rank(windows)] == [2, 1]


def test_the_shell_is_not_something_to_switch_to():
    """Program Manager is the desktop itself and is always open; offering it wastes an option."""
    windows = [FakeWindow(title="Program Manager", app="explorer"), FakeWindow(title="Inbox", app="outlook")]
    assert [w.title for w in worldmodel.rank(windows)] == ["Inbox"]


def test_the_list_is_capped_because_options_compete_for_probability_mass():
    windows = [FakeWindow(hwnd=i, title=f"w{i}") for i in range(40)]
    assert len(worldmodel.rank(windows)) == worldmodel.MAX_WINDOWS


def test_a_window_line_says_which_monitor_it_is_on():
    line = worldmodel.summary(FakeWindow(app="whatsapp", title="WhatsApp", monitor=1))
    assert "whatsapp" in line and "monitor 2" in line


def test_a_minimized_window_says_so_and_the_front_one_says_so():
    assert "(minimized)" in worldmodel.summary(FakeWindow(minimized=True))
    assert "(in front)" in worldmodel.summary(FakeWindow(foreground=True))


def test_a_long_title_is_shortened_rather_than_flooding_the_packet():
    line = worldmodel.summary(FakeWindow(title="x" * 200))
    assert len(line) < 140 and "…" in line


def test_every_offered_window_has_a_key_the_answer_can_name():
    windows = [FakeWindow(hwnd=1), FakeWindow(hwnd=2)]
    assert set(worldmodel.criteria(windows)) == {"0", "1"}
    assert [r["k"] for r in worldmodel.records(windows)] == [0, 1]


def test_records_report_the_monitor_one_based_because_nobody_says_monitor_zero():
    assert worldmodel.records([FakeWindow(monitor=2)])[0]["monitor"] == 3


def test_an_app_that_is_already_running_is_found_by_process_name():
    windows = [FakeWindow(app="chrome", title="GitHub"), FakeWindow(app="whatsapp", title="WhatsApp")]
    assert worldmodel.already_open(windows, "whatsapp").app == "whatsapp"


def test_a_process_name_beats_a_passing_mention_in_someone_elses_title():
    """A browser tab called "WhatsApp Web" must not stand in for the WhatsApp app itself."""
    windows = [FakeWindow(app="chrome", title="WhatsApp Web - Chrome"), FakeWindow(app="whatsapp", title="WhatsApp")]
    assert worldmodel.already_open(windows, "whatsapp").app == "whatsapp"


def test_a_title_match_still_counts_when_no_process_matches():
    windows = [FakeWindow(app="chrome", title="Figma - untitled")]
    assert worldmodel.already_open(windows, "figma").app == "chrome"


def test_nothing_open_matches_nothing():
    assert worldmodel.already_open([FakeWindow(app="chrome")], "whatsapp") is None
    assert worldmodel.already_open([FakeWindow(app="chrome")], "   ") is None


def test_a_packaged_app_is_offered_once_not_twice():
    """Settings is hosted inside ApplicationFrameHost, so it appears under both names. Offering
    both splits the probability between two answers that do exactly the same thing."""
    windows = [
        FakeWindow(hwnd=1, app="SystemSettings", title="Settings"),
        FakeWindow(hwnd=2, app="ApplicationFrameHost", title="Settings"),
    ]
    assert [w.hwnd for w in worldmodel.rank(windows)] == [1]


def test_a_frame_host_window_with_no_app_behind_it_is_kept():
    """For some packaged apps the frame is the only window there is, so this must not drop it."""
    windows = [FakeWindow(hwnd=2, app="ApplicationFrameHost", title="Calculator")]
    assert [w.hwnd for w in worldmodel.rank(windows)] == [2]
