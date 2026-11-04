//! Deepgram streaming speech-to-text over a websocket (`wss://api.deepgram.com/v1/listen`).
//!
//! The model is Nova-3 (the current general streaming model with key-term prompting). Audio goes
//! up as raw 16 kHz mono 16-bit PCM; results come back as `Results` messages with interim and
//! final transcripts. Deepgram's endpointing says when an utterance ended (`speech_final`), and
//! `UtteranceEnd` is the backstop when the audio carried noise that kept the endpointer waiting.
//!
//! The socket is driven from one thread: the caller sends audio chunks and polls for results
//! between them, with a short read timeout so a poll never blocks the microphone. The API key is
//! only ever in the handshake header; nothing here logs a URL, a header or a transcript.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::Message;
use tungstenite::{Error as WsError, WebSocket};

pub const DEFAULT_URL: &str = "wss://api.deepgram.com/v1/listen";
pub const MODEL: &str = "nova-3";
/// Words Deepgram should favour. Short list on purpose: the docs say 20-50 terms at most.
pub const KEY_TERMS: [&str; 4] = ["Claude", "Claude Code", "Pointer", "TypeSafe"];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// How long one `poll` waits for a message before giving the microphone its turn back.
const POLL_WAIT: Duration = Duration::from_millis(2);

/// What to ask Deepgram for.
#[derive(Debug, Clone)]
pub struct Params {
    pub base_url: String,
    /// A Deepgram language code, or `multi` for code-switching (Hindi and English mixed).
    pub language: String,
    pub sample_rate: u32,
    /// Quiet, in milliseconds, after which Deepgram ends an utterance.
    pub endpointing_ms: u32,
    /// Backstop: milliseconds of no words after which an `UtteranceEnd` is sent.
    pub utterance_end_ms: u32,
    pub keyterms: Vec<String>,
}

impl Params {
    pub fn new(language: &str) -> Params {
        Params {
            base_url: DEFAULT_URL.into(),
            language: language.into(),
            sample_rate: 16_000,
            endpointing_ms: 400,
            utterance_end_ms: 1000,
            keyterms: KEY_TERMS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The full URL, query string included. Contains no secret.
    pub fn url(&self) -> String {
        let mut url = format!(
            "{}?model={MODEL}&language={}&encoding=linear16&sample_rate={}&channels=1\
             &interim_results=true&endpointing={}&utterance_end_ms={}&smart_format=true&punctuate=true",
            self.base_url,
            encode(&self.language),
            self.sample_rate,
            self.endpointing_ms,
            self.utterance_end_ms
        );
        for term in &self.keyterms {
            url.push_str("&keyterm=");
            url.push_str(&encode(term));
        }
        url
    }
}

/// Percent-encode a query value (spaces as %20, which Deepgram documents for key terms).
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The Deepgram language for `CLICKER_STT_LANG`. Unset means English. `auto` and `multi` mean
/// code-switching. `en-US` style English codes pass through; other languages lose their region.
pub fn language_for(setting: &str) -> String {
    let s = setting.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => "en".into(),
        "auto" | "multi" | "multilingual" | "hinglish" => "multi".into(),
        _ => {
            if s.starts_with("en-") || !s.contains('-') {
                s
            } else {
                s.split('-').next().unwrap_or("en").to_string()
            }
        }
    }
}

/// One message from the server, reduced to what matters.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Transcript {
        text: String,
        is_final: bool,
        speech_final: bool,
        from_finalize: bool,
    },
    UtteranceEnd,
    SpeechStarted,
    Error(String),
    Other,
}

/// Parse one text frame. Garbage is `Other`, never a panic.
pub fn parse_message(raw: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Event::Other;
    };
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "Results" => {
            let text = v
                .pointer("/channel/alternatives/0/transcript")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
            Event::Transcript {
                text,
                is_final: flag("is_final"),
                speech_final: flag("speech_final"),
                from_finalize: flag("from_finalize"),
            }
        }
        "UtteranceEnd" => Event::UtteranceEnd,
        "SpeechStarted" => Event::SpeechStarted,
        "Error" | "error" => Event::Error(
            v.get("description")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("deepgram error")
                .to_string(),
        ),
        _ => Event::Other,
    }
}

/// What the caller sees: live words, or a finished utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// Everything said in the current utterance so far, final pieces plus the live tail.
    Interim(String),
    /// The utterance ended; this is all of it.
    Final(String),
}

/// Joins Deepgram's final pieces into utterances. A piece is final when Deepgram will not revise
/// it; the utterance is over at `speech_final`, at a `Finalize` flush, or at `UtteranceEnd`.
#[derive(Debug, Default)]
pub struct Utterance {
    done: Vec<String>,
    /// Words heard but not yet final (the live tail).
    tail: String,
}

