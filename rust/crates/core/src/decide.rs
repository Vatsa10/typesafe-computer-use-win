//! The TypeSafe side: state, criteria, and the one multi-Choice request. A port of `decide.py`.
//!
//! The decision model is reached through [`Classifier`], which `api::typesafe::TypeSafe`
//! implements, so every test here runs against a fake without the network.
//!
//! `serde_json` is built without `preserve_order`, so the state object's keys go out sorted rather
//! than in the Python dict's order. The questions and their criteria keep the Python order: the
//! api crate writes that part of the body by hand.

use api::typesafe::{Answer, ApiError, ChoiceAnswer, Question, TypeSafe};
use platform::uia::Hit;
use serde_json::{json, Map, Value};

use crate::apps::App;
use crate::catalog::Site;
use crate::config::{self, SITES};
use crate::dates::{date_hints, now_context};
use crate::models::{role_word, Field, Item, Screen};
use crate::sitepick::app_criteria;
use crate::worldmodel;

pub const STOP_KINDS: [&str; 2] = ["done", "none"];
pub const OFFSCREEN_PREFIX: &str = "offscreen:";
pub const WINDOW_PREFIX: &str = "window:";
pub const APP_PREFIX: &str = "app:";
/// The run loop's floor: a decision below it stops the run. From config, as in the Python runner.
pub const MIN_CONFIDENCE: f64 = config::DEFAULT_MIN_CONFIDENCE;

pub const SWITCH_WINDOW: &str = "Bring a window that is already open to the front, chosen in the window question. Use this whenever the goal is about something already running, including on another monitor or minimized: switching to it is always better than opening a second copy.";
pub const OPEN_APP: &str = "Launch an installed application that is not currently open, chosen in the app question. Use this only when the window list does not already contain what the goal is about.";
pub const PRESS_OFFSCREEN: &str = "Activate a labelled control that the app exposes but that is not currently visible on screen (chosen in the offscreen question). Use when the needed control is known to exist but is scrolled out of view or not yet shown.";

/// The decision model, behind one method so tests can stand in for it.
pub trait Classifier {
    fn system_one(
        &self,
        state: &Value,
        questions: &[(String, Question)],
    ) -> Result<Vec<(String, Answer)>, ApiError>;
}

impl Classifier for TypeSafe {
    fn system_one(
        &self,
        state: &Value,
        questions: &[(String, Question)],
    ) -> Result<Vec<(String, Answer)>, ApiError> {
        TypeSafe::system_one(self, state, questions)
    }
}

/// An ordered criteria list, the shape every question takes.
pub type Criteria = Vec<(String, String)>;

