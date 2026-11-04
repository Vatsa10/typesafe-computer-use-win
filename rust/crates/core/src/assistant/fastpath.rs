//! The fast path: simple commands that never reach the parent agent.
//!
//! One TypeSafe request (about 0.4 s) reads the utterance against the world: an `action` Choice
//! (open_app, switch_window, switch_tab, open_site, stop, pause, needs_agent) and a `target`
//! Choice over what is actually there (installed apps, open windows, browser tabs, known sites).
//! The model only ever picks from the lists code built, so it cannot invent a target.
//!
//! A fast action runs only when every one of these holds, and anything else goes to the agent:
//!
//! - the action is not `needs_agent`, and its confidence clears the threshold;
//! - the target's confidence clears the threshold too;
//! - the target is an exact match for what was said (the words after the verb equal the app's
//!   name or key, the site's name, or sit inside exactly one window or tab title), so a wrong
//!   guess cannot launch the wrong thing, and "open chrome and search for x" cannot match;
//! - stop and pause are only short lines made of stop-words.
//!
//! The action itself goes through the same tools the agent uses, so Dry and Act mode, the log of
//! what Pointer did and the world refresh are the same. It is written into the conversation as a
//! tool call and its result, so a follow-up sees it, and answered with a fixed line: no model call.
//!
//! `POINTER_FAST_PATH=off` disables it. `POINTER_FAST_THRESHOLD` overrides the confidence bar.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::typesafe::{Answer, ApiError, ChoiceAnswer, Question};
use serde_json::{json, Value};

use super::tasks::Tasks;
use super::tools::{self, Kind, ToolEnv, TurnCtx};
use super::{Memory, Services};
use crate::catalog::Site;
use crate::decide::Classifier;
use crate::world::{CatApp, Snapshot};

/// The bar for both answers. Set from the labelled set (see `fastpath_eval`).
pub const DEFAULT_THRESHOLD: f64 = 0.9;
pub const CLASSIFY_TIMEOUT: Duration = Duration::from_millis(3000);
const MAX_WORDS: usize = 12;
const MAX_APPS: usize = 100;
const MAX_WINDOWS: usize = 30;
const MAX_TABS: usize = 40;
const MAX_SITES: usize = 40;

