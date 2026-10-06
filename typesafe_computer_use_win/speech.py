"""Speech out: say an answer out loud instead of only printing it.

The sibling of `voice`, in the other direction, and it follows the same rules. Nothing heavy is
imported at module scope, so the base install keeps importing this module on a machine with no
speech at all; every failure is tolerated rather than raised into a run, because an answer that
could not be spoken is still an answer; and the engine is injected so no test ever makes the
machine talk.

The default engine is the Windows speech API, reached over COM as `SAPI.SpVoice`. It is offline,
free, instant and already installed, and `comtypes` is already a dependency (uiautomation pulls it
in), so this needs no new package. A PowerShell subprocess would also work but costs roughly half a
second of process start per sentence, so it is not the default and is not used here.

Speaking is asynchronous by SAPI's own flag rather than a Python thread: `Speak` returns at once
and the voice runs on its own, so the hotkeys keep working while the program talks. The same call
purges whatever was already queued, so a new answer replaces the old one instead of talking over
it, and `stop()` purges with an empty utterance, which cuts the current sentence off mid-word.
"""

from __future__ import annotations

import contextlib
import os
import re
from collections.abc import Callable

# SpeechVoiceSpeakFlags, from the SAPI 5 headers. Async hands control straight back; purge drops
# whatever is still queued or being said, which is both how `stop` works and how a new answer
# replaces the one in progress.
SVSFL_DEFAULT = 0
SVSF_ASYNC = 1
SVSF_PURGE_BEFORE_SPEAK = 2

# The cap on a spoken answer, in characters. Measured, not guessed: the ten runs in `runs/` on this
# machine hold six answers, of 88, 232, 382, 405, 458 and 480 characters, so the typical answer is
# three to five sentences and the longest is nearly half a kilobyte. Read aloud at the default SAPI
# rate that is close to forty seconds, which is far past the point where a listener stops waiting
# and reads the panel instead. 240 characters is the first two sentences of a typical answer, about
# fifteen seconds, and it leaves the short answers (the 88 one) untouched.
SPEAK_LIMIT = 240

_URL = re.compile(r"\b(?:https?://|www\.)(\S+)", re.IGNORECASE)
# A path-shaped run of non-space characters: a drive letter, a UNC share, or anything with a
# separator and an extension. Deliberately narrow, so ordinary prose with a slash survives.
_PATH = re.compile(r"(?<![\w.])(?:[A-Za-z]:[\\/]|\\\\|\.{1,2}[\\/]|~[\\/]|/)[^\s\"'<>|]*")
_SENTENCE_END = re.compile(r"[.!?](?=\s|$)")

_voice = None
_voice_tried = False


def _create_sapi():
    """The SAPI voice object. Imported here, never at module scope, and never cached by this call."""
    import comtypes.client

    return comtypes.client.CreateObject("SAPI.SpVoice")


def reset() -> None:
    """Forget the cached engine, so the next call builds a fresh one."""
    global _voice, _voice_tried
    _voice = None
    _voice_tried = False


def _sapi(create: Callable[[], object] | None = None):
    """The one voice object for this process, or None when the machine cannot speak.

    Created on first use, because building the COM object costs tens of milliseconds and loads the
    speech stack, and reused after that: a per-call object would re-open the audio device for every
    sentence and lose the handle `stop` needs. A failure is remembered too, so a machine without
    speech does not pay for a doomed COM call on every answer.
    """
    global _voice, _voice_tried
    if _voice is not None:
        return _voice
    if _voice_tried and create is None:
        return None
    _voice_tried = True
    try:
        _voice = (create or _create_sapi)()
    except Exception:  # no COM, no speech stack, no audio device, a locked session
        _voice = None
    return _voice


def available(create: Callable[[], object] | None = None) -> bool:
    """True when something on this machine can speak."""
    return _sapi(create) is not None


def voices(create: Callable[[], object] | None = None) -> list[str]:
    """The names of the installed voices, or an empty list when there is no speech at all."""
    voice = _sapi(create)
    if voice is None:
        return []
    try:
        tokens = voice.GetVoices()
        return [str(tokens.Item(i).GetDescription()) for i in range(tokens.Count)]
    except Exception:
        return []


def stop(create: Callable[[], object] | None = None) -> None:
    """Cut off whatever is being said. Safe when nothing is speaking, and safe with no engine."""
    _murf_stop()
    voice = _sapi(create)
    if voice is None:
        return
    # An empty utterance with the purge flag: the queue is dropped and the current word ends.
    with contextlib.suppress(Exception):
        voice.Speak("", SVSF_ASYNC | SVSF_PURGE_BEFORE_SPEAK)


