//! What is open right now, across every monitor. A port of `worldmodel.py`.
//!
//! The loop used to know exactly one thing about the machine: whichever window happened to be in
//! front. Everything else it had to infer from pixels, so "open whatsapp" while WhatsApp sat on the
//! second monitor read as "nothing on this screen helps" — and the honest fix is not better OCR, it
//! is telling the classifier what is already running.
//!
//! This module turns the window inventory into two things: records for the state packet, and the
//! criteria for a `window` question. It takes the inventory as an argument rather than fetching it,
//! so the ranking rules are testable without a desktop.

use platform::display::Monitor;
use platform::winlist::WindowInfo;
use serde_json::{json, Value};

use crate::models::{role_word, PointerInfo};

/// Options compete for probability mass, so this is a budget, not a limit.
pub const MAX_WINDOWS: usize = 18;
pub const TITLE_CHARS: usize = 70;

/// Windows that exist on every desktop and are never what someone means by "switch to". Program
/// Manager is the desktop itself; the shell experience host and the input host are chrome.
pub const SHELL_TITLES: [&str; 3] = [
    "program manager",
    "windows input experience",
    "windows shell experience host",
];

/// A packaged app is hosted inside ApplicationFrameHost, so Settings and the like show up twice:
/// once as the app, once as the frame around it. The frame is the duplicate worth dropping, but
/// only when the real one is also listed -- for some apps the frame is all there is.
pub const FRAME_HOST: &str = "applicationframehost";

/// Whether a window is a plausible thing to switch to.
///
/// The inventory already drops untitled, invisible and tiny windows. What is left to reject is the
/// shell's own furniture, which is always open and never meant.
pub fn interesting(window: &WindowInfo) -> bool {
    !SHELL_TITLES.contains(&window.title.trim().to_lowercase().as_str())
}

/// Remove the ApplicationFrameHost copy of a window that is already listed under its own app.
///
/// Offering both halves of one window wastes an option and splits the probability between two
/// answers that do the same thing, which reads as doubt and can stop a run.
pub fn drop_frame_duplicates(windows: Vec<WindowInfo>) -> Vec<WindowInfo> {
    let real: std::collections::HashSet<String> = windows
        .iter()
        .filter(|w| w.app.to_lowercase() != FRAME_HOST)
        .map(|w| w.title.trim().to_lowercase())
        .collect();
    windows
        .into_iter()
        .filter(|w| {
            w.app.to_lowercase() != FRAME_HOST || !real.contains(&w.title.trim().to_lowercase())
        })
        .collect()
}

/// The windows worth offering, best first. (Python's default `limit` is `MAX_WINDOWS`.)
///
/// The foreground window comes first because it is the one being worked in, then the rest in the
/// order the inventory gave them, which is already monitor then z-order. A minimized window sorts
/// last: it is reachable, but it is not what someone means when several copies of a thing are
/// open.
pub fn rank(windows: &[WindowInfo], limit: usize) -> Vec<WindowInfo> {
    let kept: Vec<WindowInfo> = windows.iter().filter(|w| interesting(w)).cloned().collect();
    let mut kept = drop_frame_duplicates(kept);
    kept.sort_by_key(|w| (!w.foreground, w.minimized, w.monitor));
    kept.truncate(limit);
    kept
}

/// Python's `repr()` of a string: single quotes unless the text holds a single quote and no double
/// quote, with backslashes, the chosen quote and control characters escaped.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c if c.is_control() => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else {
                    out.push_str(&format!("\\u{n:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// One window as a line a classifier can reason about: the app, its title, and where it is.
pub fn summary(window: &WindowInfo) -> String {
    let mut title = window.title.trim().to_string();
    if title.chars().count() > TITLE_CHARS {
        let cut: String = title.chars().take(TITLE_CHARS - 1).collect();
        title = format!("{}…", cut.trim_end());
    }
    let place = format!("monitor {}", window.monitor + 1);
    let state = if window.minimized {
        " (minimized)"
    } else if window.foreground {
        " (in front)"
    } else {
        ""
    };
    format!("{}: {} on {place}{state}", window.app, py_repr(&title))
}

/// The inventory as state. Keyed by position, which is what the window question answers with.
pub fn records(windows: &[WindowInfo]) -> Vec<Value> {
    windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            json!({
                "k": i,
                "app": w.app,
                "title": w.title.chars().take(TITLE_CHARS).collect::<String>(),
                "monitor": w.monitor + 1,
                "minimized": w.minimized,
                "in_front": w.foreground,
            })
        })
        .collect()
}

