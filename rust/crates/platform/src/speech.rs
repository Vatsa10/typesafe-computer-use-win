//! Speech out: say an answer out loud instead of only printing it. Port of `speech.py` (the SAPI
//! engine; the optional Murf engine is not ported).
//!
//! The engine is the Windows speech API, `SpVoice` over COM (`ISpVoice`): offline, instant and
//! already installed. It is created once and reused, because a per-call object would reopen the
//! audio device for every sentence and lose the handle `stop` needs. A COM object belongs to the
//! thread that made it, so the one voice lives on one dedicated thread and every public function
//! sends that thread a request. A failure to create it is remembered, so a machine without
//! speech does not pay for a doomed COM call on every answer.
//!
//! Speaking is asynchronous by SAPI's own flag: `Speak` returns at once and the voice runs on its
//! own. Every call purges whatever was queued, so a new answer replaces the old one instead of
//! talking over it, and `stop` purges with an empty utterance, which cuts the current word off.
//! Every failure returns false or empty; nothing here panics into a run.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::Speech::{
    ISpObjectTokenCategory, ISpVoice, SVSFPurgeBeforeSpeak, SVSFlagsAsync, SpObjectTokenCategory,
    SpVoice, SPCAT_VOICES,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// `SVSFlagsAsync | SVSFPurgeBeforeSpeak`: return at once, and drop whatever was being said.
const ASYNC_PURGE: u32 = (SVSFlagsAsync.0 | SVSFPurgeBeforeSpeak.0) as u32;

/// The cap on a spoken answer, in characters. Measured, not guessed: the runs on this machine held
/// answers of 88, 232, 382, 405, 458 and 480 characters. Read aloud at the default SAPI rate the
/// long ones are close to forty seconds, far past the point where a listener reads the panel
/// instead. 240 is the first two sentences of a typical answer, about fifteen seconds, and it
/// leaves the short answers (the 88 one) untouched.
pub const SPEAK_LIMIT: usize = 240;

enum Request {
    Speak(String, Sender<bool>),
    Voices(Sender<Vec<String>>),
}

/// The speech thread's mailbox, or None when the voice could not be created.
fn engine() -> Option<&'static Mutex<Sender<Request>>> {
    static ENGINE: OnceLock<Option<Mutex<Sender<Request>>>> = OnceLock::new();
    ENGINE.get_or_init(start).as_ref()
}

fn start() -> Option<Mutex<Sender<Request>>> {
    let (tx, rx) = channel::<Request>();
    let (ready_tx, ready_rx) = channel::<bool>();
    let spawned = std::thread::Builder::new()
        .name("sapi-voice".into())
        .spawn(move || {
            // SAFETY: COM init on this thread, then one object owned and used only here.
            let voice: Option<ISpVoice> = unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                CoCreateInstance(&SpVoice, None, CLSCTX_ALL).ok()
            };
            let _ = ready_tx.send(voice.is_some());
            let Some(voice) = voice else { return };
            for request in rx {
                match request {
                    Request::Speak(text, reply) => {
                        let wide = HSTRING::from(text.as_str());
                        // SAFETY: wide outlives the call; SAPI copies async text before returning.
                        let ok = unsafe { voice.Speak(&wide, ASYNC_PURGE, None) }.is_ok();
                        let _ = reply.send(ok);
                    }
                    Request::Voices(reply) => {
                        let _ = reply.send(list_voices().unwrap_or_default());
                    }
                }
            }
        });
    if spawned.is_err() || !ready_rx.recv().unwrap_or(false) {
        return None;
    }
    Some(Mutex::new(tx))
}

/// Installed voice descriptions, read from the SAPI voice token category. Speech thread only.
fn list_voices() -> windows::core::Result<Vec<String>> {
    // SAFETY: COM calls on an initialised thread; each returned string is freed after copying.
    unsafe {
        let category: ISpObjectTokenCategory =
            CoCreateInstance(&SpObjectTokenCategory, None, CLSCTX_ALL)?;
        category.SetId(SPCAT_VOICES, false)?;
        let tokens = category.EnumTokens(PCWSTR::null(), PCWSTR::null())?;
        let mut count = 0u32;
        tokens.GetCount(&mut count)?;
        let mut names = Vec::new();
        for i in 0..count {
            let token = tokens.Item(i)?;
            // The default (unnamed) value of a voice token is its description.
            let raw = token.GetStringValue(PCWSTR::null())?;
            names.push(raw.to_string().unwrap_or_default());
            CoTaskMemFree(Some(raw.0 as *const _));
        }
        Ok(names)
    }
}

