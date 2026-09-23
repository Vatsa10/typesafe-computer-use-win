"""Catalogue the installed applications, so the loop can launch one by name instead of only opening URLs.

This is `catalog.py`'s sibling: same shape, same instincts. It walks the two Start Menu `Programs`
trees Windows keeps -- one per user, one per machine -- turns every `.lnk` into an `App` row, and
caches the result as JSON under `%LOCALAPPDATA%/winclicker/apps.json`. Nothing here calls a model,
draws a UI, or touches the decision loop.

Two rules keep this safe to hand to a model:

* **The model picks a key; code owns the path.** `launch` takes an `App` that came out of this
  catalog and nothing else. A command line is never built out of model text, so there is no path
  from "the model said something odd" to "an arbitrary program ran". Anything not in the catalog
  cannot be launched at all.
* **The catalog is filtered before the model ever sees it.** The Start Menu is mostly not launch
  targets: uninstallers, readmes, licence files, admin consoles and -- worst for a computer-use
  agent -- the accessibility tools. See `DROP_NAMES` / `DROP_FOLDERS` for the reasoning.

Tolerance is the whole job, as in `catalog`. No Start Menu, a folder that denies listing, a `.lnk`
that cannot be stat-ed, a profile that does not exist -- each degrades to fewer rows or an empty
list. Nothing in this module raises.
"""

from __future__ import annotations

import json
import os
import re
import time
from dataclasses import asdict, dataclass
from pathlib import Path

from typesafe_computer_use_win.catalog import slug

__all__ = [
    "DROP_FOLDERS",
    "DROP_NAMES",
    "App",
    "cache_path",
    "launch",
    "list_apps",
    "read_start_menu",
    "start_menu_roots",
]

SOURCE = "app"
CACHE_SECONDS = 24 * 60 * 60

# Weight is a prior on "is this a thing someone asks to open", and depth is the only signal a
# shortcut file carries. A shortcut sitting directly in `Programs` was put there by an installer
# that expected it to be launched (WhatsApp, Chrome); one three folders down is a sub-tool of a
# suite. So: 1.0 at the root, minus a step per folder, floored so nothing disappears entirely.
TOP_WEIGHT = 1.0
DEPTH_STEP = 0.15
MIN_WEIGHT = 0.3

MAX_DEPTH = 8  # a Start Menu is never this deep; the bound just stops a symlink loop

# Names that are never launch targets. The Start Menu is full of these and every one of them, if
# offered to the classifier, is a chance for a goal like "open word" to resolve to a readme.
DROP_NAMES = re.compile(
    r"(?:^|\W)(?:uninstall\w*|remove|setup|readme|release\s*notes?|documentation|help|license|licence|website|home\s*page|homepage)(?:\W|$)",
    re.IGNORECASE,
)

# Whole subtrees to skip, by folder name anywhere in the path under a Programs root.
#
# `Administrative Tools`, `Windows System`, `Windows PowerShell` and `Maintenance` hold consoles
# that can reconfigure or wipe the machine -- never something to hand a model as a launch option.
# `StartUp` is not a menu of apps at all, it is what Windows runs at logon.
#
# The accessibility subtrees matter most, and for a reason specific to this project: `Magnify`,
# `Narrator`, `On-Screen Keyboard`, `VoiceAccess` and `LiveCaptions` all seize the keyboard, the
# focus or the screen. A computer-use agent that launched Narrator or Voice Access would then be
# fighting it for control of the very machine it is driving, and the run could not recover.
DROP_FOLDERS = frozenset(
    {
        "administrative tools",
        "accessibility",
        "windows accessories",
        "windows system",
        "windows powershell",
        "maintenance",
        "startup",
    }
)

# The same accessibility tools also ship as loose shortcuts outside an `Accessibility` folder.
DROP_EXACT = frozenset(
    {
        "magnify",
        "magnifier",
        "narrator",
        "on-screen keyboard",
        "voiceaccess",
        "voice access",
        "livecaptions",
        "live captions",
    }
)

