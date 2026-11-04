//! Chat completions with tool calling, for the assistant's parent agent.
//!
//! Any OpenAI-compatible server works: the base URL, model and key are configuration
//! (`POINTER_AGENT_BASE_URL`, `POINTER_AGENT_MODEL`, `POINTER_AGENT_API_KEY`, falling back to
//! `OPENAI_API_KEY`). A call can be cancelled from another thread and has its own deadline. The
//! blocking HTTP request runs on a helper thread that is abandoned on cancel, since a blocking
//! reqwest call cannot be interrupted; the request's own timeout then ends it.
//!
//! Errors are short sentences meant for a person. They never contain the key.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::typesafe::{env_var, error_message};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4.1";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(25);

/// A cancellation flag shared between the thread that asks and the thread that waits.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatError {
    Cancelled,
    Timeout,
    /// The server answered with an error status.
    Http(u16, String),
    /// No answer at all: DNS, refused, TLS.
    Network(String),
    /// An answer that is not a chat completion.
    Bad(String),
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Timeout => f.write_str("the model took too long to answer"),
            Self::Http(401 | 403, _) => f.write_str("the agent key was rejected"),
            Self::Http(404, _) => f.write_str(
                "the model or URL was not found (check POINTER_AGENT_MODEL and POINTER_AGENT_BASE_URL)",
            ),
            Self::Http(429, _) => f.write_str("the model service is rate limiting this key"),
            Self::Http(s, _) if *s >= 500 => f.write_str("the model service had an error"),
            Self::Http(s, m) => write!(f, "the model service refused the request (HTTP {s}): {m}"),
            Self::Network(m) => write!(f, "could not reach the model service: {m}"),
            Self::Bad(m) => write!(f, "unexpected reply from the model service: {m}"),
        }
    }
}

impl std::error::Error for ChatError {}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments exactly as the model wrote them: a JSON string, not validated here.
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatReply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

impl ChatReply {
    /// The assistant message to keep in the history.
    pub fn message(&self) -> Value {
        let mut m = json!({ "role": "assistant", "content": self.content });
        if !self.tool_calls.is_empty() {
            m["tool_calls"] = json!(self
                .tool_calls
                .iter()
                .map(|c| json!({ "id": c.id, "type": "function",
                                 "function": { "name": c.name, "arguments": c.arguments } }))
                .collect::<Vec<_>>());
        }
        m
    }
}

/// One chat completion, behind a trait so the agent loop is tested with a fake.
pub trait Chat: Send + Sync {
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        cancel: &Cancel,
    ) -> Result<ChatReply, ChatError>;
}

pub struct ChatClient {
    key: String,
    base_url: String,
    model: String,
    timeout: Duration,
}

impl fmt::Debug for ChatClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// The agent key: `POINTER_AGENT_API_KEY`, else `OPENAI_API_KEY`.
pub fn agent_key() -> Option<String> {
    env_var("POINTER_AGENT_API_KEY").or_else(|| env_var("OPENAI_API_KEY"))
}

impl ChatClient {
    /// A client from the environment, or None when no key is set.
    pub fn from_env() -> Option<Self> {
        let key = agent_key()?;
        let base_url = env_var("POINTER_AGENT_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let model = env_var("POINTER_AGENT_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into());
        Some(Self::new(key, base_url, model))
    }

    pub fn new(key: String, base_url: String, model: String) -> Self {
        Self {
            key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// The request body.
pub fn build_request(model: &str, messages: &[Value], tools: &[Value]) -> Value {
    let mut body = json!({ "model": model, "messages": messages });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    body
}

/// Read the first choice's message.
pub fn parse_reply(body: &Value) -> Result<ChatReply, ChatError> {
    let message = body
        .pointer("/choices/0/message")
        .ok_or_else(|| ChatError::Bad("no choices".into()))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut tool_calls = Vec::new();
    for (i, c) in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let name = c
            .pointer("/function/name")
            .and_then(Value::as_str)
            .ok_or_else(|| ChatError::Bad("a tool call had no name".into()))?;
        let arguments = c
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string();
        let id = c
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{i}"));
        tool_calls.push(ToolCall {
            id,
            name: name.to_string(),
            arguments,
        });
    }
    Ok(ChatReply {
        content,
        tool_calls,
    })
}

impl Chat for ChatClient {
    fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        cancel: &Cancel,
    ) -> Result<ChatReply, ChatError> {
        if cancel.is_cancelled() {
            return Err(ChatError::Cancelled);
        }
        let body = build_request(&self.model, messages, tools);
        let url = format!("{}/chat/completions", self.base_url);
        let key = self.key.clone();
        let timeout = self.timeout;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("chat".into())
            .spawn(move || {
                let _ = tx.send(post(&url, &key, &body, timeout));
            })
            .map_err(|e| ChatError::Network(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(result) => return result.and_then(|v| parse_reply(&v)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        return Err(ChatError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(ChatError::Timeout);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ChatError::Network("the request thread ended".into()))
                }
            }
        }
    }
}

fn post(url: &str, key: &str, body: &Value, timeout: Duration) -> Result<Value, ChatError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ChatError::Network(e.to_string()))?;
    let response = client
        .post(url)
        .bearer_auth(key)
        .json(body)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                ChatError::Timeout
            } else {
                // reqwest's text can include the URL, never the key: that is a header.
                ChatError::Network(short(&e.to_string()))
            }
        })?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| ChatError::Network(short(&e.to_string())))?;
    if !status.is_success() {
        return Err(ChatError::Http(
            status.as_u16(),
            short(&error_message(&text)),
        ));
    }
    serde_json::from_str(&text).map_err(|e| ChatError::Bad(short(&e.to_string())))
}

