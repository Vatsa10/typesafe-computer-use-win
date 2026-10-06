//! The boundary between the daemon and the loop. The daemon owns the pipe, the hotkeys and the
//! microphone; whatever drives a run sits behind [`Runner`] and talks back through [`Events`].

use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::emit::Emitter;

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
    #[allow(dead_code)] // read by `answer`, which the runner will call
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

    #[allow(dead_code)] // for the core runner, wired later
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
    #[allow(dead_code)] // for the core runner, wired later
    pub fn answer(&self, text: &str) {
        let spoken = (self.speaker)(text);
        self.out
            .event("answer", json!({ "text": text, "spoken": spoken }));
    }

    #[allow(dead_code)] // for the core runner, wired later
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

/// The placeholder until the core's runner is wired: every action says so plainly.
pub struct Unwired;

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
}
