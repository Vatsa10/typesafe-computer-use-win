"""The writer model: the only place free text is generated, when the classifier asks for it and once when the run ends."""

from __future__ import annotations

import base64
import io
import json
from dataclasses import dataclass
from urllib.parse import urlparse

import anthropic
from PIL import Image

from . import config
from .config import answer_model, writer_model
from .dates import now_context
from .models import Item, Screen
from .perception import near_field


class WriterUnavailable(RuntimeError):
    """The writer could not answer: no credit, a rejected key, a rate limit, a missing model.

    One type for every provider, so callers refuse a step rather than learning two SDKs' exception
    hierarchies.
    """


@dataclass(frozen=True)
class Writer:
    """Whichever service writes free text, behind one method.

    The providers differ only in how a JSON schema and an image ride along on a request, so that is
    all this holds. Everything above it asks for a dict and gets one.
    """

    provider: str
    client: object

    def structured(self, system: str, packet: dict, properties: dict, max_tokens: int, model=None, image=None) -> dict:
        if self.provider == "openai":
            return _openai_structured(self.client, system, packet, properties, max_tokens, model, image)
        return _anthropic_structured(self.client, system, packet, properties, max_tokens, model, image)


def make_writer() -> Writer | None:
    """A writer, or None when no credentials resolve. The SDKs only check on the first request, so
    the key is inspected here instead of letting a run fail halfway through driving a machine."""
    if config.writer_provider() == "openai":
        return _make_openai()
    client = anthropic.Anthropic()
    if client.api_key or getattr(client, "auth_token", None):
        return Writer("anthropic", client)
    return None


def _make_openai() -> Writer | None:
    try:
        import openai
    except ImportError:
        return None
    base_url = config.openai_base_url()
    client = openai.OpenAI(base_url=base_url) if base_url else openai.OpenAI()
    return Writer("openai", client) if client.api_key else None


def problem(error: Exception) -> str:
    """A short reason a run log can carry. The bodies are long and mostly JSON, and the useful part
    is nearly always one of the same few causes."""
    message = str(getattr(error, "message", "") or error)
    lowered = message.lower()
    if "credit balance is too low" in lowered or "insufficient_quota" in lowered or "exceeded your current quota" in lowered:
        return "the account is out of credit"
    if "authentication" in lowered or "invalid x-api-key" in lowered or "incorrect api key" in lowered:
        return "the key was rejected"
    if "does not exist" in lowered or "model_not_found" in lowered:
        return "that model is not available to this account; set CLICKER_WRITER_MODEL"
    if "rate limit" in lowered or "429" in lowered:
        return "the account is rate limited"
    return message.split(".")[0][:120]


ANSWER_IMAGE_EDGE = 1568  # the longest edge a vision model reads without shrinking the image itself


def _structured(writer, system: str, packet: dict, properties: dict, max_tokens: int, model=None, image=None) -> dict:
    """One structured reply, from whichever provider is configured.

    Every provider failure becomes WriterUnavailable, because every caller does the same thing with
    it: refuse the step and let the loop carry on.
    """
    try:
        return writer.structured(system, packet, properties, max_tokens, model, image)
    except WriterUnavailable:
        raise
    except Exception as e:  # each SDK raises its own hierarchy, and a gateway raises a third
        raise WriterUnavailable(problem(e)) from e


def _schema(properties: dict) -> dict:
    return {"type": "object", "properties": properties, "required": list(properties), "additionalProperties": False}


def _anthropic_structured(client, system: str, packet: dict, properties: dict, max_tokens: int, model, image) -> dict:
    content: list[dict] = [{"type": "text", "text": json.dumps(packet)}]
    if image is not None:
        content.insert(0, _image_block(image))
    response = client.messages.create(
        model=model or writer_model(),
        max_tokens=max_tokens,
        system=system,
        messages=[{"role": "user", "content": content}],
        output_config={"format": {"type": "json_schema", "schema": _schema(properties)}},
    )
    return json.loads("".join(b.text for b in response.content if b.type == "text"))


def _openai_structured(client, system: str, packet: dict, properties: dict, max_tokens: int, model, image) -> dict:
    """The same request against an OpenAI-compatible endpoint, which also covers Groq, OpenRouter
    and a local server: the shape is identical and only the base URL changes."""
    content: list[dict] = [{"type": "text", "text": json.dumps(packet)}]
    if image is not None:
        content.insert(0, _openai_image_block(image))
    request = {
        "model": model or writer_model(),
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": content}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "reply", "strict": True, "schema": _schema(properties)},
        },
    }
    try:
        response = client.chat.completions.create(max_completion_tokens=max_tokens, **request)
    except TypeError:  # an older client, or a gateway that only knows the legacy name
        response = client.chat.completions.create(max_tokens=max_tokens, **request)
    except Exception as e:
        if "max_completion_tokens" not in str(e):
            raise
        response = client.chat.completions.create(max_tokens=max_tokens, **request)
    return json.loads(response.choices[0].message.content)


