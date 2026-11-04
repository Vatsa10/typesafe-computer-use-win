//! The fast path against a fake classifier and a fake desk. No network, no screen.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::chat::Cancel;
use api::typesafe::{Answer, ApiError, ChoiceAnswer, Question};
use serde_json::Value;

use super::super::testing::{saying, FakeChat, Harness, Script};
use super::super::{Limits, Senses};
use super::*;
use crate::catalog::Site;
use crate::world::{BrowserRec, CatApp, Snapshot, TabRec, WinRec};

/// The machine the tests and the labelled set share.
pub(super) fn demo_apps() -> Vec<CatApp> {
    [
        ("calculator", "Calculator"),
        ("notepad", "Notepad"),
        ("paint", "Paint"),
        ("spotify", "Spotify"),
        ("vscode", "Visual Studio Code"),
        ("word", "Microsoft Word"),
        ("excel", "Microsoft Excel"),
        ("slack", "Slack"),
        ("discord", "Discord"),
        ("explorer", "File Explorer"),
        ("settings", "Settings"),
        ("taskmgr", "Task Manager"),
        ("terminal", "Windows Terminal"),
        ("chrome", "Google Chrome"),
    ]
    .iter()
    .map(|(k, l)| CatApp {
        key: k.to_string(),
        label: l.to_string(),
        launch: format!("shell:AppsFolder\\{k}"),
    })
    .collect()
}

pub(super) fn demo_snapshot() -> Snapshot {
    let win = |hwnd, app: &str, title: &str, fg| WinRec {
        hwnd,
        app: app.into(),
        title: title.into(),
        monitor: 0,
        minimized: false,
        foreground: fg,
    };
    let tab = |t: &str, a| TabRec {
        title: t.into(),
        active: a,
    };
    Snapshot {
        windows: vec![
            win(1, "chrome", "Inbox (3) - Gmail - Google Chrome", true),
            win(2, "Code", "main.rs - pointer - Visual Studio Code", false),
            win(3, "slack", "general - Acme - Slack", false),
            win(4, "EXCEL", "Budget.xlsx - Excel", false),
        ],
        browsers: vec![BrowserRec {
            hwnd: 1,
            app: "chrome".into(),
            monitor: 0,
            tabs: vec![
                tab("Inbox (3) - Gmail", true),
                tab("Quarterly plan - Google Docs", false),
                tab("YouTube", false),
                tab("Pull requests - GitHub", false),
            ],
            url: Some("https://mail.google.com/mail/u/0/".into()),
        }],
        monitors: vec![],
        mouse: None,
    }
}

pub(super) fn demo_sites() -> Vec<Site> {
    vec![
        Site::new(
            "youtube",
            "YouTube",
            "https://www.youtube.com/",
            1.0,
            "pinned",
        ),
        Site::new("github", "GitHub", "https://github.com/", 1.0, "pinned"),
        Site::new("gmail", "Gmail", "https://mail.google.com/", 1.0, "pinned"),
        Site::new("reddit", "Reddit", "https://www.reddit.com/", 1.0, "pinned"),
        Site::new(
            "wikipedia",
            "Wikipedia",
            "https://www.wikipedia.org/",
            1.0,
            "pinned",
        ),
    ]
}

pub(super) fn demo_world(utterance: &str) -> FastWorld {
    FastWorld::from_parts(
        &demo_apps(),
        Some(&demo_snapshot()),
        demo_sites(),
        "",
        utterance,
    )
}

#[derive(Default)]
struct Desk {
    opened: Mutex<Vec<String>>,
    focused: Mutex<Vec<isize>>,
    switched: Mutex<Vec<String>>,
}

