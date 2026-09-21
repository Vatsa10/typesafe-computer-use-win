"""Tunables, the site catalog, and environment loading."""

from __future__ import annotations

import os
from pathlib import Path

MIN_OCR_CONFIDENCE = 0.3
MAX_OPTIONS = 255  # TypeSafe Choice ceiling
ABORT_CORNER_PX = 4
DEFAULT_MIN_CONFIDENCE = 0.4
DEFAULT_STEPS = 100
DEFAULT_DELAY = 2.0
DEFAULT_WRITER_MODEL = "claude-haiku-4-5"
DEFAULT_ANSWER_MODEL = "claude-sonnet-5"  # runs once per run, on a screenshot: worth a stronger reader
DEFAULT_BROWSER = "Google Chrome"

# The curated core of the site catalog. These are always offered, whatever the browser holds, and
# they outrank anything discovered. Everything else is learned: see catalog.py.
SITES: dict[str, str] = {
    "github": "https://github.com/",
    "gmail": "https://mail.google.com/",
    "google_calendar": "https://calendar.google.com/",
    "launchdarkly": "https://app.launchdarkly.com/",
    "linear": "https://linear.app/",
    "notion": "https://www.notion.so/",
    "slack": "https://app.slack.com/",
    "typesafe_console": "https://console.typesafe.ai/",
}


def load_dotenv(path: Path) -> None:
    """Set KEY=VALUE lines from a .env file into the environment unless already set."""
    if not path.is_file():
        return
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        os.environ.setdefault(key.strip(), value.strip().strip("'\""))


def browser() -> str:
    return os.environ.get("CLICKER_BROWSER", DEFAULT_BROWSER)


def writer_model() -> str:
    return os.environ.get("CLICKER_WRITER_MODEL", DEFAULT_WRITER_MODEL)


def answer_model() -> str:
    return os.environ.get("CLICKER_ANSWER_MODEL", DEFAULT_ANSWER_MODEL)


def email() -> str | None:
    return os.environ.get("CLICKER_EMAIL") or None


# The daemon's global hotkeys. Ctrl+Alt is the least contested corner of the Windows keyboard:
# Win+key belongs to the shell, and Ctrl+Shift+key to whatever app has focus.
DEFAULT_HOTKEYS = {
    "talk": "ctrl+alt+space",
    "goal": "ctrl+alt+g",
    "pause": "ctrl+alt+p",
    "abort": "ctrl+alt+x",
    "quit": "ctrl+alt+q",
}
DEFAULT_WHISPER_MODEL = "base.en"
DEFAULT_VOICE_MAX_SECONDS = 30.0
DEFAULT_VOICE_MIN_CONFIDENCE = 0.55


def hotkey(name: str) -> str:
    return os.environ.get(f"CLICKER_HOTKEY_{name.upper()}", DEFAULT_HOTKEYS[name])


def whisper_model() -> str:
    return os.environ.get("CLICKER_WHISPER_MODEL", DEFAULT_WHISPER_MODEL)


def voice_max_seconds() -> float:
    return float(os.environ.get("CLICKER_VOICE_MAX_SECONDS", DEFAULT_VOICE_MAX_SECONDS))


def voice_min_confidence() -> float:
    return float(os.environ.get("CLICKER_VOICE_MIN_CONFIDENCE", DEFAULT_VOICE_MIN_CONFIDENCE))


# The dynamic site catalog, built from the browser's own bookmarks and history. Off means the
# classifier sees only the pinned SITES above, which is how this worked before.
DEFAULT_CATALOG_LIMIT = 30  # options handed to the site question; the cap is 255, but mass thins fast


def catalog_enabled() -> bool:
    return os.environ.get("CLICKER_CATALOG", "1").strip().lower() not in {"0", "false", "no", "off"}


def catalog_titles() -> bool:
    """Whether a page title may become a site's label. Off means labels are bare domains, so no
    page title ever reaches the model."""
    return os.environ.get("CLICKER_CATALOG_TITLES", "1").strip().lower() not in {"0", "false", "no", "off"}


def catalog_limit() -> int:
    return int(os.environ.get("CLICKER_CATALOG_LIMIT", DEFAULT_CATALOG_LIMIT))
