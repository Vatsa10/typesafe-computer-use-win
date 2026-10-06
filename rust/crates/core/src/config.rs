//! Tunables, the site catalog, and environment loading.
//!
//! A port of `config.py` and `dotenv_io.py`. Every file read and write here is UTF-8, explicitly:
//! the Windows default is cp1252 and has broken this project twice.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;

pub const MIN_OCR_CONFIDENCE: f64 = 0.3;
pub const MAX_OPTIONS: usize = 255; // TypeSafe Choice ceiling
pub const ABORT_CORNER_PX: i32 = 4;
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.4;
pub const DEFAULT_STEPS: u32 = 100;
pub const DEFAULT_DELAY: f64 = 2.0;
pub const DEFAULT_WRITER_MODEL: &str = "claude-haiku-4-5";
pub const DEFAULT_ANSWER_MODEL: &str = "claude-sonnet-5"; // runs once per run, on a screenshot: worth a stronger reader
                                                          // The same two jobs on an OpenAI-compatible endpoint. Both read a screenshot for the answer, so both
                                                          // have to be vision models. Override them when an account or a gateway offers something better.
pub const DEFAULT_OPENAI_WRITER_MODEL: &str = "gpt-4.1-mini";
pub const DEFAULT_OPENAI_ANSWER_MODEL: &str = "gpt-4.1";
pub const DEFAULT_BROWSER: &str = "Google Chrome";

/// The curated core of the site catalog. These are always offered, whatever the browser holds, and
/// they outrank anything discovered. Everything else is learned: see catalog.rs.
///
/// In the Python dict's order, which is the order they are offered in.
pub const SITES: [(&str, &str); 8] = [
    ("github", "https://github.com/"),
    ("gmail", "https://mail.google.com/"),
    ("google_calendar", "https://calendar.google.com/"),
    ("launchdarkly", "https://app.launchdarkly.com/"),
    ("linear", "https://linear.app/"),
    ("notion", "https://www.notion.so/"),
    ("slack", "https://app.slack.com/"),
    ("typesafe_console", "https://console.typesafe.ai/"),
];

/// Python's `str.splitlines()`: every line boundary it knows, not just `\n`. A lone `\r`, a form
/// feed and the Unicode separators all end a line there, so they end one here.
fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&text[start..i]);
        let mut end = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                chars.next();
                end = j + 1;
            }
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The whole file as UTF-8, never the ANSI code page. Invalid UTF-8 is an error, as in the Python.
fn read_utf8(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Set KEY=VALUE lines from a .env file into the environment unless already set.
///
/// A missing file is not an error. A file that is not UTF-8 is, as it is in the Python.
pub fn load_dotenv(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    for (key, value) in parse_lines(&read_utf8(path)?) {
        // setdefault: an existing variable always wins, even an empty one. An empty key cannot be
        // set on Windows at all, so it is skipped rather than allowed to panic.
        if !key.is_empty() && env::var_os(&key).is_none() {
            env::set_var(key, value);
        }
    }
    Ok(())
}

fn parse_lines(text: &str) -> Vec<(String, String)> {
    splitlines(text)
        .into_iter()
        .filter_map(split_line)
        .collect()
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Python's `float()` on a setting: surrounding whitespace allowed, and anything unparseable is an
/// error the caller sees, not a silent default.
fn env_float(name: &str, default: f64) -> Result<f64, String> {
    match env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("could not convert string to float: {raw:?} ({name})")),
        Err(_) => Ok(default),
    }
}

/// On unless set to one of the usual words for off.
fn env_flag(name: &str) -> bool {
    let value = env_or(name, "1").trim().to_lowercase();
    !matches!(value.as_str(), "0" | "false" | "no" | "off")
}

pub fn browser() -> String {
    env_or("CLICKER_BROWSER", DEFAULT_BROWSER)
}

/// Which service writes free text: "anthropic" or "openai".
///
/// Set CLICKER_WRITER_PROVIDER to force one. Otherwise an OpenAI key wins, including when both
/// are present, so swapping provider is a matter of which key is in .env rather than a code
/// change. Anthropic remains fully supported and is used whenever it is the only key set.
pub fn writer_provider() -> &'static str {
    let explicit = env_or("CLICKER_WRITER_PROVIDER", "").trim().to_lowercase();
    match explicit.as_str() {
        "anthropic" => return "anthropic",
        "openai" => return "openai",
        _ => {}
    }
    if env::var("OPENAI_API_KEY").is_ok_and(|v| !v.is_empty()) {
        return "openai";
    }
    "anthropic"
}

