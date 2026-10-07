//! The step loop and the run folder. A port of `typesafe_computer_use_win/runner.py`.
//!
//! One run happens on one thread: a [`Screen`] holds COM handles and is not `Send`. What crosses
//! threads is [`Control`] (pause, resume, abort), which is cheap to clone and shares one state.
//!
//! Everything the loop says or shows goes out through [`Events`]: the log lines, the box a dry run
//! draws around what it would have pressed, the final answer, and the running/paused state. The
//! machine is reached only through the two desktop seams (`perception::Desktop` to look,
//! `actions::Desktop` to act), the classifier and the writer, so every test runs against fakes.
//!
//! The run folder has the Python layout, which `runs_index` reads:
//! `runs/<YYYYmmdd-HHMMSS>/` holding `run.log`, `run.json`, and per step `step-NNN-raw.png`,
//! `step-NNN.png` (annotated), `step-NNN-payload.txt` and `step-NNN-answers.json`, plus
//! `answer-raw.png` when the answer needed a fresh capture.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use api::typesafe::ChoiceAnswer;
use platform::uia::PressHandle;
use serde_json::{json, Map, Value};

use crate::actions::{self, is_noop, perform, py_repr, Context, View};
use crate::apps::App;
use crate::catalog::Site;
use crate::config::{
    ABORT_CORNER_PX, DEFAULT_DELAY, DEFAULT_MIN_CONFIDENCE, DEFAULT_STEPS, MAX_OPTIONS,
};
use crate::decide::{
    self, base_state, item_criteria, kind_criteria, offscreen_criteria, offscreen_records,
    site_criteria, Classifier, Criteria, Decision,
};
use crate::models::{Abort, Field, Item, Screen};
use crate::perception::{self, OcrCache};
use crate::report::{annotate, ax_count, render_payload, top, Payload, PayloadItem};
use crate::talk::{self, Eyes, TalkError};
use crate::timing::{format_timing, phase, summarize, Timing};
use crate::writer::{self, Answer, StructuredWriter, WriterError};

pub const MAX_CONSECUTIVE_NOOPS: u32 = 2;

/// How long a dry run's box stays on screen, as `highlight.DEFAULT_SECONDS`.
pub const HIGHLIGHT_SECONDS: f64 = 5.0;

/// How often a wait polls the corner and the control.
const POLL: Duration = Duration::from_millis(100);

/// The outcomes that end with an answer, each in words the writer can pass on. A dry run took no
/// action and an abort is the user's own stop, so neither has anything to report.
pub const STOPPED: [(&str, &str); 5] = [
    (
        "done",
        "the classifier judged the goal already achieved on this screen",
    ),
    (
        "nothing helps",
        "the classifier found nothing on this screen that helps with the goal",
    ),
    (
        "low confidence",
        "the classifier was not confident enough in any next action",
    ),
    ("stalled", "the last actions changed nothing"),
    ("step limit", "the run used every step it was allowed"),
];

pub fn stopped_reason(outcome: &str) -> Option<&'static str> {
    STOPPED
        .iter()
        .find(|(o, _)| *o == outcome)
        .map(|(_, why)| *why)
}

// ---------------------------------------------------------------- control

#[derive(Debug, Default)]
struct Flags {
    paused: bool,
    aborted: bool,
    reason: String,
}

/// Pause and abort, shared between the daemon's threads and the loop. Clone it freely: every
/// clone is the same control.
///
/// The loop only ever calls [`Control::checkpoint`]. Pause parks the caller there; abort returns
/// the same [`Abort`] the corner escape hatch does, so a stopped run lands in the path that already
/// writes the run folder and reports the outcome.
#[derive(Debug, Clone, Default)]
pub struct Control {
    inner: Arc<(Mutex<Flags>, Condvar)>,
}

impl Control {
    pub fn new() -> Self {
        Self::default()
    }

    fn flags(&self) -> MutexGuard<'_, Flags> {
        // A panic while holding this lock leaves plain flags behind; they are still meaningful.
        self.inner.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn paused(&self) -> bool {
        self.flags().paused
    }

    pub fn aborting(&self) -> bool {
        self.flags().aborted
    }

    pub fn pause(&self) {
        self.flags().paused = true;
    }

    pub fn resume(&self) {
        self.flags().paused = false;
        self.inner.1.notify_all();
    }

    /// Flip, and report whether the run is now paused.
    pub fn toggle_pause(&self) -> bool {
        let now = {
            let mut f = self.flags();
            f.paused = !f.paused;
            f.paused
        };
        self.inner.1.notify_all();
        now
    }

    /// Stop the run at its next checkpoint. A paused run is released, so it is still killable.
    pub fn abort(&self, reason: &str) {
        {
            let mut f = self.flags();
            f.reason = reason.to_string();
            f.aborted = true;
            f.paused = false;
        }
        self.inner.1.notify_all();
    }

    /// Clear both flags, so one Control serves the next job too.
    pub fn reset(&self) {
        {
            let mut f = self.flags();
            *f = Flags::default();
        }
        self.inner.1.notify_all();
    }

    /// Block while paused; fail once aborted. Called from the run thread only.
    pub fn checkpoint(&self) -> Result<(), Abort> {
        let mut f = self.flags();
        while f.paused && !f.aborted {
            f = self
                .inner
                .1
                .wait_timeout(f, POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if f.aborted {
            return Err(Abort(f.reason.clone()));
        }
        Ok(())
    }
}

/// The corner escape hatch: the pointer slammed into the top-left corner of the primary display.
///
/// The primary display's top-left is (0,0) on the virtual desktop, whatever else is attached. The
/// Python's `x <= 4 && y <= 4` also fired on the left edge of any monitor placed above or left of
/// the primary (a monitor at origin (0,-1440) puts (1,-1440) there), so both bounds are checked.
pub fn check_corner(cursor: (i32, i32)) -> Result<(), Abort> {
    let near = |v: i32| (0..=ABORT_CORNER_PX).contains(&v);
    if near(cursor.0) && near(cursor.1) {
        return Err(Abort("mouse in top-left corner".into()));
    }
    Ok(())
}

/// The real pointer, for [`Deps::cursor`].
pub fn live_cursor() -> (i32, i32) {
    platform::display::cursor_position()
}

// ---------------------------------------------------------------- events

/// How a mark is drawn: the thing to use, or context worth seeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Point,
    Note,
}

/// One box to draw over the desktop, in physical virtual-desktop pixels: the same coordinates a
/// click uses, with the display's origin already added (once).
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub label: String,
    pub tone: Tone,
}

/// Everything a run tells whoever is watching. Called from the run thread.
pub trait Events {
    /// One log line, the same text that goes to `run.log`.
    fn line(&self, text: &str);
    /// Draw these boxes for about `seconds`.
    fn highlight(&self, marks: Vec<Mark>, seconds: f64);
    /// The final answer, or talk mode's explanation.
    fn answer(&self, text: &str);
    fn state(&self, running: bool, paused: bool);
    /// Point the buddy at one box for about `hold` seconds: a dry run's target, the item an
    /// acting run is about to press, or a teach step (`step` is (i, n), 1-based). The mark is in
    /// physical virtual-desktop pixels, origin added once, like [`Events::highlight`].
    fn point(&self, _mark: Mark, _step: Option<(usize, usize)>, _hold: f64) {}
    /// Stop pointing.
    fn clear_point(&self) {}
    /// Read a line aloud without it being the answer (a teach step). Need not block.
    fn speak(&self, _text: &str) {}
}

pub const DEFAULT_POINT_LEAD_MS: u64 = 350;

/// How long an acting run points at an item before pressing it, so the buddy gets there first:
/// `CLICKER_POINT_LEAD_MS`, default 350; 0 points without waiting.
pub fn point_lead() -> Duration {
    let ms = std::env::var("CLICKER_POINT_LEAD_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_POINT_LEAD_MS);
    Duration::from_millis(ms)
}

/// How long to give one spoken teach line before the next: 60 ms a character, 1.5 s to 6 s. The
/// voice does not report when it is done, so this estimates it.
pub fn speech_pause(text: &str) -> Duration {
    let ms = (text.chars().count() as u64 * 60).clamp(1500, 6000);
    Duration::from_millis(ms)
}

// ---------------------------------------------------------------- adapters

/// [`actions::Writer`] over any [`StructuredWriter`], so a caller passes the api client.
pub struct WriterAdapter<'a>(pub &'a dyn StructuredWriter);

impl actions::Writer for WriterAdapter<'_> {
    fn compose_url(&self, goal: &str, history: &[String]) -> Result<String, String> {
        writer::compose_url(self.0, goal, history).map_err(|e| e.to_string())
    }

    fn compose_text(
        &self,
        goal: &str,
        screen: &Screen,
        items: &[Item],
        history: &[String],
    ) -> Result<String, String> {
        writer::compose_text(self.0, goal, screen, items, history).map_err(|e| e.to_string())
    }
}

/// [`actions::Verifier`] over any [`Classifier`]. A failed request reads as 0.0: the typing is
/// then reported as unverified, which is a no-op step, never a crash.
pub struct VerifierAdapter<'a>(pub &'a dyn Classifier);