SHORTCUT_SUFFIX = " - Shortcut"


@dataclass(frozen=True)
class App:
    """One installed application the model may be asked to launch.

    The field is named `url`, not `path`, on purpose: `shortlist.shortlist` ranks anything carrying
    `key`/`label`/`url`/`weight`/`source`, and naming the `.lnk` path `url` means apps and sites go
    through the same ranker with no change to it at all. It holds a filesystem path to a shortcut.
    """

    key: str  # unique slug the model answers with: "whatsapp", "visual_studio_code"
    label: str  # the shortcut's own name: "WhatsApp", "Visual Studio Code"
    url: str  # the `.lnk` path -- see above for why it is not called `path`
    weight: float  # prior: higher is more likely to be what "open X" means
    source: str  # always "app"


def start_menu_roots() -> list[Path]:
    """The user and machine `Start Menu/Programs` directories. Neither need exist."""
    roots: list[Path] = []
    appdata = os.environ.get("APPDATA")
    if appdata:
        roots.append(Path(appdata) / "Microsoft" / "Windows" / "Start Menu" / "Programs")
    programdata = os.environ.get("PROGRAMDATA")
    if programdata:
        roots.append(Path(programdata) / "Microsoft" / "Windows" / "Start Menu" / "Programs")
    return roots


def _label_for(path: Path) -> str:
    """The shortcut's display name: its stem, with a trailing ` - Shortcut` stripped."""
    name = path.stem.strip()
    if name.lower().endswith(SHORTCUT_SUFFIX.lower()):
        name = name[: -len(SHORTCUT_SUFFIX)].strip()
    return name


def _dropped(label: str, relative: Path) -> bool:
    """True when this shortcut is noise, an admin console, or an accessibility tool."""
    if not label:
        return True
    lowered = label.lower()
    # `Administrative Tools` and friends also appear as a single loose shortcut to the folder.
    if lowered in DROP_EXACT or lowered in DROP_FOLDERS or DROP_NAMES.search(label) is not None:
        return True
    return any(part.lower() in DROP_FOLDERS for part in relative.parts[:-1])


def _weight_for(depth: int) -> float:
    """`1.0` at the Programs root, one `DEPTH_STEP` less per folder, never below `MIN_WEIGHT`."""
    return round(max(TOP_WEIGHT - DEPTH_STEP * max(depth, 0), MIN_WEIGHT), 6)


