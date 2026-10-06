//! The line protocol, as `ui/src/protocol.md` describes it.
//!
//! One JSON object per line over stdin and stdout. A request carries an `id` and its reply carries
//! the same one; an event carries no id, because it is the core talking rather than answering.
//!
//! Newline-delimited JSON on a pipe rather than a local socket: nothing to firewall, nothing to
//! authenticate, and no port for a second copy of the app to collide with.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Reply {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    pub fn ok(id: u64, result: Value) -> Self {
        Self { id, ok: true, result: Some(result), error: None }
    }

    pub fn failed(id: u64, error: impl Into<String>) -> Self {
        Self { id, ok: false, result: None, error: Some(error.into()) }
    }
}

/// Something the core says unasked: a log line, a change of state, a hotkey.
pub fn event(name: &str, payload: Value) -> Value {
    let mut message = json!({ "event": name });
    if let Value::Object(fields) = payload {
        for (key, value) in fields {
            message[key] = value;
        }
    }
    message
}

/// Parse one line. A line that is not a request is not fatal: the shell can be mid-upgrade, and a
/// core that exits on a malformed line takes a run that is driving the machine with it.
pub fn parse(line: &str) -> Option<Request> {
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips() {
        let request = parse(r#"{"id":7,"method":"start","params":{"goal":"open youtube"}}"#).unwrap();
        assert_eq!(request.id, 7);
        assert_eq!(request.method, "start");
        assert_eq!(request.params["goal"], "open youtube");
    }

    #[test]
    fn params_are_optional_because_most_methods_take_none() {
        let request = parse(r#"{"id":1,"method":"state"}"#).unwrap();
        assert_eq!(request.method, "state");
        assert!(request.params.is_null());
    }

    #[test]
    fn a_malformed_line_is_ignored_rather_than_fatal() {
        // A core that exits on a bad line takes a run that is driving the machine with it.
        assert!(parse("not json at all").is_none());
        assert!(parse(r#"{"method":"no id"}"#).is_none());
    }

    #[test]
    fn a_reply_carries_the_id_it_answers() {
        let reply = Reply::ok(7, json!({ "queued": true }));
        let encoded = serde_json::to_value(&reply).unwrap();
        assert_eq!(encoded["id"], 7);
        assert_eq!(encoded["ok"], true);
        assert_eq!(encoded["result"]["queued"], true);
        assert!(encoded.get("error").is_none(), "a success carries no error field");
    }

    #[test]
    fn a_failure_says_why_and_carries_no_result() {
        let encoded = serde_json::to_value(Reply::failed(3, "no writer is configured")).unwrap();
        assert_eq!(encoded["ok"], false);
        assert_eq!(encoded["error"], "no writer is configured");
        assert!(encoded.get("result").is_none());
    }

    #[test]
    fn an_event_has_no_id_so_the_shell_knows_nobody_asked() {
        let message = event("line", json!({ "text": "step 1" }));
        assert_eq!(message["event"], "line");
        assert_eq!(message["text"], "step 1");
        assert!(message.get("id").is_none());
    }
}
