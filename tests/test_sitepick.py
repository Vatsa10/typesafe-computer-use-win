"""Turning a shortlist into the site question, and back into a URL."""

from __future__ import annotations

from dataclasses import dataclass

from typesafe_computer_use_win.sitepick import criteria, url_for


@dataclass(frozen=True)
class FakeSite:
    key: str
    label: str
    url: str
    weight: float = 1.0
    source: str = "history"


SITES = [
    FakeSite("youtube", "YouTube", "https://www.youtube.com/"),
    FakeSite("gmail", "Gmail", "https://mail.google.com/"),
    FakeSite("notion", "Notion", "https://www.notion.so/"),
]


def test_every_offered_site_becomes_one_option():
    lines = criteria(SITES)
    assert {"youtube", "gmail", "notion"} <= set(lines)


def test_the_two_answers_that_are_not_sites_are_always_offered():
    """Without `none` the loop cannot say "stay here", and without `other` it cannot reach
    anything outside the catalog."""
    lines = criteria([])
    assert set(lines) == {"other", "none"}


def test_an_option_carries_the_host_so_two_things_called_music_differ():
    lines = criteria(
        [FakeSite("ytm", "Music", "https://music.youtube.com/"), FakeSite("sp", "Music", "https://open.spotify.com/")]
    )
    assert lines["ytm"] != lines["sp"]
    assert "music.youtube.com" in lines["ytm"] and "open.spotify.com" in lines["sp"]


def test_a_label_already_inside_the_host_is_not_repeated():
    assert criteria([FakeSite("youtube", "youtube", "https://www.youtube.com/")])["youtube"] == "www.youtube.com"


def test_an_answered_key_resolves_to_its_url():
    assert url_for(SITES, "notion") == "https://www.notion.so/"


def test_a_key_that_is_not_offered_resolves_to_nothing():
    """`other` and a stale key both land here, and both mean 'ask the writer', never 'guess'."""
    assert url_for(SITES, "other") is None
    assert url_for(SITES, "linkedin") is None


def test_no_site_ever_answers_for_another():
    urls = {url_for(SITES, s.key) for s in SITES}
    assert len(urls) == len(SITES)