/// The window question's options, one per open window, in order.
pub fn criteria(windows: &[WindowInfo]) -> Vec<(String, String)> {
    windows
        .iter()
        .enumerate()
        .map(|(i, w)| (i.to_string(), summary(w)))
        .collect()
}

/// The displays themselves, so the classifier knows how much machine there is. Only the captured
/// one is ever read, which is why a run says which that was.
pub fn monitor_summary(monitors: &[Monitor]) -> Vec<Value> {
    monitors
        .iter()
        .map(|m| {
            json!({
                "monitor": m.index + 1,
                "size": format!("{}x{}", m.width(), m.height()),
                "primary": m.primary,
            })
        })
        .collect()
}

/// What a model is told about words like "this": they point at the mouse.
pub const DEICTIC_NOTE: &str = "'this', 'here' and 'that' refer to what is under the mouse";

/// The mouse for a model: where it is, which display, what it is over, and the item marked
/// `under_mouse` when one is. `reading` is the display the capture came from.
pub fn pointer_record(p: &PointerInfo, reading: usize, under_item: Option<usize>) -> Value {
    let over = p.under.as_ref().map(|u| {
        let label = u.label.trim();
        let role = role_word(&u.role);
        if label.is_empty() {
            role.to_string()
        } else {
            format!("{role} '{label}'")
        }
    });
    let mut sentence = format!(
        "the mouse is at ({}, {}) on display {}",
        p.x.round(),
        p.y.round(),
        p.monitor + 1
    );
    if let Some(over) = &over {
        sentence.push_str(&format!(" over {over}"));
    }
    if p.monitor != reading {
        sentence.push_str(", not the display being read");
    }
    let mut record = json!({
        "summary": sentence,
        "x": p.x.round(),
        "y": p.y.round(),
        "display": p.monitor + 1,
        "on_display_being_read": p.monitor == reading,
        "idle_seconds": p.idle_seconds.map(|s| (s * 10.0).round() / 10.0),
        "over": over,
        "note": DEICTIC_NOTE,
    });
    if let Some(i) = under_item {
        record["item_under_mouse"] = json!(i);
    }
    if let Some((app, title)) = &p.display_window {
        record["top_window_on_mouse_display"] = json!({ "app": app, "title": title });
    }
    record
}