/// An OpenAI-compatible endpoint other than OpenAI's own: Groq, OpenRouter, or a local server.
///
/// The API shape is the same, so pointing this somewhere else is the whole configuration.
pub fn openai_base_url() -> Option<String> {
    let url = env_or("OPENAI_BASE_URL", "").trim().to_string();
    (!url.is_empty()).then_some(url)
}

pub fn writer_model() -> String {
    let default = if writer_provider() == "openai" {
        DEFAULT_OPENAI_WRITER_MODEL
    } else {
        DEFAULT_WRITER_MODEL
    };
    env_or("CLICKER_WRITER_MODEL", default)
}

pub fn answer_model() -> String {
    let default = if writer_provider() == "openai" {
        DEFAULT_OPENAI_ANSWER_MODEL
    } else {
        DEFAULT_ANSWER_MODEL
    };
    env_or("CLICKER_ANSWER_MODEL", default)
}

pub fn email() -> Option<String> {
    env::var("CLICKER_EMAIL").ok().filter(|v| !v.is_empty())
}

/// The daemon's global hotkeys. Ctrl+Alt is the least contested corner of the Windows keyboard:
/// Win+key belongs to the shell, and Ctrl+Shift+key to whatever app has focus.
pub const DEFAULT_HOTKEYS: [(&str, &str); 6] = [
    ("talk", "ctrl+alt+space"),
    // The bar is the one most people will use: press, speak, done. Right alt is free on a standard
    // layout, so it needs no chord; on a layout where it is AltGr, change it in Settings.
    ("bar", "rightalt"),
    ("goal", "ctrl+alt+g"),
    ("pause", "ctrl+alt+p"),
    ("abort", "ctrl+alt+x"),
    ("quit", "ctrl+alt+q"),
];
pub const DEFAULT_WHISPER_MODEL: &str = "base.en";
/// How long a pause ends a recording in the command bar. The bar records until clicked again, and
/// people do not click again, so a pause stops it. Short enough not to sit there, long enough to
/// survive thinking mid-sentence.
pub const DEFAULT_BAR_SILENCE_SECONDS: f64 = 1.6;
pub const DEFAULT_VOICE_MAX_SECONDS: f64 = 30.0;
pub const DEFAULT_VOICE_MIN_CONFIDENCE: f64 = 0.55;

/// The hotkey for one daemon action, from `CLICKER_HOTKEY_<NAME>` or its default. `None` for a
/// name with no default, which in the Python is a KeyError whatever the environment holds.
pub fn hotkey(name: &str) -> Option<String> {
    let default = DEFAULT_HOTKEYS.iter().find(|(n, _)| *n == name)?.1;
    Some(env_or(
        &format!("CLICKER_HOTKEY_{}", name.to_uppercase()),
        default,
    ))
}

pub fn whisper_model() -> String {
    env_or("CLICKER_WHISPER_MODEL", DEFAULT_WHISPER_MODEL)
}

/// Whether answers are read out loud. On by default: an answer you have to go and read is a
/// log line, and the point of asking out loud is being answered out loud.
pub fn speak_answers() -> bool {
    env_flag("CLICKER_SPEAK")
}

/// Seconds of quiet that end a recording in the bar. Zero waits for the click instead.
pub fn bar_silence_seconds() -> Result<f64, String> {
    env_float("CLICKER_BAR_SILENCE", DEFAULT_BAR_SILENCE_SECONDS)
}

pub fn voice_max_seconds() -> Result<f64, String> {
    env_float("CLICKER_VOICE_MAX_SECONDS", DEFAULT_VOICE_MAX_SECONDS)
}

