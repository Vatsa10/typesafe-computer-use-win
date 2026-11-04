//! Saved conversations, under `POINTER_HOME\conversations`.
//!
//! A conversation is one JSON file holding the messages after the system prompt. No API key is
//! ever written: keys never enter the messages, and as a second line of defence every known secret
//! is scrubbed from the text before it is saved. Camera images are never saved.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub struct Conversation {
    pub id: String,
    pub messages: Vec<Value>,
}

fn new_id(dir: &Path) -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut n = 0;
    loop {
        let id = if n == 0 {
            format!("chat-{stamp}")
        } else {
            format!("chat-{stamp}-{n}")
        };
        if !dir.join(format!("{id}.json")).exists() {
            return id;
        }
        n += 1;
    }
}

impl Conversation {
    /// An empty conversation with a fresh id. Nothing is written until a message is saved.
    pub fn new(dir: &Path) -> Self {
        Self {
            id: new_id(dir),
            messages: Vec::new(),
        }
    }

    /// The most recent saved conversation (not a task's), so a restart continues the talk.
    pub fn load_latest(dir: &Path) -> Option<Self> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("chat-") && n.ends_with(".json"))
            .collect();
        names.sort();
        let name = names.pop()?;
        let id = name.trim_end_matches(".json").to_string();
        let messages = load(dir, &id)?;
        Some(Self { id, messages })
    }
}

/// The messages of one saved conversation, repaired so every tool call has its result.
pub fn load(dir: &Path, id: &str) -> Option<Vec<Value>> {
    let text = std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut messages = v.get("messages")?.as_array()?.clone();
    repair(&mut messages);
    Some(messages)
}

/// Cut the history at the first assistant message whose tool calls were not all answered: such a
/// history is refused by the API.
pub fn repair(messages: &mut Vec<Value>) {
    let mut i = 0;
    while i < messages.len() {
        let calls: Vec<String> = messages[i]
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !calls.is_empty() {
            let answered = calls.iter().all(|id| {
                messages[i + 1..].iter().take(calls.len()).any(|m| {
                    m.get("role").and_then(Value::as_str) == Some("tool")
                        && m.get("tool_call_id").and_then(Value::as_str) == Some(id.as_str())
                })
            });
            if !answered {
                messages.truncate(i);
                return;
            }
        }
        i += 1;
    }
}

/// Every secret in the environment that must never reach a file.
pub fn env_secrets() -> Vec<String> {
    [
        "POINTER_AGENT_API_KEY",
        "OPENAI_API_KEY",
        "TYPESAFE_API_KEY",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok())
    .map(|v| v.trim().to_string())
    .filter(|v| v.len() >= 8)
    .collect()
}

/// `text` with every secret replaced.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s.as_str(), "[key removed]");
        }
    }
    out
}

/// Replace image parts with a note: images live in memory and the model request only.
pub fn strip_images(messages: &mut [Value]) {
    for m in messages {
        if let Some(parts) = m.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("image_url") {
                    *part = json!({ "type": "text", "text": "[camera image not kept]" });
                }
            }
        }
    }
}

/// Write a conversation, atomically, with secrets scrubbed and images dropped. Best effort: a
/// full disk must not end the turn.
/// Tools whose results hold what was on the screen or in front of the camera.
const NOT_SAVED: [&str; 2] = ["observe", "camera_look"];

/// A screen read holds window titles, chats and documents, and goes stale at once, so only the
/// conversation is kept on disk. The live conversation keeps it until the turn is compacted.
fn drop_screen_reads(messages: &mut [Value]) {
    let ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .filter(|c| {
            let name = c.pointer("/function/name").and_then(Value::as_str);
            name.is_some_and(|n| NOT_SAVED.contains(&n))
        })
        .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    for m in messages.iter_mut() {
        let id = m.get("tool_call_id").and_then(Value::as_str);
        if id.is_some_and(|id| ids.iter().any(|x| x == id)) {
            m["content"] = json!("[screen read, not saved]");
        }
    }
}