def _walk(root: Path) -> list[tuple[Path, int]]:
    """Every `.lnk` under `root` with its folder depth, skipping whatever cannot be listed."""
    out: list[tuple[Path, int]] = []
    stack: list[tuple[Path, int]] = [(root, 0)]
    while stack:
        folder, depth = stack.pop()
        if depth > MAX_DEPTH:
            continue
        try:
            entries = sorted(folder.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                if entry.is_dir():
                    stack.append((entry, depth + 1))
                elif entry.suffix.lower() == ".lnk":
                    out.append((entry, depth))
            except OSError:
                continue
    return out


def _unique_keys(apps: list[App]) -> list[App]:
    """Assign each row a unique slug key, suffixing a collision with a readable ordinal."""
    taken: set[str] = set()
    out: list[App] = []
    for app in apps:
        base = slug(app.label) or "app"
        key = base
        index = 2
        while key in taken:
            key = f"{base}_{index}"
            index += 1
        taken.add(key)
        out.append(App(key=key, label=app.label, url=app.url, weight=app.weight, source=SOURCE))
    return out


def read_start_menu(roots: list[Path] | None = None) -> list[App]:
    """Every launchable shortcut under `roots`, flattened, filtered and deduplicated by label.

    Nested folders collapse -- `Programs/Google/Chrome/Chrome.lnk` is just "Chrome" -- with the
    nesting surviving only as a lower weight. Two roots usually offer the same app twice; the
    shorter path wins, which is an arbitrary but stable tiebreak (the alternative, preferring the
    user Start Menu, is no more principled and changes with the profile).

    A missing root, an unreadable folder, or no Start Menu at all yields fewer rows, never an error.
    """
    if roots is None:
        roots = start_menu_roots()
    best: dict[str, tuple[int, str, App]] = {}
    for root in roots:
        root = Path(root)
        for file, depth in _walk(root):
            try:
                relative = file.relative_to(root)
            except ValueError:
                relative = Path(file.name)
            label = _label_for(file)
            if _dropped(label, relative):
                continue
            text = str(file)
            app = App(key="", label=label, url=text, weight=_weight_for(depth), source=SOURCE)
            rank = (len(text), text)
            existing = best.get(label.lower())
            if existing is None or rank < (existing[0], existing[1]):
                best[label.lower()] = (rank[0], rank[1], app)
    ordered = sorted((entry[2] for entry in best.values()), key=lambda a: (-a.weight, a.label.lower(), a.url))
    return _unique_keys(ordered)


def launch(app: App) -> bool:
    """Open `app` through the shell, returning False instead of raising when it cannot be opened.

    `os.startfile` on the `.lnk` lets the shell resolve the shortcut, which is what makes this work
    for Store apps and installers whose real target is not an exe path at all. The argument is an
    `App` from this catalog: no command line is ever assembled from a model's text.
    """
    path = getattr(app, "url", "")
    if not isinstance(path, str) or not path:
        return False
    starter = getattr(os, "startfile", None)
    if starter is None:  # not Windows
        return False
    try:
        starter(path)
    except OSError:
        return False
    return True


# ----------------------------------------------------------------- caching


def _local_dir() -> Path:
    """`%LOCALAPPDATA%/winclicker`, falling back to the home directory when the variable is unset."""
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_CACHE_HOME")
    root = Path(base) if base else Path.home() / ".cache"
    return root / "winclicker"


def cache_path() -> Path:
    """Where the built app list is cached."""
    return _local_dir() / "apps.json"


def _to_app(entry: object) -> App | None:
    """One cached row back into an `App`, or `None` when the row is not usable."""
    if not isinstance(entry, dict):
        return None
    key, label, url, source = entry.get("key"), entry.get("label"), entry.get("url"), entry.get("source")
    weight = entry.get("weight")
    if not all(isinstance(v, str) and v for v in (key, label, url, source)):
        return None
    if isinstance(weight, bool) or not isinstance(weight, int | float):
        return None
    return App(key=key, label=label, url=url, weight=float(weight), source=source)


def _read_cache(path: Path) -> list[App] | None:
    """The cached apps if the file is fresh and parses; otherwise `None` and the caller rebuilds."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return None
    if not isinstance(data, dict):
        return None
    built = data.get("built")
    if isinstance(built, bool) or not isinstance(built, int | float):
        return None
    if time.time() - float(built) > CACHE_SECONDS:
        return None
    rows = data.get("apps")
    if not isinstance(rows, list):
        return None
    apps = [app for app in (_to_app(row) for row in rows) if app is not None]
    return apps or None


def _write_cache(path: Path, apps: list[App]) -> None:
    """Best-effort cache write. A read-only or missing directory is not worth an exception."""
    payload = {"built": time.time(), "apps": [asdict(app) for app in apps]}
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    except (OSError, TypeError, ValueError):
        pass


def list_apps(refresh: bool = False) -> list[App]:
    """The app catalog, from cache when it is fresh and otherwise rebuilt and re-cached.

    Rebuilds on `refresh=True`, when the cache is absent, corrupt, or more than 24 hours old.
    Walking two Start Menu trees is far too slow to repeat on every step of a run.
    """
    path = cache_path()
    if not refresh:
        cached = _read_cache(path)
        if cached is not None:
            return cached
    apps = read_start_menu()
    _write_cache(path, apps)
    return apps
