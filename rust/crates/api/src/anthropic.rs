//! Claude over the native Messages API (`POST /v1/messages`).
//!
//! Rust has no official Anthropic SDK, so this is plain HTTP. The rest of the program speaks the
//! OpenAI chat shape (the history, the tool definitions); this module converts that shape to
//! Anthropic's and the reply back, so the agent loop does not know which provider it is on.
//!
//! Choices made for a latency-sensitive assistant on Haiku 5.5: `output_config.effort` is `low`;
//! no `temperature`, `top_p` or `top_k` (they are rejected); no assistant prefill; thinking blocks
//! from a reply are kept in the history and sent back unchanged on the next tool-loop turn; the
//! system prompt and the tools carry `cache_control` and are built byte-stably so the prefix caches.

use std::fmt;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::chat::{ChatError, ChatReply, ToolCall, Usage};
pub use crate::typesafe::ApiError;
use crate::typesafe::{env_var, error_message};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";
pub const VERSION: &str = "2023-06-01";
pub const MAX_TOKENS: u32 = 16000;
/// The history key that carries thinking blocks between turns. OpenAI requests drop it.
pub const THINKING_KEY: &str = "anthropic_thinking";
pub const REFUSAL_SENTENCE: &str = "I could not help with that one.";
const TIMEOUT: Duration = Duration::from_secs(120);

pub fn api_key() -> Option<String> {
    env_var("ANTHROPIC_API_KEY")
}

pub fn base_url() -> String {
    env_var("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into())
}

// ------------------------------------------------------------------ history conversion

fn data_url_image(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media = meta.split(';').next().unwrap_or("image/png");
        return Some(json!({"type": "image",
            "source": {"type": "base64", "media_type": media, "data": data}}));
    }
    Some(json!({"type": "image", "source": {"type": "url", "url": url}}))
}

fn user_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = p.get("text").and_then(Value::as_str).unwrap_or("");
                    (!t.is_empty()).then(|| json!({"type": "text", "text": t}))
                }
                Some("image_url") => p
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .and_then(data_url_image),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn push(out: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    // Consecutive messages of one role merge: all parallel tool results (and any images that
    // follow them) must sit in a single user message, tool results first.
    if let Some(last) = out.last_mut() {
        if last["role"] == role {
            if let Some(arr) = last["content"].as_array_mut() {
                arr.extend(blocks);
                return;
            }
        }
    }
    out.push(json!({"role": role, "content": blocks}));
}

/// The system text and the Anthropic messages for an OpenAI-shaped history.
pub fn convert_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "system" => {
                if let Some(t) = m.get("content").and_then(Value::as_str) {
                    system.push(t.to_string());
                }
            }
            "user" => push(&mut out, "user", user_blocks(&m["content"])),
            "assistant" => {
                let mut blocks: Vec<Value> = m
                    .get(THINKING_KEY)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(t) = m
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for c in m
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let args = c
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    let input = serde_json::from_str::<Value>(args)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({"type": "tool_use",
                        "id": c.get("id").cloned().unwrap_or(json!("call_0")),
                        "name": c.pointer("/function/name").cloned().unwrap_or(json!("")),
                        "input": input}));
                }
                push(&mut out, "assistant", blocks);
            }
            "tool" => {
                let text = match &m["content"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                push(
                    &mut out,
                    "user",
                    vec![json!({"type": "tool_result",
                        "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(json!("")),
                        "content": text})],
                );
            }
            _ => {}
        }
    }
    (system.join("\n\n"), out)
}

fn convert_tools(tools: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = tools
        .iter()
        .map(|t| {
            let f = t.get("function").unwrap_or(t);
            let mut o = Map::new();
            o.insert("name".into(), f.get("name").cloned().unwrap_or(json!("")));
            if let Some(d) = f.get("description") {
                o.insert("description".into(), d.clone());
            }
            o.insert(
                "input_schema".into(),
                f.get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            );
            Value::Object(o)
        })
        .collect();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    out
}

