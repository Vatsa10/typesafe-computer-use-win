//! Pointer's voice. Three engines: Rumik (`RUMIK_API_KEY`), Murf (`MURF_API_KEY`), and the
//! Windows SAPI voice as the fallback; auto picks the first that has a key, in that order.
//! `POINTER_TTS=rumik|murf|sapi` forces one; `POINTER_VOICE` picks the voice (a Murf voice id, a
//! Rumik speaker, or part of an installed Windows voice's name). The cloud voices are played by
//! this process as the bytes arrive.
//!
//! A reply is split at sentence boundaries and the sentences are synthesised two at a time, in
//! order, into one gap-free playback: the first sentence starts sounding while the rest is still
//! being made. The connection to the service is pooled and kept warm, and every line reports
//! when sound really starts and ends as a `speech` event with `real: true` plus
//! `first_audio_ms` (request to first sound). `stop` cuts the sound at once because the audio
//! device is reset. The spoken text is never logged here.
//!
//! Acknowledgement clips ("hmm", "okay", "let's see") are short files made once in the chosen
//! voice and played the moment a voice request ends, on a lowest-priority lane that the real
//! reply, Stop or the user talking on cancels at once.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::murf::{self, Murf};
use api::rumik::{self, Rumik};
use platform::{audio, speech};
use serde_json::json;

/// Sound is held back this long so a jittery network does not make it stutter.
const PREBUFFER_MS: u32 = 80;
/// How long SAPI gets to begin before the line counts as unspoken.
const SAPI_START_WAIT: Duration = Duration::from_millis(1500);
/// Both cloud voices answer at this rate.
const CLOUD_RATE: u32 = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tts {
    Rumik,
    Murf,
    Sapi,
}

impl Tts {
    pub fn name(self) -> &'static str {
        match self {
            Tts::Rumik => "rumik",
            Tts::Murf => "murf",
            Tts::Sapi => "sapi",
        }
    }
}

/// The engine for these settings: the named one, else Rumik with a key, else Murf with a key,
/// else SAPI.
pub fn choose(setting: &str, has_rumik_key: bool, has_murf_key: bool) -> Tts {
    match setting.trim().to_ascii_lowercase().as_str() {
        "sapi" | "windows" => Tts::Sapi,
        "murf" => Tts::Murf,
        "rumik" => Tts::Rumik,
        // Murf first: measured first audio 0.13 s against about 1.8 s for Rumik, which returns one burst.
        _ if has_murf_key => Tts::Murf,
        _ if has_rumik_key => Tts::Rumik,
        _ => Tts::Sapi,
    }
}

fn has_key(name: &str) -> bool {
    std::env::var(name).is_ok_and(|k| !k.trim().is_empty())
}

pub fn engine() -> Tts {
    let setting = std::env::var("POINTER_TTS").unwrap_or_default();
    choose(&setting, has_key("RUMIK_API_KEY"), has_key("MURF_API_KEY"))
}

/// Split a reply into sentences to synthesise separately. Short pieces join the next one (a
/// clip of three words sounds clipped), and a long run with no full stop breaks at a comma.
pub fn split_sentences(text: &str) -> Vec<String> {
    const MIN: usize = 24;
    const MAX: usize = 240;
    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let boundary = match c {
            '.' | '!' | '?' | '\n' => chars.get(i + 1).map_or(true, |n| n.is_whitespace()),
            _ => false,
        };
        // "3.5" and "e.g." are not ends: a full stop must be followed by space and a capital or digit.
        let real = boundary
            && (c != '.' || chars.get(i + 2).map_or(true, |n| !n.is_lowercase()))
            && !cur.trim().is_empty();
        let long = cur.len() >= MAX && matches!(c, ',' | ';' | ':');
        if real || long {
            pieces.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        pieces.push(cur);
    }
    let mut out: Vec<String> = Vec::new();
    let mut carry = String::new();
    for p in pieces {
        carry.push_str(&p);
        if carry.trim().len() >= MIN {
            out.push(carry.trim().to_string());
            carry.clear();
        }
    }
    if !carry.trim().is_empty() {
        match out.last_mut() {
            Some(last) => {
                last.push(' ');
                last.push_str(carry.trim());
            }
            None => out.push(carry.trim().to_string()),
        }
    }
    out
}

