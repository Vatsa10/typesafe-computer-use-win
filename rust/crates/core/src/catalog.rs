//! Build a site catalog out of the user's own browser, so the model is not limited to a hardcoded
//! dict. A port of `catalog.py`.
//!
//! This is a pure data layer. Nothing here calls a model, draws a UI, or touches the decision loop:
//! it reads two files that Chrome happens to leave on disk, turns them into `Site` rows, and caches
//! the result. The decision loop asks for `load_catalog()` and gets a list.
//!
//! Tolerance is the whole job, exactly as in `runs_index`. A machine with no Chrome, a profile that
//! moved, a `Bookmarks` file half-written by a crashed browser, a `History` database locked because
//! Chrome is running right now -- each of those degrades to an empty list. `build_catalog` with no
//! browser at all still returns the pinned sites from `SITES`, so the curated core is never lost.
//!
//! Two hard privacy rules shape the code:
//!
//! * Only `Bookmarks` and the `urls` table of `History` are ever opened. Never the password store,
//!   the cookie jar, `Web Data`, or any autofill database. There is no code path here that names
//!   them.
//! * `titles=false` means no page title reaches the catalog at all -- every label becomes the bare
//!   domain. `clean_label` is the softer version of the same instinct: it strips the email
//!   addresses, unread counts and order numbers that browser titles are full of.
//!
//! Testability: every function that the Python reads from `%LOCALAPPDATA%` or the Chrome profile
//! has an explicit-directory twin (`*_in` / `*_with`), so tests never mutate the process
//! environment and never touch the real profile.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The curated core. `config.rs` is the single source; re-exported here for callers of
/// `catalog::SITES`.
pub use crate::config::SITES;

/// Source ranking, best first. A key present in two sources keeps the better one.
pub const SOURCES: [&str; 4] = ["pinned", "learned", "bookmark", "history"];

pub const PINNED_WEIGHT: f64 = 1.0;
/// The writer already resolved this one for a real goal: trust it over a bookmark.
pub const LEARNED_WEIGHT: f64 = 0.8;
pub const BOOKMARK_WEIGHT: f64 = 0.6;
pub const HISTORY_FLOOR: f64 = 0.05;
/// Strictly under `BOOKMARK_WEIGHT`, so 1524 visits cannot outrank a bookmark.
pub const HISTORY_CEILING: f64 = 0.45;

pub const MAX_LABEL: usize = 60;
pub const MAX_SLUG: usize = 40;
pub const CACHE_SECONDS: f64 = 24.0 * 60.0 * 60.0;

const COMMON_TLD: [&str; 11] = [
    "com", "org", "net", "io", "co", "app", "dev", "ai", "in", "uk", "so",
];

// Titles are written by whoever owns the page, and they leak.
// The dash class is built from code points so the source stays plain ASCII and the separators stay
// unambiguous: en dash, em dash, middle dot, bullet.
const DASHES: &str = r"\x{2013}\x{2014}\x{00B7}\x{2022}";

struct Patterns {
    email: Regex,
    count: Regex,
    digit_run: Regex,
    separator: Regex,
    noise_edge: Regex,
    spaces: Regex,
    not_slug: Regex,
}

fn patterns() -> &'static Patterns {
    static CELL: OnceLock<Patterns> = OnceLock::new();
    CELL.get_or_init(|| Patterns {
        email: Regex::new(r"\b[\w.+-]+@[\w-]+\.[\w.-]+\b").unwrap(),
        // "Inbox (9,842)", "[12]"
        count: Regex::new(r"[(\[{]\s*\d[\d,.\s]*[)\]}]").unwrap(),
        // order numbers, ticket ids
        digit_run: Regex::new(r"\b\d[\d,-]{4,}\b").unwrap(),
        // " - ", " | ", " -- ", " . "
        separator: Regex::new(&format!(r"\s+[\-|:{DASHES}]\s+")).unwrap(),
        noise_edge: Regex::new(&format!(r"^[\s\-|:,.{DASHES}]+|[\s\-|:,.{DASHES}]+$")).unwrap(),
        spaces: Regex::new(r"\s+").unwrap(),
        not_slug: Regex::new(r"[^a-z0-9]+").unwrap(),
    })
}

/// One place the model may be asked to open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Site {
    /// Unique slug the model answers with: "youtube", "colab_research_google".
    pub key: String,
    /// Human text for the criteria line: "YouTube".
    pub label: String,
    /// What gets opened.
    pub url: String,
    /// Prior: higher is more likely wanted.
    pub weight: f64,
    /// "pinned" | "bookmark" | "history" | "learned".
    pub source: String,
}

impl Site {
    pub fn new(key: &str, label: &str, url: &str, weight: f64, source: &str) -> Self {
        Site {
            key: key.to_string(),
            label: label.to_string(),
            url: url.to_string(),
            weight,
            source: source.to_string(),
        }
    }
}

// ------------------------------------------------------------------- URL parsing

/// The pieces of `urllib.parse.urlsplit` this module uses. Fragment is dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct UrlParts {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
}

impl UrlParts {
    /// `parts.hostname`: the netloc without userinfo and port, lowercased; `None` when empty.
    pub fn hostname(&self) -> Option<String> {
        let after_user = self.netloc.rsplit('@').next().unwrap_or("");
        let host = if let Some(rest) = after_user.strip_prefix('[') {
            rest.split(']').next().unwrap_or("")
        } else {
            after_user.split(':').next().unwrap_or("")
        };
        if host.is_empty() {
            None
        } else {
            Some(host.to_lowercase())
        }
    }

    /// `parts.port`, or `None` when absent or not a number. Python raises on a malformed port when
    /// `.port` is read; here it simply counts as absent, which keeps every reader total.
    pub fn port(&self) -> Option<u16> {
        let after_user = self.netloc.rsplit('@').next().unwrap_or("");
        let tail = match after_user.rfind(']') {
            Some(i) => &after_user[i + 1..],
            None => after_user,
        };
        let (_, port) = tail.split_once(':')?;
        port.parse::<u16>().ok()
    }
}

/// `urlsplit`, as far as this crate needs it.
pub(crate) fn urlsplit(url: &str) -> UrlParts {
    // urlsplit strips C0 control characters and spaces at the ends, and drops tabs and newlines.
    let cleaned: String = url
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest: &str = &cleaned;
    let mut scheme = String::new();
    if let Some(colon) = rest.find(':') {
        let candidate = &rest[..colon];
        let valid = !candidate.is_empty()
            && candidate
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if valid {
            scheme = candidate.to_ascii_lowercase();
            rest = &rest[colon + 1..];
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_string();
        rest = &after[end..];
    }
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (rest.to_string(), String::new()),
    };
    UrlParts {
        scheme,
        netloc,
        path,
        query,
    }
}

// ---------------------------------------------------------------------- helpers

/// Lower is better. An unknown source sorts last rather than raising.
fn rank(source: &str) -> usize {
    SOURCES
        .iter()
        .position(|s| *s == source)
        .unwrap_or(SOURCES.len())
}