impl actions::Verifier for VerifierAdapter<'_> {
    fn verify_typed(&self, goal: &str, before: &Field, typed: &str, after: Option<&Field>) -> f64 {
        decide::verify_typed(self.0, goal, before, typed, after).unwrap_or(0.0)
    }
}

/// [`talk::Eyes`] over a perception desktop. Perception fills the screen's handle maps, so it reads
/// a copy and leaves the caller's screen as it was.
pub struct PerceptionEyes<'a>(pub &'a dyn perception::Desktop);

impl Eyes for PerceptionEyes<'_> {
    fn capture(&self, browser: &str) -> Result<Screen, String> {
        perception::capture_at(self.0, browser, None, perception::Focus::Mouse)
    }

    fn perceive(
        &self,
        screen: &Screen,
        budget: usize,
        question: &str,
    ) -> Result<Vec<Item>, String> {
        let mut copy = copy_screen(screen);
        Ok(perception::perceive(
            self.0,
            &mut copy,
            budget,
            question,
            None,
            None,
            &[],
        ))
    }
}

fn copy_screen(s: &Screen) -> Screen {
    Screen {
        bgra: s.bgra.clone(),
        width: s.width,
        height: s.height,
        scale: s.scale,
        app: s.app.clone(),
        field: s.field.clone(),
        url: s.url.clone(),
        pid: s.pid,
        window: s.window,
        origin: s.origin,
        monitor: s.monitor,
        windows: s.windows.clone(),
        monitors: s.monitors.clone(),
        ax_refs: s.ax_refs.clone(),
        offscreen: s.offscreen.clone(),
        pointer: s.pointer.clone(),
    }
}

// ---------------------------------------------------------------- configuration

/// The knobs of one run beyond the goal and whether to act.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The run folder. None makes `runs/<YYYYmmdd-HHMMSS>` under the working directory.
    pub out: Option<PathBuf>,
    pub steps: u32,
    pub min_confidence: f64,
    /// Seconds to let the screen settle after an action.
    pub delay: f64,
    /// Replay a saved capture instead of the screen. A replay never acts and never draws.
    pub image: Option<PathBuf>,
    /// The frontmost app to report during replay.
    pub app: Option<String>,
    /// The browser URL to report during replay.
    pub url: Option<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            out: None,
            steps: DEFAULT_STEPS,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            delay: DEFAULT_DELAY,
            image: None,
            app: None,
            url: None,
        }
    }
}

/// Everything a run talks to. `hands` is generic because `actions::perform` is.
pub struct Deps<'a, A> {
    /// How the run looks: `perception::LiveDesktop` for real.
    pub eyes: &'a dyn perception::Desktop,
    /// How the run acts: `actions::RealDesktop` for real. A dry run never calls it.
    pub hands: &'a A,
    pub classifier: &'a dyn Classifier,
    /// The writer for URLs, field text and the final answer. None runs without them.
    pub writer: Option<&'a dyn StructuredWriter>,
    /// The pointer position, for the corner escape hatch: [`live_cursor`] for real.
    pub cursor: &'a dyn Fn() -> (i32, i32),
    pub browser: String,
    pub email: Option<String>,
    /// The per-goal site shortlist; empty falls back to the pinned catalog.
    pub sites: Vec<Site>,
    /// The installed applications worth offering for this goal.
    pub apps: Vec<App>,
    pub options: RunOptions,
}

/// What a run came to. The run folder holds the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub out: PathBuf,
    /// "done", "nothing helps", "low confidence", "stalled", "step limit", "dry run",
    /// "aborted (<reason>)" or "crashed".
    pub outcome: String,
    pub answer: Option<Answer>,
    pub history: Vec<String>,
    pub seconds: f64,
}

/// Why the loop left early: the user's stop, or a failure the Python would have raised.
#[derive(Debug)]
enum Stop {
    Abort(String),
    Crash(String),
}

impl From<Abort> for Stop {
    fn from(a: Abort) -> Self {
        Stop::Abort(a.0)
    }
}

// ---------------------------------------------------------------- the run

/// Drive the loop toward `goal`. With `act` false this is a dry run: it decides, draws a box
/// around what it would press, and stops without touching anything.
pub fn run_goal<A: actions::Desktop<Handle = PressHandle>>(
    goal: &str,
    act: bool,
    control: &Control,
    events: &dyn Events,
    deps: &Deps<A>,
) -> RunResult {
    let out = deps.options.out.clone().unwrap_or_else(|| {
        Path::new("runs").join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string())
    });
    let _ = fs::create_dir_all(&out);
    let writer = deps.writer.map(WriterAdapter);
    let verifier = VerifierAdapter(deps.classifier);
    let ctx = Context {
        goal: goal.to_string(),
        browser: deps.browser.clone(),
        email: deps.email.clone(),
        writer: writer.as_ref().map(|w| w as &dyn actions::Writer),
        verifier: &verifier,
        history: Vec::new(),
        sites: deps.sites.clone(),
        apps: deps.apps.clone(),
    };
    let mut r = Runner::new(goal, act, control, events, deps, out, ctx);
    events.state(true, control.paused());
    let result = r.run();
    events.state(false, false);
    result
}

/// Look at the screen and answer a question about it, acting on nothing (`talk`), then teach:
/// for each next step, point at the item it uses, say it, and give the voice time before the next.
/// A step with no item is only said. `control` aborting (a spoken "stop") ends the teaching.
pub fn answer_screen(
    question: &str,
    eyes: &dyn perception::Desktop,
    writer: Option<&dyn StructuredWriter>,
    browser: &str,
    events: &dyn Events,
    control: &Control,
) -> Result<String, TalkError> {
    answer_screen_paced(
        question,
        eyes,
        writer,
        browser,
        events,
        control,
        &speech_pause,
    )
}

/// [`answer_screen`] with the pause per spoken line given, so tests need not wait for a voice.
pub fn answer_screen_paced(
    question: &str,
    eyes: &dyn perception::Desktop,
    writer: Option<&dyn StructuredWriter>,
    browser: &str,
    events: &dyn Events,
    control: &Control,
    pace: &dyn Fn(&str) -> Duration,
) -> Result<String, TalkError> {
    let Some(writer) = writer else {
        let said = "I could not answer that: no writer is configured (set OPENAI_API_KEY).";
        events.answer(said);
        return Ok(said.to_string());
    };
    let (screen, items, teach) =
        talk::teach_question(writer, &PerceptionEyes(eyes), question, browser)?;
    events.answer(&teach.answer);
    if teach.steps.is_empty() {
        return Ok(teach.answer);
    }
    let n = teach.steps.len();
    let taught = (|| -> Result<(), Abort> {
        wait_watching(control, pace(&teach.answer))?;
        for (i, step) in teach.steps.iter().enumerate() {
            control.checkpoint()?;
            let pause = pace(&step.text);
            let target = step
                .item
                .and_then(|k| items.iter().find(|it| it.index == k));
            if let Some(item) = target {
                events.point(
                    item_mark(&screen, item, step.text.clone()),
                    Some((i + 1, n)),
                    pause.as_secs_f64(),
                );
            }
            events.line(&format!("step {}/{n}: {}", i + 1, step.text));
            events.speak(&step.text);
            wait_watching(control, pause)?;
        }
        Ok(())
    })();
    events.clear_point();
    if let Err(Abort(why)) = taught {
        events.line(&format!("teaching stopped ({why})"));
    }
    Ok(teach.answer)
}

/// Wait, returning early with the abort when `control` is aborted.
fn wait_watching(control: &Control, d: Duration) -> Result<(), Abort> {
    let end = Instant::now() + d;
    loop {
        control.checkpoint()?;
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep(POLL.min(end - now));
    }
}

struct Runner<'r, 'a, A> {
    goal: String,
    act: bool,
    control: &'r Control,
    events: &'r dyn Events,
    deps: &'r Deps<'a, A>,
    out: PathBuf,
    ctx: Context<'r>,
    timings: Vec<Timing>,
    consecutive_noops: u32,
    last_url: Option<String>,
    outcome: String,
    ocr_cache: OcrCache,
    /// The latest capture, until an action makes it stale.
    view: Option<(Screen, Vec<Item>)>,
    answer: Option<Answer>,
}

impl<'r, 'a, A: actions::Desktop<Handle = PressHandle>> Runner<'r, 'a, A> {
    fn new(
        goal: &str,
        act: bool,
        control: &'r Control,
        events: &'r dyn Events,
        deps: &'r Deps<'a, A>,
        out: PathBuf,
        ctx: Context<'r>,
    ) -> Self {
        Self {
            goal: goal.to_string(),
            act,
            control,
            events,
            deps,
            out,
            ctx,
            timings: Vec::new(),
            consecutive_noops: 0,
            last_url: None,
            outcome: "crashed".into(), // every way out names its own; only a failure leaves this
            ocr_cache: OcrCache::new(),
            view: None,
            answer: None,
        }
    }

    fn replay(&self) -> bool {
        self.deps.options.image.is_some()
    }

