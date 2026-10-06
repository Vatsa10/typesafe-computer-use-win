"""Voice capture: record while a key is held or until the interface says stop, then transcribe.

Nothing here is imported at module scope from the `voice` extra. The base install must keep
importing this module, because the daemon reports a missing extra rather than failing to start.
Audio never reaches the disk.
"""

from __future__ import annotations

import os
import sys
import time
from array import array
from collections.abc import Callable

from . import windows

SAMPLE_RATE = 16000  # what whisper.cpp wants; resampling anything else is the stream's job
BLOCK_FRAMES = 1024
FULL_SCALE = 32768.0

# Measured, not guessed. Around thirty seconds of room tone from this machine's microphone, read
# through the recorder below, gave a per-block RMS with a median near 800 and a p90 near 1400, but
# a tail: the worst single block over all takes hit 3442 and sample peaks reached 5764, because the
# array mic applies its own gain. A level of 2000 let that tail register as speech and cut a
# recording short, so 5000 it is: roughly half again above the loudest quiet block, and well under
# speech at this gain. RMS rather than peak, so one keyboard spike does not read as a spoken word.
SILENCE_LEVEL = 5000

# whisper.cpp biases decoding toward the words in this prompt, which is how "Claude Code" stops
# coming back as "cloud code". Keep it one short sentence: a long prompt costs decode time on every
# utterance and makes the model echo the prompt back as if it had been spoken.
VOICE_PROMPT = (
    "Claude Code, VS Code, Chrome, WhatsApp, terminal, GitHub: say which monitor or window to use, "
    "scroll it, and stop, pause, resume or quit."
)


def voice_prompt() -> str:
    """The decoding bias, overridable with CLICKER_VOICE_PROMPT (empty disables it)."""
    override = os.environ.get("CLICKER_VOICE_PROMPT")
    return VOICE_PROMPT if override is None else override


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


def _samples(frames: list[bytes]) -> array:
    """The raw little-endian int16 bytes as signed shorts. Standard library only: audioop is gone."""
    block = array("h")
    block.frombytes(b"".join(frames))
    if sys.byteorder != "little":
        block.byteswap()
    return block


def _rms(block: array) -> float:
    """Loudness of a span of samples, 0 for an empty one."""
    if not block:
        return 0.0
    return (sum(sample * sample for sample in block) / len(block)) ** 0.5


def is_silent(frames: list[bytes], threshold: int = SILENCE_LEVEL) -> bool:
    """True when these frames carry nothing louder than the room. Empty frames are silent."""
    return _rms(_samples(frames)) < threshold


def trailing_silence_seconds(frames: list[bytes], threshold: int = SILENCE_LEVEL) -> float:
    """How many seconds of quiet sit at the end of the recording.

    Measured in BLOCK_FRAMES windows walked back from the last sample, so the answer does not
    depend on where the stream happened to split the frames.
    """
    block = _samples(frames)
    quiet = 0
    end = len(block)
    while end > 0:
        start = max(0, end - BLOCK_FRAMES)
        window = block[start:end]
        if _rms(window) >= threshold:
            break
        quiet += len(window)
        end = start
    return quiet / SAMPLE_RATE


def _record(
    done: Callable[[], bool],
    max_seconds: float,
    open_stream: Callable[[], object] | None,
    clock: Callable[[], float] | None,
    silence_seconds: float = 0.0,
    threshold: int = SILENCE_LEVEL,
) -> list[bytes]:
    """The one capture loop: read blocks until `done`, the cap, or a trailing silence after speech.

    Both recorders share this. The stream and the clock are injected so the tests never touch an
    audio device, and `done` is checked before the clock so a caller's own gate wins the first read.
    """
    open_stream = _open_stream if open_stream is None else open_stream
    clock = time.monotonic if clock is None else clock
    frames: list[bytes] = []
    heard_speech = False
    deadline = clock() + max_seconds
    with open_stream() as stream:
        while not done() and clock() < deadline:
            data, _overflowed = stream.read(BLOCK_FRAMES)
            frames.append(bytes(data))
            if silence_seconds <= 0:
                continue
            # Silence only ends a recording once something was actually said, or the drawn breath
            # before the first word would cut the user off.
            if not heard_speech:
                heard_speech = not is_silent([frames[-1]], threshold)
            elif trailing_silence_seconds(frames, threshold) >= silence_seconds:
                break
    return frames


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
    held = windows.key_held if held is None else held
    return _record(lambda: not held(vk), max_seconds, open_stream, clock)


def record_until(
    stop: Callable[[], bool],
    max_seconds: float,
    open_stream: Callable[[], object] | None = None,
    clock: Callable[[], float] | None = None,
    silence_seconds: float = 0.0,
) -> list[bytes]:
    """Capture raw int16 frames until `stop` says so, the cap runs out, or speech trails off.

    This is the recorder the command bar drives: the microphone button sets a flag instead of
    holding a key down. Same frames, same blocks and same cap as record_while_held, so
    transcribe_frames takes the output unchanged. `silence_seconds` of 0 disables the silence stop.
    """
    return _record(stop, max_seconds, open_stream, clock, silence_seconds)


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
    # Per call, not per model: the cached model outlives an environment override of the bias.
    segments = _load_model(model_name).transcribe(frames_to_audio(frames), initial_prompt=voice_prompt())
    return " ".join(segment.text.strip() for segment in segments).strip()


def listen(vk: int, max_seconds: float, model_name: str) -> str:
    """Hold-to-talk, start to finish: record while the key is down, then transcribe."""
    return transcribe_frames(record_while_held(vk, max_seconds), model_name)


def listen_until(stop: Callable[[], bool], max_seconds: float, model_name: str, silence_seconds: float = 0.0) -> str:
    """Click-to-talk, start to finish: record until the interface stops it, then transcribe."""
    return transcribe_frames(record_until(stop, max_seconds, silence_seconds=silence_seconds), model_name)
