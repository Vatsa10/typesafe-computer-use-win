//! Pick the handful of sites worth showing the classifier. A port of `shortlist.py`.
//!
//! The classifier answers a `Choice` over at most 255 options, and long before that limit the
//! probability mass spreads so thin that every answer reads as low confidence -- which trips the
//! run loop's confidence floor and stalls the run. So this module is a ranker, not a filter: it
//! hands back few, well-separated options.
//!
//! Pure ranking. No model, no I/O, no browser, and deliberately no dependency on `catalog`: any
//! type implementing `SiteLike` (`key`, `label`, `url`, `weight`, `source`) works, which is how
//! `apps::App` goes through the same ranker unchanged.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

/// Structural stand-in for `catalog::Site` -- kept as a trait so this module stays decoupled.
pub trait SiteLike {
    fn key(&self) -> &str;
    fn label(&self) -> &str;
    fn url(&self) -> &str;
    fn weight(&self) -> f64;
    fn source(&self) -> &str;
}

impl SiteLike for crate::catalog::Site {
    fn key(&self) -> &str {
        &self.key
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn weight(&self) -> f64 {
        self.weight
    }
    fn source(&self) -> &str {
        &self.source
    }
}

impl SiteLike for crate::apps::App {
    fn key(&self) -> &str {
        &self.key
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn weight(&self) -> f64 {
        self.weight
    }
    fn source(&self) -> &str {
        &self.source
    }
}

/// Words that carry no intent. A goal made only of these must match nothing on overlap.
pub const STOPWORDS: [&str; 16] = [
    "a", "an", "and", "for", "go", "in", "me", "my", "on", "open", "page", "site", "the", "to",
    "up", "with",
];

/// Domain parts shared by half the web: two unrelated ".com" sites must not tie on "com".
pub const DOMAIN_NOISE: [&str; 7] = ["www", "com", "co", "in", "org", "net", "app"];

/// A token shorter than this never takes part in substring matching -- "in" inside "linkedin" is
/// noise.
const MIN_SUBSTRING_LEN: usize = 3;

/// Weight can only ever move a site by less than this, which is half of one substring hit.
/// That keeps overlap strictly dominant and leaves the prior as a tie-breaker.
const PRIOR_CEILING: f64 = 0.1;

fn word_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| Regex::new(r"[a-z0-9]+").unwrap())
}

fn scheme_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| Regex::new(r"^[a-z][a-z0-9+.-]*://").unwrap())
}

