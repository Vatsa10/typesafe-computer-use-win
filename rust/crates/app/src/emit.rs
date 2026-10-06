//! The one way out: every reply and every event goes through a single locked writer, so a hotkey
//! event fired from the pump's worker can never land in the middle of a reply's bytes.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::ipc;

#[derive(Clone)]
pub struct Emitter {
    sink: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Emitter {
    pub fn new(sink: impl Write + Send + 'static) -> Self {
        Self {
            sink: Arc::new(Mutex::new(Box::new(sink))),
        }
    }

    /// Write one JSON value as one line. False when the shell is gone.
    pub fn send(&self, message: &Value) -> bool {
        // serde_json escapes nothing it does not have to, so non-ASCII goes out as UTF-8 bytes.
        let line = message.to_string();
        let mut sink = match self.sink.lock() {
            Ok(sink) => sink,
            Err(poisoned) => poisoned.into_inner(),
        };
        writeln!(sink, "{line}").is_ok() && sink.flush().is_ok()
    }

    pub fn reply(&self, reply: &ipc::Reply) -> bool {
        match serde_json::to_value(reply) {
            Ok(v) => self.send(&v),
            Err(_) => false,
        }
    }

    pub fn event(&self, name: &str, payload: Value) -> bool {
        self.send(&ipc::event(name, payload))
    }

    pub fn line(&self, text: impl Into<String>) {
        self.event("line", json!({ "text": text.into() }));
    }
}

/// An in-memory sink for tests: everything written, readable as lines of JSON.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct Captured(pub Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Captured {
    pub fn messages(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_never_interleave_inside_a_line() {
        let captured = Captured::default();
        let out = Emitter::new(captured.clone());
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let out = out.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        out.event(
                            "line",
                            json!({ "text": format!("{t}-{i} {}", "x".repeat(200)) }),
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            captured.messages().len(),
            400,
            "every line parsed as whole JSON"
        );
    }

    #[test]
    fn non_ascii_goes_out_as_utf8() {
        let captured = Captured::default();
        Emitter::new(captured.clone()).line("caf\u{e9} \u{2192} \u{1f600}");
        let raw = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(raw.contains("caf\u{e9} \u{2192}"));
        assert_eq!(
            captured.messages()[0]["text"],
            "caf\u{e9} \u{2192} \u{1f600}"
        );
    }
}