fn pairs(items: &[(&str, String)]) -> Criteria {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Deterministic actions offered alongside click_item. Keep them mutually exclusive.
pub fn fixed_actions(browser: &str, email: Option<&str>) -> Criteria {
    let mut actions = pairs(&[
        (
            "use_browser",
            format!(
                "Work in {browser}: bring it to the front, and open a website there if one is needed. The \
                 site question says which website, or says that the page already open there is the one to \
                 continue with. This is the only way to reach a website: never click the address bar, a URL, \
                 or a search box to get there. Works from any app, including this one."
            ),
        ),
        (
            "type_text",
            "Type free text into the focused text field. A writing model composes the text from the \
             goal and the field's label. Only valid when a text field is focused and needs content."
                .into(),
        ),
        ("press_enter", "Press Return to submit the focused form or field.".into()),
        ("press_escape", "Press Escape to dismiss a dialog, menu, or popup.".into()),
        ("scroll_down", "Scroll down to reveal more of the page.".into()),
        ("scroll_up", "Scroll up.".into()),
        ("wait", "Nothing to do yet; the screen is still loading or changing.".into()),
        ("done", "The goal is already achieved.".into()),
        ("none", "Nothing on screen or in this list helps with the goal.".into()),
    ]);
    if email.is_some_and(|e| !e.is_empty()) {
        actions.push((
            "type_email".into(),
            "Type the user's email address into the focused text field. Use this, not type_text, \
             whenever the field wants an email or username."
                .into(),
        ));
    }
    actions
}

pub fn kind_criteria(
    browser: &str,
    email: Option<&str>,
    offscreen: bool,
    windows_open: bool,
    apps_known: bool,
) -> Criteria {
    let mut out = pairs(&[(
        "click_item",
        "Click one of the on-screen text items (chosen in the item question).".into(),
    )]);
    if offscreen {
        out.push(("press_offscreen".into(), PRESS_OFFSCREEN.into()));
    }
    if windows_open {
        out.push(("switch_window".into(), SWITCH_WINDOW.into()));
    }
    if apps_known {
        out.push(("open_app".into(), OPEN_APP.into()));
    }
    out.extend(fixed_actions(browser, email));
    out
}

/// Python's `repr()` of a string, which is how item text is quoted in the criteria.
pub(crate) fn py_repr(text: &str) -> String {
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
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Each item as one line. A role prefix marks the ones the app itself declared.
pub fn item_criteria(screen: &Screen, items: &[Item]) -> Criteria {
    let hints = date_hints(items, screen, None);
    items
        .iter()
        .map(|it| {
            let role = if it.from_ax() && !it.role.is_empty() {
                format!("{} ", it.role)
            } else {
                String::new()
            };
            let hint = hints
                .get(&it.index)
                .map(|h| format!("; {h}"))
                .unwrap_or_default();
            (
                it.index.to_string(),
                format!("{role}{} ({}{hint})", py_repr(&it.text), screen.region(it)),
            )
        })
        .collect()
}

/// Each off-screen control as one line, keyed by its position in `screen.offscreen`.
pub fn offscreen_criteria<N>(nodes: &[Hit<N>]) -> Criteria {
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            (
                i.to_string(),
                format!("{} {} (not visible)", role_word(&n.role), py_repr(&n.label)),
            )
        })
        .collect()
}

/// The same controls as state, with the key the offscreen question answers with.
pub fn offscreen_records<N>(nodes: &[Hit<N>]) -> Vec<Value> {
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| json!({"k": i, "role": role_word(&n.role), "label": n.label}))
        .collect()
}

/// Which website use_browser opens: the offered sites, plus one key for anything else and one for
/// nothing. Without a shortlist this falls back to the pinned catalog.
pub fn site_criteria(sites: Option<&[Site]>) -> Criteria {
    if let Some(sites) = sites.filter(|s| !s.is_empty()) {
        return crate::sitepick::criteria(sites);
    }
    let mut out: Criteria = SITES
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    out.push((
        "other".into(),
        "A website is needed to progress the goal, but it is not one of the sites named in this list.".into(),
    ));
    out.push((
        "none".into(),
        "No website needs to be opened: the page already open in the browser is the one to continue with.".into(),
    ));
    out
}

fn last<T: Clone>(xs: &[T], n: usize) -> Vec<T> {
    xs[xs.len().saturating_sub(n)..].to_vec()
}

pub fn base_state(goal: &str, screen: &Screen, items: &[Item], history: &[String]) -> Value {
    let hints = date_hints(items, screen, None);
    let mut state = Map::new();
    state.insert("goal".into(), json!(goal));
    state.insert("now".into(), now_context());
    state.insert("frontmost_app".into(), json!(screen.app));
    state.insert("browser_active_tab_url".into(), json!(screen.url));
    state.insert(
        "focused_field".into(),
        screen
            .field
            .as_ref()
            .map(Field::summary)
            .unwrap_or(Value::Null),
    );
    state.insert("previous_actions".into(), json!(last(history, 8)));
    let under = screen.under_mouse(items);
    if let Some(p) = &screen.pointer {
        // A deictic goal ("click this") is the item flagged under_mouse: no action of its own.
        state.insert(
            "pointer".into(),
            worldmodel::pointer_record(p, screen.monitor, under),
        );
    }
    let rows: Vec<Value> = items
        .iter()
        .map(|it| {
            let mut row = Map::new();
            row.insert("i".into(), json!(it.index));
            row.insert("text".into(), json!(it.text));
            row.insert("where".into(), json!(screen.region(it)));
            if !it.role.is_empty() {
                row.insert("role".into(), json!(it.role));
            }
            if let Some(h) = hints.get(&it.index) {
                row.insert("when".into(), json!(h));
            }
            if under == Some(it.index) {
                row.insert("under_mouse".into(), json!(true));
            }
            Value::Object(row)
        })
        .collect();
    state.insert("screen_items_in_reading_order".into(), Value::Array(rows));
    if !screen.offscreen.is_empty() {
        state.insert(
            "offscreen_controls".into(),
            Value::Array(offscreen_records(&screen.offscreen)),
        );
    }
    // The capture is one display; the machine is all of them. Without this the loop cannot tell
    // "not on this screen" from "not running", and answers "nothing helps" to both.
    if !screen.windows.is_empty() {
        state.insert(
            "open_windows".into(),
            Value::Array(worldmodel::records(&screen.windows)),
        );
    }
    if !screen.monitors.is_empty() {
        state.insert(
            "displays".into(),
            Value::Array(worldmodel::monitor_summary(&screen.monitors)),
        );
        state.insert("reading_display".into(), json!(screen.monitor + 1));
    }
    Value::Object(state)
}

