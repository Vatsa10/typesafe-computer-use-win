//! The microphone, on one of four engines (`CLICKER_STT`):
//!
//! - **Deepgram** (the default when `DEEPGRAM_API_KEY` is set): this process records through
//!   `platform::audio` and streams 16 kHz PCM to Deepgram over a websocket, and every interim and
//!   final result goes out as a `transcript` event. The key stays here; the shell never sees it.
//! - **Chrome** (the default without a Deepgram key): Google's free recognizer in the user's own
//!   Chrome or Edge, which the Electron shell drives - this core never records on it.
//! - **OpenAI**: recorded on a worker thread, transcribed with a vocabulary prompt.
//! - **Windows**: the free recognizer (`platform::stt`), which records and recognizes in one go
//!   because WinRT cannot be handed recorded audio.
//!
//! When Deepgram cannot be reached, or the socket drops, the engine reads as `chrome` for a few
//! minutes and the shell is told (`stt_fallback` and a line), so the user keeps a microphone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use api::deepgram::{self, Heard, Stream};
use platform::audio::CaptureStats;
use platform::{audio, stt};
use serde_json::{json, Value};

use crate::emit::Emitter;

/// The decoding bias, the same sentence the Python uses, so "Claude Code" is not heard as "cloud
/// code". Short on purpose: a long prompt gets echoed back as if it had been spoken.
pub const VOICE_PROMPT: &str = "Claude Code, VS Code, Chrome, WhatsApp, terminal, GitHub: say which monitor or window to use, scroll it, and stop, pause, resume or quit.";

/// `CLICKER_VOICE_PROMPT` overrides the prompt; set to empty it disables it.
pub fn voice_prompt() -> Option<String> {
    let prompt = std::env::var("CLICKER_VOICE_PROMPT").unwrap_or_else(|_| VOICE_PROMPT.to_string());
    (!prompt.trim().is_empty()).then_some(prompt)
}

/// Where this module's events and lines go. Set once by `main`; tests leave it unset.
static OUT: OnceLock<Emitter> = OnceLock::new();

pub fn attach(out: Emitter) {
    let _ = OUT.set(out);
    prewarm();
}

/// An event to the shell, if one is attached.
pub fn emit(event: &str, payload: Value) {
    if let Some(out) = OUT.get() {
        out.event(event, payload);
    }
}

/// A status line to the shell, if one is attached. Never a transcript.
pub fn line(text: &str) {
    if let Some(out) = OUT.get() {
        out.line(text);
    }
}

/// Start/stop listening (the bar and the panel) and push-to-talk (the hotkey).
pub trait Listener: Send + Sync {
    /// Begin recording in the background. Ok(false) when one is already going.
    fn start(&self) -> Result<bool, String>;
    /// As `start`, for a session the shell opened: `conv` keeps the microphone open across
    /// sentences, `purpose` is `talk` or `dictate` and rides on every `transcript` event.
    fn start_session(&self, _conv: bool, _purpose: &str) -> Result<bool, String> {
        self.start()
    }
    /// Stop, transcribe, return what was heard ("" for nothing).
    fn stop(&self) -> Result<String, String>;
    /// Record while `vk` is held, then transcribe.
    fn while_held(&self, vk: u32) -> Result<String, String>;
    /// The engine's name: "deepgram", "chrome", "openai" or "windows". On "chrome" and
    /// "deepgram" the shell brackets push-to-talk and asks for the session itself.
    fn engine(&self) -> String;
    /// Block while `vk` is held (bounded by the longest utterance). Push-to-talk on the
    /// shell-driven engines: the shell records between the start and stop this brackets.
    fn hold(&self, vk: u32);
}

/// The line `listen_start`/`listen_stop` give when the core is asked to record on Chrome.
pub const IN_SHELL: &str = "the chrome speech engine runs in the shell, not the core";

type Recording = (Arc<AtomicBool>, JoinHandle<Result<String, String>>);

/// Which engine `CLICKER_STT` selects, as a typed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Deepgram,
    Chrome,
    OpenAi,
    Windows,
}

/// How long the browser recognizer stands in after Deepgram failed, before Deepgram is tried again.
const FALLBACK_FOR: Duration = Duration::from_secs(120);

