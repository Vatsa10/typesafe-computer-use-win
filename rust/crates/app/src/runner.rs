//! The boundary between the daemon and the loop. The daemon owns the pipe, the hotkeys and the
//! microphone; whatever drives a run sits behind [`Runner`] and talks back through [`Events`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};

use crate::emit::Emitter;

#[cfg(test)]
pub const NOT_WIRED: &str = "runner not wired yet";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RunState {
    pub running: bool,
    pub paused: bool,
}

/// A rectangle to point at, in PHYSICAL pixels on the virtual desktop: what the core captures and
/// clicks in. The shell converts per monitor; the core never thinks in DIPs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mark {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Reads an answer aloud. Returns whether it was spoken.
pub type Speaker = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// What a run can say to the shell. Cloneable so a run thread can keep its own copy.
#[derive(Clone)]
pub struct Events {
    out: Emitter,
    speaker: Speaker,
    hotkeys: Arc<str>,
}

impl Events {
    pub fn new(out: Emitter, speaker: Speaker, hotkeys: &str) -> Self {
        Self {
            out,
            speaker,
            hotkeys: Arc::from(hotkeys),
        }
    }

    pub fn line(&self, text: impl Into<String>) {
        self.out.line(text);
    }

    /// A run started, paused, resumed or ended.
    pub fn state(&self, state: RunState) {
        self.out.event(
            "state",
            json!({ "running": state.running, "paused": state.paused, "hotkeys": &*self.hotkeys }),
        );
    }

    /// An answer is ready: read it aloud when that is on, and tell the shell whether it was.
    pub fn answer(&self, text: &str) {
        let spoken = (self.speaker)(text);
        self.out
            .event("answer", json!({ "text": text, "spoken": spoken }));
    }

    pub fn highlight(&self, marks: &[Mark], seconds: f64) {
        self.out
            .event("highlight", json!({ "marks": marks, "seconds": seconds }));
    }
}

/// The loop, as the daemon sees it. Every method returns at once: a run happens on the runner's
/// own thread and reports through the [`Events`] it was handed.
pub trait Runner: Send + Sync {
    /// Queue a goal. `act` false is a dry run: decide and report, touch nothing.
    fn start(&self, goal: &str, act: bool, events: Events) -> Result<bool, String>;
    /// Toggle pause; returns whether it is now paused.
    fn pause(&self) -> Result<bool, String>;
    /// Abort the current run; returns whether there was one.
    fn abort(&self) -> Result<bool, String>;
    /// One spoken line: classified (goal, stop, pause, question...) and acted on.
    fn route_said(&self, text: &str, act: bool, events: Events) -> Result<Value, String>;
    /// Talk mode: look at the screen and answer. Touches nothing.
    fn ask(&self, question: &str, events: Events) -> Result<String, String>;
    fn state(&self) -> RunState;
}

/// The placeholder the dispatch tests use: every action says so plainly.
#[cfg(test)]
pub struct Unwired;

#[cfg(test)]
impl Runner for Unwired {
    fn start(&self, _: &str, _: bool, _: Events) -> Result<bool, String> {
        Err(NOT_WIRED.into())
    }
    fn pause(&self) -> Result<bool, String> {
        Err(NOT_WIRED.into())
    }
    fn abort(&self) -> Result<bool, String> {
        Err(NOT_WIRED.into())
    }
    fn route_said(&self, _: &str, _: bool, _: Events) -> Result<Value, String> {
        Err(NOT_WIRED.into())
    }
    fn ask(&self, _: &str, _: Events) -> Result<String, String> {
        Err(NOT_WIRED.into())
    }
    fn state(&self) -> RunState {
        RunState::default()
    }
}

// ---------------------------------------------------------------- the wired runner

/// The core's events, forwarded to the shell.
pub struct Bridge<'a>(pub &'a Events);

impl wcore::runner::Events for Bridge<'_> {
    fn line(&self, text: &str) {
        self.0.line(text);
    }
    fn highlight(&self, marks: Vec<wcore::runner::Mark>, seconds: f64) {
        let marks: Vec<Mark> = marks
            .into_iter()
            .map(|m| Mark {
                x: m.x,
                y: m.y,
                w: m.w,
                h: m.h,
                label: (!m.label.is_empty()).then_some(m.label),
            })
            .collect();
        self.0.highlight(&marks, seconds);
    }
    fn answer(&self, text: &str) {
        self.0.answer(text);
    }
    fn state(&self, running: bool, paused: bool) {
        self.0.state(RunState { running, paused });
    }
}

/// One queued goal.
pub struct Job {
    pub goal: String,
    pub act: bool,
    pub events: Events,
}

