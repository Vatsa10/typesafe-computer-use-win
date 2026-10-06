//! The settings tab: which `.env` keys the panel may show and edit.
//!
//! Secrets are listed so they can be entered, but their values never leave this process: a row for
//! a key carries an empty `value` and says only whether one is set, and saving an empty secret
//! leaves the stored one alone rather than wiping it.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Map, Value};
use wcore::config;

pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub fallback: String,
    pub secret: bool,
}

fn row(key: &'static str, label: &'static str, fallback: impl Into<String>) -> Setting {
    Setting {
        key,
        label,
        fallback: fallback.into(),
        secret: false,
    }
}

fn secret(key: &'static str, label: &'static str) -> Setting {
    Setting {
        key,
        label,
        fallback: String::new(),
        secret: true,
    }
}

fn hotkey_default(name: &str) -> String {
    config::DEFAULT_HOTKEYS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| s.to_string())
        .unwrap_or_default()
}

/// Every editable key, in the order the panel shows them.
pub fn catalog() -> Vec<Setting> {
    vec![
        secret("OPENAI_API_KEY", "OpenAI API key"),
        secret("ANTHROPIC_API_KEY", "Anthropic API key"),
        secret("TYPESAFE_API_KEY", "TypeSafe API key"),
        row("CLICKER_BROWSER", "Browser", config::DEFAULT_BROWSER),
        row(
            "CLICKER_EMAIL",
            "Email for type_email",
            "unset: the action is not offered",
        ),
        row(
            "CLICKER_HOTKEY_BAR",
            "Hotkey: command bar",
            hotkey_default("bar"),
        ),
        row(
            "CLICKER_HOTKEY_TALK",
            "Hotkey: push to talk",
            hotkey_default("talk"),
        ),
        row(
            "CLICKER_HOTKEY_GOAL",
            "Hotkey: typed goal",
            hotkey_default("goal"),
        ),
        row(
            "CLICKER_HOTKEY_PAUSE",
            "Hotkey: pause or resume",
            hotkey_default("pause"),
        ),
        row(
            "CLICKER_HOTKEY_ABORT",
            "Hotkey: abort the run",
            hotkey_default("abort"),
        ),
        row(
            "CLICKER_HOTKEY_QUIT",
            "Hotkey: quit",
            hotkey_default("quit"),
        ),
        row("CLICKER_SPEAK", "Read answers aloud", "1 (0 = silent)"),
        row(
            "CLICKER_BAR_SILENCE",
            "Pause that ends a bar recording (s)",
            config::DEFAULT_BAR_SILENCE_SECONDS.to_string(),
        ),
        row(
            "CLICKER_VOICE_MAX_SECONDS",
            "Longest utterance (s)",
            config::DEFAULT_VOICE_MAX_SECONDS.to_string(),
        ),
        row(
            "CLICKER_VOICE_MIN_CONFIDENCE",
            "Voice confidence floor",
            config::DEFAULT_VOICE_MIN_CONFIDENCE.to_string(),
        ),
        row(
            "CLICKER_STT",
            "Speech-to-text engine",
            "auto (openai with a key, else windows)",
        ),
        row(
            "CLICKER_TRANSCRIBE_MODEL",
            "Transcription model",
            "gpt-4o-mini-transcribe",
        ),
        row(
            "CLICKER_CATALOG",
            "Site catalog from browser",
            "1 (0 = pinned sites only)",
        ),
        row(
            "CLICKER_CATALOG_TITLES",
            "Page titles as labels",
            "1 (0 = bare domains)",
        ),
        row(
            "CLICKER_CATALOG_LIMIT",
            "Sites offered per goal",
            config::DEFAULT_CATALOG_LIMIT.to_string(),
        ),
        row("CLICKER_OCR_LANGUAGE", "OCR language", "en-US"),
        row(
            "CLICKER_WRITER_PROVIDER",
            "Writer provider",
            "openai when its key is set",
        ),
        row(
            "CLICKER_WRITER_MODEL",
            "Writer model",
            config::DEFAULT_OPENAI_WRITER_MODEL,
        ),
        row(
            "CLICKER_ANSWER_MODEL",
            "Answer model",
            config::DEFAULT_OPENAI_ANSWER_MODEL,
        ),
    ]
}