impl Senses for Desk {
    fn observe(&self) -> Result<super::super::Observation, String> {
        Err("no screen".into())
    }
    fn world(&self) -> Result<Snapshot, String> {
        Ok(demo_snapshot())
    }
    fn catalog(&self) -> Vec<CatApp> {
        demo_apps()
    }
    fn open_app(&self, app: &CatApp) -> Result<String, String> {
        self.opened.lock().unwrap().push(app.key.clone());
        Ok("new window".into())
    }
    fn focus_window(&self, hwnd: isize) -> Result<(), String> {
        self.focused.lock().unwrap().push(hwnd);
        Ok(())
    }
    fn switch_tab(&self, title: &str) -> Result<String, String> {
        self.switched.lock().unwrap().push(title.to_string());
        Ok(title.to_string())
    }
}

/// A classifier that says what it is told: the action, and the first target option whose
/// description contains `target`.
pub(super) struct Pick {
    pub action: &'static str,
    pub action_conf: f64,
    pub target: &'static str,
    pub target_conf: f64,
    pub calls: AtomicUsize,
    pub fail: bool,
}

pub(super) fn pick(action: &'static str, ac: f64, target: &'static str, tc: f64) -> Arc<Pick> {
    Arc::new(Pick {
        action,
        action_conf: ac,
        target,
        target_conf: tc,
        calls: AtomicUsize::new(0),
        fail: false,
    })
}

impl Classifier for Pick {
    fn system_one(
        &self,
        _state: &Value,
        questions: &[(String, Question)],
    ) -> Result<Vec<(String, Answer)>, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(ApiError {
                status: Some(500),
                message: "down".into(),
            });
        }
        let mut out = Vec::new();
        for (name, q) in questions {
            let Question::Choice { criteria, .. } = q else {
                continue;
            };
            let (choice, conf) = if name == "action" {
                (self.action.to_string(), self.action_conf)
            } else {
                let key = criteria
                    .iter()
                    .find(|(_, d)| d.as_deref().is_some_and(|d| d.contains(self.target)))
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| "none".into());
                (key, self.target_conf)
            };
            out.push((
                name.clone(),
                Answer::Choice(ChoiceAnswer {
                    choice,
                    confidence: conf,
                    probabilities: vec![],
                }),
            ));
        }
        Ok(out)
    }
}

struct Rig {
    h: Harness,
    desk: Arc<Desk>,
    chat: Arc<FakeChat>,
}

fn rig(tag: &str, act: bool, agent: Vec<Script>) -> Rig {
    let desk = Arc::new(Desk::default());
    let chat = FakeChat::scripted(agent);
    let h = Harness::with_senses(tag, chat.clone(), Limits::default(), desk.clone());
    h.act.store(act, Ordering::SeqCst);
    Rig { h, desk, chat }
}

fn cfg() -> FastCfg {
    FastCfg {
        enabled: true,
        threshold: 0.9,
        timeout: Duration::from_secs(2),
        sites: Some(Arc::new(|_| demo_sites())),
    }
}

fn say_to(r: &Rig, c: Arc<Pick>, text: &str) {
    let a = r.h.assistant_with(c);
    a.set_fast(cfg());
    a.run_turn(text, "typed", &Cancel::new());
}

