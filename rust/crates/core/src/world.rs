//! The machine as Pointer knows it: installed apps, open windows per monitor, browser windows with
//! their tab titles and active address, the focused app, the mouse, what changed since the last
//! turn, and what Pointer itself did.
//!
//! This is state, not a watcher. Nothing here looks at the machine on its own: the assistant
//! refreshes it at the start of a turn and after tool calls, and a fact older than [`FRESH_SECS`]
//! is refreshed before it is used. Every fact carries when it was observed.
//!
//! Privacy: window titles and tab titles live in memory only. What reaches disk is Pointer's own
//! action log, in words that never contain a title, and the app catalog (which `apps.rs` caches).
//! A page's contents, cookies and passwords are never read at all: only the tab strip's titles
//! and the address bar's text, with the query and fragment cut off.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use platform::display::Monitor;
use serde_json::{json, Value};

use crate::perception::{url_from_fields, Desktop};
use crate::worldmodel;

/// A fact older than this is refreshed before it is used.
pub const FRESH_SECS: f64 = 4.0;
pub const KEEP_CHANGES: usize = 16;
pub const KEEP_ACTIONS: usize = 40;
const ACTIONS_FILE: &str = "world-actions.json";
const SUMMARY_WINDOWS: usize = 14;
const SUMMARY_CHARS: usize = 60;

/// Browsers whose tab strip is read: process names, lowercase.
pub const BROWSER_PROCESSES: [&str; 2] = ["chrome", "msedge"];

#[derive(Debug, Clone, PartialEq)]
pub struct WinRec {
    pub hwnd: isize,
    pub app: String,
    pub title: String,
    pub monitor: usize,
    pub minimized: bool,
    pub foreground: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabRec {
    pub title: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrowserRec {
    pub hwnd: isize,
    pub app: String,
    pub monitor: usize,
    pub tabs: Vec<TabRec>,
    /// The active tab's address, query and fragment removed.
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MouseRec {
    pub x: f64,
    pub y: f64,
    pub monitor: usize,
}

/// One look at the machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub windows: Vec<WinRec>,
    pub browsers: Vec<BrowserRec>,
    /// (index, x, y, w, h, primary) per display.
    pub monitors: Vec<(usize, i32, i32, i32, i32, bool)>,
    pub mouse: Option<MouseRec>,
}

impl Snapshot {
    pub fn focused(&self) -> Option<&WinRec> {
        self.windows.iter().find(|w| w.foreground)
    }
}

/// An installed app Pointer can launch.
#[derive(Debug, Clone, PartialEq)]
pub struct CatApp {
    pub key: String,
    pub label: String,
    /// What the shell opens: a shortcut path, or `shell:AppsFolder\<id>` for a Store app.
    pub launch: String,
}

/// An address without its query or fragment: those are where tokens live.
pub fn redact_url(url: &str) -> String {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    url[..cut].to_string()
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// Look at the machine through `desktop`. `scan` reads a browser window's tab strip.
pub fn gather(
    desktop: &dyn Desktop,
    scan: &dyn Fn(isize) -> platform::uia::BrowserScan,
) -> Snapshot {
    let monitors: Vec<Monitor> = desktop.monitors();
    let ranked = worldmodel::rank(&desktop.open_windows(), 80);
    let windows: Vec<WinRec> = ranked
        .iter()
        .map(|w| WinRec {
            hwnd: w.hwnd,
            app: w.app.clone(),
            title: w.title.clone(),
            monitor: w.monitor,
            minimized: w.minimized,
            foreground: w.foreground,
        })
        .collect();
    let mut browsers = Vec::new();
    for w in &ranked {
        let app = w.app.to_lowercase();
        if !BROWSER_PROCESSES.contains(&app.as_str()) {
            continue;
        }
        let (tabs, url) = if w.minimized {
            (Vec::new(), None)
        } else {
            let s = scan(w.hwnd);
            let url = url_from_fields(&s.fields).map(|u| redact_url(&u));
            (
                s.tabs
                    .into_iter()
                    .map(|t| TabRec {
                        title: t.title,
                        active: t.selected,
                    })
                    .collect(),
                url,
            )
        };
        browsers.push(BrowserRec {
            hwnd: w.hwnd,
            app: w.app.clone(),
            monitor: w.monitor,
            tabs,
            url,
        });
    }
    let mouse = desktop.pointer().map(|(x, y, _)| MouseRec {
        x,
        y,
        monitor: platform::display::index_at(&monitors, x.round() as i32, y.round() as i32),
    });
    Snapshot {
        windows,
        browsers,
        monitors: monitors
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let b = m.bounds();
                (i, b.0, b.1, m.width(), m.height(), m.primary)
            })
            .collect(),
        mouse,
    }
}

#[derive(Debug, Clone)]
struct Change {
    seq: u64,
    at: Instant,
    text: String,
}

#[derive(Debug, Clone)]
struct ActionRec {
    at: Instant,
    unix: u64,
    /// Full words, in memory.
    text: String,
    /// What is written to disk: never a window or tab title.
    saved: String,
}

#[derive(Default, Debug)]
pub struct World {
    snap: Option<Snapshot>,
    observed: Option<Instant>,
    observed_wall: Option<chrono::DateTime<chrono::Local>>,
    catalog: Vec<CatApp>,
    catalog_loaded: bool,
    seq: u64,
    changes: VecDeque<Change>,
    /// `seq` as of the previous turn start, and as of this one.
    mark: u64,
    since: u64,
    actions: VecDeque<ActionRec>,
    dir: Option<PathBuf>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the action log lives; loads what an earlier session wrote. Idempotent.
    pub fn set_dir(&mut self, dir: &Path) {
        if self.dir.as_deref() == Some(dir) {
            return;
        }
        self.dir = Some(dir.to_path_buf());
        let Ok(text) = std::fs::read_to_string(dir.join(ACTIONS_FILE)) else {
            return;
        };
        let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        for r in rows.iter().rev().take(KEEP_ACTIONS).rev() {
            let (Some(t), Some(u)) = (r["text"].as_str(), r["at"].as_u64()) else {
                continue;
            };
            self.actions.push_back(ActionRec {
                at: Instant::now(),
                unix: u,
                text: t.to_string(),
                saved: t.to_string(),
            });
        }
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        self.observed
            .is_some_and(|t| now.saturating_duration_since(t).as_secs_f64() < FRESH_SECS)
    }