/// The speech-to-text engine, from `CLICKER_STT`: `auto` (the default) picks OpenAI when
/// `OPENAI_API_KEY` is set and the free Windows recognizer otherwise; `openai` and `windows` force
/// one. Returns "openai" or "windows".
pub fn stt_engine() -> Result<&'static str, String> {
    let has_key = env::var("OPENAI_API_KEY").is_ok_and(|v| !v.trim().is_empty());
    choose_stt(&env_or("CLICKER_STT", "auto"), has_key)
}

/// The pure half of `stt_engine`.
pub fn choose_stt(setting: &str, has_openai_key: bool) -> Result<&'static str, String> {
    match setting.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(if has_openai_key { "openai" } else { "windows" }),
        "openai" => Ok("openai"),
        "windows" => Ok("windows"),
        other => Err(format!(
            "CLICKER_STT must be auto, openai or windows, not {other:?}"
        )),
    }
}

pub fn voice_min_confidence() -> Result<f64, String> {
    env_float("CLICKER_VOICE_MIN_CONFIDENCE", DEFAULT_VOICE_MIN_CONFIDENCE)
}

/// The dynamic site catalog, built from the browser's own bookmarks and history. Off means the
/// classifier sees only the pinned SITES above, which is how this worked before.
pub const DEFAULT_CATALOG_LIMIT: i64 = 30; // options handed to the site question; the cap is 255, but mass thins fast

pub fn catalog_enabled() -> bool {
    env_flag("CLICKER_CATALOG")
}

/// Whether a page title may become a site's label. Off means labels are bare domains, so no
/// page title ever reaches the model.
pub fn catalog_titles() -> bool {
    env_flag("CLICKER_CATALOG_TITLES")
}

pub fn catalog_limit() -> Result<i64, String> {
    match env::var("CLICKER_CATALOG_LIMIT") {
        Ok(raw) => raw
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid literal for int(): {raw:?} (CLICKER_CATALOG_LIMIT)")),
        Err(_) => Ok(DEFAULT_CATALOG_LIMIT),
    }
}