fn lines(r: &Rig) -> Vec<String> {
    r.h.emit
        .of("line")
        .iter()
        .filter_map(|v| v["text"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn a_plain_open_runs_in_act_mode_with_a_fixed_reply_and_no_agent_call() {
    let r = rig("fast-open", true, vec![]);
    say_to(
        &r,
        pick("open_app", 1.0, "Installed app: Calculator", 1.0),
        "open calculator",
    );
    assert_eq!(
        *r.desk.opened.lock().unwrap(),
        vec!["calculator".to_string()]
    );
    assert_eq!(
        *r.h.voice.spoken.lock().unwrap(),
        vec!["Opened Calculator.".to_string()]
    );
    assert!(r.chat.calls().is_empty(), "the big model was never called");
    assert!(
        lines(&r)
            .iter()
            .any(|l| l.starts_with("path: fast (open_app")),
        "{:?}",
        lines(&r)
    );
}

#[test]
fn dry_mode_opens_nothing_and_says_so() {
    let r = rig("fast-dry", false, vec![]);
    say_to(
        &r,
        pick("open_app", 1.0, "Installed app: Notepad", 1.0),
        "open notepad please",
    );
    assert!(r.desk.opened.lock().unwrap().is_empty());
    assert!(r.h.voice.spoken.lock().unwrap()[0].starts_with("Dry mode"));
    assert!(r.chat.calls().is_empty());
}

#[test]
fn the_fast_action_is_in_the_history_so_a_follow_up_sees_it() {
    let r = rig("fast-history", true, vec![saying("the calculator is open")]);
    let a =
        r.h.assistant_with(pick("open_app", 1.0, "Installed app: Calculator", 1.0));
    a.set_fast(cfg());
    a.run_turn("open calculator", "typed", &Cancel::new());
    a.set_fast(FastCfg {
        enabled: false,
        ..cfg()
    });
    a.run_turn("is it open", "typed", &Cancel::new());
    let seen = &r.chat.calls()[0].messages;
    let text = serde_json::to_string(seen).unwrap();
    assert!(
        text.contains("open calculator")
            && text.contains("\"open_app\"")
            && text.contains("Opened Calculator."),
        "{text}"
    );
}

#[test]
fn low_confidence_goes_to_the_agent() {
    let r = rig("fast-low", true, vec![saying("ok")]);
    say_to(
        &r,
        pick("open_app", 0.6, "Installed app: Calculator", 1.0),
        "open calculator",
    );
    assert!(r.desk.opened.lock().unwrap().is_empty());
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
    assert!(lines(&r).iter().any(|l| l.starts_with("path: agent")));
    let r = rig("fast-low2", true, vec![saying("ok")]);
    say_to(
        &r,
        pick("open_app", 1.0, "Installed app: Calculator", 0.5),
        "open calculator",
    );
    assert!(r.desk.opened.lock().unwrap().is_empty());
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
}

#[test]
fn needs_agent_and_a_dead_classifier_go_to_the_agent() {
    let r = rig("fast-na", true, vec![saying("ok")]);
    say_to(&r, pick("needs_agent", 1.0, "none", 1.0), "what time is it");
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
    let r = rig("fast-dead", true, vec![saying("ok")]);
    let mut c = Pick {
        fail: true,
        ..Pick {
            action: "open_app",
            action_conf: 1.0,
            target: "",
            target_conf: 1.0,
            calls: AtomicUsize::new(0),
            fail: false,
        }
    };
    c.fail = true;
    say_to(&r, Arc::new(c), "open calculator");
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
}

#[test]
fn a_confident_wrong_guess_cannot_launch_anything() {
    // The model is certain, but the words are not an exact match for the app or are a longer plan.
    for line in [
        "open calculator and add two and two",
        "open calc",
        "open the calculator app and then close it",
        "abre la calculadora",
        "how do i open the calculator",
    ] {
        let r = rig("fast-guard", true, vec![saying("ok")]);
        say_to(
            &r,
            pick("open_app", 1.0, "Installed app: Calculator", 1.0),
            line,
        );
        assert!(r.desk.opened.lock().unwrap().is_empty(), "{line}");
        assert!(!r.chat.calls().is_empty(), "{line}"); // reached the agent; pursuit may nudge it again
    }
}

#[test]
fn a_window_that_matches_twice_or_a_target_that_is_not_listed_goes_to_the_agent() {
    // "chrome" is the app of the front window only, but the title also reads Google Chrome.
    let r = rig("fast-win", true, vec![saying("ok")]);
    say_to(
        &r,
        pick("switch_window", 1.0, "Open window: Code", 1.0),
        "switch to slack",
    );
    assert!(
        r.desk.focused.lock().unwrap().is_empty(),
        "the model picked VS Code, the words say Slack"
    );
    let r = rig("fast-none", true, vec![saying("ok")]);
    say_to(
        &r,
        pick("open_app", 1.0, "no such thing", 1.0),
        "open photoshop",
    );
    assert!(r.desk.opened.lock().unwrap().is_empty());
}

#[test]
fn switching_a_window_a_tab_and_opening_a_site() {
    let r = rig("fast-sw", true, vec![]);
    say_to(
        &r,
        pick("switch_window", 1.0, "Open window: slack", 1.0),
        "switch to slack",
    );
    assert_eq!(*r.desk.focused.lock().unwrap(), vec![3]);
    let r = rig("fast-tab", true, vec![]);
    say_to(
        &r,
        pick("switch_tab", 1.0, "Open browser tab: Quarterly", 1.0),
        "go to the quarterly plan tab",
    );
    assert_eq!(
        *r.desk.switched.lock().unwrap(),
        vec!["Quarterly plan - Google Docs".to_string()]
    );
    let r = rig("fast-site", true, vec![]);
    say_to(
        &r,
        pick("open_site", 1.0, "Website: YouTube", 1.0),
        "open youtube",
    );
    let runs = r.h.hands.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].0.contains("youtube.com") && runs[0].1, "{runs:?}");
    assert_eq!(r.h.voice.spoken.lock().unwrap()[0], "Opened YouTube.");
    // In Dry mode the same goes through the hands as a preview, never an action.
    let r = rig("fast-site-dry", false, vec![]);
    say_to(
        &r,
        pick("open_site", 1.0, "Website: YouTube", 1.0),
        "open youtube",
    );
    assert!(!r.h.hands.runs.lock().unwrap()[0].1);
}

#[test]
fn stop_and_pause_only_for_bare_lines() {
    let r = rig("fast-stop", true, vec![]);
    say_to(&r, pick("stop", 1.0, "none", 0.0), "stop");
    assert_eq!(r.h.voice.spoken.lock().unwrap()[0], "Stopped.");
    assert!(r.h.voice.stops.load(Ordering::SeqCst) >= 1);
    let r = rig("fast-pause", true, vec![]);
    say_to(&r, pick("pause", 1.0, "none", 0.0), "hold on");
    assert_eq!(r.h.voice.spoken.lock().unwrap()[0], "Paused.");
    // "stop the music" is not a bare stop line.
    let r = rig("fast-stop2", true, vec![saying("ok")]);
    say_to(
        &r,
        pick("stop", 1.0, "none", 0.0),
        "stop the music in spotify",
    );
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
}

#[test]
fn the_switch_turns_it_off() {
    let r = rig("fast-off", true, vec![saying("ok")]);
    let c = pick("open_app", 1.0, "Installed app: Calculator", 1.0);
    let a = r.h.assistant_with(c.clone());
    a.set_fast(FastCfg {
        enabled: false,
        ..cfg()
    });
    a.run_turn("open calculator", "typed", &Cancel::new());
    assert_eq!(c.calls.load(Ordering::SeqCst), 0);
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
}

#[test]
fn the_object_phrase_drops_the_verb_and_the_manners() {
    assert_eq!(
        object_phrase("Hey Pointer, could you open the calculator please?"),
        "calculator"
    );
    assert_eq!(object_phrase("switch to my slack window"), "slack");
    assert_eq!(
        object_phrase("open chrome and search for cats"),
        "chrome and search for cats"
    );
    assert_eq!(
        object_phrase("go back to the quarterly plan tab"),
        "quarterly plan"
    );
}

#[test]
fn an_app_that_already_has_a_window_goes_to_the_agent() {
    // Slack is open: "open slack" may mean "go to it".
    let r = rig("fast-running", true, vec![saying("ok")]);
    say_to(
        &r,
        pick("open_app", 1.0, "Installed app: Slack", 1.0),
        "open slack",
    );
    assert!(r.desk.opened.lock().unwrap().is_empty());
    assert!(!r.chat.calls().is_empty()); // reached the agent; pursuit may nudge it again
}