/// The answers to one step's questions.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub kind: ChoiceAnswer,
    pub item: Option<ChoiceAnswer>,
    pub site: ChoiceAnswer,
    pub offscreen: Option<ChoiceAnswer>,
    pub window: Option<ChoiceAnswer>,
    pub app: Option<ChoiceAnswer>,
}

impl Decision {
    pub fn clicking(&self) -> bool {
        self.kind.choice == "click_item" && self.item.is_some()
    }

    pub fn pressing_offscreen(&self) -> bool {
        self.kind.choice == "press_offscreen" && self.offscreen.is_some()
    }

    pub fn switching(&self) -> bool {
        self.kind.choice == "switch_window" && self.window.is_some()
    }

    pub fn launching(&self) -> bool {
        self.kind.choice == "open_app" && self.app.is_some()
    }

    /// The answer that names a target, if the kind needs one.
    fn target(&self) -> Option<&ChoiceAnswer> {
        if self.clicking() {
            self.item.as_ref()
        } else if self.pressing_offscreen() {
            self.offscreen.as_ref()
        } else if self.switching() {
            self.window.as_ref()
        } else if self.launching() {
            self.app.as_ref()
        } else {
            None
        }
    }

    pub fn chosen(&self) -> String {
        let prefix = if self.pressing_offscreen() {
            OFFSCREEN_PREFIX
        } else if self.switching() {
            WINDOW_PREFIX
        } else if self.launching() {
            APP_PREFIX
        } else {
            ""
        };
        match self.target() {
            Some(t) => format!("{prefix}{}", t.choice),
            None => self.kind.choice.clone(),
        }
    }

    /// Only the answers that name a target lower the confidence: a click, a press, a switch or a
    /// launch lands somewhere, and the wrong somewhere is not undone. use_browser reads the site
    /// answer too, but every outcome of it is a page the next step can leave, so a split there
    /// must not stop the run.
    pub fn confidence(&self) -> f64 {
        match self.target() {
            Some(t) => self.kind.confidence.min(t.confidence),
            None => self.kind.confidence,
        }
    }

    pub fn stops(&self) -> bool {
        STOP_KINDS.contains(&self.kind.choice.as_str())
    }

    /// Whether the decision clears the run's floor (`MIN_CONFIDENCE` unless configured otherwise).
    pub fn clears(&self, min_confidence: f64) -> bool {
        self.confidence() >= min_confidence
    }
}

fn bad(why: String) -> ApiError {
    ApiError {
        status: None,
        message: format!("unexpected TypeSafe response: {why}"),
    }
}

pub(crate) fn find<'a>(answers: &'a [(String, Answer)], name: &str) -> Option<&'a Answer> {
    answers.iter().find(|(n, _)| n == name).map(|(_, a)| a)
}

pub(crate) fn choice_of(answers: &[(String, Answer)], name: &str) -> Option<ChoiceAnswer> {
    match find(answers, name)? {
        Answer::Choice(c) => Some(c.clone()),
        Answer::Noul(_) => None,
    }
}

