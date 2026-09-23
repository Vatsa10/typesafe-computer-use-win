"""Execute one decided action. Every function returns a one-line description for the history."""

from __future__ import annotations

import time
from dataclasses import dataclass

import anthropic
from typesafe_sdk import TypeSafeClient

from . import windows
from .config import SITES
from .decide import APP_PREFIX, OFFSCREEN_PREFIX, WINDOW_PREFIX, Decision, verify_typed
from .models import Field, Item, Screen
from .sitepick import app_for, url_for
from .writer import compose_text, compose_url

VERIFY_THRESHOLD = 0.5
NOOP_MARKERS = ("refused", "failed", "waited")


@dataclass(frozen=True)
class Context:
    goal: str
    browser: str
    email: str | None
    typesafe: TypeSafeClient
    writer: anthropic.Anthropic | None
    history: list[str]
    sites: tuple = ()  # the per-goal shortlist; empty falls back to the pinned catalog
    apps: tuple = ()  # the installed applications worth offering for this goal


def writer_problem(error: anthropic.APIError) -> str:
    """A short reason a run log can carry. The full body is long and mostly JSON, and the useful
    part is almost always the first sentence: no credit, bad key, rate limited."""
    message = str(getattr(error, "message", "") or error)
    if "credit balance is too low" in message:
        return "the Anthropic account is out of credit"
    if "authentication" in message.lower() or "invalid x-api-key" in message.lower():
        return "the Anthropic key was rejected"
    return message.split(".")[0][:120]


def remember(url: str) -> None:
    """Keep a writer-resolved site, so the next run finds it by name instead of paying for a model.

    Best effort on purpose: a catalog that cannot be written must not fail the action that just
    succeeded.
    """
    if not url:
        return
    try:
        from . import catalog

        catalog.remember_site(label="", url=url)
    except Exception:
        pass


def switch_window(key: str, screen: Screen) -> str:
    """Bring an already-open window to the front, wherever it is.

    This is the action that makes a second monitor useful. Before it existed, a goal about a
    window the capture could not see read as "nothing on this screen helps", because from the
    loop's side of things it was true.
    """
    try:
        window = screen.windows[int(key)]
    except (ValueError, IndexError):
        return f"switch_window failed: no window {key!r} is open any more"
    if windows.activate_window(window.hwnd):
        return f"switched to {window.app} {window.title[:60]!r} on monitor {window.monitor + 1}"
    return f"switch_window failed: {window.app} did not come to the front"


def open_app(key: str, ctx: Context) -> str:
    """Launch an installed application by the key the classifier answered with.

    The key indexes the catalog, so the path comes from the Start Menu and never from model text:
    there is no way for this to run something that is not installed.
    """
    from . import apps as app_catalog

    app = app_for(ctx.apps, key)
    if app is None:
        return f"open_app failed: {key!r} is not one of the applications offered"
    if app_catalog.launch(app):
        time.sleep(1.0)  # a launch is not instant, and the next step reads the screen
        return f"launched {app.label}"
    return f"open_app failed: {app.label} did not start"


def is_noop(description: str) -> bool:
    return any(marker in description for marker in NOOP_MARKERS)


def perform(decision: Decision, screen: Screen, items: list[Item], ctx: Context) -> str:
    key = decision.chosen
    by_index = {str(it.index): it for it in items}
    if key in by_index:
        return click_item(by_index[key], screen)
    if key.startswith(OFFSCREEN_PREFIX):
        return press_offscreen(key[len(OFFSCREEN_PREFIX) :], screen)
    if key.startswith(WINDOW_PREFIX):
        return switch_window(key[len(WINDOW_PREFIX) :], screen)
    if key.startswith(APP_PREFIX):
        return open_app(key[len(APP_PREFIX) :], ctx)
    handler = _HANDLERS.get(key)
    if handler is None:
        raise ValueError(f"unknown action {key!r}")
    return handler(decision, screen, items, ctx)


def click_item(item: Item, screen: Screen) -> str:
    """Press an item the app declared through the accessibility tree; click the pixel under it otherwise.

    A press goes to the control itself, so it lands even when the center of the box is covered by
    a sticky header, a cookie banner, or a tooltip. An element that refuses still has a location.
    """
    ref = screen.ax_refs.get(item.index)
    if ref is not None and windows.ax_press(ref):
        return f"pressed {item.text!r} via accessibility"
    windows.click_at(screen.to_points(item))
    if ref is None:
        return f"clicked {item.text!r}"
    return f"clicked {item.text!r} (accessibility press did not take)"


