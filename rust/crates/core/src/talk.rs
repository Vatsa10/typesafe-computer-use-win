//! Talk mode: answer a question about the screen, and act on nothing. A port of `talk.py`.
//!
//! This module is safe by construction, and that is the whole point of it. It looks at the screen
//! and it speaks; it never clicks, types, scrolls, switches windows or opens anything. Nothing here
//! reaches into `actions`, `runner` or `platform::input`, and nothing here may ever be changed to
//! do so: the user asked a question because they want to be told something, not to have their
//! machine driven. A question that turns out to be a task is the classifier's problem
//! (`intent`'s `ask_screen`), and this path only ever gets the questions.
//!
//! So the whole of talk mode is one capture, one perception pass, one writer call, and a string.
//! Capture and perception come in through [`Eyes`], so this module does not depend on the
//! perception port and its tests touch no real screen.

use std::fmt;

use crate::config::MAX_OPTIONS;
use crate::models::{Item, Screen};
use crate::writer::{compose_explanation, StructuredWriter, WriterError};

/// How talk mode sees: a capture, then the items read from it. Read-only by contract.
pub trait Eyes {
    fn capture(&self, browser: &str) -> Result<Screen, String>;
    fn perceive(&self, screen: &Screen, budget: usize, question: &str)
        -> Result<Vec<Item>, String>;
}

/// A failure that is not the writer being unavailable. Those are bugs or a broken capture, and
/// are surfaced rather than turned into a sentence.
#[derive(Debug, Clone, PartialEq)]
pub enum TalkError {
    Look(String),
    Writer(WriterError),
}