/// `CLICKER_SILENCE_LEVEL`: a fixed per-block RMS silence threshold for voice capture, replacing
/// the adaptive noise-floor gate in `platform::audio`. `None` when unset or empty (adaptive).
/// The platform crate reads the same variable itself, since it does not depend on this crate.
pub fn silence_level() -> Result<Option<f64>, String> {
    match env::var("CLICKER_SILENCE_LEVEL") {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().parse::<f64>().map(Some).map_err(|_| {
            format!("could not convert string to float: {raw:?} (CLICKER_SILENCE_LEVEL)")
        }),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// dotenv_io: read and rewrite a `.env` file in place, preserving everything we were not asked to
// change.
//
// The parsing rules here mirror `load_dotenv` exactly: `KEY=VALUE` lines, `#` comment lines,
// surrounding quotes stripped, whitespace stripped.
// ---------------------------------------------------------------------------------------------

const QUOTES: [char; 2] = ['\'', '"'];
/// Characters that make a bare value ambiguous or lossy once `load_dotenv` strips it.
const NEEDS_QUOTING: [char; 5] = [' ', '\t', '#', '\'', '"'];

/// Return `(key, value)` for a data line, or `None` for a comment/blank/non-assignment.
fn split_line(line: &str) -> Option<(String, String)> {
    let stripped = line.trim();
    if stripped.is_empty() || stripped.starts_with('#') {
        return None;
    }
    let (key, value) = stripped.split_once('=')?;
    Some((
        key.trim().to_string(),
        value.trim().trim_matches(QUOTES).to_string(),
    ))
}

/// Render a value for the right-hand side of `KEY=`.
///
/// Wrapped in double quotes when it contains a space, tab, `#` or a quote character, so that
/// `load_dotenv` reads back exactly what was written. A value that itself begins or ends with a
/// quote character is written bare: `load_dotenv` strips such characters either way, so adding
/// wrapping quotes would only lose more.
pub fn quote(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.starts_with(QUOTES) || value.ends_with(QUOTES) {
        return value.to_string();
    }
    if value.contains(NEEDS_QUOTING) {
        return format!("\"{value}\"");
    }
    value.to_string()
}

/// Return `existing` with `values` applied: known keys rewritten in place, new keys appended.
///
/// Lines carry no line endings. Comments, blanks, ordering and any key absent from `values`
/// survive untouched. An empty value is written as `KEY=` rather than deleted. `values` is applied
/// in order, like the Python dict: a key given twice keeps its first position and its last value.
pub fn merge_lines<K: AsRef<str>, V: AsRef<str>>(
    existing: &[String],
    values: &[(K, V)],
) -> Vec<String> {
    let mut remaining: Vec<(String, String)> = Vec::new();
    for (k, v) in values {
        let (k, v) = (k.as_ref(), v.as_ref());
        match remaining.iter_mut().find(|(rk, _)| rk == k) {
            Some(slot) => slot.1 = v.to_string(),
            None => remaining.push((k.to_string(), v.to_string())),
        }
    }
    let mut merged: Vec<String> = Vec::with_capacity(existing.len() + remaining.len());
    for line in existing {
        let hit =
            split_line(line).and_then(|(key, _)| remaining.iter().position(|(rk, _)| *rk == key));
        match hit {
            Some(at) => {
                let (key, value) = remaining.remove(at);
                merged.push(format!("{key}={}", quote(&value)));
            }
            None => merged.push(line.clone()),
        }
    }

    if !remaining.is_empty() {
        // Do not glue a new key onto trailing blank lines we would otherwise have kept.
        while merged.last().is_some_and(|l| l.trim().is_empty()) {
            merged.pop();
        }
        merged.extend(
            remaining
                .iter()
                .map(|(key, value)| format!("{key}={}", quote(value))),
        );
    }
    merged
}

/// Return the `KEY=VALUE` pairs in `path`, or an empty map when the file is absent.
pub fn read_env(path: &Path) -> io::Result<BTreeMap<String, String>> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    Ok(parse_lines(&read_utf8(path)?).into_iter().collect())
}

/// Apply `values` to `path`, leaving every other line of the file exactly as it was.
///
/// Only the keys in `values` are ever written, so a secret that was not handed to us cannot be
/// rewritten or lost. The file's existing line ending style is kept; a new file gets LF.
pub fn write_env<K: AsRef<str>, V: AsRef<str>>(path: &Path, values: &[(K, V)]) -> io::Result<()> {
    let (newline, existing) = if path.is_file() {
        // Read as bytes and decoded here, so the real line endings stay visible instead of being
        // translated (the Python's newline="").
        let raw = read_utf8(path)?;
        let newline = if raw.contains("\r\n") { "\r\n" } else { "\n" };
        let lines: Vec<String> = splitlines(&raw).into_iter().map(String::from).collect();
        (newline, lines)
    } else {
        ("\n", Vec::new())
    };

    let merged = merge_lines(&existing, values);
    let text = if merged.is_empty() {
        String::new()
    } else {
        merged.join(newline) + newline
    };
    fs::write(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Tests that touch shared environment variables take this, since cargo runs tests in threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("wcore-config-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn s(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Parse a file exactly as `load_dotenv` does (first value wins), into a clean map rather than
    /// the shared process environment.
    fn read_back(path: &Path) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (k, v) in parse_lines(&read_utf8(path).unwrap()) {
            out.entry(k).or_insert(v);
        }
        out
    }

    // --- tests/test_config_and_writer.py (config parts) ---

    #[test]
    fn test_dotenv_sets_only_missing_keys() {
        let _g = lock();
        let dir = tmp("dotenv-missing");
        env::set_var("CLICKER_TEST_PRESENT", "keep");
        env::remove_var("CLICKER_TEST_NEW");
        fs::write(
            dir.join(".env"),
            "# comment\nCLICKER_TEST_PRESENT=override\nCLICKER_TEST_NEW=\"quoted value\"\nbroken line\n",
        )
        .unwrap();
        load_dotenv(&dir.join(".env")).unwrap();
        assert_eq!(env::var("CLICKER_TEST_PRESENT").unwrap(), "keep");
        assert_eq!(env::var("CLICKER_TEST_NEW").unwrap(), "quoted value");
        env::remove_var("CLICKER_TEST_PRESENT");
        env::remove_var("CLICKER_TEST_NEW");
    }

    #[test]
    fn test_dotenv_missing_file_is_fine() {
        load_dotenv(&tmp("dotenv-nope").join("nope.env")).unwrap();
    }

    #[test]
    fn test_dotenv_reads_utf8_not_the_ansi_code_page() {
        let _g = lock();
        let dir = tmp("dotenv-utf8");
        env::remove_var("CLICKER_TEST_UTF8");
        fs::write(dir.join(".env"), "CLICKER_TEST_UTF8=café €\n".as_bytes()).unwrap();
        load_dotenv(&dir.join(".env")).unwrap();
        assert_eq!(env::var("CLICKER_TEST_UTF8").unwrap(), "café €");
        env::remove_var("CLICKER_TEST_UTF8");
    }

    #[test]
    fn test_hotkeys_have_defaults_and_read_the_environment() {
        let _g = lock();
        env::remove_var("CLICKER_HOTKEY_TALK");
        assert_eq!(hotkey("talk").unwrap(), "ctrl+alt+space");
        env::set_var("CLICKER_HOTKEY_TALK", "ctrl+shift+v");
        assert_eq!(hotkey("talk").unwrap(), "ctrl+shift+v");
        env::remove_var("CLICKER_HOTKEY_TALK");
        assert_eq!(hotkey("nonexistent"), None);
    }

    #[test]
    fn test_every_daemon_hotkey_has_a_default() {
        let mut names: Vec<&str> = DEFAULT_HOTKEYS.iter().map(|(n, _)| *n).collect();
        names.sort();
        assert_eq!(names, ["abort", "bar", "goal", "pause", "quit", "talk"]);
        assert!(DEFAULT_HOTKEYS.contains(&("bar", "rightalt")));
    }

    #[test]
    fn test_voice_settings_have_defaults_and_read_the_environment() {
        let _g = lock();
        for name in [
            "CLICKER_WHISPER_MODEL",
            "CLICKER_VOICE_MAX_SECONDS",
            "CLICKER_VOICE_MIN_CONFIDENCE",
            "CLICKER_BAR_SILENCE",
        ] {
            env::remove_var(name);
        }
        assert_eq!(whisper_model(), "base.en");
        assert_eq!(voice_max_seconds().unwrap(), 30.0);
        assert_eq!(voice_min_confidence().unwrap(), 0.55);
        assert_eq!(bar_silence_seconds().unwrap(), 1.6);
        env::set_var("CLICKER_VOICE_MAX_SECONDS", "8");
        assert_eq!(voice_max_seconds().unwrap(), 8.0);
        env::set_var("CLICKER_VOICE_MAX_SECONDS", "eight");
        assert!(voice_max_seconds().is_err());
        env::remove_var("CLICKER_VOICE_MAX_SECONDS");
    }

    #[test]
    fn test_stt_engine_choice() {
        assert_eq!(choose_stt("auto", true).unwrap(), "openai");
        assert_eq!(choose_stt("auto", false).unwrap(), "windows");
        assert_eq!(choose_stt("", false).unwrap(), "windows");
        assert_eq!(choose_stt(" Windows ", true).unwrap(), "windows");
        assert_eq!(choose_stt("openai", false).unwrap(), "openai");
        assert!(choose_stt("whisper", true).is_err());
    }

    #[test]
    fn test_models_follow_the_provider() {
        let _g = lock();
        let saved: Vec<(&str, Option<String>)> = [
            "OPENAI_API_KEY",
            "CLICKER_WRITER_PROVIDER",
            "CLICKER_WRITER_MODEL",
            "CLICKER_ANSWER_MODEL",
        ]
        .into_iter()
        .map(|n| (n, env::var(n).ok()))
        .collect();
        for (n, _) in &saved {
            env::remove_var(n);
        }
        assert_eq!(writer_provider(), "anthropic");
        assert_eq!(writer_model(), "claude-haiku-4-5");
        assert_eq!(answer_model(), "claude-sonnet-5");
        env::set_var("OPENAI_API_KEY", "sk-test");
        assert_eq!(writer_provider(), "openai");
        assert_eq!(writer_model(), "gpt-4.1-mini");
        assert_eq!(answer_model(), "gpt-4.1");
        env::set_var("CLICKER_WRITER_PROVIDER", " Anthropic ");
        assert_eq!(writer_provider(), "anthropic");
        assert_eq!(writer_model(), "claude-haiku-4-5");
        env::set_var("CLICKER_WRITER_MODEL", "custom");
        assert_eq!(writer_model(), "custom");
        for (n, v) in saved {
            match v {
                Some(v) => env::set_var(n, v),
                None => env::remove_var(n),
            }
        }
    }

    #[test]
    fn test_flags_and_catalog_settings() {
        let _g = lock();
        env::remove_var("CLICKER_SPEAK");
        assert!(speak_answers());
        env::set_var("CLICKER_SPEAK", " OFF ");
        assert!(!speak_answers());
        env::remove_var("CLICKER_SPEAK");
        env::remove_var("CLICKER_CATALOG_LIMIT");
        assert_eq!(catalog_limit().unwrap(), 30);
        env::remove_var("CLICKER_SILENCE_LEVEL");
        assert_eq!(silence_level().unwrap(), None);
        env::set_var("CLICKER_SILENCE_LEVEL", "200");
        assert_eq!(silence_level().unwrap(), Some(200.0));
        env::remove_var("CLICKER_SILENCE_LEVEL");
        assert_eq!(SITES.len(), 8);
        assert_eq!(SITES[0], ("github", "https://github.com/"));
    }

    // --- tests/test_dotenv_io.py ---

    #[test]
    fn test_update_in_place_keeps_order_and_comments() {
        let path = tmp("in-place").join(".env");
        fs::write(
            &path,
            "# leading comment\nTYPESAFE_API_KEY=old\n\n# about the browser\nCLICKER_BROWSER=Firefox\nCLICKER_EMAIL=a@b.c\n",
        )
        .unwrap();
        write_env(&path, &[("CLICKER_BROWSER", "Google Chrome")]).unwrap();
        let text = read_utf8(&path).unwrap();
        assert_eq!(
            splitlines(&text),
            [
                "# leading comment",
                "TYPESAFE_API_KEY=old",
                "",
                "# about the browser",
                "CLICKER_BROWSER=\"Google Chrome\"",
                "CLICKER_EMAIL=a@b.c",
            ]
        );
    }

    #[test]
    fn test_new_key_is_appended() {
        let path = tmp("append").join(".env");
        fs::write(&path, "# head\nCLICKER_EMAIL=a@b.c\n").unwrap();
        write_env(&path, &[("CLICKER_WHISPER_MODEL", "small.en")]).unwrap();
        assert_eq!(
            read_utf8(&path).unwrap(),
            "# head\nCLICKER_EMAIL=a@b.c\nCLICKER_WHISPER_MODEL=small.en\n"
        );
    }

    #[test]
    fn test_empty_value_is_kept_as_bare_key() {
        let path = tmp("bare").join(".env");
        fs::write(&path, "CLICKER_BROWSER=Firefox\n").unwrap();
        write_env(&path, &[("CLICKER_BROWSER", "")]).unwrap();
        assert_eq!(read_utf8(&path).unwrap(), "CLICKER_BROWSER=\n");
        assert_eq!(read_env(&path).unwrap(), map(&[("CLICKER_BROWSER", "")]));
        assert_eq!(read_back(&path), map(&[("CLICKER_BROWSER", "")]));
    }

    #[test]
    fn test_other_keys_are_left_completely_alone() {
        let path = tmp("alone").join(".env");
        fs::write(
            &path,
            "TYPESAFE_API_KEY=sk-secret\nANTHROPIC_API_KEY='quoted-secret'\nCLICKER_EMAIL=a@b.c\n",
        )
        .unwrap();
        write_env(&path, &[("CLICKER_EMAIL", "z@y.x")]).unwrap();
        let text = read_utf8(&path).unwrap();
        assert!(text.contains("TYPESAFE_API_KEY=sk-secret"));
        assert!(text.contains("ANTHROPIC_API_KEY='quoted-secret'"));
        assert!(text.ends_with("CLICKER_EMAIL=z@y.x\n"));
    }

    #[test]
    fn test_absent_file_reads_empty_and_can_be_written_from_scratch() {
        let path = tmp("scratch").join(".env");
        assert!(read_env(&path).unwrap().is_empty());
        write_env(
            &path,
            &[("CLICKER_BROWSER", "Firefox"), ("CLICKER_EMAIL", "")],
        )
        .unwrap();
        assert_eq!(
            read_utf8(&path).unwrap(),
            "CLICKER_BROWSER=Firefox\nCLICKER_EMAIL=\n"
        );
        assert_eq!(
            read_env(&path).unwrap(),
            map(&[("CLICKER_BROWSER", "Firefox"), ("CLICKER_EMAIL", "")])
        );
    }

    #[test]
    fn test_round_trip_through_config_parser() {
        for (i, value) in [
            "Google Chrome",
            "a value with # a hash",
            "say \"hi\" now",
            "it's fine",
            "#leading-hash-is-not-a-comment",
            "plain",
            "",
            "a\tb",
            "naïve café €",
        ]
        .into_iter()
        .enumerate()
        {
            let path = tmp(&format!("round-trip-{i}")).join(".env");
            let values = map(&[("CLICKER_BROWSER", value)]);
            write_env(&path, &[("CLICKER_BROWSER", value)]).unwrap();
            assert_eq!(read_env(&path).unwrap(), values, "{value:?}");
            assert_eq!(read_back(&path), values, "{value:?}");
        }
    }

    #[test]
    fn test_quote_leaves_values_that_quoting_cannot_help() {
        // load_dotenv strips surrounding quote characters unconditionally, so a value that begins or
        // ends with one cannot survive either way; we at least do not add more.
        assert_eq!(quote("\"shouty\""), "\"shouty\"");
        assert_eq!(quote("'half open"), "'half open");
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote(""), "");
    }

    #[test]
    fn test_crlf_file_stays_crlf() {
        let path = tmp("crlf").join(".env");
        fs::write(&path, b"# head\r\nCLICKER_EMAIL=a@b.c\r\n").unwrap();
        write_env(
            &path,
            &[("CLICKER_EMAIL", "z@y.x"), ("CLICKER_BROWSER", "Firefox")],
        )
        .unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"# head\r\nCLICKER_EMAIL=z@y.x\r\nCLICKER_BROWSER=Firefox\r\n"
        );
    }

    #[test]
    fn test_lf_file_stays_lf() {
        let path = tmp("lf").join(".env");
        fs::write(&path, b"CLICKER_EMAIL=a@b.c\n").unwrap();
        write_env(&path, &[("CLICKER_EMAIL", "z@y.x")]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"CLICKER_EMAIL=z@y.x\n");
    }

    #[test]
    fn test_merge_lines_is_pure_and_leaves_input_untouched() {
        let existing = s(&["# c", "A=1", "", "B=2"]);
        let snapshot = existing.clone();
        assert_eq!(
            merge_lines(&existing, &[("B", "3"), ("C", "4")]),
            s(&["# c", "A=1", "", "B=3", "C=4"])
        );
        assert_eq!(existing, snapshot);
    }

    #[test]
    fn test_merge_lines_handles_no_values_and_no_lines() {
        let none: &[(&str, &str)] = &[];
        assert_eq!(merge_lines(&s(&["# c", "A=1"]), none), s(&["# c", "A=1"]));
        assert_eq!(merge_lines(&[], &[("A", "1")]), s(&["A=1"]));
        assert!(merge_lines(&[], none).is_empty());
    }

    #[test]
    fn test_merge_lines_updates_a_key_written_with_spaces_and_quotes() {
        assert_eq!(
            merge_lines(
                &s(&["  CLICKER_EMAIL = 'a@b.c' "]),
                &[("CLICKER_EMAIL", "z@y.x")]
            ),
            s(&["CLICKER_EMAIL=z@y.x"])
        );
    }

    #[test]
    fn test_merge_lines_updates_only_the_first_of_a_duplicated_key() {
        // The second occurrence is not in `values` any more, so it is left exactly as it was.
        assert_eq!(
            merge_lines(&s(&["A=1", "A=2"]), &[("A", "9")]),
            s(&["A=9", "A=2"])
        );
    }

    #[test]
    fn test_new_key_does_not_glue_onto_trailing_blanks() {
        assert_eq!(
            merge_lines(&s(&["A=1", "", "  "]), &[("B", "2")]),
            s(&["A=1", "B=2"])
        );
    }
}
