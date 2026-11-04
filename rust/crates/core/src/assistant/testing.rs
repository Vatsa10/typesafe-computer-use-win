//! Fakes for the assistant layer, used by this crate's tests and by the app's. Nothing here
//! touches the network, the screen, the voice or a shell.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::chat::{Cancel, Chat, ChatError, ChatReply, ToolCall};
use serde_json::{json, Value};

use super::gate::{Gate, Unavailable};
use super::pending::Pending;
use super::{
    Assistant, Emit, Hands, Limits, ObsItem, Observation, RunSummary, Senses, Services, ShellExec,
    ShellOutput, Voice,
};

// ---------------------------------------------------------------- the model

pub enum Script {
    Reply(ChatReply),
    /// Block until the call is cancelled, as a slow model would.
    Hang,
    Fail(ChatError),
}

#[derive(Debug, Clone)]
pub struct SeenCall {
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
}

pub struct FakeChat {
    script: Mutex<VecDeque<Script>>,
    calls: Mutex<Vec<SeenCall>>,
}

/// A tool call as the model would write it.
pub fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.to_string(),
    }
}

pub fn calling(calls: Vec<ToolCall>) -> Script {
    Script::Reply(ChatReply {
        content: String::new(),
        tool_calls: calls,
        ..Default::default()
    })
}

pub fn saying(text: &str) -> Script {
    Script::Reply(ChatReply {
        content: text.into(),
        tool_calls: Vec::new(),
        ..Default::default()
    })
}

