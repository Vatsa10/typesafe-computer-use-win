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
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
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

/// Token counts the service reported for one call. All zero when it reported none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Prompt tokens that were not served from the cache.
    pub input: u64,
    pub output: u64,
    /// Prompt tokens read from the cache.
    pub cache_read: u64,
    /// Prompt tokens written to the cache.
    pub cache_write: u64,
}

impl Usage {
    /// Every prompt token of the call, cached or not.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
    pub fn total(&self) -> u64 {
        self.prompt() + self.output
    }
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
    pub fn to_json(&self) -> Value {
        json!({ "input": self.input, "output": self.output, "cache_read": self.cache_read,
                "cache_write": self.cache_write, "total": self.total() })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatReply {
    /// What the service reported for this call.
    pub usage: Usage,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// Claude's thinking blocks, kept so the next turn can send them back unchanged.
    pub thinking: Vec<Value>,
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
        if !self.thinking.is_empty() {
            m[crate::anthropic::THINKING_KEY] = json!(self.thinking);
        }
        m
    }
}

/// Which service holds the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Claude,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Claude => "claude",
        }
    }
    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => DEFAULT_MODEL,
            Self::Claude => crate::anthropic::DEFAULT_MODEL,
        }
    }
}

fn openai_key() -> Option<String> {
    env_var("POINTER_AGENT_API_KEY").or_else(|| env_var("OPENAI_API_KEY"))
}

/// The provider in force. `POINTER_LLM` (`openai` or `claude`) chooses; with an explicit choice
/// and no key for it there is no provider. Unset: OpenAI if its key exists, else Claude if its key
/// exists, else none (quick mode).
pub fn provider() -> Option<Provider> {
    match env_var("POINTER_LLM").map(|v| v.to_lowercase()).as_deref() {
        Some("openai") => return openai_key().map(|_| Provider::OpenAi),
        Some("claude" | "anthropic") => {
            return crate::anthropic::api_key().map(|_| Provider::Claude)
        }
        _ => {}
    }
    if openai_key().is_some() {
        Some(Provider::OpenAi)
    } else {
        crate::anthropic::api_key().map(|_| Provider::Claude)
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
    provider: Provider,
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

/// The key of the provider in force: `POINTER_AGENT_API_KEY` else `OPENAI_API_KEY` for OpenAI,
/// `ANTHROPIC_API_KEY` for Claude.
pub fn agent_key() -> Option<String> {
    match provider()? {
        Provider::OpenAi => openai_key(),
        Provider::Claude => crate::anthropic::api_key(),
    }
}

impl ChatClient {
    /// A client from the environment, or None when no key is set.
    pub fn from_env() -> Option<Self> {
        let provider = provider()?;
        let key = agent_key()?;
        let model = env_var("POINTER_AGENT_MODEL").unwrap_or_else(|| provider.default_model().into());
        Some(match provider {
            Provider::OpenAi => {
                let base =
                    env_var("POINTER_AGENT_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into());
                Self::new(key, base, model)
            }
            Provider::Claude => Self::claude(key, crate::anthropic::base_url(), model),
        })
    }

    /// A Claude client on the native Messages API.
    pub fn claude(key: String, base_url: String, model: String) -> Self {
        Self {
            provider: Provider::Claude,
            ..Self::new(key, base_url, model)
        }
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn new(key: String, base_url: String, model: String) -> Self {
        Self {
            provider: Provider::OpenAi,
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
    // The Claude-only thinking key must not reach an OpenAI server.
    let messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(o) = m.as_object_mut() {
                o.remove(crate::anthropic::THINKING_KEY);
            }
            m
        })
        .collect();
    let mut body = json!({ "model": model, "messages": messages });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
        // gpt-6 models refuse function tools on chat completions unless reasoning is off.
        if model.starts_with("gpt-6") {
            body["reasoning_effort"] = json!("none");
        }
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
    let u = |k: &str| body.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    let cached = body
        .pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Ok(ChatReply {
        usage: Usage {
            input: u("prompt_tokens").saturating_sub(cached),
            output: u("completion_tokens"),
            cache_read: cached,
            cache_write: 0,
        },
        content,
        tool_calls,
        thinking: Vec::new(),
    })
}

/// gpt-6 models only reason alongside function tools on the Responses API; on chat completions
/// tools force reasoning off. Compatible servers keep chat completions.
fn wants_responses(model: &str, tools: &[Value]) -> bool {
    model.starts_with("gpt-6") && !tools.is_empty()
}

fn response_parts(content: &Value) -> Value {
    match content.as_array() {
        Some(parts) => Value::Array(
            parts
                .iter()
                .map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("image_url") => json!({ "type": "input_image", "image_url": p.pointer("/image_url/url") }),
                    _ => json!({ "type": "input_text", "text": p.get("text").cloned().unwrap_or(json!("")) }),
                })
                .collect(),
        ),
        None => content.clone(),
    }
}

/// The same conversation in Responses form: tool calls and results become their own input items.
pub fn build_responses_request(model: &str, messages: &[Value], tools: &[Value]) -> Value {
    let mut input = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "tool" => input.push(json!({ "type": "function_call_output", "call_id": m["tool_call_id"],
                                         "output": m["content"].as_str().unwrap_or("") })),
            "assistant" => {
                if m["content"].as_str().is_some_and(|t| !t.is_empty()) {
                    input.push(json!({ "role": "assistant", "content": m["content"] }));
                }
                for call in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    input.push(json!({ "type": "function_call", "call_id": call["id"],
                                       "name": call.pointer("/function/name"),
                                       "arguments": call.pointer("/function/arguments") }));
                }
            }
            _ => input.push(json!({ "role": role, "content": response_parts(&m["content"]) })),
        }
    }
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            let f = &t["function"];
            json!({ "type": "function", "name": f["name"], "description": f["description"],
                    "parameters": f["parameters"], "strict": false })
        })
        .collect();
    json!({ "model": model, "input": input, "tools": tools, "tool_choice": "auto",
            "reasoning": { "effort": "low" }, "store": false })
}