def _openai_image_block(image: Image.Image) -> dict:
    """The capture as a data URL, which is how this API takes an image."""
    return {"type": "image_url", "image_url": {"url": "data:image/png;base64," + _png_base64(image)}}


def _image_block(image: Image.Image) -> dict:
    """The capture as a PNG the model can read. PNG because screen text does not survive JPEG well."""
    return {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": _png_base64(image)}}


def _png_base64(image: Image.Image) -> str:
    shrunk = image.convert("RGB")
    shrunk.thumbnail((ANSWER_IMAGE_EDGE, ANSWER_IMAGE_EDGE))
    buffer = io.BytesIO()
    shrunk.save(buffer, format="PNG")
    return base64.b64encode(buffer.getvalue()).decode()


def compose_text(writer: Writer, goal: str, screen: Screen, items: list[Item], history: list[str]) -> str:
    """The exact string to type into the focused field. Empty means the writer declined."""
    packet = {
        "goal": goal,
        "now": now_context(),
        "frontmost_app": screen.app,
        "previous_actions": history[-8:],
        "focused_field": screen.field.summary() if screen.field else None,
        "text_near_field": near_field(screen, items),
        "all_screen_text": [it.text for it in items][:120],
    }
    data = _structured(
        writer,
        system=(
            "You fill in one text field on a user's screen. You receive the user's goal, recent "
            "actions, the focused field's label and placeholder, and nearby screen text. Decide the "
            "exact string to type. Never invent credentials, passwords, or personal data; for such "
            "fields, or when the field should not be filled, set fill to false."
        ),
        packet=packet,
        properties={"fill": {"type": "boolean"}, "text": {"type": "string"}, "reason": {"type": "string"}},
        max_tokens=256,
    )
    return data["text"].strip() if data["fill"] else ""


def valid_url(url: str) -> bool:
    parsed = urlparse(url)
    return parsed.scheme == "https" and "." in parsed.netloc and not any(ch.isspace() for ch in url)


def compose_url(writer: Writer, goal: str, history: list[str]) -> str:
    """The URL to open for this goal. Empty means no sensible site, or an invalid proposal."""
    data = _structured(
        writer,
        system=(
            "Given a user's goal for their web browser, give the single best https URL to open first. "
            "Prefer the site's homepage or the most direct public page. If no website is implied, set ok to false."
        ),
        packet={"goal": goal, "now": now_context(), "previous_actions": history[-8:]},
        properties={"ok": {"type": "boolean"}, "url": {"type": "string"}, "reason": {"type": "string"}},
        max_tokens=200,
    )
    url = data["url"].strip() if data["ok"] else ""
    return url if valid_url(url) else ""


@dataclass(frozen=True)
class Answer:
    text: str
    achieved: bool  # whether the screen itself shows the goal reached, in the writer's judgement


def compose_answer(writer: Writer, goal: str, screen: Screen, items: list[Item], history: list[str], stopped: str) -> Answer:
    """What to tell the user now that the run is over: the result when the screen holds it, where things stand when not.

    The classifier can stop on the right page but cannot say what the page says. The writer reads the
    capture itself as well as its text, since OCR misreads a letter here and there and drops layout.
    """
    packet = {
        "goal": goal,
        "now": now_context(),
        "why_the_run_stopped": stopped,
        "actions_taken": history,
        "frontmost_app": screen.app,
        "browser_active_tab_url": screen.url,
        "screen_text_in_reading_order": [it.text for it in items],
    }
    data = _structured(
        writer,
        system=(
            "An agent drove a user's computer toward the user's goal and has now stopped. You receive "
            "the goal, the actions it took, why it stopped, a capture of the screen as it is now, and "
            "the text read from that screen. Tell the user the result. When the goal asks for "
            "information, lead with that information, taken only from the screen: never from memory, "
            "and never a guess. When the goal asks for something to be done, say whether the screen "
            "shows it done. When the screen does not hold the result, say so plainly, then say what is "
            "on screen and the one next step that would get there. Trust the capture over the text "
            "where the two disagree. Plain text, no markdown, four sentences at most. Set achieved to "
            "true only when the screen itself shows the goal reached."
        ),
        packet=packet,
        properties={"achieved": {"type": "boolean"}, "answer": {"type": "string"}},
        max_tokens=1024,
        model=answer_model(),
        image=screen.image,
    )
    return Answer(text=data["answer"].strip(), achieved=data["achieved"])