fn fallback_slot() -> &'static Mutex<Option<Instant>> {
    static SLOT: Mutex<Option<Instant>> = Mutex::new(None);
    &SLOT
}

fn fallen_back() -> bool {
    let mut slot = fallback_slot().lock().unwrap_or_else(|p| p.into_inner());
    match *slot {
        Some(at) if at.elapsed() < FALLBACK_FOR => true,
        Some(_) => {
            *slot = None;
            false
        }
        None => false,
    }
}

/// Deepgram failed: use the browser recognizer for a while, and tell the shell and the user.
fn note_fallback(why: &str) {
    *fallback_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    line(&format!("voice: {why}; using the browser recognizer instead"));
    emit("stt_fallback", json!({ "engine": "chrome", "reason": why }));
}

impl Engine {
    pub fn from_name(name: &str) -> Engine {
        match name {
            "windows" => Engine::Windows,
            "openai" => Engine::OpenAi,
            "deepgram" => Engine::Deepgram,
            _ => Engine::Chrome,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Deepgram => "deepgram",
            Engine::Chrome => "chrome",
            Engine::OpenAi => "openai",
            Engine::Windows => "windows",
        }
    }

    /// The pure half of `current`: the setting, whether a Deepgram key exists, and whether
    /// Deepgram has just failed. `auto` prefers Deepgram, then Chrome; a named engine is forced.
    pub fn resolve(setting: &str, has_deepgram_key: bool, fell_back: bool) -> Result<Engine, String> {
        match setting.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(if has_deepgram_key && !fell_back {
                Engine::Deepgram
            } else {
                Engine::Chrome
            }),
            "deepgram" if !has_deepgram_key => {
                Err("CLICKER_STT=deepgram needs DEEPGRAM_API_KEY (Settings)".into())
            }
            "deepgram" => Ok(if fell_back { Engine::Chrome } else { Engine::Deepgram }),
            other => wcore::config::choose_stt(other).map(Engine::from_name),
        }
    }

    pub fn current() -> Result<Engine, String> {
        let setting = std::env::var("CLICKER_STT").unwrap_or_default();
        let has_key = std::env::var("DEEPGRAM_API_KEY").is_ok_and(|k| !k.trim().is_empty());
        Engine::resolve(&setting, has_key, fallen_back())
    }

    /// A clear line when this engine cannot run here, checked before recording starts.
    fn check(self) -> Result<(), String> {
        match self {
            Engine::Windows => stt::unavailable_reason().map_or(Ok(()), Err),
            Engine::OpenAi | Engine::Deepgram => Ok(()),
            Engine::Chrome => Err(IN_SHELL.into()),
        }
    }
}

#[derive(Default)]
pub struct Microphone {
    active: Mutex<Option<Recording>>,
}

fn transcribe(samples: &[i16]) -> Result<String, String> {
    if samples.is_empty() || audio::is_silent(samples, audio::SILENCE_LEVEL) {
        return Ok(String::new());
    }
    let client = api::openai::OpenAi::from_env()
        .ok_or_else(|| "voice on OpenAI needs OPENAI_API_KEY (Settings), or set CLICKER_STT=windows for the free engine".to_string())?;
    let wav = audio::to_wav(samples, audio::SAMPLE_RATE);
    let prompt = voice_prompt();
    client
        .transcribe(&wav, prompt.as_deref())
        .map(|t| t.trim().to_string())
        .map_err(|e| format!("transcription failed: {e}"))
}

/// Seconds of quiet after which the local gate first reports the speaker quiet. The endpointer
/// (`api::deepgram::Endpointer`) adds the rest of the wait: 850 ms, or 1.6 s mid-phrase.
pub const GATE_FALLBACK_SECONDS: f64 = 0.45;

/// How much microphone audio is kept while the socket is still opening, so the first words
/// are not clipped.
const RING_SAMPLES: usize = 2 * 16_000;

/// What a streaming session produced.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Every finished utterance, joined.
    pub text: String,
    /// Why the socket failed, if it did.
    pub dropped: Option<String>,
    pub stats: Option<CaptureStats>,
}

