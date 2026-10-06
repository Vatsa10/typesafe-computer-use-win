"""Push-to-talk capture. No microphone is touched: the stream and the model are injected."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import voice


def iter_clock():
    ticks = iter([0.0, 0.1, 0.2, 0.3, 0.4])
    return lambda: next(ticks, 99.0)


def test_the_module_imports_without_the_voice_extra():
    """CI installs the base set only, so nothing heavy may be imported at module scope."""
    assert not {"numpy", "np", "sounddevice", "sd", "pywhispercpp"} & set(vars(voice))


def test_recording_stops_when_the_key_comes_up():
    reads = [b"\x01\x00", b"\x02\x00", b"\x03\x00"]
    held = iter([True, True, False])

    class FakeStream:
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self, n):
            return reads.pop(0), False

    frames = voice.record_while_held(
        vk=0x20, max_seconds=30, open_stream=lambda: FakeStream(), held=lambda vk: next(held), clock=iter_clock()
    )
    assert frames == [b"\x01\x00", b"\x02\x00"]


def test_recording_stops_at_the_cap_even_while_the_key_is_down():
    class FakeStream:
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self, n):
            return b"\x00\x00", False

    ticks = iter([0.0, 0.5, 1.5, 2.5])
    frames = voice.record_while_held(
        vk=0x20,
        max_seconds=1.0,
        open_stream=lambda: FakeStream(),
        held=lambda vk: True,
        clock=lambda: next(ticks, 99.0),
    )
    assert len(frames) < 4, "the cap must end an utterance the user never releases"


def test_frames_become_float32_between_minus_one_and_one():
    pytest.importorskip("numpy")
    audio = voice.frames_to_audio([(32767).to_bytes(2, "little", signed=True), (-32768).to_bytes(2, "little", signed=True)])
    assert audio.dtype.name == "float32"
    assert audio[0] == pytest.approx(1.0, abs=1e-4)
    assert audio[1] == pytest.approx(-1.0, abs=1e-4)


def test_an_empty_recording_transcribes_to_nothing_without_loading_a_model(monkeypatch):
    monkeypatch.setattr(voice, "_load_model", lambda name: pytest.fail("no audio, so no model"))
    assert voice.transcribe_frames([], "base.en") == ""


def test_a_transcript_is_stripped_and_joined(monkeypatch):
    class FakeSegment:
        def __init__(self, text):
            self.text = text

    monkeypatch.setattr(
        voice,
        "_load_model",
        lambda name: type("M", (), {"transcribe": lambda self, a, **kw: [FakeSegment(" open "), FakeSegment("the console ")]})(),
    )
    monkeypatch.setattr(voice, "frames_to_audio", lambda frames: "audio")
    assert voice.transcribe_frames([b"\x00\x00"], "base.en") == "open the console"


class RecordingModel:
    """Captures the decode parameters transcription asked for."""

    def __init__(self):
        self.params = {}

    def transcribe(self, audio, **params):
        self.params = params
        return []


def _transcribe_with(monkeypatch):
    model = RecordingModel()
    monkeypatch.setattr(voice, "_load_model", lambda name: model)
    monkeypatch.setattr(voice, "frames_to_audio", lambda frames: "audio")
    voice.transcribe_frames([b"\x00\x00"], "base.en")
    return model


def test_the_vocabulary_bias_reaches_the_model(monkeypatch):
    """Without it, whisper hears 'cloud code' for Claude Code."""
    monkeypatch.delenv("CLICKER_VOICE_PROMPT", raising=False)
    prompt = _transcribe_with(monkeypatch).params["initial_prompt"]
    assert prompt == voice.VOICE_PROMPT
    for word in ("Claude Code", "VS Code", "Chrome", "WhatsApp", "terminal", "GitHub", "monitor", "window", "scroll"):
        assert word in prompt
    for command in ("stop", "pause", "resume", "quit"):
        assert command in prompt


def test_the_bias_is_one_short_sentence():
    """A long prompt costs decode time and gets echoed back as if it had been spoken."""
    assert len(voice.VOICE_PROMPT) < 300


def test_the_environment_overrides_the_bias(monkeypatch):
    monkeypatch.setenv("CLICKER_VOICE_PROMPT", "Panggu Balcon is not a goal.")
    assert _transcribe_with(monkeypatch).params["initial_prompt"] == "Panggu Balcon is not a goal."


def test_an_empty_override_disables_the_bias(monkeypatch):
    monkeypatch.setenv("CLICKER_VOICE_PROMPT", "")
    assert _transcribe_with(monkeypatch).params["initial_prompt"] == ""


def loud_block(frames: int = voice.BLOCK_FRAMES) -> bytes:
    """A block far above the measured room tone: alternating full-ish swings."""
    return b"".join((12000 if i % 2 else -12000).to_bytes(2, "little", signed=True) for i in range(frames))


def quiet_block(frames: int = voice.BLOCK_FRAMES) -> bytes:
    """A block at the room-tone level this machine actually measured, well under SILENCE_LEVEL."""
    return b"".join((600 if i % 2 else -600).to_bytes(2, "little", signed=True) for i in range(frames))


class ScriptedStream:
    """Hands out the scripted blocks in order, then quiet forever, so no loop can run dry."""

    def __init__(self, blocks):
        self.blocks = list(blocks)
        self.reads = 0

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def read(self, n):
        self.reads += 1
        return (self.blocks.pop(0) if self.blocks else quiet_block()), False


def test_recording_stops_when_the_flag_flips():
    """The command bar's microphone button sets a flag; the recorder checks it between reads."""
    stop = iter([False, False, True])
    frames = voice.record_until(
        lambda: next(stop),
        max_seconds=30,
        open_stream=lambda: ScriptedStream([b"\x01\x00", b"\x02\x00", b"\x03\x00"]),
        clock=iter_clock(),
    )
    assert frames == [b"\x01\x00", b"\x02\x00"]