pub fn parse_responses_reply(body: &Value) -> Result<ChatReply, ChatError> {
    let output = body.get("output").and_then(Value::as_array).ok_or_else(|| ChatError::Bad("no output".into()))?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                    content.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                }
            }
            Some("function_call") => tool_calls.push(ToolCall {
                id: item["call_id"].as_str().unwrap_or("call").to_string(),
                name: item["name"].as_str().ok_or_else(|| ChatError::Bad("a tool call had no name".into()))?.to_string(),
                arguments: item["arguments"].as_str().unwrap_or("{}").to_string(),
            }),
            _ => {}
        }
    }
    let u = |k: &str| body.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    let cached = u("input_tokens_details/cached_tokens");
    Ok(ChatReply {
        usage: Usage { input: u("input_tokens").saturating_sub(cached), output: u("output_tokens"), cache_read: cached, cache_write: 0 },
        content,
        tool_calls,
        thinking: Vec::new(),
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
        let provider = self.provider;
        let responses = provider == Provider::OpenAi && wants_responses(&self.model, tools);
        let body = match provider {
            Provider::OpenAi if responses => build_responses_request(&self.model, messages, tools),
            Provider::OpenAi => build_request(&self.model, messages, tools),
            Provider::Claude => crate::anthropic::build_request(&self.model, messages, tools),
        };
        let path = if responses { "responses" } else { "chat/completions" };
        let url = format!("{}/{path}", self.base_url);
        let url_base = self.base_url.clone();
        let key = self.key.clone();
        let timeout = self.timeout;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("chat".into())
            .spawn(move || {
                let r = match provider {
                    Provider::OpenAi => post(&url, &key, &body, timeout),
                    Provider::Claude => crate::anthropic::post(&url_base, &key, &body, timeout),
                };
                let _ = tx.send(r);
            })
            .map_err(|e| ChatError::Network(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(result) => {
                    return result.and_then(|v| match provider {
                        Provider::OpenAi if responses => parse_responses_reply(&v),
                        Provider::OpenAi => parse_reply(&v),
                        Provider::Claude => crate::anthropic::parse_reply(&v),
                    })
                }
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
        assert!(r.get("reasoning_effort").is_none());
        let r = build_request("gpt-6-luna", &[], &[json!({"type": "function"})]);
        assert_eq!(r["reasoning_effort"], "none");
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
    fn a_claude_turn_round_trips_over_http_with_headers_and_thinking() {
        let body = json!({"stop_reason": "tool_use", "content": [
            {"type": "thinking", "thinking": "t", "signature": "s"},
            {"type": "tool_use", "id": "t1", "name": "observe", "input": {}}]});
        let (url, seen) = serve(Some(http("200 OK", &body.to_string())));
        let client = ChatClient::claude("sk-ant-secret".into(), url, "claude-haiku-5-5".into());
        let reply = client
            .complete(
                &[json!({"role": "system", "content": "sys"}), json!({"role": "user", "content": "hi"})],
                &[json!({"type": "function", "function": {"name": "observe", "parameters": {"type": "object"}}})],
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(reply.tool_calls[0].name, "observe");
        assert_eq!(reply.message()["anthropic_thinking"][0]["signature"], "s");
        let raw = seen.recv().unwrap();
        let lower = raw.to_lowercase();
        assert!(raw.starts_with("POST /v1/messages"));
        assert!(lower.contains("x-api-key: sk-ant-secret"));
        assert!(lower.contains("anthropic-version: 2023-06-01"));
        assert!(!lower.contains("authorization"));
        assert!(raw.contains("\"system\"") && raw.contains("\"input_schema\""));
    }

    #[test]
    fn claude_errors_are_short_and_keyless() {
        let (url, _) = serve(Some(http(
            "401 Unauthorized",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key sk-ant-secret"}}"#,
        )));
        let err = ChatClient::claude("sk-ant-secret".into(), url, "m".into())
            .complete(&[], &[], &Cancel::new())
            .unwrap_err();
        assert_eq!(err.to_string(), "the agent key was rejected");
    }

    #[test]
    fn the_thinking_key_never_reaches_an_openai_server() {
        let r = build_request(
            "m",
            &[json!({"role": "assistant", "content": "", "anthropic_thinking": [{"type": "thinking"}]})],
            &[],
        );
        assert!(r["messages"][0].get("anthropic_thinking").is_none());
    }

    #[test]
    fn provider_choice_and_fallback_order() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let names = ["POINTER_LLM", "POINTER_AGENT_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY",
            "POINTER_AGENT_MODEL"];
        let saved: Vec<_> = names.iter().map(|n| (*n, std::env::var(n).ok())).collect();
        for n in names {
            std::env::remove_var(n);
        }
        assert_eq!(provider(), None);
        assert!(agent_key().is_none() && ChatClient::from_env().is_none());
        std::env::set_var("ANTHROPIC_API_KEY", "a");
        assert_eq!(provider(), Some(Provider::Claude));
        assert_eq!(ChatClient::from_env().unwrap().model(), "claude-haiku-5-5");
        std::env::set_var("OPENAI_API_KEY", "o");
        assert_eq!(provider(), Some(Provider::OpenAi));
        assert_eq!(ChatClient::from_env().unwrap().model(), "gpt-6-luna");
        std::env::set_var("POINTER_LLM", "claude");
        assert_eq!(provider(), Some(Provider::Claude));
        assert_eq!(agent_key().as_deref(), Some("a"));
        std::env::set_var("POINTER_AGENT_MODEL", "custom");
        assert_eq!(ChatClient::from_env().unwrap().model(), "custom");
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert_eq!(provider(), None, "an explicit choice without its key is not a silent switch");
        for (n, v) in saved {
            match v {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
        }
    }

    #[test]
    fn a_tool_turn_converts_to_responses_and_back() {
        let tool = json!({ "type": "function", "function": { "name": "pick", "description": "d", "parameters": {} } });
        let messages = vec![
            json!({ "role": "system", "content": "s" }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "look" },
                                                 { "type": "image_url", "image_url": { "url": "data:x" } }] }),
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "pick", "arguments": "{}" } }] }),
            json!({ "role": "tool", "tool_call_id": "c1", "content": "ok" }),
        ];
        let r = build_responses_request("gpt-6-luna", &messages, &[tool]);
        assert_eq!(r["reasoning"]["effort"], "low");
        assert_eq!(r["tools"][0]["name"], "pick");
        assert_eq!(r["input"][1]["content"][1]["type"], "input_image");
        assert_eq!(r["input"][2]["type"], "function_call");
        assert_eq!(r["input"][3]["type"], "function_call_output");
        let reply = parse_responses_reply(&json!({ "output": [
            { "type": "reasoning" },
            { "type": "message", "content": [{ "type": "output_text", "text": "hi" }] },
            { "type": "function_call", "call_id": "c2", "name": "pick", "arguments": "{}" }],
            "usage": { "input_tokens": 10, "output_tokens": 2, "input_tokens_details": { "cached_tokens": 4 } } })).unwrap();
        assert_eq!((reply.content.as_str(), reply.tool_calls[0].id.as_str()), ("hi", "c2"));
        assert_eq!((reply.usage.input, reply.usage.cache_read), (6, 4));
        assert!(wants_responses("gpt-6-luna", &[json!({})]) && !wants_responses("gpt-6-luna", &[]));
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