fn emit_heard(heard: &Heard, purpose: &str, finals: &mut Vec<String>) {
    match heard {
        Heard::Interim(text) => emit(
            "transcript",
            json!({ "text": text, "final": false, "purpose": purpose }),
        ),
        Heard::Final(text) => {
            finals.push(text.clone());
            emit(
                "transcript",
                json!({ "text": text, "final": true, "purpose": purpose }),
            );
        }
    }
}

/// The audio side of a session: calls the tap with each new run of samples and whether the local
/// gate thinks the speaker finished; the tap returns false to stop.
pub type Source<'a> = &'a mut dyn FnMut(&mut dyn FnMut(&[i16], bool) -> bool) -> Result<CaptureStats, String>;

/// As [`drive_lazy`] with a socket that is already open.
pub fn drive(stream: Stream, purpose: &str, cancel: &AtomicBool, source: Source) -> Outcome {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = tx.send(Ok(stream));
    drive_lazy(rx, purpose, cancel, source)
}

/// Stream whatever `source` produces to Deepgram until it ends, and collect what was heard. The
/// socket may still be opening: audio is kept (the last 2 s) until it arrives, then sent first.
/// `cancel` drops the utterance: nothing more is emitted. Transcripts are emitted as they come
/// and are never logged.
pub fn drive_lazy(
    socket: std::sync::mpsc::Receiver<Result<Stream, String>>,
    purpose: &str,
    cancel: &AtomicBool,
    source: Source,
) -> Outcome {
    use std::sync::mpsc::TryRecvError;
    let mut finals: Vec<String> = Vec::new();
    let mut dropped: Option<String> = None;
    let mut stream: Option<Stream> = None;
    let mut ring: Vec<i16> = Vec::new();
    let mut end = deepgram::Endpointer::default();
    let mut acked = false;
    let trace = std::env::var("POINTER_VOICE_TRACE").is_ok_and(|v| v == "1");
    let began = Instant::now();
    let result = {
        let mut tap = |samples: &[i16], gate_quiet: bool| -> bool {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            if stream.is_none() {
                match socket.try_recv() {
                    Ok(Ok(mut s)) => {
                        ring.extend_from_slice(samples);
                        if trace {
                            eprintln!("[voice] socket ready at +{} ms, replaying {} ms", began.elapsed().as_millis(), ring.len() / 16);
                        }
                        if let Err(e) = s.send_audio(&ring) {
                            dropped = Some(e);
                            return false;
                        }
                        ring = Vec::new();
                        stream = Some(s);
                        return true;
                    }
                    Ok(Err(e)) => {
                        dropped = Some(e);
                        return false;
                    }
                    Err(TryRecvError::Empty) => {
                        ring.extend_from_slice(samples);
                        if ring.len() > RING_SAMPLES {
                            let cut = ring.len() - RING_SAMPLES;
                            ring.drain(..cut);
                        }
                        return true;
                    }
                    Err(TryRecvError::Disconnected) => {
                        dropped = Some("the deepgram socket never opened".into());
                        return false;
                    }
                }
            }
            let s = stream.as_mut().expect("checked above");
            if let Err(e) = s.send_audio(samples) {
                dropped = Some(e);
                return false;
            }
            let now = began.elapsed().as_millis() as u64;
            match s.poll() {
                Ok(heard) => {
                    for h in &heard {
                        match h {
                            Heard::Final(_) => {
                                end.closed();
                                if !acked {
                                    crate::tts::ack_if_enabled();
                                }
                                acked = false;
                            }
                            Heard::Interim(text) => {
                                if acked {
                                    // The speaker went on: the "hmm" was early.
                                    crate::tts::cancel_ack();
                                    acked = false;
                                }
                                end.words(text, now);
                            }
                        }
                        emit_heard(h, purpose, &mut finals);
                    }
                }
                Err(e) => {
                    dropped = Some(e);
                    return false;
                }
            }
            if s.is_closed() {
                dropped = Some("the deepgram socket closed".into());
                return false;
            }
            // The local gate and the age of Deepgram's own text decide together when the speaker
            // is done; Deepgram's speech_final does not fire in a noisy room.
            if end.decide(gate_quiet, now, s.pending()) {
                if trace {
                    eprintln!("[voice] endpoint: Finalize at +{now} ms");
                }
                crate::tts::ack_if_enabled();
                acked = true;
                if let Ok(Some(h)) = s.finalize_now() {
                    // Every word was already final: no need to wait for the server's answer.
                    end.closed();
                    emit_heard(&h, purpose, &mut finals);
                    acked = false;
                }
            }
            true
        };
        source(&mut tap)
    };
    let stats = result.as_ref().ok().cloned();
    if stream.is_none() && dropped.is_none() {
        if let Ok(Ok(s)) = socket.recv_timeout(Duration::from_secs(2)) {
            stream = Some(s);
        }
    }
    if let Some(s) = stream.as_mut() {
        if dropped.is_none() && !cancel.load(Ordering::SeqCst) {
            if !ring.is_empty() {
                let _ = s.send_audio(&ring);
            }
            for h in s.finish(Duration::from_secs(2)) {
                emit_heard(&h, purpose, &mut finals);
            }
        }
    }
    if let Err(e) = result {
        dropped.get_or_insert(e);
    }
    Outcome {
        text: if cancel.load(Ordering::SeqCst) {
            String::new()
        } else {
            finals.join(" ")
        },
        dropped,
        stats,
    }
}