/// A tool call done for the user: its name, arguments and result, and the line to say.
type Done = (Option<(&'static str, Value, String)>, String);

pub type SitesFn = Arc<dyn Fn(&str) -> Vec<Site> + Send + Sync>;

#[derive(Clone)]
pub struct FastCfg {
    pub enabled: bool,
    pub threshold: f64,
    pub timeout: Duration,
    /// The sites to offer for an utterance. None reads the user's catalog.
    pub sites: Option<SitesFn>,
}

impl FastCfg {
    pub fn from_env() -> Self {
        let off = matches!(
            std::env::var("POINTER_FAST_PATH")
                .ok()
                .map(|v| v.trim().to_lowercase())
                .as_deref(),
            Some("off" | "0" | "false" | "no")
        );
        let threshold = std::env::var("POINTER_FAST_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|t| (0.0..=1.0).contains(t))
            .unwrap_or(DEFAULT_THRESHOLD);
        Self {
            enabled: !off,
            threshold,
            timeout: CLASSIFY_TIMEOUT,
            sites: None,
        }
    }
}

pub struct FastEnv<'a> {
    pub svc: &'a Arc<Services>,
    pub tasks: &'a Arc<Tasks>,
    pub memory: &'a Mutex<Memory>,
    pub classifier: Arc<dyn Classifier + Send + Sync>,
    pub cfg: &'a FastCfg,
    pub cancel: &'a api::chat::Cancel,
}

// ---------------------------------------------------------------- what is there

#[derive(Debug, Clone, Default)]
pub struct FastWorld {
    pub apps: Vec<CatApp>,
    /// (hwnd, app, title, minimized)
    pub windows: Vec<(isize, String, String, bool)>,
    pub tabs: Vec<String>,
    pub sites: Vec<Site>,
    pub foreground: String,
    pub last_spoken: String,
}

impl FastWorld {
    pub fn from_parts(
        apps: &[CatApp],
        snap: Option<&Snapshot>,
        sites: Vec<Site>,
        last_spoken: &str,
        utterance: &str,
    ) -> Self {
        let words = norm_words(utterance);
        // Apps the utterance names come first, so the cap never drops them.
        let mut ranked: Vec<&CatApp> = apps.iter().collect();
        ranked.sort_by_key(|a| {
            let l = norm(&a.label);
            let k = norm(&a.key);
            let hit = words
                .iter()
                .any(|w| w.len() >= 3 && (l.split(' ').any(|x| x == w) || k == *w));
            !hit
        });
        let mut w = Self {
            apps: ranked.into_iter().take(MAX_APPS).cloned().collect(),
            sites: sites.into_iter().take(MAX_SITES).collect(),
            last_spoken: last_spoken.to_string(),
            ..Self::default()
        };
        if let Some(s) = snap {
            for win in s.windows.iter().filter(|x| !x.title.trim().is_empty()) {
                if w.windows.len() < MAX_WINDOWS {
                    w.windows
                        .push((win.hwnd, win.app.clone(), win.title.clone(), win.minimized));
                }
            }
            for b in &s.browsers {
                for t in &b.tabs {
                    if w.tabs.len() < MAX_TABS && !t.title.trim().is_empty() {
                        w.tabs.push(t.title.clone());
                    }
                }
            }
            if let Some(f) = s.focused() {
                w.foreground = format!("{}: {}", f.app, f.title);
            }
        }
        w
    }
}

// ---------------------------------------------------------------- the decision

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    OpenApp(CatApp),
    SwitchWindow { title: String },
    SwitchTab { title: String },
    OpenSite { label: String, url: String },
    Stop,
    Pause,
}

impl Plan {
    pub fn name(&self) -> &'static str {
        match self {
            Plan::OpenApp(_) => "open_app",
            Plan::SwitchWindow { .. } => "switch_window",
            Plan::SwitchTab { .. } => "switch_tab",
            Plan::OpenSite { .. } => "open_site",
            Plan::Stop => "stop",
            Plan::Pause => "pause",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Some only when every condition held.
    pub plan: Option<Plan>,
    /// Why the agent got it, or what was done, for the log.
    pub why: String,
    pub action: String,
    pub action_conf: f64,
    pub target: String,
    pub target_conf: f64,
}

impl Decision {
    fn agent(why: impl Into<String>) -> Self {
        Self {
            plan: None,
            why: why.into(),
            action: String::new(),
            action_conf: 0.0,
            target: String::new(),
            target_conf: 0.0,
        }
    }
}

/// Lowercase words, punctuation dropped (apostrophes too: "what's" is "whats").
pub fn norm_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn norm(text: &str) -> String {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .filter(|c| *c != '\'' && *c != '\u{2019}')
        .collect();
    norm_words(&cleaned).join(" ")
}

const LEAD: [&str; 27] = [
    "please", "hey", "ok", "okay", "pointer", "can", "could", "you", "would", "will", "open",
    "launch", "start", "run", "switch", "go", "to", "the", "my", "show", "me", "bring", "up",
    "focus", "on", "a", "back",
];
const TRAIL: [&str; 8] = [
    "please",
    "now",
    "window",
    "app",
    "tab",
    "application",
    "for",
    "me",
];

/// What the user named: the line without its opening verb and politeness.
pub fn object_phrase(text: &str) -> String {
    let words = norm_words(&norm(text));
    let mut a = 0;
    while a < words.len() && LEAD.contains(&words[a].as_str()) {
        a += 1;
    }
    let mut b = words.len();
    while b > a && TRAIL.contains(&words[b - 1].as_str()) {
        b -= 1;
    }
    words[a..b].join(" ")
}

const STOP_WORDS: [&str; 24] = [
    "stop",
    "it",
    "that",
    "now",
    "please",
    "everything",
    "all",
    "talking",
    "cancel",
    "enough",
    "never",
    "mind",
    "nevermind",
    "hold",
    "on",
    "wait",
    "pause",
    "hey",
    "pointer",
    "okay",
    "ok",
    "quiet",
    "silence",
    "up",
];

fn is_stop_line(text: &str) -> bool {
    let w = norm_words(&norm(text));
    !w.is_empty() && w.len() <= 4 && w.iter().all(|x| STOP_WORDS.contains(&x.as_str()))
}

fn is_pause_word(text: &str) -> bool {
    let w = norm_words(&norm(text));
    w.iter().any(|x| x == "pause" || x == "hold" || x == "wait")
}

/// Does an open window belong to this app? The window's process name equals the app's key, or is
/// a whole word of its label ("Code" in "Visual Studio Code").
fn app_is_open(world: &FastWorld, app: &CatApp) -> bool {
    let (key, label) = (norm(&app.key), norm(&app.label));
    world.windows.iter().any(|(_, proc_, _, _)| {
        let p = norm(proc_.trim_end_matches(".exe"));
        !p.is_empty()
            && (p == key || p == label || (p.len() >= 3 && label.split(' ').any(|w| w == p)))
    })
}

fn action_question() -> Question {
    let c = |k: &str, d: &str| (k.to_string(), Some(d.to_string()));
    Question::Choice {
        instructions: "What does the user want done with this one spoken line? Polite or chatty wording around one simple command (can you, please, hey) does not change it. Choose a direct \
            command only when the line is exactly that one simple command and nothing else. Anything \
            that is a question, needs several steps, needs the screen read, or is unclear is \
            needs_agent."
            .into(),
        criteria: vec![
            c("open_app", "Launch one installed app by name, such as open calculator."),
            c("switch_window", "Bring one already open window to the front, such as switch to the Excel window."),
            c("switch_tab", "Go to one already open browser tab by its title."),
            c("open_site", "Open one named website in the browser, such as open youtube."),
            c("stop", "Stop what Pointer is doing or saying right now: stop, cancel, never mind."),
            c("pause", "Pause: hold on, wait, pause."),
            c("needs_agent", "Everything else: questions, more than one step (open an app and then do something in it), searching, typing, clicking, files, reading the screen, small talk, unclear lines, other requests."),
        ],
    }
}

fn target_question(w: &FastWorld) -> (Question, Vec<(String, String)>) {
    let mut keys: Vec<(String, String)> = Vec::new();
    for (i, a) in w.apps.iter().enumerate() {
        keys.push((format!("app_{i}"), format!("Installed app: {}", a.label)));
    }
    for (i, (_, app, title, min)) in w.windows.iter().enumerate() {
        keys.push((
            format!("window_{i}"),
            format!(
                "Open window: {app}: {title}{}",
                if *min { " (minimized)" } else { "" }
            ),
        ));
    }
    for (i, t) in w.tabs.iter().enumerate() {
        keys.push((format!("tab_{i}"), format!("Open browser tab: {t}")));
    }
    for (i, s) in w.sites.iter().enumerate() {
        keys.push((
            format!("site_{i}"),
            format!("Website: {} ({})", s.label, s.url),
        ));
    }
    keys.push((
        "none".into(),
        "None of the listed items is named by the user.".into(),
    ));
    let q = Question::Choice {
        instructions: "Which listed item does the user's line name? Pick none when the line names \
            nothing in the list, or names more than one thing."
            .into(),
        criteria: keys
            .iter()
            .map(|(k, d)| (k.clone(), Some(d.clone())))
            .collect(),
    };
    (q, keys)
}

fn state_text(text: &str, w: &FastWorld) -> Value {
    let mut s = format!("The user said: {text:?}\n");
    if !w.foreground.is_empty() {
        s.push_str(&format!("In front now: {}\n", w.foreground));
    }
    if !w.last_spoken.is_empty() {
        s.push_str(&format!("Pointer last said: {:?}\n", w.last_spoken));
    }
    Value::String(s)
}

fn choice(answers: &[(String, Answer)], name: &str) -> Option<ChoiceAnswer> {
    answers.iter().find_map(|(n, a)| match a {
        Answer::Choice(c) if n == name => Some(c.clone()),
        _ => None,
    })
}

/// Ask TypeSafe, bounded in time. The classifier runs on its own thread so a slow answer cannot
/// hold the turn.
fn ask(
    classifier: Arc<dyn Classifier + Send + Sync>,
    state: Value,
    questions: Vec<(String, Question)>,
    timeout: Duration,
    cancel: &api::chat::Cancel,
) -> Result<Vec<(String, Answer)>, ApiError> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(classifier.system_one(&state, &questions));
    });
    let end = Instant::now() + timeout;
    loop {
        if cancel.is_cancelled() {
            return Err(ApiError {
                status: None,
                message: "cancelled".into(),
            });
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(r) => return r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= end {
                    return Err(ApiError {
                        status: None,
                        message: "timed out".into(),
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ApiError {
                    status: None,
                    message: "classifier gone".into(),
                })
            }
        }
    }
}

