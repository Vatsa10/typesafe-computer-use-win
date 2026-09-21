import os

from typesafe_computer_use_win.config import load_dotenv
from typesafe_computer_use_win.writer import valid_url


def test_dotenv_sets_only_missing_keys(tmp_path, monkeypatch):
    monkeypatch.setenv("CLICKER_TEST_PRESENT", "keep")
    monkeypatch.delenv("CLICKER_TEST_NEW", raising=False)
    (tmp_path / ".env").write_text('# comment\nCLICKER_TEST_PRESENT=override\nCLICKER_TEST_NEW="quoted value"\nbroken line\n')
    load_dotenv(tmp_path / ".env")
    assert os.environ["CLICKER_TEST_PRESENT"] == "keep"
    assert os.environ["CLICKER_TEST_NEW"] == "quoted value"


def test_dotenv_missing_file_is_fine(tmp_path):
    load_dotenv(tmp_path / "nope.env")


def test_valid_url():
    assert valid_url("https://www.cnn.com")
    assert valid_url("https://news.ycombinator.com/newest")
    assert not valid_url("http://www.cnn.com")
    assert not valid_url("https://localhost")
    assert not valid_url("https://www.cnn.com/a b")
    assert not valid_url("")


def test_hotkeys_have_defaults_and_read_the_environment(monkeypatch):
    from typesafe_computer_use_win import config

    monkeypatch.delenv("CLICKER_HOTKEY_TALK", raising=False)
    assert config.hotkey("talk") == "ctrl+alt+space"
    monkeypatch.setenv("CLICKER_HOTKEY_TALK", "ctrl+shift+v")
    assert config.hotkey("talk") == "ctrl+shift+v"


def test_every_daemon_hotkey_has_a_default():
    from typesafe_computer_use_win import config

    assert set(config.DEFAULT_HOTKEYS) == {"talk", "goal", "pause", "abort", "quit"}


def test_voice_settings_have_defaults_and_read_the_environment(monkeypatch):
    from typesafe_computer_use_win import config

    for name in ("CLICKER_WHISPER_MODEL", "CLICKER_VOICE_MAX_SECONDS", "CLICKER_VOICE_MIN_CONFIDENCE"):
        monkeypatch.delenv(name, raising=False)
    assert config.whisper_model() == "base.en"
    assert config.voice_max_seconds() == 30.0
    assert config.voice_min_confidence() == 0.55
    monkeypatch.setenv("CLICKER_VOICE_MAX_SECONDS", "8")
    assert config.voice_max_seconds() == 8.0