/// A spare, already-open Deepgram socket kept alive with `KeepAlive`, so a session opens with no
/// handshake. Taken by the next session; the keeper opens another when none is in use.
mod warm {
    use super::*;

    struct Spare {
        url: String,
        stream: Stream,
        born: Instant,
    }

    static SPARE: Mutex<Option<Spare>> = Mutex::new(None);
    pub static IN_SESSION: AtomicBool = AtomicBool::new(false);
    static KEEPER: std::sync::Once = std::sync::Once::new();

    pub fn take(params: &deepgram::Params) -> Option<Stream> {
        let mut slot = SPARE.lock().unwrap_or_else(|p| p.into_inner());
        match slot.take() {
            Some(sp) if sp.url == params.url() && !sp.stream.is_closed() => Some(sp.stream),
            _ => None,
        }
    }

    /// Start the keeper once (needs a key and the Deepgram engine to do anything).
    pub fn start() {
        KEEPER.call_once(|| {
            let _ = std::thread::Builder::new().name("voice-warm".into()).spawn(|| {
                let mut ticks = 0u32;
                loop {
                    std::thread::sleep(Duration::from_secs(4));
                    ticks += 1;
                    if ticks % 10 == 0 {
                        crate::tts::warm();
                    }
                    if !matches!(Engine::current(), Ok(Engine::Deepgram)) {
                        continue;
                    }
                    let mut slot = SPARE.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(sp) = slot.as_mut() {
                        let stale = sp.born.elapsed() > Duration::from_secs(240);
                        let alive = sp.stream.keep_alive().is_ok() && sp.stream.poll().is_ok() && !sp.stream.is_closed();
                        if stale || !alive {
                            *slot = None;
                        }
                    }
                    if slot.is_none() && !IN_SESSION.load(Ordering::SeqCst) {
                        drop(slot);
                        let key = std::env::var("DEEPGRAM_API_KEY").unwrap_or_default().trim().to_string();
                        if key.is_empty() {
                            continue;
                        }
                        let params = deepgram_params();
                        if let Ok(stream) = Stream::connect(&key, &params) {
                            *SPARE.lock().unwrap_or_else(|p| p.into_inner()) =
                                Some(Spare { url: params.url(), stream, born: Instant::now() });
                        }
                    }
                }
            });
        });
    }
}

/// Keep the Deepgram socket and the voice connection warm from now on. Called once at start.
pub fn prewarm() {
    if std::env::var("DEEPGRAM_API_KEY").is_ok_and(|k| !k.trim().is_empty()) {
        warm::start();
    }
}

fn deepgram_params() -> deepgram::Params {
    let mut params =
        deepgram::Params::new(&deepgram::language_for(&std::env::var("CLICKER_STT_LANG").unwrap_or_default()));
    if let Ok(url) = std::env::var("POINTER_DEEPGRAM_URL") {
        if !url.trim().is_empty() {
            params.base_url = url;
        }
    }
    params
}