/// The system prompt as blocks. The first system message is the stable prompt and is its own
/// cached block; every later system message (the clock, the machine note, the goal status) changes
/// per turn and goes in a second block after the cache breakpoint, so it never invalidates the
/// cached prefix.
pub fn system_blocks(messages: &[Value]) -> Vec<Value> {
    let texts: Vec<&str> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = Vec::new();
    if let Some(first) = texts.first() {
        out.push(json!({"type": "text", "text": first, "cache_control": {"type": "ephemeral"}}));
    }
    if texts.len() > 1 {
        out.push(json!({"type": "text", "text": texts[1..].join("

")}));
    }
    out
}

/// The request body for a tool-loop turn.
pub fn build_request(model: &str, messages: &[Value], tools: &[Value]) -> Value {
    let (system, msgs) = convert_messages(messages);
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "output_config": {"effort": "low"},
        "messages": msgs,
    });
    let blocks = system_blocks(messages);
    if !blocks.is_empty() {
        body["system"] = json!(blocks);
    }
    let _ = system;
    if !tools.is_empty() {
        body["tools"] = json!(convert_tools(tools));
    }
    body
}

/// Read a Messages reply. A refusal becomes a short sentence the assistant can say; a reply cut
/// off at the token limit is an error, since a half-written tool call must not run.
pub fn parse_reply(body: &Value) -> Result<ChatReply, ChatError> {
    let blocks = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ChatError::Bad("no content".into()))?;
    let mut reply = ChatReply::default();
    let u = |k: &str| body.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
    reply.usage = Usage {
        input: u("input_tokens"),
        output: u("output_tokens"),
        cache_read: u("cache_read_input_tokens"),
        cache_write: u("cache_creation_input_tokens"),
    };
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => reply
                .content
                .push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => {
                let name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ChatError::Bad("a tool call had no name".into()))?;
                let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                reply.tool_calls.push(ToolCall {
                    id: b
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{}", reply.tool_calls.len())),
                    name: name.to_string(),
                    arguments: input.to_string(),
                });
            }
            Some("thinking" | "redacted_thinking") => reply.thinking.push(b.clone()),
            _ => {}
        }
    }
    match body.get("stop_reason").and_then(Value::as_str) {
        Some("refusal") => {
            return Ok(ChatReply {
                content: REFUSAL_SENTENCE.into(),
                usage: reply.usage,
                ..Default::default()
            })
        }
        Some("max_tokens") => return Err(ChatError::Bad("the answer was cut off".into())),
        _ => {}
    }
    Ok(reply)
}

// ------------------------------------------------------------------ transport

fn short(t: &str) -> String {
    t.split('\n').next().unwrap_or("").chars().take(120).collect()
}

