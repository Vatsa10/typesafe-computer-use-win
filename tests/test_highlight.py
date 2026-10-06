"""Pointing at things on screen. Geometry and markup only: no window is ever opened."""

from __future__ import annotations

from typesafe_computer_use_win import highlight
from typesafe_computer_use_win.highlight import Mark, bounds_of, placed


def marks_in(boxes: list[dict]) -> list[dict]:
    return boxes


def test_the_overlay_covers_every_mark_with_room_for_its_label():
    left, top, right, bottom = bounds_of([Mark(100, 200, 50, 20), Mark(400, 500, 80, 30)])
    assert left < 100 and right > 480
    assert top < 200 - 20, "a label sits above its box and must not fall off the top"
    assert bottom > 530


def test_coordinates_are_relative_to_the_overlay_not_the_desktop():
    """The marks arrive in virtual-desktop points, the same ones a click uses, and the window is
    placed somewhere on that desktop. Drawing at the raw numbers would put the box off-window."""
    mark = Mark(x=2700, y=400, w=240, h=52, label="Export")
    box = marks_in(placed([mark], (2672, 344)))[0]
    assert (box["x"], box["y"]) == (28, 56)
    assert (box["w"], box["h"]) == (240, 52)


def test_a_mark_on_the_third_monitor_is_placed_there_and_not_on_the_first(monkeypatch):
    from dataclasses import dataclass

    @dataclass(frozen=True)
    class M:
        index: int
        left: int
        top: int
        right: int
        bottom: int
        primary: bool = False

    monkeypatch.setattr(highlight.windows, "monitors", lambda: [M(0, 0, 0, 2560, 1440, True), M(1, 5120, 0, 7040, 1080)])
    monkeypatch.setattr(highlight.windows, "monitor_at", lambda x, y: 1)
    left, _, right, _ = highlight.clamped_bounds([Mark(6000, 400, 200, 40)])
    assert left >= 5120 and right <= 7040


def test_a_mark_running_past_the_edge_is_clamped_onto_the_display(monkeypatch):
    from dataclasses import dataclass

    @dataclass(frozen=True)
    class M:
        index: int
        left: int
        top: int
        right: int
        bottom: int
        primary: bool = True

    monkeypatch.setattr(highlight.windows, "monitors", lambda: [M(0, 0, 0, 2560, 1440)])
    monkeypatch.setattr(highlight.windows, "monitor_at", lambda x, y: 0)
    left, top, right, bottom = highlight.clamped_bounds([Mark(2500, 1400, 400, 200)])
    assert right <= 2560 and bottom <= 1440 and right > left and bottom > top


def test_a_label_is_capped_rather_than_running_off_the_screen():
    """Labels come from screen text, which is to say from whatever happens to be on screen."""
    drawn = placed([Mark(10, 10, 20, 20, label="x" * 200)], (0, 0))[0]
    assert len(drawn["label"]) <= 80, "a label is a hint, not a paragraph"


def test_only_a_handful_of_boxes_are_drawn():
    """More than a few stops being a hint and becomes a diagram nobody reads."""
    many = [Mark(i * 10, i * 10, 20, 20, label=f"{i}") for i in range(20)]
    assert len(marks_in(placed(many, (0, 0)))) == highlight.MAX_MARKS


def test_a_note_is_drawn_differently_from_the_thing_to_press():
    tones = {m["tone"] for m in marks_in(placed([Mark(0, 0, 9, 9, tone="note"), Mark(0, 0, 9, 9)], (0, 0)))}
    assert tones == {"note", "point"}


def test_nothing_to_point_at_draws_nothing():
    assert highlight.show([]) is False


def test_the_boxes_and_their_labels_are_drawn():
    """Drawing is tested on a fake canvas, because a real one needs a window on a real desktop."""

    class FakeCanvas:
        def __init__(self):
            self.rects, self.texts = [], []

        def create_rectangle(self, *box, **kw):
            self.rects.append((box, kw))
            return len(self.rects)

        def create_text(self, x, y, **kw):
            self.texts.append((x, y, kw))
            return 100 + len(self.texts)

        def bbox(self, _item):
            return (10, 10, 90, 24)

        def tag_raise(self, *a):
            pass

    canvas = FakeCanvas()
    highlight.draw(canvas, placed([Mark(100, 200, 60, 20, label="Export"), Mark(300, 400, 40, 40, tone="note")], (0, 0)))
    assert len(canvas.rects) == 3, "two marks, and a plate behind the one label"
    assert canvas.texts[0][2]["text"] == "Export"
    dashed = [kw.get("dash") for _box, kw in canvas.rects if kw.get("dash")]
    assert dashed, "a note is dashed so it does not read as the thing to press"


def test_a_label_near_the_top_is_drawn_inside_its_box_instead_of_above_it():
    class FakeCanvas:
        def __init__(self):
            self.texts = []

        def create_rectangle(self, *box, **kw):
            return 1

        def create_text(self, x, y, **kw):
            self.texts.append(y)
            return 2

        def bbox(self, _item):
            return (0, 0, 10, 10)

        def tag_raise(self, *a):
            pass

    high, low = FakeCanvas(), FakeCanvas()
    highlight.draw(high, placed([Mark(10, 4, 20, 20, label="top")], (0, 0)))
    highlight.draw(low, placed([Mark(10, 300, 20, 20, label="middle")], (0, 0)))
    assert high.texts[0] > 4, "a label above the window edge is a label nobody reads"
    assert low.texts[0] < 300
