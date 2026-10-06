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
# The same two jobs on an OpenAI-compatible endpoint. Both read a screenshot for the answer, so both
# have to be vision models. Override them when an account or a gateway offers something better.
DEFAULT_OPENAI_WRITER_MODEL = "gpt-4.1-mini"
DEFAULT_OPENAI_ANSWER_MODEL = "gpt-4.1"
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


def writer_provider() -> str:
    """Which service writes free text: "anthropic" or "openai".

    Set CLICKER_WRITER_PROVIDER to force one. Otherwise an OpenAI key wins, including when both
    are present, so swapping provider is a matter of which key is in .env rather than a code
    change. Anthropic remains fully supported and is used whenever it is the only key set.
    """
    explicit = os.environ.get("CLICKER_WRITER_PROVIDER", "").strip().lower()
    if explicit in {"anthropic", "openai"}:
        return explicit
    if os.environ.get("OPENAI_API_KEY"):
        return "openai"
    return "anthropic"


def openai_base_url() -> str | None:
    """An OpenAI-compatible endpoint other than OpenAI's own: Groq, OpenRouter, or a local server.

    The API shape is the same, so pointing this somewhere else is the whole configuration.
    """
    return os.environ.get("OPENAI_BASE_URL", "").strip() or None


def writer_model() -> str:
    default = DEFAULT_OPENAI_WRITER_MODEL if writer_provider() == "openai" else DEFAULT_WRITER_MODEL
    return os.environ.get("CLICKER_WRITER_MODEL", default)


def answer_model() -> str:
    default = DEFAULT_OPENAI_ANSWER_MODEL if writer_provider() == "openai" else DEFAULT_ANSWER_MODEL
    return os.environ.get("CLICKER_ANSWER_MODEL", default)


def email() -> str | None:
    return os.environ.get("CLICKER_EMAIL") or None


# The daemon's global hotkeys. Ctrl+Alt is the least contested corner of the Windows keyboard:
# Win+key belongs to the shell, and Ctrl+Shift+key to whatever app has focus.
DEFAULT_HOTKEYS = {
    "talk": "ctrl+alt+space",
    # The bar is the one most people will use: press, speak, done. Right alt is free on a standard
    # layout, so it needs no chord; on a layout where it is AltGr, change it in Settings.
    "bar": "rightalt",
    "goal": "ctrl+alt+g",
    "pause": "ctrl+alt+p",
    "abort": "ctrl+alt+x",
    "quit": "ctrl+alt+q",
}
DEFAULT_WHISPER_MODEL = "base.en"
# How long a pause ends a recording in the command bar. The bar records until clicked again, and
# people do not click again, so a pause stops it. Short enough not to sit there, long enough to
# survive thinking mid-sentence.
DEFAULT_BAR_SILENCE_SECONDS = 1.6
DEFAULT_VOICE_MAX_SECONDS = 30.0
DEFAULT_VOICE_MIN_CONFIDENCE = 0.55


def hotkey(name: str) -> str:
    return os.environ.get(f"CLICKER_HOTKEY_{name.upper()}", DEFAULT_HOTKEYS[name])


def whisper_model() -> str:
    return os.environ.get("CLICKER_WHISPER_MODEL", DEFAULT_WHISPER_MODEL)


def speak_answers() -> bool:
    """Whether answers are read out loud. On by default: an answer you have to go and read is a
    log line, and the point of asking out loud is being answered out loud."""
    return os.environ.get("CLICKER_SPEAK", "1").strip().lower() not in {"0", "false", "no", "off"}


def bar_silence_seconds() -> float:
    """Seconds of quiet that end a recording in the bar. Zero waits for the click instead."""
    return float(os.environ.get("CLICKER_BAR_SILENCE", DEFAULT_BAR_SILENCE_SECONDS))


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
