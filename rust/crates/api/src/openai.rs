//! OpenAI, or any OpenAI-compatible endpoint: the writer, the vision reader and transcription.
//! A port of `writer.py` (`_openai_structured`, `_openai_image_block`, `problem`).

use std::fmt;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

pub use crate::typesafe::ApiError;
use crate::typesafe::{env_var, error_message};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_TRANSCRIBE_MODEL: &str = "gpt-4o-mini-transcribe";
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct OpenAi {
    key: String,
    base_url: String,
    client: reqwest::blocking::Client,
}

impl fmt::Debug for OpenAi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAi")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OpenAi {
    /// A client from `OPENAI_API_KEY`, or None when the key is not set.
    pub fn from_env() -> Option<Self> {
        let key = env_var("OPENAI_API_KEY")?;
        let base_url = env_var("OPENAI_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into());
        Some(Self::new(key, base_url))
    }

    pub fn new(key: String, base_url: String) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("an HTTP client with only a timeout set always builds");
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            key,
            base_url,
            client,
        }
    }

    /// One structured reply: the parsed JSON object the schema describes.
    pub fn structured(
        &self,
        model: &str,
        system: &str,
        packet: &Value,
        properties: &Value,
        max_tokens: u32,
        png: Option<&[u8]>,
    ) -> Result<Value, ApiError> {
        let mut request = chat_request(model, system, packet, properties, png);
        request["max_completion_tokens"] = json!(max_tokens);
        let reply = match self.chat(&request) {
            Err(e) if wants_legacy_max_tokens(&e) => {
                let obj = request.as_object_mut().expect("the request is an object");
                obj.remove("max_completion_tokens");
                obj.insert("max_tokens".into(), json!(max_tokens));
                self.chat(&request)?
            }
            other => other?,
        };
        let content = reply["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| ApiError::new(None, "the reply had no message content"))?;
        serde_json::from_str(content)
            .map_err(|e| ApiError::new(None, format!("the reply was not JSON: {e}")))
    }

    fn chat(&self, request: &Value) -> Result<Value, ApiError> {
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.key)
            .json(request)
            .send()
            .map_err(ApiError::transport)?;
        read_json(response)
    }

    /// Speech to text. `prompt` biases the vocabulary (e.g. "Claude Code").
    pub fn transcribe(&self, wav: &[u8], prompt: Option<&str>) -> Result<String, ApiError> {
        let model =
            env_var("CLICKER_TRANSCRIBE_MODEL").unwrap_or_else(|| DEFAULT_TRANSCRIBE_MODEL.into());
        let file = reqwest::blocking::multipart::Part::bytes(wav.to_vec())
            .file_name("speech.wav")
            .mime_str("audio/wav")
            .map_err(ApiError::transport)?;
        let mut form = reqwest::blocking::multipart::Form::new()
            .text("model", model)
            .text("response_format", "json")
            .part("file", file);
        if let Some(p) = prompt.filter(|p| !p.is_empty()) {
            form = form.text("prompt", p.to_string());
        }
        let response = self
            .client
            .post(format!("{}/audio/transcriptions", self.base_url))
            .bearer_auth(&self.key)
            .multipart(form)
            .send()
            .map_err(ApiError::transport)?;
        let value = read_json(response)?;
        value["text"]
            .as_str()
            .map(|t| t.trim().to_string())
            .ok_or_else(|| ApiError::new(None, "the transcription had no text"))
    }
}

fn read_json(response: reqwest::blocking::Response) -> Result<Value, ApiError> {
    let status = response.status();
    let text = response.text().map_err(ApiError::transport)?;
    if !status.is_success() {
        return Err(ApiError::new(Some(status.as_u16()), error_message(&text)));
    }
    serde_json::from_str(&text).map_err(|e| ApiError::new(Some(status.as_u16()), e.to_string()))
}

/// The Python retries with `max_tokens` whenever the error mentions `max_completion_tokens`.
pub(crate) fn wants_legacy_max_tokens(e: &ApiError) -> bool {
    e.message.contains("max_completion_tokens")
}

/// `{type: object, properties, required: every key, additionalProperties: false}`.
pub(crate) fn schema(properties: &Value) -> Value {
    let required: Vec<Value> = properties
        .as_object()
        .map(|m| m.keys().map(|k| json!(k)).collect())
        .unwrap_or_default();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// The request without any token limit; the caller adds one under whichever name is accepted.
pub(crate) fn chat_request(
    model: &str,
    system: &str,
    packet: &Value,
    properties: &Value,
    png: Option<&[u8]>,
) -> Value {
    let mut content = Vec::new();
    if let Some(png) = png {
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        content.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}}));
    }
    content.push(json!({"type": "text", "text": packet.to_string()}));
    json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": content},
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "reply", "strict": true, "schema": schema(properties)},
        },
    })
}

