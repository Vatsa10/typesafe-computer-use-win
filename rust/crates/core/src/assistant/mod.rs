//! The assistant layer: a parent agent that owns the conversation, with the existing perception
//! and action loop as its hands.
//!
//! Everything that touches the world is behind a trait in this file (`Emit`, `Voice`, `Senses`,
//! `Hands`, `ShellExec`) or a closure in [`Services`], so the whole layer is tested with fakes:
//! no network, no screen, no voice, no shell.
//!
//! One foreground turn at a time. A new `chat` cancels the unfinished turn (the model call, a
//! running `pointer_run`, a pending confirm, the speech) and continues the same conversation. Only
//! the `interact` tool reaches the user; a turn that ends without it shows its text, silently.

pub mod agent;
pub mod answers;
pub mod control;
pub mod fastpath;
pub mod forms;
pub mod gate;
pub mod pending;
pub mod readonly;
pub mod safety;
pub mod shell;
pub mod store;
pub mod tasks;
pub mod testing;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::chat::{Cancel, Chat};
use serde_json::{json, Value};

use self::agent::{LoopCfg, LoopEnd};
use self::gate::{Gate, GateResult};
use self::pending::Pending;
use self::store::Conversation;
use self::tasks::Tasks;
use self::tools::TurnCtx;

// ---------------------------------------------------------------- modes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A parent agent owns the conversation.
    Assistant,
    /// TypeSafe routing only: the behaviour before the assistant layer.
    Quick,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Assistant => "assistant",
            Mode::Quick => "quick",
        }
    }
}

/// `POINTER_MODE` and whether an agent key exists. Assistant is the default when there is a key;
/// without one the answer is always quick, never a failure.
pub fn resolve_mode(requested: Option<&str>, has_key: bool) -> Mode {
    if !has_key {
        return Mode::Quick;
    }
    match requested.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("quick") => Mode::Quick,
        _ => Mode::Assistant,
    }
}

/// The mode now in force, from the environment.
pub fn current_mode() -> Mode {
    resolve_mode(
        std::env::var("POINTER_MODE").ok().as_deref(),
        api::chat::agent_key().is_some(),
    )
}

/// `POINTER_AUTONOMY`: full unless set to ask.
pub fn autonomy_setting() -> bool {
    !matches!(
        std::env::var("POINTER_AUTONOMY")
            .ok()
            .map(|v| v.trim().to_lowercase())
            .as_deref(),
        Some("ask")
    )
}

/// `POINTER_CAMERA`: off unless set to on, 1, true or yes.
pub fn camera_setting() -> bool {
    matches!(
        std::env::var("POINTER_CAMERA")
            .ok()
            .map(|v| v.trim().to_lowercase())
            .as_deref(),
        Some("on" | "1" | "true" | "yes")
    )
}

// ---------------------------------------------------------------- the world, as traits

/// Events to the shell.
pub trait Emit: Send + Sync {
    fn emit(&self, event: &str, payload: Value);
}

/// The voice. `speak` blocks until the line has been said, `stop` was called, or `cancel` was
/// set; it returns whether anything was spoken. `stop` cuts off whatever is being said now.
pub trait Voice: Send + Sync {
    fn speak(&self, text: &str, cancel: &Cancel) -> bool;
    fn stop(&self);
}

/// One item of the latest observe, in the capture's own pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct ObsItem {
    pub index: usize,
    pub text: String,
    pub role: String,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

/// What `observe` saw: the numbered items, and a summary for the model (items, windows, displays,
/// the mouse).
#[derive(Debug, Clone)]
pub struct Observation {
    /// The captured display's top-left on the virtual desktop, physical pixels.
    pub origin: (f64, f64),
    /// Capture pixels per desktop pixel.
    pub scale: f64,
    pub items: Vec<ObsItem>,
    pub summary: Value,
}

impl Observation {
    /// An item's box on the virtual desktop in physical pixels: x, y, w, h. The display's origin
    /// is added exactly once, as `Screen::to_points` does for a click.
    pub fn rect(&self, index: usize) -> Option<(f64, f64, f64, f64)> {
        let it = self.items.iter().find(|i| i.index == index)?;
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        Some((
            self.origin.0 + it.x1 / s,
            self.origin.1 + it.y1 / s,
            (it.x2 - it.x1) / s,
            (it.y2 - it.y1) / s,
        ))
    }
}