impl Utterance {
    fn joined(&self, tail: &str) -> String {
        let mut parts: Vec<&str> = self.done.iter().map(String::as_str).collect();
        if !tail.is_empty() {
            parts.push(tail);
        }
        parts.join(" ")
    }

    /// End the utterance now, if anything was said.
    pub fn flush(&mut self) -> Option<Heard> {
        if self.done.is_empty() {
            return None;
        }
        let text = self.joined("");
        self.done.clear();
        Some(Heard::Final(text))
    }

    /// End the utterance now if every word heard is already final: there is nothing left for
    /// Deepgram to settle, so waiting for its reply to `Finalize` would only add delay.
    pub fn flush_if_settled(&mut self) -> Option<Heard> {
        if self.tail.is_empty() {
            self.flush()
        } else {
            None
        }
    }

    /// Anything said and not yet finished.
    pub fn pending(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn feed(&mut self, event: Event) -> Option<Heard> {
        match event {
            Event::Transcript {
                text,
                is_final: false,
                ..
            } => {
                self.tail = text.clone();
                let all = self.joined(&text);
                (!all.is_empty()).then_some(Heard::Interim(all))
            }
            Event::Transcript {
                text,
                is_final: true,
                speech_final,
                from_finalize,
            } => {
                self.tail.clear();
                if !text.is_empty() {
                    self.done.push(text);
                }
                if speech_final || from_finalize {
                    self.flush()
                } else if self.done.is_empty() {
                    None
                } else {
                    Some(Heard::Interim(self.joined("")))
                }
            }
            Event::UtteranceEnd => self.flush(),
            _ => None,
        }
    }
}

enum Sock {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Sock {
    fn tcp(&self) -> &TcpStream {
        match self {
            Sock::Plain(s) => s,
            Sock::Tls(s) => &s.sock,
        }
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Plain(s) => s.read(buf),
            Sock::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Plain(s) => s.write(buf),
            Sock::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sock::Plain(s) => s.flush(),
            Sock::Tls(s) => s.flush(),
        }
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// A live connection. Dropping it closes the socket.
pub struct Stream {
    ws: WebSocket<Sock>,
    utterance: Utterance,
    closed: bool,
    last_sent: Instant,
}

/// `POINTER_VOICE_TRACE=1` prints the shape and timing of each result to stderr.
fn trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("POINTER_VOICE_TRACE").is_ok_and(|v| v == "1"))
}

fn io_wait(e: &WsError) -> bool {
    matches!(e, WsError::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

impl Stream {
    /// Connect and finish the websocket handshake. Errors are short lines with no key in them.
    pub fn connect(key: &str, params: &Params) -> Result<Stream, String> {
        let url = params.url();
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad deepgram url: {e}"))?;
        let value = format!("Token {key}")
            .parse()
            .map_err(|_| "the Deepgram key has characters a header cannot carry".to_string())?;
        request.headers_mut().insert("Authorization", value);
        let uri = request.uri().clone();
        let host = uri.host().ok_or("deepgram url has no host")?.to_string();
        let secure = uri.scheme_str() == Some("wss");
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });

        let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))
            .map_err(|e| format!("cannot resolve {host}: {e}"))?;
        let mut tcp = None;
        let mut last = String::from("no address");
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
        }
        let tcp = tcp.ok_or_else(|| format!("cannot reach deepgram: {last}"))?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(CONNECT_TIMEOUT)).ok();
        let sock = if secure {
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| format!("bad host name: {e}"))?;
            let conn = rustls::ClientConnection::new(tls_config(), name)
                .map_err(|e| format!("tls setup failed: {e}"))?;
            Sock::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
        } else {
            Sock::Plain(tcp)
        };
        let (ws, _response) = tungstenite::client(request, sock).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(WsError::Http(r)) => {
                format!("deepgram refused the connection (HTTP {})", r.status().as_u16())
            }
            tungstenite::HandshakeError::Failure(e) => format!("deepgram handshake failed: {e}"),
            tungstenite::HandshakeError::Interrupted(_) => "deepgram handshake timed out".into(),
        })?;
        ws.get_ref().tcp().set_read_timeout(Some(POLL_WAIT)).ok();
        Ok(Stream {
            ws,
            utterance: Utterance::default(),
            closed: false,
            last_sent: Instant::now(),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Send one chunk of 16 kHz mono samples.
    pub fn send_audio(&mut self, samples: &[i16]) -> Result<(), String> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.last_sent = Instant::now();
        self.write(Message::Binary(bytes))
    }

    fn write(&mut self, message: Message) -> Result<(), String> {
        match self.ws.send(message) {
            Ok(()) => Ok(()),
            Err(e) if io_wait(&e) => Ok(()), // queued; the next call flushes it
            Err(e) => {
                self.closed = true;
                Err(format!("the deepgram socket dropped ({})", brief(&e)))
            }
        }
    }

    fn control(&mut self, kind: &str) -> Result<(), String> {
        self.write(Message::Text(json!({ "type": kind }).to_string()))
    }

    /// Ask the server to finish whatever it has now (the local silence gate's fallback).
    pub fn finalize(&mut self) -> Result<(), String> {
        self.control("Finalize")
    }

    /// As `finalize`, and if every word is already final, the finished utterance right away.
    pub fn finalize_now(&mut self) -> Result<Option<Heard>, String> {
        self.control("Finalize")?;
        Ok(self.utterance.flush_if_settled())
    }

    /// Read whatever has arrived, without waiting more than a couple of milliseconds.
    pub fn poll(&mut self) -> Result<Vec<Heard>, String> {
        let mut out = Vec::new();
        loop {
            match self.ws.read() {
                Ok(Message::Text(raw)) => {
                    let event = parse_message(&raw);
                    if trace() {
                        // Shapes and timing only, never the words.
                        match &event {
                            Event::Transcript { text, is_final, speech_final, from_finalize } => eprintln!(
                                "[deepgram] results chars={} final={is_final} speech_final={speech_final} from_finalize={from_finalize}",
                                text.chars().count()
                            ),
                            other => eprintln!("[deepgram] {:?}", std::mem::discriminant(other)),
                        }
                    }
                    if let Event::Error(e) = event {
                        self.closed = true;
                        return Err(format!("deepgram: {e}"));
                    }
                    out.extend(self.utterance.feed(event));
                }
                Ok(Message::Close(_)) => {
                    self.closed = true;
                    out.extend(self.utterance.flush());
                    return Ok(out);
                }
                Ok(_) => {}
                Err(e) if io_wait(&e) => return Ok(out),
                Err(e) => {
                    self.closed = true;
                    return if out.is_empty() {
                        Err(format!("the deepgram socket dropped ({})", brief(&e)))
                    } else {
                        Ok(out)
                    };
                }
            }
        }
    }

    /// True when something was said and the utterance is still open.
    pub fn pending(&self) -> bool {
        self.utterance.pending()
    }

    /// Keep an idle socket open (Deepgram closes one that hears nothing for about ten seconds).
    pub fn keep_alive(&mut self) -> Result<(), String> {
        if self.last_sent.elapsed() < Duration::from_secs(4) {
            return Ok(());
        }
        self.last_sent = Instant::now();
        self.control("KeepAlive")
    }

    /// End the stream: flush, close, and collect what the server still had. Whatever was said and
    /// not yet finished comes back as a final utterance.
    pub fn finish(&mut self, wait: Duration) -> Vec<Heard> {
        let mut out = Vec::new();
        if !self.closed {
            let _ = self.control("Finalize");
            let _ = self.control("CloseStream");
            let _ = self.ws.flush();
            let deadline = Instant::now() + wait;
            while !self.closed && Instant::now() < deadline {
                match self.poll() {
                    Ok(more) => out.extend(more),
                    Err(_) => break,
                }
            }
        }
        out.extend(self.utterance.flush());
        let _ = self.ws.close(None);
        self.closed = true;
        out
    }
}

