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
        lambda name: type("M", (), {"transcribe": lambda self, a: [FakeSegment(" open "), FakeSegment("the console ")]})(),
    )
    monkeypatch.setattr(voice, "frames_to_audio", lambda frames: "audio")
    assert voice.transcribe_frames([b"\x00\x00"], "base.en") == "open the console"
