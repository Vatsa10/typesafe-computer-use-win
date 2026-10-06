"""Talk mode: it reads the screen, it explains, and it cannot act."""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from typesafe_computer_use_win import talk
from typesafe_computer_use_win.writer import WriterUnavailable


class FakeWriter:
    """Records the one structured call talk mode makes, and answers with a fixed sentence."""

    def __init__(self, answer="The dialog says the export finished.", error=None):
        self.answer = answer
        self.error = error
        self.calls: list[dict] = []

    def structured(self, system, packet, properties, max_tokens, model=None, image=None):
        self.calls.append(
            {"system": system, "packet": packet, "properties": properties, "max_tokens": max_tokens, "image": image}
        )
        if self.error is not None:
            raise self.error
        return {"answer": self.answer}


def fake_screen(image="capture", app="Chrome", url="https://example.com/report"):
    return SimpleNamespace(image=image, app=app, url=url)


def install(monkeypatch, screen=None, items=("Export", "Finished at 10:04")):
    """Capture and perceive replaced: no test touches a real screen."""
    screen = screen if screen is not None else fake_screen()
    seen: dict = {}

    def capture(image_path=None, app=None, url=None, browser="", timing=None):
        seen["browser"] = browser
        return screen

    def perceive(s, budget, goal, timing=None, cache=None, history=None):
        seen["perceived"] = s
        seen["goal"] = goal
        return [SimpleNamespace(text=t) for t in items]

    monkeypatch.setattr(talk, "capture", capture)
    monkeypatch.setattr(talk, "perceive", perceive)
    return seen


def test_the_question_the_capture_and_the_screen_text_all_reach_the_writer(monkeypatch):
    screen = fake_screen()
    seen = install(monkeypatch, screen)
    writer = FakeWriter()
    talk.answer_question(writer, "what does this say", log=lambda *_: None)
    assert seen["perceived"] is screen
    call = writer.calls[0]
    assert call["packet"]["question"] == "what does this say"
    assert call["packet"]["screen_text_in_reading_order"] == ["Export", "Finished at 10:04"]
    assert call["packet"]["frontmost_app"] == "Chrome"
    assert call["image"] is screen.image


def test_the_answer_comes_back_and_is_logged(monkeypatch):
    install(monkeypatch)
    lines: list[str] = []
    answer = talk.answer_question(FakeWriter("Three rows failed validation."), "what am I looking at", log=lines.append)
    assert answer == "Three rows failed validation."
    assert lines == ["Three rows failed validation."]


def test_the_browser_hint_reaches_the_capture(monkeypatch):
    seen = install(monkeypatch)
    talk.answer_question(FakeWriter(), "how do I export this", browser="chrome", log=lambda *_: None)
    assert seen["browser"] == "chrome"


def test_an_unavailable_writer_is_reported_not_raised(monkeypatch):
    install(monkeypatch)
    lines: list[str] = []
    answer = talk.answer_question(
        FakeWriter(error=WriterUnavailable("the account is out of credit")), "explain this error", log=lines.append
    )
    assert "out of credit" in answer
    assert lines == [answer]


def test_the_writer_is_asked_to_answer_only_from_the_screen(monkeypatch):
    install(monkeypatch)
    writer = FakeWriter()
    talk.answer_question(writer, "what does this say", log=lambda *_: None)
    system = writer.calls[0]["system"].lower()
    assert "visible" in system and "nothing else" in system
    assert "never from memory" in system and "never a guess" in system
    assert "on-screen controls" in system  # teach the user which control to press, do not press it
    assert "teaching, not acting" in system


def test_talk_mode_cannot_act(monkeypatch):
    """The safety property, pinned.

    Talk mode's value is that it cannot click, so anything reaching into `actions` from this path
    must fail the test. A booby-trapped `actions` is planted in talk's own namespace: if the path
    ever grows a `perform` call it will find this one and explode.
    """

    class Forbidden:
        def __getattr__(self, name):
            raise AssertionError(f"talk mode must never act, but it reached actions.{name}")

    monkeypatch.setattr(talk, "actions", Forbidden(), raising=False)
    monkeypatch.setattr(talk, "perform", Forbidden(), raising=False)
    monkeypatch.setattr(talk, "run", Forbidden(), raising=False)
    install(monkeypatch)
    assert talk.answer_question(FakeWriter("Nothing was clicked."), "what is this", log=lambda *_: None)


def test_talk_imports_nothing_that_acts():
    """The namespace itself holds no door to actions, so the booby trap above had nothing to replace."""
    for name in ("actions", "perform", "run", "runner", "click", "press"):
        assert not hasattr(talk, name), f"talk.{name} exists, and talk mode must not be able to act"


def test_a_writer_failure_that_is_not_unavailability_still_surfaces(monkeypatch):
    """Only WriterUnavailable is turned into a sentence; a bug in this path must not be swallowed."""
    install(monkeypatch)

    def exploding(*_a, **_k):
        raise KeyError("answer")

    monkeypatch.setattr(talk, "compose_explanation", exploding)
    with pytest.raises(KeyError):
        talk.answer_question(FakeWriter(), "what does this say", log=lambda *_: None)
