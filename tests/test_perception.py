from typesafe_computer_use_win.models import Item
from typesafe_computer_use_win.perception import (
    goal_echoes,
    is_echo,
    merge_blocks,
    merge_sources,
    order_items,
    to_items,
)


def line(text, x1, y1, x2, y2, conf=1.0):
    return (text, conf, (float(x1), float(y1), float(x2), float(y2)))


def test_merges_stacked_lines_across_columns():
    lines = [
        line("Kash Patel defends", 1080, 1531, 1300, 1561),
        line("Two House Democrats", 1400, 1531, 1600, 1561),
        line("removing bestiality as", 1075, 1571, 1300, 1601),
        line("defect again on key vote", 1400, 1571, 1600, 1601),
        line("FBI applicants", 1080, 1606, 1300, 1636),
    ]
    texts = sorted(t for t, _, _ in merge_blocks(lines))
    assert texts == ["Kash Patel defends removing bestiality as FBI applicants", "Two House Democrats defect again on key vote"]


def test_does_not_merge_far_or_misaligned_lines():
    lines = [line("Home", 100, 100, 200, 130), line("World", 400, 100, 500, 130), line("Footer", 100, 900, 200, 930)]
    assert len(merge_blocks(lines)) == 3


def test_merged_block_keeps_min_confidence_and_union_box():
    lines = [line("a", 100, 100, 200, 130, conf=1.0), line("b", 102, 140, 260, 170, conf=0.5)]
    ((text, conf, box),) = merge_blocks(lines)
    assert text == "a b" and conf == 0.5 and box == (100, 100, 260, 170)


def test_reading_order_rows_then_columns():
    lines = [line("right", 800, 100, 900, 130), line("left", 100, 105, 200, 135), line("below", 100, 300, 200, 330)]
    assert [it.text for it in to_items(lines, 255)] == ["left", "right", "below"]


def test_budget_caps_items():
    lines = [line(str(i), 100, 100 + 40 * i, 200, 130 + 40 * i) for i in range(10)]
    assert len(to_items(lines, 3)) == 3


def test_goal_echo_matches_wrapped_command_lines():
    goal = "go to cnn and click onto something related to AI on the homepage"
    echoes = goal_echoes(goal)
    assert is_echo('clear && uv run clicker "go to cnn and click onto something', echoes)
    assert is_echo('related to AI on the homepage" --act', echoes)
    assert not is_echo("Trending: Trump and AI warnings", echoes)


def ocr_item(index, text, x1, y1, x2, y2, conf=0.9):
    return Item(index, text, conf, float(x1), float(y1), float(x2), float(y2))


def ax_item(index, text, x1, y1, x2, y2, role="button"):
    return Item(index, text, 1.0, float(x1), float(y1), float(x2), float(y2), role=role, source="ax")


def test_merge_folds_an_overlapping_control_onto_the_ocr_block_that_names_it():
    block = ocr_item(0, "Register Now", 100, 100, 300, 130)
    control = ax_item(0, "Register Now for Disrupt", 110, 102, 290, 128, role="link")
    (merged,) = merge_sources([block], [control])
    assert merged.source == "ax+ocr" and merged.role == "link"
    assert merged.text == "Register Now for Disrupt"  # the longer of the two labels
    assert (merged.x1, merged.y1, merged.x2, merged.y2) == (100.0, 100.0, 300.0, 130.0)  # the OCR box


def test_merge_matches_on_shared_words_not_only_containment():
    block = ocr_item(0, "Buy tickets now", 100, 100, 300, 130)
    control = ax_item(0, "Buy tickets", 100, 100, 300, 130)
    assert [it.source for it in merge_sources([block], [control])] == ["ax+ocr"]


def test_merge_keeps_both_when_the_boxes_overlap_but_the_text_does_not_agree():
    block = ocr_item(0, "Search the docs", 100, 100, 300, 130)
    control = ax_item(0, "Clear input", 100, 100, 300, 130)
    assert sorted(it.source for it in merge_sources([block], [control])) == ["ax", "ocr"]


def test_merge_keeps_both_when_the_text_agrees_but_the_boxes_are_apart():
    block = ocr_item(0, "Share", 100, 100, 200, 130)
    control = ax_item(0, "Share", 900, 600, 960, 630)
    assert sorted(it.source for it in merge_sources([block], [control])) == ["ax", "ocr"]


def test_merge_consumes_each_ocr_block_at_most_once():
    block = ocr_item(0, "Send", 100, 100, 200, 130)
    controls = [ax_item(0, "Send", 100, 100, 200, 130), ax_item(1, "Send", 104, 104, 196, 126)]
    merged = merge_sources([block], controls)
    assert sorted(it.source for it in merged) == ["ax", "ax+ocr"]


def test_merge_numbers_everything_in_reading_order():
    blocks = [ocr_item(0, "below", 100, 300, 200, 330), ocr_item(1, "right", 800, 100, 900, 130)]
    controls = [ax_item(0, "left", 100, 105, 200, 135)]
    assert [(it.index, it.text) for it in merge_sources(blocks, controls)] == [(0, "left"), (1, "right"), (2, "below")]