impl Microphone {
    fn start_deepgram(&self, conv: bool, purpose: &str) -> Result<bool, String> {
        let mut active = self.active.lock().map_err(|_| "the recorder is wedged")?;
        if active.as_ref().is_some_and(|(_, h)| !h.is_finished()) {
            return Ok(false);
        }
        let key = std::env::var("DEEPGRAM_API_KEY").unwrap_or_default().trim().to_string();
        let params = deepgram_params();
        // An open conversation microphone has no cap but the day; a one-off follows the setting.
        let max = if conv {
            6.0 * 3600.0
        } else {
            wcore::config::voice_max_seconds()?
        };
        let purpose = purpose.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let handle = std::thread::Builder::new()
            .name("deepgram".into())
            .spawn(move || {
                warm::IN_SESSION.store(true, Ordering::SeqCst);
                crate::tts::set_conversation(conv && purpose == "talk");
                let stopped = || flag.load(Ordering::SeqCst);
                let mut text = Vec::new();
                let mut retried = false;
                let mut ready = Some(ready_tx);
                let result = loop {
                    // Capture starts at once; the socket (a warm spare, else a fresh one) opens beside it.
                    let (sock_tx, sock_rx) = std::sync::mpsc::channel::<Result<Stream, String>>();
                    let (key, params, tell) = (key.clone(), params.clone(), ready.take());
                    std::thread::spawn(move || {
                        let opened = warm::take(&params).map_or_else(|| Stream::connect(&key, &params), Ok);
                        if let Some(t) = tell {
                            let _ = t.send(opened.as_ref().map(|_| ()).map_err(|e| e.clone()));
                        }
                        let _ = sock_tx.send(opened);
                    });
                    let mut record = |tap: &mut dyn FnMut(&[i16], bool) -> bool| {
                        audio::record_stream(stopped, max, GATE_FALLBACK_SECONDS, tap)
                    };
                    // `stop` ends the capture; `cancel` is a separate flag so a stop still flushes.
                    let never = AtomicBool::new(false);
                    let outcome = drive_lazy(sock_rx, &purpose, &never, &mut record);
                    if let Some(s) = &outcome.stats {
                        // Levels only, never words: what the room and the speaker measured.
                        emit(
                            "stt_stats",
                            json!({ "floor": s.floor, "threshold": s.threshold, "peak_rms": s.peak_rms,
                                    "heard_speech": s.heard_speech, "seconds": s.samples as f64 / 16000.0 }),
                        );
                    }
                    if !outcome.text.is_empty() {
                        text.push(outcome.text.clone());
                    }
                    match &outcome.dropped {
                        // One quiet reconnect for an open conversation before giving up on Deepgram.
                        Some(_) if conv && !retried && !stopped() => {
                            retried = true;
                            continue;
                        }
                        Some(why) if !stopped() => note_fallback(why),
                        _ => {}
                    }
                    break Ok(text.join(" "));
                };
                crate::tts::set_conversation(false);
                warm::IN_SESSION.store(false, Ordering::SeqCst);
                result
            })
            .map_err(|e| format!("could not start recording: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {
                *active = Some((stop, handle));
                Ok(true)
            }
            Ok(Err(why)) => {
                note_fallback(&why);
                Err(why)
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                let why = "deepgram did not answer in time".to_string();
                note_fallback(&why);
                Err(why)
            }
        }
    }
}

impl Listener for Microphone {
    fn start(&self) -> Result<bool, String> {
        self.start_session(false, "talk")
    }

    fn start_session(&self, conv: bool, purpose: &str) -> Result<bool, String> {
        let engine = Engine::current()?;
        if engine == Engine::Deepgram {
            return self.start_deepgram(conv, purpose);
        }
        let mut active = self.active.lock().map_err(|_| "the recorder is wedged")?;
        if active.as_ref().is_some_and(|(_, h)| !h.is_finished()) {
            return Ok(false);
        }
        engine.check()?;
        let max = wcore::config::voice_max_seconds()?;
        let silence = wcore::config::bar_silence_seconds()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("record".into())
            .spawn(move || {
                let stopped = || flag.load(Ordering::SeqCst);
                match engine {
                    Engine::Windows => stt::recognize_live(stopped, max, silence),
                    Engine::OpenAi => transcribe(&audio::record_until(stopped, max, silence)?),
                    Engine::Chrome | Engine::Deepgram => Err(IN_SHELL.into()),
                }
            })
            .map_err(|e| format!("could not start recording: {e}"))?;
        *active = Some((stop, handle));
        Ok(true)
    }

