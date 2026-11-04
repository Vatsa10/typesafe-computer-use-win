//! Rumik (Silk) text-to-speech over HTTP: `POST https://silk-api.rumik.ai/v1/tts` with a bearer
//! key, `audio_format: "pcm"` for raw little-endian 16-bit mono at 24 kHz. Models are `muga`
//! (tone tags) and `mulberry` (named `speaker`). Same shape as `murf`: samples reach the caller
//! as they arrive. The key is never in an error line; spoken text goes to Rumik and nowhere else.

use std::fmt;
use std::io::Read;

use serde_json::json;

use crate::murf::{shared_client, PcmChunker};

pub const DEFAULT_URL: &str = "https://silk-api.rumik.ai/v1/tts";
pub const DEFAULT_MODEL: &str = "mulberry";
pub const DEFAULT_SPEAKER: &str = "emma";
pub const SAMPLE_RATE: u32 = 24_000;

pub struct Rumik {
    key: String,
    url: String,
    client: reqwest::blocking::Client,
}

impl fmt::Debug for Rumik {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rumik").field("url", &self.url).finish_non_exhaustive()
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The speaker: `POINTER_VOICE` only when it is not a Murf id (those contain a locale like
/// `en-US-`), else `RUMIK_SPEAKER`, else the default.
pub fn speaker_from_env() -> String {
    env("RUMIK_SPEAKER")
        .or_else(|| env("POINTER_VOICE").filter(|v| !v.contains("-")))
        .unwrap_or_else(|| DEFAULT_SPEAKER.into())
}

pub fn model_from_env() -> String {
    env("RUMIK_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into())
}

impl Rumik {
    pub fn from_env() -> Option<Rumik> {
        let key = env("RUMIK_API_KEY")?;
        Some(Rumik::new(key, env("RUMIK_BASE_URL").unwrap_or_else(|| DEFAULT_URL.into())))
    }

    pub fn new(key: String, url: String) -> Rumik {
        Rumik { key, url, client: shared_client() }
    }

    pub fn warm(&self) {
        let host = self.url.split("/v1/").next().unwrap_or(&self.url).to_string();
        let _ = self.client.head(host).timeout(std::time::Duration::from_secs(5)).send();
    }

    /// Synthesise `text`; `on_chunk` returns false to stop.
    pub fn stream(&self, text: &str, mut on_chunk: impl FnMut(&[i16]) -> bool) -> Result<(), String> {
        let model = model_from_env();
        let mut body = json!({ "model": model, "text": text, "audio_format": "pcm" });
        if model == "mulberry" {
            body["speaker"] = json!(speaker_from_env());
        }
        let mut response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .map_err(|e| format!("rumik unreachable ({})", if e.is_timeout() { "timeout" } else { "network" }))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("rumik refused the request (HTTP {})", status.as_u16()));
        }
        let mut chunker = PcmChunker::default();
        let mut buf = vec![0u8; 8192];
        loop {
            let n = response.read(&mut buf).map_err(|_| "rumik stream broke off".to_string())?;
            if n == 0 {
                return Ok(());
            }
            let samples = chunker.push(&buf[..n]);
            if !samples.is_empty() && !on_chunk(&samples) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_stream_decodes_sends_a_bearer_and_hides_the_key_in_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for status in ["200 OK", "401 Unauthorized"] {
                let (mut s, _) = listener.accept().unwrap();
                let mut seen = [0u8; 4096];
                let n = s.read(&mut seen).unwrap();
                let _ = tx.send(String::from_utf8_lossy(&seen[..n]).to_string());
                let body: Vec<u8> = (0..500i16).flat_map(|v| v.to_le_bytes()).collect();
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                s.write_all(head.as_bytes()).unwrap();
                s.write_all(&body).unwrap();
            }
        });
        let r = Rumik::new("rk-secret-1".into(), format!("http://127.0.0.1:{port}/v1/tts"));
        let mut total = 0;
        r.stream("hi", |c| {
            total += c.len();
            true
        })
        .unwrap();
        assert_eq!(total, 500);
        let request = rx.recv().unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer rk-secret-1") && request.contains("\"pcm\""));
        let err = r.stream("hi", |_| true).unwrap_err();
        assert!(err.contains("401") && !err.contains("rk-secret-1"));
    }
}