/// Classify and apply every guard. Pure apart from the classifier call.
pub fn decide(
    classifier: Arc<dyn Classifier + Send + Sync>,
    text: &str,
    world: &FastWorld,
    threshold: f64,
    timeout: Duration,
    cancel: &api::chat::Cancel,
) -> Decision {
    if norm_words(text).len() > MAX_WORDS {
        return Decision::agent("long line");
    }
    let (tq, tkeys) = target_question(world);
    let questions = vec![
        ("action".to_string(), action_question()),
        ("target".to_string(), tq),
    ];
    let answers = match ask(
        classifier,
        state_text(text, world),
        questions,
        timeout,
        cancel,
    ) {
        Ok(a) => a,
        Err(e) => return Decision::agent(format!("classifier: {e}")),
    };
    let (Some(act), Some(tgt)) = (choice(&answers, "action"), choice(&answers, "target")) else {
        return Decision::agent("classifier: missing answer");
    };
    let mut d = Decision {
        plan: None,
        why: String::new(),
        action: act.choice.clone(),
        action_conf: act.confidence,
        target: tgt.choice.clone(),
        target_conf: tgt.confidence,
    };
    if act.choice == "needs_agent" {
        d.why = "needs_agent".into();
        return d;
    }
    if act.confidence < threshold {
        d.why = format!(
            "action confidence {:.2} below {threshold:.2}",
            act.confidence
        );
        return d;
    }
    match act.choice.as_str() {
        "stop" | "pause" => {
            if !is_stop_line(text) {
                d.why = "not a bare stop line".into();
                return d;
            }
            // The wording decides between the two; the model only says it is one of them.
            d.plan = Some(
                if is_pause_word(text) && !norm_words(text).contains(&"stop".to_string()) {
                    Plan::Pause
                } else {
                    Plan::Stop
                },
            );
            d.why = "fast".into();
            return d;
        }
        "open_app" | "switch_window" | "switch_tab" | "open_site" => {}
        other => {
            d.why = format!("unknown action {other}");
            return d;
        }
    }
    if tgt.confidence < threshold {
        d.why = format!(
            "target confidence {:.2} below {threshold:.2}",
            tgt.confidence
        );
        return d;
    }
    let Some(label) = tkeys.iter().find(|(k, _)| *k == tgt.choice) else {
        d.why = "target is not an option".into();
        return d;
    };
    let _ = label;
    let phrase = object_phrase(text);
    if phrase.is_empty() {
        d.why = "no target words".into();
        return d;
    }
    let idx =
        |prefix: &str| -> Option<usize> { tgt.choice.strip_prefix(prefix)?.parse::<usize>().ok() };
    let plan = match act.choice.as_str() {
        "open_app" => idx("app_")
            .and_then(|i| world.apps.get(i))
            .filter(|a| phrase == norm(&a.label) || phrase == norm(&a.key))
            // An app that already has a window may be meant as "go to it": the agent decides.
            .filter(|a| !app_is_open(world, a))
            .map(|a| Plan::OpenApp(a.clone())),
        "switch_window" => idx("window_")
            .and_then(|i| world.windows.get(i))
            .filter(|(_, app, title, _)| {
                let hit = |a: &str, t: &str| {
                    phrase == norm(a) || (phrase.len() >= 3 && norm(t).contains(&phrase))
                };
                hit(app, title)
                    && world
                        .windows
                        .iter()
                        .filter(|(_, a2, t2, _)| hit(a2, t2))
                        .count()
                        == 1
            })
            .map(|(_, _, title, _)| Plan::SwitchWindow {
                title: title.clone(),
            }),
        "switch_tab" => idx("tab_")
            .and_then(|i| world.tabs.get(i))
            .filter(|t| {
                phrase.len() >= 3
                    && norm(t).contains(&phrase)
                    && world
                        .tabs
                        .iter()
                        .filter(|t2| norm(t2).contains(&phrase))
                        .count()
                        == 1
            })
            .map(|t| Plan::SwitchTab { title: t.clone() }),
        _ => idx("site_")
            .and_then(|i| world.sites.get(i))
            .filter(|s| phrase == norm(&s.label) || phrase == norm(&s.key))
            .map(|s| Plan::OpenSite {
                label: s.label.clone(),
                url: s.url.clone(),
            }),
    };
    match plan {
        Some(p) => {
            d.plan = Some(p);
            d.why = "fast".into();
        }
        None => d.why = format!("target is not an exact match for {phrase:?}"),
    }
    d
}