    fn log(&self, msg: &str) {
        let events = self.events;
        let _ = catch_unwind(AssertUnwindSafe(|| events.line(msg)));
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.out.join("run.log"))
        {
            let _ = f.write_all(format!("{msg}\n").as_bytes());
        }
    }

    /// The corner, then pause and abort. Pausing is reported to the watcher both ways.
    fn checkpoint(&self) -> Result<(), Abort> {
        check_corner((self.deps.cursor)())?;
        if self.control.paused() {
            self.events.state(true, true);
            let r = self.control.checkpoint();
            self.events.state(true, false);
            return r;
        }
        self.control.checkpoint()
    }

    /// Wait, polling the checkpoint every 100 ms.
    fn sleep_watching(&self, seconds: f64) -> Result<(), Abort> {
        let end = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        loop {
            self.checkpoint()?;
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            std::thread::sleep(POLL.min(end - now));
        }
    }

    fn run(&mut self) -> RunResult {
        let started = Instant::now();
        self.log(&format!("run folder: {}", self.out.display()));
        if self.act && !self.replay() {
            self.log("driving the machine. abort: slam the mouse into the top-left corner.");
        }
        let steps = self.deps.options.steps;
        let looped = (|| -> Result<(), Stop> {
            let mut finished = true;
            for step in 1..=steps {
                if !self.run_step(step)? {
                    finished = false;
                    break;
                }
            }
            if finished {
                self.log(&format!("\nstopped after {steps} steps"));
                self.outcome = "step limit".into();
            }
            self.conclude()
        })();
        match looped {
            Ok(()) => {}
            Err(Stop::Abort(reason)) => {
                let reason = if reason.is_empty() {
                    "Ctrl-C".into()
                } else {
                    reason
                };
                self.outcome = format!("aborted ({reason})");
                self.log(&format!(
                    "\n{} after {} actions",
                    self.outcome,
                    self.ctx.history.len()
                ));
            }
            Err(Stop::Crash(why)) => {
                self.outcome = "crashed".into();
                self.log(&format!("\ncrashed: {why}"));
            }
        }
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        self.write_summary(seconds);
        self.log(&format!("run folder: {}", self.out.display()));
        RunResult {
            out: self.out.clone(),
            outcome: self.outcome.clone(),
            answer: self.answer.clone(),
            history: self.ctx.history.clone(),
            seconds,
        }
    }

    fn write_summary(&self, seconds: f64) {
        let o = &self.deps.options;
        let config = json!({
            "goal": self.goal,
            "out": self.out.display().to_string(),
            "act": self.act.to_string(),
            "steps": o.steps.to_string(),
            "min_confidence": o.min_confidence.to_string(),
            "delay": o.delay.to_string(),
            "image": o.image.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "None".into()),
            "app": o.app.clone().unwrap_or_else(|| "None".into()),
            "url": o.url.clone().unwrap_or_else(|| "None".into()),
        });
        let summary = json!({
            "goal": self.goal,
            "act": self.act,
            "steps_taken": self.ctx.history.len(),
            "outcome": self.outcome,
            "answer": self.answer.as_ref().map(|a| a.text.clone()),
            "goal_achieved": self.answer.as_ref().map(|a| a.achieved),
            "seconds": seconds,
            "timing": summarize(&self.timings),
            "history": self.ctx.history,
            "config": config,
        });
        let text = serde_json::to_string_pretty(&summary).unwrap_or_default();
        let _ = fs::write(self.out.join("run.json"), text);
    }

    fn capture(&self, timing: Option<&mut Timing>) -> Result<Screen, Stop> {
        let o = &self.deps.options;
        match &o.image {
            Some(path) => perception::replay(path, o.app.as_deref().unwrap_or(""), o.url.clone()),
            None => perception::capture(self.deps.eyes, &self.deps.browser, timing),
        }
        .map_err(Stop::Crash)
    }

    fn sites(&self) -> Option<&[Site]> {
        (!self.ctx.sites.is_empty()).then_some(self.ctx.sites.as_slice())
    }

    /// Hand the screen the run ended on to the writer, for the answer the classifier cannot put
    /// into words. The last step's capture serves when nothing acted after it; an action makes it
    /// stale, so the screen is captured again and saved, so the answer can be checked.
    fn conclude(&mut self) -> Result<(), Stop> {
        let Some(stopped) = stopped_reason(&self.outcome) else {
            return Ok(());
        };
        let Some(w) = self.deps.writer else {
            self.log("\nno answer: no writer is configured (set OPENAI_API_KEY)");
            return Ok(());
        };
        let started = Instant::now();
        if self.view.is_none() {
            check_corner((self.deps.cursor)())?;
            let mut screen = self.capture(None)?;
            let _ = save_png(&screen, &self.out.join("answer-raw.png"));
            let items = perception::perceive(
                self.deps.eyes,
                &mut screen,
                MAX_OPTIONS,
                &self.goal,
                None,
                None,
                &self.ctx.history,
            );
            self.view = Some((screen, items));
        }
        let (screen, items) = self.view.as_ref().expect("set above");
        let answer = match writer::compose_answer(
            w,
            &self.goal,
            screen,
            items,
            &self.ctx.history,
            stopped,
        ) {
            Ok(a) => a,
            Err(WriterError::Unavailable(e)) => {
                self.log(&format!("\nno answer: the writer is unavailable ({e})"));
                return Ok(());
            }
            Err(e) => {
                self.log(&format!("\nno answer: {e}"));
                return Ok(());
            }
        };
        let verdict = if answer.achieved {
            "goal achieved"
        } else {
            "goal not achieved"
        };
        self.log(&format!(
            "\nanswer ({verdict}, {:.1}s):\n  {}",
            started.elapsed().as_secs_f64(),
            answer.text
        ));
        self.events.answer(&answer.text);
        self.answer = Some(answer);
        Ok(())
    }

    fn run_step(&mut self, step: u32) -> Result<bool, Stop> {
        self.checkpoint()?;
        let mut timing = Timing::new();
        let started = Instant::now();
        let mut screen = phase(Some(&mut timing), "capture", || self.capture(None))?;
        // The history goes in so the echo filter drops the loop's own log lines when a terminal
        // showing them is on screen: without it a run considers clicking what it just printed.
        let cache = if self.replay() {
            None
        } else {
            Some(&mut self.ocr_cache)
        };
        let items = perception::perceive(
            self.deps.eyes,
            &mut screen,
            MAX_OPTIONS,
            &self.goal,
            Some(&mut timing),
            cache,
            &self.ctx.history,
        );
        // Three digits, so a run of 100 steps still lists in order.
        let prefix = format!("step-{step:03}");
        let _ = save_png(&screen, &self.out.join(format!("{prefix}-raw.png")));
        let _ = fs::write(
            self.out.join(format!("{prefix}-payload.txt")),
            self.payload(&screen, &items),
        );

        let decided = phase(Some(&mut timing), "decide", || {
            decide::decide(
                self.deps.classifier,
                &self.goal,
                &screen,
                &items,
                &self.ctx.history,
                &self.ctx.browser,
                self.ctx.email.as_deref(),
                self.sites(),
                &self.ctx.apps,
            )
        });
        let decision = decided.map_err(|e| Stop::Crash(e.message))?;
        let chosen = decision.chosen();
        let field_box = screen.field.as_ref().map(|f| {
            let s = screen.scale;
            (f.x * s, f.y * s, (f.x + f.w) * s, (f.y + f.h) * s)
        });
        let _ = annotate(
            &screen.bgra,
            screen.width,
            screen.height,
            screen.scale,
            &items,
            &chosen,
            field_box,
            &self.out.join(format!("{prefix}.png")),
        );
        self.log_decision(step, &screen, &items, &decision);
        let mut record = answers(&decision, &screen, &items);

        // Hold the capture from here on, so the answer can read it when nothing acts after it.
        self.view = Some((screen, items));
        let keep_going = self.resolve(&decision, &mut timing)?;
        if !timing.contains("act") {
            timing.set("act", 0.0);
        }
        timing.set(
            "total",
            (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
        );

        record["timing"] = serde_json::to_value(&timing).unwrap_or(Value::Null);
        let _ = fs::write(
            self.out.join(format!("{prefix}-answers.json")),
            serde_json::to_string_pretty(&record).unwrap_or_default(),
        );
        self.log(&format!(
            "  files: {prefix}-raw.png, {prefix}.png, {prefix}-payload.txt, {prefix}-answers.json"
        ));
        self.log(&format_timing(&timing));
        self.timings.push(timing);

        if self.view.is_none() {
            // An action ran: let the screen settle before the next step, or the answer, reads it.
            self.sleep_watching(self.deps.options.delay)?;
        }
        Ok(keep_going)
    }

    /// Apply the stop rules, then the action. True to keep looping.
    fn resolve(&mut self, decision: &Decision, timing: &mut Timing) -> Result<bool, Stop> {
        if decision.stops() {
            self.log(&format!(
                "  model says {}; stopping",
                py_repr(&decision.kind.choice)
            ));
            self.outcome = if decision.kind.choice == "done" {
                "done"
            } else {
                "nothing helps"
            }
            .into();
            return Ok(false);
        }
        let min = self.deps.options.min_confidence;
        if decision.confidence() < min {
            self.log(&format!(
                "  confidence {:.2} below {min}; stopping",
                decision.confidence()
            ));
            self.outcome = "low confidence".into();
            return Ok(false);
        }
        let chosen = decision.chosen();
        if !self.act || self.replay() {
            self.log(&format!(
                "  would do: {chosen}. dry run, nothing was pressed"
            ));
            if !self.replay() {
                if let Some((screen, items)) = &self.view {
                    if let Some(mark) = point_at(screen, items, decision) {
                        let events = self.events;
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            events.highlight(vec![mark.clone()], HIGHLIGHT_SECONDS);
                            events.point(mark, None, HIGHLIGHT_SECONDS);
                        }));
                    }
                }
            }
            self.outcome = "dry run".into();
            return Ok(false);
        }

        let (screen, items) = self
            .view
            .take()
            .ok_or_else(|| Stop::Crash("no capture to act on".into()))?;
        self.preview(&chosen, &screen, &items)?;
        let hands = self.deps.hands;
        let what = phase(Some(timing), "act", || {
            perform(
                hands,
                &chosen,
                &decision.site.choice,
                &View::of(&screen),
                &items,
                &self.ctx,
            )
        })
        .map_err(|e| Stop::Crash(e.to_string()))?;
        // The view stays empty: the action made the capture stale.
        let repeated = self.ctx.history.last() == Some(&what) && screen.url == self.last_url;
        self.last_url = screen.url.clone();
        self.ctx.history.push(what.clone());
        self.log(&format!("  did: {what}"));
        if is_noop(&what) || repeated {
            self.consecutive_noops += 1;
            if self.consecutive_noops >= MAX_CONSECUTIVE_NOOPS {
                self.log(&format!(
                    "  {MAX_CONSECUTIVE_NOOPS} consecutive no-ops; stopping"
                ));
                self.outcome = "stalled".into();
                return Ok(false);
            }
        } else {
            self.consecutive_noops = 0;
        }
        Ok(true)
    }

    /// Act preview: point at what is about to be pressed or typed into, then give the buddy
    /// [`point_lead`] to get there. Actions with nothing on screen to point at are not delayed.
    fn preview(&self, chosen: &str, screen: &Screen, items: &[Item]) -> Result<(), Abort> {
        let mark = if let Some(item) = items.iter().find(|it| it.index.to_string() == chosen) {
            item_mark(screen, item, format!("press: {}", short(&item.text)))
        } else if matches!(chosen, "type_text" | "type_email") {
            match screen.field.as_ref().filter(|f| f.w > 0.0 && f.h > 0.0) {
                Some(f) => Mark {
                    x: f.x,
                    y: f.y,
                    w: f.w,
                    h: f.h,
                    label: format!("type into: {}", short(&f.label)),
                    tone: Tone::Point,
                },
                None => return Ok(()),
            }
        } else {
            return Ok(());
        };
        let lead = point_lead();
        let events = self.events;
        let _ = catch_unwind(AssertUnwindSafe(|| {
            events.point(mark, None, lead.as_secs_f64() + 1.0)
        }));
        self.sleep_watching(lead.as_secs_f64())
    }

    fn log_decision(&self, step: u32, screen: &Screen, items: &[Item], decision: &Decision) {
        let field_desc = screen
            .field
            .as_ref()
            .map(|f| format!(" field={}:{}", f.role, py_repr(&f.label)))
            .unwrap_or_default();
        let url = screen
            .url
            .as_deref()
            .map(py_repr)
            .unwrap_or_else(|| "None".into());
        self.log(&format!(
            "\nstep {step}: app={}{field_desc} url={url} items={} ax={} offscreen={} kind={} ({:.2}) site={}",
            py_repr(&screen.app),
            items.len(),
            ax_count(items),
            screen.offscreen.len(),
            decision.kind.choice,
            decision.kind.confidence,
            decision.site.choice,
        ));
        for (key, p) in top(&prob_map(&decision.kind), 4) {
            self.log(&format!("  {p:5.2}  {key}"));
        }
        if let Some(item) = &decision.item {
            self.log(&format!("  item ({:.2}):", item.confidence));
            for (key, p) in top(&prob_map(item), 4) {
                let text = items
                    .iter()
                    .find(|it| it.index.to_string() == key)
                    .map(|it| py_repr(&it.text))
                    .unwrap_or_default();
                self.log(&format!("  {p:5.2}  [{key}] {text}"));
            }
        }
        if let Some(off) = &decision.offscreen {
            self.log(&format!("  offscreen ({:.2}):", off.confidence));
            for (key, p) in top(&prob_map(off), 3) {
                let label = key
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| screen.offscreen.get(i))
                    .map(|n| py_repr(&n.label))
                    .unwrap_or_default();
                self.log(&format!("  {p:5.2}  [{key}] {label}"));
            }
        }
    }

    /// Exactly what goes to TypeSafe for this screen, plus a table of every item.
    fn payload(&self, screen: &Screen, items: &[Item]) -> String {
        let state = base_state(&self.goal, screen, items, &self.ctx.history);
        let kind = criteria_json(kind_criteria(
            &self.ctx.browser,
            self.ctx.email.as_deref(),
            !screen.offscreen.is_empty(),
            !screen.windows.is_empty(),
            !self.ctx.apps.is_empty(),
        ));
        let item = criteria_json(item_criteria(screen, items));
        let site = criteria_json(site_criteria(self.sites()));
        let off = (!screen.offscreen.is_empty())
            .then(|| criteria_json(offscreen_criteria(&screen.offscreen)));
        render_payload(&Payload {
            state: &state,
            kind_criteria: &kind,
            item_criteria: &item,
            site_criteria: &site,
            offscreen_criteria: off.as_ref(),
            items: items
                .iter()
                .map(|it| PayloadItem {
                    item: it,
                    click: screen.to_points(it),
                    region: screen.region(it),
                })
                .collect(),
            offscreen: screen
                .offscreen
                .iter()
                .map(|n| (n.role.clone(), n.label.clone()))
                .collect(),
            field: screen
                .field
                .as_ref()
                .and_then(|f| serde_json::to_value(f).ok()),
            width: screen.width,
            height: screen.height,
            scale: screen.scale,
        })
    }
}

