//! Which sites the classifier is offered for one goal. A port of `sitepick.py`.
//!
//! The catalog is everything the browser knows: a few hundred origins from history, the bookmarks,
//! the sites the writer has resolved before, and the pinned core. The classifier cannot be handed
//! all of it. A `Choice` tops out at 255 options, and long before that the probability mass spreads
//! so thin that every answer reads as doubt — and the run loop stops below its confidence floor. So
//! a deterministic shortlist runs first, and the model only ever sees a few dozen well-separated
//! names.
//!
//! The goal does not change during a run, so this is computed once per run, not once per step.
//!
//! Configuration: `config.rs` is owned elsewhere during the port, so the three switches are read
//! straight from the environment here, with the Python's names and defaults:
//! `CLICKER_CATALOG` (default on), `CLICKER_CATALOG_TITLES` (default on) and
//! `CLICKER_CATALOG_LIMIT` (default 30).

use crate::apps::{self, App};
use crate::catalog::{self, urlsplit, Site};
use crate::shortlist;

pub const FALLBACK_SOURCE: &str = "pinned";

/// Options handed to the site question; the cap is 255, but mass thins fast.
pub const DEFAULT_CATALOG_LIMIT: usize = 30;

fn env_flag(name: &str) -> bool {
    let value = std::env::var(name).unwrap_or_else(|_| "1".to_string());
    !matches!(
        value.trim().to_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// `CLICKER_CATALOG`: off means the classifier sees only the pinned sites.
pub fn catalog_enabled() -> bool {
    env_flag("CLICKER_CATALOG")
}

/// `CLICKER_CATALOG_TITLES`: whether a page title may become a site's label. Off means labels are
/// bare domains, so no page title ever reaches the model.
pub fn catalog_titles() -> bool {
    env_flag("CLICKER_CATALOG_TITLES")
}

/// `CLICKER_CATALOG_LIMIT`. The Python raises on a malformed value; here it falls back to the
/// default, since a typo in an environment variable should not stop a run.
pub fn catalog_limit() -> usize {
    std::env::var("CLICKER_CATALOG_LIMIT")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|n| n.max(0) as usize)
        .unwrap_or(DEFAULT_CATALOG_LIMIT)
}

/// The sites to offer for this goal, best first. Never fails: a browser this cannot read, or a
/// catalog this cannot build, degrades to the pinned core rather than stopping a run.
pub fn sites_for(goal: &str, limit: Option<usize>) -> Vec<Site> {
    if !catalog_enabled() {
        return catalog::pinned_sites();
    }
    let limit = limit.unwrap_or_else(catalog_limit);
    let result = std::panic::catch_unwind(|| {
        let entries = catalog::load_catalog(false, catalog_titles());
        shortlist::shortlist(goal, &entries, limit)
    });
    result.unwrap_or_else(|_| catalog::pinned_sites())
}

/// Insert or overwrite, keeping first-insertion order like a Python dict.
fn put(lines: &mut Vec<(String, String)>, key: &str, line: String) {
    if let Some(slot) = lines.iter_mut().find(|(k, _)| k == key) {
        slot.1 = line;
    } else {
        lines.push((key.to_string(), line));
    }
}

/// The site question's options: one line per site, plus the two answers that are not sites.
///
/// A line carries the label and the host, because a bare slug is not always a name the model can
/// reason about, and the host is what disambiguates two things called "Music". Returned as an
/// ordered list of `(key, line)`, preserving the Python dict's insertion order.
pub fn criteria(sites: &[Site]) -> Vec<(String, String)> {
    let mut lines: Vec<(String, String)> = Vec::new();
    for site in sites {
        let netloc = urlsplit(&site.url).netloc;
        let host = if netloc.is_empty() {
            site.url.clone()
        } else {
            netloc
        };
        let line = if !host.to_lowercase().contains(&site.label.to_lowercase()) {
            format!("{} ({host})", site.label)
        } else {
            host
        };
        put(&mut lines, &site.key, line);
    }
    put(
        &mut lines,
        "other",
        "A website is needed to progress the goal, but it is not one of the sites named in this list.".to_string(),
    );
    put(
        &mut lines,
        "none",
        "No website needs to be opened: the page already open in the browser is the one to continue with.".to_string(),
    );
    lines
}

/// The URL behind an answered key. The model picks a name; code owns the address, which is why a
/// hallucinated URL is not a failure mode here.
pub fn url_for(sites: &[Site], key: &str) -> Option<String> {
    sites.iter().find(|s| s.key == key).map(|s| s.url.clone())
}

/// The installed applications worth offering for this goal, best first.
///
/// Same shape as the sites: a deterministic shortlist first, so the classifier is handed a few
/// names rather than everything installed. An app catalog that cannot be read is simply no apps —
/// the loop then has no open_app option, which is how it behaved before.
pub fn apps_for(goal: &str, limit: Option<usize>) -> Vec<App> {
    let limit = limit.unwrap_or_else(catalog_limit);
    std::panic::catch_unwind(|| {
        shortlist::shortlist(goal, &apps::list_apps(false), (limit / 2).max(1))
    })
    .unwrap_or_default()
}

/// The app question's options. The label is the shortcut's own name, which is what a person calls
/// it.
pub fn app_criteria(found: &[App]) -> Vec<(String, String)> {
    let mut lines: Vec<(String, String)> = Vec::new();
    for app in found {
        put(&mut lines, &app.key, app.label.clone());
    }
    lines
}

/// The app behind an answered key. The model names a key; code owns the path, so it can never
/// launch something that is not installed.
pub fn app_for<'a>(found: &'a [App], key: &str) -> Option<&'a App> {
    found.iter().find(|a| a.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn site(key: &str, label: &str, url: &str) -> Site {
        Site::new(key, label, url, 1.0, "history")
    }

    fn sites() -> Vec<Site> {
        vec![
            site("youtube", "YouTube", "https://www.youtube.com/"),
            site("gmail", "Gmail", "https://mail.google.com/"),
            site("notion", "Notion", "https://www.notion.so/"),
        ]
    }

    fn as_map(lines: Vec<(String, String)>) -> HashMap<String, String> {
        lines.into_iter().collect()
    }

    #[test]
    fn every_offered_site_becomes_one_option() {
        let lines = as_map(criteria(&sites()));
        for k in ["youtube", "gmail", "notion"] {
            assert!(lines.contains_key(k));
        }
    }

    /// Without `none` the loop cannot say "stay here", and without `other` it cannot reach
    /// anything outside the catalog.
    #[test]
    fn the_two_answers_that_are_not_sites_are_always_offered() {
        let keys: HashSet<String> = criteria(&[]).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            HashSet::from(["other".to_string(), "none".to_string()])
        );
    }

    #[test]
    fn an_option_carries_the_host_so_two_things_called_music_differ() {
        let lines = as_map(criteria(&[
            site("ytm", "Music", "https://music.youtube.com/"),
            site("sp", "Music", "https://open.spotify.com/"),
        ]));
        assert_ne!(lines["ytm"], lines["sp"]);
        assert!(
            lines["ytm"].contains("music.youtube.com") && lines["sp"].contains("open.spotify.com")
        );
    }

    #[test]
    fn a_label_already_inside_the_host_is_not_repeated() {
        let lines = as_map(criteria(&[site(
            "youtube",
            "youtube",
            "https://www.youtube.com/",
        )]));
        assert_eq!(lines["youtube"], "www.youtube.com");
    }

    #[test]
    fn an_answered_key_resolves_to_its_url() {
        assert_eq!(
            url_for(&sites(), "notion").as_deref(),
            Some("https://www.notion.so/")
        );
    }

    /// `other` and a stale key both land here, and both mean 'ask the writer', never 'guess'.
    #[test]
    fn a_key_that_is_not_offered_resolves_to_nothing() {
        assert_eq!(url_for(&sites(), "other"), None);
        assert_eq!(url_for(&sites(), "linkedin"), None);
    }

    #[test]
    fn no_site_ever_answers_for_another() {
        let all = sites();
        let urls: HashSet<Option<String>> = all.iter().map(|s| url_for(&all, &s.key)).collect();
        assert_eq!(urls.len(), all.len());
    }

    #[test]
    fn app_options_and_lookup() {
        let found = vec![App {
            key: "whatsapp".into(),
            label: "WhatsApp".into(),
            url: r"C:\x\WhatsApp.lnk".into(),
            weight: 1.0,
            source: "app".into(),
        }];
        assert_eq!(
            app_criteria(&found),
            vec![("whatsapp".to_string(), "WhatsApp".to_string())]
        );
        assert_eq!(app_for(&found, "whatsapp").unwrap().label, "WhatsApp");
        assert!(app_for(&found, "notepad").is_none());
    }
}
