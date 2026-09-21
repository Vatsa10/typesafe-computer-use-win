"""Tests for the browser-derived site catalog.

Everything here runs against a fake Chrome profile built in `tmp_path`: a `Bookmarks` JSON and a
`History` sqlite file written by the test. The real profile is never read, so the suite behaves the
same on this machine and on a CI box with no browser installed at all.
"""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path

import pytest

from typesafe_computer_use_win import catalog, config
from typesafe_computer_use_win.catalog import (
    Site,
    build_catalog,
    clean_label,
    load_catalog,
    merge_sites,
    pinned_sites,
    read_bookmarks,
    read_history,
    remember_site,
    slug,
)


def url_node(name: str, url: str) -> dict:
    return {"type": "url", "name": name, "url": url}


def folder(name: str, *children: dict) -> dict:
    return {"type": "folder", "name": name, "children": list(children)}


def write_bookmarks(path: Path, *, bar: list[dict] | None = None, other: list[dict] | None = None) -> Path:
    data = {
        "roots": {
            "bookmark_bar": {"type": "folder", "name": "Bookmarks bar", "children": bar or []},
            "other": {"type": "folder", "name": "Other bookmarks", "children": other or []},
            "sync_transaction_version": "1",  # Chrome puts non-folder junk in roots; it must not crash us
        },
        "version": 1,
    }
    path.write_text(json.dumps(data), encoding="utf-8")
    return path


def write_history(path: Path, rows: list[tuple[str, str, int]]) -> Path:
    connection = sqlite3.connect(path)
    try:
        connection.execute("CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT, visit_count INTEGER)")
        connection.executemany("INSERT INTO urls (url, title, visit_count) VALUES (?, ?, ?)", rows)
        connection.commit()
    finally:
        connection.close()
    return path


@pytest.fixture
def local(tmp_path: Path, monkeypatch) -> Path:
    """Point `%LOCALAPPDATA%` at a temp dir so the cache and learned store never touch the real one."""
    base = tmp_path / "local"
    base.mkdir()
    monkeypatch.setenv("LOCALAPPDATA", str(base))
    return base / "winclicker"


# ------------------------------------------------------------- clean_label


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("Inbox (9,842) - vatsajoshi2@gmail.com - Gmail", "Inbox"),
        ("Jiya Lage Na | YouTube Music", "Jiya Lage Na"),
        ("(3) WhatsApp", "WhatsApp"),
        ("Order #112-4839201-9930 - Amazon.in", "Order #"),
        ("  spaced   out    title  ", "spaced out title"),
        ("GitHub", "GitHub"),
        ("vatsajoshi2@gmail.com", ""),
        ("(12)", ""),
        ("Colab \u2014 Google Research", "Colab"),
        ("Docs \u00b7 Notion", "Docs"),
        ("", ""),
    ],
)
def test_clean_label_ugly_titles(raw: str, expected: str) -> None:
    assert clean_label(raw) == expected


def test_clean_label_strips_email_even_without_a_separator() -> None:
    assert "@" not in clean_label("Mail for vatsajoshi2@gmail.com today")


def test_clean_label_caps_at_sixty_characters_on_a_word_boundary() -> None:
    long = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor"
    out = clean_label(long)
    assert len(out) <= 60
    assert not out.endswith(" ")
    assert long.startswith(out)


def test_clean_label_tolerates_a_non_string() -> None:
    assert clean_label(None) == ""  # type: ignore[arg-type]


def test_slug_is_readable() -> None:
    assert slug("Colab") == "colab"
    assert slug("Google Calendar") == "google_calendar"
    assert slug("!!!") == "site"
    assert slug("colab.research.google") == "colab_research_google"


# --------------------------------------------------------------- bookmarks


