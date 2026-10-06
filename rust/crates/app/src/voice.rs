//! The microphone: recorded on a worker thread, transcribed by OpenAI with a vocabulary prompt.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use platform::audio;

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
}

type Recording = (Arc<AtomicBool>, JoinHandle<Result<Vec<i16>, String>>);

#[derive(Default)]
pub struct Microphone {
    active: Mutex<Option<Recording>>,
}

fn transcribe(samples: &[i16]) -> Result<String, String> {
    if samples.is_empty() || audio::is_silent(samples, audio::SILENCE_LEVEL) {
        return Ok(String::new());
    }
    let client = api::openai::OpenAi::from_env()
        .ok_or_else(|| "voice needs OPENAI_API_KEY (Settings)".to_string())?;
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
        let max = wcore::config::voice_max_seconds()?;
        let silence = wcore::config::bar_silence_seconds()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("record".into())
            .spawn(move || audio::record_until(|| flag.load(Ordering::SeqCst), max, silence))
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
        let samples = handle
            .join()
            .map_err(|_| "the recorder crashed".to_string())??;
        transcribe(&samples)
    }

    fn while_held(&self, vk: u32) -> Result<String, String> {
        let samples = audio::record_while_held(vk, wcore::config::voice_max_seconds()?)?;
        transcribe(&samples)
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
    fn silence_is_heard_as_nothing_without_calling_the_api() {
        assert_eq!(transcribe(&[0i16; 16_000]).unwrap(), "");
        assert_eq!(transcribe(&[]).unwrap(), "");
    }
}