#[cfg(test)]
mod live {
    //! Real-network turns, run by hand: `POINTER_HOME=<dir with .env> cargo test -p api live -- --ignored --nocapture`.
    //! The tools are fakes; nothing touches the machine. Keys are never printed.
    use super::*;
    use crate::typesafe::ApiError;
    use base64::Engine;
    use std::time::Instant;

    const RED: &str = "iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAb0lEQVR4nO3PAQkAAAyEwO9feoshgnABdLep8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3IPanc8OLDQitxAAAAAElFTkSuQmCC";

    fn load() {
        if let Ok(home) = std::env::var("POINTER_HOME") {
            let text = std::fs::read_to_string(std::path::Path::new(&home).join(".env")).unwrap_or_default();
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    if std::env::var_os(k.trim()).is_none() {
                        std::env::set_var(k.trim(), v.trim().trim_matches('"'));
                    }
                }
            }
        }
    }

    fn tools() -> Vec<Value> {
        ["get_time", "get_weather"]
            .iter()
            .map(|n| json!({"type": "function", "function": {"name": n, "description": format!("Return the {n} result."),
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "additionalProperties": false}}}))
            .collect()
    }

    fn turn(client: &ChatClient) {
        let mut m = vec![
            json!({"role": "system", "content": "You are a terse assistant. Use tools when asked."}),
            json!({"role": "user", "content": "Call get_time and get_weather for Paris, both at once, then tell me both results."}),
        ];
        let cancel = Cancel::new();
        let t = Instant::now();
        let r1 = client.complete(&m, &tools(), &cancel).expect("first call");
        println!("  call 1: {} ms, {} tool calls, thinking blocks {}", t.elapsed().as_millis(), r1.tool_calls.len(), r1.thinking.len());
        assert!(!r1.tool_calls.is_empty(), "no tool call: {r1:?}");
        m.push(r1.message());
        for c in &r1.tool_calls {
            m.push(json!({"role": "tool", "tool_call_id": c.id, "content": format!("{} ok: 14:05 / 12C rain", c.name)}));
        }
        m.push(json!({"role": "user", "content": [
            {"type": "text", "text": "Here is the screen. What single colour fills it? Answer with the colour word and the tool results."},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{RED}")}}]}));
        let t = Instant::now();
        let r2 = client.complete(&m, &tools(), &cancel).expect("second call");
        println!("  call 2: {} ms, {} tool calls, reply: {}", t.elapsed().as_millis(), r2.tool_calls.len(), r2.content.replace(char::from(10), " "));
        assert!(r2.content.to_lowercase().contains("red"), "image not read: {r2:?}");
    }

    fn writer(name: &str, f: impl Fn(&str, &str, &Value, &Value, u32, Option<&[u8]>) -> Result<Value, ApiError>, model: &str) {
        let props = json!({"colour": {"type": "string"}, "ok": {"type": "boolean"}});
        let png = base64::engine::general_purpose::STANDARD.decode(RED).unwrap();
        let t = Instant::now();
        let v = f(model, "Describe the image.", &json!({"task": "name the colour"}), &props, 300, Some(&png));
        println!("  {name} writer: {} ms -> {v:?}", t.elapsed().as_millis());
        assert!(v.is_ok_and(|v| v["colour"].as_str().unwrap_or("").to_lowercase().contains("red")));
    }

    #[test]
    #[ignore = "network: real OpenAI call"]
    fn live_openai_luna_tool_loop_and_writer() {
        load();
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
        println!("gpt-6-luna");
        let c = ChatClient::new(key.clone(), DEFAULT_BASE_URL.into(), "gpt-6-luna".into());
        turn(&c);
        let o = crate::openai::OpenAi::new(key, DEFAULT_BASE_URL.into());
        writer("openai", |m, s, p, pr, mt, i| o.structured(m, s, p, pr, mt, i), "gpt-6-luna");
    }

    #[test]
    #[ignore = "network: real Anthropic call"]
    fn live_claude_haiku_tool_loop_and_writer() {
        load();
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        println!("claude-haiku-5-5");
        let c = ChatClient::claude(key.clone(), crate::anthropic::DEFAULT_BASE_URL.into(), "claude-haiku-5-5".into());
        turn(&c);
        let a = crate::anthropic::Claude::new(key, crate::anthropic::DEFAULT_BASE_URL.into());
        writer("claude", |m, s, p, pr, mt, i| a.structured(m, s, p, pr, mt, i), "claude-haiku-5-5");
    }
}
