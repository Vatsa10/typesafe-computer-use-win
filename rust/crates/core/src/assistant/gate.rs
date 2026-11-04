//! The turn gate: while the assistant is speaking, is what the microphone just heard meant for it?
//!
//! TypeSafe answers one Choice, `interrupt` or `ignore`, given the transcript, the request being
//! worked on and the last line spoken. Some cases never reach the model:
//!
//! - a correction (starts with no, wait, stop or actually) always interrupts;
//! - the same text heard again within 15 s passes: saying it twice means it was meant;
//! - any error or no answer within 1.5 s interrupts, because ignoring a real request is worse than
//!   cutting off a sentence;
//! - `CLICKER_TURN_GATE=off` turns the gate off, so everything interrupts.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::typesafe::Question;
use serde_json::json;

use crate::decide::{choice_of, Classifier};

pub const TIMEOUT: Duration = Duration::from_millis(1500);
pub const REPEAT_WINDOW: Duration = Duration::from_secs(15);

/// The words that open a correction.
pub const CORRECTIONS: [&str; 4] = ["no", "wait", "stop", "actually"];

#[derive(Debug, Clone, PartialEq)]
pub struct GateResult {
    pub interrupt: bool,
    pub confidence: f64,
    /// Why, for the log: `model`, `correction`, `repeat`, `error`, `timeout`, `off`, `empty`.
    pub why: &'static str,
}

fn result(interrupt: bool, confidence: f64, why: &'static str) -> GateResult {
    GateResult {
        interrupt,
        confidence,
        why,
    }
}