fn ask<T>(make: impl FnOnce(Sender<T>) -> Request) -> Option<T> {
    let mailbox = engine()?;
    let (tx, rx) = channel();
    mailbox.lock().ok()?.send(make(tx)).ok()?;
    rx.recv().ok()
}

/// True when something on this machine can speak.
pub fn available() -> bool {
    engine().is_some()
}

/// The names of the installed voices, or empty when there is no speech at all.
pub fn voices() -> Vec<String> {
    ask(Request::Voices).unwrap_or_default()
}

/// Say `text` out loud, asynchronously, replacing whatever is being said. False when nothing
/// could speak it (empty text, no engine, a device that went away); never panics.
pub fn speak(text: &str) -> bool {
    let spoken = text.trim();
    if spoken.is_empty() {
        return false;
    }
    let spoken = spoken.to_string();
    ask(|reply| Request::Speak(spoken, reply)).unwrap_or(false)
}

/// Cut off whatever is being said. Safe when nothing is speaking and with no engine. Returns
/// whether the purge reached the engine.
pub fn stop() -> bool {
    // An empty utterance with the purge flag: the queue is dropped and the current word ends.
    ask(|reply| Request::Speak(String::new(), reply)).unwrap_or(false)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn starts_with_ci(chars: &[char], at: usize, prefix: &str) -> bool {
    let p: Vec<char> = prefix.chars().collect();
    chars.len() >= at + p.len()
        && chars[at..at + p.len()]
            .iter()
            .zip(&p)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Length of a URL starting at `at` (`https?://` or `www.` then at least one non-space).
fn url_at(chars: &[char], at: usize) -> Option<usize> {
    if at > 0 && is_word(chars[at - 1]) {
        return None;
    }
    let prefix = ["https://", "http://", "www."]
        .into_iter()
        .find(|p| starts_with_ci(chars, at, p))?
        .len();
    let rest = chars[at + prefix..]
        .iter()
        .take_while(|c| !c.is_whitespace())
        .count();
    (rest > 0).then_some(prefix + rest)
}

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Length of a path starting at `at`: a drive, a UNC share, `./`, `../`, `~/` or a bare `/`,
/// then everything up to whitespace or a quote/angle/pipe. Deliberately narrow, so prose with a
/// slash ("and/or") survives.
fn path_at(chars: &[char], at: usize) -> Option<usize> {
    if at > 0 && (is_word(chars[at - 1]) || chars[at - 1] == '.') {
        return None;
    }
    let get = |i: usize| chars.get(at + i).copied();
    let prefix = match (get(0), get(1), get(2)) {
        (Some(d), Some(':'), Some(s)) if d.is_ascii_alphabetic() && is_sep(s) => 3,
        (Some('\\'), Some('\\'), _) => 2,
        (Some('.'), Some('.'), Some(s)) if is_sep(s) => 3,
        (Some('.'), Some(s), _) if is_sep(s) => 2,
        (Some('~'), Some(s), _) if is_sep(s) => 2,
        (Some('/'), _, _) => 1,
        _ => return None,
    };
    let rest = chars[at + prefix..]
        .iter()
        .take_while(|&&c| !(c.is_whitespace() || "\"'<>|".contains(c)))
        .count();
    Some(prefix + rest)
}

/// The host of a URL, without the scheme, a leading www. or anything after it.
fn host(url: &str) -> String {
    let mut body = url;
    for scheme in ["https://", "http://"] {
        if body.len() >= scheme.len() && body[..scheme.len()].eq_ignore_ascii_case(scheme) {
            body = &body[scheme.len()..];
            break;
        }
    }
    if body.len() >= 4 && body[..4].eq_ignore_ascii_case("www.") {
        body = &body[4..];
    }
    let h = body.split(['/', '?', '#']).next().unwrap_or("");
    let h = h.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
    if h.is_empty() {
        url.to_string()
    } else {
        h.to_string()
    }
}

/// The final component of a path, which is the only part worth hearing.
fn last_part(path: &str) -> String {
    let tail = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    if tail.is_empty() {
        path.to_string()
    } else {
        tail.to_string()
    }
}

/// Replace every match of `find` with `with(matched text)`, scanning left to right.
fn substitute(
    text: &str,
    find: fn(&[char], usize) -> Option<usize>,
    with: fn(&str) -> String,
) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match find(&chars, i) {
            Some(n) if n > 0 => {
                let matched: String = chars[i..i + n].iter().collect();
                out.push_str(&with(&matched));
                i += n;
            }
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// `text` with what should not be read aloud taken out, capped at a sentence boundary.
///
/// A URL comes down to its host and a path to its last part, because letter-by-letter run folders
/// and query strings are unbearable out loud, and the whole thing is cut at the last sentence that
/// fits in [`SPEAK_LIMIT`] characters so the voice stops on a full stop instead of mid-word.
pub fn speakable(text: &str) -> String {
    speakable_within(text, SPEAK_LIMIT)
}

pub fn speakable_within(text: &str, limit: usize) -> String {
    let trimmed = substitute(text, url_at, host);
    let trimmed = substitute(&trimmed, path_at, last_part);
    let trimmed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= limit {
        return trimmed;
    }
    let head = &chars[..limit + 1];
    let last_end = (0..head.len())
        .filter(|&i| {
            matches!(head[i], '.' | '!' | '?') && head.get(i + 1).is_none_or(|c| c.is_whitespace())
        })
        .map(|i| i + 1)
        .next_back();
    let collect = |s: &[char]| s.iter().collect::<String>().trim().to_string();
    if let Some(end) = last_end.filter(|&e| e >= limit / 3) {
        return collect(&head[..end]);
    }
    match head.iter().rposition(|&c| c == ' ') {
        Some(cut) if cut > 0 => collect(&head[..cut]),
        _ => collect(&chars[..limit]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_become_hosts() {
        assert_eq!(
            speakable("See https://www.example.com/a/b?q=1 for more."),
            "See example.com for more."
        );
        assert_eq!(speakable("Go to www.github.com."), "Go to github.com");
        assert_eq!(speakable("HTTP://Docs.rs/x"), "Docs.rs");
        assert_eq!(speakable("at http://x.org)"), "at x.org");
    }

    #[test]
    fn paths_become_last_part() {
        assert_eq!(
            speakable(r"Saved to C:\Users\me\runs\2026-10-06\report.md today"),
            "Saved to report.md today"
        );
        assert_eq!(speakable("open /usr/local/bin/tool now"), "open tool now");
        assert_eq!(speakable(r"share \\server\docs\plan.txt"), "share plan.txt");
        assert_eq!(speakable("./src/main.rs and ../x/y.py"), "main.rs and y.py");
        assert_eq!(speakable("~/notes/todo.txt"), "todo.txt");
        assert_eq!(speakable("D:/dir/"), "dir");
    }

    #[test]
    fn prose_with_a_slash_survives() {
        assert_eq!(
            speakable("yes and/or no, 1/2 done"),
            "yes and/or no, 1/2 done"
        );
        assert_eq!(speakable("  many   spaces\n\there "), "many spaces here");
    }

    #[test]
    fn short_answers_untouched() {
        let s = "A".repeat(88);
        assert_eq!(speakable(&s), s);
        assert_eq!(speakable(""), "");
    }

    #[test]
    fn cap_at_sentence_boundary() {
        let s1 = format!("{}.", "a ".repeat(60).trim()); // 120 chars
        let s2 = format!("{}!", "b ".repeat(50).trim()); // 100 chars
        let s3 = format!("{}.", "c ".repeat(60).trim());
        let text = format!("{s1} {s2} {s3}");
        assert_eq!(speakable(&text), format!("{s1} {s2}"));
        assert!(speakable(&text).chars().count() <= SPEAK_LIMIT);
    }

    #[test]
    fn sentence_ending_exactly_at_the_cap_counts() {
        let s1 = format!("{}.", "x".repeat(SPEAK_LIMIT - 1));
        let text = format!("{s1} more words");
        assert_eq!(speakable(&text), s1);
    }

    #[test]
    fn no_early_sentence_falls_back_to_word_boundary() {
        // The only full stop is before limit/3, so cut at the last space instead.
        let text = format!("Hi. {}", "word ".repeat(80));
        let out = speakable(&text);
        assert!(out.chars().count() <= SPEAK_LIMIT);
        assert!(out.ends_with("word") && out.len() > 200, "{out}");
        // No space at all: hard cut at the limit.
        let blob = "z".repeat(500);
        assert_eq!(speakable(&blob), "z".repeat(SPEAK_LIMIT));
    }

    #[test]
    fn decimal_point_is_not_a_sentence_end() {
        let text = format!("Version 3.5 is out {}", "y ".repeat(200));
        let out = speakable(&text);
        assert!(!out.ends_with("3."), "{out}");
    }

    #[test]
    fn host_and_last_part_edges() {
        assert_eq!(host("https://"), "https://");
        assert_eq!(last_part("/"), "/");
    }

    /// Lists SAPI voices. Never speaks. `cargo test -p platform speech -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_list_voices() {
        println!("available: {}", available());
        let v = voices();
        for name in &v {
            println!("voice: {name}");
        }
        assert!(!v.is_empty());
    }
}