def test_budget_drops_the_faintest_ocr_blocks_before_any_control():
    blocks = [ocr_item(i, f"text {i}", 100, 100 + 40 * i, 200, 130 + 40 * i, conf=0.3 + 0.1 * i) for i in range(3)]
    controls = [ax_item(0, "Send", 800, 100, 900, 130)]
    kept = merge_sources(blocks, controls, budget=2)
    assert sorted(it.text for it in kept) == ["Send", "text 2"]


def test_budget_falls_back_to_dropping_controls_when_only_controls_remain():
    controls = [ax_item(i, f"control {i}", 100, 100 + 40 * i, 200, 130 + 40 * i) for i in range(4)]
    assert len(merge_sources([], controls, budget=2)) == 2


def test_order_items_renumbers_rows_then_columns():
    items = [ocr_item(7, "right", 800, 100, 900, 130), ocr_item(2, "left", 100, 105, 200, 135)]
    assert [(it.index, it.text) for it in order_items(items)] == [(0, "left"), (1, "right")]


# ------------------------------------------------------------------ reading a second monitor


def test_the_read_region_is_measured_from_the_display_being_captured(screen_factory=None):
    """The window is in virtual-desktop points, the crop is in capture pixels.

    Measured live before this was handled: on monitor 2 the region clamped to nothing and a step
    read 0 items and 0 controls, while the same screen on the primary read 174 and 42.
    """
    from dataclasses import replace

    from PIL import Image

    from typesafe_computer_use_win.models import Screen
    from typesafe_computer_use_win.perception import ocr_region

    base = Screen(
        image=Image.new("RGB", (2560, 1440)),
        scale=1.0,
        app="code",
        field=None,
        url=None,
        window=(2600.0, 200.0, 1200.0, 800.0),  # a window on the second monitor
    )
    on_primary = ocr_region(base)  # origin (0, 0): the window is off the right of this capture
    on_its_own_monitor = ocr_region(replace(base, origin=(2560.0, 157.0)))

    assert on_its_own_monitor != on_primary
    left, top, right, bottom = on_its_own_monitor
    assert 0 <= left < right <= 2560 and 0 <= top < bottom <= 1440
    assert right - left < 2560, "the crop should be the window, not a fallback to the whole screen"


def test_a_window_on_the_captured_display_is_unaffected_by_the_offset():
    from PIL import Image

    from typesafe_computer_use_win.models import Screen
    from typesafe_computer_use_win.perception import ocr_region

    primary = Screen(
        image=Image.new("RGB", (2560, 1440)),
        scale=1.0,
        app="code",
        field=None,
        url=None,
        window=(100.0, 100.0, 800.0, 600.0),
    )
    assert ocr_region(primary) == ocr_region(primary)
    left, _, right, _ = ocr_region(primary)
    assert left < right <= 2560


# ------------------------------------------------------------------ never reading our own panel


def _monitor(index, left, top, right, bottom, primary=False):
    from typesafe_computer_use_win.windows import Monitor

    return Monitor(index=index, left=left, top=top, right=right, bottom=bottom, primary=primary)


def _window(hwnd, app, pid, monitor, title=None, foreground=False):
    from typesafe_computer_use_win.windows import WindowInfo

    return WindowInfo(
        hwnd=hwnd,
        title=title or f"{app} window",
        app=app,
        pid=pid,
        rect=(0, 0, 800, 600),
        monitor=monitor,
        minimized=False,
        foreground=foreground,
    )


def _fake_desktop(monkeypatch, monitors, inventory, front=("python", None)):
    """Stand in for the desktop. `front[1]` of None means the foreground window is this process's."""
    import os

    from PIL import Image

    from typesafe_computer_use_win import perception, windows

    front_pid = os.getpid() if front[1] is None else front[1]
    monkeypatch.setattr(windows, "monitors", lambda: list(monitors))
    monkeypatch.setattr(windows, "open_windows", lambda *a, **k: list(inventory))
    monkeypatch.setattr(windows, "frontmost_pid", lambda: front_pid)
    monkeypatch.setattr(windows, "frontmost_app_and_pid", lambda: (front[0], front_pid))
    monkeypatch.setattr(windows, "screenshot", lambda bounds=None: Image.new("RGB", (640, 480)))
    monkeypatch.setattr(windows, "display_scale", lambda image: 1.0)
    monkeypatch.setattr(windows, "frontmost_window_bounds", lambda pid=None: None)
    monkeypatch.setattr(windows, "focused_field", lambda: None)
    monkeypatch.setattr(windows, "browser_url", lambda browser: None)
    return perception


def test_survey_falls_back_to_another_window_display_when_the_foreground_is_ours(monkeypatch):
    screens = [_monitor(0, 0, 0, 1920, 1080, primary=True), _monitor(1, 1920, 0, 4480, 1440)]
    inventory = [_window(2, "chrome", 4242, 1)]
    perception = _fake_desktop(monkeypatch, screens, inventory)
    _monitors, _inventory, which, origin = perception.survey()
    assert which == 1 and origin == (1920.0, 0.0)


