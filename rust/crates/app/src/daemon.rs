//! The daemon: one request in, one reply out, plus whatever a hotkey sets off.
//!
//! Every collaborator that touches the world (the loop, the microphone, the voice) is injected, so
//! the dispatch is tested with fakes and never records or registers anything.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use serde_json::{json, Value};
use wcore::runs_index;

use crate::emit::Emitter;
use crate::ipc::{Reply, Request};
use crate::runner::{Events, Runner, Speaker};
use crate::settings;
use crate::voice::Listener;

/// What the panel's toggles set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub act: bool,
    pub voice: bool,
    pub hide: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            act: true,
            voice: true,
            hide: true,
        }
    }
}

/// Types dictated text into whatever has keyboard focus. Injected so tests never type.
pub trait Typer: Send + Sync {
    /// Type `text` into the focused field. Err when it would not be safe to type now.
    fn type_text(&self, text: &str) -> Result<(), String>;
}

/// The real keyboard. Waits for the hotkey's modifiers to come up first: a dictated "a" typed
/// while Ctrl+Alt is still down is a shortcut, not a letter.
pub struct Keyboard;

/// Shift, Ctrl, Alt and both Windows keys.
const MODIFIERS: [u16; 5] = [0x10, 0x11, 0x12, 0x5B, 0x5C];