    pub fn has_snapshot(&self) -> bool {
        self.snap.is_some()
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snap.as_ref()
    }

    /// Take a new look. Windows opened, closed and the focus moving become changes, against the
    /// previous look; the very first look has nothing to compare with.
    pub fn update(&mut self, snap: Snapshot, now: Instant) {
        if let Some(old) = &self.snap {
            let mut found = Vec::new();
            for w in &snap.windows {
                if !old.windows.iter().any(|o| o.hwnd == w.hwnd) {
                    found.push(format!("opened {}", describe(w)));
                }
            }
            for o in &old.windows {
                if !snap.windows.iter().any(|w| w.hwnd == o.hwnd) {
                    found.push(format!("closed {}", describe(o)));
                }
            }
            let (was, is) = (old.focused().map(|w| w.hwnd), snap.focused().map(|w| w.hwnd));
            if was != is {
                if let Some(f) = snap.focused() {
                    found.push(format!("focus moved to {}", describe(f)));
                }
            }
            for text in found {
                self.seq += 1;
                self.changes.push_back(Change {
                    seq: self.seq,
                    at: now,
                    text,
                });
            }
            while self.changes.len() > KEEP_CHANGES {
                self.changes.pop_front();
            }
        }
        self.snap = Some(snap);
        self.observed = Some(now);
        self.observed_wall = Some(chrono::Local::now());
    }

    /// Mark the start of a turn: the changes reported are those since the previous turn started.
    /// Call it right after the turn's first refresh.
    pub fn begin_turn(&mut self) {
        self.since = self.mark;
        self.mark = self.seq;
    }

    pub fn set_catalog(&mut self, apps: Vec<CatApp>) {
        self.catalog = apps;
        self.catalog_loaded = true;
    }

    pub fn catalog_loaded(&self) -> bool {
        self.catalog_loaded
    }

