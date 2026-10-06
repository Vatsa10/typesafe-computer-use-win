//! TypeSafe, the decision model: one `system_one` call asks every question about one state.
//!
//! `serde_json` is built without `preserve_order` here, so a `serde_json::Map` would sort the
//! criteria alphabetically. The request body is therefore written by hand, keeping questions and
//! criteria in the order the caller gave them.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
const DEFAULT_MODEL: &str = "jev-latest";
const TIMEOUT: Duration = Duration::from_secs(10);

/// A failed call to either service. `status` is the HTTP status when the server answered.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    pub(crate) fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn transport(e: reqwest::Error) -> Self {
        Self::new(e.status().map(|s| s.as_u16()), e.to_string())
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ApiError {}

/// One question. Criteria map each option to an optional description, in the order given.
#[derive(Debug, Clone, PartialEq)]
pub enum Question {
    Choice {
        instructions: String,
        criteria: Vec<(String, Option<String>)>,
    },
    Noul {
        instructions: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: Vec<(String, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Choice(ChoiceAnswer),
    Noul(f64),
}

pub struct TypeSafe {
    key: String,
    base_url: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl fmt::Debug for TypeSafe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypeSafe")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl TypeSafe {
    /// A client from `TYPESAFE_API_KEY`, or None when the key is not set.
    pub fn from_env() -> Option<Self> {
        let key = env_var("TYPESAFE_API_KEY")?;
        let base_url = env_var("TYPESAFE_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let model = env_var("TYPESAFE_DEFAULT_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into());
        Some(Self::new(key, base_url, model))
    }

    pub fn new(key: String, base_url: String, model: String) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("an HTTP client with only a timeout set always builds");
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            key,
            base_url,
            model,
            client,
        }
    }

    /// Ask every question about `state`. Answers come back in the order the questions were given.
    pub fn system_one(
        &self,
        state: &Value,
        questions: &[(String, Question)],
    ) -> Result<Vec<(String, Answer)>, ApiError> {
        let body = request_body(state, &self.model, questions);
        let response = self
            .client
            .post(format!("{}/v1/systemone", self.base_url))
            .bearer_auth(&self.key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(ApiError::transport)?;
        let status = response.status();
        let text = response.text().map_err(ApiError::transport)?;
        if !status.is_success() {
            return Err(ApiError::new(Some(status.as_u16()), error_message(&text)));
        }
        parse_response(&text, questions)
    }
}

/// The message inside an error body, or the body itself.
pub(crate) fn error_message(text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let found = parsed.as_ref().and_then(|v| {
        let e = v.get("error").unwrap_or(v);
        e.get("message")
            .or_else(|| e.get("detail"))
            .and_then(|m| m.as_str().map(str::to_string))
            .or_else(|| e.as_str().map(str::to_string))
    });
    found.unwrap_or_else(|| text.trim().to_string())
}

fn quote(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// The JSON body, written by hand so questions and criteria keep the caller's order.
pub(crate) fn request_body(state: &Value, model: &str, questions: &[(String, Question)]) -> String {
    let mut out = String::new();
    out.push_str("{\"state\":");
    out.push_str(&state.to_string());
    out.push_str(",\"model\":");
    out.push_str(&quote(model));
    out.push_str(",\"questions\":{");
    for (i, (name, question)) in questions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&quote(name));
        out.push(':');
        match question {
            Question::Choice {
                instructions,
                criteria,
            } => {
                out.push_str("{\"type\":\"choice\",\"criteria\":{");
                for (j, (option, description)) in criteria.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push_str(&quote(option));
                    out.push(':');
                    match description {
                        Some(d) => out.push_str(&quote(d)),
                        None => out.push_str("null"),
                    }
                }
                out.push_str("},\"instructions\":");
                out.push_str(&quote(instructions));
                out.push('}');
            }
            Question::Noul { instructions } => {
                out.push_str("{\"type\":\"noul\",\"instructions\":");
                out.push_str(&quote(instructions));
                out.push('}');
            }
        }
    }
    out.push_str("}}");
    out
}

fn bad(why: impl Into<String>) -> ApiError {
    ApiError::new(
        None,
        format!("unexpected TypeSafe response: {}", why.into()),
    )
}

