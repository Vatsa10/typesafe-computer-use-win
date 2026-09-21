"""Push-to-talk: record while a key is held, transcribe locally with whisper.cpp.

Nothing here is imported at module scope from the `voice` extra. The base install must keep
importing this module, because the daemon reports a missing extra rather than failing to start.
Audio never reaches the disk.
"""

from __future__ import annotations

import time
from collections.abc import Callable

from . import windows

SAMPLE_RATE = 16000  # what whisper.cpp wants; resampling anything else is the stream's job
BLOCK_FRAMES = 1024
FULL_SCALE = 32768.0


class VoiceUnavailable(RuntimeError):
    """The voice extra is not installed, or the machine has no input device."""


def _open_stream():
    """A 16 kHz mono int16 input stream. Raises VoiceUnavailable when there is nothing to record."""
    try:
        import sounddevice
    except ImportError as e:
        raise VoiceUnavailable("voice needs the extra: uv sync --extra voice") from e
    try:
        return sounddevice.RawInputStream(samplerate=SAMPLE_RATE, channels=1, dtype="int16", blocksize=BLOCK_FRAMES)
    except Exception as e:  # no device, device in use, driver refusal
        raise VoiceUnavailable(f"no usable microphone ({e})") from e


def record_while_held(
    vk: int,
    max_seconds: float,
    open_stream: Callable[[], object] | None = None,
    held: Callable[[int], bool] | None = None,
    clock: Callable[[], float] | None = None,
) -> list[bytes]:
    """Capture raw int16 frames while the key is physically down, up to the cap.

    The three callables are injected so the tests never touch an audio device.
    """
    open_stream = _open_stream if open_stream is None else open_stream
    held = windows.key_held if held is None else held
    clock = time.monotonic if clock is None else clock
    frames: list[bytes] = []
    deadline = clock() + max_seconds
    with open_stream() as stream:
        while held(vk) and clock() < deadline:
            data, _overflowed = stream.read(BLOCK_FRAMES)
            frames.append(bytes(data))
    return frames


def frames_to_audio(frames: list[bytes]):
    """Raw little-endian int16 to the float32 array whisper.cpp reads."""
    import numpy as np

    return np.frombuffer(b"".join(frames), dtype="<i2").astype("float32") / FULL_SCALE


_model = None
_model_name = ""


def _load_model(name: str):
    """One model per process. The first call downloads it, which is why it is not eager."""
    global _model, _model_name
    if _model is None or _model_name != name:
        try:
            from pywhispercpp.model import Model
        except ImportError as e:
            raise VoiceUnavailable("voice needs the extra: uv sync --extra voice") from e
        _model = Model(name, redirect_whispercpp_logs_to=None)
        _model_name = name
    return _model


def transcribe_frames(frames: list[bytes], model_name: str) -> str:
    """The transcript of one utterance, or an empty string when nothing was said."""
    if not frames:
        return ""
    segments = _load_model(model_name).transcribe(frames_to_audio(frames))
    return " ".join(segment.text.strip() for segment in segments).strip()


def listen(vk: int, max_seconds: float, model_name: str) -> str:
    """Hold-to-talk, start to finish: record while the key is down, then transcribe."""
    return transcribe_frames(record_while_held(vk, max_seconds), model_name)