def test_capture_reports_the_fallback_window_app_and_pid_when_the_foreground_is_ours(monkeypatch):
    screens = [_monitor(0, 0, 0, 1920, 1080, primary=True)]
    inventory = [_window(2, "chrome", 4242, 0)]
    perception = _fake_desktop(monkeypatch, screens, inventory)
    screen = perception.capture()
    assert screen.app == "chrome" and screen.pid == 4242  # not "python", and not our own pid


def test_our_own_foreground_with_no_other_window_keeps_todays_behaviour(monkeypatch):
    import os

    screens = [_monitor(0, 0, 0, 1920, 1080, primary=True), _monitor(1, 1920, 0, 4480, 1440)]
    perception = _fake_desktop(monkeypatch, screens, [])
    _monitors, _inventory, which, origin = perception.survey()
    assert which == 0 and origin == (0.0, 0.0)
    screen = perception.capture()
    assert screen.app == "python" and screen.pid == os.getpid()


def test_capture_leaves_a_foreign_foreground_window_alone(monkeypatch):
    screens = [_monitor(0, 0, 0, 1920, 1080, primary=True), _monitor(1, 1920, 0, 4480, 1440)]
    inventory = [_window(2, "chrome", 4242, 1, foreground=True), _window(3, "code", 99, 0)]
    perception = _fake_desktop(monkeypatch, screens, inventory, front=("chrome", 4242))
    _monitors, _inventory, which, _origin = perception.survey()
    screen = perception.capture()
    assert which == 1
    assert screen.app == "chrome" and screen.pid == 4242


# ------------------------------------------------------------------ never reading our own log


def test_a_screen_line_echoing_a_recent_action_is_dropped():
    from typesafe_computer_use_win.perception import history_echoes

    history = ["did: switched to chrome 'Vatsa10/typesafe-computer-use' on monitor 3", "did: typed the search query"]
    echoes = history_echoes(history)
    assert is_echo("did: switched to chrome — Google Chrome'", echoes)
    assert is_echo("0.10 [22] did: switched to CHROME ...", echoes)
    assert not is_echo("Google Chrome", echoes)
    assert not is_echo("Trending: Trump and AI warnings", echoes)


def test_a_short_history_line_does_not_filter_unrelated_screen_text():
    from typesafe_computer_use_win.perception import history_echoes

    echoes = history_echoes(["waited", "typed y", "did: done"])
    assert echoes == set()
    assert not is_echo("waited for the page", echoes)
    assert not is_echo("Start", echoes)


def test_history_echoes_tolerate_no_history():
    from typesafe_computer_use_win.perception import history_echoes

    assert history_echoes(None) == set() and history_echoes([]) == set()


def test_the_goal_echo_filter_still_works_alongside_history():
    from typesafe_computer_use_win.perception import history_echoes

    goal = "go to cnn and click onto something related to AI on the homepage"
    echoes = goal_echoes(goal) | history_echoes(["did: switched to chrome on monitor 3"])
    assert is_echo('clear && uv run clicker "go to cnn and click onto something', echoes)
    assert is_echo("did: switched to chrome on monitor 3", echoes)
    assert not is_echo("Trending: Trump and AI warnings", echoes)


def test_a_control_is_placed_against_the_display_being_captured_not_the_desktop():
    """Measured on a desk with a monitor above the primary: the capture starts at y=-1440 and the
    menu bar's controls report y=-1440, so they belong at pixel 0. Without the offset the boxes
    land off the capture, the region hints name the wrong third of the screen, and Screen.to_points
    adds the origin a second time, so a click misses by a whole monitor."""
    from typesafe_computer_use_win.models import AxNode
    from typesafe_computer_use_win.perception import to_ax_items

    node = AxNode(role="AXButton", label="File", x=44.0, y=-1440.0, w=46.0, h=44.0, pressable=True, ref=None)
    on_primary = to_ax_items([node], 1.0)[0]
    on_its_own_display = to_ax_items([node], 1.0, origin=(1.0, -1440.0))[0]

    assert on_primary.y1 == -1440.0, "without the offset it sits a monitor above the capture"
    assert on_its_own_display.y1 == 0.0
    assert on_its_own_display.x1 == 43.0
    assert on_its_own_display.y2 == 44.0


def test_a_click_point_is_not_offset_twice():
    """to_points adds the origin back, so the item must have had it taken off exactly once."""
    from PIL import Image

    from typesafe_computer_use_win.models import AxNode, Screen
    from typesafe_computer_use_win.perception import to_ax_items

    origin = (2560.0, 157.0)
    node = AxNode(role="AXButton", label="Send", x=2600.0, y=200.0, w=40.0, h=20.0, pressable=True, ref=None)
    item = to_ax_items([node], 1.0, origin)[0]
    screen = Screen(image=Image.new("RGB", (2560, 1440)), scale=1.0, app="x", field=None, url=None, origin=origin)
    x, y = screen.to_points(item)
    assert (round(x), round(y)) == (2620, 210), "the centre of the control, in desktop coordinates"