    fn stop(&self) -> Result<String, String> {
        let taken = self
            .active
            .lock()
            .map_err(|_| "the recorder is wedged")?
            .take();
        let Some((stop, handle)) = taken else {
            return Err("not listening".into());
        };
        stop.store(true, Ordering::SeqCst);
        handle
            .join()
            .map_err(|_| "the recorder crashed".to_string())?
    }

    fn while_held(&self, vk: u32) -> Result<String, String> {
        let max = wcore::config::voice_max_seconds()?;
        match Engine::current()? {
            Engine::Windows => {
                Engine::Windows.check()?;
                stt::recognize_live(|| !audio::key_held(vk), max, 0.0)
            }
            Engine::OpenAi => transcribe(&audio::record_while_held(vk, max)?),
            Engine::Deepgram => {
                self.start_deepgram(false, "talk")?;
                self.hold(vk);
                self.stop()
            }
            Engine::Chrome => Err(IN_SHELL.into()),
        }
    }

    fn engine(&self) -> String {
        Engine::current().map_or_else(|_| "chrome".into(), |e| e.name().into())
    }

    fn hold(&self, vk: u32) {
        let max = wcore::config::voice_max_seconds().unwrap_or(30.0);
        let deadline = Instant::now() + Duration::from_secs_f64(max.max(1.0));
        while audio::key_held(vk) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::deepgram::Params;
    use std::net::TcpStream;
    use tungstenite::{Message, WebSocket};

    #[test]
    fn the_prompt_names_claude_so_it_is_not_heard_as_cloud() {
        assert!(VOICE_PROMPT.starts_with("Claude Code"));
    }

    #[test]
    fn the_engine_name_maps_to_the_engine() {
        assert_eq!(Engine::from_name("windows"), Engine::Windows);
        assert_eq!(Engine::from_name("openai"), Engine::OpenAi);
        assert_eq!(Engine::from_name("chrome"), Engine::Chrome);
        assert_eq!(Engine::from_name("deepgram"), Engine::Deepgram);
        for e in [Engine::Deepgram, Engine::Chrome, Engine::OpenAi, Engine::Windows] {
            assert_eq!(Engine::from_name(e.name()), e);
        }
    }

    #[test]
    fn deepgram_is_the_default_with_a_key_and_a_named_engine_wins() {
        let r = Engine::resolve;
        assert_eq!(r("auto", true, false).unwrap(), Engine::Deepgram);
        assert_eq!(r("", true, false).unwrap(), Engine::Deepgram);
        assert_eq!(r("auto", false, false).unwrap(), Engine::Chrome);
        assert_eq!(r("chrome", true, false).unwrap(), Engine::Chrome);
        assert_eq!(r(" Windows ", true, false).unwrap(), Engine::Windows);
        assert_eq!(r("openai", true, false).unwrap(), Engine::OpenAi);
        assert_eq!(r("deepgram", true, false).unwrap(), Engine::Deepgram);
        assert!(r("deepgram", false, false).is_err());
        assert!(r("whisper", true, false).is_err());
    }

    #[test]
    fn after_a_failure_the_engine_reads_as_chrome() {
        assert_eq!(Engine::resolve("auto", true, true).unwrap(), Engine::Chrome);
        assert_eq!(Engine::resolve("deepgram", true, true).unwrap(), Engine::Chrome);
        // An explicit choice of another engine is not touched by the fallback.
        assert_eq!(Engine::resolve("openai", true, true).unwrap(), Engine::OpenAi);
    }

    #[test]
    fn silence_is_heard_as_nothing_without_calling_the_api() {
        assert_eq!(transcribe(&[0i16; 16_000]).unwrap(), "");
        assert_eq!(transcribe(&[]).unwrap(), "");
    }

    fn fake_server(script: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static) -> Params {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            script(&mut tungstenite::accept(tcp).unwrap());
        });
        let mut p = Params::new("en");
        p.base_url = format!("ws://127.0.0.1:{port}/v1/listen");
        p
    }

    fn result(text: &str, is_final: bool, speech_final: bool) -> Message {
        Message::Text(
            json!({ "type": "Results", "is_final": is_final, "speech_final": speech_final,
                    "channel": { "alternatives": [{ "transcript": text }] } })
            .to_string(),
        )
    }

    /// A microphone that plays `chunks` of silence and calls the tap, as the real one would.
    fn fake_mic(chunks: usize, quiet_after: usize) -> impl FnMut(&mut dyn FnMut(&[i16], bool) -> bool) -> Result<CaptureStats, String> {
        move |tap| {
            for i in 0..chunks {
                std::thread::sleep(Duration::from_millis(15));
                if !tap(&[0i16; 1024], i >= quiet_after) {
                    break;
                }
            }
            Ok(CaptureStats::default())
        }
    }

    #[test]
    fn a_session_collects_the_finished_utterances() {
        let params = fake_server(|ws| {
            let _ = ws.read();
            ws.send(result("open the", true, false)).unwrap();
            ws.send(result("open the calculator", true, true)).unwrap();
            while let Ok(m) = ws.read() {
                if matches!(&m, Message::Text(t) if t.contains("CloseStream")) {
                    break;
                }
            }
            let _ = ws.close(None);
        });
        let stream = Stream::connect("k", &params).unwrap();
        let never = AtomicBool::new(false);
        let mut mic = fake_mic(40, 1000);
        let out = drive(stream, "talk", &never, &mut mic);
        assert!(out.dropped.is_none(), "{:?}", out.dropped);
        assert!(out.text.contains("calculator"), "{}", out.text);
    }

    #[test]
    fn a_dropped_socket_ends_the_session_with_a_reason() {
        let params = fake_server(|ws| {
            let _ = ws.read();
            // vanish without a close frame
        });
        let stream = Stream::connect("k", &params).unwrap();
        let never = AtomicBool::new(false);
        let mut mic = fake_mic(400, 1000);
        let started = Instant::now();
        let out = drive(stream, "talk", &never, &mut mic);
        assert!(out.dropped.is_some());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_local_gate_asks_deepgram_to_finish_an_open_utterance() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
        let params = fake_server(move |ws| {
            // One final piece that is not speech_final: the utterance stays open.
            let _ = ws.read();
            ws.send(result("hello there", true, false)).unwrap();
            while let Ok(m) = ws.read() {
                if let Message::Text(t) = m {
                    let _ = seen_tx.send(t.clone());
                    if t.contains("Finalize") {
                        let mut reply = json!({ "type": "Results", "is_final": true, "speech_final": true,
                            "from_finalize": true, "channel": { "alternatives": [{ "transcript": "" }] } });
                        reply["from_finalize"] = json!(true);
                        ws.send(Message::Text(reply.to_string())).unwrap();
                    }
                    if t.contains("CloseStream") {
                        break;
                    }
                }
            }
            let _ = ws.close(None);
        });
        let stream = Stream::connect("k", &params).unwrap();
        let never = AtomicBool::new(false);
        let mut mic = fake_mic(120, 5);
        let out = drive(stream, "talk", &never, &mut mic);
        assert_eq!(out.text, "hello there");
        let sent: Vec<String> = seen_rx.try_iter().collect();
        assert!(sent.iter().any(|m| m.contains("Finalize")), "{sent:?}");
    }

    #[test]
    fn a_cancelled_session_returns_nothing() {
        let params = fake_server(|ws| {
            let _ = ws.read();
            ws.send(result("secret words", true, true)).unwrap();
            while ws.read().is_ok() {}
        });
        let stream = Stream::connect("k", &params).unwrap();
        let cancel = AtomicBool::new(true);
        let mut mic = fake_mic(20, 1000);
        assert_eq!(drive(stream, "talk", &cancel, &mut mic).text, "");
    }
}