/// Where TypeSafe found something on the screen, on the virtual desktop in physical pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// TypeSafe's box confidence. Not a reliable gate: it reads low on exact boxes.
    pub confidence: f64,
}

pub trait Senses: Send + Sync {
    fn observe(&self) -> Result<Observation, String>;

    /// Ask TypeSafe where `what` is on the screen under the mouse. Looks only; never clicks.
    fn locate(&self, _what: &str) -> Result<Located, String> {
        Err("locating is not available here".into())
    }

    /// A fresh look at the whole machine (windows, browsers' tab titles, the mouse).
    fn world(&self) -> Result<crate::world::Snapshot, String> {
        Err("the world cannot be read here".into())
    }

    /// The installed apps Pointer can launch.
    fn catalog(&self) -> Vec<crate::world::CatApp> {
        Vec::new()
    }

    /// Launch an app from the catalog and report what appeared. Only called in Act mode.
    fn open_app(&self, _app: &crate::world::CatApp) -> Result<String, String> {
        Err("launching apps is not available here".into())
    }

    /// The form controls of a window (text fields, radios, checkboxes, selects), read from the
    /// page, including those scrolled out of view. `Foreground` must be a browser; `Named` is a
    /// window the caller named; `Bound` is the window an earlier read chose. The view says which
    /// window it came from.
    fn form_read(&self, _target: &forms::Target) -> Result<forms::PageView, String> {
        Err("forms cannot be read here".into())
    }

    /// Fill entries into the window `target` (a handle from `form_read`) with real focus and
    /// keystrokes, then read the page back and say per entry whether it holds. Before every
    /// keystroke it must check that the front window is `target` and the focused element is the
    /// expected field, and stop with an error if not. Only called in Act mode.
    fn form_fill(&self, _target: isize, _entries: &[forms::FormEntry]) -> Result<Vec<forms::FormResult>, String> {
        Err("forms cannot be filled here".into())
    }

    /// Press the button or link named `label` in window `target` with real focus and keys, after
    /// checking the front window is `target` and the button holds the focus; wait for the page to
    /// settle, and say whether it changed. Only called in Act mode.
    fn form_click(&self, _target: isize, _label: &str) -> Result<forms::ClickReport, String> {
        Err("pages cannot be clicked here".into())
    }

    /// Bring the window `hwnd` forward. Only called in Act mode.
    fn focus_window(&self, _hwnd: isize) -> Result<(), String> {
        Err("focusing windows is not available here".into())
    }

