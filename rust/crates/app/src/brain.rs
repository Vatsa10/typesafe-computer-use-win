//! The assistant as the daemon sees it: which mode is in force, and the methods the shell calls.
//!
//! Quick mode keeps today's behaviour exactly: a line goes to TypeSafe routing and nothing else.
//! Assistant mode hands the line to the parent agent. A missing agent key is quick mode, never an
//! error.

use std::sync::Arc;

use serde_json::{json, Value};
use wcore::assistant::{Assistant, Mode};

pub struct Brain {
    pub assistant: Option<Arc<Assistant>>,
    /// The mode now in force. A closure so Settings changes apply at once, and tests choose.
    pub mode: Box<dyn Fn() -> Mode + Send + Sync>,
}

impl Brain {
    /// No assistant at all: everything is quick.
    #[cfg(test)]
    pub fn quick() -> Self {
        Self {
            assistant: None,
            mode: Box::new(|| Mode::Quick),
        }
    }

    /// The real one: the mode follows `POINTER_MODE` and whether an agent key exists.
    pub fn live(assistant: Arc<Assistant>) -> Self {
        Self {
            assistant: Some(assistant),
            mode: Box::new(wcore::assistant::current_mode),
        }
    }

    pub fn mode(&self) -> Mode {
        if self.assistant.is_none() {
            return Mode::Quick;
        }
        (self.mode)()
    }

    /// The assistant, when it is the one in charge.
    pub fn active(&self) -> Option<&Arc<Assistant>> {
        match self.mode() {
            Mode::Assistant => self.assistant.as_ref(),
            Mode::Quick => None,
        }
    }

    pub fn busy(&self) -> bool {
        self.assistant.as_ref().is_some_and(|a| a.busy())
    }

    pub fn chat(&self, text: &str, source: &str) -> Result<Value, String> {
        let a = self.active().ok_or("the assistant is not in charge")?;
        a.chat(text, source)?;
        Ok(json!({ "queued": true, "mode": "assistant" }))
    }

    /// `{ interrupt, confidence }`. Not speaking, quick mode, or no assistant: there is nothing to
    /// protect, so a line always goes through.
    pub fn gate(&self, heard: &str, speaking: bool) -> Value {
        match self.active() {
            Some(a) if speaking => {
                let r = a.gate(heard);
                json!({ "interrupt": r.interrupt, "confidence": r.confidence })
            }
            _ => json!({ "interrupt": true, "confidence": 1.0 }),
        }
    }

    pub fn interrupt(&self) -> Value {
        if let Some(a) = &self.assistant {
            a.interrupt();
        }
        json!({ "ok": self.assistant.is_some() })
    }

    pub fn stop_all(&self) {
        if let Some(a) = &self.assistant {
            a.stop_all();
        }
    }

    pub fn new_conversation(&self) -> Value {
        if let Some(a) = &self.assistant {
            a.new_conversation();
        }
        json!({ "ok": self.assistant.is_some() })
    }

    pub fn tasks(&self) -> Value {
        match &self.assistant {
            Some(a) => Value::Array(a.tasks().list()),
            None => json!([]),
        }
    }

    pub fn task_stop(&self, id: &str) -> Result<Value, String> {
        let a = self.assistant.as_ref().ok_or("there are no tasks")?;
        Ok(json!({ "stopped": a.tasks().stop(id)? }))
    }

    pub fn confirm_reply(&self, id: &str, allow: bool) -> Value {
        let ok = self
            .assistant
            .as_ref()
            .is_some_and(|a| a.confirm_reply(id, allow));
        json!({ "ok": ok })
    }

    pub fn camera_frames(&self, id: &str, images: Vec<String>, error: Option<String>) -> Value {
        let ok = self
            .assistant
            .as_ref()
            .is_some_and(|a| a.camera_frames(id, images, error));
        json!({ "ok": ok })
    }
}
