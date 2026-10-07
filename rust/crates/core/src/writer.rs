//! The writer model: the only place free text is generated, when the classifier asks for it and
//! once when the run ends. A port of `writer.py`, on OpenAI only.
//!
//! The service is reached through [`StructuredWriter`], which `api::openai::OpenAi` implements, so
//! tests run against a fake. Request shape (strict JSON schema, system message, PNG data URL,
//! `max_completion_tokens` with the legacy fallback) lives in the api crate.

use std::fmt;

use api::openai::{problem, ApiError, OpenAi};
use serde_json::{json, Value};

use crate::config;
use crate::dates::now_context;
use crate::models::{Field, Item, Screen};

/// The longest edge a vision model reads without shrinking the image itself.
pub const ANSWER_IMAGE_EDGE: u32 = 1568;

/// Why the writer gave no text.
#[derive(Debug, Clone, PartialEq)]
pub enum WriterError {
    /// The service could not answer: no credit, a rejected key, a rate limit, a missing model.
    /// Callers refuse the step and carry on. The string is `api::openai::problem`'s short reason.
    Unavailable(String),
    /// The service answered with something other than the schema. A bug, not a condition to
    /// swallow, as a `KeyError` is in the Python.
    BadReply(String),
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => f.write_str(why),
            Self::BadReply(why) => write!(f, "the writer's reply did not match its schema: {why}"),
        }
    }
}

impl std::error::Error for WriterError {}

/// Whichever service writes free text, behind one method.
pub trait StructuredWriter {
    fn structured(
        &self,
        model: &str,
        system: &str,
        packet: &Value,
        properties: &Value,
        max_tokens: u32,
        png: Option<&[u8]>,
    ) -> Result<Value, ApiError>;
}

impl StructuredWriter for OpenAi {
    fn structured(
        &self,
        model: &str,
        system: &str,
        packet: &Value,
        properties: &Value,
        max_tokens: u32,
        png: Option<&[u8]>,
    ) -> Result<Value, ApiError> {
        OpenAi::structured(self, model, system, packet, properties, max_tokens, png)
    }
}

/// A writer, or None when no OpenAI key resolves, so a run never fails halfway for want of one.
pub fn make_writer() -> Option<OpenAi> {
    OpenAi::from_env()
}

fn env_model(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// `CLICKER_WRITER_MODEL`, else the OpenAI default: the writer is OpenAI everywhere, so the
/// Anthropic default `config::writer_model` would pick without an OpenAI key never applies.
pub fn writer_model() -> String {
    env_model("CLICKER_WRITER_MODEL", config::DEFAULT_OPENAI_WRITER_MODEL)
}

/// `CLICKER_ANSWER_MODEL`, else the OpenAI default.
pub fn answer_model() -> String {
    env_model("CLICKER_ANSWER_MODEL", config::DEFAULT_OPENAI_ANSWER_MODEL)
}

/// The capture as PNG, shrunk so its long edge is at most `ANSWER_IMAGE_EDGE`. PNG because screen
/// text does not survive JPEG well. None when the screen holds no usable pixels.
pub fn png_bytes(screen: &Screen) -> Option<Vec<u8>> {
    let (w, h) = (screen.width, screen.height);
    if w == 0 || h == 0 || screen.bgra.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    let rgb: Vec<u8> = screen.bgra[..(w as usize) * (h as usize) * 4]
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let img = image::RgbImage::from_raw(w, h, rgb)?;
    let img = if w.max(h) > ANSWER_IMAGE_EDGE {
        let (nw, nh) = if w >= h {
            let nh = ((h as f64) * ANSWER_IMAGE_EDGE as f64 / w as f64)
                .round()
                .max(1.0) as u32;
            (ANSWER_IMAGE_EDGE, nh)
        } else {
            let nw = ((w as f64) * ANSWER_IMAGE_EDGE as f64 / h as f64)
                .round()
                .max(1.0) as u32;
            (nw, ANSWER_IMAGE_EDGE)
        };
        image::imageops::thumbnail(&img, nw, nh)
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// One structured reply. Every provider failure becomes `WriterError::Unavailable`.
fn structured(
    writer: &dyn StructuredWriter,
    model: &str,
    system: &str,
    packet: &Value,
    properties: &Value,
    max_tokens: u32,
    png: Option<&[u8]>,
) -> Result<Value, WriterError> {
    writer
        .structured(model, system, packet, properties, max_tokens, png)
        .map_err(|e| WriterError::Unavailable(problem(&e.message)))
}

fn field_str(data: &Value, key: &str) -> Result<String, WriterError> {
    data.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| WriterError::BadReply(format!("no string {key:?}")))
}

fn field_bool(data: &Value, key: &str) -> Result<bool, WriterError> {
    data.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| WriterError::BadReply(format!("no boolean {key:?}")))
}

fn last<T: Clone>(xs: &[T], n: usize) -> Vec<T> {
    xs[xs.len().saturating_sub(n)..].to_vec()
}

/// Text of items within a radius of the focused field, in screen points. A port of
/// `perception.near_field`, kept here so this module does not wait on the perception port.
pub fn near_field(screen: &Screen, items: &[Item], radius_pt: f64) -> Vec<String> {
    let Some(f) = &screen.field else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|it| {
            let (cx, cy) = screen.to_points(it);
            (cx - (f.x + f.w / 2.0)).abs() < radius_pt + f.w / 2.0
                && (cy - (f.y + f.h / 2.0)).abs() < radius_pt
        })
        .map(|it| it.text.clone())
        .collect()
}