impl FakeChat {
    pub fn scripted(script: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Plain text replies, one per call.
    pub fn say<const N: usize>(texts: [&str; N]) -> Self {
        Self {
            script: Mutex::new(texts.iter().map(|t| saying(t)).collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn failing() -> Self {
        Self {
            script: Mutex::new(VecDeque::from([Script::Fail(ChatError::Http(
                500,
                "down".into(),
            ))])),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<SeenCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl Chat for FakeChat {
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        cancel: &Cancel,
    ) -> Result<ChatReply, ChatError> {
        self.calls.lock().unwrap().push(SeenCall {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
        });
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(Script::Reply(r)) => Ok(r),
            Some(Script::Fail(e)) => Err(e),
            Some(Script::Hang) => {
                let end = Instant::now() + Duration::from_secs(10);
                while !cancel.is_cancelled() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(ChatError::Cancelled)
            }
            None => Ok(ChatReply {
                content: "ok".into(),
                tool_calls: Vec::new(),
                ..Default::default()
            }),
        }
    }
}

// ---------------------------------------------------------------- the world

#[derive(Default)]
pub struct FakeEmit(pub Mutex<Vec<(String, Value)>>);

impl Emit for FakeEmit {
    fn emit(&self, event: &str, payload: Value) {
        self.0.lock().unwrap().push((event.to_string(), payload));
    }
}

impl FakeEmit {
    pub fn all(&self) -> Vec<(String, Value)> {
        self.0.lock().unwrap().clone()
    }
    pub fn of(&self, event: &str) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|(e, _)| e == event)
            .map(|(_, v)| v)
            .collect()
    }
    pub fn names(&self) -> Vec<String> {
        self.all().into_iter().map(|(e, _)| e).collect()
    }
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
    /// Wait until `what` holds, or panic after five seconds.
    pub fn wait_for(&self, what: impl Fn(&FakeEmit) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !what(self) {
            assert!(Instant::now() < end, "timed out; saw {:?}", self.names());
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[derive(Default)]
pub struct FakeVoice {
    pub spoken: Mutex<Vec<String>>,
    pub stops: AtomicUsize,
    /// How long a line takes to say, unless stopped.
    pub take_ms: AtomicUsize,
    stopped: AtomicBool,
}

impl Voice for FakeVoice {
    fn speak(&self, text: &str, cancel: &Cancel) -> bool {
        self.spoken.lock().unwrap().push(text.to_string());
        self.stopped.store(false, Ordering::SeqCst);
        let end =
            Instant::now() + Duration::from_millis(self.take_ms.load(Ordering::SeqCst) as u64);
        while Instant::now() < end && !cancel.is_cancelled() && !self.stopped.load(Ordering::SeqCst)
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }
    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.stopped.store(true, Ordering::SeqCst);
    }
}

pub struct FakeSenses(pub Mutex<Observation>);

/// An observation of a second display at (2560, -1440), scale 1.5, with items 0 and 1.
pub fn sample_observation() -> Observation {
    let item = |index, text: &str, x1, y1, x2, y2| ObsItem {
        index,
        text: text.into(),
        role: "button".into(),
        x1,
        y1,
        x2,
        y2,
    };
    Observation {
        origin: (2560.0, -1440.0),
        scale: 1.5,
        items: vec![
            item(0, "Save", 150.0, 75.0, 300.0, 135.0),
            item(1, "Send", 300.0, 150.0, 450.0, 210.0),
        ],
        summary: json!({ "app": "Notepad",
            "items": [{ "index": 0, "text": "Save" }, { "index": 1, "text": "Send" }] }),
    }
}

impl Senses for FakeSenses {
    fn observe(&self) -> Result<Observation, String> {
        Ok(self.0.lock().unwrap().clone())
    }
}

pub struct FakeHands {
    pub runs: Mutex<Vec<(String, bool)>>,
    pub hang: AtomicBool,
}

impl Hands for FakeHands {
    fn run_goal(&self, goal: &str, act: bool, cancel: &Cancel) -> Result<RunSummary, String> {
        self.runs.lock().unwrap().push((goal.to_string(), act));
        if self.hang.load(Ordering::SeqCst) {
            let end = Instant::now() + Duration::from_secs(10);
            while !cancel.is_cancelled() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            return Err("the run was stopped".into());
        }
        Ok(RunSummary {
            outcome: if act { "done".into() } else { "dry run".into() },
            answer: "It is open.".into(),
            seconds: 1.25,
        })
    }
}

#[derive(Default)]
pub struct FakeShell {
    pub ran: Mutex<Vec<String>>,
}

impl ShellExec for FakeShell {
    fn run(
        &self,
        command: &str,
        _: Option<&Path>,
        _: Duration,
        _: &Cancel,
    ) -> Result<ShellOutput, String> {
        self.ran.lock().unwrap().push(command.to_string());
        Ok(ShellOutput {
            code: Some(0),
            output: "hello from the shell".into(),
        })
    }
}

// ---------------------------------------------------------------- the whole thing

pub struct Harness {
    pub svc: Arc<Services>,
    pub chat: Arc<FakeChat>,
    pub emit: Arc<FakeEmit>,
    pub voice: Arc<FakeVoice>,
    pub hands: Arc<FakeHands>,
    pub shell: Arc<FakeShell>,
    pub act: Arc<AtomicBool>,
    pub camera: Arc<AtomicBool>,
    pub home: PathBuf,
    pub profile: PathBuf,
}

fn fresh(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("assist-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Harness {
    pub fn new(tag: &str, script: Vec<Script>) -> Self {
        Self::with(
            tag,
            FakeChat::scripted(script),
            Limits {
                confirm_wait: Duration::from_millis(300),
                camera_wait: Duration::from_millis(300),
                ..Limits::default()
            },
        )
    }

    pub fn with(tag: &str, chat: Arc<FakeChat>, limits: Limits) -> Self {
        Self::with_senses(tag, chat, limits, Arc::new(FakeSenses(Mutex::new(sample_observation()))))
    }

    pub fn with_senses(
        tag: &str,
        chat: Arc<FakeChat>,
        limits: Limits,
        senses: Arc<dyn Senses>,
    ) -> Self {
        let root = fresh(tag);
        let (home, profile) = (root.join("home"), root.join("profile"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let emit = Arc::new(FakeEmit::default());
        let voice = Arc::new(FakeVoice::default());
        let hands = Arc::new(FakeHands {
            runs: Mutex::new(Vec::new()),
            hang: AtomicBool::new(false),
        });
        let shell = Arc::new(FakeShell::default());
        let act = Arc::new(AtomicBool::new(false));
        let camera = Arc::new(AtomicBool::new(false));
        let (c, a, cam) = (chat.clone(), act.clone(), camera.clone());
        let svc = Arc::new(Services {
            chat: Arc::new(move || Ok(c.clone() as Arc<dyn Chat>)),
            emit: emit.clone(),
            voice: voice.clone(),
            senses,
            hands: hands.clone(),
            shell: shell.clone(),
            act_mode: Arc::new(move || a.load(Ordering::SeqCst)),
            camera_enabled: Arc::new(move || cam.load(Ordering::SeqCst)),
            home: home.clone(),
            profile: profile.clone(),
            limits,
            confirms: Pending::new(),
            cameras: Pending::new(),
            documents: Arc::new(crate::documents::Documents::new(
                home.join("docs-home"),
                vec![profile.clone()],
                "",
            )),
            writer: Arc::new(|| Err("no writer".into())),
            autonomy_full: Arc::new(|| false),
        });
        Self {
            svc,
            chat,
            emit,
            voice,
            hands,
            shell,
            act,
            camera,
            home,
            profile,
        }
    }

    pub fn assistant(&self) -> Arc<Assistant> {
        Assistant::new(
            self.svc.clone(),
            Gate::new(Arc::new(Unavailable)).with_enabled(Some(true)),
        )
    }

    /// An assistant whose decision model (gate and fast path) is `classifier`.
    pub fn assistant_with(
        &self,
        classifier: Arc<dyn crate::decide::Classifier + Send + Sync>,
    ) -> Arc<Assistant> {
        Assistant::new(self.svc.clone(), Gate::new(classifier).with_enabled(Some(true)))
    }

    /// Answer the next `confirm` event, in the background.
    pub fn answer_confirms(&self, allow: bool) {
        let (svc, emit) = (self.svc.clone(), self.emit.clone());
        std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(5);
            let mut answered = 0;
            while Instant::now() < end && answered < 8 {
                for c in emit.of("confirm") {
                    let id = c["id"].as_str().unwrap().to_string();
                    if svc.confirms.reply(&id, allow) {
                        answered += 1;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
    }
}
