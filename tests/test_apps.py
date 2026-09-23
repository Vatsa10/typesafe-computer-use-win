"""Tests for the installed-application catalog.

Every test builds a fake Start Menu tree in `tmp_path`. The real Start Menu is never read and its
existence is never assumed, so this behaves the same on this machine and on a CI box with nothing
installed. `os.startfile` is always monkeypatched: calling the real one would open an application
on whoever's machine is running the suite.
"""

from __future__ import annotations

import json
import re
import time
from pathlib import Path

import pytest

from typesafe_computer_use_win import apps
from typesafe_computer_use_win.apps import (
    DROP_FOLDERS,
    App,
    cache_path,
    launch,
    list_apps,
    read_start_menu,
    start_menu_roots,
)
from typesafe_computer_use_win.shortlist import shortlist

SLUG = re.compile(r"^[a-z0-9]+(?:_[a-z0-9]+)*$")


def make(root: Path, *relative: str) -> list[Path]:
    """Create each `.lnk` (given as a relative path) under `root` and return the paths."""
    out: list[Path] = []
    for item in relative:
        path = root / item
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("fake shortcut", encoding="utf-8")
        out.append(path)
    return out


def labels(found: list[App]) -> set[str]:
    return {app.label for app in found}


def test_nested_folders_are_flattened(tmp_path: Path) -> None:
    make(tmp_path, "WhatsApp.lnk", "Google/Chrome/Chrome.lnk", "A/B/C/Deep Tool.lnk")
    found = read_start_menu([tmp_path])
    assert labels(found) == {"WhatsApp", "Chrome", "Deep Tool"}


def test_shortcut_suffix_is_stripped(tmp_path: Path) -> None:
    make(tmp_path, "Visual Studio Code - Shortcut.lnk")
    found = read_start_menu([tmp_path])
    assert [(app.label, app.key) for app in found] == [("Visual Studio Code", "visual_studio_code")]


def test_only_lnk_files_are_read(tmp_path: Path) -> None:
    make(tmp_path, "Real.lnk")
    (tmp_path / "Notes.txt").write_text("hi", encoding="utf-8")
    (tmp_path / "Thing.url").write_text("hi", encoding="utf-8")
    assert labels(read_start_menu([tmp_path])) == {"Real"}


@pytest.mark.parametrize(
    "name",
    [
        "Uninstall Steam.lnk",
        "Uninstaller.lnk",
        "Remove Python.lnk",
        "Setup.lnk",
        "Readme.lnk",
        "Release Notes.lnk",
        "Documentation.lnk",
        "Help.lnk",
        "License.lnk",
        "Node.js Website.lnk",
        "Homepage.lnk",
        "Home Page.lnk",
    ],
)
def test_noise_names_are_rejected(tmp_path: Path, name: str) -> None:
    make(tmp_path, name, "Keeper.lnk")
    assert labels(read_start_menu([tmp_path])) == {"Keeper"}


@pytest.mark.parametrize("folder", sorted(DROP_FOLDERS))
def test_dropped_subtrees_are_rejected(tmp_path: Path, folder: str) -> None:
    make(tmp_path, f"{folder.title()}/Something.lnk", f"{folder.title()}/Nested/Deeper.lnk", "Keeper.lnk")
    assert labels(read_start_menu([tmp_path])) == {"Keeper"}


@pytest.mark.parametrize("folder", sorted(DROP_FOLDERS))
def test_a_loose_shortcut_to_a_dropped_folder_is_rejected(tmp_path: Path, folder: str) -> None:
    make(tmp_path, f"{folder.title()}.lnk", "Keeper.lnk")
    assert labels(read_start_menu([tmp_path])) == {"Keeper"}


@pytest.mark.parametrize("name", ["Magnify", "Narrator", "On-Screen Keyboard", "VoiceAccess", "LiveCaptions"])
def test_accessibility_tools_are_never_launch_targets(tmp_path: Path, name: str) -> None:
    """These fight the agent for the keyboard, the focus or the screen, so they must never be offered."""
    make(tmp_path, f"Accessibility/{name}.lnk", f"{name}.lnk", "Keeper.lnk")
    assert labels(read_start_menu([tmp_path])) == {"Keeper"}


def test_dedup_by_label_keeps_the_shorter_path(tmp_path: Path) -> None:
    user = tmp_path / "user"
    machine = tmp_path / "machine-programs-folder"
    make(user, "Chrome.lnk")
    make(machine, "Google/Chrome/Chrome.lnk")
    found = read_start_menu([machine, user])
    assert len(found) == 1
    assert found[0].url == str(user / "Chrome.lnk")


def test_depth_lowers_weight(tmp_path: Path) -> None:
    make(tmp_path, "Top.lnk", "One/Mid.lnk", "One/Two/Three/Low.lnk")
    weights = {app.label: app.weight for app in read_start_menu([tmp_path])}
    assert weights["Top"] > weights["Mid"] > weights["Low"]
    assert weights["Top"] == apps.TOP_WEIGHT
    assert weights["Low"] >= apps.MIN_WEIGHT


def test_weight_is_bounded_at_extreme_depth(tmp_path: Path) -> None:
    make(tmp_path, "a/b/c/d/e/f/Buried.lnk")
    found = read_start_menu([tmp_path])
    assert found and found[0].weight == apps.MIN_WEIGHT


