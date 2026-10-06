//! What a spoken or typed line means to the daemon. A port of `intent.py`.
//!
//! A transcript is not a goal. "stop", "pause a sec", "never mind" and "open the console" all
//! arrive as text, and only the last one is something to accomplish. Handing raw text to the loop
//! would make the daemon try to achieve the word "stop", so the same classifier that picks actions
//! decides what the line is first. No free text is generated anywhere in this path.

use api::typesafe::{ApiError, Question};
use serde_json::json;

use crate::config;
use crate::decide::{choice_of, noul_of, Classifier};

/// The heard gate gates the commands that start work, and only those. Measured against the live
/// model, short imperatives score low on any "is this a complete instruction" question: "stop"
/// read 0.24, "keep going" 0.35, "quit" 0.43. Gating control commands on that number makes a
/// running loop impossible to stop by voice, which is the one thing voice must always manage.
pub const MIN_HEARD: f64 = 0.5;
pub const DEFAULT_MIN_CONFIDENCE: f64 = config::DEFAULT_VOICE_MIN_CONFIDENCE;
pub const IGNORED: &str = "ignore";
/// `run_goal` becomes clicking; `ask_screen` spends a vision call and then tells the user something
/// confident about a screen they did not ask about. Both must be heard cleanly.
pub const MUST_BE_HEARD: [&str; 2] = ["run_goal", "ask_screen"];

pub const COMMANDS: [(&str, &str); 7] = [
    (
        "run_goal",
        "The line is a task to carry out on this computer: something to open, find, fill in, check or \
         navigate to: the user wants it done for them, not explained to them. This is the only answer \
         that touches the machine.",
    ),
    (
        "ask_screen",
        "The line is a question about what is on the screen right now, or a request to be taught or \
         walked through something, rather than a task to carry out: what something says or means, what \
         the user is looking at, what an error is, or how they would do something themselves. This \
         answer only looks and explains; it never clicks anything.",
    ),
    (
        "stop_run",
        "The line asks for the current run to stop now: stop, cancel that, abort, never mind.",
    ),
    (
        "pause_run",
        "The line asks for the current run to hold where it is, to be continued later.",
    ),
    (
        "resume_run",
        "The line asks for a paused run to carry on: continue, keep going, resume.",
    ),
    (
        "quit_daemon",
        "The line asks to shut the assistant down entirely: quit, exit, goodbye.",
    ),
    (
        IGNORED,
        "The line is not addressed to the assistant at all, or asks for something it does not do: half \
         a sentence, a word caught by accident, or talk meant for someone else in the room.",
    ),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    pub command: String,
    pub goal: String,
    pub confidence: f64,
    pub heard: f64,
    pub transcript: String,
    pub min_confidence: f64,
}

impl Intent {
    /// Fields in the Python's positional order, with the default confidence floor.
    pub fn new(command: &str, goal: &str, confidence: f64, heard: f64, transcript: &str) -> Self {
        Self {
            command: command.into(),
            goal: goal.into(),
            confidence,
            heard,
            transcript: transcript.into(),
            min_confidence: DEFAULT_MIN_CONFIDENCE,
        }
    }

    /// Whether the daemon should act.
    ///
    /// Every answer must clear the confidence floor. Only the commands that start work --
    /// `run_goal` and `ask_screen` -- must also clear MIN_HEARD; refusing a half-caught "stop"
    /// would leave the user shouting at a run that will not stop.
    pub fn actionable(&self) -> bool {
        if self.command == IGNORED || self.confidence < self.min_confidence {
            return false;
        }
        if MUST_BE_HEARD.contains(&self.command.as_str()) {
            self.heard >= MIN_HEARD
        } else {
            true
        }
    }
}

