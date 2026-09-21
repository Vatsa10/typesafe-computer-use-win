"""Which sites the classifier is offered for one goal.

The catalog is everything the browser knows: a few hundred origins from history, the bookmarks,
the sites the writer has resolved before, and the pinned core. The classifier cannot be handed all
of it. A `Choice` tops out at 255 options, and long before that the probability mass spreads so
thin that every answer reads as doubt — and the run loop stops below its confidence floor. So a
deterministic shortlist runs first, and the model only ever sees a few dozen well-separated names.

The goal does not change during a run, so this is computed once per run, not once per step.
"""

from __future__ import annotations

from . import config

FALLBACK_SOURCE = "pinned"


def sites_for(goal: str, limit: int | None = None) -> tuple:
    """The sites to offer for this goal, best first. Never raises: a browser this cannot read, or
    a catalog this cannot build, degrades to the pinned core rather than stopping a run."""
    from . import catalog

    if not config.catalog_enabled():
        return tuple(catalog.pinned_sites())
    limit = config.catalog_limit() if limit is None else limit
    try:
        from . import shortlist

        entries = catalog.load_catalog(titles=config.catalog_titles())
        return tuple(shortlist.shortlist(goal, entries, limit=limit))
    except Exception:
        return tuple(catalog.pinned_sites())


def criteria(sites) -> dict[str, str]:
    """The site question's options: one line per site, plus the two answers that are not sites.

    A line carries the label and the host, because a bare slug is not always a name the model can
    reason about, and the host is what disambiguates two things called "Music".
    """
    from urllib.parse import urlparse

    lines = {}
    for site in sites:
        host = urlparse(site.url).netloc or site.url
        lines[site.key] = f"{site.label} ({host})" if site.label.lower() not in host.lower() else host
    lines["other"] = "A website is needed to progress the goal, but it is not one of the sites named in this list."
    lines["none"] = "No website needs to be opened: the page already open in the browser is the one to continue with."
    return lines


def url_for(sites, key: str) -> str | None:
    """The URL behind an answered key. The model picks a name; code owns the address, which is why
    a hallucinated URL is not a failure mode here."""
    for site in sites:
        if site.key == key:
            return site.url
    return None