/// Drives one job to its end on the run thread. The real one builds the screen, the hands and the
/// clients right there, because the screen's handles are bound to the thread that fetched them.
pub type Execute = Box<dyn Fn(&Job, &wcore::runner::Control) + Send>;
/// Classifies one spoken line: (text, running, paused).
pub type Classify =
    Box<dyn Fn(&str, bool, bool) -> Result<wcore::intent::Intent, String> + Send + Sync>;
/// Looks at the screen and answers, emitting `answer` itself. Touches nothing.
pub type Answerer = Box<dyn Fn(&str, &Events) -> Result<String, String> + Send + Sync>;

/// The loop behind the daemon: one dedicated run thread and one shared control, which the hotkeys
/// reach through [`Runner::pause`] and [`Runner::abort`].
pub struct Wired {
    control: wcore::runner::Control,
    running: Arc<AtomicBool>,
    jobs: Mutex<mpsc::Sender<Job>>,
    classify: Classify,
    answer: Answerer,
}

impl Wired {
    pub fn new(execute: Execute, classify: Classify, answer: Answerer) -> Self {
        let control = wcore::runner::Control::new();
        let running = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Job>();
        let (c, r) = (control.clone(), running.clone());
        std::thread::Builder::new()
            .name("run".into())
            .spawn(move || {
                for job in rx {
                    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        execute(&job, &c)
                    }));
                    r.store(false, Ordering::SeqCst);
                    if ran.is_err() {
                        job.events.line("the run crashed");
                    }
                    // Always end on idle, even when the run never got far enough to say so.
                    job.events.state(RunState::default());
                }
            })
            .expect("the run thread would not start");
        Self {
            control,
            running,
            jobs: Mutex::new(tx),
            classify,
            answer,
        }
    }

    /// The real collaborators: the live desktop, TypeSafe for deciding, OpenAI for writing.
    pub fn live() -> Self {
        Self::new(
            Box::new(execute_live),
            Box::new(classify_live),
            Box::new(answer_live),
        )
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Act on a classified line; returns what was done, for the feed.
    fn routed(&self, intent: &wcore::intent::Intent, act: bool, events: &Events) -> String {
        if !intent.actionable() {
            return "ignored".into();
        }
        let running = self.running();
        match intent.command.as_str() {
            "run_goal" => match self.start(&intent.goal, act, events.clone()) {
                Ok(_) => format!("starting ({})", if act { "acting" } else { "dry run" }),
                Err(e) => format!("not started: {e}"),
            },
            "ask_screen" => match self.ask(&intent.transcript, events.clone()) {
                Ok(_) => "answered".into(),
                Err(e) => format!("could not answer: {e}"),
            },
            "quit_daemon" => {
                if running {
                    self.control.abort("quitting");
                }
                "quit: any run is aborted; closing is the shell's call".into()
            }
            // Said while idle these are a misread of something else in the room, and acting on
            // them would leave flags set for the next job.
            other if !running => format!(
                "nothing is running to {}",
                other.split('_').next().unwrap_or(other)
            ),
            "stop_run" => {
                self.control.abort("asked to stop");
                "aborting the run".into()
            }
            "pause_run" => {
                self.control.pause();
                events.state(self.state());
                "paused".into()
            }
            "resume_run" => {
                self.control.resume();
                events.state(self.state());
                "resumed".into()
            }
            _ => "ignored".into(),
        }
    }
}

impl Runner for Wired {
    fn start(&self, goal: &str, act: bool, events: Events) -> Result<bool, String> {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("a run is already going: abort it before starting another".into());
        }
        // Here, not on the run thread: an abort that lands before the thread picks the job up
        // must survive. The previous run is over, so nothing else reads these flags now.
        self.control.reset();
        let job = Job {
            goal: goal.to_string(),
            act,
            events,
        };
        let sent = self
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(job);
        if sent.is_err() {
            self.running.store(false, Ordering::SeqCst);
            return Err("the run thread is gone".into());
        }
        Ok(true)
    }

    fn pause(&self) -> Result<bool, String> {
        if !self.running() {
            return Err("nothing is running to pause".into());
        }
        Ok(self.control.toggle_pause())
    }

    fn abort(&self) -> Result<bool, String> {
        if !self.running() {
            return Ok(false);
        }
        self.control.abort("asked to stop");
        Ok(true)
    }

    fn route_said(&self, text: &str, act: bool, events: Events) -> Result<Value, String> {
        let state = self.state();
        let intent = (self.classify)(text, state.running, state.paused)?;
        let routed = self.routed(&intent, act, &events);
        events.line(format!(
            "heard {:?} -> {} ({:.2}, heard {:.2}): {routed}",
            intent.transcript, intent.command, intent.confidence, intent.heard
        ));
        Ok(json!({
            "command": intent.command, "goal": intent.goal, "confidence": intent.confidence,
            "heard": intent.heard, "actionable": intent.actionable(), "routed": routed,
        }))
    }

    fn ask(&self, question: &str, events: Events) -> Result<String, String> {
        (self.answer)(question, &events)
    }

    fn state(&self) -> RunState {
        let running = self.running();
        RunState {
            running,
            paused: running && self.control.paused(),
        }
    }
}