impl fmt::Display for TalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Look(why) => write!(f, "could not read the screen: {why}"),
            Self::Writer(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TalkError {}

/// Look at the screen, answer the question about it, and return what was said.
///
/// A writer with no credit or a rejected key comes back as a sentence the user can read: talk
/// mode is something a person is waiting on an answer from, and a traceback is not an answer.
pub fn answer_question(
    writer: &dyn StructuredWriter,
    eyes: &dyn Eyes,
    question: &str,
    browser: &str,
    log: &mut dyn FnMut(&str),
) -> Result<String, TalkError> {
    let screen = eyes.capture(browser).map_err(TalkError::Look)?;
    let items = eyes
        .perceive(&screen, MAX_OPTIONS, question)
        .map_err(TalkError::Look)?;
    let answer = match compose_explanation(writer, question, &screen, &items) {
        Ok(a) => a,
        Err(WriterError::Unavailable(why)) => {
            format!("I could not answer that: the writer is unavailable ({why}).")
        }
        Err(e) => return Err(TalkError::Writer(e)),
    };
    log(&answer);
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::tests::{make_item, screen};
    use crate::writer::tests::Fake;
    use serde_json::json;
    use std::cell::RefCell;

    struct FakeEyes {
        texts: Vec<&'static str>,
        app: &'static str,
        browser: RefCell<Option<String>>,
        perceived: RefCell<Option<(String, usize)>>,
    }

    fn eyes() -> FakeEyes {
        FakeEyes {
            texts: vec!["Export", "Finished at 10:04"],
            app: "Chrome",
            browser: RefCell::new(None),
            perceived: RefCell::new(None),
        }
    }

    impl Eyes for FakeEyes {
        fn capture(&self, browser: &str) -> Result<Screen, String> {
            *self.browser.borrow_mut() = Some(browser.into());
            let mut s = screen();
            s.app = self.app.into();
            s.url = Some("https://example.com/report".into());
            Ok(s)
        }

        fn perceive(&self, s: &Screen, budget: usize, question: &str) -> Result<Vec<Item>, String> {
            *self.perceived.borrow_mut() = Some((s.app.clone(), budget));
            assert!(!question.is_empty());
            Ok(self
                .texts
                .iter()
                .enumerate()
                .map(|(i, t)| make_item(i, t, 100.0 + 40.0 * i as f64, 130.0 + 40.0 * i as f64))
                .collect())
        }
    }

    fn quiet() -> impl FnMut(&str) {
        |_: &str| {}
    }

    #[test]
    fn the_question_the_capture_and_the_screen_text_all_reach_the_writer() {
        let e = eyes();
        let w = Fake::new(json!({"answer": "The dialog says the export finished."}));
        answer_question(&w, &e, "what does this say", "", &mut quiet()).unwrap();
        assert_eq!(
            *e.perceived.borrow(),
            Some(("Chrome".to_string(), MAX_OPTIONS))
        );
        let call = &w.calls.borrow()[0];
        assert_eq!(call.packet["question"], "what does this say");
        assert_eq!(
            call.packet["screen_text_in_reading_order"],
            json!(["Export", "Finished at 10:04"])
        );
        assert_eq!(call.packet["frontmost_app"], "Chrome");
        assert!(call.png.as_ref().is_some_and(|p| p.starts_with(b"\x89PNG")));
    }

    #[test]
    fn the_answer_comes_back_and_is_logged() {
        let mut lines: Vec<String> = Vec::new();
        let w = Fake::new(json!({"answer": "Three rows failed validation."}));
        let got = answer_question(&w, &eyes(), "what am I looking at", "", &mut |l: &str| {
            lines.push(l.into())
        })
        .unwrap();
        assert_eq!(got, "Three rows failed validation.");
        assert_eq!(lines, ["Three rows failed validation."]);
    }

    #[test]
    fn the_browser_hint_reaches_the_capture() {
        let e = eyes();
        let w = Fake::new(json!({"answer": "x"}));
        answer_question(&w, &e, "how do I export this", "chrome", &mut quiet()).unwrap();
        assert_eq!(e.browser.borrow().as_deref(), Some("chrome"));
    }

    #[test]
    fn an_unavailable_writer_is_reported_not_raised() {
        let mut lines: Vec<String> = Vec::new();
        let w = Fake::failing("Your credit balance is too low to access the API");
        let got = answer_question(&w, &eyes(), "explain this error", "", &mut |l: &str| {
            lines.push(l.into())
        })
        .unwrap();
        assert!(got.contains("out of credit"));
        assert_eq!(lines, [got]);
    }

    #[test]
    fn the_writer_is_asked_to_answer_only_from_the_screen() {
        let w = Fake::new(json!({"answer": "x"}));
        answer_question(&w, &eyes(), "what does this say", "", &mut quiet()).unwrap();
        let system = w.calls.borrow()[0].system.to_lowercase();
        assert!(system.contains("visible") && system.contains("nothing else"));
        assert!(system.contains("never from memory") && system.contains("never a guess"));
        assert!(system.contains("on-screen controls"));
        assert!(system.contains("teaching, not acting"));
    }

    #[test]
    fn a_writer_failure_that_is_not_unavailability_still_surfaces() {
        let w = Fake::new(json!({"not_answer": 1}));
        let got = answer_question(&w, &eyes(), "what does this say", "", &mut quiet());
        assert!(matches!(
            got,
            Err(TalkError::Writer(WriterError::BadReply(_)))
        ));
    }

    #[test]
    fn a_broken_capture_surfaces() {
        struct Blind;
        impl Eyes for Blind {
            fn capture(&self, _: &str) -> Result<Screen, String> {
                Err("no display".into())
            }
            fn perceive(&self, _: &Screen, _: usize, _: &str) -> Result<Vec<Item>, String> {
                unreachable!()
            }
        }
        let w = Fake::new(json!({"answer": "x"}));
        let got = answer_question(&w, &Blind, "q", "", &mut quiet());
        assert_eq!(got, Err(TalkError::Look("no display".into())));
        assert!(w.calls.borrow().is_empty());
    }

    /// The safety property, pinned: talk mode cannot act. Its only inputs are a read-only `Eyes`
    /// and a writer, and its source names nothing that acts.
    #[test]
    fn talk_imports_nothing_that_acts() {
        let source = include_str!("talk.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let code: String = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for door in [
            "actions", "runner", "perform", "input", "click", "press", "uia", "platform",
        ] {
            assert!(
                !code.contains(door),
                "talk.rs names `{door}`, and talk mode must not be able to act"
            );
        }
    }
}