pub const COMPOSE_TEXT_SYSTEM: &str = "You fill in one text field on a user's screen. You receive the user's goal, recent actions, the focused field's label and placeholder, and nearby screen text. Decide the exact string to type. Never invent credentials, passwords, or personal data; for such fields, or when the field should not be filled, set fill to false.";
pub const COMPOSE_URL_SYSTEM: &str = "Given a user's goal for their web browser, give the single best https URL to open first. Prefer the site's homepage or the most direct public page. If no website is implied, set ok to false.";
pub const COMPOSE_ANSWER_SYSTEM: &str = "An agent drove a user's computer toward the user's goal and has now stopped. You receive the goal, the actions it took, why it stopped, a capture of the screen as it is now, and the text read from that screen. Tell the user the result. When the goal asks for information, lead with that information, taken only from the screen: never from memory, and never a guess. When the goal asks for something to be done, say whether the screen shows it done. When the screen does not hold the result, say so plainly, then say what is on screen and the one next step that would get there. Trust the capture over the text where the two disagree. Plain text, no markdown, four sentences at most. Set achieved to true only when the screen itself shows the goal reached.";
pub const COMPOSE_EXPLANATION_SYSTEM: &str = "A user asked a question about the screen in front of them. You receive the question, a capture of their screen as it is now, and the text read from that screen. Answer from what is visible and nothing else: never from memory, and never a guess. When the screen does not contain the answer, say so plainly, then say what is on screen instead. When the user asks how to do something, do not do it and do not offer to: name the actual on-screen controls they should use, by the labels visible on the capture, in the order they would use them. You are teaching, not acting. Trust the capture over the text where the two disagree. Plain text, no markdown, five sentences at most.";

/// The exact string to type into the focused field. Empty means the writer declined.
pub fn compose_text(
    writer: &dyn StructuredWriter,
    goal: &str,
    screen: &Screen,
    items: &[Item],
    history: &[String],
) -> Result<String, WriterError> {
    let all: Vec<&str> = items.iter().take(120).map(|it| it.text.as_str()).collect();
    let packet = json!({
        "goal": goal,
        "now": now_context(),
        "frontmost_app": screen.app,
        "previous_actions": last(history, 8),
        "focused_field": screen.field.as_ref().map(Field::summary),
        "text_near_field": near_field(screen, items, 160.0),
        "all_screen_text": all,
    });
    let properties = json!({"fill": {"type": "boolean"}, "text": {"type": "string"}, "reason": {"type": "string"}});
    let data = structured(
        writer,
        &writer_model(),
        COMPOSE_TEXT_SYSTEM,
        &packet,
        &properties,
        256,
        None,
    )?;
    let text = field_str(&data, "text")?;
    Ok(if field_bool(&data, "fill")? {
        text.trim().to_string()
    } else {
        String::new()
    })
}

/// https, a dotted host, and no whitespace anywhere.
pub fn valid_url(url: &str) -> bool {
    let parts = crate::catalog::urlsplit(url);
    parts.scheme == "https" && parts.netloc.contains('.') && !url.chars().any(char::is_whitespace)
}