/// Classify one line against what the daemon is currently doing.
///
/// The daemon's state travels with the transcript, so "pause" said while nothing runs reads as
/// `ignore` rather than as a pause of nothing. An empty line never reaches the model.
pub fn interpret(
    client: &dyn Classifier,
    transcript: &str,
    running: bool,
    paused: bool,
    min_confidence: f64,
) -> Result<Intent, ApiError> {
    let said = transcript.split_whitespace().collect::<Vec<_>>().join(" ");
    if said.is_empty() {
        let mut i = Intent::new(IGNORED, "", 0.0, 0.0, "");
        i.min_confidence = min_confidence;
        return Ok(i);
    }
    let state = json!({
        "transcript": said,
        "a_run_is_active": running,
        "the_run_is_paused": paused,
        "what_the_assistant_does": "does two things with this Windows machine. It drives it toward a goal \
            spoken in plain English -- opening sites, clicking controls, filling fields -- and it also \
            answers questions about what is on the screen, explaining or teaching without touching anything",
    });
    let questions = vec![
        (
            "command".to_string(),
            Question::Choice {
                instructions: "The user said this out loud to an assistant that drives their computer. What \
                               are they asking for? Answer with what the line means for the assistant right \
                               now, given whether a run is active and whether it is paused."
                    .into(),
                criteria: COMMANDS
                    .iter()
                    .map(|(k, v)| (k.to_string(), Some(v.to_string())))
                    .collect(),
            },
        ),
        (
            "heard".to_string(),
            Question::Noul {
                instructions: "Was this transcript caught cleanly enough to act on? Judge the audio, not the \
                               length: a short command like 'stop' or 'keep going' is complete. Say no only \
                               for a garbled line, a false start, a sentence that breaks off mid-word, or \
                               speech that was clearly meant for someone else."
                    .into(),
            },
        ),
    ];
    let answers = client.system_one(&state, &questions)?;
    let command = choice_of(&answers, "command").ok_or_else(|| ApiError {
        status: None,
        message: "unexpected TypeSafe response: no command answer".into(),
    })?;
    Ok(Intent {
        goal: if command.choice == "run_goal" {
            said.clone()
        } else {
            String::new()
        },
        command: command.choice,
        confidence: command.confidence,
        heard: noul_of(&answers, "heard")?,
        transcript: said,
        min_confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::typesafe::{Answer, ChoiceAnswer};
    use serde_json::Value;
    use std::cell::RefCell;

    struct FakeClient {
        command: String,
        confidence: f64,
        heard: f64,
        state: RefCell<Option<Value>>,
        questions: RefCell<Vec<(String, Question)>>,
    }

    fn fake(command: &str, confidence: f64, heard: f64) -> FakeClient {
        FakeClient {
            command: command.into(),
            confidence,
            heard,
            state: RefCell::new(None),
            questions: RefCell::new(Vec::new()),
        }
    }

    fn default_fake() -> FakeClient {
        fake("run_goal", 0.9, 0.9)
    }

    impl Classifier for FakeClient {
        fn system_one(
            &self,
            state: &Value,
            questions: &[(String, Question)],
        ) -> Result<Vec<(String, Answer)>, ApiError> {
            *self.state.borrow_mut() = Some(state.clone());
            *self.questions.borrow_mut() = questions.to_vec();
            Ok(vec![
                (
                    "command".into(),
                    Answer::Choice(ChoiceAnswer {
                        choice: self.command.clone(),
                        confidence: self.confidence,
                        probabilities: vec![(self.command.clone(), self.confidence)],
                    }),
                ),
                ("heard".into(), Answer::Noul(self.heard)),
            ])
        }
    }

    fn run(c: &FakeClient, said: &str, running: bool) -> Intent {
        interpret(c, said, running, false, DEFAULT_MIN_CONFIDENCE).unwrap()
    }

    #[test]
    fn a_spoken_goal_becomes_a_runnable_intent() {
        let i = run(
            &fake("run_goal", 0.88, 0.9),
            "open the console and check billing",
            false,
        );
        assert_eq!(i.command, "run_goal");
        assert_eq!(i.goal, "open the console and check billing");
        assert!(i.actionable());
    }

    #[test]
    fn the_daemon_state_travels_with_the_transcript() {
        let c = default_fake();
        run(&c, "pause", true);
        let s = c.state.borrow().clone().unwrap();
        assert_eq!(s["a_run_is_active"], true);
        assert_eq!(s["the_run_is_paused"], false);
        assert_eq!(s["transcript"], "pause");
    }

    #[test]
    fn both_questions_are_asked() {
        let c = default_fake();
        run(&c, "stop", true);
        let names: Vec<String> = c
            .questions
            .borrow()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(names, ["command", "heard"]);
    }

    #[test]
    fn a_low_confidence_command_is_not_actionable() {
        assert!(!run(&fake("quit_daemon", 0.4, 0.9), "mumble mumble", false).actionable());
    }

    #[test]
    fn a_transcript_that_was_not_really_heard_is_not_actionable() {
        assert!(!run(&fake("run_goal", 0.95, 0.2), "uh, the, uh", false).actionable());
    }

    #[test]
    fn ignore_is_never_actionable_however_confident() {
        assert!(!run(
            &fake("ignore", 0.99, 0.9),
            "hey are you coming for lunch",
            false
        )
        .actionable());
    }

    #[test]
    fn an_empty_transcript_never_reaches_the_model() {
        struct Exploding;
        impl Classifier for Exploding {
            fn system_one(
                &self,
                _: &Value,
                _: &[(String, Question)],
            ) -> Result<Vec<(String, Answer)>, ApiError> {
                panic!("nothing was said, so nothing to ask")
            }
        }
        let i = interpret(&Exploding, "   ", false, false, DEFAULT_MIN_CONFIDENCE).unwrap();
        assert!(i.command == "ignore" && !i.actionable());
    }

    #[test]
    fn only_a_run_goal_carries_a_goal() {
        assert_eq!(
            run(&fake("stop_run", 0.93, 0.9), "stop that", true).goal,
            ""
        );
    }

    #[test]
    fn the_fields_are_in_the_documented_order() {
        let i = Intent::new("run_goal", "do a thing", 0.8, 0.9, "do a thing");
        assert_eq!(
            (
                i.command.as_str(),
                i.goal.as_str(),
                i.confidence,
                i.heard,
                i.transcript.as_str()
            ),
            ("run_goal", "do a thing", 0.8, 0.9, "do a thing")
        );
        assert_eq!(i.min_confidence, 0.55);
    }

    #[test]
    fn a_caller_can_raise_the_bar() {
        let c = fake("run_goal", 0.7, 0.9);
        assert!(!interpret(&c, "do a thing", false, false, 0.9)
            .unwrap()
            .actionable());
        assert!(interpret(&c, "do a thing", false, false, 0.6)
            .unwrap()
            .actionable());
    }

    #[test]
    fn the_transcript_is_whitespace_normalised() {
        let i = run(&default_fake(), "  open   the  console \n", false);
        assert_eq!(i.transcript, "open the console");
        assert_eq!(i.goal, "open the console");
    }

    #[test]
    fn a_short_command_acts_even_though_it_scores_low_on_heard() {
        assert!(run(&fake("stop_run", 0.99, 0.24), "stop stop stop", true).actionable());
        for cmd in ["pause_run", "resume_run", "quit_daemon"] {
            assert!(run(&fake(cmd, 0.9, 0.1), "x", true).actionable(), "{cmd}");
        }
    }

    #[test]
    fn a_half_caught_goal_is_still_refused() {
        assert!(!run(&fake("run_goal", 0.95, 0.2), "open the uh the", false).actionable());
    }

    #[test]
    fn a_command_below_the_confidence_floor_is_still_refused() {
        assert!(!run(&fake("quit_daemon", 0.3, 0.9), "mumble", false).actionable());
    }

    #[test]
    fn a_question_about_the_screen_routes_to_ask_screen() {
        for said in [
            "what does this say",
            "how do I export this",
            "what am I looking at",
            "explain this error",
        ] {
            let i = run(&fake("ask_screen", 0.9, 0.9), said, false);
            assert_eq!(i.command, "ask_screen");
            assert!(i.actionable());
        }
    }

    #[test]
    fn a_task_still_routes_to_run_goal() {
        for said in ["open youtube", "like this post", "switch to whatsapp"] {
            let i = run(&fake("run_goal", 0.9, 0.9), said, false);
            assert_eq!(i.command, "run_goal");
            assert_eq!(i.goal, said);
        }
    }

    #[test]
    fn ask_screen_is_offered_as_a_criterion() {
        let c = fake("ask_screen", 0.9, 0.9);
        run(&c, "what does this say", false);
        let qs = c.questions.borrow();
        match &qs[0].1 {
            Question::Choice { criteria, .. } => {
                assert!(criteria.iter().any(|(k, _)| k == "ask_screen"))
            }
            _ => panic!("command is a choice"),
        }
    }

    #[test]
    fn ask_screen_carries_no_goal_because_it_never_drives_anything() {
        let i = run(&fake("ask_screen", 0.9, 0.9), "what does this say", false);
        assert_eq!(i.goal, "");
        assert_eq!(i.transcript, "what does this say");
    }

    #[test]
    fn a_half_caught_question_is_refused_by_the_heard_gate() {
        assert!(!run(&fake("ask_screen", 0.95, 0.2), "what does the uh", false).actionable());
    }

    #[test]
    fn a_cleanly_heard_question_below_the_confidence_floor_is_refused() {
        assert!(!run(&fake("ask_screen", 0.3, 0.95), "what does this say", false).actionable());
    }
}