fn brief(e: &WsError) -> &'static str {
    match e {
        WsError::ConnectionClosed | WsError::AlreadyClosed => "closed",
        WsError::Io(_) => "network",
        WsError::Protocol(_) => "protocol",
        _ => "error",
    }
}

/// Silence that ends a finished-sounding utterance.
pub const END_MS: u64 = 850;
/// Silence that ends one whose words stop mid-phrase ("open the", "and", a dangling preposition).
pub const END_MID_PHRASE_MS: u64 = 1600;

/// Words after which a sentence is rarely over.
const DANGLING: [&str; 38] = [
    "the", "a", "an", "and", "or", "but", "so", "then", "to", "of", "in", "on", "at", "for", "with",
    "from", "by", "into", "onto", "about", "my", "your", "that", "if", "because", "also", "like",
    "is", "are", "was", "up", "over", "than", "as", "it's", "what", "which", "open",
];

/// Do these words stop in the middle of a phrase?
pub fn ends_mid_phrase(text: &str) -> bool {
    let t = text.trim_end();
    if t.ends_with(',') || t.ends_with('-') || t.ends_with("...") {
        return true;
    }
    let last = t
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
        .to_ascii_lowercase();
    !last.is_empty() && !t.ends_with(['.', '?', '!']) && DANGLING.contains(&last.as_str())
}