def press_offscreen(key: str, screen: Screen) -> str:
    """Press a control the app exposes but does not show.

    AXPress does not need the element to be visible: a note row scrolled thousands of points down
    and a link the browser parked above the viewport both take it. There is no pixel to fall back
    on, so a refusal is the end of it and reads as a no-op.
    """
    nodes = screen.offscreen
    node = nodes[int(key)] if key.isdigit() and int(key) < len(nodes) else None
    if node is None:
        return f"press_offscreen refused: there is no off-screen control {key!r}"
    if windows.ax_press(node.ref):
        return f"pressed {node.label!r} (off-screen control) via accessibility"
    return f"press_offscreen refused: {node.label!r} did not accept the press"


def fill_field(field: Field, text: str) -> str:
    """Put text in the focused field, by value if the element accepts one and keystrokes otherwise.

    Setting the value is one message instead of one per character, and it cannot be stolen by a
    page that moves the focus mid-word. It is also widely ignored, so the value is read back and
    only a field that really holds the text counts. Returns which path ran, for the history.
    """
    ref = field.ref
    if ref is not None:
        windows.ax_focus(ref)
        if windows.ax_set_value(ref, text):
            back = windows.ax_value(ref)
            if back is not None and back.endswith(text):
                return "via accessibility"
    windows.type_text(text)
    return "via keystrokes"


def _use_browser(decision: Decision, screen, items, ctx: Context) -> str:
    """Go to the browser, and open the website the site answer named.

    `none` is the page already open there, so bringing the browser forward is the whole action. A
    catalog key is its URL, and `other` is a site outside the catalog, which only the writer can
    name. Opening a URL activates the browser too, so the three cases differ only in the page.
    """
    site = decision.site.choice
    if site == "none":
        if windows.activate(ctx.browser):
            return f"activated {ctx.browser}"
        return f"use_browser failed: {ctx.browser} did not come to the front"
    url = url_for(ctx.sites, site) if ctx.sites else SITES.get(site)
    if url is None:
        if ctx.writer is None:
            return "use_browser refused: the site is outside the catalog and no writer is available to propose a URL"
        try:
            url = compose_url(ctx.writer, ctx.goal, ctx.history)
        except anthropic.APIError as e:
            # An expired key, a rate limit or an empty balance must read as a refusal, not a
            # crash: the loop already knows what to do with a step that achieved nothing.
            return f"use_browser refused: the writer could not propose a URL ({writer_problem(e)})"
        remember(url)  # resolved once by a model, a lookup for every run after this one
    if not url:
        return "use_browser refused: the writer proposed no usable URL for this goal"
    if windows.open_url(ctx.browser, url):
        return f"opened {url}"
    return f"use_browser failed: opened {url} but {ctx.browser} did not come to the front"


def _type_email(decision, screen: Screen, items, ctx: Context) -> str:
    if not (screen.field and screen.field.is_text):
        return "type_email refused: no text field is focused"
    how = fill_field(screen.field, ctx.email or "")
    return f"typed email {how}"


def _type_text(decision, screen: Screen, items, ctx: Context) -> str:
    if not (screen.field and screen.field.is_text):
        return "type_text refused: no text field is focused"
    if ctx.writer is None:
        return "type_text refused: no writer available"
    try:
        text = compose_text(ctx.writer, ctx.goal, screen, items, ctx.history)
    except anthropic.APIError as e:
        return f"type_text refused: the writer is unavailable ({writer_problem(e)})"
    if not text:
        return "type_text refused: writer declined to fill this field"
    how = fill_field(screen.field, text)
    time.sleep(0.3)
    p = verify_typed(ctx.typesafe, ctx.goal, screen.field, text, windows.focused_field())
    if p < VERIFY_THRESHOLD:
        windows.clear_field()
        return f"typed {text!r} into {screen.field.label!r} {how} but verification failed ({p:.2f}); cleared it"
    return f"typed {text!r} into {screen.field.label!r} {how} (verified {p:.2f})"


def _key(name: str, description: str):
    def handler(decision, screen, items, ctx) -> str:
        windows.press(name)
        return description

    return handler


def _scroll(lines: int, description: str):
    def handler(decision, screen, items, ctx) -> str:
        windows.scroll(lines)
        return description

    return handler


_HANDLERS = {
    "use_browser": _use_browser,
    "type_email": _type_email,
    "type_text": _type_text,
    "press_enter": _key("return", "pressed Return"),
    "press_escape": _key("escape", "pressed Escape"),
    "scroll_down": _scroll(-10, "scrolled down"),
    "scroll_up": _scroll(10, "scrolled up"),
    "wait": lambda decision, screen, items, ctx: "waited",
}