/// Keep the cloud connection warm (called every so often while a conversation is open).
pub fn warm() {
    if !CONVERSATION.load(Ordering::SeqCst) {
        return;
    }
    match engine() {
        Tts::Rumik => Rumik::from_env().iter().for_each(Rumik::warm),
        Tts::Murf => Murf::from_env().iter().for_each(Murf::warm),
        Tts::Sapi => {}
    }
}

/// The flag of the line being spoken now; a new line cuts the old one off.
fn current() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    static CURRENT: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
    &CURRENT
}

/// True from the start of a real line to its end: an acknowledgement never starts over it.
static SPEAKING: AtomicUsize = AtomicUsize::new(0);

fn begin() -> Arc<AtomicBool> {
    let mine = Arc::new(AtomicBool::new(false));
    let mut slot = current().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(previous) = slot.replace(mine.clone()) {
        previous.store(true, Ordering::SeqCst);
    }
    cancel_ack();
    mine
}

/// Cut off whatever is being said, now.
pub fn stop() {
    if let Some(flag) = current().lock().unwrap_or_else(|p| p.into_inner()).take() {
        flag.store(true, Ordering::SeqCst);
    }
    cancel_ack();
    // The echo-cancelling window first: it is told now, not when the playing thread next looks.
    crate::voice::window::global().cut();
    speech::stop();
}

/// Play 16-bit mono chunks and block until heard or stopped. Through the echo-cancelling window
/// while the microphone is open there (Chromium then holds the playback as its echo reference);
/// otherwise, or when the window goes away before any sound, through this process's own device.
/// `on_start` runs when sound really begins.
fn play(
    rate: u32,
    rx: &Receiver<Vec<i16>>,
    stopped: &dyn Fn() -> bool,
    prebuffer_ms: u32,
    on_start: impl FnOnce(),
) -> Result<audio::PlayEnd, String> {
    use crate::voice::window::{self, Played};
    let mut on_start: Option<Box<dyn FnOnce() + '_>> = Some(Box::new(on_start));
    let link = window::global();
    let mut replay: Vec<i16> = Vec::new();
    if link.usable_for_playback() {
        match link.play(rate, rx, stopped, prebuffer_ms, &mut on_start) {
            Played::Done(end) => return Ok(end),
            Played::Lost(kept) => {
                crate::voice::line("voice: the echo-cancelling window closed; playing from the core");
                replay = kept;
            }
        }
    }
    let first = on_start.take();
    let start = move || first.map_or((), |f| f());
    if replay.is_empty() {
        return audio::play_stream(rate, rx, stopped, prebuffer_ms, start);
    }
    // What the window had been given, then the rest of the line (collected: this is the rare
    // path, and synthesis runs faster than it plays), into the core's player.
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => replay.extend(c),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) if stopped() => break,
            Err(_) => {}
        }
    }
    let (tx2, rx2) = channel::<Vec<i16>>();
    let _ = tx2.send(replay);
    drop(tx2);
    audio::play_stream(rate, &rx2, stopped, prebuffer_ms, start)
}

fn mark(action: &str, engine: Tts, cut: bool, first_audio_ms: Option<u128>) {
    let mut payload = json!({ "action": action, "real": true, "engine": engine.name(), "cut": cut });
    if let Some(ms) = first_audio_ms {
        payload["first_audio_ms"] = json!(ms as u64);
        if std::env::var("POINTER_VOICE_TRACE").is_ok_and(|v| v == "1") {
            eprintln!("[tts] {} first audio after {ms} ms", engine.name());
        }
    }
    crate::voice::emit("speech", payload);
}

/// Say `text`, blocking until the sound has really finished or was cut (`cancel` or `stop`).
/// True when something was heard. Falls back down the chain (Rumik, Murf, Windows) when a
/// service cannot be reached, and says so in a line.
pub fn speak_blocking(text: &str, cancel: &dyn Fn() -> bool) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let t0 = Instant::now();
    let flag = begin();
    let stopped = || flag.load(Ordering::SeqCst) || cancel();
    SPEAKING.fetch_add(1, Ordering::SeqCst);
    let heard = speak_chain(text, &stopped, t0);
    SPEAKING.fetch_sub(1, Ordering::SeqCst);
    // Only clear the slot if it is still ours.
    let mut slot = current().lock().unwrap_or_else(|p| p.into_inner());
    if slot.as_ref().is_some_and(|f| Arc::ptr_eq(f, &flag)) {
        *slot = None;
    }
    heard
}

