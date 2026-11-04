//! `my-answers.json`: the answers only the user can give (expected salary, notice period, visa or
//! work authorisation, gender, disability, race, veteran status, relocation, attestation consent,
//! criminal history), kept once under `POINTER_HOME`. They are the user's data, written by the
//! user in Settings or learned from what the user said; they are never inferred from a resume.

use std::path::Path;

use regex::Regex;
use serde_json::{json, Map, Value};

pub const FILE: &str = "my-answers.json";

/// The fixed keys, in the order Settings shows them.
pub const KEYS: [(&str, &str); 11] = [
    ("expected_salary", "Expected salary"),
    ("notice_period", "Notice period"),
    ("start_date", "Earliest start date"),
    ("work_authorization", "Work authorisation"),
    ("visa_sponsorship", "Visa sponsorship needed"),
    ("gender", "Gender"),
    ("disability", "Disability"),
    ("race", "Race or ethnicity"),
    ("veteran", "Veteran status"),
    ("relocation", "Willing to relocate"),
    ("attestation", "Attestation or consent (yes to tick)"),
];

/// Not shown as its own field, but a key all the same.
const EXTRA: [&str; 1] = ["criminal_history"];

/// Which fixed key a field label asks about, if any.
pub fn key_for(label: &str) -> Option<&'static str> {
    let l = label.to_lowercase();
    let has = |re: &str| Regex::new(re).unwrap().is_match(&l);
    Some(if has(r"\b(salary|salaries|compensation|ctc|remuneration|pay|wage)\b") {
        "expected_salary"
    } else if has(r"notice") {
        "notice_period"
    } else if has(r"start date|available to start|earliest") {
        "start_date"
    } else if has(r"sponsor") {
        "visa_sponsorship"
    } else if has(r"visa|work authori|authori[sz]ed|eligib|citizen|immigration|right to work") {
        "work_authorization"
    } else if has(r"\b(gender|sex|pronouns?|transgender)\b") {
        "gender"
    } else if has(r"disab") {
        "disability"
    } else if has(r"\b(race|racial|ethnic\w*|hispanic)\b") {
        "race"
    } else if has(r"veteran") {
        "veteran"
    } else if has(r"relocat") {
        "relocation"
    } else if has(r"criminal|convict|felony|misdemeanou?r|arrest") {
        "criminal_history"
    } else if has(r"attest|certify|declar|affirm|i agree|agree to|consent|acknowledg|signature") {
        "attestation"
    } else {
        return None;
    })
}

pub fn load(home: &Path) -> Map<String, Value> {
    std::fs::read_to_string(home.join(FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub fn save(home: &Path, data: &Map<String, Value>) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let tmp = home.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(data.clone())).unwrap())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, home.join(FILE)).map_err(|e| e.to_string())
}

fn text_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The saved answer for a field, fitted to its options. Err says why a saved answer cannot be used.
pub fn lookup(home: &Path, label: &str, options: &[String]) -> Option<Result<String, String>> {
    let data = load(home);
    let wanted = label.trim().to_lowercase();
    let value = data
        .get("custom")
        .and_then(Value::as_object)
        .and_then(|c| text_of(c.get(&wanted)))
        .or_else(|| key_for(label).and_then(|k| text_of(data.get(k))))?;
    if options.is_empty() {
        return Some(Ok(value));
    }
    let v = value.to_lowercase();
    let pick = options
        .iter()
        .find(|o| o.trim().to_lowercase() == v)
        .or_else(|| options.iter().find(|o| o.to_lowercase().contains(&v) || v.contains(&o.trim().to_lowercase())));
    Some(match pick {
        Some(o) => Ok(o.clone()),
        None => Err("the saved answer is not one of the choices".into()),
    })
}

/// Remember an answer the user gave: under its fixed key when the label names one, else by label.
pub fn learn(home: &Path, label: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("value is required".into());
    }
    let mut data = load(home);
    match key_for(label) {
        Some(k) => {
            data.insert(k.into(), json!(value));
        }
        None => {
            let mut custom = data
                .get("custom")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            custom.insert(label.trim().to_lowercase(), json!(value));
            data.insert("custom".into(), Value::Object(custom));
        }
    }
    save(home, &data)?;
    Ok(key_for(label).unwrap_or("custom").to_string())
}

/// What Settings shows: every fixed key with its label and saved value.
pub fn describe(home: &Path) -> Value {
    let data = load(home);
    let fields: Vec<Value> = KEYS
        .iter()
        .map(|(k, label)| json!({ "key": k, "label": label, "value": text_of(data.get(*k)).unwrap_or_default() }))
        .collect();
    json!({ "fields": fields })
}

/// Settings saves the fixed keys it shows; the learned ones are left alone.
pub fn set_fixed(home: &Path, values: &Map<String, Value>) -> Result<(), String> {
    let mut data = load(home);
    for (k, _) in KEYS.iter().map(|(k, l)| (*k, *l)).chain(EXTRA.iter().map(|k| (*k, ""))) {
        if let Some(v) = values.get(k) {
            let t = v.as_str().unwrap_or("").trim().to_string();
            if t.is_empty() {
                data.remove(k);
            } else {
                data.insert(k.into(), json!(t));
            }
        }
    }
    save(home, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("answers-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn labels_map_to_keys() {
        assert_eq!(key_for("Expected salary (USD)"), Some("expected_salary"));
        assert_eq!(key_for("Will you now or in the future require sponsorship?"), Some("visa_sponsorship"));
        assert_eq!(key_for("Are you legally authorized to work here?"), Some("work_authorization"));
        assert_eq!(key_for("I certify the above is true"), Some("attestation"));
        assert_eq!(key_for("Favourite colour"), None);
    }

    #[test]
    fn saved_answers_are_used_and_fitted_to_the_choices() {
        let h = home("use");
        assert!(lookup(&h, "Expected salary", &[]).is_none(), "nothing saved: nothing invented");
        set_fixed(&h, json!({"expected_salary": "123", "gender": "Prefer not to say"}).as_object().unwrap()).unwrap();
        assert_eq!(lookup(&h, "Expected salary", &[]), Some(Ok("123".into())));
        let opts = vec!["Male".to_string(), "Prefer not to say".to_string()];
        assert_eq!(lookup(&h, "Gender", &opts), Some(Ok("Prefer not to say".into())));
        let other = vec!["Male".to_string(), "Female".to_string()];
        assert!(matches!(lookup(&h, "Gender", &other), Some(Err(_))));
    }

    #[test]
    fn what_the_user_says_is_learned() {
        let h = home("learn");
        assert_eq!(learn(&h, "Notice period", "2 weeks").unwrap(), "notice_period");
        assert_eq!(learn(&h, "Favourite colour", "green").unwrap(), "custom");
        assert_eq!(lookup(&h, "notice period", &[]), Some(Ok("2 weeks".into())));
        assert_eq!(lookup(&h, "Favourite colour", &[]), Some(Ok("green".into())));
        assert!(learn(&h, "x", " ").is_err());
    }
}