/// Lowercased words with punctuation and domain parts split apart, minus stopwords.
///
/// `"Open the Colab research page"` -> `{"colab", "research"}`.
pub fn tokens(text: &str) -> HashSet<String> {
    let lowered = text.to_lowercase();
    word_re()
        .find_iter(&lowered)
        .map(|m| m.as_str())
        .filter(|w| !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

/// The meaningful parts of a URL's host. `"https://mail.google.com/"` -> `{"mail", "google"}`.
pub fn domain_tokens(url: &str) -> HashSet<String> {
    let lowered = url.trim().to_lowercase();
    let rest = scheme_re().replace(&lowered, "");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    word_re()
        .find_iter(host)
        .map(|m| m.as_str())
        .filter(|w| !DOMAIN_NOISE.contains(w) && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

fn site_tokens<S: SiteLike + ?Sized>(site: &S) -> HashSet<String> {
    let mut out = tokens(site.label());
    out.extend(domain_tokens(site.url()));
    out
}

/// 1.0 for each goal word matched exactly, 0.5 for each matched only as a substring.
///
/// The substring rule is what makes a goal saying "youtube" hit a site labelled "YouTube Music":
/// a goal word matches a site word when either one contains the other and both are at least
/// three characters long. Half credit, so an exact hit always outranks a compound-word hit.
fn overlap(goal_words: &HashSet<String>, site_words: &HashSet<String>) -> f64 {
    let mut total = 0.0;
    for word in goal_words {
        if site_words.contains(word) {
            total += 1.0;
            continue;
        }
        if word.len() >= MIN_SUBSTRING_LEN
            && site_words.iter().any(|other| {
                other.len() >= MIN_SUBSTRING_LEN
                    && (other.contains(word.as_str()) || word.contains(other.as_str()))
            })
        {
            total += 0.5;
        }
    }
    total
}

/// Squash any weight into `[0, PRIOR_CEILING)` so the prior can never outvote an overlap hit.
fn prior(weight: f64) -> f64 {
    let w = weight.max(0.0);
    PRIOR_CEILING * (w / (1.0 + w))
}

/// How much of the site's own name the match accounts for, softened by a square root.
///
/// Without this, a bookmark titled "Shawn yzxiao chatytb engage with youtube" scores exactly as
/// well for "open youtube" as the site actually called YouTube, because both contain the word
/// once. Measured against the real profile, that long bookmark outranked youtube outright. So a
/// match is worth more when it covers more of the label: 1 of 1 words beats 1 of 6. The square
/// root keeps a two-word name like "YouTube Music" competitive rather than crushing it.
pub fn precision(matched: f64, site_words: &HashSet<String>) -> f64 {
    if site_words.is_empty() {
        return 0.0;
    }
    let n = site_words.len() as f64;
    (matched.min(n) / n).sqrt()
}

/// `overlap(goal, label + domain) * precision + squashed prior`.
///
/// The prior term is bounded below 0.1 while the smallest overlap step is 0.5, so a site whose
/// label matches the goal always beats a heavier site that does not match at all.
pub fn score<S: SiteLike + ?Sized>(goal: &str, site: &S) -> f64 {
    let site_words = site_tokens(site);
    let hit = overlap(&tokens(goal), &site_words);
    hit * precision(hit, &site_words) + prior(site.weight())
}

/// Score descending, then weight descending, then key ascending.
fn order<S: SiteLike>(a: &(f64, &S), b: &(f64, &S)) -> std::cmp::Ordering {
    use std::cmp::Ordering::Equal;
    b.0.partial_cmp(&a.0)
        .unwrap_or(Equal)
        .then_with(|| b.1.weight().partial_cmp(&a.1.weight()).unwrap_or(Equal))
        .then_with(|| a.1.key().cmp(b.1.key()))
}

/// The ~`limit` sites worth offering for `goal`, best first. (Python's default `limit` is 30.)
///
/// Every `source="pinned"` site is included whatever it scores -- they are the curated core and
/// must stay reachable. The remaining budget goes to the highest-scoring other sites, and any
/// non-pinned site with no overlap at all is dropped rather than padding the list out to `limit`.
/// Ordering is by score, then weight, then key, so the same goal always yields the same list.
pub fn shortlist<S: SiteLike + Clone>(goal: &str, sites: &[S], limit: usize) -> Vec<S> {
    let goal_words = tokens(goal);
    let mut pinned: Vec<(f64, &S)> = Vec::new();
    let mut rest: Vec<(f64, &S)> = Vec::new();
    for site in sites {
        let site_words = site_tokens(site);
        let hit = overlap(&goal_words, &site_words);
        let value = hit * precision(hit, &site_words) + prior(site.weight());
        if site.source() == "pinned" {
            pinned.push((value, site));
        } else if hit > 0.0 {
            rest.push((value, site));
        }
    }
    pinned.sort_by(order);
    rest.sort_by(order);
    let remaining = limit.saturating_sub(pinned.len());
    rest.truncate(remaining);
    let mut chosen = pinned;
    chosen.extend(rest);
    chosen.sort_by(order);
    chosen.into_iter().map(|(_, s)| s.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Local stand-in with the same fields as `catalog::Site`, so these tests stay decoupled.
    #[derive(Debug, Clone, PartialEq)]
    struct FakeSite {
        key: String,
        label: String,
        url: String,
        weight: f64,
        source: String,
    }

    impl SiteLike for FakeSite {
        fn key(&self) -> &str {
            &self.key
        }
        fn label(&self) -> &str {
            &self.label
        }
        fn url(&self) -> &str {
            &self.url
        }
        fn weight(&self) -> f64 {
            self.weight
        }
        fn source(&self) -> &str {
            &self.source
        }
    }

    fn site(key: &str, label: &str, url: &str) -> FakeSite {
        site_w(key, label, url, 1.0, "history")
    }

    fn site_w(key: &str, label: &str, url: &str, weight: f64, source: &str) -> FakeSite {
        FakeSite {
            key: key.into(),
            label: label.into(),
            url: url.into(),
            weight,
            source: source.into(),
        }
    }

    fn keys(sites: &[FakeSite]) -> Vec<String> {
        sites.iter().map(|s| s.key.clone()).collect()
    }

    fn set(words: &[&str]) -> HashSet<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn tokens_splits_punctuation_and_lowercases() {
        assert_eq!(
            tokens("Colab-Research (Google)!"),
            set(&["colab", "research", "google"])
        );
    }

    #[test]
    fn tokens_drops_stopwords() {
        assert!(tokens("open the page").is_empty());
        for w in [
            "the", "a", "open", "go", "to", "my", "and", "on", "in", "page", "site",
        ] {
            assert!(STOPWORDS.contains(&w));
        }
    }

    #[test]
    fn domain_tokens_strips_scheme_path_and_noise() {
        assert_eq!(
            domain_tokens("https://mail.google.com/"),
            set(&["mail", "google"])
        );
        assert_eq!(
            domain_tokens("https://www.notion.so/workspace?x=1"),
            set(&["notion", "so"])
        );
        for w in ["www", "com", "co", "in", "org", "net", "app"] {
            assert!(DOMAIN_NOISE.contains(&w));
        }
    }

    #[test]
    fn label_match_beats_much_heavier_unrelated_prior() {
        let notion = site_w("notion", "Notion", "https://notion.so/", 1.0, "history");
        let youtube = site_w(
            "youtube",
            "YouTube",
            "https://youtube.com/",
            10.0,
            "history",
        );
        assert!(score("open notion", &notion) > score("open notion", &youtube));
        assert_eq!(
            keys(&shortlist("open notion", &[youtube, notion], 30)),
            vec!["notion"]
        );
    }

    #[test]
    fn domain_match_works_when_the_label_does_not() {
        let gmail = site("gmail", "Mail", "https://mail.google.com/");
        assert_eq!(keys(&shortlist("check gmail", &[gmail], 30)), vec!["gmail"]);
    }

    #[test]
    fn substring_matches_compound_labels_but_scores_below_exact() {
        let exact = site("yt", "YouTube", "https://youtube.com/");
        let compound = site("ytm", "YouTube Music", "https://music.youtube.com/");
        // The exact name wins: "YouTube" is entirely about youtube, "YouTube Music" only half so.
        // Measured on the real profile, without this a bookmark titled "...engage with youtube"
        // outranked the site actually called YouTube.
        assert!(score("youtube", &exact) > score("youtube", &compound));
        let partial = site("ytk", "Youtubers Guild", "https://ytguild.example/");
        assert!(score("youtube", &exact) > score("youtube", &partial));
    }

    #[test]
    fn pinned_always_present_even_at_zero_overlap() {
        let pinned = site_w("figma", "Figma", "https://figma.com/", 0.1, "pinned");
        let hit = site("notion", "Notion", "https://notion.so/");
        let result = shortlist("open notion", &[pinned, hit], 30);
        assert_eq!(keys(&result), vec!["notion", "figma"]);
    }

    #[test]
    fn zero_overlap_non_pinned_entries_are_dropped_not_padded() {
        let mut sites: Vec<FakeSite> = (0..10)
            .map(|i| {
                site(
                    &format!("s{i}"),
                    &format!("Site {i}"),
                    &format!("https://s{i}.example/"),
                )
            })
            .collect();
        sites.push(site("notion", "Notion", "https://notion.so/"));
        assert_eq!(keys(&shortlist("notion", &sites, 30)), vec!["notion"]);
    }

    #[test]
    fn stopword_only_goal_matches_nothing() {
        let sites = [
            site("notion", "Notion", "https://notion.so/"),
            site("yt", "YouTube", "https://youtube.com/"),
        ];
        assert!(shortlist("open the page", &sites, 30).is_empty());
    }

    #[test]
    fn www_and_com_do_not_create_matches() {
        let a = site("a", "Alpha", "https://www.alpha.com/");
        let b = site("b", "Beta", "https://www.beta.com/");
        assert!(shortlist("www com", &[a.clone(), b.clone()], 30).is_empty());
        assert_eq!(keys(&shortlist("alpha", &[a, b], 30)), vec!["a"]);
    }

    #[test]
    fn limit_respected_and_highest_scores_kept() {
        let sites: Vec<FakeSite> = (0..10)
            .map(|i| {
                site_w(
                    &format!("notion{i}"),
                    "Notion",
                    &format!("https://notion{i}.example/"),
                    i as f64,
                    "history",
                )
            })
            .collect();
        assert_eq!(
            keys(&shortlist("notion", &sites, 3)),
            vec!["notion9", "notion8", "notion7"]
        );
    }

    #[test]
    fn ties_broken_deterministically_by_weight_then_key() {
        let mut sites = vec![
            site_w("zulu", "Notion", "https://zulu.example/", 1.0, "history"),
            site_w("alpha", "Notion", "https://alpha.example/", 1.0, "history"),
            site_w("mike", "Notion", "https://mike.example/", 2.0, "history"),
        ];
        let expected = vec!["mike", "alpha", "zulu"];
        assert_eq!(keys(&shortlist("notion", &sites, 30)), expected);
        sites.reverse();
        assert_eq!(keys(&shortlist("notion", &sites, 30)), expected);
    }

    #[test]
    fn empty_catalog_and_empty_goal_return_sensibly() {
        let none: [FakeSite; 0] = [];
        assert!(shortlist("notion", &none, 30).is_empty());
        assert!(shortlist("", &none, 30).is_empty());
        let pinned = site_w("figma", "Figma", "https://figma.com/", 1.0, "pinned");
        let plain = site("notion", "Notion", "https://notion.so/");
        assert!(shortlist("", std::slice::from_ref(&plain), 30).is_empty());
        assert_eq!(keys(&shortlist("", &[pinned, plain], 30)), vec!["figma"]);
    }

    #[test]
    fn five_hundred_sites_stay_under_the_limit_and_stay_fast() {
        let sites: Vec<FakeSite> = (0..500)
            .map(|i| {
                site_w(
                    &format!("notion{i}"),
                    &format!("Notion {i}"),
                    &format!("https://notion{i}.example/"),
                    (i % 7) as f64,
                    "history",
                )
            })
            .collect();
        let start = Instant::now();
        let result = shortlist("open my notion page", &sites, 30);
        let elapsed = start.elapsed().as_secs_f64();
        assert_eq!(result.len(), 30);
        assert!(
            elapsed < 0.25,
            "shortlist took {elapsed:.3}s -- this runs on every step"
        );
    }
}