// ---------------------------------------------------------------- doing it

fn say(env: &FastEnv, text: &str) {
    let svc = env.svc;
    svc.emit("reply", json!({ "text": text }));
    env.memory
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .last_spoken = text.to_string();
    svc.assistant_state("speaking", "");
    svc.emit("speech", json!({ "action": "start", "text": text }));
    svc.voice.speak(text, env.cancel);
    svc.emit("speech", json!({ "action": "end", "text": text }));
}

fn log(env: &FastEnv, line: String) {
    env.svc.emit("line", json!({ "text": line }));
}

fn run_tool(env: &FastEnv, name: &str, args: &Value) -> String {
    let mut ctx = TurnCtx::default();
    let mut e = ToolEnv {
        svc: env.svc,
        kind: Kind::Foreground,
        label: None,
        cancel: env.cancel,
        tasks: env.tasks,
        memory: env.memory,
        ctx: &mut ctx,
    };
    let out = tools::dispatch(&mut e, name, args);
    if tools::changes_world(name) {
        tools::refresh_world(env.svc, env.memory, true);
    }
    out
}

fn push_turn(messages: &mut Vec<Value>, tool: Option<(&str, &Value, &str)>, reply: &str) {
    if let Some((name, args, result)) = tool {
        messages.push(json!({ "role": "assistant", "content": "", "tool_calls": [
            { "id": "fast-1", "type": "function",
              "function": { "name": name, "arguments": args.to_string() } } ] }));
        messages.push(json!({ "role": "tool", "tool_call_id": "fast-1", "content": result }));
    }
    messages.push(json!({ "role": "assistant", "content": reply }));
}