fn short(text: &str) -> String {
    text.split('\n')
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot HTTP server on loopback. `reply` None never answers. Returns the base URL and a
    /// receiver of the raw request text.
    fn serve(reply: Option<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut buf = vec![0u8; 65536];
            let mut got = Vec::new();
            loop {
                let n = s.read(&mut buf).unwrap_or(0);
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let len = text[..split]
                        .to_lowercase()
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: ").map(str::to_string))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if got.len() >= split + 4 + len || n == 0 {
                        break;
                    }
                } else if n == 0 {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&got).to_string());
            match reply {
                Some(r) => {
                    let _ = s.write_all(r.as_bytes());
                }
                None => std::thread::sleep(Duration::from_secs(3)),
            }
        });
        (url, rx)
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn request_carries_tools_only_when_given() {
        let r = build_request("m", &[json!({"role": "user", "content": "hi"})], &[]);
        assert!(r.get("tools").is_none());
        let r = build_request("m", &[], &[json!({"type": "function"})]);
        assert_eq!(r["tool_choice"], "auto");
        assert_eq!(r["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_tool_call_round_trips_over_http_with_the_key_in_a_header() {
        let body = json!({"choices": [{"message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": "c1", "type": "function",
              "function": {"name": "observe", "arguments": "{}"}}]}}]});
        let (url, seen) = serve(Some(http("200 OK", &body.to_string())));
        let client = ChatClient::new("sk-test-secret".into(), url, "some-model".into());
        let reply = client
            .complete(
                &[json!({"role": "user", "content": "hi"})],
                &[json!({"type": "function", "function": {"name": "observe"}})],
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(reply.tool_calls[0].name, "observe");
        assert_eq!(reply.tool_calls[0].id, "c1");
        assert_eq!(
            reply.message()["tool_calls"][0]["function"]["name"],
            "observe"
        );
        let raw = seen.recv().unwrap();
        assert!(raw.starts_with("POST /chat/completions"));
        assert!(raw
            .to_lowercase()
            .contains("authorization: bearer sk-test-secret"));
        assert!(raw.contains("\"model\":\"some-model\""));
    }

    #[test]
    fn errors_are_short_and_never_hold_the_key() {
        let (url, _) = serve(Some(http(
            "401 Unauthorized",
            r#"{"error":{"message":"Incorrect API key provided: sk-test-secret"}}"#,
        )));
        let err = ChatClient::new("sk-test-secret".into(), url, "m".into())
            .complete(&[], &[], &Cancel::new())
            .unwrap_err();
        assert_eq!(err.to_string(), "the agent key was rejected");
        assert!(!format!("{err:?}").is_empty());
        let rate = ChatError::Http(429, "x".into()).to_string();
        assert!(rate.contains("rate limiting"));
        assert!(ChatError::Http(503, "x".into())
            .to_string()
            .contains("had an error"));
    }

    #[test]
    fn a_silent_server_times_out() {
        let (url, _) = serve(None);
        let t = Instant::now();
        let err = ChatClient::new("k".into(), url, "m".into())
            .with_timeout(Duration::from_millis(300))
            .complete(&[], &[], &Cancel::new())
            .unwrap_err();
        assert_eq!(err, ChatError::Timeout);
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancel_returns_promptly_while_the_server_is_silent() {
        let (url, _) = serve(None);
        let cancel = Cancel::new();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c.cancel();
        });
        let t = Instant::now();
        let err = ChatClient::new("k".into(), url, "m".into())
            .complete(&[], &[], &cancel)
            .unwrap_err();
        assert_eq!(err, ChatError::Cancelled);
        assert!(t.elapsed() < Duration::from_secs(2));
        // Already cancelled: no request is made at all.
        assert_eq!(
            ChatClient::new("k".into(), "http://127.0.0.1:9".into(), "m".into())
                .complete(&[], &[], &cancel)
                .unwrap_err(),
            ChatError::Cancelled
        );
    }

    #[test]
    fn parse_handles_text_and_missing_ids() {
        let r = parse_reply(&json!({"choices": [{"message": {"content": "hello"}}]})).unwrap();
        assert_eq!(r.content, "hello");
        assert!(r.tool_calls.is_empty());
        let r = parse_reply(&json!({"choices": [{"message": {"content": "",
            "tool_calls": [{"function": {"name": "x", "arguments": "{\"a\":1}"}}]}}]}))
        .unwrap();
        assert_eq!(r.tool_calls[0].id, "call_0");
        assert!(parse_reply(&json!({})).is_err());
    }
}
