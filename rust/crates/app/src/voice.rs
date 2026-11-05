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

/// The echo-cancelled audio window, as the core sees it.
///
/// A hidden Electron window in the shell owns the microphone (Chromium `getUserMedia` with echo
/// cancellation, noise suppression and auto gain) and plays Pointer's voice through an audio
/// element, so Chromium holds the playback as the echo reference. The core and the window talk
/// over the line protocol: the window sends `audio_in { pcm_b64, seq, t }` (16 kHz mono, 30 ms
/// frames) and `tts_state`; the core sends `tts_pcm` chunks (about 25 ms each), `tts_stop` and
/// `aec_capture { on }` as events. Frames are base64 of little-endian 16-bit PCM, so nothing in
/// them can be mistaken for a line break. When the window is not there the core records and
/// plays itself, and the adaptive filter in `platform::audio` does what it can.
pub mod window {
    use super::*;
    use audio::PlayEnd;
    use base64::Engine as _;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::sync::Condvar;

    /// Why a capture or a playback gave up on the window.
    pub const LOST: &str = "the echo-cancelled audio window closed";
    /// A capture that hears no frame at all in this long counts the window as lost.
    const FIRST_FRAME_WAIT: Duration = Duration::from_secs(6);
    /// Playback is sent in frames of this many milliseconds.
    pub const FRAME_MS: u32 = 25;
    /// Frames held while waiting for a missing one, before it is given up on.
    const MAX_REORDER: usize = 6;

    type Sink = Box<dyn Fn(&str, Value) + Send + Sync>;

    #[derive(Default)]
    struct Mic {
        next: Option<u64>,
        pending: BTreeMap<u64, Vec<i16>>,
        ready: VecDeque<Vec<i16>>,
        frames: u64,
        lag_sum_ms: f64,
        lag_max_ms: f64,
        lag_n: u64,
    }

    #[derive(Default)]
    struct Play {
        id: u64,
        started: bool,
        ended: Option<bool>,
    }

    /// What `Link::play` did.
    pub enum Played {
        Done(PlayEnd),
        /// The window went away before any sound: every sample handed to it so far, to be played
        /// by the core instead.
        Lost(Vec<i16>),
    }

    /// Mic lag the window reported: frame stamped in the window to frame received here.
    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    pub struct Lag {
        pub frames: u64,
        pub mean_ms: f64,
        pub max_ms: f64,
    }

    pub struct Link {
        ready: AtomicBool,
        capturing: AtomicBool,
        mic: Mutex<Mic>,
        mic_cv: Condvar,
        play: Mutex<Play>,
        play_cv: Condvar,
        next_id: AtomicU64,
        sink: Sink,
    }