def test_keys_are_unique_and_slug_shaped(tmp_path: Path) -> None:
    make(tmp_path, "Visual Studio Code.lnk", "One/Visual Studio Code!.lnk", "WhatsApp.lnk", "Two/Micro$oft Edge.lnk")
    found = read_start_menu([tmp_path])
    keys = [app.key for app in found]
    assert len(keys) == len(set(keys)) == len(found)
    assert all(SLUG.match(key) for key in keys), keys
    assert "whatsapp" in keys
    assert "visual_studio_code" in keys


def test_missing_root_is_empty(tmp_path: Path) -> None:
    assert read_start_menu([tmp_path / "nope"]) == []
    assert read_start_menu([]) == []


def test_missing_root_among_real_ones_still_yields_the_others(tmp_path: Path) -> None:
    make(tmp_path, "Keeper.lnk")
    assert labels(read_start_menu([tmp_path / "nope", tmp_path])) == {"Keeper"}


def test_start_menu_roots_without_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("APPDATA", raising=False)
    monkeypatch.delenv("PROGRAMDATA", raising=False)
    assert start_menu_roots() == []


def test_start_menu_roots_from_environment(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("APPDATA", str(tmp_path / "roaming"))
    monkeypatch.setenv("PROGRAMDATA", str(tmp_path / "pd"))
    roots = start_menu_roots()
    assert len(roots) == 2
    assert all(root.name == "Programs" for root in roots)


# ---------------------------------------------------------------- launching


def test_launch_calls_startfile_with_the_catalog_path(monkeypatch: pytest.MonkeyPatch) -> None:
    seen: list[str] = []
    monkeypatch.setattr(apps.os, "startfile", seen.append, raising=False)
    app = App(key="whatsapp", label="WhatsApp", url=r"C:\fake\WhatsApp.lnk", weight=1.0, source="app")
    assert launch(app) is True
    assert seen == [r"C:\fake\WhatsApp.lnk"]


def test_launch_returns_false_when_startfile_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(_path: str) -> None:
        raise OSError("shortcut is gone")

    monkeypatch.setattr(apps.os, "startfile", boom, raising=False)
    app = App(key="gone", label="Gone", url=r"C:\fake\Gone.lnk", weight=1.0, source="app")
    assert launch(app) is False


def test_launch_returns_false_for_an_empty_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(apps.os, "startfile", lambda _path: pytest.fail("must not be called"), raising=False)
    assert launch(App(key="x", label="X", url="", weight=1.0, source="app")) is False


# ------------------------------------------------------------------ caching


@pytest.fixture
def local(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point the cache at `tmp_path` so no real `%LOCALAPPDATA%` file is touched."""
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path / "local"))
    return cache_path()


def test_list_apps_caches_and_reuses(local: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / "menu"
    make(root, "WhatsApp.lnk")
    monkeypatch.setattr(apps, "start_menu_roots", lambda: [root])
    first = list_apps(refresh=True)
    assert labels(first) == {"WhatsApp"}
    assert local.is_file()

    # A changed Start Menu is not seen until a refresh, which is the point of the cache.
    make(root, "Spotify.lnk")
    assert labels(list_apps()) == {"WhatsApp"}
    assert labels(list_apps(refresh=True)) == {"WhatsApp", "Spotify"}


def test_corrupt_cache_rebuilds(local: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / "menu"
    make(root, "WhatsApp.lnk")
    monkeypatch.setattr(apps, "start_menu_roots", lambda: [root])
    local.parent.mkdir(parents=True, exist_ok=True)
    local.write_text("{ not json at all", encoding="utf-8")
    assert labels(list_apps()) == {"WhatsApp"}
    assert json.loads(local.read_text(encoding="utf-8"))["apps"]


def test_stale_cache_rebuilds(local: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / "menu"
    make(root, "Spotify.lnk")
    monkeypatch.setattr(apps, "start_menu_roots", lambda: [root])
    local.parent.mkdir(parents=True, exist_ok=True)
    stale = {
        "built": time.time() - apps.CACHE_SECONDS - 60,
        "apps": [{"key": "old", "label": "Old", "url": "x.lnk", "weight": 1.0, "source": "app"}],
    }
    local.write_text(json.dumps(stale), encoding="utf-8")
    assert labels(list_apps()) == {"Spotify"}


def test_missing_cache_rebuilds(local: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / "menu"
    make(root, "Steam.lnk")
    monkeypatch.setattr(apps, "start_menu_roots", lambda: [root])
    assert not local.exists()
    assert labels(list_apps()) == {"Steam"}


def test_cache_rows_of_the_wrong_shape_are_ignored(local: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / "menu"
    make(root, "Steam.lnk")
    monkeypatch.setattr(apps, "start_menu_roots", lambda: [root])
    local.parent.mkdir(parents=True, exist_ok=True)
    local.write_text(json.dumps({"built": time.time(), "apps": ["nope", 3, {}]}), encoding="utf-8")
    assert labels(list_apps()) == {"Steam"}


def test_empty_start_menu_yields_no_apps(local: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(apps, "start_menu_roots", lambda: [tmp_path / "does-not-exist"])
    assert list_apps(refresh=True) == []


# ------------------------------------------------------------- the protocol


def test_apps_can_be_ranked_by_shortlist_unchanged(tmp_path: Path) -> None:
    """`App` satisfies what `shortlist` needs, so the site ranker works on apps with no change."""
    make(tmp_path, "WhatsApp.lnk", "Spotify.lnk", "Visual Studio Code.lnk", "Deep/Calculator.lnk")
    found = read_start_menu([tmp_path])
    ranked = shortlist("open whatsapp", found, limit=3)
    assert ranked and ranked[0].label == "WhatsApp"
    assert all(isinstance(app, App) for app in ranked)
    assert "Calculator" not in labels(ranked)
