"""The history reader, against folders written the way a killed run leaves them."""

from __future__ import annotations

import json
from pathlib import Path

from typesafe_computer_use_win.runs_index import StepRecord, list_runs, load_run, step_answers, steps_of

COMPLETE = {
    "goal": "open the billing page",
    "act": True,
    "steps_taken": 3,
    "outcome": "done",
    "answer": "billing shows $42 due",
    "goal_achieved": True,
    "seconds": 12.5,
    "timing": {"total": 12.5},
    "history": ["click [4]", "click [9]", "type 'billing'"],
    "config": {"goal": "open the billing page"},
}


def make_run(root: Path, name: str, summary: dict | str | None = None, steps: int = 0) -> Path:
    folder = root / name
    folder.mkdir(parents=True)
    if isinstance(summary, str):
        (folder / "run.json").write_text(summary, encoding="utf-8")
    elif summary is not None:
        (folder / "run.json").write_text(json.dumps(summary), encoding="utf-8")
    for n in range(1, steps + 1):
        write_step(folder, n)
    return folder


def write_step(folder: Path, n: int, parts: tuple[str, ...] = ("-raw.png", ".png", "-payload.txt", "-answers.json")) -> None:
    for part in parts:
        target = folder / f"step-{n:03d}{part}"
        if part == "-answers.json":
            target.write_text(json.dumps({"kind": "click", "confidence": 0.9}), encoding="utf-8")
        elif part == "-payload.txt":
            target.write_text("payload", encoding="utf-8")
        else:
            target.write_bytes(b"\x89PNG")


def test_complete_run_parses_fully(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-101500", COMPLETE, steps=3)
    run = load_run(folder)
    assert run.name == "2026-09-21-101500"
    assert run.path == folder
    assert run.goal == "open the billing page"
    assert run.outcome == "done"
    assert run.answer == "billing shows $42 due"
    assert run.goal_achieved is True
    assert run.seconds == 12.5
    assert run.steps_taken == 3
    assert run.acted is True


def test_list_runs_is_newest_first(tmp_path: Path) -> None:
    make_run(tmp_path, "2026-09-20-090000", COMPLETE)
    make_run(tmp_path, "2026-09-21-101500", COMPLETE)
    make_run(tmp_path, "2026-09-19-235959", COMPLETE)
    assert [r.name for r in list_runs(tmp_path)] == ["2026-09-21-101500", "2026-09-20-090000", "2026-09-19-235959"]


def test_run_without_summary_still_lists_as_unknown(tmp_path: Path) -> None:
    """Killed before the finally block: no run.json, but the step files are still there."""
    make_run(tmp_path, "2026-09-21-110000", None, steps=2)
    (run,) = list_runs(tmp_path)
    assert run.outcome == "unknown"
    assert run.goal == ""
    assert run.answer is None
    assert run.goal_achieved is None
    assert run.seconds is None
    assert run.acted is False
    assert run.steps_taken == 2  # recovered by counting what was written


def test_corrupt_summary_degrades_instead_of_raising(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-120000", '{"goal": "half a doc", "outcome": "do', steps=1)
    run = load_run(folder)
    assert run.outcome == "unknown"
    assert run.goal == ""
    assert run.steps_taken == 1


def test_summary_of_the_wrong_shape_degrades(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-121000", "[1, 2, 3]")
    assert load_run(folder).outcome == "unknown"


def test_stray_file_in_the_root_is_skipped(tmp_path: Path) -> None:
    make_run(tmp_path, "2026-09-21-130000", COMPLETE)
    (tmp_path / "notes.txt").write_text("not a run", encoding="utf-8")
    (tmp_path / "latest.json").write_text("{}", encoding="utf-8")
    assert [r.name for r in list_runs(tmp_path)] == ["2026-09-21-130000"]


def test_missing_root_is_an_empty_history(tmp_path: Path) -> None:
    assert list_runs(tmp_path / "runs") == []


def test_root_that_is_a_file_is_an_empty_history(tmp_path: Path) -> None:
    target = tmp_path / "runs"
    target.write_text("not a folder", encoding="utf-8")
    assert list_runs(target) == []


def test_steps_sort_numerically_not_lexically(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-140000", COMPLETE)
    for n in (10, 2, 1, 100):
        write_step(folder, n)
    assert [s.number for s in steps_of(folder)] == [1, 2, 10, 100]


def test_steps_with_fewer_than_three_digits_are_read(tmp_path: Path) -> None:
    """Older runs numbered step files without padding."""
    folder = make_run(tmp_path, "2026-09-21-141000", COMPLETE)
    (folder / "step-2-raw.png").write_bytes(b"\x89PNG")
    (folder / "step-10.png").write_bytes(b"\x89PNG")
    assert [(s.number, s.raw is not None, s.annotated is not None) for s in steps_of(folder)] == [
        (2, True, False),
        (10, False, True),
    ]


def test_partial_step_set_reports_none_for_what_is_absent(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-150000", None)
    write_step(folder, 1, ("-raw.png", "-payload.txt"))
    (step,) = steps_of(folder)
    assert step.raw is not None and step.payload is not None
    assert step.annotated is None
    assert step.answers is None
    assert step_answers(step) == {}


def test_step_answers_parses_and_tolerates_corruption(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-151000", None, steps=1)
    (step,) = steps_of(folder)
    assert step_answers(step) == {"kind": "click", "confidence": 0.9}
    assert step.answers is not None
    step.answers.write_text('{"kind": "cli', encoding="utf-8")
    assert step_answers(step) == {}
    assert step_answers(StepRecord(number=7)) == {}


def test_unrelated_files_in_a_run_folder_are_not_steps(tmp_path: Path) -> None:
    folder = make_run(tmp_path, "2026-09-21-160000", COMPLETE, steps=1)
    (folder / "answer-raw.png").write_bytes(b"\x89PNG")
    (folder / "run.log").write_text("log", encoding="utf-8")
    (folder / "step-notes.txt").write_text("nope", encoding="utf-8")
    assert [s.number for s in steps_of(folder)] == [1]


def test_non_ascii_payload_reads_back(tmp_path: Path) -> None:
    """Payloads hold screen text, and the Windows default encoding would raise on it."""
    folder = make_run(tmp_path, "2026-09-21-170000", None)
    text = "Rechnung fällig — 42 € · naïve café · 日本語"
    (folder / "step-001-payload.txt").write_text(text, encoding="utf-8")
    (step,) = steps_of(folder)
    assert step.payload is not None
    assert step.payload.read_text(encoding="utf-8") == text


def test_non_ascii_summary_and_answers_read_back(tmp_path: Path) -> None:
    summary = dict(COMPLETE, goal="öffne die Rechnung", answer="42 € fällig")
    folder = make_run(tmp_path, "2026-09-21-171000", summary)
    (folder / "step-001-answers.json").write_text(json.dumps({"chosen": "click 'Löschen'"}), encoding="utf-8")
    run = load_run(folder)
    assert run.goal == "öffne die Rechnung"
    assert run.answer == "42 € fällig"
    (step,) = steps_of(folder)
    assert step_answers(step) == {"chosen": "click 'Löschen'"}
