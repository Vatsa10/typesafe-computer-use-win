"""Read the `runs/` folder, so a UI can browse what already happened.

Nothing here writes, captures or clicks: it is the data layer behind a history tab, and every
function is pure with respect to the disk.

A run folder is written by a process that may be killed mid-run, so tolerance is the whole job.
`run.json` lands in the `finally` block, which means a run killed hard has step files and no
summary at all; a run killed mid-write has half a JSON document. Neither may raise here. Anything
that cannot be recovered degrades to a default, and the folder still appears in the list.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path

STEP = re.compile(r"^step-(\d+)(-raw\.png|-payload\.txt|-answers\.json|\.png)$")

# What each matched suffix is called on StepRecord.
FIELDS = {"-raw.png": "raw", "-payload.txt": "payload", "-answers.json": "answers", ".png": "annotated"}

UNKNOWN = "unknown"


@dataclass(frozen=True)
class StepRecord:
    """The files one step left behind. Each is independently optional: a run that died between
    two writes leaves `-raw.png` without the annotated `.png` beside it."""

    number: int
    annotated: Path | None = None  # step-NNN.png
    raw: Path | None = None  # step-NNN-raw.png
    payload: Path | None = None  # step-NNN-payload.txt
    answers: Path | None = None  # step-NNN-answers.json


@dataclass(frozen=True)
class RunSummary:
    """One run, as much of it as survived."""

    path: Path
    name: str  # the folder name, which is the timestamp
    goal: str
    outcome: str
    answer: str | None
    goal_achieved: bool | None
    seconds: float | None
    steps_taken: int
    acted: bool


def _read_json(path: Path) -> dict:
    """Whatever object the file holds, or `{}`. Missing, truncated, unreadable and not-an-object
    all mean the same thing to a reader: nothing to show."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return {}
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return {}
    return data if isinstance(data, dict) else {}


def _str(value: object, default: str = "") -> str:
    return value if isinstance(value, str) else default


def _number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, int | float):
        return None
    return float(value)


def steps_of(path: Path) -> list[StepRecord]:
    """The step files in a run folder, ascending by step number.

    Sorted numerically, not lexically, so step 2 comes before step 10, and the digit count is read
    from the name rather than assumed: older runs numbered with fewer than three digits.
    """
    found: dict[int, dict[str, Path]] = {}
    try:
        entries = sorted(path.iterdir())
    except OSError:
        return []
    for entry in entries:
        match = STEP.match(entry.name)
        if match is None or not entry.is_file():
            continue
        found.setdefault(int(match.group(1)), {})[FIELDS[match.group(2)]] = entry
    return [StepRecord(number=n, **found[n]) for n in sorted(found)]


def step_answers(step: StepRecord) -> dict:
    """The parsed `-answers.json` for a step; `{}` when it is missing or corrupt."""
    return {} if step.answers is None else _read_json(step.answers)


def load_run(path: Path) -> RunSummary:
    """One run folder, read as far as it goes.

    With no `run.json` the outcome is `"unknown"` and the step count is recovered by counting step
    files, which is what a killed run leaves to count.
    """
    data = _read_json(path / "run.json")
    steps = data.get("steps_taken")
    return RunSummary(
        path=path,
        name=path.name,
        goal=_str(data.get("goal")),
        outcome=_str(data.get("outcome"), UNKNOWN) or UNKNOWN,
        answer=_str(data.get("answer")) or None,
        goal_achieved=data.get("goal_achieved") if isinstance(data.get("goal_achieved"), bool) else None,
        seconds=_number(data.get("seconds")),
        steps_taken=steps if isinstance(steps, int) and not isinstance(steps, bool) else len(steps_of(path)),
        acted=data.get("act") is True,
    )


def list_runs(root: Path) -> list[RunSummary]:
    """Every run folder under `root`, newest first by folder name, which is a timestamp.

    A missing `root` is an empty history, not an error, and a stray file beside the folders is
    skipped rather than read.
    """
    try:
        entries = list(root.iterdir())
    except OSError:
        return []
    folders = sorted((e for e in entries if e.is_dir()), key=lambda e: e.name, reverse=True)
    return [load_run(folder) for folder in folders]