/// Decides when to send `Finalize`, from two clocks: the local gate's quiet and how long
/// Deepgram's own text has stood still. Either one can end the utterance, so a room whose noise
/// keeps the gate "loud" (where Deepgram never says `speech_final`) still closes on the words.
#[derive(Debug, Default)]
pub struct Endpointer {
    text: String,
    changed_ms: u64,
    quiet_since_ms: Option<u64>,
    asked: bool,
}

impl Endpointer {
    /// Words (interim or final-so-far) for the open utterance, at time `now_ms`.
    pub fn words(&mut self, text: &str, now_ms: u64) {
        if text != self.text {
            self.text = text.to_string();
            self.changed_ms = now_ms;
            self.asked = false;
        }
    }

    /// The utterance closed (a Final arrived).
    pub fn closed(&mut self) {
        self.text.clear();
        self.asked = false;
    }

    /// Feed the gate each block. True exactly once per utterance, when Finalize should go.
    pub fn decide(&mut self, gate_quiet: bool, now_ms: u64, pending: bool) -> bool {
        if gate_quiet {
            self.quiet_since_ms.get_or_insert(now_ms);
        } else {
            self.quiet_since_ms = None;
        }
        if self.asked || (self.text.is_empty() && !pending) {
            return false;
        }
        let need = if ends_mid_phrase(&self.text) { END_MID_PHRASE_MS } else { END_MS };
        // The gate flags quiet after its own short window, so its clock starts earlier than ours.
        let gate_for = self.quiet_since_ms.map_or(0, |q| now_ms.saturating_sub(q) + GATE_WINDOW_MS);
        let text_for = now_ms.saturating_sub(self.changed_ms);
        // Words lag the audio a little, so the text clock must hold a bit longer than the gate.
        if gate_for >= need || text_for >= need + 250 {
            self.asked = true;
            return true;
        }
        false
    }
}

