//! Free, keyless speech-to-text: the recognizer built into Windows
//! (`Windows.Media.SpeechRecognition`).
//!
//! Why live recognition and not "transcribe the samples we already recorded": the WinRT
//! `SpeechRecognizer` has no public way to take audio from a buffer or a stream. It always listens
//! to the default capture device (the `SetInputToWaveStream` of the old SAPI is not projected into
//! WinRT, and a custom audio input is not public). So this engine records *and* recognizes in one
//! go: a `ContinuousRecognitionSession` on the default microphone, run until the caller says stop
//! or the longest utterance passes, collecting every `ResultGenerated` phrase.
//!
//! Constraints: the dictation topic, plus a list constraint naming the words the free-form
//! dictation gets wrong ("Claude", not "cloud"). Windows may refuse to compile a topic together
//! with a list; then dictation alone is used, since the list is only a hint.
//!
//! Dictation needs Settings > Privacy & security > Speech > "Online speech recognition" turned on
//! and an installed speech language; without them `available()` is false and `recognize_live`
//! says which of the two to fix.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Foundation::Collections::IIterable;
use windows::Foundation::TypedEventHandler;
use windows::Media::SpeechRecognition::{
    SpeechContinuousRecognitionResultGeneratedEventArgs, SpeechContinuousRecognitionSession,
    SpeechRecognitionConfidence, SpeechRecognitionListConstraint, SpeechRecognitionResultStatus,
    SpeechRecognitionScenario, SpeechRecognitionTopicConstraint, SpeechRecognizer,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// Words dictation mishears, offered as a list constraint alongside it.
pub const PHRASES: &[&str] = &["Claude", "Claude Code", "Pointer", "TypeSafe"];

/// HRESULT for "the speech privacy policy was not accepted" (online speech recognition off).
const PRIVACY_DECLINED: i32 = 0x8004_5509_u32 as i32;

pub const PRIVACY_HINT: &str = "Windows speech recognition is not available: turn on Settings > Privacy & security > Speech > Online speech recognition";
pub const LANGUAGE_HINT: &str = "Windows speech recognition is not available: install a speech language in Settings > Time & language > Speech";

fn init() {
    // Already-initialized (either model) is fine: WinRT works on any apartment here.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

fn explain(e: windows::core::Error) -> String {
    if e.code().0 == PRIVACY_DECLINED {
        PRIVACY_HINT.to_string()
    } else {
        format!("Windows speech recognition failed: {e}")
    }
}

/// A recognizer with dictation (and the phrase list when Windows accepts it) compiled.
fn recognizer() -> Result<SpeechRecognizer, String> {
    init();
    if SpeechRecognizer::SystemSpeechLanguage().is_err() {
        return Err(LANGUAGE_HINT.into());
    }
    let compile = |with_list: bool| -> Result<SpeechRecognizer, String> {
        let r = SpeechRecognizer::new().map_err(|_| LANGUAGE_HINT.to_string())?;
        let c = r.Constraints().map_err(explain)?;
        let topic = SpeechRecognitionTopicConstraint::Create(
            SpeechRecognitionScenario::Dictation,
            &HSTRING::from("dictation"),
        )
        .map_err(explain)?;
        c.Append(&topic).map_err(explain)?;
        if with_list {
            let words: Vec<HSTRING> = PHRASES.iter().map(|p| HSTRING::from(*p)).collect();
            let words = IIterable::<HSTRING>::try_from(words).map_err(explain)?;
            let list = SpeechRecognitionListConstraint::Create(&words).map_err(explain)?;
            c.Append(&list).map_err(explain)?;
        }
        let result = r
            .CompileConstraintsAsync()
            .and_then(|op| op.get())
            .map_err(explain)?;
        match result.Status().map_err(explain)? {
            SpeechRecognitionResultStatus::Success => Ok(r),
            SpeechRecognitionResultStatus::TopicLanguageNotSupported => Err(LANGUAGE_HINT.into()),
            SpeechRecognitionResultStatus::NetworkFailure => Err(PRIVACY_HINT.into()),
            other => Err(format!(
                "Windows speech recognition could not compile its grammar ({other:?})"
            )),
        }
    };
    compile(true).or_else(|_| compile(false))
}

/// Whether the recognizer can be created with a speech language and dictation compiles.
pub fn available() -> bool {
    recognizer().is_ok()
}

/// Why `available()` is false, or None when it is true.
pub fn unavailable_reason() -> Option<String> {
    recognizer().err()
}

/// Listen to the default microphone until `stop()` is true, `max_seconds` pass, or (when
/// `silence_seconds > 0`) that long passes after the last phrase heard, and return everything heard
/// (trimmed; "" for nothing). Low-confidence (rejected) phrases are dropped.
pub fn recognize_live(
    stop: impl Fn() -> bool,
    max_seconds: f64,
    silence_seconds: f64,
) -> Result<String, String> {
    let recognizer = recognizer()?;
    let session: SpeechContinuousRecognitionSession =
        recognizer.ContinuousRecognitionSession().map_err(explain)?;
    let heard = Arc::new(Mutex::new((Vec::<String>::new(), None::<Instant>)));
    let sink = heard.clone();
    let token = session
        .ResultGenerated(&TypedEventHandler::new(
            move |_, args: &Option<SpeechContinuousRecognitionResultGeneratedEventArgs>| {
                if let Some(result) = args.as_ref().and_then(|a| a.Result().ok()) {
                    let rejected =
                        result.Confidence().ok() == Some(SpeechRecognitionConfidence::Rejected);
                    if let (false, Ok(text)) = (rejected, result.Text()) {
                        let text = text.to_string();
                        if !text.trim().is_empty() {
                            if let Ok(mut h) = sink.lock() {
                                h.0.push(text.trim().to_string());
                                h.1 = Some(Instant::now());
                            }
                        }
                    }
                }
                Ok(())
            },
        ))
        .map_err(explain)?;
    session
        .StartAsync()
        .and_then(|a| a.get())
        .map_err(explain)?;
    let deadline = Instant::now() + Duration::from_secs_f64(max_seconds.max(0.5));
    let quiet = |h: &Arc<Mutex<(Vec<String>, Option<Instant>)>>| {
        silence_seconds > 0.0
            && h.lock()
                .ok()
                .and_then(|h| h.1)
                .is_some_and(|t| t.elapsed() >= Duration::from_secs_f64(silence_seconds))
    };
    while !stop() && Instant::now() < deadline && !quiet(&heard) {
        std::thread::sleep(Duration::from_millis(30));
    }
    // StopAsync finishes the phrase in flight and raises its ResultGenerated before completing.
    let stopped = session.StopAsync().and_then(|a| a.get());
    let _ = session.RemoveResultGenerated(token);
    stopped.map_err(explain)?;
    let text = heard.lock().map(|h| h.0.join(" ")).unwrap_or_default();
    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phrase_list_names_claude() {
        assert!(PHRASES.contains(&"Claude") && PHRASES.contains(&"Claude Code"));
    }

    #[test]
    fn availability_is_answered_without_panicking() {
        let ok = available();
        assert_eq!(ok, unavailable_reason().is_none());
        eprintln!(
            "windows speech available: {ok} ({:?})",
            unavailable_reason()
        );
    }

    /// Speak for about four seconds. Run by hand: cargo test -p platform stt -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_recognizes_four_seconds_from_the_microphone() {
        let text = recognize_live(|| false, 4.0, 0.0).unwrap();
        println!("heard: {text:?}");
    }
}
