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
pub mod control;
pub mod gate;
pub mod pending;
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

pub trait Senses: Send + Sync {
    fn observe(&self) -> Result<Observation, String>;
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
}

impl Assistant {
    pub fn new(svc: Arc<Services>, gate: Gate) -> Arc<Self> {
        let conv = Conversation::load_latest(&svc.conversations_dir())
            .unwrap_or_else(|| Conversation::new(&svc.conversations_dir()));
        let tasks = Tasks::load(svc.clone());
        Arc::new(Self {
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
            system: agent::system_prompt((self.svc.act_mode)(), (self.svc.camera_enabled)()),
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
        *self.memory.lock().unwrap_or_else(|p| p.into_inner()) = Memory::default();
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