pub fn save(dir: &Path, id: &str, messages: &[Value]) {
    let mut kept = messages.to_vec();
    strip_images(&mut kept);
    drop_screen_reads(&mut kept);
    let body = json!({ "id": id, "messages": kept }).to_string();
    let body = scrub(&body, &env_secrets());
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!("{id}.json.tmp"));
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn plain_text(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Search the saved conversations for `query` (every word must appear in a message). At most
/// `limit` hits, newest conversation first.
pub fn search(dir: &Path, query: &str, limit: usize) -> Vec<Value> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files.reverse();
    let mut hits = Vec::new();
    for path in files {
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(messages) = load(dir, &id) else {
            continue;
        };
        for m in &messages {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            if role != "user" && role != "assistant" {
                continue;
            }
            let text = plain_text(m);
            let lower = text.to_lowercase();
            if words.iter().all(|w| lower.contains(w.as_str())) {
                let snippet: String = text.chars().take(300).collect();
                hits.push(json!({ "conversation": id, "role": role, "text": snippet }));
                if hits.len() >= limit {
                    return hits;
                }
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("assist-store-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_load_and_latest_round_trip() {
        let dir = tmp("rt");
        let conv = Conversation::new(&dir);
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        save(&dir, &conv.id, &msgs);
        assert_eq!(load(&dir, &conv.id).unwrap(), msgs);
        let latest = Conversation::load_latest(&dir).unwrap();
        assert_eq!(latest.id, conv.id);
        assert!(Conversation::new(&dir).id != conv.id, "ids are unique");
    }

    #[test]
    fn secrets_and_images_never_reach_the_file() {
        let dir = tmp("secrets");
        let msgs = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "my key is sk-live-0123456789"},
            {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,AAAA"}},
        ]})];
        // `save` scrubs the environment's secrets; `scrub` is the pure part under test.
        assert_eq!(
            scrub(
                "a sk-live-0123456789 b",
                &["sk-live-0123456789".to_string()]
            ),
            "a [key removed] b"
        );
        save(&dir, "chat-x", &msgs);
        let text = std::fs::read_to_string(dir.join("chat-x.json")).unwrap();
        assert!(!text.contains("AAAA") && text.contains("camera image not kept"));
        let mut live = msgs.clone();
        strip_images(&mut live);
        assert!(!live[0].to_string().contains("base64"));
    }

    #[test]
    fn repair_drops_a_dangling_tool_call() {
        let mut m = vec![
            json!({"role": "user", "content": "x"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "observe", "arguments": "{}"}}]}),
        ];
        repair(&mut m);
        assert_eq!(m.len(), 1);
        let mut ok = vec![
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "observe", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "ok"}),
        ];
        repair(&mut ok);
        assert_eq!(ok.len(), 2);
    }

    #[test]
    fn search_finds_words_in_saved_talk() {
        let dir = tmp("search");
        save(
            &dir,
            "chat-1",
            &[
                json!({"role": "user", "content": "remind me about the Budget meeting"}),
                json!({"role": "tool", "tool_call_id": "t", "content": "budget in a tool result"}),
            ],
        );
        let hits = search(&dir, "budget MEETING", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["conversation"], "chat-1");
        assert!(search(&dir, "", 5).is_empty());
        assert!(search(&dir, "nothing here", 5).is_empty());
    }
}

#[cfg(test)]
mod screen_read_tests {
    use super::*;

    #[test]
    fn screen_reads_are_not_written_and_the_talk_is() {
        let dir = std::env::temp_dir().join(format!("pointer-store-{}", std::process::id()));
        let call = |id: &str, name: &str| {
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "id": id, "type": "function", "function": { "name": name, "arguments": "{}" } }
            ] })
        };
        let result =
            |id: &str, text: &str| json!({ "role": "tool", "tool_call_id": id, "content": text });
        let messages = vec![
            json!({ "role": "user", "content": "what is this" }),
            call("a", "observe"),
            result("a", "private chat title"),
            call("b", "interact"),
            result("b", "ok: spoken"),
        ];
        save(&dir, "chat-test", &messages);
        let text = std::fs::read_to_string(dir.join("chat-test.json")).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(!text.contains("private chat title"));
        assert!(text.contains("what is this") && text.contains("ok: spoken"));
        // Every call still has a result, so the saved history stays valid for the API.
        let mut back = serde_json::from_str::<Value>(&text).unwrap()["messages"]
            .as_array()
            .unwrap()
            .clone();
        repair(&mut back);
        assert_eq!(back.len(), 5);
    }
}