def speak(text: str, blocking: bool = False, create: Callable[[], object] | None = None) -> bool:
    """Say `text` out loud. False when nothing could speak it, and it never raises.

    Asynchronous by default, so the caller stays responsive while the answer plays, and always
    purging, so a new answer replaces the one in progress rather than queueing behind it.
    """
    spoken = (text or "").strip()
    if not spoken:
        return False
    if _engine() == "murf" and _murf_speak(spoken, blocking):
        return True
    voice = _sapi(create)
    if voice is None:
        return False
    flags = SVSF_PURGE_BEFORE_SPEAK | (SVSFL_DEFAULT if blocking else SVSF_ASYNC)
    try:
        voice.Speak(spoken, flags)
    except Exception:  # the device went away mid-sentence; an unspoken answer is not a failed run
        return False
    return True


def speakable(text: str, limit: int = SPEAK_LIMIT) -> str:
    """`text` with what should not be read aloud taken out, capped at a sentence boundary.

    A URL comes down to its host and a path to its last part, because letter-by-letter run folders
    and query strings are unbearable out loud, and the whole thing is cut at the last sentence that
    fits so the voice stops on a full stop instead of mid-word.
    """
    trimmed = _URL.sub(lambda m: _host(m.group(0)), text or "")
    trimmed = _PATH.sub(lambda m: _last_part(m.group(0)), trimmed)
    trimmed = re.sub(r"\s+", " ", trimmed).strip()
    if len(trimmed) <= limit:
        return trimmed
    head = trimmed[: limit + 1]
    ends = [m.end() for m in _SENTENCE_END.finditer(head)]
    if ends and ends[-1] >= limit // 3:
        return head[: ends[-1]].strip()
    cut = head.rfind(" ")
    return (head[:cut] if cut > 0 else trimmed[:limit]).strip()


def _host(url: str) -> str:
    """The host of a URL, without the scheme, a leading www. or anything after it."""
    body = re.sub(r"^https?://", "", url, flags=re.IGNORECASE)
    body = re.sub(r"^www\.", "", body, flags=re.IGNORECASE)
    host = body.split("/")[0].split("?")[0].split("#")[0]
    return host.rstrip(".,;:!?)") or url


def _last_part(path: str) -> str:
    """The final component of a path, which is the only part worth hearing."""
    tail = re.split(r"[\\/]", path.rstrip("\\/"))[-1]
    return tail or path


# --- the optional second engine ------------------------------------------------------------------
# Off unless CLICKER_TTS=murf and MURF_API_KEY is in the environment. `config.load_dotenv` puts the
# key there; this module never reads .env itself and never prints or logs the key.


def _engine() -> str:
    return os.environ.get("CLICKER_TTS", "").strip().lower()


def murf_voice() -> str:
    """The Murf voice to synthesise with, overridable with CLICKER_MURF_VOICE."""
    return os.environ.get("CLICKER_MURF_VOICE", "Natalie").strip() or "Natalie"


def _murf_audio(text: str) -> bytes | None:
    """WAV bytes from Murf's REST API, or None on any failure, so the caller falls back to SAPI.

    POST https://api.murf.ai/v1/speech/generate with an `api-key` header returns JSON whose
    `audioFile` is a URL to the rendered audio; WAV because that is what winsound can play.
    """
    key = os.environ.get("MURF_API_KEY", "").strip()
    if not key:
        return None
    import json
    import urllib.request

    body = json.dumps({"text": text, "voiceId": murf_voice(), "locale": "en-US", "format": "WAV"}).encode("utf-8")
    request = urllib.request.Request(
        "https://api.murf.ai/v1/speech/generate",
        data=body,
        headers={"api-key": key, "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            payload = json.loads(response.read().decode("utf-8"))
        url = payload.get("audioFile")
        if not url:
            return None
        with urllib.request.urlopen(url, timeout=20) as audio:
            return audio.read()
    except Exception:  # no network, a rejected or exhausted key, a changed response shape
        return None


_murf_file: str | None = None


def _murf_speak(text: str, blocking: bool) -> bool:
    """Render through Murf and play it. False on any failure, which means: use SAPI instead."""
    audio = _murf_audio(text)
    if not audio:
        return False
    global _murf_file
    import tempfile
    import winsound

    _murf_stop()
    try:
        descriptor, path = tempfile.mkstemp(prefix="winclicker-tts-", suffix=".wav")
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(audio)
        _murf_file = path
        flags = winsound.SND_FILENAME | (0 if blocking else winsound.SND_ASYNC)
        winsound.PlaySound(_murf_file, flags)
    except Exception:
        return False
    return True


def _murf_stop() -> None:
    """Silence a Murf clip that is playing and drop its temporary file."""
    global _murf_file
    if _murf_file is None:
        return
    with contextlib.suppress(Exception):
        import winsound

        winsound.PlaySound(None, winsound.SND_PURGE)
    with contextlib.suppress(OSError):
        os.unlink(_murf_file)
    _murf_file = None
