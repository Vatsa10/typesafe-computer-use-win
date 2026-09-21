"""What a spoken or typed line means to the daemon.

A transcript is not a goal. "stop", "pause a sec", "never mind" and "open the console" all arrive
as text, and only the last one is something to accomplish. Handing raw text to the loop would make
the daemon try to achieve the word "stop", so the same classifier that picks actions decides what
the line is first. No free text is generated anywhere in this path.
"""

from __future__ import annotations

from dataclasses import dataclass

from typesafe_sdk import Choice, Noul

# The heard gate gates goals only. Measured against the live model, short imperatives score low on
# any "is this a complete instruction" question: "stop" read 0.24, "keep going" 0.35, "quit" 0.43.
# Gating control commands on that number makes a running loop impossible to stop by voice, which is
# the one thing voice must always manage. The risks are not symmetric either: a misheard "stop"
# ends a run the user can start again, while a misheard goal sets a machine clicking at something
# nobody asked for. So a goal must be heard cleanly, and a command only has to be recognised.
MIN_HEARD = 0.5
DEFAULT_MIN_CONFIDENCE = 0.55
IGNORED = "ignore"

COMMANDS = {
    "run_goal": (
        "The line is a task to carry out on this computer: something to open, find, fill in, "
        "check or navigate to. This is the only answer that starts work."
    ),
    "stop_run": "The line asks for the current run to stop now: stop, cancel that, abort, never mind.",
    "pause_run": "The line asks for the current run to hold where it is, to be continued later.",
    "resume_run": "The line asks for a paused run to carry on: continue, keep going, resume.",
    "quit_daemon": "The line asks to shut the assistant down entirely: quit, exit, goodbye.",
    IGNORED: (
        "The line is not addressed to the assistant at all, or asks for something it does not do: "
        "half a sentence, a word caught by accident, or talk meant for someone else in the room."
    ),
}


@dataclass(frozen=True)
class Intent:
    command: str
    goal: str
    confidence: float
    heard: float
    transcript: str
    min_confidence: float = DEFAULT_MIN_CONFIDENCE

    @property
    def actionable(self) -> bool:
        """Whether the daemon should act.

        Every answer must clear the confidence floor. Only `run_goal` must also clear MIN_HEARD:
        a goal becomes clicking, so a half-caught one is worth refusing, while refusing a
        half-caught "stop" would leave the user shouting at a run that will not stop.
        """
        if self.command == IGNORED or self.confidence < self.min_confidence:
            return False
        return self.heard >= MIN_HEARD if self.command == "run_goal" else True


def interpret(client, transcript: str, running: bool, paused: bool, min_confidence: float = DEFAULT_MIN_CONFIDENCE) -> Intent:
    """Classify one line against what the daemon is currently doing.

    The daemon's state travels with the transcript, so "pause" said while nothing runs reads as
    `ignore` rather than as a pause of nothing.
    """
    said = " ".join(transcript.split())
    if not said:
        return Intent(IGNORED, "", 0.0, 0.0, "", min_confidence)
    state = {
        "transcript": said,
        "a_run_is_active": running,
        "the_run_is_paused": paused,
        "what_the_assistant_does": (
            "drives this Windows machine toward a goal spoken in plain English: opening sites, clicking controls, filling fields"
        ),
    }
    questions = {
        "command": Choice(
            instructions=(
                "The user said this out loud to an assistant that drives their computer. What are "
                "they asking for? Answer with what the line means for the assistant right now, "
                "given whether a run is active and whether it is paused."
            ),
            criteria=COMMANDS,
        ),
        "heard": Noul(
            instructions=(
                "Was this transcript caught cleanly enough to act on? Judge the audio, not the "
                "length: a short command like 'stop' or 'keep going' is complete. Say no only for "
                "a garbled line, a false start, a sentence that breaks off mid-word, or speech "
                "that was clearly meant for someone else."
            )
        ),
    }
    answers = client.system_one(state=state, questions=questions).answers
    command = answers["command"]
    return Intent(
        command=command.choice,
        goal=said if command.choice == "run_goal" else "",
        confidence=command.confidence,
        heard=answers["heard"].noul,
        transcript=said,
        min_confidence=min_confidence,
    )