def test_nested_bookmark_folders_are_flattened(tmp_path: Path) -> None:
    path = write_bookmarks(
        tmp_path / "Bookmarks",
        bar=[
            url_node("Hacker News", "https://news.ycombinator.com/"),
            folder(
                "Work",
                url_node("Linear", "https://linear.app/team"),
                folder("Deep", url_node("Colab", "https://colab.research.google.com/drive/abc123")),
            ),
        ],
        other=[url_node("Arxiv", "https://arxiv.org/")],
    )
    sites = read_bookmarks(path)
    assert {s.label for s in sites} == {"Hacker News", "Linear", "Colab", "Arxiv"}
    assert all(s.source == "bookmark" for s in sites)


def test_bookmarks_keep_their_exact_deep_link(tmp_path: Path) -> None:
    deep = "https://colab.research.google.com/drive/abc123"
    path = write_bookmarks(tmp_path / "Bookmarks", bar=[url_node("Colab", deep)])
    assert read_bookmarks(path)[0].url == deep


def test_bookmark_without_a_usable_url_is_dropped(tmp_path: Path) -> None:
    path = write_bookmarks(
        tmp_path / "Bookmarks",
        bar=[url_node("JS", "javascript:void(0)"), url_node("File", "file:///C:/x.html"), url_node("Ok", "https://ok.com/")],
    )
    assert [s.url for s in read_bookmarks(path)] == ["https://ok.com/"]


def test_bookmark_without_a_name_falls_back_to_the_domain(tmp_path: Path) -> None:
    path = write_bookmarks(tmp_path / "Bookmarks", bar=[url_node("", "https://www.example.com/x")])
    assert read_bookmarks(path)[0].label == "example"


def test_missing_bookmarks_file_is_empty(tmp_path: Path) -> None:
    assert read_bookmarks(tmp_path / "nope" / "Bookmarks") == []


def test_corrupt_bookmarks_file_is_empty(tmp_path: Path) -> None:
    path = tmp_path / "Bookmarks"
    path.write_text('{"roots": {"bookmark_bar": {"type": "fold', encoding="utf-8")
    assert read_bookmarks(path) == []


def test_bookmarks_with_wrong_shape_is_empty(tmp_path: Path) -> None:
    path = tmp_path / "Bookmarks"
    path.write_text('["not", "an", "object"]', encoding="utf-8")
    assert read_bookmarks(path) == []


# ----------------------------------------------------------------- history


def test_history_collapses_an_origin_into_one_weighted_entry(tmp_path: Path) -> None:
    path = write_history(
        tmp_path / "History",
        [
            ("https://www.youtube.com/watch?v=1", "Jiya Lage Na | YouTube Music", 10),
            ("https://www.youtube.com/watch?v=2", "Some Very Long Video Title Here - YouTube", 20),
            ("https://www.youtube.com/feed/subscriptions", "YouTube", 5),
            ("https://youtube.com/feed/history", "YouTube", 3),
            ("https://github.com/one", "one - GitHub", 4),
        ],
    )
    sites = read_history(path)
    youtube = [s for s in sites if "youtube" in s.url]
    assert len(youtube) == 1  # four rows, one entry: www and bare host are the same origin
    assert youtube[0].url == "https://youtube.com/"
    assert youtube[0].label == "YouTube"  # the recurring title, not a one-off video name
    assert youtube[0].weight > next(s for s in sites if "github" in s.url).weight


def test_history_prefers_a_recurring_title_over_a_one_off_page(tmp_path: Path) -> None:
    path = write_history(
        tmp_path / "History",
        [
            ("https://site.example.com/a", "Feed", 30),
            ("https://site.example.com/b", "Feed", 20),
            ("https://site.example.com/c", "gs", 90),  # most visited, but a one-off fragment
        ],
    )
    assert read_history(path)[0].label == "Feed"


def test_history_falls_back_to_the_domain_when_no_title_recurs(tmp_path: Path) -> None:
    """A page of unique titles is a page of search queries: the domain is the private answer."""
    rows = [(f"https://google.com/search?q={i}", f"how do i {i} - Google Search", 5) for i in range(6)]
    path = write_history(tmp_path / "History", rows)
    site = read_history(path)[0]
    assert site.label == "google"
    assert "how do i" not in site.label