/// The rows for the panel. A secret's value is always empty; `fallback` says whether it is set.
pub fn rows(current: &BTreeMap<String, String>) -> Vec<Value> {
    catalog()
        .into_iter()
        .map(|s| {
            if s.secret {
                let set = current.get(s.key).is_some_and(|v| !v.is_empty())
                    || std::env::var(s.key).is_ok_and(|v| !v.is_empty());
                let fallback = if set { "set (hidden; type to replace)" } else { "not set" };
                json!({ "key": s.key, "label": s.label, "value": "", "fallback": fallback, "secret": true })
            } else {
                let value = current.get(s.key).cloned().unwrap_or_default();
                json!({ "key": s.key, "label": s.label, "value": value, "fallback": s.fallback, "secret": false })
            }
        })
        .collect()
}

/// What to write: known keys only, trimmed, and an empty secret means "unchanged".
pub fn to_write(values: &Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for setting in &catalog() {
        let Some(raw) = values.get(setting.key) else {
            continue;
        };
        let text = match raw {
            Value::String(s) => s.trim().to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        if setting.secret && text.is_empty() {
            continue;
        }
        out.push((setting.key.to_string(), text));
    }
    out
}

/// Write the panel's values into `.env` and the live environment. Returns the keys written,
/// never their values.
pub fn save(path: &Path, values: &Map<String, Value>) -> Result<Vec<String>, String> {
    let pairs = to_write(values);
    config::write_env(path, &pairs)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    for (key, value) in &pairs {
        // So a key typed in just now works without a restart. Hotkeys still need one.
        std::env::set_var(key, value);
    }
    Ok(pairs.into_iter().map(|(k, _)| k).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("OPENAI_API_KEY".to_string(), "sk-very-secret".to_string()),
            ("CLICKER_BROWSER".to_string(), "Firefox".to_string()),
        ])
    }

    #[test]
    fn a_secret_value_never_reaches_the_panel() {
        let rows = rows(&current());
        let text = serde_json::to_string(&rows).unwrap();
        assert!(!text.contains("sk-very-secret"));
        let key = rows.iter().find(|r| r["key"] == "OPENAI_API_KEY").unwrap();
        assert_eq!(key["value"], "");
        assert_eq!(key["secret"], true);
        assert!(key["fallback"].as_str().unwrap().starts_with("set"));
    }

    #[test]
    fn plain_settings_show_their_value_and_default() {
        let rows = rows(&current());
        let browser = rows.iter().find(|r| r["key"] == "CLICKER_BROWSER").unwrap();
        assert_eq!(browser["value"], "Firefox");
        assert_eq!(browser["fallback"], config::DEFAULT_BROWSER);
        assert!(rows
            .iter()
            .all(|r| r["label"].is_string() && r["fallback"].is_string()));
    }

    #[test]
    fn an_empty_secret_is_left_alone_and_unknown_keys_are_dropped() {
        let values = json!({
            "OPENAI_API_KEY": "", "TYPESAFE_API_KEY": "  ts-new  ",
            "CLICKER_BROWSER": " Edge ", "PATH": "C:/evil",
        });
        let pairs = to_write(values.as_object().unwrap());
        assert!(pairs
            .iter()
            .all(|(k, _)| k != "OPENAI_API_KEY" && k != "PATH"));
        assert!(pairs.contains(&("TYPESAFE_API_KEY".into(), "ts-new".into())));
        assert!(pairs.contains(&("CLICKER_BROWSER".into(), "Edge".into())));
    }

    #[test]
    fn save_keeps_the_stored_secret_and_reports_only_key_names() {
        let dir = std::env::temp_dir().join(format!("app-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "OPENAI_API_KEY=sk-keep\n# note\n").unwrap();
        let values = json!({ "OPENAI_API_KEY": "", "CLICKER_CATALOG_LIMIT": "12" });
        let written = save(&path, values.as_object().unwrap()).unwrap();
        assert_eq!(written, vec!["CLICKER_CATALOG_LIMIT".to_string()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("OPENAI_API_KEY=sk-keep") && text.contains("# note"));
        assert!(text.contains("CLICKER_CATALOG_LIMIT=12"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