fn prob_map(answer: &ChoiceAnswer) -> BTreeMap<String, f64> {
    answer.probabilities.iter().cloned().collect()
}

fn prob_json(answer: Option<&ChoiceAnswer>) -> Value {
    match answer {
        Some(a) => Value::Object(
            a.probabilities
                .iter()
                .map(|(k, p)| (k.clone(), json!(p)))
                .collect(),
        ),
        None => Value::Null,
    }
}

fn criteria_json(criteria: Criteria) -> Value {
    Value::Object(
        criteria
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect::<Map<String, Value>>(),
    )
}

/// The box a dry run draws around what it would have pressed, in the coordinates a click uses.
///
/// This is what makes a dry run worth watching: the thing itself is circled on the screen. Showing
/// somebody the button cannot press the wrong one. The display's origin is added exactly once, as
/// `Screen::to_points` does for a click.
pub fn point_at(screen: &Screen, items: &[Item], decision: &Decision) -> Option<Mark> {
    let chosen = decision.chosen();
    let target = items.iter().find(|it| it.index.to_string() == chosen)?;
    Some(item_mark(
        screen,
        target,
        format!("would press: {}", short(&target.text)),
    ))
}

/// An item's box on the virtual desktop, the origin added exactly once, as `Screen::to_points`.
pub fn item_mark(screen: &Screen, item: &Item, label: String) -> Mark {
    let (left, top) = screen.origin;
    let s = screen.scale;
    Mark {
        x: left + item.x1 / s,
        y: top + item.y1 / s,
        w: (item.x2 - item.x1) / s,
        h: (item.y2 - item.y1) / s,
        label,
        tone: Tone::Point,
    }
}

fn short(text: &str) -> String {
    text.chars().take(40).collect()
}

/// What the classifier returned for this step. The caller fills in the step's `timing`.
pub fn answers(decision: &Decision, screen: &Screen, items: &[Item]) -> Value {
    let choice = |a: &Option<ChoiceAnswer>| a.as_ref().map(|a| a.choice.clone());
    let windows: Vec<Value> = screen
        .windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            json!({
                "k": i,
                "app": w.app,
                "title": w.title.chars().take(70).collect::<String>(),
                "monitor": w.monitor + 1,
            })
        })
        .collect();
    json!({
        "kind": decision.kind.choice,
        "kind_confidence": decision.kind.confidence,
        "kind_probabilities": prob_json(Some(&decision.kind)),
        "item": choice(&decision.item),
        "item_confidence": decision.item.as_ref().map(|a| a.confidence),
        "item_probabilities": prob_json(decision.item.as_ref()),
        "site": decision.site.choice,
        "site_probabilities": prob_json(Some(&decision.site)),
        "offscreen": choice(&decision.offscreen),
        "offscreen_probabilities": prob_json(decision.offscreen.as_ref()),
        "window": choice(&decision.window),
        "window_probabilities": prob_json(decision.window.as_ref()),
        // app_choice, not app: the frontmost process is already recorded as "app" below.
        "app_choice": choice(&decision.app),
        "app_probabilities": prob_json(decision.app.as_ref()),
        "open_windows": windows,
        "offscreen_controls": offscreen_records(&screen.offscreen),
        "chosen": decision.chosen(),
        "confidence": decision.confidence(),
        "timing": Value::Null,
        "items": items,
        "field": screen.field,
        "app": screen.app,
        "url": screen.url,
    })
}

