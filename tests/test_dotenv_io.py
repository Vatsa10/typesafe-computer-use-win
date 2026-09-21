"""Tests for the .env reader/rewriter: it must preserve the file, not regenerate it."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from typesafe_computer_use_win import config
from typesafe_computer_use_win.dotenv_io import merge_lines, quote, read_env, write_env


def _read_back(path: Path, monkeypatch: pytest.MonkeyPatch) -> dict[str, str]:
    """Parse a file through config.load_dotenv itself, into a clean environment."""
    monkeypatch.setattr(os, "environ", {})
    config.load_dotenv(path)
    return dict(os.environ)


def test_update_in_place_keeps_order_and_comments(tmp_path: Path) -> None:
    path = tmp_path / ".env"
    path.write_text(
        "# leading comment\nTYPESAFE_API_KEY=old\n\n# about the browser\nCLICKER_BROWSER=Firefox\nCLICKER_EMAIL=a@b.c\n",
        encoding="utf-8",
    )
    write_env(path, {"CLICKER_BROWSER": "Google Chrome"})
    assert path.read_text(encoding="utf-8").splitlines() == [
        "# leading comment",
        "TYPESAFE_API_KEY=old",
        "",
        "# about the browser",
        'CLICKER_BROWSER="Google Chrome"',
        "CLICKER_EMAIL=a@b.c",
    ]


def test_new_key_is_appended(tmp_path: Path) -> None:
    path = tmp_path / ".env"
    path.write_text("# head\nCLICKER_EMAIL=a@b.c\n", encoding="utf-8")
    write_env(path, {"CLICKER_WHISPER_MODEL": "small.en"})
    assert path.read_text(encoding="utf-8") == "# head\nCLICKER_EMAIL=a@b.c\nCLICKER_WHISPER_MODEL=small.en\n"


def test_empty_value_is_kept_as_bare_key(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = tmp_path / ".env"
    path.write_text("CLICKER_BROWSER=Firefox\n", encoding="utf-8")
    write_env(path, {"CLICKER_BROWSER": ""})
    assert path.read_text(encoding="utf-8") == "CLICKER_BROWSER=\n"
    assert read_env(path) == {"CLICKER_BROWSER": ""}
    assert _read_back(path, monkeypatch) == {"CLICKER_BROWSER": ""}


def test_other_keys_are_left_completely_alone(tmp_path: Path) -> None:
    path = tmp_path / ".env"
    original = "TYPESAFE_API_KEY=sk-secret\nANTHROPIC_API_KEY='quoted-secret'\nCLICKER_EMAIL=a@b.c\n"
    path.write_text(original, encoding="utf-8")
    write_env(path, {"CLICKER_EMAIL": "z@y.x"})
    text = path.read_text(encoding="utf-8")
    assert "TYPESAFE_API_KEY=sk-secret" in text
    assert "ANTHROPIC_API_KEY='quoted-secret'" in text
    assert text.endswith("CLICKER_EMAIL=z@y.x\n")


def test_absent_file_reads_empty_and_can_be_written_from_scratch(tmp_path: Path) -> None:
    path = tmp_path / ".env"
    assert read_env(path) == {}
    write_env(path, {"CLICKER_BROWSER": "Firefox", "CLICKER_EMAIL": ""})
    assert path.read_text(encoding="utf-8") == "CLICKER_BROWSER=Firefox\nCLICKER_EMAIL=\n"
    assert read_env(path) == {"CLICKER_BROWSER": "Firefox", "CLICKER_EMAIL": ""}


@pytest.mark.parametrize(
    "value",
    [
        "Google Chrome",
        "a value with # a hash",
        'say "hi" now',
        "it's fine",
        "#leading-hash-is-not-a-comment",
        "plain",
        "",
        "a\tb",
    ],
)
def test_round_trip_through_config_parser(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, value: str) -> None:
    path = tmp_path / ".env"
    values = {"CLICKER_BROWSER": value}
    write_env(path, values)
    assert read_env(path) == values
    assert _read_back(path, monkeypatch) == values


def test_quote_leaves_values_that_quoting_cannot_help(tmp_path: Path) -> None:
    # load_dotenv strips surrounding quote characters unconditionally, so a value that begins or
    # ends with one cannot survive either way; we at least do not add more.
    assert quote('"shouty"') == '"shouty"'
    assert quote("plain") == "plain"
    assert quote("two words") == '"two words"'
    assert quote("") == ""


def test_crlf_file_stays_crlf(tmp_path: Path) -> None:
    path = tmp_path / ".env"
    path.write_bytes(b"# head\r\nCLICKER_EMAIL=a@b.c\r\n")
    write_env(path, {"CLICKER_EMAIL": "z@y.x", "CLICKER_BROWSER": "Firefox"})
    assert path.read_bytes() == b"# head\r\nCLICKER_EMAIL=z@y.x\r\nCLICKER_BROWSER=Firefox\r\n"


def test_lf_file_stays_lf(tmp_path: Path) -> None:
    path = tmp_path / ".env"
    path.write_bytes(b"CLICKER_EMAIL=a@b.c\n")
    write_env(path, {"CLICKER_EMAIL": "z@y.x"})
    assert path.read_bytes() == b"CLICKER_EMAIL=z@y.x\n"


def test_merge_lines_is_pure_and_leaves_input_untouched() -> None:
    existing = ["# c", "A=1", "", "B=2"]
    snapshot = list(existing)
    assert merge_lines(existing, {"B": "3", "C": "4"}) == ["# c", "A=1", "", "B=3", "C=4"]
    assert existing == snapshot


def test_merge_lines_handles_no_values_and_no_lines() -> None:
    assert merge_lines(["# c", "A=1"], {}) == ["# c", "A=1"]
    assert merge_lines([], {"A": "1"}) == ["A=1"]
    assert merge_lines([], {}) == []


def test_merge_lines_updates_a_key_written_with_spaces_and_quotes() -> None:
    assert merge_lines(["  CLICKER_EMAIL = 'a@b.c' "], {"CLICKER_EMAIL": "z@y.x"}) == ["CLICKER_EMAIL=z@y.x"]


def test_merge_lines_updates_only_the_first_of_a_duplicated_key() -> None:
    # The second occurrence is not in `values` any more, so it is left exactly as it was.
    assert merge_lines(["A=1", "A=2"], {"A": "9"}) == ["A=9", "A=2"]