    /// Press the browser tab matching `title`. Only called in Act mode.
    fn switch_tab(&self, _title: &str) -> Result<String, String> {
        Err("switching tabs is not available here".into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub outcome: String,
    pub answer: String,
    pub seconds: f64,
}

/// The existing run loop, as a blocking call. `act` false is a dry run.
pub trait Hands: Send + Sync {
    fn run_goal(&self, goal: &str, act: bool, cancel: &Cancel) -> Result<RunSummary, String>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellOutput {
    pub code: Option<i32>,
    pub output: String,
}

pub trait ShellExec: Send + Sync {
    fn run(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<ShellOutput, String>;
}

// ---------------------------------------------------------------- services

#[derive(Debug, Clone)]
pub struct Limits {
    /// Model calls in one foreground turn.
    pub max_iterations: usize,
    pub turn_time: Duration,
    pub task_iterations: usize,
    pub task_time: Duration,
    pub confirm_wait: Duration,
    pub camera_wait: Duration,
    pub shell_timeout: Duration,
    /// Serialized history above this many characters is compacted.
    pub compact_chars: usize,
    /// Messages kept verbatim when compacting.
    pub keep_recent: usize,
    pub max_tasks: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_iterations: 12,
            turn_time: Duration::from_secs(180),
            task_iterations: 30,
            task_time: Duration::from_secs(900),
            confirm_wait: Duration::from_secs(60),
            camera_wait: Duration::from_secs(10),
            shell_timeout: Duration::from_secs(60),
            compact_chars: 60_000,
            keep_recent: 8,
            max_tasks: 3,
        }
    }
}

pub type ChatSource = Arc<dyn Fn() -> Result<Arc<dyn Chat>, String> + Send + Sync>;
pub type Flag = Arc<dyn Fn() -> bool + Send + Sync>;
/// The structured writer, resolved per call like the chat model. Err says why there is none.
pub type WriterSource =
    Arc<dyn Fn() -> Result<Box<dyn crate::writer::StructuredWriter>, String> + Send + Sync>;

pub struct Services {
    /// The model, resolved per call so a key saved in Settings applies at once.
    pub chat: ChatSource,
    pub emit: Arc<dyn Emit>,
    pub voice: Arc<dyn Voice>,
    pub senses: Arc<dyn Senses>,
    pub hands: Arc<dyn Hands>,
    pub shell: Arc<dyn ShellExec>,
    /// The panel's Act mode.
    pub act_mode: Flag,
    /// The camera setting.
    pub camera_enabled: Flag,
    /// `POINTER_HOME`: conversations, the task registry.
    pub home: PathBuf,
    /// The user's profile folder: the only place files are read or written.
    pub profile: PathBuf,
    pub limits: Limits,
    pub confirms: Pending<bool>,
    /// Camera frames (data URLs) or an error message.
    pub cameras: Pending<Result<Vec<String>, String>>,
    /// The document index and profile cache for find_documents and get_profile.
    pub documents: Arc<crate::documents::Documents>,
    pub writer: WriterSource,
    /// Autonomy is full: nothing is asked except payments, deleting or overwriting the user's
    /// files, and passwords. False is `ask`: writes, shell and resumes need a yes.
    pub autonomy_full: Flag,
}

impl Services {
    pub fn conversations_dir(&self) -> PathBuf {
        self.home.join("conversations")
    }

    pub fn emit(&self, event: &str, payload: Value) {
        self.emit.emit(event, payload);
    }

    pub fn assistant_state(&self, state: &str, detail: &str) {
        self.emit("assistant", json!({ "state": state, "detail": detail }));
    }
}

// ---------------------------------------------------------------- the assistant

/// What the gate needs to know about the conversation.
#[derive(Debug, Default)]
pub struct Memory {
    pub current_request: String,
    pub last_spoken: String,
    /// What Pointer knows about the machine, kept across turns.
    pub world: crate::world::World,
    /// The user said yes to sending a resume's text to the model, for this session.
    pub profile_ok: bool,
    /// The profile get_profile read last, for form_answer.
    pub profile: Option<crate::documents::Profile>,
}

struct TurnSlot {
    cancel: Cancel,
}

pub struct Assistant {
    svc: Arc<Services>,
    conv: Mutex<Conversation>,
    turn: Mutex<TurnSlot>,
    tasks: Arc<Tasks>,
    gate: Gate,
    memory: Mutex<Memory>,
    busy: AtomicUsize,
    classifier: Arc<dyn crate::decide::Classifier + Send + Sync>,
    fast: Mutex<fastpath::FastCfg>,
}

impl Assistant {
    pub fn new(svc: Arc<Services>, gate: Gate) -> Arc<Self> {
        let conv = Conversation::load_latest(&svc.conversations_dir())
            .unwrap_or_else(|| Conversation::new(&svc.conversations_dir()));
        let tasks = Tasks::load(svc.clone());
        let classifier = gate.classifier();
        Arc::new(Self {
            classifier,
            fast: Mutex::new(fastpath::FastCfg::from_env()),
            svc,
            conv: Mutex::new(conv),
            turn: Mutex::new(TurnSlot {
                cancel: Cancel::new(),
            }),
            tasks,
            gate,
            memory: Mutex::new(Memory::default()),
            busy: AtomicUsize::new(0),
        })
    }

    /// Replace the fast path's settings (tests and the shell).
    pub fn set_fast(&self, cfg: fastpath::FastCfg) {
        *self.fast.lock().unwrap_or_else(|p| p.into_inner()) = cfg;
    }

    pub fn services(&self) -> &Arc<Services> {
        &self.svc
    }

    pub fn tasks(&self) -> &Arc<Tasks> {
        &self.tasks
    }

    /// Whether a foreground turn is in progress.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    /// Cancel the unfinished turn, if any, and register a new one.
    fn begin_turn(&self) -> Cancel {
        let mut slot = self.turn.lock().unwrap_or_else(|p| p.into_inner());
        slot.cancel.cancel();
        slot.cancel = Cancel::new();
        slot.cancel.clone()
    }

    /// A line from the user: voice or typed. Returns at once; the turn runs on its own thread.
    /// An unfinished turn is cancelled first and the same conversation carries on.
    pub fn chat(self: &Arc<Self>, text: &str, source: &str) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("chat needs text".into());
        }
        let cancel = self.begin_turn();
        self.svc.voice.stop();
        self.busy.fetch_add(1, Ordering::SeqCst);
        let me = self.clone();
        let (text, source) = (text.to_string(), source.to_string());
        let spawned = std::thread::Builder::new()
            .name("assistant".into())
            .spawn(move || {
                let _guard = BusyGuard(&me.busy);
                me.run_turn(&text, &source, &cancel);
            });
        if let Err(e) = spawned {
            self.busy.fetch_sub(1, Ordering::SeqCst);
            return Err(format!("could not start the turn: {e}"));
        }
        Ok(())
    }