fn speak_chain(text: &str, stopped: &dyn Fn() -> bool, t0: Instant) -> bool {
    let first = engine();
    let mut chain = if first == Tts::Sapi { vec![] } else { vec![first] };
    if first == Tts::Rumik && has_key("MURF_API_KEY") {
        chain.push(Tts::Murf);
    }
    for (i, e) in chain.iter().enumerate() {
        match cloud_speak(*e, text, stopped, t0) {
            Ok(heard) => return heard,
            Err(why) => {
                let next = chain.get(i + 1).map_or("the Windows voice", |n| n.name());
                crate::voice::line(&format!("voice: {why}; using {next} for this line"));
                if stopped() {
                    return false;
                }
            }
        }
    }
    !stopped() && sapi_speak(text, stopped, t0)
}

/// Say `text` without waiting (the quick-mode answers). True when a voice exists to try.
pub fn speak_async(text: &str) -> bool {
    let text = text.trim().to_string();
    if text.is_empty() {
        return false;
    }
    std::thread::Builder::new()
        .name("speak".into())
        .spawn(move || {
            speak_blocking(&text, &|| false);
        })
        .is_ok()
}

/// One sentence from the chosen service, chunk by chunk. `on_chunk` returns false to stop.
fn synth(engine: Tts, text: &str, on_chunk: &mut dyn FnMut(&[i16]) -> bool) -> Result<(), String> {
    match engine {
        Tts::Rumik => Rumik::from_env().ok_or("Rumik has no key")?.stream(text, on_chunk),
        Tts::Murf => Murf::from_env()
            .ok_or("Murf has no key")?
            .stream(text, &murf::voice_from_env(), on_chunk),
        Tts::Sapi => Err("the Windows voice is not streamed".into()),
    }
}

/// The reply as a pipeline: up to two sentences are being made at once, played in order with no
/// gap. Err when nothing could be played because the service failed.
fn cloud_speak(engine: Tts, text: &str, stopped: &dyn Fn() -> bool, t0: Instant) -> Result<bool, String> {
    if engine == Tts::Sapi {
        return Err("no cloud voice".into());
    }
    if stopped() {
        return Ok(false);
    }
    let parts = split_sentences(text);
    let (tx, rx) = channel::<Vec<i16>>();
    let halt = Arc::new(AtomicBool::new(false));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let (halt, failure) = (halt.clone(), failure.clone());
        std::thread::Builder::new()
            .name("tts-pipeline".into())
            .spawn(move || {
                let launch = |part: String| -> Receiver<Vec<i16>> {
                    let (stx, srx) = channel::<Vec<i16>>();
                    let (halt, failure) = (halt.clone(), failure.clone());
                    std::thread::spawn(move || {
                        let r = synth(engine, &part, &mut |c| {
                            !halt.load(Ordering::SeqCst) && stx.send(c.to_vec()).is_ok()
                        });
                        if let Err(e) = r {
                            failure.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(e);
                        }
                    });
                    srx
                };
                let mut queue: std::collections::VecDeque<Receiver<Vec<i16>>> = std::collections::VecDeque::new();
                let mut todo = parts.into_iter();
                for _ in 0..2 {
                    if let Some(p) = todo.next() {
                        queue.push_back(launch(p));
                    }
                }
                while let Some(srx) = queue.pop_front() {
                    if let Some(p) = todo.next() {
                        queue.push_back(launch(p));
                    }
                    while let Ok(chunk) = srx.recv() {
                        if halt.load(Ordering::SeqCst) || tx.send(chunk).is_err() {
                            return;
                        }
                    }
                }
                // `tx` drops here, which tells the player nothing more is coming.
            })
            .map_err(|e| format!("could not start the {} stream: {e}", engine.name()))?;
    }
    let started = AtomicBool::new(false);
    let played = play(CLOUD_RATE, &rx, stopped, PREBUFFER_MS, || {
        started.store(true, Ordering::SeqCst);
        mark("start", engine, false, Some(t0.elapsed().as_millis()));
    });
    halt.store(true, Ordering::SeqCst);
    let heard = started.load(Ordering::SeqCst);
    if heard {
        mark("end", engine, stopped(), None);
    }
    match played {
        Err(e) => Err(e),
        Ok(_) if !heard => {
            let why = failure.lock().unwrap_or_else(|p| p.into_inner()).take();
            match why {
                Some(e) => Err(e),
                None => Ok(false),
            }
        }
        Ok(_) => Ok(true),
    }
}