impl Typer for Keyboard {
    fn type_text(&self, text: &str) -> Result<(), String> {
        use std::time::{Duration, Instant};
        // Settle: the key release that ended the hold has only just happened.
        std::thread::sleep(Duration::from_millis(120));
        let deadline = Instant::now() + Duration::from_secs(2);
        while MODIFIERS.iter().any(|&vk| platform::input::key_held(vk)) {
            if Instant::now() >= deadline {
                return Err("a modifier key is still held, so nothing was typed".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        platform::input::type_text(text);
        Ok(())
    }
}

/// What a transcript is for: a command or goal (`talk`), or text for the focused field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Talk,
    Dictate,
}

impl Purpose {
    /// Anything but "dictate" is talk, so a shell that predates the tag keeps working.
    pub fn from_param(value: Option<&str>) -> Purpose {
        match value {
            Some("dictate") => Purpose::Dictate,
            _ => Purpose::Talk,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Purpose::Talk => "talk",
            Purpose::Dictate => "dictate",
        }
    }
}

/// The refusal while a run is clicking and typing: our keystrokes would land among its own.
pub const DICTATE_WHILE_ACTING: &str = "dictation is off while Pointer is acting";

pub struct Paths {
    pub runs: PathBuf,
    pub dotenv: PathBuf,
}

pub struct Daemon {
    pub runner: Box<dyn Runner>,
    pub listener: Box<dyn Listener>,
    pub out: Emitter,
    pub speaker: Speaker,
    pub paths: Paths,
    pub hotkeys: String,
    /// The push-to-talk key, when the talk hotkey parsed.
    pub talk_vk: Option<u32>,
    /// The push-to-dictate key, when the dictate hotkey parsed.
    pub dictate_vk: Option<u32>,
    pub typer: Box<dyn Typer>,
    pub modes: Mutex<Modes>,
}

fn param_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

/// A run folder name from the panel. Only a bare folder name: no separators, no `..`.
fn safe_name(name: &str) -> Option<&str> {
    let ok = !name.is_empty() && !name.contains(['/', '\\', ':']) && name != "." && name != "..";
    ok.then_some(name)
}

/// The hint for the corner of the window: every binding by name.
pub fn hotkey_hint() -> String {
    ["bar", "talk", "dictate", "pause", "abort"]
        .iter()
        .filter_map(|n| wcore::config::hotkey(n).map(|k| format!("{k} {n}")))
        .collect::<Vec<_>>()
        .join("   ")
}

impl Daemon {
    pub fn events(&self) -> Events {
        Events::new(self.out.clone(), self.speaker.clone(), &self.hotkeys)
    }

    fn modes(&self) -> Modes {
        *self.modes.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn state_value(&self) -> Value {
        let s = self.runner.state();
        json!({ "running": s.running, "paused": s.paused, "hotkeys": self.hotkeys,
                "stt": self.listener.engine(),
                "stt_lang": std::env::var("CLICKER_STT_LANG").ok().filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "en-US".into()) })
    }

    fn emit_state(&self) {
        self.events().state(self.runner.state());
    }

    pub fn handle(&self, request: &Request) -> Reply {
        match self.dispatch(&request.method, &request.params) {
            Ok(result) => Reply::ok(request.id, result),
            Err(error) => Reply::failed(request.id, error),
        }
    }

    pub fn dispatch(&self, method: &str, params: &Value) -> Result<Value, String> {
        match method {
            "state" => Ok(self.state_value()),
            "displays" => Ok(displays()),
            "capture" => capture(params),
            "runs" => Ok(self.runs()),
            "steps" => self.steps(params),
            "shot" => self.shot(params),
            "settings" => {
                let current = wcore::config::read_env(&self.paths.dotenv)
                    .map_err(|e| format!("could not read settings: {e}"))?;
                Ok(json!(settings::rows(&current)))
            }
            "save_settings" => {
                let values = params
                    .get("values")
                    .and_then(Value::as_object)
                    .ok_or("save_settings needs { values: { KEY: value } }")?;
                let written = settings::save(&self.paths.dotenv, values)?;
                Ok(json!({ "saved": true, "keys": written }))
            }
            "set_mode" => Ok(self.set_mode(params)),
            "listen_start" => {
                if !self.modes().voice {
                    return Err("voice is off".into());
                }
                Ok(json!({ "listening": self.listener.start()? }))
            }
            "listen_stop" => Ok(json!({ "heard": self.listener.stop()? })),
            "heard" => {
                // What the shell's browser recognizer heard: routed exactly as `say`, unless it
                // was dictation, which is typed and never read as a command.
                if Purpose::from_param(param_str(params, "purpose")) == Purpose::Dictate {
                    // A failure is a line too: the shell's reply is not where people look.
                    let typed = self
                        .dictate(param_str(params, "text").unwrap_or(""))
                        .inspect_err(|e| self.out.line(format!("dictate: {e}")))?;
                    return Ok(json!({ "typed": typed }));
                }
                let text = param_str(params, "text")
                    .map(str::trim)
                    .filter(|t| !t.is_empty());
                let text = text.ok_or("heard needs text")?;
                self.runner
                    .route_heard(text, self.modes().act, self.events())
            }
            "pause" => {
                let paused = self.runner.pause()?;
                self.emit_state();
                Ok(json!({ "paused": paused }))
            }
            "abort" => {
                let aborted = self.runner.abort()?;
                self.emit_state();
                Ok(json!({ "ok": aborted }))
            }
            "start" => {
                let goal = param_str(params, "goal")
                    .map(str::trim)
                    .filter(|g| !g.is_empty());
                let goal = goal.ok_or("start needs a goal")?;
                // act null means "whatever the panel's mode says": the bar has no toggle of its own.
                let act = params
                    .get("act")
                    .and_then(Value::as_bool)
                    .unwrap_or(self.modes().act);
                let queued = self.runner.start(goal, act, self.events())?;
                Ok(json!({ "queued": queued }))
            }
            "say" => {
                let text = param_str(params, "text")
                    .map(str::trim)
                    .filter(|t| !t.is_empty());
                let text = text.ok_or("say needs text")?;
                self.runner
                    .route_said(text, self.modes().act, self.events())
            }
            "ask" => {
                let question = param_str(params, "question")
                    .map(str::trim)
                    .filter(|q| !q.is_empty());
                let answer = self
                    .runner
                    .ask(question.ok_or("ask needs a question")?, self.events())?;
                Ok(json!({ "answer": answer }))
            }
            other => Err(format!("no method {other}")),
        }
    }

    fn set_mode(&self, params: &Value) -> Value {
        let mut modes = self.modes.lock().unwrap_or_else(|p| p.into_inner());
        let flag = |key: &str, now: bool| params.get(key).and_then(Value::as_bool).unwrap_or(now);
        *modes = Modes {
            act: flag("act", modes.act),
            voice: flag("voice", modes.voice),
            hide: flag("hide", modes.hide),
        };
        json!({ "act": modes.act, "voice": modes.voice, "hide": modes.hide })
    }

    fn runs(&self) -> Value {
        let rows: Vec<Value> = runs_index::list_runs(&self.paths.runs)
            .into_iter()
            .map(|r| {
                json!({ "name": r.name, "goal": r.goal, "outcome": r.outcome, "answer": r.answer,
                        "goal_achieved": r.goal_achieved, "seconds": r.seconds,
                        "steps_taken": r.steps_taken, "acted": r.acted })
            })
            .collect();
        json!(rows)
    }

    fn run_dir(&self, params: &Value) -> Result<PathBuf, String> {
        let name = param_str(params, "name")
            .and_then(safe_name)
            .ok_or("needs a run name")?;
        Ok(self.paths.runs.join(name))
    }

    fn steps(&self, params: &Value) -> Result<Value, String> {
        let rows: Vec<Value> = runs_index::steps_of(&self.run_dir(params)?)
            .into_iter()
            .map(|s| json!({ "number": s.number, "has_shot": s.annotated.is_some() || s.raw.is_some() }))
            .collect();
        Ok(json!(rows))
    }

    fn shot(&self, params: &Value) -> Result<Value, String> {
        let dir = self.run_dir(params)?;
        let number = params
            .get("number")
            .and_then(Value::as_u64)
            .ok_or("shot needs a step number")?;
        let step = runs_index::steps_of(&dir)
            .into_iter()
            .find(|s| s.number == number);
        let path = step.and_then(|s| s.annotated.or(s.raw));
        Ok(json!({ "data_url": path.as_deref().map(data_url).unwrap_or_default() }))
    }

    /// A hotkey fired. Runs on the input worker, never on the pump.
    pub fn on_hotkey(&self, name: &str) {
        self.out.event("hotkey", json!({ "name": name }));
        let result = match name {
            "pause" => self
                .runner
                .pause()
                .map(|p| self.out.line(if p { "paused" } else { "resumed" })),
            "abort" | "quit" => self.runner.abort().map(|a| {
                self.out.line(if a {
                    "aborting the run"
                } else {
                    "nothing to abort"
                })
            }),
            "talk" => self.talk(),
            "dictate" => self.dictate_held(),
            _ => Ok(()), // bar and goal open windows, which is the shell's job
        };
        match result {
            Ok(()) => self.emit_state(),
            Err(e) => self.out.line(format!("{name}: {e}")),
        }
    }

    fn talk(&self) -> Result<(), String> {
        if !self.modes().voice {
            return Err("voice is off".into());
        }
        let vk = self.talk_vk.ok_or("the talk hotkey did not parse")?;
        let Some(said) = self.record_held(vk, Purpose::Talk)? else {
            return Ok(());
        };
        if said.is_empty() {
            self.out.line("heard nothing");
            return Ok(());
        }
        self.out.line(format!("heard: {said}"));
        self.runner
            .route_said(&said, self.modes().act, self.events())
            .map(|_| ())
    }

    /// Hold the key on `purpose`'s behalf. None on chrome, where the shell records between the
    /// `stt` start and stop and answers with `heard`; otherwise what the core heard.
    fn record_held(&self, vk: u32, purpose: Purpose) -> Result<Option<String>, String> {
        if self.listener.engine() == "chrome" {
            let tag = purpose.name();
            self.out
                .event("stt", json!({ "action": "start", "purpose": tag }));
            self.listener.hold(vk);
            self.out
                .event("stt", json!({ "action": "stop", "purpose": tag }));
            return Ok(None);
        }
        self.listener.while_held(vk).map(Some)
    }

    /// Push-to-dictate: record while held, then type into the focused field.
    fn dictate_held(&self) -> Result<(), String> {
        if !self.modes().voice {
            return Err("voice is off".into());
        }
        let vk = self.dictate_vk.ok_or("the dictate hotkey did not parse")?;
        self.refuse_while_acting()?;
        if let Some(said) = self.record_held(vk, Purpose::Dictate)? {
            self.dictate(&said)?;
        }
        Ok(())
    }

    fn refuse_while_acting(&self) -> Result<(), String> {
        if self.runner.state().running && self.modes().act {
            return Err(DICTATE_WHILE_ACTING.into());
        }
        Ok(())
    }

    /// Type dictated text, never routing it. Returns how many characters went in. The text is
    /// not echoed to the log: it may be private.
    fn dictate(&self, text: &str) -> Result<usize, String> {
        let text = text.trim();
        if text.is_empty() {
            self.out.line("heard nothing to dictate");
            return Ok(0);
        }
        self.refuse_while_acting()?;
        // A trailing space, so the next dictation joins on as a new word.
        let typed = format!("{text} ");
        self.typer.type_text(&typed)?;
        let count = typed.chars().count();
        self.out.line(format!("dictated {count} characters"));
        Ok(count)
    }
}

/// A capture as something an `<img>` can show.
pub fn data_url(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        Err(_) => String::new(),
    }
}

fn displays() -> Value {
    let screens: Vec<_> = platform::display::monitors()
        .into_iter()
        .map(|m| {
            json!({ "index": m.index, "left": m.left, "top": m.top,
                    "right": m.right, "bottom": m.bottom, "primary": m.primary })
        })
        .collect();
    json!(screens)
}

fn capture(params: &Value) -> Result<Value, String> {
    // Not always the primary: the display being worked on is the one that matters.
    let wanted = params.get("monitor").and_then(Value::as_u64).unwrap_or(0) as usize;
    let screens = platform::display::monitors();
    let screen = screens
        .get(wanted)
        .ok_or_else(|| format!("no display {wanted}"))?;
    let shot = platform::capture::capture_monitor(screen).ok_or("the display would not capture")?;
    Ok(json!({ "width": shot.width, "height": shot.height,
               "origin": [shot.origin.0, shot.origin.1], "bytes": shot.bgra.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::Captured;
    use crate::runner::{RunState, Unwired, NOT_WIRED};
    use crate::voice::IN_SHELL;
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<String>>,
        paused: Mutex<bool>,
        /// Inverted so the default fake reports a run in progress, as it always has.
        idle: bool,
    }

    /// Records what would have been typed.
    #[derive(Clone, Default)]
    struct FakeTyper(Arc<Mutex<Vec<String>>>);
    impl Typer for FakeTyper {
        fn type_text(&self, text: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(text.into());
            Ok(())
        }
    }
    impl FakeTyper {
        fn typed(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn start(&self, goal: &str, act: bool, events: Events) -> Result<bool, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start {goal} {act}"));
            events.answer("done");
            Ok(true)
        }
        fn pause(&self) -> Result<bool, String> {
            let mut p = self.paused.lock().unwrap();
            *p = !*p;
            Ok(*p)
        }
        fn abort(&self) -> Result<bool, String> {
            self.calls.lock().unwrap().push("abort".into());
            Ok(true)
        }
        fn route_said(&self, text: &str, _: bool, _: Events) -> Result<Value, String> {
            self.calls.lock().unwrap().push(format!("said {text}"));
            Ok(json!({ "command": "run_goal" }))
        }
        fn ask(&self, q: &str, _: Events) -> Result<String, String> {
            Ok(format!("about {q}"))
        }
        fn state(&self) -> RunState {
            RunState {
                running: !self.idle,
                paused: *self.paused.lock().unwrap(),
            }
        }
    }

    struct FakeMic;
    impl Listener for FakeMic {
        fn start(&self) -> Result<bool, String> {
            Ok(true)
        }
        fn stop(&self) -> Result<String, String> {
            Ok("open Claude Code".into())
        }
        fn while_held(&self, _: u32) -> Result<String, String> {
            Ok("stop".into())
        }
        fn engine(&self) -> String {
            "openai".into()
        }
        fn hold(&self, _: u32) {}
    }

    struct ShellMic;
    impl Listener for ShellMic {
        fn start(&self) -> Result<bool, String> {
            Err(IN_SHELL.into())
        }
        fn stop(&self) -> Result<String, String> {
            Err(IN_SHELL.into())
        }
        fn while_held(&self, _: u32) -> Result<String, String> {
            Err(IN_SHELL.into())
        }
        fn engine(&self) -> String {
            "chrome".into()
        }
        fn hold(&self, _: u32) {}
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("app-daemon-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn daemon(runner: Box<dyn Runner>, dir: &Path) -> (Daemon, Captured) {
        let (d, captured, _) = daemon_typing(runner, dir);
        (d, captured)
    }

    fn daemon_typing(runner: Box<dyn Runner>, dir: &Path) -> (Daemon, Captured, FakeTyper) {
        let captured = Captured::default();
        let typer = FakeTyper::default();
        let d = Daemon {
            runner,
            listener: Box::new(FakeMic),
            out: Emitter::new(captured.clone()),
            speaker: Arc::new(|_| false),
            paths: Paths {
                runs: dir.join("runs"),
                dotenv: dir.join(".env"),
            },
            hotkeys: "rightalt bar".into(),
            talk_vk: Some(0x20),
            dictate_vk: Some(0x44),
            typer: Box::new(typer.clone()),
            modes: Mutex::new(Modes::default()),
        };
        (d, captured, typer)
    }

    fn idle_runner() -> Box<dyn Runner> {
        Box::new(FakeRunner {
            idle: true,
            ..FakeRunner::default()
        })
    }

    fn lines(captured: &Captured) -> Vec<String> {
        captured
            .messages()
            .into_iter()
            .filter(|m| m["event"] == "line")
            .map(|m| m["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn dictated_text_is_typed_and_never_routed() {
        let dir = tmp("dictate");
        let (d, captured, typer) = daemon_typing(idle_runner(), &dir);
        let reply = d
            .dispatch(
                "heard",
                &json!({ "text": " hello there ", "purpose": "dictate" }),
            )
            .unwrap();
        assert_eq!(reply, json!({ "typed": 12 }));
        // "stop" dictated is words for the field, not a command for the run.
        d.dispatch("heard", &json!({ "text": "stop", "purpose": "dictate" }))
            .unwrap();
        assert_eq!(typer.typed(), vec!["hello there ", "stop "]);
        assert_eq!(
            lines(&captured),
            vec!["dictated 12 characters", "dictated 5 characters"]
        );
        assert!(
            !captured
                .messages()
                .iter()
                .any(|m| m.to_string().contains("hello")),
            "the dictated text itself is never logged"
        );
    }

    #[test]
    fn talk_purpose_still_routes_and_types_nothing() {
        let dir = tmp("talkpurpose");
        let (d, _, typer) = daemon_typing(idle_runner(), &dir);
        for params in [
            json!({ "text": "stop", "purpose": "talk" }),
            json!({ "text": "stop" }),
        ] {
            assert_eq!(
                d.dispatch("heard", &params).unwrap(),
                json!({ "command": "run_goal" })
            );
        }
        assert!(typer.typed().is_empty());
    }

    #[test]
    fn empty_dictation_types_nothing() {
        let dir = tmp("dictempty");
        let (d, _, typer) = daemon_typing(idle_runner(), &dir);
        assert_eq!(
            d.dispatch("heard", &json!({ "text": "  ", "purpose": "dictate" }))
                .unwrap(),
            json!({ "typed": 0 })
        );
        assert!(typer.typed().is_empty());
    }

    #[test]
    fn dictation_is_refused_while_a_run_acts_but_not_in_dry_run() {
        let dir = tmp("dictacting");
        // The default fake reports a run in progress; the panel defaults to Act.
        let (d, captured, typer) = daemon_typing(Box::new(FakeRunner::default()), &dir);
        let err = d
            .dispatch("heard", &json!({ "text": "hi", "purpose": "dictate" }))
            .unwrap_err();
        assert_eq!(err, DICTATE_WHILE_ACTING);
        d.on_hotkey("dictate");
        assert!(typer.typed().is_empty());
        let said = lines(&captured);
        assert!(said.contains(&format!("dictate: {DICTATE_WHILE_ACTING}")));
        // A dry run touches nothing, so dictation goes ahead beside it.
        d.dispatch("set_mode", &json!({ "act": false })).unwrap();
        d.dispatch("heard", &json!({ "text": "hi", "purpose": "dictate" }))
            .unwrap();
        assert_eq!(typer.typed(), vec!["hi "]);
    }

    struct Shared(Arc<FakeRunner>);
    impl Runner for Shared {
        fn start(&self, g: &str, a: bool, e: Events) -> Result<bool, String> {
            self.0.start(g, a, e)
        }
        fn pause(&self) -> Result<bool, String> {
            self.0.pause()
        }
        fn abort(&self) -> Result<bool, String> {
            self.0.abort()
        }
        fn route_said(&self, t: &str, a: bool, e: Events) -> Result<Value, String> {
            self.0.route_said(t, a, e)
        }
        fn ask(&self, q: &str, e: Events) -> Result<String, String> {
            self.0.ask(q, e)
        }
        fn state(&self) -> RunState {
            self.0.state()
        }
    }

    #[test]
    fn the_dictate_hotkey_records_and_types_on_the_core_engines() {
        let dir = tmp("dicthotkey");
        let runner = Arc::new(FakeRunner {
            idle: true,
            ..FakeRunner::default()
        });
        let (d, _, typer) = daemon_typing(Box::new(Shared(runner.clone())), &dir);
        // FakeMic hears "stop": typed, not obeyed.
        d.on_hotkey("dictate");
        assert_eq!(typer.typed(), vec!["stop "]);
        assert!(
            runner.calls.lock().unwrap().is_empty(),
            "nothing was routed"
        );
    }

    #[test]
    fn on_chrome_the_stt_events_carry_the_purpose() {
        let dir = tmp("chromepurpose");
        let (mut d, captured, typer) = daemon_typing(idle_runner(), &dir);
        d.listener = Box::new(ShellMic);
        d.on_hotkey("dictate");
        d.on_hotkey("talk");
        let stt: Vec<_> = captured
            .messages()
            .into_iter()
            .filter(|m| m["event"] == "stt")
            .map(|m| (m["action"].clone(), m["purpose"].clone()))
            .collect();
        assert_eq!(
            stt,
            vec![
                (json!("start"), json!("dictate")),
                (json!("stop"), json!("dictate")),
                (json!("start"), json!("talk")),
                (json!("stop"), json!("talk")),
            ]
        );
        assert!(
            typer.typed().is_empty(),
            "the shell sends the text back as heard"
        );
    }

    #[test]
    fn the_unwired_runner_says_so_plainly() {
        let dir = tmp("unwired");
        let (d, _) = daemon(Box::new(Unwired), &dir);
        for (method, params) in [
            ("start", json!({ "goal": "open youtube", "act": true })),
            ("say", json!({ "text": "stop" })),
            ("pause", json!({})),
            ("abort", json!({})),
        ] {
            assert_eq!(
                d.dispatch(method, &params).unwrap_err(),
                NOT_WIRED,
                "{method}"
            );
        }
        let state = d.dispatch("state", &Value::Null).unwrap();
        assert_eq!(
            state,
            json!({ "running": false, "paused": false, "hotkeys": "rightalt bar", "stt": "openai",
                    "stt_lang": std::env::var("CLICKER_STT_LANG").unwrap_or_else(|_| "en-US".into()) })
        );
    }

    #[test]
    fn start_uses_the_panel_mode_when_act_is_null_and_answers_reach_the_shell() {
        let dir = tmp("start");
        let (d, captured) = daemon(Box::new(FakeRunner::default()), &dir);
        d.dispatch(
            "set_mode",
            &json!({ "act": false, "voice": true, "hide": false }),
        )
        .unwrap();
        let reply = d.handle(&Request {
            id: 4,
            method: "start".into(),
            params: json!({ "goal": " go ", "act": null }),
        });
        assert!(reply.ok);
        let answers: Vec<_> = captured
            .messages()
            .into_iter()
            .filter(|m| m["event"] == "answer")
            .collect();
        assert_eq!(
            answers,
            vec![json!({ "event": "answer", "text": "done", "spoken": false })]
        );
        assert!(d.dispatch("start", &json!({ "goal": "  " })).is_err());
    }

    #[test]
    fn pause_reports_and_emits_state() {
        let dir = tmp("pause");
        let (d, captured) = daemon(Box::new(FakeRunner::default()), &dir);
        assert_eq!(
            d.dispatch("pause", &json!({})).unwrap(),
            json!({ "paused": true })
        );
        let state = captured
            .messages()
            .into_iter()
            .find(|m| m["event"] == "state")
            .unwrap();
        assert_eq!(state["running"], true);
        assert_eq!(state["paused"], true);
    }

    #[test]
    fn listening_returns_what_was_heard_and_respects_voice_off() {
        let dir = tmp("listen");
        let (d, _) = daemon(Box::new(FakeRunner::default()), &dir);
        assert_eq!(
            d.dispatch("listen_start", &json!({})).unwrap(),
            json!({ "listening": true })
        );
        assert_eq!(
            d.dispatch("listen_stop", &json!({})).unwrap(),
            json!({ "heard": "open Claude Code" })
        );
        d.dispatch("set_mode", &json!({ "voice": false })).unwrap();
        assert!(d.dispatch("listen_start", &json!({})).is_err());
    }

    #[test]
    fn a_hotkey_is_announced_and_talk_routes_what_was_heard() {
        let dir = tmp("hotkey");
        let (d, captured) = daemon(Box::new(FakeRunner::default()), &dir);
        d.on_hotkey("bar");
        d.on_hotkey("talk");
        let messages = captured.messages();
        assert_eq!(messages[0], json!({ "event": "hotkey", "name": "bar" }));
        assert!(messages.iter().any(|m| m["text"] == "heard: stop"));
    }

    #[test]
    fn on_chrome_talk_asks_the_shell_to_record_and_heard_routes_like_say() {
        let dir = tmp("chrome");
        let (mut d, captured) = daemon(Box::new(FakeRunner::default()), &dir);
        d.listener = Box::new(ShellMic);
        assert_eq!(d.dispatch("state", &Value::Null).unwrap()["stt"], "chrome");
        d.on_hotkey("talk");
        let stt: Vec<_> = captured
            .messages()
            .into_iter()
            .filter(|m| m["event"] == "stt")
            .map(|m| m["action"].clone())
            .collect();
        assert_eq!(stt, vec![json!("start"), json!("stop")]);
        assert_eq!(
            d.dispatch("heard", &json!({ "text": " open youtube " }))
                .unwrap(),
            json!({ "command": "run_goal" })
        );
        assert!(d.dispatch("heard", &json!({ "text": "  " })).is_err());
        assert!(d.dispatch("listen_start", &json!({})).is_err());
    }

    #[test]
    fn unknown_methods_and_path_escapes_fail() {
        let dir = tmp("escape");
        let (d, _) = daemon(Box::new(Unwired), &dir);
        assert_eq!(
            d.dispatch("nope", &Value::Null).unwrap_err(),
            "no method nope"
        );
        assert!(d.dispatch("steps", &json!({ "name": "../.." })).is_err());
        assert!(d
            .dispatch("shot", &json!({ "name": "a/b", "number": 1 }))
            .is_err());
    }

    #[test]
    fn runs_steps_and_shots_come_from_the_run_folders() {
        let dir = tmp("runs");
        let run = dir.join("runs").join("20260101-120000");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("step-001.png"), [0x89, b'P', b'N', b'G']).unwrap();
        let (d, _) = daemon(Box::new(Unwired), &dir);
        let runs = d.dispatch("runs", &Value::Null).unwrap();
        assert_eq!(runs[0]["name"], "20260101-120000");
        let steps = d
            .dispatch("steps", &json!({ "name": "20260101-120000" }))
            .unwrap();
        assert_eq!(steps, json!([{ "number": 1, "has_shot": true }]));
        let shot = d
            .dispatch("shot", &json!({ "name": "20260101-120000", "number": 1 }))
            .unwrap();
        assert_eq!(shot["data_url"], "data:image/png;base64,iVBORw==");
        let missing = d
            .dispatch("shot", &json!({ "name": "20260101-120000", "number": 9 }))
            .unwrap();
        assert_eq!(missing["data_url"], "");
    }

    #[test]
    fn settings_never_echo_a_secret() {
        let dir = tmp("settings");
        std::fs::write(dir.join(".env"), "TYPESAFE_API_KEY=ts-hush\n").unwrap();
        let (d, _) = daemon(Box::new(Unwired), &dir);
        let rows = d.dispatch("settings", &Value::Null).unwrap();
        assert!(!rows.to_string().contains("ts-hush"));
        let saved = d
            .dispatch(
                "save_settings",
                &json!({ "values": { "TYPESAFE_API_KEY": "", "CLICKER_BROWSER": "Edge" } }),
            )
            .unwrap();
        assert!(!saved.to_string().contains("Edge"), "values are not echoed");
        let text = std::fs::read_to_string(dir.join(".env")).unwrap();
        assert!(text.contains("TYPESAFE_API_KEY=ts-hush") && text.contains("CLICKER_BROWSER=Edge"));
    }
}