def test_history_min_visits_filters_rows(tmp_path: Path) -> None:
    path = write_history(
        tmp_path / "History",
        [("https://rare.com/a", "Rare", 1), ("https://common.com/a", "Common", 9)],
    )
    assert [s.url for s in read_history(path, min_visits=2)] == ["https://common.com/"]
    assert len(read_history(path, min_visits=1)) == 2


def test_history_limit_keeps_the_most_visited(tmp_path: Path) -> None:
    rows = [(f"https://site{i}.com/", f"Site {i}", i + 2) for i in range(10)]
    sites = read_history(write_history(tmp_path / "History", rows), limit=3)
    assert [s.url for s in sites] == ["https://site9.com/", "https://site8.com/", "https://site7.com/"]


def test_history_weights_stay_under_a_bookmark(tmp_path: Path) -> None:
    path = write_history(tmp_path / "History", [("https://huge.com/a", "Huge", 1524), ("https://small.com/a", "Small", 2)])
    for site in read_history(path):
        assert catalog.HISTORY_FLOOR <= site.weight <= catalog.HISTORY_CEILING
        assert site.weight < catalog.BOOKMARK_WEIGHT


def test_history_origin_with_no_clean_title_falls_back_to_the_domain(tmp_path: Path) -> None:
    path = write_history(tmp_path / "History", [("https://mail.google.com/u/0", "vatsajoshi2@gmail.com", 6)])
    assert read_history(path)[0].label == "mail.google"


def test_missing_history_file_is_empty(tmp_path: Path) -> None:
    assert read_history(tmp_path / "History") == []


def test_history_that_is_not_a_database_is_empty(tmp_path: Path) -> None:
    path = tmp_path / "History"
    path.write_bytes(b"this is not sqlite at all, it is a locked or truncated file")
    assert read_history(path) == []


def test_history_without_a_urls_table_is_empty(tmp_path: Path) -> None:
    path = tmp_path / "History"
    connection = sqlite3.connect(path)
    connection.execute("CREATE TABLE visits (id INTEGER)")
    connection.close()
    assert read_history(path) == []


def test_history_read_never_opens_the_live_file_for_writing(tmp_path: Path, monkeypatch) -> None:
    """The DB is copied before reading, which is what makes a locked profile survivable."""
    path = write_history(tmp_path / "History", [("https://a.com/", "A", 5)])
    copied: list[str] = []
    real = catalog.shutil.copyfile

    def spy(src, dst, **kwargs):
        copied.append(str(src))
        return real(src, dst, **kwargs)

    monkeypatch.setattr(catalog.shutil, "copyfile", spy)
    assert read_history(path)
    assert copied == [str(path)]


# ------------------------------------------------------- the privacy switch


def test_titles_false_leaks_no_title_text(tmp_path: Path, local: Path) -> None:
    secret = "Inbox (9,842) - vatsajoshi2@gmail.com - Gmail"
    write_bookmarks(tmp_path / "Bookmarks", bar=[url_node(secret, "https://mail.google.com/u/0/inbox")])
    write_history(tmp_path / "History", [("https://bank.example.org/acct", "Statement for Vatsa Joshi", 40)])

    sites = build_catalog(titles=False, root=tmp_path)
    blob = json.dumps([{"key": s.key, "label": s.label} for s in sites])
    for leak in ("Inbox", "vatsajoshi2", "gmail.com", "Statement", "Vatsa", "Joshi", "9,842"):
        assert leak not in blob
    labels = {s.label for s in sites if s.source in ("bookmark", "history")}
    assert labels == {"mail.google", "bank.example"}


def test_titles_true_keeps_a_cleaned_title(tmp_path: Path, local: Path) -> None:
    write_bookmarks(tmp_path / "Bookmarks", bar=[url_node("Jiya Lage Na | YouTube Music", "https://music.youtube.com/w")])
    labels = {s.label for s in build_catalog(titles=True, root=tmp_path)}
    assert "Jiya Lage Na" in labels


# ------------------------------------------------------------------- keys