fn sapi_speak(text: &str, stopped: &dyn Fn() -> bool, t0: Instant) -> bool {
    if stopped() {
        return false;
    }
    if !speech::speak(&speech::speakable(text)) {
        return false;
    }
    let began = Instant::now();
    let mut started = false;
    loop {
        if stopped() {
            speech::stop();
            break;
        }
        let speaking = speech::is_speaking();
        if speaking && !started {
            started = true;
            mark("start", Tts::Sapi, false, Some(t0.elapsed().as_millis()));
        }
        if started && !speaking {
            break;
        }
        if !started && began.elapsed() > SAPI_START_WAIT {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if started {
        mark("end", Tts::Sapi, stopped(), None);
    }
    started
}

// ------------------------------------------------------------------------------------------
// Acknowledgement clips.

/// A conversation is open and the microphone is the talk kind: acknowledgements are on.
static CONVERSATION: AtomicBool = AtomicBool::new(false);
static ACK_CANCEL: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
static ACK_PLAYING: AtomicBool = AtomicBool::new(false);
static ACK_NEXT: AtomicUsize = AtomicUsize::new(0);

/// What the clips say. Plain words, one short breath each.
pub const ACK_LINES: [&str; 3] = ["Hmm.", "Okay.", "Let's see."];

/// A conversation opened or closed. Opening it makes any missing clips, once, in the background.
pub fn set_conversation(on: bool) {
    CONVERSATION.store(on, Ordering::SeqCst);
    if on {
        let _ = std::thread::Builder::new().name("ack-make".into()).spawn(|| {
            let _ = ensure_clips();
        });
    }
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

fn home() -> std::path::PathBuf {
    std::env::var("POINTER_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(Into::into)
        .or_else(|| std::env::var("LOCALAPPDATA").ok().map(|d| std::path::PathBuf::from(d).join("pointer")))
        .unwrap_or_else(std::env::temp_dir)
}

/// The folder of this voice's clips: a new voice or engine means new clips.
fn clip_dir(engine: Tts) -> std::path::PathBuf {
    let voice = match engine {
        Tts::Rumik => format!("{}/{}", rumik::model_from_env(), rumik::speaker_from_env()),
        Tts::Murf => murf::voice_from_env(),
        Tts::Sapi => String::new(),
    };
    home().join("voice").join("ack").join(format!("{}-{:08x}", engine.name(), fnv(&voice) as u32))
}

/// Make the clips that are missing, with the engine in use. Windows SAPI cannot be captured,
/// so on it there are no clips and no acknowledgement.
pub fn ensure_clips() -> Result<usize, String> {
    let engine = engine();
    if engine == Tts::Sapi {
        return Ok(0);
    }
    let dir = clip_dir(engine);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut made = 0;
    for (i, line) in ACK_LINES.iter().enumerate() {
        let path = dir.join(format!("{i}.wav"));
        if path.exists() {
            continue;
        }
        let mut samples: Vec<i16> = Vec::new();
        synth(engine, line, &mut |c| {
            samples.extend_from_slice(c);
            true
        })?;
        if samples.is_empty() {
            continue;
        }
        std::fs::write(&path, audio::to_wav(&samples, CLOUD_RATE)).map_err(|e| e.to_string())?;
        made += 1;
    }
    Ok(made)
}

/// Cut an acknowledgement short, now.
pub fn cancel_ack() {
    if let Some(flag) = ACK_CANCEL.lock().unwrap_or_else(|p| p.into_inner()).take() {
        flag.store(true, Ordering::SeqCst);
    }
}

/// A voice request just ended: play one clip, if a conversation is open, replies are not muted,
/// nothing is being said and no clip is already sounding.
pub fn ack_if_enabled() {
    if !CONVERSATION.load(Ordering::SeqCst)
        || SPEAKING.load(Ordering::SeqCst) > 0
        || ACK_PLAYING.swap(true, Ordering::SeqCst)
    {
        return;
    }
    if !wcore::config::speak_answers() {
        ACK_PLAYING.store(false, Ordering::SeqCst);
        return;
    }
    let n = ACK_NEXT.fetch_add(1, Ordering::SeqCst) % ACK_LINES.len();
    let path = clip_dir(engine()).join(format!("{n}.wav"));
    let Some((rate, samples)) = std::fs::read(&path).ok().and_then(|b| audio::parse_wav(&b)) else {
        ACK_PLAYING.store(false, Ordering::SeqCst);
        return;
    };
    let flag = Arc::new(AtomicBool::new(false));
    *ACK_CANCEL.lock().unwrap_or_else(|p| p.into_inner()) = Some(flag.clone());
    let spawned = std::thread::Builder::new().name("ack".into()).spawn(move || {
        let (tx, rx) = channel::<Vec<i16>>();
        for c in samples.chunks(2400) {
            let _ = tx.send(c.to_vec());
        }
        drop(tx);
        let t0 = Instant::now();
        let text = ACK_LINES[n];
        let stopped = || flag.load(Ordering::SeqCst) || SPEAKING.load(Ordering::SeqCst) > 0;
        let result = play(rate, &rx, &stopped, 0, || {
            // Into the shell's echo history, so the clip heard back through the mic is not a request.
            crate::voice::emit(
                "speech",
                json!({ "action": "start", "real": true, "ack": true, "text": text, "engine": engine().name(),
                        "first_audio_ms": t0.elapsed().as_millis() as u64 }),
            );
        });
        if result.is_ok() {
            crate::voice::emit("speech", json!({ "action": "end", "real": true, "ack": true, "cut": stopped() }));
        }
        ACK_PLAYING.store(false, Ordering::SeqCst);
    });
    if spawned.is_err() {
        ACK_PLAYING.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_voice_follows_the_keys_murf_then_rumik_then_windows() {
        assert_eq!(choose("", true, true), Tts::Murf);
        assert_eq!(choose("", true, false), Tts::Rumik);
        assert_eq!(choose("auto", false, true), Tts::Murf);
        assert_eq!(choose("", false, false), Tts::Sapi);
        assert_eq!(choose("sapi", true, true), Tts::Sapi);
        assert_eq!(choose(" MURF ", false, false), Tts::Murf);
        assert_eq!(choose("rumik", false, false), Tts::Rumik);
        assert_eq!(choose("windows", true, true), Tts::Sapi);
    }

    #[test]
    fn a_reply_splits_at_sentences_and_short_ones_join() {
        let p = split_sentences("Okay. I opened Calculator for you. It is on the left of your screen now! Anything else?");
        assert_eq!(p, vec!["Okay. I opened Calculator for you.", "It is on the left of your screen now! Anything else?"]);
        assert_eq!(split_sentences("Pi is 3.14 roughly, not more than that at all, really."), vec!["Pi is 3.14 roughly, not more than that at all, really."]);
        assert_eq!(split_sentences("Hi."), vec!["Hi."]);
        assert!(split_sentences("  ").is_empty());
        let joined = split_sentences("One sentence that is long enough. Two sentence that is long enough.").join(" ");
        assert_eq!(joined, "One sentence that is long enough. Two sentence that is long enough.");
    }

    #[test]
    fn a_new_line_cuts_the_old_one_and_stop_cuts_the_new_one() {
        let first = begin();
        let second = begin();
        assert!(first.load(Ordering::SeqCst));
        assert!(!second.load(Ordering::SeqCst));
        stop();
        assert!(second.load(Ordering::SeqCst));
    }

    #[test]
    fn a_real_line_cancels_an_acknowledgement_and_no_ack_plays_outside_a_conversation() {
        let flag = Arc::new(AtomicBool::new(false));
        *ACK_CANCEL.lock().unwrap() = Some(flag.clone());
        let _line = begin();
        assert!(flag.load(Ordering::SeqCst));
        CONVERSATION.store(false, Ordering::SeqCst);
        ack_if_enabled();
        assert!(!ACK_PLAYING.load(Ordering::SeqCst));
    }

    #[test]
    fn empty_text_is_not_spoken() {
        assert!(!speak_blocking("   ", &|| false));
        assert!(!speak_async(""));
    }

    #[test]
    fn a_line_cancelled_before_it_starts_is_not_heard() {
        // The cancel is already set, so no engine may report sound.
        std::env::set_var("POINTER_TTS", "sapi");
        assert!(!speak_blocking("hello there", &|| true));
        std::env::remove_var("POINTER_TTS");
    }

    #[test]
    fn clip_folders_differ_by_engine_and_voice() {
        let a = clip_dir(Tts::Murf);
        let b = clip_dir(Tts::Rumik);
        assert_ne!(a, b);
    }
}