/// The open window that best matches a name, or `None`.
///
/// Used to answer the question the loop could never answer before: is the thing being asked for
/// already running somewhere? A match on the process name is stronger than one in a title, since a
/// title mentions all sorts of things.
pub fn already_open<'a>(windows: &'a [WindowInfo], name: &str) -> Option<&'a WindowInfo> {
    let wanted = name.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    windows
        .iter()
        .find(|w| w.app.to_lowercase() == wanted)
        .or_else(|| {
            windows
                .iter()
                .find(|w| w.app.to_lowercase().contains(&wanted))
        })
        .or_else(|| {
            windows
                .iter()
                .find(|w| w.title.to_lowercase().contains(&wanted))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(hwnd: isize, title: &str, app: &str) -> WindowInfo {
        WindowInfo {
            hwnd,
            title: title.to_string(),
            app: app.to_string(),
            pid: 100,
            rect: (0, 0, 800, 600),
            monitor: 0,
            minimized: false,
            foreground: false,
        }
    }

    fn hwnds(windows: &[WindowInfo]) -> Vec<isize> {
        windows.iter().map(|w| w.hwnd).collect()
    }

    #[test]
    fn the_window_in_front_is_offered_first() {
        let windows = vec![
            WindowInfo {
                monitor: 1,
                ..win(1, "GitHub", "chrome")
            },
            WindowInfo {
                foreground: true,
                ..win(2, "main.py", "code")
            },
        ];
        assert_eq!(hwnds(&rank(&windows, MAX_WINDOWS)), vec![2, 1]);
    }

    #[test]
    fn a_minimized_window_sorts_behind_a_visible_one() {
        let windows = vec![
            WindowInfo {
                minimized: true,
                ..win(1, "Window", "whatsapp")
            },
            win(2, "Window", "slack"),
        ];
        assert_eq!(hwnds(&rank(&windows, MAX_WINDOWS)), vec![2, 1]);
    }

    /// Program Manager is the desktop itself and is always open; offering it wastes an option.
    #[test]
    fn the_shell_is_not_something_to_switch_to() {
        let windows = vec![
            win(1, "Program Manager", "explorer"),
            win(1, "Inbox", "outlook"),
        ];
        let titles: Vec<String> = rank(&windows, MAX_WINDOWS)
            .into_iter()
            .map(|w| w.title)
            .collect();
        assert_eq!(titles, vec!["Inbox"]);
    }

    #[test]
    fn the_list_is_capped_because_options_compete_for_probability_mass() {
        let windows: Vec<WindowInfo> = (0..40).map(|i| win(i, &format!("w{i}"), "app")).collect();
        assert_eq!(rank(&windows, MAX_WINDOWS).len(), MAX_WINDOWS);
    }

    #[test]
    fn a_window_line_says_which_monitor_it_is_on() {
        let line = summary(&WindowInfo {
            monitor: 1,
            ..win(1, "WhatsApp", "whatsapp")
        });
        assert!(line.contains("whatsapp") && line.contains("monitor 2"));
        assert_eq!(line, "whatsapp: 'WhatsApp' on monitor 2");
    }

    #[test]
    fn a_minimized_window_says_so_and_the_front_one_says_so() {
        assert!(summary(&WindowInfo {
            minimized: true,
            ..win(1, "Window", "app")
        })
        .contains("(minimized)"));
        assert!(summary(&WindowInfo {
            foreground: true,
            ..win(1, "Window", "app")
        })
        .contains("(in front)"));
    }

    #[test]
    fn a_long_title_is_shortened_rather_than_flooding_the_packet() {
        let line = summary(&win(1, &"x".repeat(200), "app"));
        assert!(line.chars().count() < 140 && line.contains('…'));
    }

    #[test]
    fn a_title_is_quoted_the_way_python_repr_quotes_it() {
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("say \"hi\" it's"), "'say \"hi\" it\\'s'");
        assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn every_offered_window_has_a_key_the_answer_can_name() {
        let windows = vec![win(1, "Window", "app"), win(2, "Window", "app")];
        let keys: Vec<String> = criteria(&windows).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["0", "1"]);
        let ks: Vec<i64> = records(&windows)
            .iter()
            .map(|r| r["k"].as_i64().unwrap())
            .collect();
        assert_eq!(ks, vec![0, 1]);
    }

    #[test]
    fn records_report_the_monitor_one_based_because_nobody_says_monitor_zero() {
        let rec = records(&[WindowInfo {
            monitor: 2,
            ..win(1, "Window", "app")
        }]);
        assert_eq!(rec[0]["monitor"], 3);
    }

    #[test]
    fn an_app_that_is_already_running_is_found_by_process_name() {
        let windows = vec![win(1, "GitHub", "chrome"), win(2, "WhatsApp", "whatsapp")];
        assert_eq!(already_open(&windows, "whatsapp").unwrap().app, "whatsapp");
    }

    /// A browser tab called "WhatsApp Web" must not stand in for the WhatsApp app itself.
    #[test]
    fn a_process_name_beats_a_passing_mention_in_someone_elses_title() {
        let windows = vec![
            win(1, "WhatsApp Web - Chrome", "chrome"),
            win(2, "WhatsApp", "whatsapp"),
        ];
        assert_eq!(already_open(&windows, "whatsapp").unwrap().app, "whatsapp");
    }

    #[test]
    fn a_title_match_still_counts_when_no_process_matches() {
        let windows = vec![win(1, "Figma - untitled", "chrome")];
        assert_eq!(already_open(&windows, "figma").unwrap().app, "chrome");
    }

    #[test]
    fn nothing_open_matches_nothing() {
        let windows = vec![win(1, "Window", "chrome")];
        assert!(already_open(&windows, "whatsapp").is_none());
        assert!(already_open(&windows, "   ").is_none());
    }

    /// Settings is hosted inside ApplicationFrameHost, so it appears under both names. Offering
    /// both splits the probability between two answers that do exactly the same thing.
    #[test]
    fn a_packaged_app_is_offered_once_not_twice() {
        let windows = vec![
            win(1, "Settings", "SystemSettings"),
            win(2, "Settings", "ApplicationFrameHost"),
        ];
        assert_eq!(hwnds(&rank(&windows, MAX_WINDOWS)), vec![1]);
    }

    /// For some packaged apps the frame is the only window there is, so this must not drop it.
    #[test]
    fn a_frame_host_window_with_no_app_behind_it_is_kept() {
        let windows = vec![win(2, "Calculator", "ApplicationFrameHost")];
        assert_eq!(hwnds(&rank(&windows, MAX_WINDOWS)), vec![2]);
    }

    #[test]
    fn monitors_are_summarised_one_based() {
        let m = Monitor {
            index: 0,
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
            primary: true,
        };
        assert_eq!(
            monitor_summary(&[m]),
            vec![json!({"monitor": 1, "size": "1920x1080", "primary": true})]
        );
    }
}