def test_keys_are_unique_and_collisions_get_a_domain_suffix(tmp_path: Path) -> None:
    sites = merge_sites(
        [
            [
                Site("", "Colab", "https://colab.research.google.com/", catalog.BOOKMARK_WEIGHT, "bookmark"),
                Site("", "Colab", "https://colab.example.org/", catalog.BOOKMARK_WEIGHT, "bookmark"),
                Site("", "Colab", "https://third.colab.net/", catalog.BOOKMARK_WEIGHT, "bookmark"),
            ]
        ]
    )
    keys = [s.key for s in sites]
    assert len(set(keys)) == 3
    assert "colab" in keys
    assert all(not k.split("_")[-1].isdigit() for k in keys)
    assert any(k == "colab_colab_research_google" for k in keys)


def test_keys_never_collapse_to_the_empty_string() -> None:
    sites = merge_sites([[Site("", "!!!", "https://a.example.com/", 0.6, "bookmark")]])
    assert sites[0].key


# --------------------------------------------------------- pinned / merge


def test_pinned_sites_cover_config_and_outrank_everything() -> None:
    sites = pinned_sites()
    assert {s.key for s in sites} == set(config.SITES)
    assert all(s.weight == catalog.PINNED_WEIGHT for s in sites)
    assert all(s.weight > catalog.BOOKMARK_WEIGHT for s in sites)


def test_pinned_sites_are_always_present_even_with_no_browser(tmp_path: Path, local: Path) -> None:
    sites = build_catalog(root=tmp_path / "no-such-profile")
    assert {s.key for s in sites} == set(config.SITES)


def test_pinned_keeps_its_config_key_when_a_bookmark_shares_the_label(tmp_path: Path, local: Path) -> None:
    write_bookmarks(tmp_path / "Bookmarks", bar=[url_node("Github", "https://github.com/explore")])
    sites = build_catalog(root=tmp_path)
    by_key = {s.key: s for s in sites}
    assert by_key["github"].url == config.SITES["github"]
    assert by_key["github"].source == "pinned"
    other = [s for s in sites if s.url == "https://github.com/explore"]
    assert other and other[0].key != "github"


def test_merge_precedence_prefers_the_better_source_for_the_same_url() -> None:
    url = "https://notion.so/"
    merged = merge_sites(
        [
            [Site("", "Notion hist", url, 0.4, "history")],
            [Site("", "Notion bm", url, catalog.BOOKMARK_WEIGHT, "bookmark")],
            [Site("", "Notion learned", url, catalog.LEARNED_WEIGHT, "learned")],
        ]
    )
    assert len(merged) == 1
    assert merged[0].source == "learned"


def test_merge_is_ordered_best_first() -> None:
    merged = merge_sites(
        [
            [Site("", "H", "https://h.com/", 0.2, "history")],
            [Site("", "P", "https://p.com/", 1.0, "pinned")],
            [Site("", "B", "https://b.com/", 0.6, "bookmark")],
        ]
    )
    assert [s.source for s in merged] == ["pinned", "bookmark", "history"]


def test_pinned_url_seen_in_history_stays_pinned(tmp_path: Path, local: Path) -> None:
    write_history(tmp_path / "History", [(config.SITES["gmail"], "Inbox (9,842) - Gmail", 900)])
    sites = build_catalog(root=tmp_path)
    gmail = [s for s in sites if s.url == config.SITES["gmail"]]
    assert len(gmail) == 1
    assert gmail[0].source == "pinned"
    assert gmail[0].weight == catalog.PINNED_WEIGHT


# ------------------------------------------------------- learned + caching


def test_remember_site_survives_a_rebuild(tmp_path: Path, local: Path) -> None:
    site = remember_site("My Dashboard", "https://dash.internal.example.com/home")
    assert site.source == "learned"
    assert site.key == "my_dashboard"
    rebuilt = build_catalog(root=tmp_path / "no-profile")
    assert any(s.url == "https://dash.internal.example.com/home" and s.source == "learned" for s in rebuilt)