/// The capture as a full-size PNG.
pub fn save_png(screen: &Screen, path: &Path) -> Result<(), String> {
    let n = screen.width as usize * screen.height as usize * 4;
    if screen.bgra.len() < n {
        return Err("the capture is shorter than its size".into());
    }
    let mut rgba = screen.bgra[..n].to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    image::RgbaImage::from_raw(screen.width, screen.height, rgba)
        .ok_or("bad capture size")?
        .save(path)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::tests::{make_item, screen};
    use crate::perception::Line;
    use crate::runs_index;
    use crate::writer::tests::Fake as FakeWriter;
    use api::typesafe::{Answer as Reply, ApiError, Question};
    use platform::capture::Shot;
    use platform::display::Monitor;
    use platform::uia::{Walked, WalkedOf};
    use platform::winlist::WindowInfo;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GOAL: &str = "find the next upcoming bruno mars concert";

    fn scratch() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wcore-runner-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
            chrono::Local::now().format("%H%M%S%f")
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ------------------------------------------------------------ fakes

    /// One point: the mark, the teach step, the hold.
    type Pointed = (Mark, Option<(usize, usize)>, f64);

    #[derive(Default)]
    struct Recorder {
        lines: RefCell<Vec<String>>,
        marks: RefCell<Vec<(Vec<Mark>, f64)>>,
        answers: RefCell<Vec<String>>,
        states: RefCell<Vec<(bool, bool)>>,
        points: RefCell<Vec<Pointed>>,
        clears: Cell<usize>,
        spoken: RefCell<Vec<String>>,
        /// Aborted at the first point, as a spoken "stop" would.
        stop_at_point: Option<Control>,
    }

    impl Events for Recorder {
        fn line(&self, text: &str) {
            self.lines.borrow_mut().push(text.to_string());
        }
        fn highlight(&self, marks: Vec<Mark>, seconds: f64) {
            self.marks.borrow_mut().push((marks, seconds));
        }
        fn answer(&self, text: &str) {
            self.answers.borrow_mut().push(text.to_string());
        }
        fn state(&self, running: bool, paused: bool) {
            self.states.borrow_mut().push((running, paused));
        }
        fn point(&self, mark: Mark, step: Option<(usize, usize)>, hold: f64) {
            self.points.borrow_mut().push((mark, step, hold));
            if let Some(c) = &self.stop_at_point {
                c.abort("asked to stop");
            }
        }
        fn clear_point(&self) {
            self.clears.set(self.clears.get() + 1);
        }
        fn speak(&self, text: &str) {
            self.spoken.borrow_mut().push(text.to_string());
        }
    }

    /// Records every effect; a dry run must leave this empty.
    #[derive(Default)]
    struct Hands {
        calls: RefCell<Vec<String>>,
    }

    impl Hands {
        fn note(&self, s: &str) {
            self.calls.borrow_mut().push(s.to_string());
        }
    }

    impl actions::Desktop for Hands {
        type Handle = PressHandle;
        fn ax_press(&self, _: &PressHandle) -> bool {
            self.note("ax_press");
            false
        }
        fn ax_focus(&self, _: &PressHandle) -> bool {
            self.note("ax_focus");
            false
        }
        fn ax_set_value(&self, _: &PressHandle, _: &str) -> bool {
            self.note("ax_set_value");
            false
        }
        fn ax_value(&self, _: &PressHandle) -> Option<String> {
            self.note("ax_value");
            None
        }
        fn focused_handle(&self) -> Option<PressHandle> {
            self.note("focused_handle");
            None
        }
        fn focused_field(&self) -> Option<Field> {
            self.note("focused_field");
            None
        }
        fn click_at(&self, p: (f64, f64)) {
            self.note(&format!("click_at {p:?}"));
        }
        fn type_text(&self, _: &str) {
            self.note("type_text");
        }
        fn press(&self, _: actions::Key, _: bool) {
            self.note("press");
        }
        fn scroll(&self, lines: i32) {
            self.note(&format!("scroll {lines}"));
        }
        fn activate_window(&self, _: isize) -> bool {
            self.note("activate_window");
            false
        }
        fn foreground_window(&self) -> isize {
            self.note("foreground_window");
            0
        }
        fn activate(&self, _: &str) -> bool {
            self.note("activate");
            false
        }
        fn open_url(&self, _: &str, _: &str) -> bool {
            self.note("open_url");
            false
        }
        fn launch(&self, _: &App) -> bool {
            self.note("launch");
            false
        }
        fn remember_site(&self, _: &str) {
            self.note("remember_site");
        }
        fn pause(&self, _: f64) {
            self.note("pause");
        }
    }

    /// A display above the primary, at (1, -1440), showing one line of text.
    struct Eyes2 {
        captures: Cell<usize>,
        refuse: bool,
    }

    impl Eyes2 {
        fn new() -> Self {
            Self {
                captures: Cell::new(0),
                refuse: false,
            }
        }
    }

    impl perception::Desktop for Eyes2 {
        fn monitors(&self) -> Vec<Monitor> {
            vec![Monitor {
                index: 0,
                left: 1,
                top: -1440,
                right: 801,
                bottom: -840,
                primary: false,
            }]
        }
        fn open_windows(&self) -> Vec<WindowInfo> {
            Vec::new()
        }
        fn foreground_hwnd(&self) -> isize {
            0
        }
        fn process_name(&self, _: u32) -> String {
            "fake".into()
        }
        fn screenshot(&self, _: Option<&Monitor>) -> Option<Shot> {
            assert!(!self.refuse, "captured again");
            self.captures.set(self.captures.get() + 1);
            Some(Shot {
                width: 800,
                height: 600,
                bgra: vec![255; 800 * 600 * 4],
                origin: (1, -1440),
            })
        }
        fn focused_field(&self) -> Option<Field> {
            None
        }
        fn browser_url(&self, _: isize, _: (f64, f64), _: (f64, f64)) -> Option<String> {
            None
        }
        fn ocr(&self, _: &[u8], width: u32, height: u32) -> Vec<Line> {
            // Centered in whatever is read, so a cropped read still finds it.
            let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
            vec![(
                "TICKETS".into(),
                0.99,
                (cx - 50.0, cy - 10.0, cx + 50.0, cy + 10.0),
            )]
        }
        fn walk(&self, _: isize, _: f64, _: f64, _: (f64, f64)) -> Walked {
            WalkedOf {
                on_screen: Vec::new(),
                off_screen: Vec::new(),
                capped: false,
            }
        }
    }

    /// Answers every step with the same reply; counts the requests.
    struct Brain {
        reply: Result<Vec<(String, Reply)>, ApiError>,
        calls: Cell<usize>,
    }

    fn ca(choice: &str, confidence: f64) -> ChoiceAnswer {
        ChoiceAnswer {
            choice: choice.into(),
            confidence,
            probabilities: vec![(choice.into(), confidence)],
        }
    }

    impl Brain {
        fn kind(kind: &str, confidence: f64) -> Self {
            let mut reply = vec![
                ("kind".into(), Reply::Choice(ca(kind, confidence))),
                ("site".into(), Reply::Choice(ca("none", 1.0))),
            ];
            if kind == "click_item" {
                reply.push(("item".into(), Reply::Choice(ca("0", 0.8))));
            }
            Self {
                reply: Ok(reply),
                calls: Cell::new(0),
            }
        }
    }

    impl Classifier for Brain {
        fn system_one(
            &self,
            _: &Value,
            _: &[(String, Question)],
        ) -> Result<Vec<(String, Reply)>, ApiError> {
            self.calls.set(self.calls.get() + 1);
            self.reply.clone()
        }
    }

    fn far_from_corner() -> (i32, i32) {
        (500, 500)
    }

    fn deps<'a>(
        eyes: &'a Eyes2,
        hands: &'a Hands,
        brain: &'a Brain,
        writer: Option<&'a dyn StructuredWriter>,
        out: PathBuf,
    ) -> Deps<'a, Hands> {
        Deps {
            eyes,
            hands,
            classifier: brain,
            writer,
            cursor: &far_from_corner,
            browser: "Google Chrome".into(),
            email: None,
            sites: Vec::new(),
            apps: Vec::new(),
            options: RunOptions {
                out: Some(out),
                steps: 5,
                delay: 0.0,
                ..RunOptions::default()
            },
        }
    }

    fn decision(kind: &str, confidence: f64) -> Decision {
        Decision {
            kind: ca(kind, confidence),
            item: None,
            site: ca("none", 1.0),
            offscreen: None,
            window: None,
            app: None,
        }
    }

    /// A runner over the given deps, in the state a step leaves it, for the resolve and
    /// conclude tests.
    fn with_runner<R>(
        d: &Deps<Hands>,
        act: bool,
        events: &Recorder,
        f: impl FnOnce(&mut Runner<Hands>) -> R,
    ) -> R {
        let control = Control::new();
        let writer = d.writer.map(WriterAdapter);
        let verifier = VerifierAdapter(d.classifier);
        let ctx = Context {
            goal: GOAL.into(),
            browser: d.browser.clone(),
            email: None,
            writer: writer.as_ref().map(|w| w as &dyn actions::Writer),
            verifier: &verifier,
            history: Vec::new(),
            sites: Vec::new(),
            apps: Vec::new(),
        };
        let out = d.options.out.clone().unwrap();
        let mut r = Runner::new(GOAL, act, &control, events, d, out, ctx);
        f(&mut r)
    }

    // ------------------------------------------------------------ control

    #[test]
    fn a_fresh_control_never_blocks_and_never_aborts() {
        assert!(Control::new().checkpoint().is_ok());
    }

    #[test]
    fn abort_fails_the_next_checkpoint() {
        let control = Control::new();
        control.abort("hotkey");
        assert_eq!(control.checkpoint(), Err(Abort("hotkey".into())));
    }

    #[test]
    fn pause_blocks_until_resumed() {
        let control = Control::new();
        control.pause();
        let (tx, rx) = std::sync::mpsc::channel();
        let c = control.clone();
        std::thread::spawn(move || {
            let _ = c.checkpoint();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a paused checkpoint must not return"
        );
        control.resume();
        assert!(
            rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "resume must release the checkpoint"
        );
    }

    #[test]
    fn abort_wins_over_pause_so_a_paused_run_can_still_be_killed() {
        let control = Control::new();
        control.pause();
        let c = control.clone();
        let waiter = std::thread::spawn(move || c.checkpoint());
        std::thread::sleep(Duration::from_millis(100));
        control.abort("quit");
        assert_eq!(waiter.join().unwrap(), Err(Abort("quit".into())));
    }

    #[test]
    fn toggle_reports_the_state_it_moved_to() {
        let control = Control::new();
        assert!(control.toggle_pause());
        assert!(control.paused());
        assert!(!control.toggle_pause());
        assert!(!control.paused());
    }

    #[test]
    fn reset_clears_both_flags_for_the_next_run() {
        let control = Control::new();
        control.pause();
        control.abort("done with that one");
        control.reset();
        assert!(!control.paused() && !control.aborting());
        assert!(control.checkpoint().is_ok());
    }

    #[test]
    fn the_corner_aborts_and_anywhere_else_does_not() {
        assert_eq!(
            check_corner((0, 0)),
            Err(Abort("mouse in top-left corner".into()))
        );
        assert!(check_corner((4, 4)).is_err());
        assert!(check_corner((5, 0)).is_ok());
        assert!(check_corner((500, 500)).is_ok());
    }

    #[test]
    fn only_the_primary_monitors_corner_aborts_not_a_monitor_above_it() {
        assert!(
            check_corner((1, -1440)).is_ok(),
            "left edge of a monitor above"
        );
        assert!(check_corner((-1920, 2)).is_ok(), "a monitor to the left");
        assert!(check_corner((-2, -2)).is_ok());
        assert!(check_corner((2, 2)).is_err());
    }

    // ------------------------------------------------------------ the answer

    #[test]
    fn a_run_that_has_nothing_to_report_asks_for_no_answer() {
        for outcome in ["dry run", "aborted (Ctrl-C)", "crashed"] {
            let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
            let fake = FakeWriter::new(json!({"achieved": true, "answer": "unused"}));
            let d = deps(&eyes, &hands, &brain, Some(&fake), scratch());
            let events = Recorder::default();
            with_runner(&d, false, &events, |r| {
                r.outcome = outcome.into();
                r.view = Some((screen(), Vec::new()));
                r.conclude().unwrap();
                assert!(r.answer.is_none());
            });
            assert!(fake.calls.borrow().is_empty() && events.lines.borrow().is_empty());
        }
    }

    #[test]
    fn without_a_writer_the_run_says_why_there_is_no_answer() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let d = deps(&eyes, &hands, &brain, None, scratch());
        let events = Recorder::default();
        with_runner(&d, false, &events, |r| {
            r.outcome = "done".into();
            r.view = Some((screen(), Vec::new()));
            r.conclude().unwrap();
            assert!(r.answer.is_none());
        });
        let lines = events.lines.borrow();
        assert!(lines[0].contains("no answer") && lines[0].contains("OPENAI_API_KEY"));
    }

    #[test]
    fn the_last_capture_is_answered_from_when_nothing_acted_after_it() {
        let mut eyes = Eyes2::new();
        eyes.refuse = true;
        let (hands, brain) = (Hands::default(), Brain::kind("wait", 0.9));
        let fake = FakeWriter::new(json!({"achieved": true, "answer": "Sep 19, 2026 in Miami."}));
        let out = scratch();
        let d = deps(&eyes, &hands, &brain, Some(&fake), out.clone());
        let events = Recorder::default();
        with_runner(&d, false, &events, |r| {
            r.outcome = "done".into();
            r.view = Some((screen(), vec![make_item(0, "SEP 19, 2026", 10.0, 30.0)]));
            r.conclude().unwrap();
            assert_eq!(
                r.answer,
                Some(Answer {
                    text: "Sep 19, 2026 in Miami.".into(),
                    achieved: true
                })
            );
        });
        let lines = events.lines.borrow();
        assert!(lines[0].contains("goal achieved") && lines[0].contains("Sep 19, 2026 in Miami."));
        assert!(!out.join("answer-raw.png").exists());
        let call = &fake.calls.borrow()[0];
        assert!(call.packet["why_the_run_stopped"]
            .as_str()
            .unwrap()
            .contains("already achieved"));
        assert_eq!(
            *events.answers.borrow(),
            vec!["Sep 19, 2026 in Miami.".to_string()]
        );
    }

    #[test]
    fn the_screen_is_captured_again_when_an_action_made_the_last_capture_stale() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let fake = FakeWriter::new(json!({"achieved": false, "answer": "No dates on screen."}));
        let out = scratch();
        let d = deps(&eyes, &hands, &brain, Some(&fake), out.clone());
        let events = Recorder::default();
        with_runner(&d, false, &events, |r| {
            r.outcome = "step limit".into();
            r.view = None;
            r.conclude().unwrap();
            assert_eq!(
                r.answer,
                Some(Answer {
                    text: "No dates on screen.".into(),
                    achieved: false
                })
            );
        });
        assert!(events.lines.borrow()[0].contains("goal not achieved"));
        assert!(out.join("answer-raw.png").exists());
        assert_eq!(eyes.captures.get(), 1);
        assert_eq!(
            fake.calls.borrow()[0].packet["screen_text_in_reading_order"],
            json!(["TICKETS"])
        );
    }

    #[test]
    fn a_writer_that_fails_costs_the_answer_and_not_the_run() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let fake = FakeWriter::failing("connection refused");
        let d = deps(&eyes, &hands, &brain, Some(&fake), scratch());
        let events = Recorder::default();
        with_runner(&d, false, &events, |r| {
            r.outcome = "done".into();
            r.view = Some((screen(), Vec::new()));
            assert!(r.conclude().is_ok());
            assert!(r.answer.is_none());
        });
        assert!(events.lines.borrow()[0].contains("no answer: the writer is unavailable"));
    }

    // ------------------------------------------------------------ the stop rules

    #[test]
    fn every_stop_names_its_outcome() {
        for (kind, confidence, outcome) in [
            ("done", 0.9, "done"),
            ("none", 0.9, "nothing helps"),
            ("scroll_down", 0.2, "low confidence"),
            ("scroll_down", 0.9, "dry run"),
        ] {
            let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
            let d = deps(&eyes, &hands, &brain, None, scratch());
            let events = Recorder::default();
            with_runner(&d, false, &events, |r| {
                r.view = Some((screen(), Vec::new()));
                let keep_going = r
                    .resolve(&decision(kind, confidence), &mut Timing::new())
                    .unwrap();
                assert!(!keep_going);
                assert_eq!(r.outcome, outcome);
                assert_eq!(stopped_reason(outcome).is_some(), outcome != "dry run");
            });
            assert!(hands.calls.borrow().is_empty());
        }
    }

    #[test]
    fn an_action_makes_the_last_capture_stale() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let d = deps(&eyes, &hands, &brain, None, scratch());
        let events = Recorder::default();
        with_runner(&d, true, &events, |r| {
            r.view = Some((screen(), Vec::new()));
            let keep_going = r
                .resolve(&decision("scroll_down", 0.9), &mut Timing::new())
                .unwrap();
            assert!(keep_going);
            assert!(r.view.is_none());
            assert_eq!(r.ctx.history, vec!["scrolled down".to_string()]);
        });
        assert_eq!(*hands.calls.borrow(), vec!["scroll -10".to_string()]);
    }

    #[test]
    fn two_no_ops_in_a_row_stall_the_run() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let d = deps(&eyes, &hands, &brain, None, scratch());
        let result = run_goal(GOAL, true, &Control::new(), &Recorder::default(), &d);
        assert_eq!(result.outcome, "stalled");
        assert_eq!(result.history, vec!["waited", "waited"]);
    }

    #[test]
    fn the_same_action_on_the_same_page_three_times_stalls_the_run() {
        let (eyes, hands, brain) = (
            Eyes2::new(),
            Hands::default(),
            Brain::kind("scroll_down", 0.9),
        );
        let d = deps(&eyes, &hands, &brain, None, scratch());
        let result = run_goal(GOAL, true, &Control::new(), &Recorder::default(), &d);
        assert_eq!(result.outcome, "stalled");
        assert_eq!(result.history.len(), 3);
    }

    #[test]
    fn a_run_that_uses_every_step_says_so_and_asks_for_the_answer() {
        let (eyes, hands, brain) = (
            Eyes2::new(),
            Hands::default(),
            Brain::kind("scroll_down", 0.9),
        );
        let fake = FakeWriter::new(json!({"achieved": false, "answer": "Still looking."}));
        let out = scratch();
        let mut d = deps(&eyes, &hands, &brain, Some(&fake), out.clone());
        d.options.steps = 1;
        let events = Recorder::default();
        let result = run_goal(GOAL, true, &Control::new(), &events, &d);
        assert_eq!(result.outcome, "step limit");
        assert_eq!(result.answer.unwrap().text, "Still looking.");
        assert!(out.join("answer-raw.png").exists());
        let run = runs_index::load_run(&out);
        assert_eq!(run.outcome, "step limit");
        assert_eq!(run.answer.as_deref(), Some("Still looking."));
        assert!(run.acted);
        assert_eq!(run.steps_taken, 1);
    }

    #[test]
    fn the_run_folder_has_the_layout_the_index_reads() {
        let (eyes, hands, brain) = (
            Eyes2::new(),
            Hands::default(),
            Brain::kind("click_item", 0.1),
        );
        let out = scratch();
        let d = deps(&eyes, &hands, &brain, None, out.clone());
        let result = run_goal(GOAL, false, &Control::new(), &Recorder::default(), &d);
        assert_eq!(result.outcome, "low confidence");
        let run = runs_index::load_run(&out);
        assert_eq!(run.goal, GOAL);
        assert_eq!(run.outcome, "low confidence");
        assert!(!run.acted);
        let steps = runs_index::steps_of(&out);
        assert_eq!(steps.len(), 1);
        let s = &steps[0];
        assert!(
            s.annotated.is_some() && s.raw.is_some() && s.payload.is_some() && s.answers.is_some()
        );
        let answers = runs_index::step_answers(s);
        assert_eq!(answers["kind"], json!("click_item"));
        assert!(answers["timing"]["total"].is_number());
        assert!(out.join("run.log").exists());
    }

    // ------------------------------------------------------------ the dry run

    #[test]
    fn a_dry_run_never_touches_the_desktop_and_points_with_the_origin_added_once() {
        let (eyes, hands, brain) = (
            Eyes2::new(),
            Hands::default(),
            Brain::kind("click_item", 0.9),
        );
        let out = scratch();
        let d = deps(&eyes, &hands, &brain, None, out.clone());
        let events = Recorder::default();
        let result = run_goal(GOAL, false, &Control::new(), &events, &d);

        assert_eq!(result.outcome, "dry run");
        assert!(result.history.is_empty());
        assert!(
            hands.calls.borrow().is_empty(),
            "a dry run acted: {:?}",
            hands.calls.borrow()
        );
        assert_eq!(brain.calls.get(), 1);

        let answers = runs_index::step_answers(&runs_index::steps_of(&out)[0]);
        let item = &answers["items"][0];
        let (x1, y1) = (item["x1"].as_f64().unwrap(), item["y1"].as_f64().unwrap());
        let marks = events.marks.borrow();
        assert_eq!(marks.len(), 1);
        let (m, seconds) = (&marks[0].0[0], marks[0].1);
        assert_eq!(seconds, HIGHLIGHT_SECONDS);
        assert_eq!((m.x, m.y), (1.0 + x1, -1440.0 + y1));
        assert!(
            m.y < -840.0 && m.y >= -1440.0,
            "the mark is on the upper display"
        );
        assert_eq!(m.label, "would press: TICKETS");
        assert_eq!(m.tone, Tone::Point);
        assert_eq!(*events.states.borrow(), vec![(true, false), (false, false)]);
        // The buddy points at the same box, origin added once, and nothing else is pointed at.
        let points = events.points.borrow();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0], (m.clone(), None, HIGHLIGHT_SECONDS));
    }

    // ------------------------------------------------------------ act preview

    /// Records each point with how many desktop calls had happened by then.
    struct Ordered<'a> {
        hands: &'a Hands,
        points: RefCell<Vec<(Mark, usize)>>,
    }

    impl Events for Ordered<'_> {
        fn line(&self, _: &str) {}
        fn highlight(&self, _: Vec<Mark>, _: f64) {}
        fn answer(&self, _: &str) {}
        fn state(&self, _: bool, _: bool) {}
        fn point(&self, mark: Mark, step: Option<(usize, usize)>, _: f64) {
            assert!(step.is_none());
            self.points
                .borrow_mut()
                .push((mark, self.hands.calls.borrow().len()));
        }
    }

    fn act_on_item(lead_ms: &str) -> (Vec<(Mark, usize)>, Vec<String>, Duration) {
        let _g = crate::writer::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CLICKER_POINT_LEAD_MS", lead_ms);
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let d = deps(&eyes, &hands, &brain, None, scratch());
        let events = Ordered {
            hands: &hands,
            points: RefCell::new(Vec::new()),
        };
        let control = Control::new();
        let verifier = VerifierAdapter(d.classifier);
        let ctx = Context {
            goal: GOAL.into(),
            browser: d.browser.clone(),
            email: None,
            writer: None,
            verifier: &verifier,
            history: Vec::new(),
            sites: Vec::new(),
            apps: Vec::new(),
        };
        let mut r = Runner::new(GOAL, true, &control, &events, &d, scratch(), ctx);
        let mut s = screen();
        s.scale = 1.0;
        s.origin = (1.0, -1440.0);
        let mut item = make_item(0, "Sign in", 0.0, 20.0);
        item.x1 = 40.0;
        item.x2 = 60.0;
        r.view = Some((s, vec![item]));
        let mut dec = decision("click_item", 0.9);
        dec.item = Some(ca("0", 0.9));
        let t = Instant::now();
        assert!(r.resolve(&dec, &mut Timing::new()).unwrap());
        let took = t.elapsed();
        std::env::remove_var("CLICKER_POINT_LEAD_MS");
        let points = events.points.into_inner();
        let calls = hands.calls.borrow().clone();
        (points, calls, took)
    }

    #[test]
    fn an_acting_run_points_at_the_item_before_it_clicks_and_waits_the_lead() {
        let (points, calls, took) = act_on_item("300");
        assert_eq!(points.len(), 1);
        let (m, calls_before) = &points[0];
        assert_eq!(*calls_before, 0, "pointed after the desktop was touched");
        assert_eq!((m.x, m.y, m.w, m.h), (41.0, -1440.0, 20.0, 20.0));
        assert_eq!(m.label, "press: Sign in");
        assert_eq!(calls, vec!["click_at (51.0, -1430.0)".to_string()]);
        assert!(took >= Duration::from_millis(300), "{took:?}");
    }

    #[test]
    fn a_zero_lead_still_points_but_does_not_wait() {
        let (points, calls, took) = act_on_item("0");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].1, 0);
        assert_eq!(calls.len(), 1);
        assert!(took < Duration::from_millis(250), "{took:?}");
    }

    #[test]
    fn an_action_with_nothing_on_screen_is_not_pointed_at() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let d = deps(&eyes, &hands, &brain, None, scratch());
        let events = Recorder::default();
        with_runner(&d, true, &events, |r| {
            r.view = Some((screen(), Vec::new()));
            r.resolve(&decision("scroll_down", 0.9), &mut Timing::new())
                .unwrap();
        });
        assert!(events.points.borrow().is_empty());
    }

    #[test]
    fn the_lead_defaults_to_350_ms() {
        let _g = crate::writer::tests::ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CLICKER_POINT_LEAD_MS");
        assert_eq!(point_lead(), Duration::from_millis(350));
        std::env::set_var("CLICKER_POINT_LEAD_MS", "junk");
        assert_eq!(point_lead(), Duration::from_millis(350));
        std::env::remove_var("CLICKER_POINT_LEAD_MS");
    }

    #[test]
    fn a_spoken_line_gets_60_ms_a_character_within_bounds() {
        assert_eq!(speech_pause("hi"), Duration::from_millis(1500));
        assert_eq!(speech_pause(&"x".repeat(50)), Duration::from_millis(3000));
        assert_eq!(speech_pause(&"x".repeat(500)), Duration::from_millis(6000));
    }

    #[test]
    fn point_at_adds_the_display_origin_exactly_once() {
        let mut s = screen();
        s.scale = 1.0;
        s.origin = (1.0, -1440.0);
        let mut item = make_item(0, "Sign in", 0.0, 20.0);
        item.x1 = 40.0;
        item.x2 = 60.0;
        let mut d = decision("click_item", 0.9);
        d.item = Some(ca("0", 0.9));
        let m = point_at(&s, &[item], &d).unwrap();
        assert_eq!((m.x, m.y, m.w, m.h), (41.0, -1440.0, 20.0, 20.0));
        assert_eq!(m.label, "would press: Sign in");
        assert!(point_at(&s, &[], &d).is_none());
    }

    // ------------------------------------------------------------ aborts

    #[test]
    fn an_aborted_control_stops_the_run_and_still_writes_the_folder() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let out = scratch();
        let d = deps(&eyes, &hands, &brain, None, out.clone());
        let control = Control::new();
        control.abort("quit");
        let result = run_goal(GOAL, true, &control, &Recorder::default(), &d);
        assert_eq!(result.outcome, "aborted (quit)");
        assert_eq!(runs_index::load_run(&out).outcome, "aborted (quit)");
        assert!(hands.calls.borrow().is_empty() && brain.calls.get() == 0);
    }

    #[test]
    fn the_mouse_in_the_corner_aborts_the_run() {
        let (eyes, hands, brain) = (Eyes2::new(), Hands::default(), Brain::kind("wait", 0.9));
        let mut d = deps(&eyes, &hands, &brain, None, scratch());
        let corner = || (0, 0);
        d.cursor = &corner;
        let result = run_goal(GOAL, true, &Control::new(), &Recorder::default(), &d);
        assert_eq!(result.outcome, "aborted (mouse in top-left corner)");
        assert!(hands.calls.borrow().is_empty());
    }

    #[test]
    fn a_classifier_failure_is_a_crash_with_a_run_folder() {
        let (eyes, hands) = (Eyes2::new(), Hands::default());
        let brain = Brain {
            reply: Err(ApiError {
                status: Some(500),
                message: "boom".into(),
            }),
            calls: Cell::new(0),
        };
        let out = scratch();
        let d = deps(&eyes, &hands, &brain, None, out.clone());
        let events = Recorder::default();
        let result = run_goal(GOAL, true, &Control::new(), &events, &d);
        assert_eq!(result.outcome, "crashed");
        assert!(events
            .lines
            .borrow()
            .iter()
            .any(|l| l.contains("crashed: boom")));
        assert_eq!(runs_index::load_run(&out).outcome, "crashed");
    }

    // ------------------------------------------------------------ adapters and talk

    #[test]
    fn the_writer_adapter_passes_the_url_through() {
        let fake = FakeWriter::new(json!({"ok": true, "url": "https://www.brunomars.com"}));
        let w = WriterAdapter(&fake);
        assert_eq!(
            actions::Writer::compose_url(&w, GOAL, &[]),
            Ok("https://www.brunomars.com".to_string())
        );
    }

    #[test]
    fn a_verifier_that_cannot_ask_reads_as_unverified() {
        let brain = Brain {
            reply: Err(ApiError {
                status: None,
                message: "offline".into(),
            }),
            calls: Cell::new(0),
        };
        let field = Field {
            role: "AXTextField".into(),
            label: "Search".into(),
            placeholder: String::new(),
            value: String::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        let v = VerifierAdapter(&brain);
        assert_eq!(
            actions::Verifier::verify_typed(&v, GOAL, &field, "x", None),
            0.0
        );
    }

    #[test]
    fn answer_screen_explains_and_never_acts() {
        let eyes = Eyes2::new();
        let fake = FakeWriter::new(json!({"answer": " Press TICKETS. ", "steps": []}));
        let events = Recorder::default();
        let said = answer_screen(
            "how do I buy tickets?",
            &eyes,
            Some(&fake),
            "",
            &events,
            &Control::new(),
        )
        .unwrap();
        assert_eq!(said, "Press TICKETS.");
        assert_eq!(*events.answers.borrow(), vec![said]);
        assert_eq!(fake.calls.borrow()[0].packet["items"][0]["text"], "TICKETS");
        assert!(events.points.borrow().is_empty());
    }

    fn teach_reply() -> Value {
        json!({"answer": "Tickets are on this page.", "steps": [
            {"text": "Click TICKETS", "item": 0},
            {"text": "Pick a date", "item": null},
            {"text": "Click TICKETS again", "item": 0},
        ]})
    }

    fn no_pause(_: &str) -> Duration {
        Duration::ZERO
    }

    #[test]
    fn teaching_points_at_each_step_in_order_and_says_every_step() {
        let eyes = Eyes2::new();
        let fake = FakeWriter::new(teach_reply());
        let events = Recorder::default();
        let said = answer_screen_paced(
            "how do I buy tickets?",
            &eyes,
            Some(&fake),
            "",
            &events,
            &Control::new(),
            &no_pause,
        )
        .unwrap();
        assert_eq!(said, "Tickets are on this page.");
        assert_eq!(*events.answers.borrow(), vec![said]);
        let points = events.points.borrow();
        // The step with no item is said but not pointed at.
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].1, Some((1, 3)));
        assert_eq!(points[1].1, Some((3, 3)));
        assert_eq!(points[0].0.label, "Click TICKETS");
        assert_eq!(points[1].0.label, "Click TICKETS again");
        // On the display at (1, -1440): the origin is added once.
        assert!(points[0].0.y >= -1440.0 && points[0].0.y < -840.0);
        assert!(points[0].0.x >= 1.0 && points[0].0.x < 801.0);
        assert_eq!(
            *events.spoken.borrow(),
            vec!["Click TICKETS", "Pick a date", "Click TICKETS again"]
        );
        let lines = events.lines.borrow();
        assert_eq!(
            *lines,
            vec![
                "step 1/3: Click TICKETS",
                "step 2/3: Pick a date",
                "step 3/3: Click TICKETS again"
            ]
        );
        assert_eq!(events.clears.get(), 1);
    }

    #[test]
    fn a_spoken_stop_ends_the_teaching_after_the_current_step() {
        let eyes = Eyes2::new();
        let fake = FakeWriter::new(teach_reply());
        let control = Control::new();
        let events = Recorder {
            stop_at_point: Some(control.clone()),
            ..Recorder::default()
        };
        answer_screen_paced(
            "how do I buy tickets?",
            &eyes,
            Some(&fake),
            "",
            &events,
            &control,
            &no_pause,
        )
        .unwrap();
        assert_eq!(events.points.borrow().len(), 1);
        assert_eq!(events.spoken.borrow().len(), 1);
        assert_eq!(events.clears.get(), 1);
        assert!(events
            .lines
            .borrow()
            .last()
            .unwrap()
            .contains("teaching stopped"));
    }

    #[test]
    fn answer_screen_without_a_writer_says_why() {
        let events = Recorder::default();
        let said = answer_screen(
            "what is this?",
            &Eyes2::new(),
            None,
            "",
            &events,
            &Control::new(),
        )
        .unwrap();
        assert!(said.contains("no writer"));
        assert_eq!(events.answers.borrow().len(), 1);
    }

    /// Teach on the real screen: reads it and asks OpenAI, never acts. Prints the steps.
    #[test]
    #[ignore = "network: reads the real screen and makes one OpenAI call"]
    fn live_teach_prints_the_steps() {
        struct Print;
        impl Events for Print {
            fn line(&self, text: &str) {
                println!("{text}");
            }
            fn highlight(&self, marks: Vec<Mark>, seconds: f64) {
                println!("highlight {marks:?} for {seconds}s");
            }
            fn answer(&self, text: &str) {
                println!("answer: {text}");
            }
            fn state(&self, _: bool, _: bool) {}
            fn point(&self, mark: Mark, step: Option<(usize, usize)>, hold: f64) {
                println!(
                    "point {step:?} at ({:.0}, {:.0}, {:.0}x{:.0}) for {hold:.1}s: {}",
                    mark.x, mark.y, mark.w, mark.h, mark.label
                );
            }
            fn clear_point(&self) {
                println!("clear point");
            }
        }
        let env = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../.env");
        let _ = crate::config::load_dotenv(Path::new(env));
        let ai = writer::make_writer().expect("OPENAI_API_KEY");
        let t = Instant::now();
        let said = answer_screen_paced(
            "how do I open settings here?",
            &perception::LiveDesktop,
            Some(&ai),
            &crate::config::browser(),
            &Print,
            &Control::new(),
            &no_pause,
        );
        println!("{said:?} in {} ms", t.elapsed().as_millis());
        assert!(said.is_ok());
    }

    /// A real dry run on the real desktop: looks, decides, prints, never acts. Needs TypeSafe.
    #[test]
    #[ignore]
    fn live_dry_run_prints_the_decision() {
        struct Print;
        impl Events for Print {
            fn line(&self, text: &str) {
                println!("{text}");
            }
            fn highlight(&self, marks: Vec<Mark>, seconds: f64) {
                println!("highlight {marks:?} for {seconds}s");
            }
            fn answer(&self, text: &str) {
                println!("answer: {text}");
            }
            fn state(&self, running: bool, paused: bool) {
                println!("state running={running} paused={paused}");
            }
        }
        let _ = crate::config::load_dotenv(Path::new("../../../.env"));
        let typesafe = api::typesafe::TypeSafe::from_env().expect("TypeSafe credentials");
        let hands = Hands::default();
        let d = Deps {
            eyes: &perception::LiveDesktop,
            hands: &hands,
            classifier: &typesafe,
            writer: None,
            cursor: &live_cursor,
            browser: crate::config::browser(),
            email: None,
            sites: Vec::new(),
            apps: Vec::new(),
            options: RunOptions {
                out: Some(scratch()),
                ..RunOptions::default()
            },
        };
        let result = run_goal("what time is it", false, &Control::new(), &Print, &d);
        println!("{result:?}");
        assert!(hands.calls.borrow().is_empty());
    }
}