/// One POST to `/v1/messages`.
pub fn post(base: &str, key: &str, body: &Value, timeout: Duration) -> Result<Value, ChatError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ChatError::Network(e.to_string()))?;
    let response = client
        .post(format!("{}/v1/messages", base.trim_end_matches('/')))
        .header("x-api-key", key)
        .header("anthropic-version", VERSION)
        .json(body)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                ChatError::Timeout
            } else {
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

// ------------------------------------------------------------------ the writer

/// Claude as the writer: one structured JSON reply, optionally about a screenshot.
pub struct Claude {
    key: String,
    base_url: String,
}

impl fmt::Debug for Claude {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Claude")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Claude {
    pub fn from_env() -> Option<Self> {
        Some(Self::new(api_key()?, base_url()))
    }

    pub fn new(key: String, base_url: String) -> Self {
        Self { key, base_url }
    }

    pub fn structured(
        &self,
        model: &str,
        system: &str,
        packet: &Value,
        properties: &Value,
        max_tokens: u32,
        png: Option<&[u8]>,
    ) -> Result<Value, ApiError> {
        let body = structured_request(model, system, packet, properties, max_tokens, png);
        let reply = post(&self.base_url, &self.key, &body, TIMEOUT).map_err(|e| match e {
            ChatError::Http(s, m) => ApiError::new(Some(s), m),
            other => ApiError::new(None, other.to_string()),
        })?;
        structured_text(&reply)
    }
}

pub(crate) fn structured_request(
    model: &str,
    system: &str,
    packet: &Value,
    properties: &Value,
    max_tokens: u32,
    png: Option<&[u8]>,
) -> Value {
    let mut content = Vec::new();
    if let Some(png) = png {
        content.push(json!({"type": "image", "source": {"type": "base64",
            "media_type": "image/png",
            "data": base64::engine::general_purpose::STANDARD.encode(png)}}));
    }
    content.push(json!({"type": "text", "text": packet.to_string()}));
    json!({
        "model": model,
        "max_tokens": max_tokens.max(1024),
        "system": system,
        "output_config": {"effort": "low",
            "format": {"type": "json_schema", "schema": crate::openai::schema(properties)}},
        "messages": [{"role": "user", "content": content}],
    })
}

pub(crate) fn structured_text(reply: &Value) -> Result<Value, ApiError> {
    if reply["stop_reason"] == "refusal" {
        return Err(ApiError::new(None, "the model declined to answer"));
    }
    let text: String = reply["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    serde_json::from_str(&text)
        .map_err(|e| ApiError::new(None, format!("the reply was not JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "observe", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "look", "arguments": "{\"x\":1}"}}],
                THINKING_KEY: [{"type": "thinking", "thinking": "hm", "signature": "sig"}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "ra"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "rb"}),
            json!({"role": "user", "content": [
                {"type": "text", "text": "pic"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,QUJD"}}]}),
        ]
    }

    #[test]
    fn request_shape() {
        let tools = [json!({"type": "function", "function": {"name": "observe",
            "description": "d", "parameters": {"type": "object", "properties": {}}}})];
        let r = build_request("claude-haiku-5-5", &history(), &tools);
        assert_eq!(r["max_tokens"], 16000);
        assert_eq!(r["output_config"]["effort"], "low");
        for k in ["temperature", "top_p", "top_k", "thinking"] {
            assert!(r.get(k).is_none(), "{k}");
        }
        assert_eq!(r["system"][0]["text"], "be brief");
        assert_eq!(r["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(r["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(r["tools"][0]["cache_control"]["type"], "ephemeral");
        assert!(r["tools"][0].get("function").is_none());
        assert_eq!(build_request("claude-haiku-5-5", &history(), &tools), r);
    }

    #[test]
    fn the_stable_prompt_is_cached_and_the_per_turn_note_follows_the_breakpoint() {
        let msgs = [
            json!({"role": "system", "content": "stable prompt"}),
            json!({"role": "system", "content": "It is Monday. Goal: x."}),
            json!({"role": "user", "content": "hi"}),
        ];
        let r = build_request("m", &msgs, &[]);
        assert_eq!(r["system"][0]["text"], "stable prompt");
        assert_eq!(r["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(r["system"][1]["text"], "It is Monday. Goal: x.");
        assert!(r["system"][1].get("cache_control").is_none());
        // A different note leaves the cached block byte for byte the same.
        let msgs2 = [msgs[0].clone(), json!({"role": "system", "content": "Tuesday"}), msgs[2].clone()];
        assert_eq!(build_request("m", &msgs2, &[])["system"][0], r["system"][0]);
    }

    #[test]
    fn usage_is_read_from_the_reply() {
        let r = parse_reply(&json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "x"}],
            "usage": {"input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 900,
                      "cache_creation_input_tokens": 30}}))
        .unwrap();
        assert_eq!((r.usage.input, r.usage.output, r.usage.cache_read, r.usage.cache_write), (12, 5, 900, 30));
        assert_eq!(r.usage.prompt(), 942);
    }

    #[test]
    fn parallel_results_share_one_user_message_and_thinking_comes_first() {
        let (_, m) = convert_messages(&history());
        assert_eq!(m.len(), 3);
        let a = &m[1]["content"];
        assert_eq!(a[0]["type"], "thinking");
        assert_eq!(a[0]["signature"], "sig");
        assert_eq!(a[1]["type"], "tool_use");
        assert_eq!(a[2]["input"]["x"], 1);
        let u = m[2]["content"].as_array().unwrap();
        assert_eq!(u.len(), 4);
        assert_eq!(u[0]["tool_use_id"], "a");
        assert_eq!(u[1]["tool_use_id"], "b");
        assert_eq!(u[3]["source"]["type"], "base64");
        assert_eq!(u[3]["source"]["media_type"], "image/jpeg");
        assert_eq!(u[3]["source"]["data"], "QUJD");
    }

    #[test]
    fn replies_parse_and_keep_thinking() {
        let r = parse_reply(&json!({"stop_reason": "tool_use", "content": [
            {"type": "thinking", "thinking": "t", "signature": "s"},
            {"type": "text", "text": "ok"},
            {"type": "tool_use", "id": "t1", "name": "observe", "input": {"a": 1}}]}))
        .unwrap();
        assert_eq!(r.content, "ok");
        assert_eq!(r.tool_calls[0].arguments, "{\"a\":1}");
        let m = r.message();
        assert_eq!(m[THINKING_KEY][0]["signature"], "s");
        let (_, msgs) = convert_messages(&[m]);
        assert_eq!(msgs[0]["content"][0]["thinking"], "t");
    }

    #[test]
    fn refusal_is_a_short_sentence_and_truncation_an_error() {
        let r = parse_reply(&json!({"stop_reason": "refusal", "content": []})).unwrap();
        assert_eq!(r.content, REFUSAL_SENTENCE);
        assert!(r.tool_calls.is_empty());
        assert!(parse_reply(&json!({"stop_reason": "max_tokens", "content": []})).is_err());
    }
}