fn execute_live(job: &Job, control: &wcore::runner::Control) {
    use wcore::{actions, config, perception, runner, sitepick, writer};
    let Some(classifier) = api::typesafe::TypeSafe::from_env() else {
        job.events.line("a run needs TYPESAFE_API_KEY (Settings)");
        return;
    };
    let scribe = writer::make_writer();
    if scribe.is_none() {
        job.events
            .line("no OPENAI_API_KEY: running without the writer");
    }
    let eyes = perception::LiveDesktop;
    let hands = actions::RealDesktop;
    let deps = runner::Deps {
        eyes: &eyes,
        hands: &hands,
        classifier: &classifier,
        writer: scribe.as_ref().map(|w| w as &dyn writer::StructuredWriter),
        cursor: &runner::live_cursor,
        browser: config::browser(),
        email: config::email(),
        // Once per job: the goal is fixed for the whole run.
        sites: sitepick::sites_for(&job.goal, None),
        apps: sitepick::apps_for(&job.goal, None),
        options: runner::RunOptions::default(),
    };
    let result = runner::run_goal(&job.goal, job.act, control, &Bridge(&job.events), &deps);
    job.events.line(format!(
        "run ended: {} in {:.1}s ({})",
        result.outcome,
        result.seconds,
        result.out.display()
    ));
}

fn classify_live(text: &str, running: bool, paused: bool) -> Result<wcore::intent::Intent, String> {
    let client = api::typesafe::TypeSafe::from_env()
        .ok_or("understanding speech needs TYPESAFE_API_KEY (Settings)")?;
    let floor = wcore::config::voice_min_confidence()?;
    wcore::intent::interpret(&client, text, running, paused, floor)
        .map_err(|e| format!("could not classify that: {e}"))
}

fn answer_live(question: &str, events: &Events) -> Result<String, String> {
    let scribe = wcore::writer::make_writer();
    wcore::runner::answer_screen(
        question,
        &wcore::perception::LiveDesktop,
        scribe
            .as_ref()
            .map(|w| w as &dyn wcore::writer::StructuredWriter),
        &wcore::config::browser(),
        &Bridge(events),
    )
    .map_err(|e| e.to_string())
}