/// The hostname without `www.`, or `""` for anything that is not an http(s) URL.
fn host(url: &str) -> String {
    let parts = urlsplit(url);
    if parts.scheme != "http" && parts.scheme != "https" {
        return String::new();
    }
    match parts.hostname() {
        Some(h) => h.strip_prefix("www.").map(str::to_string).unwrap_or(h),
        None => String::new(),
    }
}

/// `https://host/` for an http(s) URL, or `""`.
///
/// `www.` is dropped and ports are kept, so `www.youtube.com` and `youtube.com` aggregate into one
/// history entry instead of two that mean the same thing.
fn origin(url: &str) -> String {
    let h = host(url);
    if h.is_empty() {
        return String::new();
    }
    let parts = urlsplit(url);
    let port = match parts.port() {
        Some(p) if p != 0 => format!(":{p}"),
        _ => String::new(),
    };
    format!("{}://{h}{port}/", parts.scheme)
}

/// A www- and trailing-slash-insensitive identity, used only to decide that two rows are the same
/// place. The winning row keeps its own URL verbatim.
fn identity(url: &str) -> String {
    let parts = urlsplit(url);
    let path = parts.path.trim_end_matches('/');
    let query = if parts.query.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.query)
    };
    format!("{}{path}{query}", host(url))
}

/// The fallback label: the host with `www.` and a trailing common TLD removed.
///
/// `mail.google.com` -> `mail.google`, which reads better in a criteria line than the full host and
/// is still unambiguous. A host that is only a TLD-looking token is left alone.
fn domain_label(url: &str) -> String {
    let h = host(url);
    if h.is_empty() {
        return String::new();
    }
    let mut parts: Vec<&str> = h.split('.').collect();
    if parts.len() > 1 && COMMON_TLD.contains(parts.last().unwrap()) {
        parts.pop();
    }
    let joined = parts.join(".");
    if joined.is_empty() {
        h
    } else {
        joined
    }
}

/// Python's `round(x, 6)`: correctly rounded through the decimal representation.
fn round6(x: f64) -> f64 {
    format!("{x:.6}").parse().unwrap_or(x)
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// A browser title, reduced to something short, safe to show, and free of personal detail.
///
/// Strips, in order: email addresses, parenthesised counts (`Inbox (9,842)` -> `Inbox`), long digit
/// runs such as order numbers, and every segment after a ` - ` / ` | ` / ` — ` separator, which in
/// a page title is almost always the site's own name. Whitespace collapses and the result is capped
/// at 60 characters on a word boundary. A title that cleans away to nothing returns `""`; callers
/// substitute the domain.
pub fn clean_label(title: &str) -> String {
    let p = patterns();
    let text = p.email.replace_all(title, " ");
    let text = p.count.replace_all(&text, " ");
    let text = p.digit_run.replace_all(&text, " ");
    // Keep the first segment that still has content: the leading one is the page, the rest is chrome.
    let first = p
        .separator
        .split(&text)
        .map(|part| p.noise_edge.replace_all(part, "").into_owned())
        .find(|part| !part.trim().is_empty())
        .unwrap_or_default();
    let collapsed = p.spaces.replace_all(&first, " ");
    let text = p.noise_edge.replace_all(&collapsed, "").trim().to_string();
    if char_len(&text) <= MAX_LABEL {
        return text;
    }
    let cut: String = text.chars().take(MAX_LABEL).collect();
    let chars: Vec<char> = cut.chars().collect();
    let space = chars.iter().rposition(|c| *c == ' ');
    let out: String = match space {
        Some(i) if i > MAX_LABEL / 2 => chars[..i].iter().collect(),
        _ => cut,
    };
    out.trim_end().to_string()
}

/// A lowercase identifier a model can type back verbatim: `slug("Colab")` -> `"colab"`.
pub fn slug(text: &str) -> String {
    let lowered = text.to_lowercase();
    let replaced = patterns().not_slug.replace_all(&lowered, "_");
    let mut value = replaced.trim_matches('_').to_string();
    if value.len() > MAX_SLUG {
        // Only ASCII survives the substitution, so byte and char positions agree.
        value = value[..MAX_SLUG].trim_end_matches('_').to_string();
    }
    if value.is_empty() {
        "site".to_string()
    } else {
        value
    }
}

/// A short alphabetic tag derived from `text`, for the last-resort collision suffix.
///
/// Alphabetic on purpose: a bare number tells the model nothing, and the brief forbids one.
fn letters(text: &str) -> String {
    const MODULUS: u64 = 26 * 26 * 26 * 26;
    let mut total: u64 = 0;
    for ch in text.chars() {
        total = (total * 131 + ch as u64) % MODULUS;
    }
    let mut out = String::new();
    for _ in 0..4 {
        let index = total % 26;
        total /= 26;
        out.push((b'a' + index as u8) as char);
    }
    out
}

/// Key candidates for `site`, best first. Every one is readable; none is a bare number.
fn candidates(site: &Site) -> Vec<String> {
    let base = slug(&site.label);
    let dl = domain_label(&site.url);
    let h = host(&site.url);
    let domain = slug(if dl.is_empty() { &h } else { &dl });
    let path = if h.is_empty() {
        String::new()
    } else {
        urlsplit(&site.url).path.trim_matches('/').to_string()
    };
    let host_slug = slug(&h);
    let mut out = vec![base.clone()];
    if !domain.is_empty() && domain != base {
        out.push(format!("{base}_{domain}"));
        out.push(domain);
    }
    if !host_slug.is_empty() && host_slug != base {
        out.push(format!("{base}_{host_slug}"));
    }
    if !path.is_empty() {
        out.push(slug(&format!("{base}_{path}")));
    }
    out.push(format!("{base}_{}", letters(&site.url)));
    out
}

/// The first unused candidate key, falling back to a hash-derived alphabetic suffix.
fn key_for(site: &Site, taken: &HashSet<String>) -> String {
    for candidate in candidates(site) {
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    let tail = letters(&format!("{}{}", site.url, site.label));
    format!("{}_{tail}", slug(&site.label))
}

/// A `Site` with a settled label, or `None` when the URL is not something we can open.
fn make_site(label: &str, url: &str, weight: f64, source: &str) -> Option<Site> {
    let url = url.trim();
    if host(url).is_empty() {
        return None;
    }
    let text = label.trim();
    let label = if text.is_empty() {
        domain_label(url)
    } else {
        text.to_string()
    };
    Some(Site::new("", &label, url, weight, source))
}

fn read_text_lossy(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

// -------------------------------------------------------------------- bookmarks

/// Flatten a bookmark tree in place. Folders nest arbitrarily; only `url` nodes produce a Site.
fn walk_bookmarks(node: &Value, out: &mut Vec<Site>, titles: bool, depth: usize) {
    let Some(obj) = node.as_object() else { return };
    if depth > 30 {
        return;
    }
    if obj.get("type").and_then(Value::as_str) == Some("url") {
        if let Some(url) = obj.get("url").and_then(Value::as_str) {
            let raw = if titles {
                obj.get("name").and_then(Value::as_str).unwrap_or("")
            } else {
                ""
            };
            if let Some(site) = make_site(&clean_label(raw), url, BOOKMARK_WEIGHT, "bookmark") {
                out.push(site);
            }
        }
        return;
    }
    if let Some(children) = obj.get("children").and_then(Value::as_array) {
        for child in children {
            walk_bookmarks(child, out, titles, depth + 1);
        }
    }
}

/// Every bookmark under every root, flattened, keeping each bookmark's exact URL.
///
/// A bookmark is curated: a deep link into a specific Colab notebook was saved on purpose, so unlike
/// history these are not collapsed to their origin. A missing, unreadable or corrupt file is an
/// empty list.
pub fn read_bookmarks(path: &Path, titles: bool) -> Vec<Site> {
    let Some(text) = read_text_lossy(path) else {
        return vec![];
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return vec![];
    };
    let Some(roots) = data.get("roots").and_then(Value::as_object) else {
        return vec![];
    };
    let mut out = Vec::new();
    for root in roots.values() {
        walk_bookmarks(root, &mut out, titles, 0);
    }
    out
}

// ---------------------------------------------------------------------- history

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Removes the temporary copy however the read ends.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_copy_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winclicker-history-{}-{nanos}-{n}.db",
        std::process::id()
    ))
}

