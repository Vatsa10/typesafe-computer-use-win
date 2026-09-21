"""Ranking tests for the shortlist.  No model, no I/O: a local stand-in stands in for catalog.Site."""

from __future__ import annotations

import time
from dataclasses import dataclass

from typesafe_computer_use_win.shortlist import DOMAIN_NOISE, STOPWORDS, domain_tokens, score, shortlist, tokens


@dataclass(frozen=True)
class FakeSite:
    """Local stand-in with the same fields as catalog.Site, so this module stays decoupled."""

    key: str
    label: str
    url: str
    weight: float = 1.0
    source: str = "history"


def keys(sites: list) -> list[str]:
    return [site.key for site in sites]


def test_tokens_splits_punctuation_and_lowercases() -> None:
    assert tokens("Colab-Research (Google)!") == {"colab", "research", "google"}


def test_tokens_drops_stopwords() -> None:
    assert tokens("open the page") == set()
    assert {"the", "a", "open", "go", "to", "my", "and", "on", "in", "page", "site"} <= STOPWORDS


def test_domain_tokens_strips_scheme_path_and_noise() -> None:
    assert domain_tokens("https://mail.google.com/") == {"mail", "google"}
    assert domain_tokens("https://www.notion.so/workspace?x=1") == {"notion", "so"}
    assert {"www", "com", "co", "in", "org", "net", "app"} <= DOMAIN_NOISE


def test_label_match_beats_much_heavier_unrelated_prior() -> None:
    notion = FakeSite("notion", "Notion", "https://notion.so/", weight=1.0)
    youtube = FakeSite("youtube", "YouTube", "https://youtube.com/", weight=10.0)
    assert score("open notion", notion) > score("open notion", youtube)
    assert keys(shortlist("open notion", [youtube, notion])) == ["notion"]


def test_domain_match_works_when_the_label_does_not() -> None:
    gmail = FakeSite("gmail", "Mail", "https://mail.google.com/")
    assert keys(shortlist("check gmail", [gmail])) == ["gmail"]


def test_substring_matches_compound_labels_but_scores_below_exact() -> None:
    exact = FakeSite("yt", "YouTube", "https://youtube.com/")
    compound = FakeSite("ytm", "YouTube Music", "https://music.youtube.com/")
    # The exact name wins: "YouTube" is entirely about youtube, "YouTube Music" only half so.
    # Measured on the real profile, without this a bookmark titled "...engage with youtube"
    # outranked the site actually called YouTube.
    assert score("youtube", exact) > score("youtube", compound)
    partial = FakeSite("ytk", "Youtubers Guild", "https://ytguild.example/")
    assert score("youtube", exact) > score("youtube", partial)


def test_pinned_always_present_even_at_zero_overlap() -> None:
    pinned = FakeSite("figma", "Figma", "https://figma.com/", weight=0.1, source="pinned")
    hit = FakeSite("notion", "Notion", "https://notion.so/")
    result = shortlist("open notion", [pinned, hit])
    assert set(keys(result)) == {"notion", "figma"}
    assert keys(result) == ["notion", "figma"]


def test_zero_overlap_non_pinned_entries_are_dropped_not_padded() -> None:
    sites = [FakeSite(f"s{i}", f"Site {i}", f"https://s{i}.example/") for i in range(10)]
    sites.append(FakeSite("notion", "Notion", "https://notion.so/"))
    assert keys(shortlist("notion", sites)) == ["notion"]


def test_stopword_only_goal_matches_nothing() -> None:
    sites = [FakeSite("notion", "Notion", "https://notion.so/"), FakeSite("yt", "YouTube", "https://youtube.com/")]
    assert shortlist("open the page", sites) == []


def test_www_and_com_do_not_create_matches() -> None:
    a = FakeSite("a", "Alpha", "https://www.alpha.com/")
    b = FakeSite("b", "Beta", "https://www.beta.com/")
    assert shortlist("www com", [a, b]) == []
    assert keys(shortlist("alpha", [a, b])) == ["a"]


def test_limit_respected_and_highest_scores_kept() -> None:
    sites = [FakeSite(f"notion{i}", "Notion", f"https://notion{i}.example/", weight=float(i)) for i in range(10)]
    result = shortlist("notion", sites, limit=3)
    assert keys(result) == ["notion9", "notion8", "notion7"]


def test_ties_broken_deterministically_by_weight_then_key() -> None:
    sites = [
        FakeSite("zulu", "Notion", "https://zulu.example/", weight=1.0),
        FakeSite("alpha", "Notion", "https://alpha.example/", weight=1.0),
        FakeSite("mike", "Notion", "https://mike.example/", weight=2.0),
    ]
    expected = ["mike", "alpha", "zulu"]
    assert keys(shortlist("notion", sites)) == expected
    assert keys(shortlist("notion", list(reversed(sites)))) == expected


def test_empty_catalog_and_empty_goal_return_sensibly() -> None:
    assert shortlist("notion", []) == []
    assert shortlist("", []) == []
    pinned = FakeSite("figma", "Figma", "https://figma.com/", source="pinned")
    plain = FakeSite("notion", "Notion", "https://notion.so/")
    assert shortlist("", [plain]) == []
    assert keys(shortlist("", [pinned, plain])) == ["figma"]


def test_five_hundred_sites_stay_under_the_limit_and_stay_fast() -> None:
    sites = [FakeSite(f"notion{i}", f"Notion {i}", f"https://notion{i}.example/", weight=float(i % 7)) for i in range(500)]
    start = time.perf_counter()
    result = shortlist("open my notion page", sites)
    elapsed = time.perf_counter() - start
    assert len(result) == 30
    assert elapsed < 0.25, f"shortlist took {elapsed:.3f}s -- this runs on every step"
