"""Build a site catalog out of the user's own browser, so the model is not limited to a hardcoded dict.

This is a pure data layer. Nothing here calls a model, draws a UI, or touches the decision loop: it
reads two files that Chrome happens to leave on disk, turns them into `Site` rows, and caches the
result. The decision loop asks for `load_catalog()` and gets a list.

Tolerance is the whole job, exactly as in `runs_index`. A machine with no Chrome, a profile that
moved, a `Bookmarks` file half-written by a crashed browser, a `History` database locked because
Chrome is running right now -- each of those degrades to an empty list. `build_catalog` with no
browser at all still returns the pinned sites from `config.SITES`, so the curated core is never lost.

Two hard privacy rules shape the code:

* Only `Bookmarks` and the `urls` table of `History` are ever opened. Never the password store, the
  cookie jar, `Web Data`, or any autofill database. There is no code path here that names them.
* `titles=False` means no page title reaches the catalog at all -- every label becomes the bare
  domain. `clean_label` is the softer version of the same instinct: it strips the email addresses,
  unread counts and order numbers that browser titles are full of.
"""

from __future__ import annotations

import json
import math
import os
import re
import shutil
import sqlite3
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import urlsplit

from typesafe_computer_use_win import config

# Source ranking, best first. A key present in two sources keeps the better one.
SOURCES = ("pinned", "learned", "bookmark", "history")

PINNED_WEIGHT = 1.0
LEARNED_WEIGHT = 0.8  # the writer already resolved this one for a real goal: trust it over a bookmark
BOOKMARK_WEIGHT = 0.6
HISTORY_FLOOR = 0.05
HISTORY_CEILING = 0.45  # strictly under BOOKMARK_WEIGHT, so 1524 visits cannot outrank a bookmark

MAX_LABEL = 60
MAX_SLUG = 40
CACHE_SECONDS = 24 * 60 * 60

# Titles are written by whoever owns the page, and they leak.
EMAIL = re.compile(r"\b[\w.+-]+@[\w-]+\.[\w.-]+\b")
COUNT = re.compile(r"[(\[{]\s*[\d][\d,.\s]*[)\]}]")  # "Inbox (9,842)", "[12]"
DIGIT_RUN = re.compile(r"\b[\d][\d,-]{4,}\b")  # order numbers, ticket ids
# Built from code points so the source stays plain ASCII and the separators stay unambiguous.
DASHES = "".join(chr(code) for code in (0x2013, 0x2014, 0x00B7, 0x2022))  # en dash, em dash, dot, bullet
SEPARATOR = re.compile(rf"\s+[-|:{DASHES}]\s+")  # " - ", " | ", " -- ", " . "
NOISE_EDGE = re.compile(rf"^[\s\-|:,.{DASHES}]+|[\s\-|:,.{DASHES}]+$")
NOT_SLUG = re.compile(r"[^a-z0-9]+")
COMMON_TLD = ("com", "org", "net", "io", "co", "app", "dev", "ai", "in", "uk", "so")


@dataclass(frozen=True)
class Site:
    """One place the model may be asked to open."""

    key: str  # unique slug the model answers with: "youtube", "colab_research_google"
    label: str  # human text for the criteria line: "YouTube"
    url: str  # what gets opened
    weight: float  # prior: higher is more likely wanted
    source: str  # "pinned" | "bookmark" | "history" | "learned"


def _rank(source: str) -> int:
    """Lower is better. An unknown source sorts last rather than raising."""
    return SOURCES.index(source) if source in SOURCES else len(SOURCES)


def _host(url: str) -> str:
    """The hostname without `www.`, or `""` for anything that is not an http(s) URL."""
    try:
        parts = urlsplit(url)
    except ValueError:
        return ""
    if parts.scheme not in ("http", "https") or not parts.hostname:
        return ""
    host = parts.hostname.lower()
    return host[4:] if host.startswith("www.") else host


def _origin(url: str) -> str:
    """`https://host/` for an http(s) URL, or `""`.

    `www.` is dropped and ports are kept, so `www.youtube.com` and `youtube.com` aggregate into one
    history entry instead of two that mean the same thing.
    """
    try:
        parts = urlsplit(url)
    except ValueError:
        return ""
    host = _host(url)
    if not host:
        return ""
    port = f":{parts.port}" if parts.port else ""
    return f"{parts.scheme}://{host}{port}/"