/// `(url, title, visit_count)` from the `urls` table, via a copy of the database.
///
/// Chrome holds an exclusive lock on `History` while it runs, so the live file cannot be opened even
/// read-only. Copying it to a temp file and reading the copy is the one thing that works, and it is
/// also the safest: nothing here can write to the real profile. Only the `urls` table is touched.
fn read_url_rows(path: &Path) -> Vec<(String, String, i64)> {
    if !path.is_file() {
        return vec![];
    }
    let temp = TempFile(temp_copy_path());
    if std::fs::copy(path, &temp.0).is_err() {
        return vec![];
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(connection) = rusqlite::Connection::open_with_flags(&temp.0, flags) else {
        return vec![];
    };
    let rows = (|| -> rusqlite::Result<Vec<(String, String, i64)>> {
        let mut statement = connection.prepare("SELECT url, title, visit_count FROM urls")?;
        let mapped = statement.query_map([], |row| {
            use rusqlite::types::Value as Sql;
            let url: Sql = row.get(0)?;
            let title: Sql = row.get(1)?;
            let visits: Sql = row.get(2)?;
            Ok((url, title, visits))
        })?;
        let mut out = Vec::new();
        for item in mapped {
            let (url, title, visits) = item?;
            let rusqlite::types::Value::Text(url) = url else {
                continue;
            };
            let title = match title {
                rusqlite::types::Value::Text(t) => t,
                _ => String::new(),
            };
            let count = match visits {
                rusqlite::types::Value::Integer(n) => n,
                _ => 0,
            };
            out.push((url, title, count));
        }
        Ok(out)
    })();
    drop(connection);
    rows.unwrap_or_default()
}

/// One representative label for an origin, or `""` when none of its titles is worth using.
///
/// An origin has hundreds of titles and almost all of them describe a page, not the site: a search
/// query, a video name, a document heading. Picking the shortest outright gives nonsense like `gs`
/// for google.com, and picking the most-visited leaks whatever the user happened to read most.
///
/// So the measure of "cleanest" here is recurrence: a cleaned title that shows up on several
/// different rows is the site's own furniture -- `Feed`, `Inbox`, `Claude` -- while a one-off is
/// content. Among recurring titles, one that echoes the domain wins, then the most frequent, then
/// the shortest, then alphabetical order so a rebuild picks the same one again. If nothing recurs,
/// this returns `""` and the caller falls back to the domain, which is the private answer anyway.
fn best_title(titles: &[(String, i64)], domain: &str) -> String {
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut visits_by: HashMap<String, i64> = HashMap::new();
    for (title, visits) in titles {
        let text = clean_label(title);
        if char_len(&text) < 2 {
            continue;
        }
        *counts.entry(text.clone()).or_insert(0) += 1;
        *visits_by.entry(text).or_insert(0) += visits;
    }
    let stem = domain.split('.').next().unwrap_or("").to_lowercase();
    counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(text, count)| {
            let lowered = text.to_lowercase();
            let echoes =
                !(!stem.is_empty() && (lowered.contains(&stem) || stem.contains(&lowered)));
            (
                (
                    echoes,
                    -count,
                    char_len(text),
                    -visits_by[text],
                    text.clone(),
                ),
                text,
            )
        })
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, text)| text.clone())
        .unwrap_or_default()
}

/// `log(1 + count)`; log so that a 1524-visit domain is a few times a 5-visit one, not 300x.
fn log_count(count: i64) -> f64 {
    (count.max(0) as f64).ln_1p()
}

/// Browsing history, aggregated to one entry per origin and ranked by total visits.
///
/// Tens of thousands of URLs collapse to a few hundred domains: visit counts are summed per origin,
/// and one representative label is chosen from that origin's titles. Rows under `min_visits` are
/// dropped before aggregating, so a single accidental click never becomes a catalog entry. Weights
/// are log-normalised into `[0.05, 0.45]`, below `BOOKMARK_WEIGHT`, so a domain with 1524 visits
/// still cannot outrank a deliberately saved bookmark.
///
/// Any failure -- absent file, locked database, not a database at all, missing `urls` table --
/// returns an empty list. Python defaults: `titles=True, min_visits=2, limit=400`.
pub fn read_history(path: &Path, titles: bool, min_visits: i64, limit: usize) -> Vec<Site> {
    if limit == 0 {
        return vec![];
    }
    let mut totals: HashMap<String, i64> = HashMap::new();
    let mut seen_titles: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    for (url, title, visits) in read_url_rows(path) {
        if visits < min_visits {
            continue;
        }
        let o = origin(&url);
        if o.is_empty() {
            continue;
        }
        *totals.entry(o.clone()).or_insert(0) += visits;
        if titles && !title.is_empty() {
            seen_titles.entry(o).or_default().push((title, visits));
        }
    }
    if totals.is_empty() {
        return vec![];
    }
    let mut ranked: Vec<(String, i64)> = totals.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(limit);
    let top = ranked.iter().map(|(_, c)| *c).max().unwrap_or(0);
    let span = HISTORY_CEILING - HISTORY_FLOOR;
    let scale = log_count(top);
    let mut out = Vec::new();
    for (o, count) in ranked {
        let share = if scale != 0.0 {
            log_count(count) / scale
        } else {
            1.0
        };
        let weight = round6(HISTORY_FLOOR + span * share);
        let label = if titles {
            best_title(
                seen_titles.get(&o).map(Vec::as_slice).unwrap_or(&[]),
                &domain_label(&o),
            )
        } else {
            String::new()
        };
        if let Some(site) = make_site(&label, &o, weight, "history") {
            out.push(site);
        }
    }
    out
}

// ---------------------------------------------------------- pinned and learned

