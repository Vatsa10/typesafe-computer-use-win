"""The Log echo sink: a UI can watch every line the run loop logs."""

from __future__ import annotations

import inspect

from typesafe_computer_use_win.report import Log
from typesafe_computer_use_win.runner import run


def test_lines_reach_the_sink_in_order(tmp_path):
    seen: list[str] = []
    log = Log(tmp_path / "run.log", seen.append)
    log("first")
    log("second")
    log()
    assert seen == ["first", "second", ""]


def test_lines_still_reach_the_file(tmp_path):
    path = tmp_path / "run.log"
    log = Log(path, lambda _msg: None)
    log("first")
    log("second")
    assert path.read_text(encoding="utf-8") == "first\nsecond\n"


def test_file_keeps_non_ascii_text(tmp_path):
    """Screen text is not ASCII and the Windows default codec is cp1252."""
    path = tmp_path / "run.log"
    log = Log(path, lambda _msg: None)
    log("café — ✓")
    assert path.read_text(encoding="utf-8") == "café — ✓\n"


def test_a_raising_sink_does_not_propagate_and_the_file_still_gets_the_line(tmp_path):
    path = tmp_path / "run.log"
    calls: list[str] = []

    def broken(msg: str) -> None:
        calls.append(msg)
        raise RuntimeError("the UI died")

    log = Log(path, broken)
    log("one")  # must not raise
    log("two")
    assert calls == ["one", "two"]  # still called after the first failure
    assert path.read_text(encoding="utf-8") == "one\ntwo\n"


def test_the_default_sink_is_print(tmp_path, capsys):
    log = Log(tmp_path / "run.log")
    log("to stdout")
    assert capsys.readouterr().out == "to stdout\n"
    assert (tmp_path / "run.log").read_text(encoding="utf-8") == "to stdout\n"


def test_an_explicit_sink_leaves_stdout_untouched(tmp_path, capsys):
    seen: list[str] = []
    log = Log(tmp_path / "run.log", seen.append)
    log("not printed")
    assert capsys.readouterr().out == ""
    assert seen == ["not printed"]


def test_a_pathless_log_still_echoes():
    seen: list[str] = []
    Log(None, seen.append)("no file")
    assert seen == ["no file"]


def test_run_signature_keeps_control_third_and_adds_echo_last():
    params = list(inspect.signature(run).parameters.values())
    assert [p.name for p in params] == ["cfg", "ctx_factory", "control", "echo"]
    assert params[2].default is None
    assert params[3].default is print
