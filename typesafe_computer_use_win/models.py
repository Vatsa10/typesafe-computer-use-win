"""Data carried between perception, decision, and action."""

from __future__ import annotations

from dataclasses import dataclass, field, fields

from PIL import Image

TEXT_ROLES = {"AXTextField", "AXTextArea", "AXSearchField", "AXComboBox"}
Box = tuple[float, float, float, float]  # x1, y1, x2, y2 in capture pixels

# Accessibility roles as one human word. Anything unlisted is "other".
ROLE_WORDS = {
    "AXButton": "button",
    "AXCell": "cell",
    "AXCheckBox": "checkbox",
    "AXComboBox": "field",
    "AXDockItem": "taskbar item",
    "AXImage": "image",
    "AXLink": "link",
    "AXMenuBarItem": "menu",
    "AXMenuButton": "button",
    "AXPopUpButton": "popup",
    "AXRadioButton": "radio",
    "AXRow": "cell",
    "AXSearchField": "field",
    "AXSlider": "slider",
    "AXTab": "tab",
    "AXTextArea": "field",
    "AXTextField": "field",
}


class Abort(Exception):
    """Raised when the user triggers an escape hatch."""


@dataclass(frozen=True)
class Item:
    """One clickable thing: text plus its pixel box on the capture.

    `source` says where it came from: "ocr" for a merged text block, "ax" for an
    accessibility control, "ax+ocr" when the two agree on the same thing. `role` is a
    short human word (button, link, field, ...) and is empty for OCR-only items.
    """

    index: int
    text: str
    ocr_confidence: float
    x1: float
    y1: float
    x2: float
    y2: float
    role: str = ""
    source: str = "ocr"

    @property
    def center(self) -> tuple[float, float]:
        return (self.x1 + self.x2) / 2, (self.y1 + self.y2) / 2

    @property
    def from_ax(self) -> bool:
        return self.source in ("ax", "ax+ocr")


@dataclass(frozen=True)
class AxNode:
    """One actionable accessibility element, in screen points.

    `ref` is the element itself, the handle an action is sent to. It is opaque here and
    stays out of equality and repr so a node compares as the facts it reports.
    """

    role: str
    label: str
    x: float
    y: float
    w: float
    h: float
    pressable: bool
    ref: object | None = field(default=None, compare=False, repr=False)

    @property
    def role_word(self) -> str:
        return ROLE_WORDS.get(self.role, "other")


@dataclass(frozen=True)
class Field:
    """The focused accessibility element, in screen points.

    `ref` is the element itself, so text can be set on it directly instead of typed.
    """

    role: str
    label: str
    placeholder: str
    value: str
    x: float
    y: float
    w: float
    h: float
    ref: object | None = field(default=None, compare=False, repr=False)

    @property
    def is_text(self) -> bool:
        return self.role in TEXT_ROLES

    def record(self) -> dict:
        """Everything but the opaque element handle, which no log can serialize."""
        return {f.name: getattr(self, f.name) for f in fields(self) if f.name != "ref"}

    def summary(self) -> dict:
        return {
            "role": self.role,
            "label": self.label,
            "placeholder": self.placeholder,
            "current_value": self.value[:200],
        }


@dataclass(frozen=True)
class Screen:
    """Everything captured about the display at one instant."""

    image: Image.Image
    scale: float  # capture pixels per screen point
    app: str
    field: Field | None
    url: str | None
    pid: int | None = None  # frontmost process, for the accessibility walk; None in replay
    window: tuple[float, float, float, float] | None = None  # frontmost window, x/y/w/h in points; None in replay
    # Where the captured display sits on the virtual desktop. A second monitor starts somewhere
    # other than zero, and can start left of or above the primary, which makes this negative. Every
    # click is posted in virtual-desktop coordinates, so the offset has to come back before acting.
    origin: tuple[float, float] = (0.0, 0.0)
    ax_refs: dict[int, object] = field(default_factory=dict)  # item index -> accessibility element, when it has one
    offscreen: list[AxNode] = field(default_factory=list)  # labelled controls the app exposes but does not show
    # What else is running. A capture is one display, but the machine is all of them, and a goal
    # about something on another monitor is answerable only if the loop is told it is open.
    windows: tuple = ()
    monitors: tuple = ()
    monitor: int = 0  # which display this capture came from

    @property
    def size_pt(self) -> tuple[float, float]:
        return self.image.width / self.scale, self.image.height / self.scale

    def region(self, item: Item) -> str:
        cx, cy = item.center
        col = ["left", "center", "right"][min(2, int(3 * cx / self.image.width))]
        row = ["top", "middle", "bottom"][min(2, int(3 * cy / self.image.height))]
        return f"{row}-{col}"

    def to_points(self, item: Item) -> tuple[float, float]:
        """Where to click, in the virtual-desktop coordinates synthetic input speaks.

        The capture is one display, so an item's pixels are relative to that display's top-left.
        Adding the origin is what keeps a click on the third monitor from landing on the first.
        """
        cx, cy = item.center
        return self.origin[0] + cx / self.scale, self.origin[1] + cy / self.scale

    @property
    def bounds_pt(self) -> tuple[float, float, float, float]:
        """The captured display as left, top, right, bottom in points, on the virtual desktop."""
        width, height = self.size_pt
        return self.origin[0], self.origin[1], self.origin[0] + width, self.origin[1] + height
