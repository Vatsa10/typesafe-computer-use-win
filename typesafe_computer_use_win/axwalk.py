"""The platform-free part of the accessibility walk: pruning rules, role vocabulary, and the
breadth-first hunt itself.

Nothing here imports a platform library. The four callables handed to `walk_actionable` are the
only way into the tree, so a Mac AX tree and a Windows UIA tree walk through the same code; an
adapter reports roles with the AX names below.
"""

from __future__ import annotations

import time
from collections import deque
from collections.abc import Callable, Iterable
from typing import NamedTuple

from .models import AxNode

AX_PRESS = "AXPress"

AX_ACTIONABLE_ROLES = {
    "AXButton",
    "AXCell",
    "AXCheckBox",
    "AXComboBox",
    "AXDisclosureTriangle",
    "AXImage",
    "AXIncrementor",
    "AXLink",
    "AXMenuBarItem",
    "AXMenuButton",
    "AXPopUpButton",
    "AXRadioButton",
    "AXRow",
    "AXSearchField",
    "AXSlider",
    "AXTab",
    "AXTextArea",
    "AXTextField",
}
# A bare child, usually a decorative AXImage, borrows the label of a parent that is itself a control.
AX_LABEL_PARENT_ROLES = {
    "AXButton",
    "AXCell",
    "AXCheckBox",
    "AXLink",
    "AXMenuButton",
    "AXPopUpButton",
    "AXRadioButton",
    "AXRow",
    "AXTab",
}
# List containers keep their label in a shallow AXStaticText rather than on themselves.
AX_LABEL_DESCENDANT_ROLES = {"AXCell", "AXRow"}
AX_SKIP_SUBTREE_ROLES = {"AXMenu"}  # a closed menu: thousands of zero-sized items, none on screen
AX_NODE_CAP = 4000
AX_TIME_CAP = 0.6
AX_OFFSCREEN_CAP = 120  # off-screen controls collected before the walk stops looking for more
AX_MIN_SIDE_PT = 4.0  # anything thinner is a Chromium sliver for a scrolled-out node
AX_FANOUT = 8  # children scanned per level when recovering a label

Frame = tuple[float, float, float, float]  # x, y, w, h in points


class AxAttrs(NamedTuple):
    role: str
    label: str
    frame: Frame | None


def off_display(frame: Frame | None, display_w_pt: float, display_h_pt: float, origin: tuple[float, float] = (0.0, 0.0)) -> bool:
    """True when a real frame lies wholly outside the captured display: a note list thousands of
    screens down, or a web node the browser parked above the viewport. A zero-size frame claims
    nothing, which is what an application element and a closed menu report, so their subtrees are
    still worth a look.

    A frame is in virtual-desktop coordinates, which do not start at zero on a second monitor and
    go negative on one placed left of the primary. `origin` is where the captured display sits, so
    that a window at x=5112 counts as on screen when that is the display being read, and off it
    when it is not.
    """
    if frame is None:
        return False
    x, y, w, h = frame
    if w <= 0 or h <= 0:
        return False
    left, top = origin
    right, bottom = left + display_w_pt, top + display_h_pt
    return x >= right or y >= bottom or x + w <= left or y + h <= top


def node_identity(node) -> object:
    """Accessibility elements hash by the element they wrap, so two fetches of one control compare
    equal; anything unhashable (a fake node in a test) falls back to object identity."""
    try:
        hash(node)
    except TypeError:
        return ("id", id(node))
    return node


def subtree_key(role: str, label: str, frame: Frame | None) -> tuple | None:
    """Identity of a node for de-duplication: same role, label and frame is the same control, whatever
    object the bridge wrapped it in. Frameless and zero-size nodes are containers and are never keyed."""
    if frame is None or frame[2] <= 0 or frame[3] <= 0:
        return None
    return (role, label, round(frame[0]), round(frame[1]), round(frame[2]), round(frame[3]))


def clickable(frame: Frame | None) -> bool:
    return frame is not None and min(frame[2], frame[3]) >= AX_MIN_SIDE_PT


