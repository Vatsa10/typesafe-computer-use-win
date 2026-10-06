//! The microphone, on one of three engines (`CLICKER_STT`): Chrome (the default: Google's free
//! recognizer in the user's own Chrome or Edge, which the Electron shell drives - this core never
//! records on it), OpenAI (recorded on a worker thread, transcribed with a vocabulary prompt) or the
//! free Windows recognizer (`platform::stt`), which records and recognizes in one go because WinRT
//! cannot be handed recorded audio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use platform::{audio, stt};

/// The decoding bias, the same sentence the Python uses, so "Claude Code" is not heard as "cloud
/// code". Short on purpose: a long prompt gets echoed back as if it had been spoken.
pub const VOICE_PROMPT: &str = "Claude Code, VS Code, Chrome, WhatsApp, terminal, GitHub: say which monitor or window to use, scroll it, and stop, pause, resume or quit.";

/// `CLICKER_VOICE_PROMPT` overrides the prompt; set to empty it disables it.
pub fn voice_prompt() -> Option<String> {
    let prompt = std::env::var("CLICKER_VOICE_PROMPT").unwrap_or_else(|_| VOICE_PROMPT.to_string());
    (!prompt.trim().is_empty()).then_some(prompt)
}

/// Start/stop listening (the bar and the panel) and push-to-talk (the hotkey).
pub trait Listener: Send + Sync {
    /// Begin recording in the background. Ok(false) when one is already going.
    fn start(&self) -> Result<bool, String>;
    /// Stop, transcribe, return what was heard ("" for nothing).
    fn stop(&self) -> Result<String, String>;
    /// Record while `vk` is held, then transcribe.
    fn while_held(&self, vk: u32) -> Result<String, String>;
    /// The engine's name: "chrome", "openai" or "windows". On "chrome" the shell records.
    fn engine(&self) -> String;
    /// Block while `vk` is held (bounded by the longest utterance). Push-to-talk on the chrome
    /// engine: the shell records between the start and stop this brackets.
    fn hold(&self, vk: u32);
}

/// The line `listen_start`/`listen_stop` give when the core is asked to record on Chrome.
pub const IN_SHELL: &str = "the chrome speech engine runs in the shell, not the core";

type Recording = (Arc<AtomicBool>, JoinHandle<Result<String, String>>);

/// Which engine `CLICKER_STT` selects, as a typed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Chrome,
    OpenAi,
    Windows,
}

impl Engine {
    pub fn from_name(name: &str) -> Engine {
        match name {
            "windows" => Engine::Windows,
            "openai" => Engine::OpenAi,
            _ => Engine::Chrome,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Chrome => "chrome",
            Engine::OpenAi => "openai",
            Engine::Windows => "windows",
        }
    }

    pub fn current() -> Result<Engine, String> {
        wcore::config::stt_engine().map(Engine::from_name)
    }

    /// A clear line when this engine cannot run here, checked before recording starts.
    fn check(self) -> Result<(), String> {
        match self {
            Engine::Windows => stt::unavailable_reason().map_or(Ok(()), Err),
            Engine::OpenAi => Ok(()),
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

impl Listener for Microphone {
    fn start(&self) -> Result<bool, String> {
        let mut active = self.active.lock().map_err(|_| "the recorder is wedged")?;
        if active.as_ref().is_some_and(|(_, h)| !h.is_finished()) {
            return Ok(false);
        }
        let engine = Engine::current()?;
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
                    Engine::Chrome => Err(IN_SHELL.into()),
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
            Engine::Chrome => Err(IN_SHELL.into()),
        }
    }

    fn engine(&self) -> String {
        Engine::current().map_or_else(|_| "chrome".into(), |e| e.name().into())
    }

    fn hold(&self, vk: u32) {
        let max = wcore::config::voice_max_seconds().unwrap_or(30.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(max.max(1.0));
        while audio::key_held(vk) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_claude_so_it_is_not_heard_as_cloud() {
        assert!(VOICE_PROMPT.starts_with("Claude Code"));
    }

    #[test]
    fn the_engine_name_maps_to_the_engine() {
        assert_eq!(Engine::from_name("windows"), Engine::Windows);
        assert_eq!(Engine::from_name("openai"), Engine::OpenAi);
        assert_eq!(Engine::from_name("chrome"), Engine::Chrome);
        assert_eq!(
            Engine::from_name(wcore::config::choose_stt("auto").unwrap()),
            Engine::Chrome
        );
        for e in [Engine::Chrome, Engine::OpenAi, Engine::Windows] {
            assert_eq!(Engine::from_name(e.name()), e);
        }
    }

    #[test]
    fn silence_is_heard_as_nothing_without_calling_the_api() {
        assert_eq!(transcribe(&[0i16; 16_000]).unwrap(), "");
        assert_eq!(transcribe(&[]).unwrap(), "");
    }
}