/// A short reason a run log can carry; a port of `writer.problem`.
pub fn problem(message: &str) -> String {
    let lowered = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lowered.contains(n));
    if has(&[
        "credit balance is too low",
        "insufficient_quota",
        "exceeded your current quota",
    ]) {
        return "the account is out of credit".into();
    }
    if has(&["authentication", "invalid x-api-key", "incorrect api key"]) {
        return "the key was rejected".into();
    }
    if has(&["does not exist", "model_not_found"]) {
        return "that model is not available to this account; set CLICKER_WRITER_MODEL".into();
    }
    if has(&["rate limit", "429"]) {
        return "the account is rate limited".into();
    }
    message
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_shape_with_image() {
        let props = json!({"fill": {"type": "boolean"}, "text": {"type": "string"}});
        let r = chat_request(
            "gpt-x",
            "sys",
            &json!({"goal": "g"}),
            &props,
            Some(&[1, 2, 3]),
        );
        assert_eq!(r["model"], "gpt-x");
        assert_eq!(
            r["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        let content = &r["messages"][1]["content"];
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,AQID");
        assert_eq!(
            content[1],
            json!({"type": "text", "text": "{\"goal\":\"g\"}"})
        );
        let js = &r["response_format"]["json_schema"];
        assert_eq!(r["response_format"]["type"], "json_schema");
        assert_eq!(js["name"], "reply");
        assert_eq!(js["strict"], true);
        assert_eq!(js["schema"]["additionalProperties"], false);
        assert_eq!(js["schema"]["required"], json!(["fill", "text"]));
        assert_eq!(js["schema"]["properties"], props);
        assert!(r.get("max_tokens").is_none() && r.get("max_completion_tokens").is_none());
    }

    #[test]
    fn request_without_image_is_text_only() {
        let r = chat_request("m", "s", &json!({}), &json!({}), None);
        let content = r["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn falls_back_only_when_the_parameter_is_named() {
        let e = ApiError::new(
            Some(400),
            "Unsupported parameter: 'max_completion_tokens' is not supported with this model.",
        );
        assert!(wants_legacy_max_tokens(&e));
        assert!(!wants_legacy_max_tokens(&ApiError::new(
            Some(401),
            "Incorrect API key provided"
        )));
    }

    #[test]
    fn problem_maps_every_case() {
        let credit = "the account is out of credit";
        assert_eq!(problem("Your credit balance is too low to access"), credit);
        assert_eq!(problem("code: insufficient_quota"), credit);
        assert_eq!(problem("You exceeded your current quota, please"), credit);
        let key = "the key was rejected";
        assert_eq!(problem("authentication_error"), key);
        assert_eq!(problem("invalid x-api-key"), key);
        assert_eq!(problem("Incorrect API key provided: sk-..."), key);
        let model = "that model is not available to this account; set CLICKER_WRITER_MODEL";
        assert_eq!(problem("The model `gpt-9` does not exist"), model);
        assert_eq!(problem("model_not_found"), model);
        assert_eq!(problem("Rate limit reached"), "the account is rate limited");
        assert_eq!(problem("Error code: 429"), "the account is rate limited");
        assert_eq!(problem("Server exploded. Try later."), "Server exploded");
        assert_eq!(problem(&"x".repeat(300)).len(), 120);
    }

    fn synthesise_wav(text: &str) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!("api-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.wav");
        let script = format!(
            "Add-Type -AssemblyName System.Speech; $s=New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile('{}'); $s.Speak('{}'); $s.Dispose()",
            out.display(),
            text
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .status()
            .unwrap();
        assert!(status.success());
        let wav = std::fs::read(&out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        wav
    }

    #[test]
    #[ignore = "network: makes real OpenAI calls"]
    fn live_structured_and_transcribe() {
        crate::typesafe::load_dotenv();
        let ai = OpenAi::from_env().expect("OPENAI_API_KEY");
        let model = env_var("CLICKER_WRITER_MODEL").unwrap_or_else(|| "gpt-4o-mini".into());
        let props = json!({"fill": {"type": "boolean"}, "text": {"type": "string"}});
        let t = std::time::Instant::now();
        let got = ai.structured(
            &model,
            "You fill in one text field. Decide the exact string to type.",
            &json!({"goal": "search for rust tutorials", "focused_field": "Search"}),
            &props,
            200,
            None,
        );
        println!(
            "LIVE structured ({model}) {:?} in {} ms",
            got.as_ref().map_err(|e| problem(&e.message)),
            t.elapsed().as_millis()
        );
        assert!(got.is_ok());

        let wav = synthesise_wav("open Claude Code");
        for prompt in [None, Some("Claude Code")] {
            let t = std::time::Instant::now();
            let text = ai.transcribe(&wav, prompt);
            println!(
                "LIVE transcribe prompt={prompt:?} {:?} in {} ms",
                text.as_ref().map_err(|e| problem(&e.message)),
                t.elapsed().as_millis()
            );
            assert!(text.is_ok());
        }
    }
}