def test_record_until_stops_at_the_cap_when_the_flag_never_flips():
    ticks = iter([0.0, 0.5, 1.5, 2.5])
    frames = voice.record_until(
        lambda: False,
        max_seconds=1.0,
        open_stream=lambda: ScriptedStream([]),
        clock=lambda: next(ticks, 99.0),
    )
    assert len(frames) < 4, "the cap must end a recording nothing ever stops"


def test_silence_detection_separates_quiet_from_loud():
    assert voice.is_silent([quiet_block()])
    assert voice.is_silent([]), "nothing recorded is not speech"
    assert not voice.is_silent([loud_block()])


def test_the_silence_level_sits_above_the_measured_room_tone_and_below_speech():
    """Room tone on this machine's microphone peaked at a per-block RMS of 3442."""
    assert 3442 < voice.SILENCE_LEVEL < 12000


def test_trailing_silence_is_measured_across_block_boundaries():
    """The answer must not depend on how the stream happened to split the frames."""
    one_second = voice.SAMPLE_RATE
    quiet = quiet_block(one_second)
    whole = [loud_block(), quiet]
    assert voice.trailing_silence_seconds(whole) == pytest.approx(1.0, abs=0.1)
    ragged = [loud_block(), quiet[:777], quiet[777:5555], quiet[5555:]]
    assert voice.trailing_silence_seconds(ragged) == pytest.approx(voice.trailing_silence_seconds(whole))


def test_trailing_silence_is_zero_when_the_last_block_is_speech():
    assert voice.trailing_silence_seconds([quiet_block(), loud_block()]) == 0.0


def test_a_trailing_silence_ends_the_recording_once_speech_has_been_heard():
    blocks = [loud_block(), *[quiet_block()] * 40]
    ticks = iter([n * 0.01 for n in range(200)])
    frames = voice.record_until(
        lambda: False,
        max_seconds=60,
        open_stream=lambda: ScriptedStream(blocks),
        clock=lambda: next(ticks, 99.0),
        silence_seconds=0.5,
    )
    assert voice.trailing_silence_seconds(frames) == pytest.approx(0.5, abs=0.1)
    assert len(frames) < 41, "the silence must end the recording before the scripted blocks run out"


def test_silence_before_any_speech_does_not_end_the_recording():
    """A user drawing breath must not have the recording cut out from under them."""
    stop = iter([False] * 30 + [True])
    ticks = iter([n * 0.01 for n in range(200)])
    frames = voice.record_until(
        lambda: next(stop),
        max_seconds=60,
        open_stream=lambda: ScriptedStream([]),
        clock=lambda: next(ticks, 99.0),
        silence_seconds=0.1,
    )
    assert len(frames) == 30, "only the flag may end a recording that has heard nothing yet"


def test_listen_until_passes_the_recorded_frames_through_to_transcription(monkeypatch):
    seen = {}

    def fake_transcribe(frames, name):
        seen["frames"] = (frames, name)
        return "open the console"

    monkeypatch.setattr(voice, "record_until", lambda stop, max_seconds, silence_seconds=0.0: [b"\x07\x00"])
    monkeypatch.setattr(voice, "transcribe_frames", fake_transcribe)
    assert voice.listen_until(lambda: True, 30.0, "base.en", silence_seconds=1.5) == "open the console"
    assert seen["frames"] == ([b"\x07\x00"], "base.en")