/// The URL to open for this goal. Empty means no sensible site, or an invalid proposal.
pub fn compose_url(
    writer: &dyn StructuredWriter,
    goal: &str,
    history: &[String],
) -> Result<String, WriterError> {
    let packet = json!({"goal": goal, "now": now_context(), "previous_actions": last(history, 8)});
    let properties =
        json!({"ok": {"type": "boolean"}, "url": {"type": "string"}, "reason": {"type": "string"}});
    let data = structured(
        writer,
        &writer_model(),
        COMPOSE_URL_SYSTEM,
        &packet,
        &properties,
        200,
        None,
    )?;
    let url = field_str(&data, "url")?;
    let url = if field_bool(&data, "ok")? {
        url.trim().to_string()
    } else {
        String::new()
    };
    Ok(if valid_url(&url) { url } else { String::new() })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub text: String,
    /// Whether the screen itself shows the goal reached, in the writer's judgement.
    pub achieved: bool,
}

/// What to tell the user now that the run is over: the result when the screen holds it, where
/// things stand when not. The writer reads the capture as well as its text.
pub fn compose_answer(
    writer: &dyn StructuredWriter,
    goal: &str,
    screen: &Screen,
    items: &[Item],
    history: &[String],
    stopped: &str,
) -> Result<Answer, WriterError> {
    let text: Vec<&str> = items.iter().map(|it| it.text.as_str()).collect();
    let packet = json!({
        "goal": goal,
        "now": now_context(),
        "why_the_run_stopped": stopped,
        "actions_taken": history,
        "frontmost_app": screen.app,
        "browser_active_tab_url": screen.url,
        "screen_text_in_reading_order": text,
    });
    let properties = json!({"achieved": {"type": "boolean"}, "answer": {"type": "string"}});
    let png = png_bytes(screen);
    let data = structured(
        writer,
        &answer_model(),
        COMPOSE_ANSWER_SYSTEM,
        &packet,
        &properties,
        1024,
        png.as_deref(),
    )?;
    Ok(Answer {
        text: field_str(&data, "answer")?.trim().to_string(),
        achieved: field_bool(&data, "achieved")?,
    })
}

/// The answer to a question the user asked about the screen in front of them. Nothing here acts;
/// the writer reads the screen and explains it, naming the controls the user would press.
pub fn compose_explanation(
    writer: &dyn StructuredWriter,
    question: &str,
    screen: &Screen,
    items: &[Item],
) -> Result<String, WriterError> {
    let text: Vec<&str> = items.iter().map(|it| it.text.as_str()).collect();
    let packet = json!({
        "question": question,
        "now": now_context(),
        "frontmost_app": screen.app,
        "browser_active_tab_url": screen.url,
        "screen_text_in_reading_order": text,
    });
    let properties = json!({"answer": {"type": "string"}});
    let png = png_bytes(screen);
    let data = structured(
        writer,
        &answer_model(),
        COMPOSE_EXPLANATION_SYSTEM,
        &packet,
        &properties,
        1024,
        png.as_deref(),
    )?;
    Ok(field_str(&data, "answer")?.trim().to_string())
}

pub const COMPOSE_TEACH_SYSTEM: &str = "A user asked a question about the screen in front of them, and you are teaching them, not acting: you never press anything yourself. You receive the question, a capture of their screen as it is now, and a numbered list of the items read from that screen (index, text, role, region). Answer briefly from what is visible and nothing else: never from memory, and never a guess; plain text, no markdown, two sentences at most. Then give up to 5 concrete next steps the user would take, in order, each one short sentence naming the on-screen control by its visible label. For each step set item to the index of the single numbered item from the provided list that the step uses, or null when no listed item applies (a keyboard shortcut, a control that is not on screen yet, or anything not in the list). Never invent items or indexes that are not in the list. When the question needs no steps, return an empty steps list. Trust the capture over the text where the two disagree.";

/// The most steps a teach answer gives; anything past this is dropped.
pub const MAX_TEACH_STEPS: usize = 5;

/// One thing for the user to do, and the item on screen it uses, when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub text: String,
    /// An `Item::index` from the list the writer was given; never an index outside it.
    pub item: Option<usize>,
}

/// A short answer, then the steps that would get the user there.
#[derive(Debug, Clone, PartialEq)]
pub struct Teach {
    pub answer: String,
    pub steps: Vec<Step>,
}

