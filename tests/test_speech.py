"""Speech out. No machine talks here: the SAPI object is a fake, injected or monkeypatched."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import speech


class FakeVoice:
    """Records what SAPI was asked to say, and with which flags."""

    def __init__(self) -> None:
        self.said: list[tuple[str, int]] = []

    def Speak(self, text, flags):
        self.said.append((text, flags))
        return len(self.said)


@pytest.fixture(autouse=True)
def forget_engine(monkeypatch):
    """Each test starts with no cached voice and the default engine."""
    monkeypatch.delenv("CLICKER_TTS", raising=False)
    speech.reset()
    yield
    speech.reset()


def test_the_module_imports_without_touching_com():
    """Nothing heavy at module scope: comtypes and winsound are imported inside the functions."""
    assert not {"comtypes", "winsound", "urllib"} & set(vars(speech))


def test_speak_returns_false_and_does_not_raise_without_an_engine(monkeypatch):
    def no_speech():
        raise OSError("no speech stack on this machine")

    monkeypatch.setattr(speech, "_create_sapi", no_speech)
    assert speech.speak("anything at all") is False
    assert speech.available() is False
    assert speech.voices() == []


def test_speak_returns_false_when_the_device_fails_mid_sentence(monkeypatch):
    class Broken:
        def Speak(self, text, flags):
            raise OSError("the audio device went away")

    monkeypatch.setattr(speech, "_create_sapi", Broken)
    assert speech.speak("hello") is False


def test_empty_text_is_not_spoken(monkeypatch):
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    assert speech.speak("   ") is False
    assert voice.said == []


def test_the_com_object_is_created_lazily_and_reused(monkeypatch):
    built = []

    def create():
        built.append(FakeVoice())
        return built[-1]

    monkeypatch.setattr(speech, "_create_sapi", create)
    assert built == []  # nothing is built until something is said
    assert speech.speak("first") is True
    assert speech.speak("second") is True
    assert speech.available() is True
    assert len(built) == 1
    assert [text for text, _flags in built[0].said] == ["first", "second"]


def test_speaking_is_asynchronous_and_purges_the_previous_answer(monkeypatch):
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    speech.speak("a long first answer")
    speech.speak("the answer that replaces it")
    flags = [flag for _text, flag in voice.said]
    assert all(flag & speech.SVSF_ASYNC for flag in flags)
    assert all(flag & speech.SVSF_PURGE_BEFORE_SPEAK for flag in flags)


def test_blocking_speech_waits_but_still_purges(monkeypatch):
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    speech.speak("say this and wait", blocking=True)
    (_text, flags) = voice.said[-1]
    assert not flags & speech.SVSF_ASYNC
    assert flags & speech.SVSF_PURGE_BEFORE_SPEAK


def test_stop_purges_the_current_utterance(monkeypatch):
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    speech.speak("half of a long sentence")
    speech.stop()
    assert voice.said[-1] == ("", speech.SVSF_ASYNC | speech.SVSF_PURGE_BEFORE_SPEAK)


def test_stop_is_safe_when_nothing_is_speaking(monkeypatch):
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    speech.stop()  # no speak call before it
    assert voice.said == [("", speech.SVSF_ASYNC | speech.SVSF_PURGE_BEFORE_SPEAK)]


def test_stop_is_safe_with_no_engine_at_all(monkeypatch):
    monkeypatch.setattr(speech, "_create_sapi", lambda: (_ for _ in ()).throw(OSError("none")))
    speech.stop()


def test_stop_survives_a_refusing_com_object(monkeypatch):
    class Broken:
        def Speak(self, text, flags):
            raise OSError("gone")

    monkeypatch.setattr(speech, "_create_sapi", Broken)
    speech.stop()


def test_voices_lists_what_the_machine_can_speak_with(monkeypatch):
    class Token:
        def __init__(self, name):
            self.name = name

        def GetDescription(self):
            return self.name

    class Tokens:
        Count = 2

        def Item(self, i):
            return Token(["Microsoft David Desktop", "Microsoft Zira Desktop"][i])

    class Voice(FakeVoice):
        def GetVoices(self):
            return Tokens()

    monkeypatch.setattr(speech, "_create_sapi", Voice)
    assert speech.voices() == ["Microsoft David Desktop", "Microsoft Zira Desktop"]


def test_a_short_plain_answer_is_left_alone():
    answer = "Google Chrome is already open, as shown on the screen with an active GitHub page loaded."
    assert speech.speakable(answer) == answer


def test_urls_come_down_to_their_host():
    spoken = speech.speakable("I see https://github.com/Vatsa10/typesafe-computer-use/pull/12?tab=files open.")
    assert spoken == "I see github.com open."


def test_a_bare_www_url_also_comes_down_to_its_host():
    assert speech.speakable("Open www.youtube.com/watch?v=abc now.") == "Open youtube.com now."


def test_paths_come_down_to_their_last_part():
    spoken = speech.speakable(r"The run is in D:\Files\Vatsa\Projects\typesafe-computer-use\runs\20261006-202115\run.json")
    assert spoken == "The run is in run.json"


def test_a_posix_path_comes_down_to_its_last_part():
    assert speech.speakable("Wrote /tmp/claude/scratch/answer.txt there.") == "Wrote answer.txt there."


def test_an_over_long_answer_is_cut_at_a_sentence_boundary():
    answer = (
        "The goal to like the post is already achieved. "
        "The screen confirms that the 'Daytona' song by Karan Aujla and Ikky has been saved to 'Liked music', "
        "which indicates it has been liked. No further action is needed. Your goal is complete."
    )
    spoken = speech.speakable(answer, limit=120)
    assert spoken == "The goal to like the post is already achieved."
    assert len(spoken) <= 120


def test_the_default_cap_cuts_a_real_answer_on_a_full_stop():
    answer = (
        "You have two windows open side by side: WhatsApp Web on the left and YouTube Music on the right. "
        "On WhatsApp, you are chatting with contacts and discussing topics about editing videos and APIs. "
        "On YouTube Music, you're listening to a playlist with the song 'Daytona' currently playing. "
        "If you wanted a summary of either platform or further details, let me know what you'd like next."
    )
    spoken = speech.speakable(answer)
    assert len(spoken) <= speech.SPEAK_LIMIT
    assert spoken.endswith(".")
    assert spoken.startswith("You have two windows open side by side")


def test_an_answer_with_no_sentence_boundary_is_cut_on_a_word():
    spoken = speech.speakable("word " * 100, limit=40)
    assert len(spoken) <= 40
    assert not spoken.endswith("wor")


def test_whitespace_is_collapsed():
    assert speech.speakable("two\n\nlines   and\ttabs") == "two lines and tabs"


def test_murf_is_only_reached_when_it_is_asked_for(monkeypatch):
    """CLICKER_TTS unset means SAPI, whatever is in the environment."""
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    monkeypatch.setattr(speech, "_murf_speak", lambda text, blocking: pytest.fail("murf must not be used"))
    monkeypatch.setenv("MURF_API_KEY", "not-a-real-key")
    assert speech.speak("hello") is True


def test_murf_falls_back_to_sapi_when_it_fails(monkeypatch):
    voice = FakeVoice()
    monkeypatch.setattr(speech, "_create_sapi", lambda: voice)
    monkeypatch.setenv("CLICKER_TTS", "murf")
    monkeypatch.setattr(speech, "_murf_audio", lambda text: None)
    assert speech.speak("hello") is True
    assert voice.said == [("hello", speech.SVSF_ASYNC | speech.SVSF_PURGE_BEFORE_SPEAK)]


def test_murf_without_a_key_does_not_call_the_api(monkeypatch):
    monkeypatch.delenv("MURF_API_KEY", raising=False)
    monkeypatch.setattr(speech, "_create_sapi", lambda: (_ for _ in ()).throw(OSError("no speech")), raising=True)
    monkeypatch.setenv("CLICKER_TTS", "murf")
    assert speech.speak("hello") is False  # no key, no network call, no crash