def test_remember_site_does_not_duplicate_the_same_url(tmp_path: Path, local: Path) -> None:
    remember_site("Old", "https://dash.example.com/")
    remember_site("New", "https://dash.example.com/")
    learned = json.loads(catalog.learned_path().read_text(encoding="utf-8"))
    assert learned == [{"label": "New", "url": "https://dash.example.com/"}]


def test_remember_site_writes_utf8(tmp_path: Path, local: Path) -> None:
    remember_site("Caf\u00e9 \u2014 \u65e5\u672c", "https://cafe.example.com/")
    text = catalog.learned_path().read_text(encoding="utf-8")
    assert "cafe.example.com" in text


def test_learned_outranks_a_bookmark_for_the_same_place(tmp_path: Path, local: Path) -> None:
    remember_site("Dash", "https://dash.example.com/")
    write_bookmarks(tmp_path / "Bookmarks", bar=[url_node("Dash bookmark", "https://dash.example.com/")])
    sites = build_catalog(root=tmp_path)
    match = [s for s in sites if s.url == "https://dash.example.com/"]
    assert len(match) == 1
    assert match[0].source == "learned"
    assert match[0].weight > catalog.BOOKMARK_WEIGHT


def test_corrupt_learned_store_degrades_to_empty(tmp_path: Path, local: Path) -> None:
    catalog.learned_path().parent.mkdir(parents=True, exist_ok=True)
    catalog.learned_path().write_text("{not json", encoding="utf-8")
    assert catalog.learned_sites() == []
    assert build_catalog(root=tmp_path / "none")  # pinned still there


def test_cache_round_trip(tmp_path: Path, local: Path, monkeypatch) -> None:
    first = load_catalog(refresh=True)
    assert catalog.cache_path().is_file()

    def explode(*args, **kwargs):
        raise AssertionError("load_catalog rebuilt instead of reading a fresh cache")

    monkeypatch.setattr(catalog, "build_catalog", explode)
    again = load_catalog()
    assert [(s.key, s.url, s.weight, s.source) for s in again] == [(s.key, s.url, s.weight, s.source) for s in first]


def test_cache_is_created_under_localappdata(tmp_path: Path, local: Path) -> None:
    load_catalog(refresh=True)
    assert catalog.cache_path() == local / "catalog.json"
    assert catalog.learned_path() == local / "learned.json"


def test_corrupt_cache_rebuilds_instead_of_raising(tmp_path: Path, local: Path) -> None:
    path = catalog.cache_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{ this is not json", encoding="utf-8")
    sites = load_catalog()
    assert {s.key for s in sites} >= set(config.SITES)
    assert json.loads(path.read_text(encoding="utf-8"))["sites"]


def test_stale_cache_rebuilds(tmp_path: Path, local: Path, monkeypatch) -> None:
    load_catalog(refresh=True)
    payload = json.loads(catalog.cache_path().read_text(encoding="utf-8"))
    payload["built"] = time.time() - catalog.CACHE_SECONDS - 60
    payload["sites"] = [{"key": "stale", "label": "Stale", "url": "https://stale.com/", "weight": 1.0, "source": "pinned"}]
    catalog.cache_path().write_text(json.dumps(payload), encoding="utf-8")
    assert [s.key for s in load_catalog()] != ["stale"]


def test_cache_does_not_serve_the_other_titles_setting(tmp_path: Path, local: Path, monkeypatch) -> None:
    load_catalog(refresh=True, titles=True)
    built: list[bool] = []
    real = catalog.build_catalog

    def spy(titles=True, root=None):
        built.append(titles)
        return real(titles=titles, root=root)

    monkeypatch.setattr(catalog, "build_catalog", spy)
    load_catalog(titles=False)
    assert built == [False]


def test_load_catalog_tolerates_an_unwritable_cache_dir(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setattr(catalog, "cache_path", lambda: tmp_path / "file.txt" / "catalog.json")
    (tmp_path / "file.txt").write_text("blocking", encoding="utf-8")
    assert load_catalog(refresh=True)