/// Python's `str.title()`: a letter after a non-letter is upper-cased, every other letter lowered.
fn title_case(text: &str) -> String {
    let mut out = String::new();
    let mut prev_alpha = false;
    for ch in text.chars() {
        if ch.is_alphabetic() {
            if prev_alpha {
                out.extend(ch.to_lowercase());
            } else {
                out.extend(ch.to_uppercase());
            }
            prev_alpha = true;
        } else {
            out.push(ch);
            prev_alpha = false;
        }
    }
    out
}

/// The curated core from `SITES`, at the top weight so it is always reachable.
pub fn pinned_sites() -> Vec<Site> {
    SITES
        .iter()
        .filter_map(|(key, url)| {
            make_site(
                &title_case(&key.replace('_', " ")),
                url,
                PINNED_WEIGHT,
                "pinned",
            )
            .map(|site| Site::new(key, &site.label, url, PINNED_WEIGHT, "pinned"))
        })
        .collect()
}

/// `%LOCALAPPDATA%/winclicker`, falling back to the home directory when the variable is unset.
pub(crate) fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()));
    let root = match base {
        Some(b) => PathBuf::from(b),
        None => home_dir().join(".cache"),
    };
    root.join("winclicker")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Where sites resolved by the writer are kept, separate from the cache so a rebuild keeps them.
pub fn learned_path() -> PathBuf {
    local_dir().join("learned.json")
}

/// Where the built catalog is cached.
pub fn cache_path() -> PathBuf {
    local_dir().join("catalog.json")
}

/// The JSON list a file holds, or `[]` for missing, unreadable, corrupt or wrong-shaped.
fn read_json_list(path: &Path) -> Vec<Value> {
    read_text_lossy(path)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| match v {
            Value::Array(items) => Some(items),
            _ => None,
        })
        .unwrap_or_default()
}

/// Python's `str(entry.get(name, ""))` for a JSON value.
fn py_str(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(other) => other.to_string(),
    }
}

/// What the writer has resolved before, ranked under pinned but over bookmarks.
pub fn learned_sites() -> Vec<Site> {
    learned_sites_in(&local_dir())
}

/// `learned_sites` against an explicit `winclicker` directory.
pub fn learned_sites_in(dir: &Path) -> Vec<Site> {
    read_json_list(&dir.join("learned.json"))
        .iter()
        .filter_map(|entry| {
            let obj = entry.as_object()?;
            make_site(
                &py_str(obj.get("label")),
                &py_str(obj.get("url")),
                LEARNED_WEIGHT,
                "learned",
            )
        })
        .collect()
}

/// Record a site the writer resolved, so the next rebuild still knows it.
///
/// Written to `learned.json`, never to the cache: the cache is disposable and a refresh would throw
/// this away. Re-remembering the same URL updates its label instead of adding a duplicate row. A
/// write that fails still returns the Site, because the caller is mid-run and should not be stopped
/// by a read-only disk.
pub fn remember_site(label: &str, url: &str) -> Site {
    remember_site_in(&local_dir(), label, url)
}

/// `remember_site` against an explicit `winclicker` directory.
pub fn remember_site_in(dir: &Path, label: &str, url: &str) -> Site {
    let Some(site) = make_site(label, url, LEARNED_WEIGHT, "learned") else {
        return Site::new(&slug(label), label.trim(), url, LEARNED_WEIGHT, "learned");
    };
    let path = dir.join("learned.json");
    let mut entries: Vec<Value> = read_json_list(&path)
        .into_iter()
        .filter(|e| {
            e.as_object()
                .is_some_and(|o| o.get("url").and_then(Value::as_str) != Some(site.url.as_str()))
        })
        .collect();
    entries.push(json!({"label": site.label, "url": site.url}));
    if std::fs::create_dir_all(dir).is_ok() {
        if let Ok(text) = serde_json::to_string_pretty(&entries) {
            let _ = std::fs::write(&path, text);
        }
    }
    Site::new(
        &slug(&site.label),
        &site.label,
        &site.url,
        LEARNED_WEIGHT,
        "learned",
    )
}

// ---------------------------------------------------------------- merge / build

/// One list, deduped by URL and then keyed uniquely, keeping the best source for each place.
///
/// "Best" is the source ranking -- pinned, learned, bookmark, history -- and the higher weight
/// within a source. Keys are assigned last, in final order, so the winner of a collision is the
/// higher-ranked site and the loser gets a domain-based suffix rather than a bare number.
pub fn merge_sites(groups: Vec<Vec<Site>>) -> Vec<Site> {
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, Site> = HashMap::new();
    for site in groups.into_iter().flatten() {
        let id = identity(&site.url);
        match best.get(&id) {
            None => {
                order.push(id.clone());
                best.insert(id, site);
            }
            Some(existing) => {
                let better =
                    (rank(&site.source), -site.weight) < (rank(&existing.source), -existing.weight);
                if better {
                    best.insert(id, site);
                }
            }
        }
    }
    let mut ordered: Vec<Site> = order
        .into_iter()
        .filter_map(|id| best.remove(&id))
        .collect();
    ordered.sort_by(|a, b| {
        rank(&a.source)
            .cmp(&rank(&b.source))
            .then_with(|| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
            .then_with(|| a.url.cmp(&b.url))
    });
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(ordered.len());
    for site in ordered {
        // A pinned key is part of the prompt contract already: keep it verbatim when it is free.
        let key = if site.source == "pinned" && !site.key.is_empty() && !taken.contains(&site.key) {
            site.key.clone()
        } else {
            key_for(&site, &taken)
        };
        taken.insert(key.clone());
        out.push(Site { key, ..site });
    }
    out
}

/// The default Chrome profile directory. It need not exist; every reader tolerates its absence.
pub fn chrome_profile() -> PathBuf {
    let root = match std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        Some(b) => PathBuf::from(b),
        None => home_dir().join("AppData").join("Local"),
    };
    root.join("Google")
        .join("Chrome")
        .join("User Data")
        .join("Default")
}

/// The full catalog, read fresh from disk.
///
/// `root` is the browser profile directory holding `Bookmarks` and `History`; it defaults to the
/// default Chrome profile. With no browser at all this still returns the pinned sites plus whatever
/// the writer has learned, which is the floor the decision loop is entitled to assume.
pub fn build_catalog(titles: bool, root: Option<&Path>) -> Vec<Site> {
    let profile = root.map(Path::to_path_buf).unwrap_or_else(chrome_profile);
    build_catalog_with(titles, &profile, &local_dir())
}

/// `build_catalog` with an explicit profile and `winclicker` directory (for the learned store).
pub fn build_catalog_with(titles: bool, profile: &Path, local: &Path) -> Vec<Site> {
    merge_sites(vec![
        pinned_sites(),
        learned_sites_in(local),
        read_bookmarks(&profile.join("Bookmarks"), titles),
        read_history(&profile.join("History"), titles, 2, 400),
    ])
}