    /// The window the real core talks to: events go to the shell.
    pub fn global() -> &'static Link {
        static LINK: OnceLock<Link> = OnceLock::new();
        LINK.get_or_init(|| Link::new(Box::new(|name, payload| emit(name, payload))))
    }

    pub fn encode(samples: &[i16]) -> String {
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    pub fn decode(b64: &str) -> Result<Vec<i16>, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| format!("pcm_b64 is not base64: {e}"))?;
        if bytes.len() % 2 != 0 {
            return Err("pcm_b64 is not whole 16-bit samples".into());
        }
        Ok(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
    }

    fn epoch_ms() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    }

    impl Link {
        pub fn new(sink: Sink) -> Link {
            Link {
                ready: AtomicBool::new(false),
                capturing: AtomicBool::new(false),
                mic: Mutex::new(Mic::default()),
                mic_cv: Condvar::new(),
                play: Mutex::new(Play::default()),
                play_cv: Condvar::new(),
                next_id: AtomicU64::new(0),
                sink,
            }
        }

        /// The window is up (or gone).
        pub fn set_ready(&self, ready: bool) {
            self.ready.store(ready, Ordering::SeqCst);
            self.mic_cv.notify_all();
            self.play_cv.notify_all();
        }

        pub fn ready(&self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }

        /// Playback goes through the window only while it also records: that is when Chromium
        /// is cancelling, and it needs the reference.
        pub fn usable_for_playback(&self) -> bool {
            self.ready() && self.capturing.load(Ordering::SeqCst)
        }

        pub fn lag(&self) -> Lag {
            let m = self.mic.lock().unwrap_or_else(|p| p.into_inner());
            Lag {
                frames: m.frames,
                mean_ms: if m.lag_n == 0 { 0.0 } else { m.lag_sum_ms / m.lag_n as f64 },
                max_ms: m.lag_max_ms,
            }
        }

        /// One `audio_in` frame. Frames are put back in `seq` order (each request is handled on
        /// its own thread, so two can swap); a frame missing for more than a few others is given
        /// up on. `sent_ms` is the window's wall clock at the end of the frame. Frames that arrive
        /// while nothing is capturing are dropped. Returns the samples taken.
        pub fn push_mic(&self, seq: Option<u64>, sent_ms: Option<f64>, b64: &str) -> Result<usize, String> {
            let samples = decode(b64)?;
            if !self.capturing.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let n = samples.len();
            let mut m = self.mic.lock().unwrap_or_else(|p| p.into_inner());
            m.frames += 1;
            if let Some(t) = sent_ms {
                let lag = epoch_ms() - t;
                if (0.0..10_000.0).contains(&lag) {
                    m.lag_sum_ms += lag;
                    m.lag_n += 1;
                    m.lag_max_ms = m.lag_max_ms.max(lag);
                }
            }
            match seq {
                None => m.ready.push_back(samples),
                Some(seq) => {
                    let next = *m.next.get_or_insert(seq);
                    if seq >= next {
                        m.pending.insert(seq, samples);
                    }
                    loop {
                        let want = m.next.unwrap_or(seq);
                        if let Some(s) = m.pending.remove(&want) {
                            m.ready.push_back(s);
                            m.next = Some(want + 1);
                        } else if m.pending.len() > MAX_REORDER {
                            m.next = m.pending.keys().next().copied();
                        } else {
                            break;
                        }
                    }
                }
            }
            drop(m);
            self.mic_cv.notify_one();
            Ok(n)
        }

        fn next_chunk(&self, wait: Duration) -> Option<Vec<i16>> {
            let mut m = self.mic.lock().unwrap_or_else(|p| p.into_inner());
            if m.ready.is_empty() {
                m = self.mic_cv.wait_timeout(m, wait).unwrap_or_else(|p| p.into_inner()).0;
            }
            // Several frames waiting are handed over as one run, so the tap keeps pace.
            let mut out = Vec::new();
            while let Some(c) = m.ready.pop_front() {
                out.extend(c);
            }
            (!out.is_empty()).then_some(out)
        }

        /// The window's microphone as a capture source: what `audio::record_stream` is for the
        /// core's own, with the same gate and the same tap. Err(`LOST`) when the window goes away
        /// or never sends a frame.
        pub fn capture(
            &self,
            stop: &dyn Fn() -> bool,
            max_seconds: f64,
            silence_seconds: f64,
            tap: &mut dyn FnMut(&[i16], bool) -> bool,
        ) -> Result<CaptureStats, String> {
            *self.mic.lock().unwrap_or_else(|p| p.into_inner()) = Mic::default();
            self.capturing.store(true, Ordering::SeqCst);
            (self.sink)("aec_capture", json!({ "on": true }));
            let result = self.capture_loop(stop, max_seconds, silence_seconds, tap);
            self.capturing.store(false, Ordering::SeqCst);
            (self.sink)("aec_capture", json!({ "on": false }));
            result
        }

        fn capture_loop(
            &self,
            stop: &dyn Fn() -> bool,
            max_seconds: f64,
            silence_seconds: f64,
            tap: &mut dyn FnMut(&[i16], bool) -> bool,
        ) -> Result<CaptureStats, String> {
            let new_gate = |threshold: Option<f64>| match (audio::silence_level_override(), threshold) {
                (Some(level), _) => audio::SilenceGate::new(silence_seconds, level),
                (None, Some(t)) => audio::SilenceGate::new(silence_seconds, t),
                (None, None) => audio::SilenceGate::adaptive(silence_seconds),
            };
            let mut gate = new_gate(None);
            let mut stats = CaptureStats::default();
            let mut out: Vec<i16> = Vec::new();
            let mut total = 0usize;
            let began = Instant::now();
            let deadline = began + Duration::from_secs_f64(max_seconds.max(0.0));
            while !stop() && Instant::now() < deadline {
                if !self.ready() {
                    return Err(LOST.into());
                }
                let Some(chunk) = self.next_chunk(Duration::from_millis(40)) else {
                    if total == 0 && began.elapsed() > FIRST_FRAME_WAIT {
                        return Err(LOST.into());
                    }
                    continue;
                };
                let before = out.len();
                out.extend_from_slice(&chunk);
                total += chunk.len();
                let gate_stop = gate.should_stop(&out);
                for block in out[before..].chunks(audio::BLOCK_FRAMES) {
                    stats.peak_rms = stats.peak_rms.max(audio::rms(block));
                }
                if !tap(&out[before..], gate_stop) {
                    break;
                }
                if out.len() > 45 * audio::SAMPLE_RATE as usize {
                    let threshold = gate.threshold();
                    stats.floor = gate.floor().or(stats.floor);
                    let keep = out.len() - 2 * audio::SAMPLE_RATE as usize;
                    out.drain(..keep);
                    gate = new_gate(threshold);
                }
            }
            stats.floor = gate.floor().or(stats.floor);
            stats.threshold = gate.threshold();
            stats.heard_speech = gate.heard_speech();
            stats.samples = total;
            Ok(stats)
        }

        /// The window reports a playback: `started` when sound really began, `ended` when it
        /// finished or was cut. A stale id is ignored.
        pub fn on_state(&self, id: u64, state: &str, cut: bool) {
            let mut p = self.play.lock().unwrap_or_else(|p| p.into_inner());
            if p.id != id || id == 0 {
                return;
            }
            match state {
                "started" => p.started = true,
                "ended" => {
                    // Played to the end means it sounded, even if "started" was not seen.
                    if !cut {
                        p.started = true;
                    }
                    p.ended.get_or_insert(cut);
                }
                _ => {}
            }
            drop(p);
            self.play_cv.notify_all();
        }

        /// Cut whatever the window is playing, now: tells it at once, without waiting for the
        /// playing thread to notice.
        pub fn cut(&self) {
            let id = {
                let mut p = self.play.lock().unwrap_or_else(|p| p.into_inner());
                if p.id == 0 || p.ended.is_some() {
                    return;
                }
                p.ended = Some(true);
                p.id
            };
            (self.sink)("tts_stop", json!({ "id": id }));
            self.play_cv.notify_all();
        }

        fn send_frame(&self, id: u64, rate: u32, seq: u64, samples: &[i16], last: bool, prebuffer_ms: u32) {
            (self.sink)(
                "tts_pcm",
                json!({ "id": id, "rate": rate, "seq": seq, "pcm_b64": encode(samples), "last": last,
                        "prebuffer_ms": prebuffer_ms }),
            );
        }

        /// Play the chunks that arrive on `chunks` through the window, in frames of `FRAME_MS`.
        /// Blocks until the window says it is over, `stopped` says so (the window is told at
        /// once), or the window is lost. `on_start` runs when the window reports sound really
        /// began (it is left in place when the window was lost before that).
        pub fn play(
            &self,
            rate: u32,
            chunks: &Receiver<Vec<i16>>,
            stopped: &dyn Fn() -> bool,
            prebuffer_ms: u32,
            on_start: &mut Option<Box<dyn FnOnce() + '_>>,
        ) -> Played {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
            *self.play.lock().unwrap_or_else(|p| p.into_inner()) = Play { id, started: false, ended: None };
            let frame = (rate * FRAME_MS / 1000) as usize;
            let (mut buf, mut sent): (Vec<i16>, Vec<i16>) = (Vec::new(), Vec::new());
            let (mut seq, mut sender_gone, mut last_sent, mut started) = (0u64, false, false, false);
            let began = Instant::now();
            let result = loop {
                if stopped() {
                    self.cut();
                    break Played::Done(PlayEnd::Stopped);
                }
                let (now_started, ended) = {
                    let p = self.play.lock().unwrap_or_else(|p| p.into_inner());
                    (p.started, p.ended)
                };
                if now_started && !started {
                    started = true;
                    if let Some(f) = on_start.take() {
                        f();
                    }
                }
                if let Some(cut) = ended {
                    break Played::Done(if cut { PlayEnd::Stopped } else { PlayEnd::Finished });
                }
                if !self.ready() {
                    // Gone mid-line: what was heard was heard. Before any sound, the core takes over.
                    if started {
                        break Played::Done(PlayEnd::Stopped);
                    }
                    sent.extend_from_slice(&buf);
                    break Played::Lost(sent);
                }
                if sender_gone {
                    let p = self.play.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = self.play_cv.wait_timeout(p, Duration::from_millis(8));
                } else {
                    match chunks.recv_timeout(Duration::from_millis(8)) {
                        Ok(c) => {
                            buf.extend_from_slice(&c);
                            while buf.len() >= frame {
                                let part: Vec<i16> = buf.drain(..frame).collect();
                                self.send_frame(id, rate, seq, &part, false, prebuffer_ms);
                                seq += 1;
                                sent.extend_from_slice(&part);
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => sender_gone = true,
                    }
                }
                if sender_gone && !last_sent {
                    last_sent = true;
                    self.send_frame(id, rate, seq, &buf, true, prebuffer_ms);
                    sent.extend_from_slice(&std::mem::take(&mut buf));
                }
                // A window that never answers must not hold the line forever.
                let audio_s = sent.len() as f64 / f64::from(rate);
                if last_sent && began.elapsed().as_secs_f64() > audio_s + 8.0 {
                    self.cut();
                    break Played::Done(PlayEnd::Stopped);
                }
            };
            let mut p = self.play.lock().unwrap_or_else(|p| p.into_inner());
            if p.id == id {
                p.id = 0;
            }
            result
        }
    }
}

/// Said once per process: the window is not there, so the core records and plays itself.
fn note_no_window() {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::SeqCst) {
        line("voice: no echo-cancelling window; recording in the core with the adaptive echo filter");
    }
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
                    // The echo-cancelling window records when the shell has one up; otherwise the core does.
                    let link = window::global();
                    let use_window = link.ready();
                    if !use_window {
                        note_no_window();
                    }
                    let mut record = |tap: &mut dyn FnMut(&[i16], bool) -> bool| {
                        if use_window {
                            link.capture(&stopped, max, GATE_FALLBACK_SECONDS, tap)
                        } else {
                            audio::record_stream(stopped, max, GATE_FALLBACK_SECONDS, tap)
                        }
                    };
                    // `stop` ends the capture; `cancel` is a separate flag so a stop still flushes.
                    let never = AtomicBool::new(false);
                    let outcome = drive_lazy(sock_rx, &purpose, &never, &mut record);
                    if let Some(s) = &outcome.stats {
                        // Levels only, never words: what the room and the speaker measured.
                        let mut stats = json!({ "floor": s.floor, "threshold": s.threshold, "peak_rms": s.peak_rms,
                                    "heard_speech": s.heard_speech, "seconds": s.samples as f64 / 16000.0,
                                    "source": if use_window { "window" } else { "core" } });
                        if use_window {
                            let lag = link.lag();
                            stats["mic_lag_ms"] = json!(lag.mean_ms);
                            stats["mic_lag_max_ms"] = json!(lag.max_ms);
                        }
                        emit("stt_stats", stats);
                    }
                    if outcome.dropped.as_deref() == Some(window::LOST) && !stopped() {
                        // The window went away: the core takes the microphone, quietly, once.
                        line("voice: the echo-cancelling window closed; recording in the core instead");
                        continue;
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
    // ---- the echo-cancelling window ------------------------------------------------------

    use super::window::{self, Link, Played};
    use std::sync::mpsc::channel;

    type Events = std::sync::mpsc::Receiver<(String, Value)>;

    fn link_with_events() -> (Arc<Link>, Events) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let link = Link::new(Box::new(move |n, v| {
            let _ = tx.lock().unwrap().send((n.to_string(), v));
        }));
        (Arc::new(link), rx)
    }

    #[test]
    fn pcm_survives_base64_whatever_the_bytes() {
        let all: Vec<i16> = (0..2048).map(|i| (i * 31 - 32768) as i16).chain([i16::MIN, i16::MAX, 10, 13, 0]).collect();
        assert_eq!(window::decode(&window::encode(&all)).unwrap(), all);
        assert!(window::decode("AAAA").is_err(), "odd bytes are not samples");
        assert!(window::decode("%%%").is_err());
    }

    #[test]
    fn mic_frames_come_back_in_order_and_a_lost_one_is_skipped() {
        let (link, _ev) = link_with_events();
        link.set_ready(true);
        let l2 = link.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            // 2 and 1 swapped, as two request threads can; 4 never comes, 5..=11 pile up.
            for seq in [0u64, 2, 1, 3] {
                l2.push_mic(Some(seq), None, &window::encode(&[seq as i16; 4])).unwrap();
            }
            for seq in 5..=11u64 {
                l2.push_mic(Some(seq), None, &window::encode(&[seq as i16; 4])).unwrap();
            }
            std::thread::sleep(Duration::from_millis(100));
        });
        let mut got: Vec<i16> = Vec::new();
        let stop_at = Instant::now() + Duration::from_millis(400);
        let stats = link
            .capture(&|| Instant::now() > stop_at, 5.0, 0.45, &mut |s, _| {
                got.extend_from_slice(s);
                true
            })
            .unwrap();
        feeder.join().unwrap();
        let firsts: Vec<i16> = got.chunks(4).map(|c| c[0]).collect();
        assert_eq!(firsts, vec![0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11], "{firsts:?}");
        assert_eq!(stats.samples, 44);
        assert!(link.lag().frames >= 11);
    }

    #[test]
    fn frames_sent_when_nothing_captures_are_dropped() {
        let (link, _ev) = link_with_events();
        assert_eq!(link.push_mic(Some(0), None, &window::encode(&[1, 2])).unwrap(), 0);
    }

    #[test]
    fn a_capture_says_so_when_the_window_goes_away() {
        let (link, ev) = link_with_events();
        link.set_ready(true);
        let l2 = link.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            l2.set_ready(false);
        });
        let r = link.capture(&|| false, 5.0, 0.45, &mut |_, _| true);
        assert_eq!(r.unwrap_err(), window::LOST);
        let names: Vec<String> = ev.try_iter().map(|(n, v)| format!("{n}:{}", v["on"])).collect();
        assert_eq!(names, vec!["aec_capture:true", "aec_capture:false"]);
    }

    #[test]
    fn a_session_streams_the_window_mic_to_deepgram() {
        let params = fake_server(|ws| {
            let _ = ws.read();
            ws.send(result("open the calculator", true, true)).unwrap();
            while let Ok(m) = ws.read() {
                if matches!(&m, Message::Text(t) if t.contains("CloseStream")) {
                    break;
                }
            }
            let _ = ws.close(None);
        });
        let stream = Stream::connect("k", &params).unwrap();
        let (link, _ev) = link_with_events();
        link.set_ready(true);
        let l2 = link.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            for seq in 0..40u64 {
                l2.push_mic(Some(seq), None, &window::encode(&[0i16; 480])).unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
            l2.set_ready(false);
        });
        let never = AtomicBool::new(false);
        let mut src = |tap: &mut dyn FnMut(&[i16], bool) -> bool| link.capture(&|| false, 5.0, 0.45, tap);
        let out = drive(stream, "talk", &never, &mut src);
        assert!(out.text.contains("calculator"), "{} {:?}", out.text, out.dropped);
    }

    /// A fake window: acknowledges the first frame as sound, and the last frame as the end.
    fn fake_player(link: Arc<Link>, ev: Events, frames: Arc<Mutex<Vec<Value>>>) {
        std::thread::spawn(move || {
            while let Ok((name, v)) = ev.recv() {
                let id = v["id"].as_u64().unwrap_or(0);
                match name.as_str() {
                    "tts_pcm" => {
                        let first = frames.lock().unwrap().is_empty();
                        frames.lock().unwrap().push(v.clone());
                        if first {
                            link.on_state(id, "started", false);
                        }
                        if v["last"] == true {
                            link.on_state(id, "ended", false);
                        }
                    }
                    "tts_stop" => link.on_state(id, "ended", true),
                    _ => {}
                }
            }
        });
    }

    #[test]
    fn playback_goes_to_the_window_in_small_frames_and_reports_real_start_and_end() {
        let (link, ev) = link_with_events();
        link.set_ready(true);
        let frames = Arc::new(Mutex::new(Vec::new()));
        fake_player(link.clone(), ev, frames.clone());
        let (tx, rx) = channel::<Vec<i16>>();
        let line: Vec<i16> = (0..24_000).map(|i| (i % 2000) as i16).collect(); // one second at 24 kHz
        for c in line.chunks(5000) {
            tx.send(c.to_vec()).unwrap();
        }
        drop(tx);
        let started = AtomicBool::new(false);
        let mut cb: Option<Box<dyn FnOnce() + '_>> = Some(Box::new(|| started.store(true, Ordering::SeqCst)));
        let r = link.play(24_000, &rx, &|| false, 80, &mut cb);
        assert!(matches!(r, Played::Done(audio::PlayEnd::Finished)));
        assert!(started.load(Ordering::SeqCst), "on_start follows the window's own report");
        let frames = frames.lock().unwrap();
        // 25 ms at 24 kHz is 600 samples: 40 whole frames, then the closing one.
        assert_eq!(frames.len(), 41);
        let mut heard: Vec<i16> = Vec::new();
        for f in frames.iter() {
            heard.extend(window::decode(f["pcm_b64"].as_str().unwrap()).unwrap());
        }
        assert_eq!(heard, line, "the same samples, in order");
        assert!(frames.iter().all(|f| f["rate"] == 24_000 && f["prebuffer_ms"] == 80));
        assert_eq!(frames.last().unwrap()["last"], true);
    }

    #[test]
    fn stop_reaches_the_window_at_once_and_ends_the_line() {
        let (link, ev) = link_with_events();
        link.set_ready(true);
        let frames = Arc::new(Mutex::new(Vec::new()));
        fake_player(link.clone(), ev, frames.clone());
        let (tx, rx) = channel::<Vec<i16>>();
        let l2 = link.clone();
        let cutter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let at = Instant::now();
            l2.cut();
            at
        });
        std::thread::spawn(move || {
            // A long line still arriving.
            for _ in 0..200 {
                if tx.send(vec![7i16; 2400]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let r = link.play(24_000, &rx, &|| false, 0, &mut None);
        let ended = Instant::now();
        let cut_at = cutter.join().unwrap();
        assert!(matches!(r, Played::Done(audio::PlayEnd::Stopped)));
        assert!(ended.duration_since(cut_at) < Duration::from_millis(40), "{:?}", ended.duration_since(cut_at));
    }

    #[test]
    fn a_stopped_flag_tells_the_window_without_waiting_for_a_chunk() {
        let (link, ev) = link_with_events();
        link.set_ready(true);
        let (_tx, rx) = channel::<Vec<i16>>();
        let flag = AtomicBool::new(false);
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                flag.store(true, Ordering::SeqCst);
            });
            link.play(24_000, &rx, &|| flag.load(Ordering::SeqCst), 0, &mut None)
        });
        assert!(matches!(r, Played::Done(audio::PlayEnd::Stopped)));
        assert!(ev.try_iter().any(|(n, _)| n == "tts_stop"));
    }

    #[test]
    fn a_window_lost_before_any_sound_hands_the_samples_back() {
        let (link, _ev) = link_with_events();
        link.set_ready(true);
        let (tx, rx) = channel::<Vec<i16>>();
        tx.send(vec![5i16; 1000]).unwrap();
        let l2 = link.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            l2.set_ready(false);
        });
        let ran = AtomicBool::new(false);
        let mut cb: Option<Box<dyn FnOnce() + '_>> = Some(Box::new(|| ran.store(true, Ordering::SeqCst)));
        match link.play(24_000, &rx, &|| false, 0, &mut cb) {
            Played::Lost(kept) => assert_eq!(kept, vec![5i16; 1000]),
            _ => panic!("expected the samples back"),
        }
        assert!(cb.is_some() && !ran.load(Ordering::SeqCst), "on_start is left for the core's player");
    }

    #[test]
    fn playback_uses_the_window_only_while_it_also_records() {
        let (link, _ev) = link_with_events();
        assert!(!link.usable_for_playback());
        link.set_ready(true);
        assert!(!link.usable_for_playback(), "no capture, no echo reference to keep");
    }
}