pub(crate) fn noul_of(answers: &[(String, Answer)], name: &str) -> Result<f64, ApiError> {
    match find(answers, name) {
        Some(Answer::Noul(n)) => Ok(*n),
        _ => Err(bad(format!("no noul answer for {name}"))),
    }
}

fn choice(instructions: &str, criteria: Criteria) -> Question {
    Question::Choice {
        instructions: instructions.into(),
        criteria: criteria.into_iter().map(|(k, v)| (k, Some(v))).collect(),
    }
}

/// The step's questions, in the Python order: kind, site, then item, offscreen, window and app
/// when there is anything to offer for them.
pub fn questions(
    screen: &Screen,
    items: &[Item],
    browser: &str,
    email: Option<&str>,
    sites: Option<&[Site]>,
    apps: &[App],
) -> Vec<(String, Question)> {
    let mut q = vec![
        (
            "kind".to_string(),
            choice(
                "You are driving this computer one action at a time. Which kind of action makes the most \
                 progress toward the goal right now? Do not repeat an action that was just taken unless the \
                 screen changed.",
                kind_criteria(
                    browser,
                    email,
                    !screen.offscreen.is_empty(),
                    !screen.windows.is_empty(),
                    !apps.is_empty(),
                ),
            ),
        ),
        (
            "site".to_string(),
            choice(
                "If the browser is used this step, which website should it show? Name a site from the \
                 list when the goal calls for that one, 'other' when the goal calls for a site the list \
                 does not name, and 'none' to stay on the page that is already open in the browser.",
                site_criteria(sites),
            ),
        ),
    ];
    if !items.is_empty() {
        q.push((
            "item".into(),
            choice(
                "If clicking an on-screen item is the right move, which item? Items marked with a role \
                 come from the app's accessibility tree and are real controls; plain items are text read \
                 from the screen.",
                item_criteria(screen, items),
            ),
        ));
    }
    if !screen.offscreen.is_empty() {
        q.push((
            "offscreen".into(),
            choice(
                "If activating a control that is not on screen is the right move, which control? These \
                 are real controls of the app, reachable without the mouse, but nothing on the capture \
                 points at them.",
                offscreen_criteria(&screen.offscreen),
            ),
        ));
    }
    if !screen.windows.is_empty() {
        q.push((
            "window".into(),
            choice(
                "If switching to a window that is already open is the right move, which window? These \
                 are real windows on this machine, including ones on other monitors and minimized ones, \
                 and switching to one is always better than opening it again.",
                worldmodel::criteria(&screen.windows),
            ),
        ));
    }
    if !apps.is_empty() {
        q.push((
            "app".into(),
            choice(
                "If launching an application is the right move, which one? These are installed on this \
                 machine. Prefer switching to an open window over launching a second copy.",
                app_criteria(apps),
            ),
        ));
    }
    q
}

#[allow(clippy::too_many_arguments)]
pub fn decide(
    client: &dyn Classifier,
    goal: &str,
    screen: &Screen,
    items: &[Item],
    history: &[String],
    browser: &str,
    email: Option<&str>,
    sites: Option<&[Site]>,
    apps: &[App],
) -> Result<Decision, ApiError> {
    let q = questions(screen, items, browser, email, sites, apps);
    let answers = client.system_one(&base_state(goal, screen, items, history), &q)?;
    Ok(Decision {
        kind: choice_of(&answers, "kind").ok_or_else(|| bad("no kind answer".into()))?,
        item: choice_of(&answers, "item"),
        site: choice_of(&answers, "site").ok_or_else(|| bad("no site answer".into()))?,
        offscreen: choice_of(&answers, "offscreen"),
        window: choice_of(&answers, "window"),
        app: choice_of(&answers, "app"),
    })
}