    pub fn catalog(&self) -> &[CatApp] {
        &self.catalog
    }

    /// The app a key (or, failing that, a name) means: the key exactly, else the label ignoring
    /// case, else the only label containing it.
    pub fn find_app(&self, wanted: &str) -> Result<&CatApp, String> {
        let w = wanted.trim().to_lowercase();
        if w.is_empty() {
            return Err("an app key is required".into());
        }
        if let Some(a) = self.catalog.iter().find(|a| a.key.to_lowercase() == w) {
            return Ok(a);
        }
        if let Some(a) = self.catalog.iter().find(|a| a.label.to_lowercase() == w) {
            return Ok(a);
        }
        let hits: Vec<&CatApp> = self
            .catalog
            .iter()
            .filter(|a| a.label.to_lowercase().contains(&w) || a.key.to_lowercase().contains(&w))
            .collect();
        match hits.len() {
            0 => Err(format!(
                "no installed app matches {wanted:?}: look at the world tool's apps"
            )),
            1 => Ok(hits[0]),
            _ => Err(format!(
                "several apps match {wanted:?} ({}): use an exact key",
                hits.iter()
                    .take(6)
                    .map(|a| a.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Record something Pointer did. `saved` is the same in words that name no window or tab.
    pub fn record_action(&mut self, text: &str, saved: &str, now: Instant) {
        self.actions.push_back(ActionRec {
            at: now,
            unix: unix_now(),
            text: text.to_string(),
            saved: saved.to_string(),
        });
        while self.actions.len() > KEEP_ACTIONS {
            self.actions.pop_front();
        }
        self.save_actions();
    }

    fn save_actions(&self) {
        let Some(dir) = &self.dir else { return };
        let rows: Vec<Value> = self
            .actions
            .iter()
            .map(|a| json!({ "at": a.unix, "text": a.saved }))
            .collect();
        if std::fs::create_dir_all(dir).is_ok() {
            if let Ok(text) = serde_json::to_string_pretty(&rows) {
                let _ = std::fs::write(dir.join(ACTIONS_FILE), text);
            }
        }
    }

    fn observed_json(&self, now: Instant) -> Value {
        match (self.observed, self.observed_wall) {
            (Some(t), Some(w)) => json!({
                "observed_at": w.format("%H:%M:%S").to_string(),
                "observed_ago_s": now.saturating_duration_since(t).as_secs(),
            }),
            _ => json!({ "observed_at": null }),
        }
    }

    fn changes_json(&self, since: u64, now: Instant) -> Vec<Value> {
        self.changes
            .iter()
            .filter(|c| c.seq > since)
            .map(|c| {
                json!({ "what": c.text,
                        "ago_s": now.saturating_duration_since(c.at).as_secs() })
            })
            .collect()
    }

    fn actions_json(&self, n: usize, now: Instant) -> Vec<Value> {
        let skip = self.actions.len().saturating_sub(n);
        self.actions
            .iter()
            .skip(skip)
            .map(|a| {
                json!({ "did": a.text,
                        "ago_s": now.saturating_duration_since(a.at).as_secs() })
            })
            .collect()
    }

    /// The short version injected into every turn.
    pub fn summary(&self, now: Instant) -> Value {
        let Some(s) = &self.snap else {
            return json!({ "world": "not observed yet" });
        };
        let mut out = self.observed_json(now);
        let o = out.as_object_mut().expect("an object");
        if let Some(f) = s.focused() {
            o.insert("focused".into(), json!(format!("{}: {}", f.app, clip(&f.title, SUMMARY_CHARS))));
        }
        if let Some(m) = &s.mouse {
            o.insert(
                "mouse".into(),
                json!({ "x": m.x.round(), "y": m.y.round(), "display": m.monitor + 1 }),
            );
        }
        o.insert("displays".into(), json!(s.monitors.len()));
        let mut per_monitor = serde_json::Map::new();
        let mut shown = 0;
        for w in &s.windows {
            if shown >= SUMMARY_WINDOWS {
                break;
            }
            shown += 1;
            let line = format!(
                "{}: {}{}",
                w.app,
                clip(&w.title, SUMMARY_CHARS),
                if w.minimized { " (minimized)" } else { "" }
            );
            per_monitor
                .entry(format!("display {}", w.monitor + 1))
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .expect("an array")
                .push(json!(line));
        }
        o.insert("open_windows".into(), Value::Object(per_monitor));
        if s.windows.len() > shown {
            o.insert("more_windows".into(), json!(s.windows.len() - shown));
        }
        if !s.browsers.is_empty() {
            let rows: Vec<Value> = s
                .browsers
                .iter()
                .map(|b| {
                    let active = b.tabs.iter().find(|t| t.active).map(|t| clip(&t.title, SUMMARY_CHARS));
                    json!({ "browser": b.app, "display": b.monitor + 1,
                            "tabs": b.tabs.len(), "active_tab": active, "url": b.url })
                })
                .collect();
            o.insert("browsers".into(), json!(rows));
        }
        o.insert("installed_apps".into(), json!(self.catalog.len()));
        let changes = self.changes_json(self.since, now);
        if !changes.is_empty() {
            o.insert("changed_since_last_turn".into(), json!(changes));
        }
        let did = self.actions_json(5, now);
        if !did.is_empty() {
            o.insert("pointer_did_recently".into(), json!(did));
        }
        o.insert(
            "note".into(),
            json!("The world tool has every window, every tab title, the app list and the full log."),
        );
        json!({ "world": out })
    }

    /// Everything, or one `section`: windows, tabs, apps, changes, actions.
    pub fn detail(&self, section: Option<&str>, now: Instant) -> Value {
        let want = |name: &str| section.is_none_or(|s| s == name || s == "all");
        let mut out = self.observed_json(now);
        let o = out.as_object_mut().expect("an object");
        if let Some(s) = &self.snap {
            if want("windows") {
                o.insert(
                    "windows".into(),
                    json!(s
                        .windows
                        .iter()
                        .map(|w| json!({ "app": w.app, "title": w.title, "display": w.monitor + 1,
                                         "minimized": w.minimized, "focused": w.foreground }))
                        .collect::<Vec<_>>()),
                );
                o.insert(
                    "displays".into(),
                    json!(s
                        .monitors
                        .iter()
                        .map(|(i, x, y, w, h, p)| json!({ "display": i + 1, "x": x, "y": y,
                                                          "w": w, "h": h, "primary": p }))
                        .collect::<Vec<_>>()),
                );
                if let Some(m) = &s.mouse {
                    o.insert(
                        "mouse".into(),
                        json!({ "x": m.x.round(), "y": m.y.round(), "display": m.monitor + 1 }),
                    );
                }
            }
            if want("tabs") {
                o.insert(
                    "browsers".into(),
                    json!(s
                        .browsers
                        .iter()
                        .map(|b| json!({
                            "browser": b.app, "display": b.monitor + 1, "url": b.url,
                            "tabs": b.tabs.iter().map(|t| json!({ "title": t.title, "active": t.active })).collect::<Vec<_>>(),
                        }))
                        .collect::<Vec<_>>()),
                );
            }
        }
        if want("apps") {
            o.insert(
                "apps".into(),
                json!(self
                    .catalog
                    .iter()
                    .map(|a| json!({ "key": a.key, "name": a.label, "launch": a.launch }))
                    .collect::<Vec<_>>()),
            );
        }
        if want("changes") {
            o.insert(
                "changes".into(),
                json!(self.changes_json(0, now)),
            );
        }
        if want("actions") {
            o.insert("pointer_did".into(), json!(self.actions_json(KEEP_ACTIONS, now)));
        }
        out
    }
}

fn describe(w: &WinRec) -> String {
    format!("{}: {}", w.app, clip(&w.title, SUMMARY_CHARS))
}

/// Seconds as a duration, for tests that move time.
pub fn secs(n: f64) -> Duration {
    Duration::from_secs_f64(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(hwnd: isize, app: &str, title: &str, monitor: usize, fg: bool) -> WinRec {
        WinRec {
            hwnd,
            app: app.into(),
            title: title.into(),
            monitor,
            minimized: false,
            foreground: fg,
        }
    }

    fn snap(windows: Vec<WinRec>) -> Snapshot {
        Snapshot {
            windows,
            ..Default::default()
        }
    }

    #[test]
    fn facts_go_stale_after_a_few_seconds() {
        let mut w = World::new();
        let t0 = Instant::now();
        assert!(!w.is_fresh(t0));
        w.update(snap(vec![]), t0);
        assert!(w.is_fresh(t0 + secs(1.0)));
        assert!(!w.is_fresh(t0 + secs(FRESH_SECS + 0.5)));
    }

    #[test]
    fn opened_closed_and_focus_moves_are_changes_since_the_last_turn() {
        let mut w = World::new();
        let t = Instant::now();
        w.update(snap(vec![win(1, "chrome", "Docs", 0, true), win(2, "code", "main.rs", 1, false)]), t);
        w.begin_turn();
        // The first look compares with nothing.
        assert!(w.changes.is_empty());
        w.update(
            snap(vec![
                win(1, "chrome", "Docs", 0, false),
                win(3, "Notepad", "Untitled", 0, true),
            ]),
            t + secs(2.0),
        );
        w.begin_turn();
        let text: Vec<String> = w.changes.iter().map(|c| c.text.clone()).collect();
        assert!(text.iter().any(|c| c.starts_with("opened Notepad")), "{text:?}");
        assert!(text.iter().any(|c| c.starts_with("closed code")), "{text:?}");
        assert!(text.iter().any(|c| c.starts_with("focus moved to Notepad")), "{text:?}");
        let s = w.summary(t + secs(3.0));
        assert_eq!(s["world"]["changed_since_last_turn"].as_array().unwrap().len(), 3);
        // The next turn, with nothing new, reports nothing.
        w.update(snap(w.snap.clone().unwrap().windows), t + secs(5.0));
        w.begin_turn();
        assert!(w.summary(t + secs(5.0))["world"]["changed_since_last_turn"].is_null());
    }

    #[test]
    fn the_summary_is_compact_and_carries_observed_at() {
        let mut w = World::new();
        let t = Instant::now();
        let many: Vec<WinRec> = (0..40).map(|i| win(i, "app", &format!("title {i}"), (i % 2) as usize, i == 0)).collect();
        w.update(snap(many), t);
        let s = w.summary(t + secs(2.0));
        assert!(s["world"]["observed_at"].is_string());
        assert_eq!(s["world"]["observed_ago_s"], 2);
        assert_eq!(s["world"]["more_windows"], 40 - SUMMARY_WINDOWS);
        assert!(s.to_string().len() < 2500, "{}", s.to_string().len());
        assert_eq!(w.summary(t)["world"]["focused"], "app: title 0");
        assert_eq!(World::new().summary(t)["world"], "not observed yet");
    }

    #[test]
    fn tab_titles_and_window_titles_are_never_persisted() {
        let dir = std::env::temp_dir().join(format!("pointer-world-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut w = World::new();
        w.set_dir(&dir);
        let t = Instant::now();
        let mut s = snap(vec![win(1, "chrome", "Secret Window Title", 0, true)]);
        s.browsers.push(BrowserRec {
            hwnd: 1,
            app: "chrome".into(),
            monitor: 0,
            tabs: vec![TabRec { title: "Secret Tab Title".into(), active: true }],
            url: Some("https://example.com/a".into()),
        });
        w.update(s, t);
        w.record_action("switched to the tab \"Secret Tab Title\"", "switched a browser tab", t);
        w.record_action("opened Notepad", "opened Notepad", t);
        // In memory the full words are there; on disk only the saved ones.
        assert!(w.detail(Some("actions"), t).to_string().contains("Secret Tab Title"));
        let mut disk = String::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            disk.push_str(&std::fs::read_to_string(e.path()).unwrap());
        }
        assert!(!disk.contains("Secret"), "{disk}");
        assert!(disk.contains("switched a browser tab") && disk.contains("opened Notepad"));
        // A new session reads the log back.
        let mut again = World::new();
        again.set_dir(&dir);
        assert_eq!(again.actions.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_address_loses_its_query_and_fragment() {
        assert_eq!(redact_url("https://a.com/x?token=1#f"), "https://a.com/x");
        assert_eq!(redact_url("https://a.com/x"), "https://a.com/x");
    }

    #[test]
    fn apps_are_found_by_key_name_or_a_unique_part() {
        let mut w = World::new();
        let app = |k: &str, l: &str| CatApp { key: k.into(), label: l.into(), launch: format!("shell:{k}") };
        w.set_catalog(vec![app("notepad", "Notepad"), app("notepad_plus", "Notepad++"), app("calculator", "Calculator")]);
        assert_eq!(w.find_app("notepad").unwrap().label, "Notepad");
        assert_eq!(w.find_app("Calculator").unwrap().key, "calculator");
        assert_eq!(w.find_app("calc").unwrap().key, "calculator");
        assert!(w.find_app("pad").unwrap_err().contains("several"));
        assert!(w.find_app("zzz").is_err());
    }

    struct FakeDesktop;

    impl Desktop for FakeDesktop {
        fn monitors(&self) -> Vec<Monitor> {
            vec![
                Monitor { index: 0, left: 0, top: 0, right: 2560, bottom: 1440, primary: true },
                Monitor { index: 1, left: 1, top: -1440, right: 2561, bottom: 0, primary: false },
            ]
        }
        fn open_windows(&self) -> Vec<platform::winlist::WindowInfo> {
            let w = |hwnd, app: &str, title: &str, monitor, fg| platform::winlist::WindowInfo {
                hwnd,
                title: title.into(),
                app: app.into(),
                pid: 1,
                rect: (0, 0, 800, 600),
                monitor,
                minimized: false,
                foreground: fg,
            };
            vec![
                w(10, "chrome", "Inbox - Chrome", 0, true),
                w(11, "Code", "main.rs", 1, false),
                w(12, "Program Manager", "Program Manager", 0, false),
            ]
        }
        fn foreground_hwnd(&self) -> isize {
            10
        }
        fn process_name(&self, _: u32) -> String {
            String::new()
        }
        fn screenshot(&self, _: Option<&Monitor>) -> Option<platform::capture::Shot> {
            None
        }
        fn focused_field(&self) -> Option<crate::models::Field> {
            None
        }
        fn browser_url(&self, _: isize, _: (f64, f64), _: (f64, f64)) -> Option<String> {
            None
        }
        fn ocr(&self, _: &[u8], _: u32, _: u32) -> Vec<crate::perception::Line> {
            Vec::new()
        }
        fn walk(&self, _: isize, _: f64, _: f64, _: (f64, f64)) -> platform::uia::Walked {
            platform::uia::WalkedOf { on_screen: vec![], off_screen: vec![], capped: false }
        }
        fn pointer(&self) -> Option<(f64, f64, Option<f64>)> {
            Some((100.0, -700.0, None))
        }
    }

    #[test]
    fn gather_reads_windows_browser_tabs_and_the_mouse_on_the_monitor_above() {
        let scan = |hwnd: isize| {
            assert_eq!(hwnd, 10, "only the browser is scanned");
            platform::uia::BrowserScan {
                tabs: vec![
                    platform::uia::TabInfo { title: "Inbox".into(), selected: true },
                    platform::uia::TabInfo { title: "Docs".into(), selected: false },
                ],
                fields: vec![(
                    "AXTextField".into(),
                    "Address and search bar".into(),
                    "mail.example.com/u/0?token=abc#x".into(),
                )],
            }
        };
        let s = gather(&FakeDesktop, &scan);
        assert_eq!(s.windows.len(), 2, "the desktop shell is not a window");
        assert_eq!(s.windows[0].app, "chrome");
        assert!(s.windows[0].foreground);
        assert_eq!(s.browsers.len(), 1);
        assert_eq!(s.browsers[0].tabs.len(), 2);
        assert!(s.browsers[0].tabs[0].active);
        assert_eq!(s.browsers[0].url.as_deref(), Some("https://mail.example.com/u/0"));
        let m = s.mouse.unwrap();
        assert_eq!(m.monitor, 1, "(100, -700) is on the display above the primary");
        assert_eq!(s.monitors[1], (1, 1, -1440, 2560, 1440, false));
    }
}
