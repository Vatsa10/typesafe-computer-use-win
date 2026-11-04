//! Murf text-to-speech over HTTP streaming (`POST /v1/speech/stream`, Falcon). The response is
//! raw little-endian 16-bit mono PCM at the requested sample rate (no WAV header with
//! `format: PCM`), sent as it is synthesised: the first bytes arrive in about a quarter second,
//! so playback can start before the sentence is finished.
//!
//! The key goes in the `api-key` header and is never in an error line. The spoken text is sent to
//! Murf and goes nowhere else.

use std::fmt;
use std::io::Read;
use std::time::Duration;

use serde_json::json;

pub const DEFAULT_URL: &str = "https://global.api.murf.ai/v1/speech/stream";
/// A natural conversational US English voice that Falcon serves.
pub const DEFAULT_VOICE: &str = "en-US-natalie";
/// Falcon's output rate when asked for 24 kHz PCM.
pub const SAMPLE_RATE: u32 = 24_000;

/// One client for the whole process, so connections are pooled and kept alive between lines.
pub fn shared_client() -> reqwest::blocking::Client {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(90))
                .pool_idle_timeout(Duration::from_secs(300))
                .tcp_keepalive(Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new())
        })
        .clone()
}

pub struct Murf {
    key: String,
    url: String,
    client: reqwest::blocking::Client,
}

impl fmt::Debug for Murf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Murf").field("url", &self.url).finish_non_exhaustive()
    }
}

/// Bytes to samples across chunk boundaries: a chunk may end in the middle of a sample.
#[derive(Debug, Default)]
pub struct PcmChunker {
    carry: Option<u8>,
}

impl PcmChunker {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<i16> {
        let mut out = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut rest = bytes;
        if let Some(low) = self.carry.take() {
            match rest.split_first() {
                Some((&high, tail)) => {
                    out.push(i16::from_le_bytes([low, high]));
                    rest = tail;
                }
                None => {
                    self.carry = Some(low);
                    return out;
                }
            }
        }
        let pairs = rest.chunks_exact(2);
        let odd = pairs.remainder();
        out.extend(pairs.map(|p| i16::from_le_bytes([p[0], p[1]])));
        self.carry = odd.first().copied();
        out
    }
}

/// The voice to use: `POINTER_VOICE`, else the default.
pub fn voice_from_env() -> String {
    std::env::var("POINTER_VOICE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_VOICE.into())
}

impl Murf {
    pub fn from_env() -> Option<Murf> {
        let key = std::env::var("MURF_API_KEY").ok()?.trim().to_string();
        if key.is_empty() {
            return None;
        }
        let url = std::env::var("MURF_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_URL.into());
        Some(Murf::new(key, url))
    }

    pub fn new(key: String, url: String) -> Murf {
        Murf { key, url, client: shared_client() }
    }

    /// Open (or refresh) the pooled TLS connection to the service without synthesising anything,
    /// so the next line skips the handshake. Errors are ignored: this is only a head start.
    pub fn warm(&self) {
        let host = self.url.split("/v1/").next().unwrap_or(&self.url).to_string();
        let _ = self.client.head(host).timeout(Duration::from_secs(5)).send();
    }

    /// Synthesise `text` and hand each run of samples to `on_chunk` as it arrives. `on_chunk`
    /// returns false to stop (the reply body is dropped, which closes the connection).
    pub fn stream(
        &self,
        text: &str,
        voice: &str,
        mut on_chunk: impl FnMut(&[i16]) -> bool,
    ) -> Result<(), String> {
        let body = json!({
            "text": text,
            "voiceId": voice,
            "format": "PCM",
            "sampleRate": SAMPLE_RATE,
            "channelType": "MONO",
        });
        let mut response = self
            .client
            .post(&self.url)
            .header("api-key", &self.key)
            .json(&body)
            .send()
            .map_err(|e| format!("murf unreachable ({})", if e.is_timeout() { "timeout" } else { "network" }))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("murf refused the request (HTTP {})", status.as_u16()));
        }
        let mut chunker = PcmChunker::default();
        let mut buf = vec![0u8; 8192];
        loop {
            let n = response
                .read(&mut buf)
                .map_err(|_| "murf stream broke off".to_string())?;
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
    fn chunks_that_split_a_sample_still_decode() {
        let mut c = PcmChunker::default();
        let all: Vec<u8> = [1i16, -2, 300, -400, 5].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut got = Vec::new();
        for part in [&all[..3], &all[3..4], &all[4..9], &all[9..]] {
            got.extend(c.push(part));
        }
        assert_eq!(got, vec![1, -2, 300, -400, 5]);
    }

    #[test]
    fn the_default_voice_is_used_without_a_setting() {
        std::env::remove_var("POINTER_VOICE");
        assert_eq!(voice_from_env(), DEFAULT_VOICE);
    }

    fn serve_once(status: &'static str, body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut seen = [0u8; 4096];
            let _ = s.read(&mut seen);
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(&body).unwrap();
        });
        format!("http://127.0.0.1:{port}/v1/speech/stream")
    }

    #[test]
    fn a_stream_decodes_and_can_be_cut_off() {
        let pcm: Vec<u8> = (0..1000i16).flat_map(|s| s.to_le_bytes()).collect();
        let m = Murf::new("k".into(), serve_once("200 OK", pcm));
        let mut total = 0;
        m.stream("hi", DEFAULT_VOICE, |c| {
            total += c.len();
            true
        })
        .unwrap();
        assert_eq!(total, 1000);
        let pcm: Vec<u8> = vec![0; 40_000];
        let m = Murf::new("k".into(), serve_once("200 OK", pcm));
        let mut calls = 0;
        m.stream("hi", DEFAULT_VOICE, |_| {
            calls += 1;
            false
        })
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn an_error_status_is_a_line_without_the_key() {
        let m = Murf::new("secret-key-9".into(), serve_once("402 Payment Required", b"no".to_vec()));
        let err = m.stream("hi", DEFAULT_VOICE, |_| true).unwrap_err();
        assert!(err.contains("402") && !err.contains("secret-key-9"));
    }
}