/// The answers, in question order. Probabilities follow the question's criteria order.
pub(crate) fn parse_response(
    text: &str,
    questions: &[(String, Question)],
) -> Result<Vec<(String, Answer)>, ApiError> {
    let value: Value = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    let answers = value
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("no answers"))?;
    let mut out = Vec::with_capacity(questions.len());
    for (name, question) in questions {
        let a = answers
            .get(name)
            .ok_or_else(|| bad(format!("no answer for {name}")))?;
        let answer = match question {
            Question::Choice { criteria, .. } => {
                let choice = a
                    .get("choice")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad(format!("{name} has no choice")))?
                    .to_string();
                let confidence = a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
                let probs = a.get("probabilities").and_then(Value::as_object);
                let mut probabilities: Vec<(String, f64)> = criteria
                    .iter()
                    .map(|(k, _)| {
                        let p = probs
                            .and_then(|m| m.get(k))
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0);
                        (k.clone(), p)
                    })
                    .collect();
                if let Some(m) = probs {
                    for (k, v) in m {
                        if !criteria.iter().any(|(c, _)| c == k) {
                            probabilities.push((k.clone(), v.as_f64().unwrap_or(0.0)));
                        }
                    }
                }
                Answer::Choice(ChoiceAnswer {
                    choice,
                    confidence,
                    probabilities,
                })
            }
            Question::Noul { .. } => Answer::Noul(
                a.get("noul")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| bad(format!("{name} has no noul")))?,
            ),
        };
        out.push((name.clone(), answer));
    }
    Ok(out)
}

/// Reads `KEY=VALUE` lines from the repository `.env` into the process environment, for live
/// tests. Values are never printed.
#[cfg(test)]
pub(crate) fn load_dotenv() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../.env");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            // SAFETY: only the ignored live tests call this, before any client is built.
            unsafe { std::env::set_var(k.trim(), v) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn choice(criteria: &[(&str, Option<&str>)]) -> Question {
        Question::Choice {
            instructions: "Which action?".into(),
            criteria: criteria
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
                .collect(),
        }
    }

    fn questions(criteria: &[(&str, Option<&str>)]) -> Vec<(String, Question)> {
        vec![
            ("kind".into(), choice(criteria)),
            (
                "ok".into(),
                Question::Noul {
                    instructions: "Is this clear?".into(),
                },
            ),
        ]
    }

    const WAIT_FIRST: &[(&str, Option<&str>)] = &[("wait", None), ("open", Some("open a site"))];
    const OPEN_FIRST: &[(&str, Option<&str>)] = &[("open", Some("open a site")), ("wait", None)];

    #[test]
    fn body_keeps_order_and_null_criteria() {
        let body = request_body(&json!({"goal": "x"}), "jev-latest", &questions(WAIT_FIRST));
        assert_eq!(
            body,
            r#"{"state":{"goal":"x"},"model":"jev-latest","questions":{"kind":{"type":"choice","criteria":{"wait":null,"open":"open a site"},"instructions":"Which action?"},"ok":{"type":"noul","instructions":"Is this clear?"}}}"#
        );
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["questions"]["kind"]["criteria"]["wait"], Value::Null);
    }

    #[test]
    fn body_escapes_strings() {
        let q = vec![(
            "a\"b".to_string(),
            Question::Noul {
                instructions: "line\nnext".into(),
            },
        )];
        let body = request_body(&json!(null), "m", &q);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["questions"]["a\"b"]["instructions"], "line\nnext");
    }

    #[test]
    fn parses_answers_with_probabilities() {
        let text = r#"{"model":"jev-1.13.0","answers":{"kind":{"type":"choice","choice":"open","confidence":1.0,"probabilities":{"open":1.0,"wait":0.0}},"ok":{"type":"noul","noul":0.92}},"usage":{"input_tokens":318,"output_tokens":48}}"#;
        let got = parse_response(text, &questions(WAIT_FIRST)).unwrap();
        assert_eq!(
            got,
            vec![
                (
                    "kind".to_string(),
                    Answer::Choice(ChoiceAnswer {
                        choice: "open".into(),
                        confidence: 1.0,
                        probabilities: vec![("wait".into(), 0.0), ("open".into(), 1.0)],
                    })
                ),
                ("ok".to_string(), Answer::Noul(0.92)),
            ]
        );
    }

    #[test]
    fn missing_answer_is_an_error() {
        assert!(parse_response(r#"{"answers":{}}"#, &questions(WAIT_FIRST)).is_err());
    }

    #[test]
    fn error_message_digs_out_message() {
        assert_eq!(
            error_message(r#"{"error":{"message":"bad key"}}"#),
            "bad key"
        );
        assert_eq!(error_message(r#"{"detail":"nope"}"#), "nope");
        assert_eq!(error_message("plain"), "plain");
    }

    #[test]
    #[ignore = "network: makes real TypeSafe calls"]
    fn live_system_one() {
        load_dotenv();
        let ts = TypeSafe::from_env().expect("TYPESAFE_API_KEY");
        let state = json!({"goal": "open claude.ai in the browser", "screen": "an empty desktop"});
        for order in [OPEN_FIRST, WAIT_FIRST] {
            let t = std::time::Instant::now();
            let got = ts.system_one(&state, &questions(order)).expect("live call");
            println!("LIVE typesafe {got:?} in {} ms", t.elapsed().as_millis());
        }
    }
}
