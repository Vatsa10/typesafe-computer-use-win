"""What a spoken line means: jev decides, code dispatches."""

from __future__ import annotations

from dataclasses import dataclass

from typesafe_computer_use_win.intent import Intent, interpret


@dataclass
class FakeChoiceAnswer:
    choice: str
    confidence: float
    probabilities: dict


@dataclass
class FakeNoulAnswer:
    noul: float


class FakeResult:
    def __init__(self, answers):
        self.answers = answers


class FakeClient:
    """Records the state and questions, answers with whatever it was told to."""

    def __init__(self, command="run_goal", confidence=0.9, heard=0.9):
        self.answer = FakeChoiceAnswer(command, confidence, {command: confidence})
        self.heard = FakeNoulAnswer(heard)
        self.state = None
        self.questions = None

    def system_one(self, state, questions):
        self.state = state
        self.questions = questions
        return FakeResult({"command": self.answer, "heard": self.heard})


def test_a_spoken_goal_becomes_a_runnable_intent():
    client = FakeClient(command="run_goal", confidence=0.88)
    intent = interpret(client, "open the console and check billing", running=False, paused=False)
    assert intent.command == "run_goal"
    assert intent.goal == "open the console and check billing"
    assert intent.actionable is True


def test_the_daemon_state_travels_with_the_transcript():
    client = FakeClient()
    interpret(client, "pause", running=True, paused=False)
    assert client.state["a_run_is_active"] is True
    assert client.state["the_run_is_paused"] is False
    assert client.state["transcript"] == "pause"


def test_both_questions_are_asked():
    client = FakeClient()
    interpret(client, "stop", running=True, paused=False)
    assert set(client.questions) == {"command", "heard"}


def test_a_low_confidence_command_is_not_actionable():
    client = FakeClient(command="quit_daemon", confidence=0.4)
    intent = interpret(client, "mumble mumble", running=False, paused=False)
    assert intent.actionable is False


def test_a_transcript_that_was_not_really_heard_is_not_actionable():
    client = FakeClient(command="run_goal", confidence=0.95, heard=0.2)
    intent = interpret(client, "uh, the, uh", running=False, paused=False)
    assert intent.actionable is False


def test_ignore_is_never_actionable_however_confident():
    client = FakeClient(command="ignore", confidence=0.99)
    assert interpret(client, "hey are you coming for lunch", running=False, paused=False).actionable is False


def test_an_empty_transcript_never_reaches_the_model():
    class Exploding:
        def system_one(self, state, questions):
            raise AssertionError("nothing was said, so nothing to ask")

    intent = interpret(Exploding(), "   ", running=False, paused=False)
    assert intent.command == "ignore" and intent.actionable is False


def test_only_a_run_goal_carries_a_goal():
    client = FakeClient(command="stop_run", confidence=0.93)
    assert interpret(client, "stop that", running=True, paused=False).goal == ""


def test_the_fields_are_positional_in_the_documented_order():
    intent = Intent("run_goal", "do a thing", 0.8, 0.9, "do a thing")
    assert (intent.command, intent.goal, intent.confidence, intent.heard, intent.transcript) == (
        "run_goal",
        "do a thing",
        0.8,
        0.9,
        "do a thing",
    )
    assert intent.min_confidence == 0.55


def test_a_caller_can_raise_the_bar():
    client = FakeClient(command="run_goal", confidence=0.7)
    assert interpret(client, "do a thing", running=False, paused=False, min_confidence=0.9).actionable is False
    assert interpret(client, "do a thing", running=False, paused=False, min_confidence=0.6).actionable is True


def test_the_transcript_is_whitespace_normalised():
    client = FakeClient()
    intent = interpret(client, "  open   the  console \n", running=False, paused=False)
    assert intent.transcript == "open the console"
    assert intent.goal == "open the console"