    /// The turn itself, on the calling thread. Tests call this; `chat` calls it on a thread.
    pub fn run_turn(&self, text: &str, source: &str, cancel: &Cancel) {
        // The conversation lock is the queue: a cancelled turn releases it quickly, and a turn
        // that was superseded before it began never starts.
        let mut conv = self.conv.lock().unwrap_or_else(|p| p.into_inner());
        if cancel.is_cancelled() {
            return;
        }
        {
            let mut m = self.memory.lock().unwrap_or_else(|p| p.into_inner());
            m.current_request = text.to_string();
        }
        self.svc.assistant_state("thinking", "");
        let _ = source; // voice and typed lines are handled alike
        conv.messages
            .push(json!({ "role": "user", "content": text }));
        let fast_cfg = self.fast.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if fast_cfg.enabled {
            let env = fastpath::FastEnv {
                svc: &self.svc,
                tasks: &self.tasks,
                memory: &self.memory,
                classifier: self.classifier.clone(),
                cfg: &fast_cfg,
                cancel,
            };
            if fastpath::try_fast(&env, text, &mut conv.messages) {
                store::strip_images(&mut conv.messages);
                store::save(&self.svc.conversations_dir(), &conv.id, &conv.messages);
                if !cancel.is_cancelled() {
                    self.svc.assistant_state("idle", "");
                }
                return;
            }
        }
        let chat = match (self.svc.chat)() {
            Ok(c) => c,
            Err(e) => {
                self.finish_unreached(&mut conv, &e);
                return;
            }
        };
        let cfg = LoopCfg {
            svc: &self.svc,
            chat: chat.as_ref(),
            kind: tools::Kind::Foreground,
            label: None,
            cancel,
            tasks: &self.tasks,
            memory: &self.memory,
            system: agent::system_prompt(
                (self.svc.act_mode)(),
                (self.svc.camera_enabled)(),
                (self.svc.autonomy_full)(),
            ),
            max_iterations: self.svc.limits.max_iterations,
            time: self.svc.limits.turn_time,
        };
        let mut ctx = TurnCtx::default();
        let dir = self.svc.conversations_dir();
        let id = conv.id.clone();
        let end = agent::run(&cfg, &mut ctx, &mut conv.messages, &|m: &[Value]| {
            store::save(&dir, &id, m)
        });
        match end {
            LoopEnd::Cancelled => {}
            LoopEnd::Final(text) => {
                if !ctx.interacted && !text.trim().is_empty() {
                    self.svc.emit("reply", json!({ "text": text.trim() }));
                }
            }
            LoopEnd::Failed(why) => {
                self.svc.emit(
                    "reply",
                    json!({ "text": format!("I could not do that: {why}") }),
                );
                self.svc
                    .emit("line", json!({ "text": format!("assistant: {why}") }));
            }
            LoopEnd::OutOfSteps => {
                self.svc.emit("reply", json!({ "text":
                    "I used all the steps I am allowed on that and stopped. Tell me how to carry on." }));
            }
            LoopEnd::OutOfTime => {
                self.svc.emit(
                    "reply",
                    json!({ "text":
                    "That took longer than I am allowed, so I stopped. Tell me how to carry on." }),
                );
            }
        }
        store::strip_images(&mut conv.messages);
        store::save(&dir, &id, &conv.messages);
        if !cancel.is_cancelled() {
            self.svc.assistant_state("idle", "");
        }
    }