def descendant_label(kids: list, children: Callable, attrs: Callable[..., AxAttrs]) -> str:
    """The first static text within two levels, which is where list rows hide their label."""
    for kid in kids[:AX_FANOUT]:
        role, label, _ = attrs(kid)
        if role == "AXStaticText" and label:
            return label
    for kid in kids[:AX_FANOUT]:
        for grandkid in list(children(kid))[:AX_FANOUT]:
            role, label, _ = attrs(grandkid)
            if role == "AXStaticText" and label:
                return label
    return ""


def walk_actionable(
    root,
    children: Callable[..., Iterable],
    attrs: Callable[..., AxAttrs],
    actions: Callable[..., Iterable[str]],
    display_w_pt: float,
    display_h_pt: float,
    origin: tuple[float, float] = (0.0, 0.0),
    node_cap: int = AX_NODE_CAP,
    time_cap: float = AX_TIME_CAP,
    offscreen_cap: int = AX_OFFSCREEN_CAP,
    clock: Callable[[], float] = time.monotonic,
) -> tuple[list[AxNode], list[AxNode], bool]:
    """Breadth-first hunt for labelled controls: the on-screen ones, the reachable off-screen ones,
    and whether a cap cut the walk short.

    The four callables are the only way into the tree, so the pruning rules are platform-free
    and testable against a plain dict. The caps are the point: an unbounded walk of a note list
    or a long web page costs seconds and finds nothing on screen.

    A node that misses the display, or that the app clamped to a sliver, is not on screen and is
    not offered as one: its subtree stays pruned from `found`. But AXPress does not need a node to
    be visible, so a labelled one that accepts the action is collected separately, down to
    `offscreen_cap`, after which those subtrees are dropped again and the walk is the old one.
    """
    found: list[AxNode] = []
    offscreen: list[AxNode] = []
    deadline = clock() + time_cap
    queue = deque([(root, "", False, False)])
    seen = 0
    visited: set = set()  # elements compare by identity across fetches, so a self-listing app is walked once
    # A control handed over as several distinct objects is offered once. The repeat is still walked:
    # UIA wraps a window in panes of its own size and label, and pruning there loses the whole app.
    # ponytail: the re-walk is bounded by node_cap and time_cap, which is cheap enough to not track subtrees.
    visited_keys: set[tuple] = set()
    while queue:
        if seen >= node_cap or clock() >= deadline:
            return found, offscreen, True
        node, parent_label, parent_emitted, hidden = queue.popleft()
        identity = node_identity(node)
        if identity in visited:
            continue
        visited.add(identity)
        seen += 1
        role, own_label, frame = attrs(node)
        if role in AX_SKIP_SUBTREE_ROLES:
            continue
        key = subtree_key(role, own_label, frame)
        repeat = key is not None and key in visited_keys  # same control handed over again, or a wrapper
        if key is not None:
            visited_keys.add(key)
        hidden = hidden or off_display(frame, display_w_pt, display_h_pt, origin)
        if hidden and len(offscreen) >= offscreen_cap:
            continue  # nothing left to collect down there, and it never counted on screen
        kids = list(children(node))
        label, inherited = own_label, False
        if not label and role in AX_LABEL_DESCENDANT_ROLES:
            label = descendant_label(kids, children, attrs)
        if not label and parent_label:
            label, inherited = parent_label, True
        emitted = False
        duplicate = repeat or (inherited and parent_emitted)  # something already stands for this control
        nameless_group = role == "AXGroup" and not own_label  # a Chromium layout box, not a control
        visible = not hidden and clickable(frame)
        if label and not duplicate and not nameless_group:
            if visible:
                pressable = AX_PRESS in actions(node)
                if pressable or role in AX_ACTIONABLE_ROLES:
                    x, y, w, h = frame
                    found.append(AxNode(role=role, label=label, x=x, y=y, w=w, h=h, pressable=pressable, ref=node))
                    emitted = True
            elif frame is not None and len(offscreen) < offscreen_cap and AX_PRESS in actions(node):
                x, y, w, h = frame
                offscreen.append(AxNode(role=role, label=label, x=x, y=y, w=w, h=h, pressable=True, ref=node))
        child_label = own_label if role in AX_LABEL_PARENT_ROLES else ""
        queue.extend((kid, child_label, emitted, hidden) for kid in kids)
    return found, offscreen, False