/// Read `text` aloud when `CLICKER_SPEAK` allows it. Never blocks on the voice.
pub fn speak(text: &str) -> bool {
    if text.trim().is_empty() || !wcore::config::speak_answers() {
        return false;
    }
    platform::speech::speak(&platform::speech::speakable(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::Captured;

    #[test]
    fn highlight_carries_physical_marks_and_a_duration() {
        let captured = Captured::default();
        let events = Events::new(Emitter::new(captured.clone()), Arc::new(|_| true), "");
        let mark = Mark {
            x: 2561.0,
            y: -1440.0,
            w: 120.0,
            h: 40.0,
            label: Some("Send".into()),
        };
        events.highlight(&[mark], 4.0);
        events.answer("it is the send button");
        let m = captured.messages();
        assert_eq!(
            m[0],
            json!({ "event": "highlight", "seconds": 4.0,
            "marks": [{ "x": 2561.0, "y": -1440.0, "w": 120.0, "h": 40.0, "label": "Send" }] })
        );
        assert_eq!(
            m[1],
            json!({ "event": "answer", "text": "it is the send button", "spoken": true })
        );
    }

    use std::time::{Duration, Instant};
    use wcore::intent::Intent;
    use wcore::runner::Events as _;

    /// A run that reports like the core does, then holds at checkpoints until aborted.
    fn holding_run() -> Execute {
        Box::new(|job: &Job, control: &wcore::runner::Control| {
            let bridge = Bridge(&job.events);
            bridge.state(true, false);
            bridge.line(&format!("goal: {} act={}", job.goal, job.act));
            bridge.highlight(
                vec![wcore::runner::Mark {
                    x: 10.0,
                    y: -1430.0,
                    w: 5.0,
                    h: 6.0,
                    label: String::new(),
                    tone: wcore::runner::Tone::Point,
                }],
                3.0,
            );
            while control.checkpoint().is_ok() {
                std::thread::sleep(Duration::from_millis(5));
            }
            bridge.answer("stopped");
        })
    }

    fn says(command: &'static str, heard: f64) -> Classify {
        Box::new(move |text: &str, _, _| Ok(Intent::new(command, text, 0.99, heard, text)))
    }

    fn wired(command: &'static str, heard: f64) -> (Wired, Events, Captured) {
        let captured = Captured::default();
        let events = Events::new(Emitter::new(captured.clone()), Arc::new(|_| false), "keys");
        let answer: Answerer = Box::new(|q: &str, e: &Events| {
            e.answer(&format!("about {q}"));
            Ok(format!("about {q}"))
        });
        (
            Wired::new(holding_run(), says(command, heard), answer),
            events,
            captured,
        )
    }

    fn wait_until(what: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !what() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_run_is_refused_while_one_is_going_and_abort_ends_it_on_idle() {
        let (w, events, captured) = wired("run_goal", 1.0);
        assert_eq!(w.start("open mail", false, events.clone()), Ok(true));
        assert!(w.state().running);
        let second = w.start("again", false, events.clone()).unwrap_err();
        assert!(second.contains("already"), "{second}");
        wait_until(|| {
            captured
                .messages()
                .iter()
                .any(|m| m["event"] == "highlight")
        });
        assert_eq!(w.pause(), Ok(true));
        assert!(w.state().paused);
        assert_eq!(w.abort(), Ok(true), "abort releases a paused run");
        wait_until(|| !w.state().running);
        assert_eq!(w.abort(), Ok(false));
        assert!(w.pause().is_err());
        let m = captured.messages();
        let h = m.iter().find(|m| m["event"] == "highlight").unwrap();
        assert_eq!(
            *h,
            json!({ "event": "highlight", "seconds": 3.0,
                    "marks": [{ "x": 10.0, "y": -1430.0, "w": 5.0, "h": 6.0 }] })
        );
        assert!(m.iter().any(|m| m["text"] == "goal: open mail act=false"));
        assert!(m
            .iter()
            .any(|m| m["event"] == "answer" && m["text"] == "stopped"));
        let states: Vec<_> = m.iter().filter(|m| m["event"] == "state").collect();
        assert_eq!(states.first().unwrap()["running"], true);
        assert_eq!(states.last().unwrap()["running"], false);
        // The control was reset: the next run is not born aborted.
        assert_eq!(w.start("next", true, events), Ok(true));
        assert!(w.state().running && !w.state().paused);
        w.abort().unwrap();
    }

    #[test]
    fn a_spoken_goal_starts_a_run_in_the_current_mode_and_says_so() {
        let (w, events, captured) = wired("run_goal", 0.9);
        let v = w.route_said("open youtube", false, events).unwrap();
        assert_eq!(v["routed"], "starting (dry run)");
        wait_until(|| {
            captured
                .messages()
                .iter()
                .any(|m| m["text"] == "goal: open youtube act=false")
        });
        assert!(captured.messages().iter().any(|m| m["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("heard \"open youtube\" -> run_goal"))));
        w.abort().unwrap();
    }

    #[test]
    fn a_half_heard_goal_is_ignored_but_a_half_heard_stop_still_stops() {
        let (w, events, _) = wired("run_goal", 0.2);
        let v = w.route_said("open the", true, events.clone()).unwrap();
        assert_eq!(v["routed"], "ignored");
        assert!(!w.state().running);

        let (w, events, _) = wired("stop_run", 0.2);
        assert_eq!(
            w.route_said("stop", true, events.clone()).unwrap()["routed"],
            "nothing is running to stop"
        );
        w.start("go", false, events.clone()).unwrap();
        assert_eq!(
            w.route_said("stop", true, events).unwrap()["routed"],
            "aborting the run"
        );
        wait_until(|| !w.state().running);
    }

    #[test]
    fn pause_and_resume_by_voice_emit_state() {
        let (w, events, captured) = wired("pause_run", 0.1);
        w.start("go", false, events.clone()).unwrap();
        assert_eq!(
            w.route_said("hold on", true, events.clone()).unwrap()["routed"],
            "paused"
        );
        assert!(w.state().paused);
        assert!(captured
            .messages()
            .iter()
            .any(|m| m["event"] == "state" && m["paused"] == true));
        w.abort().unwrap();
    }

    #[test]
    fn a_question_goes_to_the_screen_answerer_and_touches_nothing() {
        let (w, events, captured) = wired("ask_screen", 0.9);
        let v = w
            .route_said("what is this error", true, events.clone())
            .unwrap();
        assert_eq!(v["routed"], "answered");
        assert!(!w.state().running);
        assert_eq!(w.ask("why", events).unwrap(), "about why");
        let answers: Vec<_> = captured
            .messages()
            .into_iter()
            .filter(|m| m["event"] == "answer")
            .collect();
        assert_eq!(answers[0]["text"], "about what is this error");
    }
}