def _identity(url: str) -> str:
    """A www- and trailing-slash-insensitive identity, used only to decide that two rows are the
    same place. The winning row keeps its own URL verbatim."""
    parts = urlsplit(url)
    path = parts.path.rstrip("/")
    query = f"?{parts.query}" if parts.query else ""
    return f"{_host(url)}{path}{query}"


def _domain_label(url: str) -> str:
    """The fallback label: the host with `www.` and a trailing common TLD removed.

    `mail.google.com` -> `mail.google`, which reads better in a criteria line than the full host and
    is still unambiguous. A host that is only a TLD-looking token is left alone.
    """
    host = _host(url)
    if not host:
        return ""
    parts = host.split(".")
    if len(parts) > 1 and parts[-1] in COMMON_TLD:
        parts = parts[:-1]
    return ".".join(parts) or host


def clean_label(title: str) -> str:
    """A browser title, reduced to something short, safe to show, and free of personal detail.

    Strips, in order: email addresses, parenthesised counts (`Inbox (9,842)` -> `Inbox`), long digit
    runs such as order numbers, and every segment after a ` - ` / ` | ` / ` — ` separator, which in
    a page title is almost always the site's own name. Whitespace collapses and the result is capped
    at 60 characters on a word boundary. A title that cleans away to nothing returns `""`; callers
    substitute the domain.
    """
    if not isinstance(title, str):
        return ""
    text = EMAIL.sub(" ", title)
    text = COUNT.sub(" ", text)
    text = DIGIT_RUN.sub(" ", text)
    # Keep the first segment that still has content: the leading one is the page, the rest is chrome.
    segments = [NOISE_EDGE.sub("", part) for part in SEPARATOR.split(text)]
    text = next((part for part in segments if part.strip()), "")
    text = NOISE_EDGE.sub("", re.sub(r"\s+", " ", text)).strip()
    if len(text) <= MAX_LABEL:
        return text
    cut = text[:MAX_LABEL]
    space = cut.rfind(" ")
    return (cut[:space] if space > MAX_LABEL // 2 else cut).rstrip()


def slug(text: str) -> str:
    """A lowercase identifier a model can type back verbatim: `slug("Colab")` -> `"colab"`."""
    value = NOT_SLUG.sub("_", (text or "").lower()).strip("_")
    if len(value) > MAX_SLUG:
        value = value[:MAX_SLUG].rstrip("_")
    return value or "site"


def _letters(text: str) -> str:
    """A short alphabetic tag derived from `text`, for the last-resort collision suffix.

    Alphabetic on purpose: a bare number tells the model nothing, and the brief forbids one.
    """
    total = 0
    for char in text:
        total = (total * 131 + ord(char)) % (26**4)
    out = ""
    for _ in range(4):
        total, index = divmod(total, 26)
        out += chr(ord("a") + index)
    return out


def _candidates(site: Site) -> list[str]:
    """Key candidates for `site`, best first. Every one is readable; none is a bare number."""
    base = slug(site.label)
    domain = slug(_domain_label(site.url) or _host(site.url))
    path = urlsplit(site.url).path.strip("/") if _host(site.url) else ""
    host = slug(_host(site.url))
    out = [base]
    if domain and domain != base:
        out += [f"{base}_{domain}", domain]
    if host and host != base:
        out.append(f"{base}_{host}")
    if path:
        out.append(slug(f"{base}_{path}"))
    out.append(f"{base}_{_letters(site.url)}")
    return out


def _key_for(site: Site, taken: set[str]) -> str:
    """The first unused candidate key, falling back to a hash-derived alphabetic suffix."""
    for candidate in _candidates(site):
        if candidate not in taken:
            return candidate
    tail = _letters(site.url + site.label)
    return f"{slug(site.label)}_{tail}"


def _site(label: str, url: str, weight: float, source: str) -> Site | None:
    """A `Site` with a settled label, or `None` when the URL is not something we can open."""
    url = url.strip() if isinstance(url, str) else ""
    if not _host(url):
        return None
    text = label.strip() if isinstance(label, str) else ""
    return Site(key="", label=text or _domain_label(url), url=url, weight=weight, source=source)


# ---------------------------------------------------------------- bookmarks


def _walk_bookmarks(node: object, out: list[Site], titles: bool, depth: int = 0) -> None:
    """Flatten a bookmark tree in place. Folders nest arbitrarily; only `url` nodes produce a Site."""
    if depth > 30 or not isinstance(node, dict):
        return
    kind = node.get("type")
    if kind == "url":
        url = node.get("url")
        if isinstance(url, str):
            raw = node.get("name") if titles else ""
            label = clean_label(raw if isinstance(raw, str) else "")
            site = _site(label, url, BOOKMARK_WEIGHT, "bookmark")
            if site is not None:
                out.append(site)
        return
    children = node.get("children")
    if isinstance(children, list):
        for child in children:
            _walk_bookmarks(child, out, titles, depth + 1)


def read_bookmarks(path: Path, titles: bool = True) -> list[Site]:
    """Every bookmark under every root, flattened, keeping each bookmark's exact URL.

    A bookmark is curated: a deep link into a specific Colab notebook was saved on purpose, so unlike
    history these are not collapsed to their origin. A missing, unreadable or corrupt file is an
    empty list.
    """
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return []
    roots = data.get("roots") if isinstance(data, dict) else None
    if not isinstance(roots, dict):
        return []
    out: list[Site] = []
    for root in roots.values():
        _walk_bookmarks(root, out, titles)
    return out


# ------------------------------------------------------------------ history


def _read_url_rows(path: Path) -> list[tuple[str, str, int]]:
    """`(url, title, visit_count)` from the `urls` table, via a copy of the database.

    Chrome holds an exclusive lock on `History` while it runs, so the live file cannot be opened even
    read-only. Copying it to a temp file and reading the copy is the one thing that works, and it is
    also the safest: nothing here can write to the real profile. Only the `urls` table is touched.
    """
    source = Path(path)
    if not source.is_file():
        return []
    handle, temp_name = tempfile.mkstemp(prefix="winclicker-history-", suffix=".db")
    os.close(handle)
    temp = Path(temp_name)
    try:
        try:
            shutil.copyfile(source, temp)
        except OSError:
            return []
        try:
            connection = sqlite3.connect(f"file:{temp}?mode=ro", uri=True)
        except sqlite3.Error:
            return []
        try:
            cursor = connection.execute("SELECT url, title, visit_count FROM urls")
            rows = cursor.fetchall()
        except sqlite3.Error:
            return []
        finally:
            connection.close()
    finally:
        temp.unlink(missing_ok=True)
    out: list[tuple[str, str, int]] = []
    for url, title, visits in rows:
        if not isinstance(url, str):
            continue
        count = visits if isinstance(visits, int) and not isinstance(visits, bool) else 0
        out.append((url, title if isinstance(title, str) else "", count))
    return out


def _best_title(titles: list[tuple[str, int]], domain: str) -> str:
    """One representative label for an origin, or `""` when none of its titles is worth using.

    An origin has hundreds of titles and almost all of them describe a page, not the site: a search
    query, a video name, a document heading. Picking the shortest outright gives nonsense like `gs`
    for google.com, and picking the most-visited leaks whatever the user happened to read most.

    So the measure of "cleanest" here is recurrence: a cleaned title that shows up on several
    different rows is the site's own furniture -- `Feed`, `Inbox`, `Claude` -- while a one-off is
    content. Among recurring titles, one that echoes the domain wins, then the most frequent, then
    the shortest, then alphabetical order so a rebuild picks the same one again. If nothing recurs,
    this returns `""` and the caller falls back to the domain, which is the private answer anyway.
    """
    counts: dict[str, int] = {}
    visits_by: dict[str, int] = {}
    for title, visits in titles:
        text = clean_label(title)
        if len(text) < 2:
            continue
        counts[text] = counts.get(text, 0) + 1
        visits_by[text] = visits_by.get(text, 0) + visits
    recurring = [text for text, count in counts.items() if count > 1]
    if not recurring:
        return ""
    stem = domain.split(".")[0].lower()

    def score(text: str) -> tuple:
        lowered = text.lower()
        echoes = not (stem and (stem in lowered or lowered in stem))
        return (echoes, -counts[text], len(text), -visits_by[text], text)

    return min(recurring, key=score)


def read_history(path: Path, titles: bool = True, min_visits: int = 2, limit: int = 400) -> list[Site]:
    """Browsing history, aggregated to one entry per origin and ranked by total visits.

    Tens of thousands of URLs collapse to a few hundred domains: visit counts are summed per origin,
    and one representative label is chosen from that origin's titles. Rows under `min_visits` are
    dropped before aggregating, so a single accidental click never becomes a catalog entry. Weights
    are log-normalised into `[0.05, 0.45]`, below `BOOKMARK_WEIGHT`, so a domain with 1524 visits
    still cannot outrank a deliberately saved bookmark.

    Any failure -- absent file, locked database, not a database at all, missing `urls` table --
    returns an empty list.
    """
    if limit <= 0:
        return []
    totals: dict[str, int] = {}
    seen_titles: dict[str, list[tuple[str, int]]] = {}
    for url, title, visits in _read_url_rows(path):
        if visits < min_visits:
            continue
        origin = _origin(url)
        if not origin:
            continue
        totals[origin] = totals.get(origin, 0) + visits
        if titles and title:
            seen_titles.setdefault(origin, []).append((title, visits))
    if not totals:
        return []
    ranked = sorted(totals.items(), key=lambda pair: (-pair[1], pair[0]))[:limit]
    top = max(count for _, count in ranked)
    span = HISTORY_CEILING - HISTORY_FLOOR
    scale = _log(top)
    out: list[Site] = []
    for origin, count in ranked:
        share = (_log(count) / scale) if scale else 1.0
        weight = round(HISTORY_FLOOR + span * share, 6)
        label = _best_title(seen_titles.get(origin, []), _domain_label(origin)) if titles else ""
        site = _site(label, origin, weight, "history")
        if site is not None:
            out.append(site)
    return out


def _log(count: int) -> float:
    """`log(1 + count)`; log so that a 1524-visit domain is a few times a 5-visit one, not 300x."""
    return math.log1p(max(count, 0))


# ------------------------------------------------------- pinned and learned


def pinned_sites() -> list[Site]:
    """The curated core from `config.SITES`, at the top weight so it is always reachable."""
    out: list[Site] = []
    for key, url in config.SITES.items():
        site = _site(key.replace("_", " ").title(), url, PINNED_WEIGHT, "pinned")
        if site is not None:
            out.append(Site(key=key, label=site.label, url=url, weight=PINNED_WEIGHT, source="pinned"))
    return out


def _local_dir() -> Path:
    """`%LOCALAPPDATA%/winclicker`, falling back to the home directory when the variable is unset."""
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_CACHE_HOME")
    root = Path(base) if base else Path.home() / ".cache"
    return root / "winclicker"


def learned_path() -> Path:
    """Where sites resolved by the writer are kept, separate from the cache so a rebuild keeps them."""
    return _local_dir() / "learned.json"


def cache_path() -> Path:
    """Where the built catalog is cached."""
    return _local_dir() / "catalog.json"


def _read_json_list(path: Path) -> list:
    """The JSON list a file holds, or `[]` for missing, unreadable, corrupt or wrong-shaped."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return []
    return data if isinstance(data, list) else []


def learned_sites() -> list[Site]:
    """What the writer has resolved before, ranked under pinned but over bookmarks."""
    out: list[Site] = []
    for entry in _read_json_list(learned_path()):
        if not isinstance(entry, dict):
            continue
        site = _site(str(entry.get("label", "")), str(entry.get("url", "")), LEARNED_WEIGHT, "learned")
        if site is not None:
            out.append(site)
    return out


def remember_site(label: str, url: str) -> Site:
    """Record a site the writer resolved, so the next rebuild still knows it.

    Written to `learned.json`, never to the cache: the cache is disposable and a refresh would throw
    this away. Re-remembering the same URL updates its label instead of adding a duplicate row. A
    write that fails still returns the Site, because the caller is mid-run and should not be stopped
    by a read-only disk.
    """
    site = _site(label, url, LEARNED_WEIGHT, "learned")
    if site is None:
        site = Site(key=slug(label), label=label.strip(), url=url, weight=LEARNED_WEIGHT, source="learned")
        return site
    entries = [e for e in _read_json_list(learned_path()) if isinstance(e, dict) and e.get("url") != site.url]
    entries.append({"label": site.label, "url": site.url})
    path = learned_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(entries, indent=2), encoding="utf-8")
    except OSError:
        pass
    return Site(key=slug(site.label), label=site.label, url=site.url, weight=LEARNED_WEIGHT, source="learned")


# ------------------------------------------------------------ merge / build


def merge_sites(groups: list[list[Site]]) -> list[Site]:
    """One list, deduped by URL and then keyed uniquely, keeping the best source for each place.

    "Best" is the source ranking -- pinned, learned, bookmark, history -- and the higher weight
    within a source. Keys are assigned last, in final order, so the winner of a collision is the
    higher-ranked site and the loser gets a domain-based suffix rather than a bare number.
    """
    best: dict[str, Site] = {}
    for group in groups:
        for site in group:
            identity = _identity(site.url)
            existing = best.get(identity)
            if existing is None or (_rank(site.source), -site.weight) < (_rank(existing.source), -existing.weight):
                best[identity] = site
    ordered = sorted(best.values(), key=lambda s: (_rank(s.source), -s.weight, s.label.lower(), s.url))
    taken: set[str] = set()
    out: list[Site] = []
    for site in ordered:
        # A pinned key is part of the prompt contract already: keep it verbatim when it is free.
        key = site.key if site.source == "pinned" and site.key and site.key not in taken else _key_for(site, taken)
        taken.add(key)
        out.append(Site(key=key, label=site.label, url=site.url, weight=site.weight, source=site.source))
    return out


def chrome_profile() -> Path:
    """The default Chrome profile directory. It need not exist; every reader tolerates its absence."""
    base = os.environ.get("LOCALAPPDATA")
    root = Path(base) if base else Path.home() / "AppData" / "Local"
    return root / "Google" / "Chrome" / "User Data" / "Default"


def build_catalog(titles: bool = True, root: Path | None = None) -> list[Site]:
    """The full catalog, read fresh from disk.

    `root` is the browser profile directory holding `Bookmarks` and `History`; it defaults to the
    default Chrome profile. With no browser at all this still returns the pinned sites plus whatever
    the writer has learned, which is the floor the decision loop is entitled to assume.
    """
    profile = chrome_profile() if root is None else Path(root)
    return merge_sites(
        [
            pinned_sites(),
            learned_sites(),
            read_bookmarks(profile / "Bookmarks", titles=titles),
            read_history(profile / "History", titles=titles),
        ]
    )


# ----------------------------------------------------------------- caching


def _to_site(entry: object) -> Site | None:
    """One cached row back into a `Site`, or `None` when the row is not usable."""
    if not isinstance(entry, dict):
        return None
    key, label, url = entry.get("key"), entry.get("label"), entry.get("url")
    weight, source = entry.get("weight"), entry.get("source")
    if not all(isinstance(v, str) and v for v in (key, label, url, source)):
        return None
    if isinstance(weight, bool) or not isinstance(weight, int | float):
        return None
    return Site(key=key, label=label, url=url, weight=float(weight), source=source)


def _read_cache(path: Path, titles: bool) -> list[Site] | None:
    """The cached catalog if it is fresh, matches `titles` and parses; otherwise `None`.

    A corrupt cache is indistinguishable here from an absent one, on purpose: the caller rebuilds.
    """
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return None
    if not isinstance(data, dict) or data.get("titles") is not titles:
        return None
    built = data.get("built")
    if isinstance(built, bool) or not isinstance(built, int | float):
        return None
    if time.time() - float(built) > CACHE_SECONDS:
        return None
    rows = data.get("sites")
    if not isinstance(rows, list):
        return None
    sites = [site for site in (_to_site(row) for row in rows) if site is not None]
    return sites or None


def _write_cache(path: Path, sites: list[Site], titles: bool) -> None:
    """Best-effort cache write. A read-only or missing directory is not worth an exception."""
    payload = {"built": time.time(), "titles": titles, "sites": [asdict(site) for site in sites]}
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    except (OSError, TypeError, ValueError):
        pass


def load_catalog(refresh: bool = False, titles: bool = True) -> list[Site]:
    """The catalog, from cache when it is fresh and otherwise rebuilt and re-cached.

    Rebuilds when `refresh=True`, when the cache is absent or corrupt, when it was built for the
    other `titles` setting, or when it is more than 24 hours old. Building reads two files and a
    sqlite copy, which is too slow to do on every step of a run.
    """
    path = cache_path()
    if not refresh:
        cached = _read_cache(path, titles)
        if cached is not None:
            return cached
    sites = build_catalog(titles=titles)
    _write_cache(path, sites, titles)
    return sites
