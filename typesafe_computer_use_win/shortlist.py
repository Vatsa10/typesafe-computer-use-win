"""Pick the handful of sites worth showing the classifier.

The classifier answers a ``Choice`` over at most 255 options, and long before that limit the
probability mass spreads so thin that every answer reads as low confidence -- which trips the
run loop's confidence floor and stalls the run.  So this module is a ranker, not a filter: it
hands back few, well-separated options.

Pure ranking.  No model, no I/O, no browser, and deliberately no import of ``catalog``: any
object carrying ``key``, ``label``, ``url``, ``weight`` and ``source`` works.
"""

from __future__ import annotations

import re
from typing import Protocol

__all__ = ["DOMAIN_NOISE", "STOPWORDS", "domain_tokens", "score", "shortlist", "tokens"]


class SiteLike(Protocol):
    """Structural stand-in for ``catalog.Site`` -- kept structural so this module stays decoupled."""

    key: str
    label: str
    url: str
    weight: float
    source: str


# Words that carry no intent.  A goal made only of these must match nothing on overlap.
STOPWORDS = frozenset(
    {
        "a",
        "an",
        "and",
        "for",
        "go",
        "in",
        "me",
        "my",
        "on",
        "open",
        "page",
        "site",
        "the",
        "to",
        "up",
        "with",
    }
)

# Domain parts shared by half the web: two unrelated ".com" sites must not tie on "com".
DOMAIN_NOISE = frozenset({"www", "com", "co", "in", "org", "net", "app"})

# A token shorter than this never takes part in substring matching -- "in" inside "linkedin" is noise.
_MIN_SUBSTRING_LEN = 3

# Weight can only ever move a site by less than this, which is half of one substring hit.
# That keeps overlap strictly dominant and leaves the prior as a tie-breaker.
_PRIOR_CEILING = 0.1

_WORD = re.compile(r"[a-z0-9]+")
_SCHEME = re.compile(r"^[a-z][a-z0-9+.-]*://")


def tokens(text: str) -> set[str]:
    """Lowercased words with punctuation and domain parts split apart, minus stopwords.

    ``"Open the Colab research page"`` -> ``{"colab", "research"}``.
    """
    return {word for word in _WORD.findall(text.lower()) if word not in STOPWORDS}


def domain_tokens(url: str) -> set[str]:
    """The meaningful parts of a URL's host.  ``"https://mail.google.com/"`` -> ``{"mail", "google"}``."""
    rest = _SCHEME.sub("", url.strip().lower())
    host = re.split(r"[/?#]", rest, maxsplit=1)[0]
    host = host.rsplit("@", 1)[-1].split(":", 1)[0]
    return {part for part in _WORD.findall(host) if part not in DOMAIN_NOISE and part not in STOPWORDS}


def _site_tokens(site: SiteLike) -> set[str]:
    return tokens(site.label) | domain_tokens(site.url)


def _overlap(goal_words: set[str], site_words: set[str]) -> float:
    """1.0 for each goal word matched exactly, 0.5 for each matched only as a substring.

    The substring rule is what makes a goal saying "youtube" hit a site labelled "YouTube Music":
    a goal word matches a site word when either one contains the other and both are at least
    three characters long.  Half credit, so an exact hit always outranks a compound-word hit.
    """
    total = 0.0
    for word in goal_words:
        if word in site_words:
            total += 1.0
            continue
        if len(word) >= _MIN_SUBSTRING_LEN and any(
            len(other) >= _MIN_SUBSTRING_LEN and (word in other or other in word) for other in site_words
        ):
            total += 0.5
    return total


def _prior(weight: float) -> float:
    """Squash any weight into ``[0, _PRIOR_CEILING)`` so the prior can never outvote an overlap hit."""
    w = max(float(weight), 0.0)
    return _PRIOR_CEILING * (w / (1.0 + w))


def precision(matched: float, site_words: set[str]) -> float:
    """How much of the site's own name the match accounts for, softened by a square root.

    Without this, a bookmark titled "Shawn yzxiao chatytb engage with youtube" scores exactly as
    well for "open youtube" as the site actually called YouTube, because both contain the word
    once. Measured against the real profile, that long bookmark outranked youtube outright. So a
    match is worth more when it covers more of the label: 1 of 1 words beats 1 of 6. The square
    root keeps a two-word name like "YouTube Music" competitive rather than crushing it.
    """
    if not site_words:
        return 0.0
    return (min(matched, len(site_words)) / len(site_words)) ** 0.5


def score(goal: str, site: SiteLike) -> float:
    """``overlap(goal, label + domain) * precision + squashed prior``.

    The prior term is bounded below 0.1 while the smallest overlap step is 0.5, so a site whose
    label matches the goal always beats a heavier site that does not match at all.
    """
    site_words = _site_tokens(site)
    overlap = _overlap(tokens(goal), site_words)
    return overlap * precision(overlap, site_words) + _prior(site.weight)


def _order_key(scored: tuple[float, SiteLike]) -> tuple[float, float, str]:
    value, site = scored
    return (-value, -float(site.weight), site.key)


def shortlist(goal: str, sites: list, limit: int = 30) -> list:
    """The ~``limit`` sites worth offering for ``goal``, best first.

    Every ``source="pinned"`` site is included whatever it scores -- they are the curated core and
    must stay reachable.  The remaining budget goes to the highest-scoring other sites, and any
    non-pinned site with no overlap at all is dropped rather than padding the list out to ``limit``.
    Ordering is by score, then weight, then key, so the same goal always yields the same list.
    """
    goal_words = tokens(goal)
    pinned: list[tuple[float, SiteLike]] = []
    rest: list[tuple[float, SiteLike]] = []
    for site in sites:
        site_words = _site_tokens(site)
        overlap = _overlap(goal_words, site_words)
        value = overlap * precision(overlap, site_words) + _prior(site.weight)
        if site.source == "pinned":
            pinned.append((value, site))
        elif overlap > 0.0:
            rest.append((value, site))

    pinned.sort(key=_order_key)
    rest.sort(key=_order_key)
    remaining = max(limit - len(pinned), 0)
    chosen = pinned + rest[:remaining]
    chosen.sort(key=_order_key)
    return [site for _, site in chosen]