/// Answer a question about the screen and teach the next steps, naming the numbered item each
/// step uses. An index the list does not hold becomes `None`: the writer cannot invent a target.
pub fn compose_teach(
    writer: &dyn StructuredWriter,
    question: &str,
    screen: &Screen,
    items: &[Item],
) -> Result<Teach, WriterError> {
    let listed: Vec<Value> = items
        .iter()
        .map(|it| {
            json!({
                "index": it.index,
                "text": it.text,
                "role": it.role,
                "region": screen.region(it),
            })
        })
        .collect();
    let packet = json!({
        "question": question,
        "now": now_context(),
        "frontmost_app": screen.app,
        "browser_active_tab_url": screen.url,
        "items": listed,
    });
    let properties = json!({
        "answer": {"type": "string"},
        "steps": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["text", "item"],
                "properties": {
                    "text": {"type": "string"},
                    "item": {"type": ["integer", "null"]},
                },
            },
        },
    });
    let png = png_bytes(screen);
    let data = structured(
        writer,
        &answer_model(),
        COMPOSE_TEACH_SYSTEM,
        &packet,
        &properties,
        1024,
        png.as_deref(),
    )?;
    let answer = field_str(&data, "answer")?.trim().to_string();
    let raw = data
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| WriterError::BadReply("no array \"steps\"".into()))?;
    let mut steps = Vec::new();
    for s in raw.iter().take(MAX_TEACH_STEPS) {
        let text = field_str(s, "text")?.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let item = s
            .get("item")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
            .filter(|i| items.iter().any(|it| it.index == *i));
        steps.push(Step { text, item });
    }
    Ok(Teach { answer, steps })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::decide::tests::{make_item, screen};
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// Tests that touch CLICKER_* model variables hold this.
    pub(crate) static ENV: Mutex<()> = Mutex::new(());

    #[derive(Debug, Clone)]
    pub(crate) struct Call {
        pub model: String,
        pub system: String,
        pub packet: Value,
        pub properties: Value,
        pub max_tokens: u32,
        pub png: Option<Vec<u8>>,
    }

    pub(crate) struct Fake {
        pub reply: Result<Value, ApiError>,
        pub calls: RefCell<Vec<Call>>,
    }

    impl Fake {
        pub(crate) fn new(reply: Value) -> Self {
            Self {
                reply: Ok(reply),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn failing(message: &str) -> Self {
            Self {
                reply: Err(ApiError {
                    status: Some(400),
                    message: message.into(),
                }),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl StructuredWriter for Fake {
        fn structured(
            &self,
            model: &str,
            system: &str,
            packet: &Value,
            properties: &Value,
            max_tokens: u32,
            png: Option<&[u8]>,
        ) -> Result<Value, ApiError> {
            self.calls.borrow_mut().push(Call {
                model: model.into(),
                system: system.into(),
                packet: packet.clone(),
                properties: properties.clone(),
                max_tokens,
                png: png.map(<[u8]>::to_vec),
            });
            self.reply.clone()
        }
    }

    #[test]
    fn valid_url_matches_the_python() {
        assert!(valid_url("https://www.cnn.com"));
        assert!(valid_url("https://news.ycombinator.com/newest"));
        assert!(!valid_url("http://www.cnn.com"));
        assert!(!valid_url("https://localhost"));
        assert!(!valid_url("https://www.cnn.com/a b"));
        assert!(!valid_url(""));
    }

    #[test]
    fn compose_url_uses_the_writer_model_and_the_url_schema() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CLICKER_WRITER_MODEL", "gpt-test");
        let w = Fake::new(json!({"ok": true, "url": " https://example.com ", "reason": "x"}));
        assert_eq!(
            compose_url(&w, "open example", &[]).unwrap(),
            "https://example.com"
        );
        std::env::remove_var("CLICKER_WRITER_MODEL");
        let call = &w.calls.borrow()[0];
        assert_eq!(call.model, "gpt-test");
        assert_eq!(call.max_tokens, 200);
        assert!(call.png.is_none());
        assert_eq!(call.system, COMPOSE_URL_SYSTEM);
        let mut keys: Vec<&String> = call.properties.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["ok", "reason", "url"]);
        assert_eq!(call.packet["goal"], "open example");
    }

    #[test]
    fn a_declined_or_invalid_url_is_empty() {
        let w = Fake::new(json!({"ok": false, "url": "https://example.com", "reason": "x"}));
        assert_eq!(compose_url(&w, "g", &[]).unwrap(), "");
        let w = Fake::new(json!({"ok": true, "url": "http://example.com", "reason": "x"}));
        assert_eq!(compose_url(&w, "g", &[]).unwrap(), "");
    }

    #[test]
    fn a_provider_failure_becomes_one_error_type() {
        let w = Fake::failing("Your credit balance is too low to access the API");
        let e = compose_url(&w, "open example", &[]).unwrap_err();
        assert_eq!(
            e,
            WriterError::Unavailable("the account is out of credit".into())
        );
        assert!(e.to_string().contains("out of credit"));
    }

    #[test]
    fn a_reply_missing_a_key_is_a_bad_reply_not_unavailability() {
        let w = Fake::new(json!({"ok": true}));
        assert!(matches!(
            compose_url(&w, "g", &[]),
            Err(WriterError::BadReply(_))
        ));
    }

    #[test]
    fn compose_text_fills_or_declines() {
        let w = Fake::new(json!({"fill": true, "text": "  rust tutorials ", "reason": "x"}));
        let items = vec![make_item(0, "Search", 100.0, 130.0)];
        let history: Vec<String> = (0..12).map(|i| format!("a{i}")).collect();
        assert_eq!(
            compose_text(&w, "search rust", &screen(), &items, &history).unwrap(),
            "rust tutorials"
        );
        let call = &w.calls.borrow()[0];
        assert_eq!(call.max_tokens, 256);
        assert_eq!(call.packet["previous_actions"].as_array().unwrap().len(), 8);
        assert_eq!(call.packet["all_screen_text"], json!(["Search"]));
        assert_eq!(call.packet["focused_field"], Value::Null);
        assert_eq!(call.packet["text_near_field"], json!([]));
        let w = Fake::new(json!({"fill": false, "text": "hunter2", "reason": "password"}));
        assert_eq!(
            compose_text(&w, "log in", &screen(), &items, &[]).unwrap(),
            ""
        );
    }

    #[test]
    fn near_field_finds_the_text_around_the_focused_field() {
        let mut s = screen();
        // Items are in capture pixels at scale 2; the field is in points.
        s.field = Some(Field {
            role: "AXTextField".into(),
            label: "Search".into(),
            placeholder: String::new(),
            value: String::new(),
            x: 100.0,
            y: 50.0,
            w: 100.0,
            h: 20.0,
        });
        let near = make_item(0, "near", 100.0, 130.0);
        let mut far = make_item(1, "far", 1100.0, 1130.0);
        far.x1 = 1800.0;
        far.x2 = 1900.0;
        assert_eq!(near_field(&s, &[near, far], 160.0), ["near"]);
    }

    #[test]
    fn the_answer_request_carries_the_capture_and_the_run() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CLICKER_ANSWER_MODEL", "answer-model");
        let w = Fake::new(json!({"achieved": true, "answer": " Sep 19, 2026 in Miami. "}));
        let got = compose_answer(
            &w,
            "find the next upcoming bruno mars concert",
            &screen(),
            &[make_item(0, "SEP 19, 2026", 100.0, 130.0)],
            &["clicked 'TOUR'".to_string()],
            "the goal is achieved",
        )
        .unwrap();
        std::env::remove_var("CLICKER_ANSWER_MODEL");
        assert_eq!(
            got,
            Answer {
                text: "Sep 19, 2026 in Miami.".into(),
                achieved: true
            }
        );
        let call = &w.calls.borrow()[0];
        assert_eq!(call.model, "answer-model");
        assert_eq!(call.max_tokens, 1024);
        assert_eq!(call.packet["why_the_run_stopped"], "the goal is achieved");
        assert_eq!(call.packet["actions_taken"], json!(["clicked 'TOUR'"]));
        assert_eq!(
            call.packet["screen_text_in_reading_order"],
            json!(["SEP 19, 2026"])
        );
        assert!(call.png.as_ref().unwrap().starts_with(b"\x89PNG"));
    }

    #[test]
    fn an_image_is_shrunk_before_it_is_sent_and_not_distorted() {
        let mut s = screen();
        s.width = 3456;
        s.height = 2234;
        s.bgra = vec![0; 3456 * 2234 * 4];
        let png = png_bytes(&s).unwrap();
        let sent = image::load_from_memory(&png).unwrap();
        assert_eq!(sent.width().max(sent.height()), ANSWER_IMAGE_EDGE);
        let ratio = sent.width() as f64 / sent.height() as f64;
        assert!((ratio - 3456.0 / 2234.0).abs() / (3456.0 / 2234.0) < 0.01);
        // The capture itself is left intact.
        assert_eq!(s.bgra.len(), 3456 * 2234 * 4);
    }

    #[test]
    fn a_small_capture_keeps_its_size_and_an_empty_one_sends_no_image() {
        let mut s = screen();
        s.width = 4;
        s.height = 2;
        s.bgra = [0u8, 0, 255, 255].repeat(8);
        let sent = image::load_from_memory(&png_bytes(&s).unwrap())
            .unwrap()
            .to_rgb8();
        assert_eq!(sent.dimensions(), (4, 2));
        assert_eq!(sent.get_pixel(0, 0).0, [255, 0, 0]); // BGRA red becomes RGB red
        s.bgra.clear();
        assert!(png_bytes(&s).is_none());
    }

    #[test]
    fn explanation_answers_only_from_the_screen() {
        let w = Fake::new(json!({"answer": " It says hello. "}));
        let got = compose_explanation(
            &w,
            "what does this say",
            &screen(),
            &[make_item(0, "hello", 100.0, 130.0)],
        );
        assert_eq!(got.unwrap(), "It says hello.");
        let call = &w.calls.borrow()[0];
        assert_eq!(call.packet["question"], "what does this say");
        assert!(call.png.is_some());
        assert_eq!(call.properties, json!({"answer": {"type": "string"}}));
    }

    #[test]
    fn teach_sends_the_numbered_items_and_the_capture_and_drops_invented_indexes() {
        let w = Fake::new(json!({"answer": " Settings is top right. ", "steps": [
            {"text": "Click Settings", "item": 1},
            {"text": "Press Ctrl+, instead", "item": null},
            {"text": "Click the ghost", "item": 99},
            {"text": "a", "item": 0}, {"text": "b", "item": 0}, {"text": "c", "item": 0},
        ]}));
        let mut settings = make_item(1, "Settings", 100.0, 130.0);
        settings.role = "button".into();
        let items = vec![make_item(0, "File", 10.0, 30.0), settings];
        let got = compose_teach(&w, "how do I open settings?", &screen(), &items).unwrap();
        assert_eq!(got.answer, "Settings is top right.");
        assert_eq!(got.steps.len(), MAX_TEACH_STEPS);
        assert_eq!(
            got.steps[..3],
            [
                Step {
                    text: "Click Settings".into(),
                    item: Some(1)
                },
                Step {
                    text: "Press Ctrl+, instead".into(),
                    item: None
                },
                Step {
                    text: "Click the ghost".into(),
                    item: None
                },
            ]
        );
        let call = &w.calls.borrow()[0];
        assert_eq!(call.system, COMPOSE_TEACH_SYSTEM);
        assert_eq!(call.max_tokens, 1024);
        assert!(call.png.as_ref().unwrap().starts_with(b"\x89PNG"));
        assert_eq!(call.packet["question"], "how do I open settings?");
        let listed = &call.packet["items"][1];
        assert_eq!(listed["index"], 1);
        assert_eq!(listed["text"], "Settings");
        assert_eq!(listed["role"], "button");
        assert!(listed["region"].is_string());
        let step = &call.properties["steps"]["items"];
        assert_eq!(step["additionalProperties"], false);
        assert_eq!(step["required"], json!(["text", "item"]));
        assert_eq!(
            step["properties"]["item"]["type"],
            json!(["integer", "null"])
        );
        let system = call.system.to_lowercase();
        assert!(system.contains("up to 5") && system.contains("never invent items"));
        assert!(system.contains("null"));
    }

    #[test]
    fn teach_without_steps_is_a_bad_reply() {
        let w = Fake::new(json!({"answer": "x"}));
        assert!(matches!(
            compose_teach(&w, "q", &screen(), &[]),
            Err(WriterError::BadReply(_))
        ));
    }

    #[test]
    fn the_models_default_to_openai() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CLICKER_WRITER_MODEL");
        std::env::remove_var("CLICKER_ANSWER_MODEL");
        assert_eq!(writer_model(), config::DEFAULT_OPENAI_WRITER_MODEL);
        assert_eq!(answer_model(), config::DEFAULT_OPENAI_ANSWER_MODEL);
    }

    #[test]
    #[ignore = "network: makes one real OpenAI writer call"]
    fn live_compose_url() {
        let env = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../.env");
        let _ = config::load_dotenv(std::path::Path::new(env));
        let ai = make_writer().expect("OPENAI_API_KEY");
        let t = std::time::Instant::now();
        let got = compose_url(&ai, "open the rust programming language website", &[]);
        println!(
            "LIVE compose_url ({}) {got:?} in {} ms",
            writer_model(),
            t.elapsed().as_millis()
        );
        assert!(got.is_ok_and(|u| valid_url(&u)));
    }
}