// ---------------------------------------------------------------------- caching

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One cached row back into a `Site`, or `None` when the row is not usable.
fn to_site(entry: &Value) -> Option<Site> {
    let obj = entry.as_object()?;
    let text = |name: &str| -> Option<String> {
        obj.get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let (key, label, url, source) = (text("key")?, text("label")?, text("url")?, text("source")?);
    let weight = obj.get("weight").and_then(Value::as_f64)?;
    Some(Site {
        key,
        label,
        url,
        weight,
        source,
    })
}

/// The cached catalog if it is fresh, matches `titles` and parses; otherwise `None`.
///
/// A corrupt cache is indistinguishable here from an absent one, on purpose: the caller rebuilds.
fn read_cache(path: &Path, titles: bool) -> Option<Vec<Site>> {
    let data: Value = serde_json::from_str(&read_text_lossy(path)?).ok()?;
    let obj = data.as_object()?;
    if obj.get("titles").and_then(Value::as_bool) != Some(titles) {
        return None;
    }
    let built = obj.get("built").and_then(Value::as_f64)?;
    if now_seconds() - built > CACHE_SECONDS {
        return None;
    }
    let sites: Vec<Site> = obj
        .get("sites")?
        .as_array()?
        .iter()
        .filter_map(to_site)
        .collect();
    if sites.is_empty() {
        None
    } else {
        Some(sites)
    }
}

/// Best-effort cache write. A read-only or missing directory is not worth an exception.
fn write_cache(path: &Path, sites: &[Site], titles: bool) {
    let payload = json!({"built": now_seconds(), "titles": titles, "sites": sites});
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(text) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(path, text);
    }
}

/// The catalog, from cache when it is fresh and otherwise rebuilt and re-cached.
///
/// Rebuilds when `refresh=true`, when the cache is absent or corrupt, when it was built for the
/// other `titles` setting, or when it is more than 24 hours old. Building reads two files and a
/// sqlite copy, which is too slow to do on every step of a run.
pub fn load_catalog(refresh: bool, titles: bool) -> Vec<Site> {
    let local = local_dir();
    load_catalog_with(
        refresh,
        titles,
        &chrome_profile(),
        &local,
        &local.join("catalog.json"),
    )
}

/// `load_catalog` with an explicit profile, `winclicker` directory and cache file.
pub fn load_catalog_with(
    refresh: bool,
    titles: bool,
    profile: &Path,
    local: &Path,
    cache: &Path,
) -> Vec<Site> {
    if !refresh {
        if let Some(cached) = read_cache(cache, titles) {
            return cached;
        }
    }
    let sites = build_catalog_with(titles, profile, local);
    write_cache(cache, &sites, titles);
    sites
}