    /// The model could not even be chosen (no key): say so, and keep the conversation tidy.
    fn finish_unreached(&self, conv: &mut Conversation, why: &str) {
        conv.messages.pop();
        self.svc.emit(
            "reply",
            json!({ "text": format!("I could not do that: {why}") }),
        );
        self.svc.assistant_state("idle", "");
    }

    /// The gate: is what was just heard meant for the assistant, while it speaks?
    pub fn gate(&self, heard: &str) -> GateResult {
        let (request, spoken) = {
            let m = self.memory.lock().unwrap_or_else(|p| p.into_inner());
            (m.current_request.clone(), m.last_spoken.clone())
        };
        self.gate.decide(heard, &request, &spoken, Instant::now())
    }

    /// Stop the speech only.
    pub fn interrupt(&self) {
        self.svc.voice.stop();
    }

    /// Stop everything: the turn, the run it started, the speech and the background tasks.
    pub fn stop_all(&self) {
        self.begin_turn().cancel();
        self.svc.voice.stop();
        self.tasks.stop_all();
        self.svc.assistant_state("idle", "");
    }

    /// Start afresh: the old conversation stays on disk, and the next chat begins a new one.
    pub fn new_conversation(&self) {
        let cancel = self.begin_turn();
        cancel.cancel();
        self.svc.voice.stop();
        let mut conv = self.conv.lock().unwrap_or_else(|p| p.into_inner());
        *conv = Conversation::new(&self.svc.conversations_dir());
        {
            // The conversation starts over; what Pointer knows about the machine does not.
            let mut m = self.memory.lock().unwrap_or_else(|p| p.into_inner());
            m.current_request.clear();
            m.last_spoken.clear();
        }
        self.svc.assistant_state("idle", "");
    }

    /// The shell's answer to a `confirm` event.
    pub fn confirm_reply(&self, id: &str, allow: bool) -> bool {
        self.svc.confirms.reply(id, allow)
    }

    /// The shell's answer to a `camera` event.
    pub fn camera_frames(&self, id: &str, images: Vec<String>, error: Option<String>) -> bool {
        let reply = match error {
            Some(e) if !e.is_empty() => Err(e),
            _ => Ok(images),
        };
        self.svc.cameras.reply(id, reply)
    }

    pub fn messages_len(&self) -> usize {
        self.conv
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messages
            .len()
    }
}

struct BusyGuard<'a>(&'a AtomicUsize);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn the_mode_follows_the_key_and_the_setting() {
        assert_eq!(resolve_mode(None, true), Mode::Assistant);
        assert_eq!(resolve_mode(Some("assistant"), true), Mode::Assistant);
        assert_eq!(resolve_mode(Some(" Quick "), true), Mode::Quick);
        assert_eq!(resolve_mode(Some("garbage"), true), Mode::Assistant);
        // No key: quick, whatever was asked for.
        assert_eq!(resolve_mode(Some("assistant"), false), Mode::Quick);
        assert_eq!(resolve_mode(None, false), Mode::Quick);
    }

    #[test]
    fn marks_resolve_to_physical_pixels_with_the_origin_added_once() {
        let obs = Observation {
            origin: (2560.0, -1440.0),
            scale: 1.5,
            items: vec![ObsItem {
                index: 7,
                text: "Send".into(),
                role: "button".into(),
                x1: 300.0,
                y1: 150.0,
                x2: 450.0,
                y2: 210.0,
            }],
            summary: json!({}),
        };
        assert_eq!(obs.rect(7), Some((2760.0, -1340.0, 100.0, 40.0)));
        assert_eq!(obs.rect(8), None);
    }
}