/// The quiet the capture gate has already seen when it first reports quiet.
pub const GATE_WINDOW_MS: u64 = 450;

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn dangling_words_wait_longer() {
        assert!(ends_mid_phrase("open the"));
        assert!(ends_mid_phrase("play something and"));
        assert!(ends_mid_phrase("so,"));
        assert!(!ends_mid_phrase("open the calculator"));
        assert!(!ends_mid_phrase("what time is it?"));
    }

    #[test]
    fn a_finished_phrase_ends_at_850_and_a_dangling_one_at_1600() {
        let mut e = Endpointer::default();
        e.words("open calculator", 0);
        let mut fired = None;
        for t in (0..3000).step_by(50) {
            if e.decide(t >= 100, t, true) && fired.is_none() {
                fired = Some(t);
            }
        }
        
        assert!((450..=550).contains(&fired.unwrap()), "{fired:?}");
        let mut e = Endpointer::default();
        e.words("open the", 0);
        let mut fired = None;
        for t in (0..3000).step_by(50) {
            if e.decide(t >= 100, t, true) && fired.is_none() {
                fired = Some(t);
            }
        }
        assert!((1100..=1250).contains(&fired.unwrap()), "{fired:?}");
    }

    #[test]
    fn stale_text_closes_a_noisy_room_where_the_gate_never_goes_quiet() {
        let mut e = Endpointer::default();
        e.words("open calculator", 0);
        let mut fired = None;
        for t in (0..3000).step_by(50) {
            if e.decide(false, t, true) && fired.is_none() {
                fired = Some(t);
            }
        }
        assert!((1100..=1200).contains(&fired.unwrap()), "{fired:?}");
    }

    #[test]
    fn new_words_hold_it_off_and_it_fires_once() {
        let mut e = Endpointer::default();
        let mut count = 0;
        for t in (0..4000).step_by(50) {
            if t < 1000 {
                e.words(&format!("w{}", t / 100), t);
            }
            if e.decide(t >= 1000, t, true) {
                count += 1;
            }
        }
        assert_eq!(count, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(text: &str, is_final: bool, speech_final: bool) -> String {
        json!({ "type": "Results", "is_final": is_final, "speech_final": speech_final,
                "channel": { "alternatives": [{ "transcript": text, "confidence": 0.9 }] } })
        .to_string()
    }

    #[test]
    fn the_url_asks_for_nova_3_streaming_and_the_key_terms() {
        let url = Params::new("en").url();
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?model=nova-3&language=en"));
        for part in [
            "encoding=linear16",
            "sample_rate=16000",
            "interim_results=true",
            "endpointing=400",
            "utterance_end_ms=1000",
            "keyterm=Claude&",
            "keyterm=Claude%20Code",
            "keyterm=Pointer",
            "keyterm=TypeSafe",
        ] {
            assert!(url.contains(part), "{part} missing from {url}");
        }
        assert!(!url.to_lowercase().contains("token"));
    }

    #[test]
    fn languages_map_to_deepgram_codes() {
        assert_eq!(language_for(""), "en");
        assert_eq!(language_for("auto"), "multi");
        assert_eq!(language_for(" Multi "), "multi");
        assert_eq!(language_for("en-US"), "en-us");
        assert_eq!(language_for("hi-IN"), "hi");
        assert_eq!(language_for("es"), "es");
    }

    #[test]
    fn results_parse_and_garbage_is_ignored() {
        assert_eq!(
            parse_message(&results(" open calculator ", true, true)),
            Event::Transcript {
                text: "open calculator".into(),
                is_final: true,
                speech_final: true,
                from_finalize: false
            }
        );
        assert_eq!(parse_message("{\"type\":\"UtteranceEnd\"}"), Event::UtteranceEnd);
        assert_eq!(parse_message("not json"), Event::Other);
        assert_eq!(parse_message("{\"type\":\"Metadata\"}"), Event::Other);
        assert_eq!(
            parse_message("{\"type\":\"Error\",\"description\":\"bad\"}"),
            Event::Error("bad".into())
        );
    }

    #[test]
    fn pieces_join_into_one_utterance_at_speech_final() {
        let mut u = Utterance::default();
        assert_eq!(
            u.feed(parse_message(&results("open", false, false))),
            Some(Heard::Interim("open".into()))
        );
        assert_eq!(
            u.feed(parse_message(&results("open the", true, false))),
            Some(Heard::Interim("open the".into()))
        );
        assert_eq!(
            u.feed(parse_message(&results("calc", false, false))),
            Some(Heard::Interim("open the calc".into()))
        );
        assert_eq!(
            u.feed(parse_message(&results("calculator", true, true))),
            Some(Heard::Final("open the calculator".into()))
        );
        assert!(!u.pending());
    }

    #[test]
    fn utterance_end_is_the_backstop_and_empty_ends_say_nothing() {
        let mut u = Utterance::default();
        u.feed(parse_message(&results("hello", true, false)));
        assert_eq!(u.feed(Event::UtteranceEnd), Some(Heard::Final("hello".into())));
        assert_eq!(u.feed(Event::UtteranceEnd), None);
        assert_eq!(u.feed(parse_message(&results("", true, true))), None);
        assert_eq!(u.feed(parse_message(&results("", false, false))), None);
    }

    /// A tiny local server that speaks websocket, so connect, audio, results and a dropped socket
    /// are exercised for real without the network.
    fn fake_server(script: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            script(&mut ws);
        });
        format!("ws://127.0.0.1:{port}/v1/listen")
    }

    fn local(url: String) -> Params {
        let mut p = Params::new("en");
        p.base_url = url;
        p
    }

    #[test]
    fn audio_goes_up_and_transcripts_come_back() {
        let url = fake_server(|ws| {
            let first = ws.read().unwrap();
            assert!(matches!(first, Message::Binary(b) if b.len() == 8));
            ws.send(Message::Text(results("open calculator", true, true))).unwrap();
            loop {
                match ws.read() {
                    Ok(Message::Text(t)) if t.contains("CloseStream") => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = ws.close(None);
        });
        let mut s = Stream::connect("test-key", &local(url)).unwrap();
        s.send_audio(&[1, 2, 3, 4]).unwrap();
        let mut heard = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while heard.is_empty() && Instant::now() < deadline {
            heard.extend(s.poll().unwrap());
        }
        assert_eq!(heard, vec![Heard::Final("open calculator".into())]);
        assert!(s.finish(Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn a_dropped_socket_is_an_error_not_a_hang() {
        let url = fake_server(|ws| {
            let _ = ws.read();
            // drop the connection without a close frame
        });
        let mut s = Stream::connect("test-key", &local(url)).unwrap();
        s.send_audio(&[0; 160]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut failed = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            if s.poll().is_err() || s.is_closed() {
                failed = true;
                break;
            }
            let _ = s.send_audio(&[0; 160]);
        }
        assert!(failed);
    }

    #[test]
    fn a_refused_connection_names_no_key() {
        let mut p = Params::new("en");
        p.base_url = "ws://127.0.0.1:1/v1/listen".into();
        let err = Stream::connect("secret-key-123", &p).err().unwrap();
        assert!(!err.contains("secret-key-123"));
    }
}