/// `CLICKER_TURN_GATE` as a setting: only off, 0, false or no turn it off.
pub fn enabled_from(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Does the line open with a correction word?
pub fn is_correction(text: &str) -> bool {
    words(text)
        .first()
        .is_some_and(|w| CORRECTIONS.contains(&w.as_str()))
}

/// The classifier when there is no TypeSafe key: every question errors, so the gate interrupts.
pub struct Unavailable;

impl Classifier for Unavailable {
    fn system_one(
        &self,
        _: &serde_json::Value,
        _: &[(String, Question)],
    ) -> Result<Vec<(String, api::typesafe::Answer)>, api::typesafe::ApiError> {
        Err(api::typesafe::ApiError {
            status: None,
            message: "no TypeSafe key".into(),
        })
    }
}

pub struct Gate {
    classifier: Arc<dyn Classifier + Send + Sync>,
    seen: Mutex<VecDeque<(String, Instant)>>,
    timeout: Duration,
    /// Some(false) forces the gate off, Some(true) on; None reads the environment.
    enabled: Option<bool>,
}

impl Gate {
    pub fn new(classifier: Arc<dyn Classifier + Send + Sync>) -> Self {
        Self {
            classifier,
            seen: Mutex::new(VecDeque::new()),
            timeout: TIMEOUT,
            enabled: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_enabled(mut self, enabled: Option<bool>) -> Self {
        self.enabled = enabled;
        self
    }

    fn is_enabled(&self) -> bool {
        self.enabled
            .unwrap_or_else(|| enabled_from(std::env::var("CLICKER_TURN_GATE").ok().as_deref()))
    }

    /// Decide, and remember the text for the repeat rule.
    pub fn decide(
        &self,
        heard: &str,
        current_request: &str,
        last_spoken: &str,
        now: Instant,
    ) -> GateResult {
        let said = words(heard).join(" ");
        let verdict = self.verdict(heard, &said, current_request, last_spoken, now);
        if !said.is_empty() {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            seen.retain(|(_, t)| now.saturating_duration_since(*t) <= REPEAT_WINDOW);
            seen.push_back((said, now));
        }
        verdict
    }

    fn verdict(
        &self,
        heard: &str,
        said: &str,
        current_request: &str,
        last_spoken: &str,
        now: Instant,
    ) -> GateResult {
        if !self.is_enabled() {
            return result(true, 1.0, "off");
        }
        if said.is_empty() {
            return result(false, 1.0, "empty");
        }
        if is_correction(heard) {
            return result(true, 1.0, "correction");
        }
        let repeated = self
            .seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .any(|(t, at)| t == said && now.saturating_duration_since(*at) <= REPEAT_WINDOW);
        if repeated {
            return result(true, 1.0, "repeat");
        }
        self.ask(heard, current_request, last_spoken)
    }

    fn ask(&self, heard: &str, current_request: &str, last_spoken: &str) -> GateResult {
        let state = json!({
            "transcript": heard,
            "current_request": current_request,
            "last_spoken_line": last_spoken,
        });
        let questions = vec![(
            "gate".to_string(),
            Question::Choice {
                instructions: "An assistant is speaking out loud to its user. The microphone just caught \
                               the transcript. Is the transcript the user speaking to the assistant, \
                               meaning to interrupt what it is saying, or is it something else: a \
                               background voice, the assistant's own words heard back, a TV, a remark to \
                               someone nearby, or a stray word?"
                    .into(),
                criteria: vec![
                    (
                        "interrupt".into(),
                        Some("The user is addressing the assistant: a new request, a question, a correction or a follow-up to what is being said.".into()),
                    ),
                    (
                        "ignore".into(),
                        Some("Not meant for the assistant: background speech, an echo of the assistant's own last line, or a stray word.".into()),
                    ),
                ],
            },
        )];
        let (tx, rx) = mpsc::channel();
        let classifier = self.classifier.clone();
        let spawned = std::thread::Builder::new()
            .name("gate".into())
            .spawn(move || {
                let _ = tx.send(classifier.system_one(&state, &questions));
            });
        if spawned.is_err() {
            return result(true, 0.0, "error");
        }
        match rx.recv_timeout(self.timeout) {
            Ok(Ok(answers)) => match choice_of(&answers, "gate") {
                Some(c) => result(c.choice != "ignore", c.confidence, "model"),
                None => result(true, 0.0, "error"),
            },
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => result(true, 0.0, "error"),
            Err(mpsc::RecvTimeoutError::Timeout) => result(true, 0.0, "timeout"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::typesafe::{Answer, ApiError, ChoiceAnswer};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        reply: Result<&'static str, ()>,
        delay: Duration,
        calls: AtomicUsize,
        state: Mutex<Option<Value>>,
    }

    fn fake(reply: Result<&'static str, ()>, delay_ms: u64) -> Arc<Fake> {
        Arc::new(Fake {
            reply,
            delay: Duration::from_millis(delay_ms),
            calls: AtomicUsize::new(0),
            state: Mutex::new(None),
        })
    }

    impl Classifier for Fake {
        fn system_one(
            &self,
            state: &Value,
            _: &[(String, Question)],
        ) -> Result<Vec<(String, Answer)>, ApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.state.lock().unwrap() = Some(state.clone());
            std::thread::sleep(self.delay);
            match self.reply {
                Ok(choice) => Ok(vec![(
                    "gate".into(),
                    Answer::Choice(ChoiceAnswer {
                        choice: choice.into(),
                        confidence: 0.9,
                        probabilities: vec![],
                    }),
                )]),
                Err(()) => Err(ApiError {
                    status: Some(500),
                    message: "down".into(),
                }),
            }
        }
    }

    fn gate(f: &Arc<Fake>) -> Gate {
        Gate::new(f.clone()).with_enabled(Some(true))
    }

    #[test]
    fn the_model_decides_and_sees_all_three_inputs() {
        let f = fake(Ok("ignore"), 0);
        let g = gate(&f);
        let r = g.decide(
            "the kettle is on",
            "open mail",
            "Opening mail now.",
            Instant::now(),
        );
        assert_eq!((r.interrupt, r.why), (false, "model"));
        let s = f.state.lock().unwrap().clone().unwrap();
        assert_eq!(s["transcript"], "the kettle is on");
        assert_eq!(s["current_request"], "open mail");
        assert_eq!(s["last_spoken_line"], "Opening mail now.");
        let f = fake(Ok("interrupt"), 0);
        assert!(
            gate(&f)
                .decide("open the calendar", "", "", Instant::now())
                .interrupt
        );
    }

    #[test]
    fn corrections_skip_the_model() {
        let f = fake(Ok("ignore"), 0);
        let g = gate(&f);
        for said in [
            "No, the other one",
            "wait",
            "Stop.",
            "Actually make it blue",
            "  NO ",
        ] {
            let r = g.decide(said, "x", "y", Instant::now());
            assert!(r.interrupt && r.why == "correction", "{said}");
        }
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        // Only a leading word counts, and whole words only.
        assert!(!is_correction("nothing to see"));
        assert!(!is_correction("I said no"));
    }

    #[test]
    fn errors_and_slow_answers_interrupt() {
        let r = gate(&fake(Err(()), 0)).decide("hmm", "", "", Instant::now());
        assert_eq!((r.interrupt, r.why), (true, "error"));
        let slow = fake(Ok("ignore"), 400);
        let g = Gate::new(slow)
            .with_enabled(Some(true))
            .with_timeout(Duration::from_millis(100));
        let t = Instant::now();
        let r = g.decide("hmm", "", "", t);
        assert_eq!((r.interrupt, r.why), (true, "timeout"));
        assert!(t.elapsed() < Duration::from_millis(350));
    }

    #[test]
    fn the_same_text_within_fifteen_seconds_passes() {
        let f = fake(Ok("ignore"), 0);
        let g = gate(&f);
        let t = Instant::now();
        assert!(!g.decide("the kettle is on", "", "", t).interrupt);
        let again = g.decide("The kettle is on!", "", "", t + Duration::from_secs(10));
        assert_eq!((again.interrupt, again.why), (true, "repeat"));
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        // Outside the window it is asked afresh.
        let late = g.decide("the kettle is on", "", "", t + Duration::from_secs(40));
        assert_eq!(late.why, "model");
    }

    #[test]
    fn off_interrupts_everything_and_empty_never_does() {
        let f = fake(Ok("ignore"), 0);
        let off = Gate::new(f.clone()).with_enabled(Some(false));
        let r = off.decide("anything", "", "", Instant::now());
        assert_eq!((r.interrupt, r.why), (true, "off"));
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        assert!(!gate(&f).decide("  ", "", "", Instant::now()).interrupt);
        assert!(!enabled_from(Some("off")) && !enabled_from(Some(" 0 ")));
        assert!(enabled_from(None) && enabled_from(Some("on")));
    }
}