/// Probability that the field now holds a sensible value for its purpose.
pub fn verify_typed(
    client: &dyn Classifier,
    goal: &str,
    field_before: &Field,
    typed: &str,
    field_after: Option<&Field>,
) -> Result<f64, ApiError> {
    let state = json!({
        "goal": goal,
        "field": field_before.summary(),
        "text_typed": typed,
        "field_value_now": field_after.map(|f| f.value.chars().take(300).collect::<String>()),
        "field_still_focused": field_after
            .is_some_and(|f| f.role == field_before.role && f.label == field_before.label),
    });
    let q = vec![(
        "ok".to_string(),
        Question::Noul {
            instructions:
                "Did the typing succeed: does the field now contain the typed text, and is that \
                           text a sensible value for what this field asks for, given the goal?"
                    .into(),
        },
    )];
    noul_of(&client.system_one(&state, &q)?, "ok")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    pub(crate) fn answer(choice: &str, confidence: f64) -> ChoiceAnswer {
        ChoiceAnswer {
            choice: choice.into(),
            confidence,
            probabilities: vec![(choice.into(), confidence)],
        }
    }

    pub(crate) fn screen() -> Screen {
        Screen {
            bgra: vec![0; 2000 * 1200 * 4],
            width: 2000,
            height: 1200,
            scale: 2.0,
            app: "Google Chrome".into(),
            field: None,
            url: None,
            pid: None,
            window: None,
            origin: (0.0, 0.0),
            monitor: 0,
            windows: Vec::new(),
            monitors: Vec::new(),
            ax_refs: Default::default(),
            offscreen: Vec::new(),
            pointer: None,
        }
    }

    pub(crate) fn make_item(index: usize, text: &str, y1: f64, y2: f64) -> Item {
        Item {
            index,
            text: text.into(),
            ocr_confidence: 1.0,
            x1: 100.0,
            y1,
            x2: 400.0,
            y2,
            role: String::new(),
            source: "ocr".into(),
        }
    }

    /// The mouse over the "Settings" row of `pointed_items`, on a display at (1, -1440).
    pub(crate) fn pointed_screen() -> Screen {
        let mut s = screen();
        s.origin = (1.0, -1440.0);
        s.monitor = 1;
        // Item pixels (250, 115) at scale 2: the origin is subtracted once, then scaled.
        s.pointer = Some(crate::models::PointerInfo {
            x: 126.0,
            y: -1382.5,
            monitor: 1,
            idle_seconds: Some(0.4),
            under: Some(platform::uia::Under {
                role: "AXButton".into(),
                label: "Settings".into(),
                x: 126.0,
                y: -1390.0,
                w: 150.0,
                h: 15.0,
            }),
            display_window: None,
        });
        s
    }

    pub(crate) fn pointed_items() -> Vec<Item> {
        vec![
            make_item(0, "File", 10.0, 30.0),
            make_item(1, "Settings", 100.0, 130.0),
        ]
    }

    #[test]
    fn base_state_tells_where_the_mouse_is_and_flags_the_item_under_it() {
        let state = base_state("click this", &pointed_screen(), &pointed_items(), &[]);
        let pointer = &state["pointer"];
        assert_eq!(pointer["display"], 2);
        assert_eq!(pointer["on_display_being_read"], true);
        assert_eq!(pointer["over"], "button 'Settings'");
        assert_eq!(pointer["item_under_mouse"], 1);
        let summary = pointer["summary"].as_str().unwrap();
        assert!(
            summary.contains("the mouse is at (126, -1382)") || summary.contains("(126, -1383)")
        );
        assert!(summary.contains("over button 'Settings'"), "{summary}");
        assert!(pointer["note"].as_str().unwrap().contains("'this'"));
        let rows = state["screen_items_in_reading_order"].as_array().unwrap();
        assert_eq!(rows[1]["under_mouse"], true);
        assert!(rows[0].get("under_mouse").is_none());
        // No mouse: no pointer key and no flags.
        let state = base_state("g", &screen(), &pointed_items(), &[]);
        assert!(state.get("pointer").is_none());
        assert!(!state.to_string().contains("under_mouse"));
    }

    fn d(
        kind: ChoiceAnswer,
        item: Option<ChoiceAnswer>,
        site: ChoiceAnswer,
        offscreen: Option<ChoiceAnswer>,
    ) -> Decision {
        Decision {
            kind,
            item,
            site,
            offscreen,
            window: None,
            app: None,
        }
    }

    fn crit_has(c: &Criteria, k: &str) -> bool {
        c.iter().any(|(key, _)| key == k)
    }

    fn crit_get<'a>(c: &'a Criteria, k: &str) -> &'a str {
        &c.iter().find(|(key, _)| key == k).unwrap().1
    }

    #[test]
    fn decision_click_uses_item_and_min_confidence() {
        let x = d(
            answer("click_item", 0.9),
            Some(answer("12", 0.6)),
            answer("none", 1.0),
            None,
        );
        assert!(x.clicking() && x.chosen() == "12" && x.confidence() == 0.6 && !x.stops());
    }

    #[test]
    fn decision_fixed_action_ignores_item() {
        let x = d(
            answer("use_browser", 0.8),
            Some(answer("3", 0.1)),
            answer("github", 0.9),
            None,
        );
        assert!(!x.clicking() && x.chosen() == "use_browser" && x.confidence() == 0.8);
    }

    #[test]
    fn decision_use_browser_ignores_a_split_site_answer() {
        let x = d(
            answer("use_browser", 0.88),
            None,
            answer("other", 0.45),
            None,
        );
        assert!(x.chosen() == "use_browser" && x.confidence() == 0.88);
    }

    #[test]
    fn decision_stops_on_done_or_none() {
        assert!(d(answer("done", 0.9), None, answer("none", 1.0), None).stops());
        assert!(d(answer("none", 0.9), None, answer("none", 1.0), None).stops());
    }

    #[test]
    fn decision_press_offscreen_uses_the_offscreen_answer_and_min_confidence() {
        let x = d(
            answer("press_offscreen", 0.9),
            Some(answer("3", 0.9)),
            answer("none", 1.0),
            Some(answer("7", 0.5)),
        );
        assert!(x.pressing_offscreen() && !x.clicking());
        assert_eq!(x.chosen(), "offscreen:7");
        assert_eq!(x.confidence(), 0.5);
    }

    #[test]
    fn decision_ignores_an_offscreen_answer_for_any_other_kind() {
        let x = d(
            answer("click_item", 0.9),
            Some(answer("3", 0.8)),
            answer("none", 1.0),
            Some(answer("7", 0.1)),
        );
        assert!(!x.pressing_offscreen() && x.chosen() == "3" && x.confidence() == 0.8);
    }

    #[test]
    fn decision_switch_and_launch_name_their_target_and_lower_confidence() {
        let mut x = d(
            answer("switch_window", 0.9),
            None,
            answer("none", 1.0),
            None,
        );
        x.window = Some(answer("2", 0.4));
        assert_eq!((x.chosen(), x.confidence()), ("window:2".to_string(), 0.4));
        assert!(x.clears(0.4) && !x.clears(0.41));
        let mut y = d(answer("open_app", 0.7), None, answer("none", 1.0), None);
        y.app = Some(answer("whatsapp", 0.95));
        assert_eq!(
            (y.chosen(), y.confidence()),
            ("app:whatsapp".to_string(), 0.7)
        );
        assert!(y.clears(MIN_CONFIDENCE));
    }

    #[test]
    fn kind_criteria_offers_press_offscreen_only_when_there_are_offscreen_controls() {
        assert!(!crit_has(
            &kind_criteria("Google Chrome", None, false, false, false),
            "press_offscreen"
        ));
        assert!(crit_has(
            &kind_criteria("Google Chrome", None, true, false, false),
            "press_offscreen"
        ));
    }

    #[test]
    fn offscreen_criteria_and_records_name_the_role_and_say_it_is_not_visible() {
        let node = |role: &str, label: &str, y: f64| Hit {
            role: role.into(),
            label: label.into(),
            x: 0.0,
            y,
            w: 120.0,
            h: 32.0,
            pressable: true,
            handle: (),
        };
        let nodes = vec![
            node("AXLink", "Register Now", -4200.0),
            node("AXRow", "Note 900", 42718.0),
        ];
        assert_eq!(
            offscreen_criteria(&nodes),
            vec![
                (
                    "0".to_string(),
                    "link 'Register Now' (not visible)".to_string()
                ),
                ("1".to_string(), "cell 'Note 900' (not visible)".to_string()),
            ]
        );
        assert_eq!(
            offscreen_records(&nodes),
            vec![
                json!({"k": 0, "role": "link", "label": "Register Now"}),
                json!({"k": 1, "role": "cell", "label": "Note 900"}),
            ]
        );
        // A screen with no off-screen controls carries no such key. (A populated `Screen.offscreen`
        // holds live UIA handles, so the records are checked through the generic helper above.)
        let state = base_state(
            "buy the thing",
            &screen(),
            &[make_item(0, "Buy", 100.0, 130.0)],
            &[],
        );
        assert!(state.get("offscreen_controls").is_none());
    }

    #[test]
    fn kind_criteria_offers_one_browser_action() {
        let crit = kind_criteria("Google Chrome", None, false, false, false);
        assert!(crit_has(&crit, "use_browser"));
        assert!(!crit_has(&crit, "switch_to_browser") && !crit_has(&crit, "open_site"));
        let ub = crit_get(&crit, "use_browser");
        assert!(ub.contains("Google Chrome") && ub.contains("address bar"));
    }

    #[test]
    fn site_criteria_covers_the_catalog_a_site_outside_it_and_no_site() {
        let crit = site_criteria(None);
        assert_eq!(crit_get(&crit, "github"), SITES[0].1);
        assert!(crit_get(&crit, "other").contains("not one of the sites named in this list"));
        assert!(crit_get(&crit, "none").contains("already open"));
        assert_eq!(site_criteria(Some(&[])), crit);
    }

    #[test]
    fn kind_criteria_offers_email_only_when_set() {
        assert!(!crit_has(
            &kind_criteria("Google Chrome", None, false, false, false),
            "type_email"
        ));
        assert!(crit_has(
            &kind_criteria(
                "Google Chrome",
                Some("user@example.com"),
                false,
                false,
                false
            ),
            "type_email"
        ));
        assert!(crit_has(
            &kind_criteria("Google Chrome", None, false, false, false),
            "click_item"
        ));
    }

    #[test]
    fn kind_criteria_keeps_the_python_order() {
        let keys: Vec<String> = kind_criteria("B", Some("e@x.y"), true, true, true)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            keys,
            [
                "click_item",
                "press_offscreen",
                "switch_window",
                "open_app",
                "use_browser",
                "type_text",
                "press_enter",
                "press_escape",
                "scroll_down",
                "scroll_up",
                "wait",
                "done",
                "none",
                "type_email"
            ]
        );
    }

    #[test]
    fn item_criteria_and_state_carry_region_and_dates() {
        let s = screen();
        let items = vec![
            make_item(0, "Sale ends Oct 1, 2099", 100.0, 130.0),
            make_item(1, "Buy", 140.0, 170.0),
        ];
        let crit = item_criteria(&s, &items);
        assert!(
            crit_get(&crit, "0").starts_with("'Sale ends Oct 1, 2099' (top-left; dated 2099-10-01")
        );
        assert!(crit_get(&crit, "1").contains("near a line dated 2099-10-01"));
        let state = base_state(
            "buy the thing",
            &s,
            &items,
            &["opened https://example.com/".to_string()],
        );
        assert_eq!(state["goal"], "buy the thing");
        assert_eq!(
            state["previous_actions"],
            json!(["opened https://example.com/"])
        );
        assert!(state["screen_items_in_reading_order"][1]["when"]
            .as_str()
            .unwrap()
            .starts_with("near a line dated"));
        assert!(state["now"].get("today").is_some());
    }

    #[test]
    fn an_ax_item_carries_its_role_and_history_keeps_the_last_eight() {
        let s = screen();
        let mut it = make_item(3, "it's", 100.0, 130.0);
        it.role = "button".into();
        it.source = "ax".into();
        assert_eq!(
            crit_get(&item_criteria(&s, &[it.clone()]), "3"),
            "button \"it's\" (top-left)"
        );
        let history: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let state = base_state("g", &s, &[it], &history);
        assert_eq!(
            state["previous_actions"],
            json!(["2", "3", "4", "5", "6", "7", "8", "9"])
        );
        assert_eq!(state["screen_items_in_reading_order"][0]["role"], "button");
        assert_eq!(state["focused_field"], Value::Null);
    }

    /// Records the request and answers each choice question with its first option.
    pub(crate) struct Fake {
        pub state: RefCell<Option<Value>>,
        pub questions: RefCell<Vec<(String, Question)>>,
        pub reply: Vec<(String, Answer)>,
    }

    impl Classifier for Fake {
        fn system_one(
            &self,
            state: &Value,
            questions: &[(String, Question)],
        ) -> Result<Vec<(String, Answer)>, ApiError> {
            *self.state.borrow_mut() = Some(state.clone());
            *self.questions.borrow_mut() = questions.to_vec();
            Ok(self.reply.clone())
        }
    }

    #[test]
    fn decide_asks_kind_site_and_item_and_reads_the_answers() {
        let fake = Fake {
            state: RefCell::new(None),
            questions: RefCell::new(Vec::new()),
            reply: vec![
                ("kind".into(), Answer::Choice(answer("click_item", 0.9))),
                ("site".into(), Answer::Choice(answer("none", 1.0))),
                ("item".into(), Answer::Choice(answer("0", 0.7))),
            ],
        };
        let items = vec![make_item(0, "Buy", 100.0, 130.0)];
        let got = decide(
            &fake,
            "buy",
            &screen(),
            &items,
            &[],
            "Google Chrome",
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(got.chosen(), "0");
        assert_eq!(got.confidence(), 0.7);
        let names: Vec<String> = fake
            .questions
            .borrow()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(names, ["kind", "site", "item"]);
        assert_eq!(fake.state.borrow().as_ref().unwrap()["goal"], "buy");
        // No items: no item question.
        decide(
            &fake,
            "buy",
            &screen(),
            &[],
            &[],
            "Google Chrome",
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(fake.questions.borrow().len(), 2);
    }

    #[test]
    fn decide_without_a_kind_answer_is_an_error() {
        let fake = Fake {
            state: RefCell::new(None),
            questions: RefCell::new(Vec::new()),
            reply: vec![],
        };
        assert!(decide(&fake, "g", &screen(), &[], &[], "B", None, None, &[]).is_err());
    }

    #[test]
    fn verify_typed_reads_the_noul() {
        let fake = Fake {
            state: RefCell::new(None),
            questions: RefCell::new(Vec::new()),
            reply: vec![("ok".into(), Answer::Noul(0.83))],
        };
        let f = Field {
            role: "AXTextField".into(),
            label: "Search".into(),
            placeholder: String::new(),
            value: String::new(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
        let mut after = f.clone();
        after.value = "rust".into();
        assert_eq!(
            verify_typed(&fake, "g", &f, "rust", Some(&after)).unwrap(),
            0.83
        );
        let st = fake.state.borrow().clone().unwrap();
        assert_eq!(st["field_value_now"], "rust");
        assert_eq!(st["field_still_focused"], true);
    }

    #[test]
    #[ignore = "network: makes one real TypeSafe decision"]
    fn live_decision_on_a_fixture_state() {
        let env = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../.env");
        let _ = config::load_dotenv(std::path::Path::new(env));
        let ts = TypeSafe::from_env().expect("TYPESAFE_API_KEY");
        let items = vec![
            make_item(0, "Search", 100.0, 130.0),
            make_item(1, "Sign in", 140.0, 170.0),
            make_item(2, "Images", 180.0, 210.0),
        ];
        let t = std::time::Instant::now();
        let got = decide(
            &ts,
            "open github in the browser",
            &screen(),
            &items,
            &[],
            "Google Chrome",
            None,
            None,
            &[],
        );
        match &got {
            Ok(dn) => println!(
                "LIVE decide kind={} ({:.2}) site={} ({:.2}) chosen={} conf={:.2} in {} ms",
                dn.kind.choice,
                dn.kind.confidence,
                dn.site.choice,
                dn.site.confidence,
                dn.chosen(),
                dn.confidence(),
                t.elapsed().as_millis()
            ),
            Err(e) => println!("LIVE decide failed: status={:?}", e.status),
        }
        assert!(got.is_ok());
    }
}