// ------------------------------------------------------------------------ tests

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh, empty directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "wcore-test-{tag}-{}-{nanos}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn url_node(name: &str, url: &str) -> Value {
        json!({"type": "url", "name": name, "url": url})
    }

    fn folder(name: &str, children: Vec<Value>) -> Value {
        json!({"type": "folder", "name": name, "children": children})
    }

    fn write_bookmarks(path: &Path, bar: Vec<Value>, other: Vec<Value>) -> PathBuf {
        let data = json!({
            "roots": {
                "bookmark_bar": {"type": "folder", "name": "Bookmarks bar", "children": bar},
                "other": {"type": "folder", "name": "Other bookmarks", "children": other},
                // Chrome puts non-folder junk in roots; it must not crash us
                "sync_transaction_version": "1",
            },
            "version": 1,
        });
        std::fs::write(path, data.to_string()).unwrap();
        path.to_path_buf()
    }

    fn write_history(path: &Path, rows: &[(&str, &str, i64)]) -> PathBuf {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT, visit_count INTEGER)",
            [],
        )
        .unwrap();
        for (u, t, v) in rows {
            c.execute(
                "INSERT INTO urls (url, title, visit_count) VALUES (?1, ?2, ?3)",
                rusqlite::params![u, t, v],
            )
            .unwrap();
        }
        path.to_path_buf()
    }

    fn history(path: &Path) -> Vec<Site> {
        read_history(path, true, 2, 400)
    }

    fn pinned_keys() -> HashSet<String> {
        SITES.iter().map(|(k, _)| k.to_string()).collect()
    }

    fn sites_url(key: &str) -> &'static str {
        SITES.iter().find(|(k, _)| *k == key).unwrap().1
    }

    /// A profile dir plus a `winclicker` local dir, both temporary.
    struct Env {
        tmp: TempDir,
    }

    impl Env {
        fn new(tag: &str) -> Self {
            let tmp = TempDir::new(tag);
            std::fs::create_dir_all(tmp.path().join("local")).unwrap();
            Env { tmp }
        }
        fn root(&self) -> &Path {
            self.tmp.path()
        }
        fn local(&self) -> PathBuf {
            self.tmp.path().join("local").join("winclicker")
        }
        fn cache(&self) -> PathBuf {
            self.local().join("catalog.json")
        }
        fn build(&self, titles: bool, profile: &Path) -> Vec<Site> {
            build_catalog_with(titles, profile, &self.local())
        }
        fn load(&self, refresh: bool, titles: bool) -> Vec<Site> {
            load_catalog_with(refresh, titles, self.root(), &self.local(), &self.cache())
        }
    }

    // ---------------------------------------------------------- clean_label

    #[test]
    fn clean_label_ugly_titles() {
        let cases = [
            ("Inbox (9,842) - vatsajoshi2@gmail.com - Gmail", "Inbox"),
            ("Jiya Lage Na | YouTube Music", "Jiya Lage Na"),
            ("(3) WhatsApp", "WhatsApp"),
            ("Order #112-4839201-9930 - Amazon.in", "Order #"),
            ("  spaced   out    title  ", "spaced out title"),
            ("GitHub", "GitHub"),
            ("vatsajoshi2@gmail.com", ""),
            ("(12)", ""),
            ("Colab \u{2014} Google Research", "Colab"),
            ("Docs \u{00b7} Notion", "Docs"),
            ("", ""),
        ];
        for (raw, expected) in cases {
            assert_eq!(clean_label(raw), expected, "for {raw:?}");
        }
    }

    #[test]
    fn clean_label_strips_email_even_without_a_separator() {
        assert!(!clean_label("Mail for vatsajoshi2@gmail.com today").contains('@'));
    }

    #[test]
    fn clean_label_caps_at_sixty_characters_on_a_word_boundary() {
        let long = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
        let out = clean_label(long);
        assert!(out.chars().count() <= 60);
        assert!(!out.ends_with(' '));
        assert!(long.starts_with(&out));
    }

    #[test]
    fn slug_is_readable() {
        assert_eq!(slug("Colab"), "colab");
        assert_eq!(slug("Google Calendar"), "google_calendar");
        assert_eq!(slug("!!!"), "site");
        assert_eq!(slug("colab.research.google"), "colab_research_google");
    }

    // ------------------------------------------------------------ bookmarks

    #[test]
    fn nested_bookmark_folders_are_flattened() {
        let t = TempDir::new("bm");
        let path = write_bookmarks(
            &t.path().join("Bookmarks"),
            vec![
                url_node("Hacker News", "https://news.ycombinator.com/"),
                folder(
                    "Work",
                    vec![
                        url_node("Linear", "https://linear.app/team"),
                        folder(
                            "Deep",
                            vec![url_node(
                                "Colab",
                                "https://colab.research.google.com/drive/abc123",
                            )],
                        ),
                    ],
                ),
            ],
            vec![url_node("Arxiv", "https://arxiv.org/")],
        );
        let sites = read_bookmarks(&path, true);
        let labels: HashSet<&str> = sites.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            HashSet::from(["Hacker News", "Linear", "Colab", "Arxiv"])
        );
        assert!(sites.iter().all(|s| s.source == "bookmark"));
    }

    #[test]
    fn bookmarks_keep_their_exact_deep_link() {
        let t = TempDir::new("bm");
        let deep = "https://colab.research.google.com/drive/abc123";
        let path = write_bookmarks(
            &t.path().join("Bookmarks"),
            vec![url_node("Colab", deep)],
            vec![],
        );
        assert_eq!(read_bookmarks(&path, true)[0].url, deep);
    }

    #[test]
    fn bookmark_without_a_usable_url_is_dropped() {
        let t = TempDir::new("bm");
        let path = write_bookmarks(
            &t.path().join("Bookmarks"),
            vec![
                url_node("JS", "javascript:void(0)"),
                url_node("File", "file:///C:/x.html"),
                url_node("Ok", "https://ok.com/"),
            ],
            vec![],
        );
        let urls: Vec<String> = read_bookmarks(&path, true)
            .into_iter()
            .map(|s| s.url)
            .collect();
        assert_eq!(urls, vec!["https://ok.com/"]);
    }

    #[test]
    fn bookmark_without_a_name_falls_back_to_the_domain() {
        let t = TempDir::new("bm");
        let path = write_bookmarks(
            &t.path().join("Bookmarks"),
            vec![url_node("", "https://www.example.com/x")],
            vec![],
        );
        assert_eq!(read_bookmarks(&path, true)[0].label, "example");
    }

    #[test]
    fn missing_bookmarks_file_is_empty() {
        let t = TempDir::new("bm");
        assert!(read_bookmarks(&t.path().join("nope").join("Bookmarks"), true).is_empty());
    }

    #[test]
    fn corrupt_bookmarks_file_is_empty() {
        let t = TempDir::new("bm");
        let path = t.path().join("Bookmarks");
        std::fs::write(&path, r#"{"roots": {"bookmark_bar": {"type": "fold"#).unwrap();
        assert!(read_bookmarks(&path, true).is_empty());
    }

    #[test]
    fn bookmarks_with_wrong_shape_is_empty() {
        let t = TempDir::new("bm");
        let path = t.path().join("Bookmarks");
        std::fs::write(&path, r#"["not", "an", "object"]"#).unwrap();
        assert!(read_bookmarks(&path, true).is_empty());
    }

    // -------------------------------------------------------------- history

    #[test]
    fn history_collapses_an_origin_into_one_weighted_entry() {
        let t = TempDir::new("hist");
        let path = write_history(
            &t.path().join("History"),
            &[
                (
                    "https://www.youtube.com/watch?v=1",
                    "Jiya Lage Na | YouTube Music",
                    10,
                ),
                (
                    "https://www.youtube.com/watch?v=2",
                    "Some Very Long Video Title Here - YouTube",
                    20,
                ),
                ("https://www.youtube.com/feed/subscriptions", "YouTube", 5),
                ("https://youtube.com/feed/history", "YouTube", 3),
                ("https://github.com/one", "one - GitHub", 4),
            ],
        );
        let sites = history(&path);
        let youtube: Vec<&Site> = sites.iter().filter(|s| s.url.contains("youtube")).collect();
        // four rows, one entry: www and bare host are the same origin
        assert_eq!(youtube.len(), 1);
        assert_eq!(youtube[0].url, "https://youtube.com/");
        // the recurring title, not a one-off video name
        assert_eq!(youtube[0].label, "YouTube");
        let github = sites.iter().find(|s| s.url.contains("github")).unwrap();
        assert!(youtube[0].weight > github.weight);
    }

    #[test]
    fn history_prefers_a_recurring_title_over_a_one_off_page() {
        let t = TempDir::new("hist");
        let path = write_history(
            &t.path().join("History"),
            &[
                ("https://site.example.com/a", "Feed", 30),
                ("https://site.example.com/b", "Feed", 20),
                // most visited, but a one-off fragment
                ("https://site.example.com/c", "gs", 90),
            ],
        );
        assert_eq!(history(&path)[0].label, "Feed");
    }

    /// A page of unique titles is a page of search queries: the domain is the private answer.
    #[test]
    fn history_falls_back_to_the_domain_when_no_title_recurs() {
        let t = TempDir::new("hist");
        let rows: Vec<(String, String, i64)> = (0..6)
            .map(|i| {
                (
                    format!("https://google.com/search?q={i}"),
                    format!("how do i {i} - Google Search"),
                    5,
                )
            })
            .collect();
        let refs: Vec<(&str, &str, i64)> = rows
            .iter()
            .map(|(u, t, v)| (u.as_str(), t.as_str(), *v))
            .collect();
        let path = write_history(&t.path().join("History"), &refs);
        let site = &history(&path)[0];
        assert_eq!(site.label, "google");
        assert!(!site.label.contains("how do i"));
    }

    #[test]
    fn history_min_visits_filters_rows() {
        let t = TempDir::new("hist");
        let path = write_history(
            &t.path().join("History"),
            &[
                ("https://rare.com/a", "Rare", 1),
                ("https://common.com/a", "Common", 9),
            ],
        );
        let urls: Vec<String> = read_history(&path, true, 2, 400)
            .into_iter()
            .map(|s| s.url)
            .collect();
        assert_eq!(urls, vec!["https://common.com/"]);
        assert_eq!(read_history(&path, true, 1, 400).len(), 2);
    }

    #[test]
    fn history_limit_keeps_the_most_visited() {
        let t = TempDir::new("hist");
        let rows: Vec<(String, String, i64)> = (0..10)
            .map(|i| (format!("https://site{i}.com/"), format!("Site {i}"), i + 2))
            .collect();
        let refs: Vec<(&str, &str, i64)> = rows
            .iter()
            .map(|(u, t, v)| (u.as_str(), t.as_str(), *v))
            .collect();
        let path = write_history(&t.path().join("History"), &refs);
        let urls: Vec<String> = read_history(&path, true, 2, 3)
            .into_iter()
            .map(|s| s.url)
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://site9.com/",
                "https://site8.com/",
                "https://site7.com/"
            ]
        );
    }

    #[test]
    fn history_weights_stay_under_a_bookmark() {
        let t = TempDir::new("hist");
        let path = write_history(
            &t.path().join("History"),
            &[
                ("https://huge.com/a", "Huge", 1524),
                ("https://small.com/a", "Small", 2),
            ],
        );
        for site in history(&path) {
            assert!(HISTORY_FLOOR <= site.weight && site.weight <= HISTORY_CEILING);
            assert!(site.weight < BOOKMARK_WEIGHT);
        }
    }

    #[test]
    fn history_origin_with_no_clean_title_falls_back_to_the_domain() {
        let t = TempDir::new("hist");
        let path = write_history(
            &t.path().join("History"),
            &[("https://mail.google.com/u/0", "vatsajoshi2@gmail.com", 6)],
        );
        assert_eq!(history(&path)[0].label, "mail.google");
    }

    #[test]
    fn missing_history_file_is_empty() {
        let t = TempDir::new("hist");
        assert!(history(&t.path().join("History")).is_empty());
    }

    #[test]
    fn history_that_is_not_a_database_is_empty() {
        let t = TempDir::new("hist");
        let path = t.path().join("History");
        std::fs::write(
            &path,
            b"this is not sqlite at all, it is a locked or truncated file",
        )
        .unwrap();
        assert!(history(&path).is_empty());
    }

    #[test]
    fn history_without_a_urls_table_is_empty() {
        let t = TempDir::new("hist");
        let path = t.path().join("History");
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute("CREATE TABLE visits (id INTEGER)", []).unwrap();
        drop(c);
        assert!(history(&path).is_empty());
    }

    /// The DB is copied before reading, which is what makes a locked profile survivable: a writer
    /// holding an exclusive lock on the live file (as Chrome does) does not stop the read, and the
    /// live file is left byte-for-byte as it was.
    #[test]
    fn history_read_goes_through_a_copy_and_survives_a_locked_live_file() {
        let t = TempDir::new("hist");
        let path = write_history(&t.path().join("History"), &[("https://a.com/", "A", 5)]);
        let before = std::fs::read(&path).unwrap();
        let holder = rusqlite::Connection::open(&path).unwrap();
        holder
            .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(!history(&path).is_empty());
        holder.execute_batch("ROLLBACK;").unwrap();
        drop(holder);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    // ---------------------------------------------------- the privacy switch

    #[test]
    fn titles_false_leaks_no_title_text() {
        let env = Env::new("priv");
        let secret = "Inbox (9,842) - vatsajoshi2@gmail.com - Gmail";
        write_bookmarks(
            &env.root().join("Bookmarks"),
            vec![url_node(secret, "https://mail.google.com/u/0/inbox")],
            vec![],
        );
        write_history(
            &env.root().join("History"),
            &[(
                "https://bank.example.org/acct",
                "Statement for Vatsa Joshi",
                40,
            )],
        );
        let sites = env.build(false, env.root());
        let blob = serde_json::to_string(
            &sites
                .iter()
                .map(|s| json!({"key": s.key, "label": s.label}))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        for leak in [
            "Inbox",
            "vatsajoshi2",
            "gmail.com",
            "Statement",
            "Vatsa",
            "Joshi",
            "9,842",
        ] {
            assert!(!blob.contains(leak), "{leak} leaked");
        }
        let labels: HashSet<&str> = sites
            .iter()
            .filter(|s| s.source == "bookmark" || s.source == "history")
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(labels, HashSet::from(["mail.google", "bank.example"]));
    }

    #[test]
    fn titles_true_keeps_a_cleaned_title() {
        let env = Env::new("priv");
        write_bookmarks(
            &env.root().join("Bookmarks"),
            vec![url_node(
                "Jiya Lage Na | YouTube Music",
                "https://music.youtube.com/w",
            )],
            vec![],
        );
        assert!(env
            .build(true, env.root())
            .iter()
            .any(|s| s.label == "Jiya Lage Na"));
    }

    // ------------------------------------------------------------------ keys

    #[test]
    fn keys_are_unique_and_collisions_get_a_domain_suffix() {
        let sites = merge_sites(vec![vec![
            Site::new(
                "",
                "Colab",
                "https://colab.research.google.com/",
                BOOKMARK_WEIGHT,
                "bookmark",
            ),
            Site::new(
                "",
                "Colab",
                "https://colab.example.org/",
                BOOKMARK_WEIGHT,
                "bookmark",
            ),
            Site::new(
                "",
                "Colab",
                "https://third.colab.net/",
                BOOKMARK_WEIGHT,
                "bookmark",
            ),
        ]]);
        let keys: Vec<&str> = sites.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys.iter().collect::<HashSet<_>>().len(), 3);
        assert!(keys.contains(&"colab"));
        assert!(keys.iter().all(|k| !k
            .rsplit('_')
            .next()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_digit())));
        assert!(keys.contains(&"colab_colab_research_google"));
    }

    #[test]
    fn keys_never_collapse_to_the_empty_string() {
        let sites = merge_sites(vec![vec![Site::new(
            "",
            "!!!",
            "https://a.example.com/",
            0.6,
            "bookmark",
        )]]);
        assert!(!sites[0].key.is_empty());
    }

    // -------------------------------------------------------- pinned / merge

    #[test]
    fn pinned_sites_cover_config_and_outrank_everything() {
        let sites = pinned_sites();
        assert_eq!(
            sites.iter().map(|s| s.key.clone()).collect::<HashSet<_>>(),
            pinned_keys()
        );
        assert!(sites.iter().all(|s| s.weight == PINNED_WEIGHT));
        assert!(sites.iter().all(|s| s.weight > BOOKMARK_WEIGHT));
        let calendar = sites.iter().find(|s| s.key == "google_calendar").unwrap();
        assert_eq!(calendar.label, "Google Calendar");
    }

    #[test]
    fn pinned_sites_are_always_present_even_with_no_browser() {
        let env = Env::new("pin");
        let sites = env.build(true, &env.root().join("no-such-profile"));
        assert_eq!(
            sites.iter().map(|s| s.key.clone()).collect::<HashSet<_>>(),
            pinned_keys()
        );
    }

    #[test]
    fn pinned_keeps_its_config_key_when_a_bookmark_shares_the_label() {
        let env = Env::new("pin");
        write_bookmarks(
            &env.root().join("Bookmarks"),
            vec![url_node("Github", "https://github.com/explore")],
            vec![],
        );
        let sites = env.build(true, env.root());
        let github = sites.iter().find(|s| s.key == "github").unwrap();
        assert_eq!(github.url, sites_url("github"));
        assert_eq!(github.source, "pinned");
        let other: Vec<&Site> = sites
            .iter()
            .filter(|s| s.url == "https://github.com/explore")
            .collect();
        assert!(!other.is_empty() && other[0].key != "github");
    }

    #[test]
    fn merge_precedence_prefers_the_better_source_for_the_same_url() {
        let url = "https://notion.so/";
        let merged = merge_sites(vec![
            vec![Site::new("", "Notion hist", url, 0.4, "history")],
            vec![Site::new("", "Notion bm", url, BOOKMARK_WEIGHT, "bookmark")],
            vec![Site::new(
                "",
                "Notion learned",
                url,
                LEARNED_WEIGHT,
                "learned",
            )],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, "learned");
    }

    #[test]
    fn merge_is_ordered_best_first() {
        let merged = merge_sites(vec![
            vec![Site::new("", "H", "https://h.com/", 0.2, "history")],
            vec![Site::new("", "P", "https://p.com/", 1.0, "pinned")],
            vec![Site::new("", "B", "https://b.com/", 0.6, "bookmark")],
        ]);
        let sources: Vec<&str> = merged.iter().map(|s| s.source.as_str()).collect();
        assert_eq!(sources, vec!["pinned", "bookmark", "history"]);
    }

    #[test]
    fn pinned_url_seen_in_history_stays_pinned() {
        let env = Env::new("pin");
        write_history(
            &env.root().join("History"),
            &[(sites_url("gmail"), "Inbox (9,842) - Gmail", 900)],
        );
        let sites = env.build(true, env.root());
        let gmail: Vec<&Site> = sites
            .iter()
            .filter(|s| s.url == sites_url("gmail"))
            .collect();
        assert_eq!(gmail.len(), 1);
        assert_eq!(gmail[0].source, "pinned");
        assert_eq!(gmail[0].weight, PINNED_WEIGHT);
    }

    // ---------------------------------------------------- learned + caching

    #[test]
    fn remember_site_survives_a_rebuild() {
        let env = Env::new("learn");
        let site = remember_site_in(
            &env.local(),
            "My Dashboard",
            "https://dash.internal.example.com/home",
        );
        assert_eq!(site.source, "learned");
        assert_eq!(site.key, "my_dashboard");
        let rebuilt = env.build(true, &env.root().join("no-profile"));
        assert!(rebuilt
            .iter()
            .any(|s| s.url == "https://dash.internal.example.com/home" && s.source == "learned"));
    }

    #[test]
    fn remember_site_does_not_duplicate_the_same_url() {
        let env = Env::new("learn");
        remember_site_in(&env.local(), "Old", "https://dash.example.com/");
        remember_site_in(&env.local(), "New", "https://dash.example.com/");
        let text = std::fs::read_to_string(env.local().join("learned.json")).unwrap();
        let learned: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            learned,
            json!([{"label": "New", "url": "https://dash.example.com/"}])
        );
    }

    #[test]
    fn remember_site_writes_utf8() {
        let env = Env::new("learn");
        remember_site_in(
            &env.local(),
            "Caf\u{e9} \u{2014} \u{65e5}\u{672c}",
            "https://cafe.example.com/",
        );
        let text = std::fs::read_to_string(env.local().join("learned.json")).unwrap();
        assert!(text.contains("cafe.example.com"));
    }

    #[test]
    fn learned_outranks_a_bookmark_for_the_same_place() {
        let env = Env::new("learn");
        remember_site_in(&env.local(), "Dash", "https://dash.example.com/");
        write_bookmarks(
            &env.root().join("Bookmarks"),
            vec![url_node("Dash bookmark", "https://dash.example.com/")],
            vec![],
        );
        let sites = env.build(true, env.root());
        let found: Vec<&Site> = sites
            .iter()
            .filter(|s| s.url == "https://dash.example.com/")
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, "learned");
        assert!(found[0].weight > BOOKMARK_WEIGHT);
    }

    #[test]
    fn corrupt_learned_store_degrades_to_empty() {
        let env = Env::new("learn");
        std::fs::create_dir_all(env.local()).unwrap();
        std::fs::write(env.local().join("learned.json"), "{not json").unwrap();
        assert!(learned_sites_in(&env.local()).is_empty());
        // pinned still there
        assert!(!env.build(true, &env.root().join("none")).is_empty());
    }

    #[test]
    fn cache_round_trip() {
        let env = Env::new("cache");
        let first = env.load(true, true);
        assert!(env.cache().is_file());
        // A bookmark added after the build is not seen until a refresh: the second load came from
        // the cache, not from a rebuild.
        write_bookmarks(
            &env.root().join("Bookmarks"),
            vec![url_node("Later", "https://later.example.com/")],
            vec![],
        );
        let again = env.load(false, true);
        let shape = |v: &[Site]| -> Vec<(String, String, f64, String)> {
            v.iter()
                .map(|s| (s.key.clone(), s.url.clone(), s.weight, s.source.clone()))
                .collect()
        };
        assert_eq!(shape(&again), shape(&first));
    }

    #[test]
    fn cache_is_created_under_the_local_dir() {
        let env = Env::new("cache");
        env.load(true, true);
        assert!(env.local().join("catalog.json").is_file());
    }

    #[test]
    fn corrupt_cache_rebuilds_instead_of_raising() {
        let env = Env::new("cache");
        std::fs::create_dir_all(env.local()).unwrap();
        std::fs::write(env.cache(), "{ this is not json").unwrap();
        let keys: HashSet<String> = env.load(false, true).into_iter().map(|s| s.key).collect();
        assert!(keys.is_superset(&pinned_keys()));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(env.cache()).unwrap()).unwrap();
        assert!(!written["sites"].as_array().unwrap().is_empty());
    }

    #[test]
    fn stale_cache_rebuilds() {
        let env = Env::new("cache");
        env.load(true, true);
        let mut payload: Value =
            serde_json::from_str(&std::fs::read_to_string(env.cache()).unwrap()).unwrap();
        payload["built"] = json!(now_seconds() - CACHE_SECONDS - 60.0);
        payload["sites"] = json!([{"key": "stale", "label": "Stale", "url": "https://stale.com/", "weight": 1.0, "source": "pinned"}]);
        std::fs::write(env.cache(), payload.to_string()).unwrap();
        let keys: Vec<String> = env.load(false, true).into_iter().map(|s| s.key).collect();
        assert_ne!(keys, vec!["stale".to_string()]);
    }

    #[test]
    fn cache_does_not_serve_the_other_titles_setting() {
        let env = Env::new("cache");
        write_bookmarks(
            &env.root().join("Bookmarks"),
            vec![url_node("Secret Title", "https://titled.example.com/")],
            vec![],
        );
        assert!(env
            .load(true, true)
            .iter()
            .any(|s| s.label == "Secret Title"));
        let other = env.load(false, false);
        assert!(other.iter().all(|s| s.label != "Secret Title"));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(env.cache()).unwrap()).unwrap();
        assert_eq!(written["titles"], json!(false));
    }

    #[test]
    fn load_catalog_tolerates_an_unwritable_cache_dir() {
        let env = Env::new("cache");
        let blocker = env.root().join("file.txt");
        std::fs::write(&blocker, "blocking").unwrap();
        let sites = load_catalog_with(
            true,
            true,
            env.root(),
            &env.local(),
            &blocker.join("catalog.json"),
        );
        assert!(!sites.is_empty());
    }

    #[test]
    fn urlsplit_matches_python_on_the_shapes_used() {
        let p = urlsplit("https://User@WWW.Example.com:8080/a/b?x=1#frag");
        assert_eq!(p.scheme, "https");
        assert_eq!(p.netloc, "User@WWW.Example.com:8080");
        assert_eq!(p.path, "/a/b");
        assert_eq!(p.query, "x=1");
        assert_eq!(p.hostname().as_deref(), Some("www.example.com"));
        assert_eq!(p.port(), Some(8080));
        assert_eq!(
            origin("https://www.example.com:8080/a"),
            "https://example.com:8080/"
        );
        assert_eq!(host("javascript:void(0)"), "");
        assert_eq!(host("file:///C:/x.html"), "");
    }
}
