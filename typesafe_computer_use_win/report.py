"""Logging, annotated screenshots, and the human-readable payload dump."""

from __future__ import annotations

import contextlib
import json
from collections.abc import Callable
from pathlib import Path

from PIL import ImageDraw, ImageFont

from .decide import base_state, item_criteria, kind_criteria, offscreen_criteria, site_criteria
from .models import Item, Screen

# Windows ships Segoe UI everywhere; the Consolas fallback covers a trimmed install. The label on
# an annotated capture is unreadable at the default bitmap font, which is what PIL falls back to.
FONT_PATHS = ("C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/consola.ttf", "C:/Windows/Fonts/arial.ttf")
RULE = "=" * 78


class Log:
    """Send each line to a sink and append it to a file.

    The sink defaults to `print`, so a plain `Log(path)` behaves exactly as it always has. A UI can
    pass its own callable to mirror the run's lines into a live feed. A sink that raises must not
    take the run down: the exception is swallowed and the line still reaches the file.
    """

    def __init__(self, path: Path | None = None, echo: Callable[[str], object] | None = print):
        self.path = path
        self.echo = echo

    def __call__(self, msg: str = "") -> None:
        if self.echo is not None:
            with contextlib.suppress(Exception):  # a broken sink must not kill the loop
                self.echo(msg)
        if self.path is not None:
            with self.path.open("a", encoding="utf-8") as f:
                f.write(msg + "\n")


def _label_font(size: int):
    """The first font on this machine that can draw a readable label, else PIL's bitmap default."""
    for path in FONT_PATHS:
        try:
            return ImageFont.truetype(path, size)
        except OSError:
            continue
    return ImageFont.load_default()


def top(answer, n: int = 5) -> list[tuple[str, float]]:
    return sorted(answer.probabilities.items(), key=lambda kv: -kv[1])[:n]


def ax_count(items: list[Item]) -> int:
    return sum(1 for it in items if it.from_ax)


def render_payload(goal: str, screen: Screen, items: list[Item], history: list[str], browser: str, email: str | None) -> str:
    """Exactly what goes to TypeSafe for this screen, plus a table of every item."""
    parts = [
        RULE,
        "STATE  (sent as `state`)",
        RULE,
        json.dumps(base_state(goal, screen, items, history), indent=2),
        "",
        RULE,
        "QUESTION kind  (Choice criteria)",
        RULE,
        json.dumps(kind_criteria(browser, email, bool(screen.offscreen)), indent=2),
        "",
        RULE,
        "QUESTION item  (Choice criteria)",
        RULE,
        json.dumps(item_criteria(screen, items), indent=2),
        "",
        RULE,
        "QUESTION site  (Choice criteria)",
        RULE,
        json.dumps(site_criteria(), indent=2),
        "",
    ]
    if screen.offscreen:
        parts += [
            RULE,
            "QUESTION offscreen  (Choice criteria)",
            RULE,
            json.dumps(offscreen_criteria(screen.offscreen), indent=2),
            "",
        ]
    parts += [
        RULE,
        f"ITEMS  ({len(items)} after merge/filter, {ax_count(items)} from the accessibility tree; "
        f"pixel boxes on the {screen.image.width}x{screen.image.height} capture, scale {screen.scale:g})",
        RULE,
    ]
    for it in items:
        cx, cy = screen.to_points(it)
        parts.append(
            f"[{it.index:3d}] src={it.source:6} role={it.role or '-':8} conf={it.ocr_confidence:.2f} "
            f"box=({it.x1:.0f},{it.y1:.0f})-({it.x2:.0f},{it.y2:.0f}) "
            f"click_pt=({cx:.0f},{cy:.0f}) {screen.region(it):13} {it.text!r}"
        )
    if screen.offscreen:
        parts += [
            "",
            RULE,
            f"OFFSCREEN CONTROLS  ({len(screen.offscreen)} the app exposes without showing; pressed through "
            "accessibility, never clicked)",
            RULE,
        ]
        parts += [f"[{i:3d}] role={node.role:22} {node.label!r}" for i, node in enumerate(screen.offscreen)]
    if screen.field:
        parts += ["", "FOCUSED FIELD", json.dumps(screen.field.record(), indent=2)]
    return "\n".join(parts) + "\n"


def annotate(screen: Screen, items: list[Item], chosen: str, out: Path) -> None:
    """Blue boxes for OCR blocks, orange for accessibility controls, red for the chosen one, green for the focused field."""
    image = screen.image.copy()
    draw = ImageDraw.Draw(image)
    font = _label_font(int(11 * screen.scale))
    for it in items:
        hit = str(it.index) == chosen
        color = (255, 0, 0) if hit else (255, 140, 0) if it.from_ax else (0, 160, 255)
        draw.rectangle((it.x1, it.y1, it.x2, it.y2), outline=color, width=3 if hit else 1)
        draw.text((it.x1, max(0, it.y1 - 12 * screen.scale)), str(it.index), fill=color, font=font)
    f = screen.field
    if f is not None:
        s = screen.scale
        draw.rectangle((f.x * s, f.y * s, (f.x + f.w) * s, (f.y + f.h) * s), outline=(0, 200, 0), width=3)
    image.save(out)