/// Try the fast path for `text`. True means it was handled: acted (or declined in Dry mode),
/// answered and written into `messages`. False means the agent gets the turn, and nothing was
/// done or recorded.
pub fn try_fast(env: &FastEnv, text: &str, messages: &mut Vec<Value>) -> bool {
    let started = Instant::now();
    tools::refresh_world(env.svc, env.memory, false);
    let world = {
        let m = env.memory.lock().unwrap_or_else(|p| p.into_inner());
        let sites = match &env.cfg.sites {
            Some(f) => f(text),
            None => crate::sitepick::sites_for(text, Some(MAX_SITES)),
        };
        FastWorld::from_parts(
            m.world.catalog(),
            m.world.snapshot(),
            sites,
            &m.last_spoken,
            text,
        )
    };
    let d = decide(
        env.classifier.clone(),
        text,
        &world,
        env.cfg.threshold,
        env.cfg.timeout,
        env.cancel,
    );
    let ms = started.elapsed().as_millis();
    let Some(plan) = d.plan.clone() else {
        log(
            env,
            format!(
                "path: agent ({}; action {} {:.2}, target {} {:.2}; classified in {ms} ms)",
                d.why, d.action, d.action_conf, d.target, d.target_conf
            ),
        );
        return false;
    };
    let act = (env.svc.act_mode)();
    let dry = |what: &str| format!("Dry mode, so I did not {what}.");
    let outcome: Result<Done, String> = match &plan {
        Plan::Stop => {
            env.tasks.stop_all();
            env.svc.voice.stop();
            Ok((None, "Stopped.".into()))
        }
        Plan::Pause => {
            env.svc.voice.stop();
            Ok((None, "Paused.".into()))
        }
        Plan::OpenApp(app) => {
            let args = json!({ "key": app.key });
            let r = run_tool(env, "open_app", &args);
            if r.starts_with("error") {
                Err(r)
            } else {
                let said = if act {
                    format!("Opened {}.", app.label)
                } else {
                    dry(&format!("open {}", app.label))
                };
                Ok((Some(("open_app", args, r)), said))
            }
        }
        Plan::SwitchWindow { title } => {
            let args = json!({ "title": title });
            let r = run_tool(env, "switch_window", &args);
            if r.starts_with("error") {
                Err(r)
            } else {
                let said = if act {
                    "Switched.".to_string()
                } else {
                    dry("switch windows")
                };
                Ok((Some(("switch_window", args, r)), said))
            }
        }
        Plan::SwitchTab { title } => {
            let args = json!({ "title": title });
            let r = run_tool(env, "switch_tab", &args);
            if r.starts_with("error") {
                Err(r)
            } else {
                let said = if act {
                    "Switched.".to_string()
                } else {
                    dry("switch tabs")
                };
                Ok((Some(("switch_tab", args, r)), said))
            }
        }
        Plan::OpenSite { label, url } => {
            let args =
                json!({ "goal": format!("Open {label} ({url}) in the browser. Nothing else.") });
            let r = run_tool(env, "pointer_run", &args);
            if r.starts_with("error") {
                Err(r)
            } else {
                let said = if act {
                    format!("Opened {label}.")
                } else {
                    dry(&format!("open {label}"))
                };
                Ok((Some(("pointer_run", args, r)), said))
            }
        }
    };
    match outcome {
        Err(why) => {
            log(
                env,
                format!(
                    "path: agent (fast {} failed: {}; classified in {ms} ms)",
                    plan.name(),
                    why.chars().take(120).collect::<String>()
                ),
            );
            false
        }
        Ok((tool, reply)) => {
            let call = tool.as_ref().map(|(n, a, r)| (*n, a, r.as_str()));
            push_turn(messages, call, &reply);
            log(
                env,
                format!(
                    "path: fast ({} {}; action {:.2}, target {:.2}; {ms} ms to decide, {} ms in all)",
                    plan.name(),
                    d.target,
                    d.action_conf,
                    d.target_conf,
                    started.elapsed().as_millis()
                ),
            );
            say(env, &reply);
            true
        }
    }
}

#[cfg(test)]
mod eval;
#[cfg(test)]
mod tests;
