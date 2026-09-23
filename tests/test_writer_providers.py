"""Free text from either provider. No network: both clients are fakes."""

from __future__ import annotations

import json
from types import SimpleNamespace

import pytest
from PIL import Image

from typesafe_computer_use_win import config, writer
from typesafe_computer_use_win.writer import Writer, WriterUnavailable, compose_url


class FakeOpenAI:
    """Records the request and replies the way an OpenAI-compatible endpoint does."""

    def __init__(self, reply: dict, reject: str = ""):
        self.requests: list[dict] = []
        self.reject = reject
        self.chat = SimpleNamespace(completions=SimpleNamespace(create=self._create))
        self._reply = reply

    def _create(self, **request):
        if self.reject and self.reject in request:
            raise TypeError(f"unexpected keyword argument {self.reject!r}")
        self.requests.append(request)
        return SimpleNamespace(choices=[SimpleNamespace(message=SimpleNamespace(content=json.dumps(self._reply)))])


def openai_writer(reply: dict, reject: str = "") -> Writer:
    return Writer("openai", FakeOpenAI(reply, reject))


def test_an_openai_request_carries_a_strict_schema(monkeypatch):
    """Strict mode is what makes the reply parseable without a retry loop."""
    monkeypatch.setenv("CLICKER_WRITER_MODEL", "gpt-test")
    w = openai_writer({"ok": True, "url": "https://example.com", "reason": "x"})
    assert compose_url(w, "open example", []) == "https://example.com"
    request = w.client.requests[0]
    assert request["model"] == "gpt-test"
    schema = request["response_format"]["json_schema"]
    assert schema["strict"] is True
    assert schema["schema"]["required"] == ["ok", "url", "reason"]
    assert schema["schema"]["additionalProperties"] is False


def test_the_system_prompt_is_its_own_message_on_openai():
    """Anthropic takes a system parameter; this API takes a system role, and dropping it would
    silently lose every instruction."""
    w = openai_writer({"ok": False, "url": "", "reason": "x"})
    compose_url(w, "open example", [])
    roles = [m["role"] for m in w.client.requests[0]["messages"]]
    assert roles == ["system", "user"]


def test_a_capture_rides_as_a_data_url(screen):
    from typesafe_computer_use_win.writer import compose_answer

    w = openai_writer({"achieved": True, "answer": "done"})
    compose_answer(w, "goal", screen, [], [], "the goal is achieved")
    content = w.client.requests[0]["messages"][1]["content"]
    image = next(part for part in content if part["type"] == "image_url")
    assert image["image_url"]["url"].startswith("data:image/png;base64,")


def test_a_gateway_that_rejects_the_new_token_argument_is_retried_with_the_old_one():
    """Groq, OpenRouter and older servers speak max_tokens. Failing the run over a parameter name
    would be a poor reason to lose a step."""
    w = openai_writer({"ok": True, "url": "https://example.com", "reason": "x"}, reject="max_completion_tokens")
    assert compose_url(w, "open example", []) == "https://example.com"
    assert "max_tokens" in w.client.requests[0]


def test_a_provider_failure_becomes_one_error_type_whichever_sdk_raised_it():
    class Angry:
        def __init__(self):
            self.chat = SimpleNamespace(completions=SimpleNamespace(create=self._create))

        def _create(self, **request):
            raise RuntimeError("Your credit balance is too low to access the API")

    with pytest.raises(WriterUnavailable, match="out of credit"):
        compose_url(Writer("openai", Angry()), "open example", [])


def test_the_provider_decides_which_model_name_is_the_default(monkeypatch):
    monkeypatch.delenv("CLICKER_WRITER_MODEL", raising=False)
    monkeypatch.setenv("CLICKER_WRITER_PROVIDER", "openai")
    assert config.writer_model() == config.DEFAULT_OPENAI_WRITER_MODEL
    monkeypatch.setenv("CLICKER_WRITER_PROVIDER", "anthropic")
    assert config.writer_model() == config.DEFAULT_WRITER_MODEL


def test_no_key_means_no_writer_rather_than_a_run_that_fails_halfway(monkeypatch):
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    monkeypatch.delenv("CLICKER_WRITER_PROVIDER", raising=False)
    assert writer.make_writer() is None


def test_an_image_is_shrunk_before_it_is_sent_on_either_provider():
    """A 4K capture costs real money and time on every provider, and the model reads no more of it
    than the long edge allows. Both paths share this, so it is asserted on the bytes themselves."""
    import base64
    import io

    big = Image.new("RGB", (3456, 2234))
    sent = Image.open(io.BytesIO(base64.b64decode(writer._png_base64(big))))
    assert max(sent.size) == writer.ANSWER_IMAGE_EDGE
    assert sent.size[0] / sent.size[1] == pytest.approx(3456 / 2234, rel=0.01)  # not distorted
