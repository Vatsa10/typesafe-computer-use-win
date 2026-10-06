//! Read the `runs/` folder, so a UI can browse what already happened. A port of `runs_index.py`.
//!
//! Nothing here writes, captures or clicks: it is the data layer behind a history tab, and every
//! function is pure with respect to the disk.
//!
//! A run folder is written by a process that may be killed mid-run, so tolerance is the whole job.
//! `run.json` lands in the `finally` block, which means a run killed hard has step files and no
//! summary at all; a run killed mid-write has half a JSON document. Neither may fail here.
//! Anything that cannot be recovered degrades to a default, and the folder still appears in the
//! list.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

pub const UNKNOWN: &str = "unknown";

fn step_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^step-(\d+)(-raw\.png|-payload\.txt|-answers\.json|\.png)$")
            .expect("STEP compiles")
    })
}

/// The files one step left behind. Each is independently optional: a run that died between
/// two writes leaves `-raw.png` without the annotated `.png` beside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepRecord {
    pub number: u64,
    pub annotated: Option<PathBuf>, // step-NNN.png
    pub raw: Option<PathBuf>,       // step-NNN-raw.png
    pub payload: Option<PathBuf>,   // step-NNN-payload.txt
    pub answers: Option<PathBuf>,   // step-NNN-answers.json
}

impl StepRecord {
    /// The field a matched suffix fills.
    fn slot(&mut self, suffix: &str) -> &mut Option<PathBuf> {
        match suffix {
            "-raw.png" => &mut self.raw,
            "-payload.txt" => &mut self.payload,
            "-answers.json" => &mut self.answers,
            _ => &mut self.annotated, // ".png"
        }
    }
}

/// One run, as much of it as survived.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub path: PathBuf,
    pub name: String, // the folder name, which is the timestamp
    pub goal: String,
    pub outcome: String,
    pub answer: Option<String>,
    pub goal_achieved: Option<bool>,
    pub seconds: Option<f64>,
    pub steps_taken: i64,
    pub acted: bool,
}

/// Whatever object the file holds, or an empty map. Missing, truncated, unreadable and
/// not-an-object all mean the same thing to a reader: nothing to show.
///
/// Decoded as UTF-8 with bad bytes replaced, never the ANSI code page.
fn read_json(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = fs::read(path) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn as_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

/// A JSON number, never a bool (serde keeps the two apart, as `isinstance` has to in Python).
fn as_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

/// The step files in a run folder, ascending by step number.
///
/// Sorted numerically, not lexically, so step 2 comes before step 10, and the digit count is read
/// from the name rather than assumed: older runs numbered with fewer than three digits.
pub fn steps_of(path: &Path) -> Vec<StepRecord> {
    let Ok(read) = fs::read_dir(path) else {
        return Vec::new();
    };
    // Sorted by name first, as the Python sorts its paths, so when two names give one number
    // (step-1.png and step-001.png) the later one wins the same way.
    let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    let mut found: BTreeMap<u64, StepRecord> = BTreeMap::new();
    for entry in entries {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(m) = step_re().captures(name) else {
            continue;
        };
        if !entry.is_file() {
            continue;
        }
        let Ok(number) = m[1].parse::<u64>() else {
            continue;
        };
        let suffix = m[2].to_string();
        let record = found.entry(number).or_insert_with(|| StepRecord {
            number,
            ..StepRecord::default()
        });
        *record.slot(&suffix) = Some(entry);
    }
    found.into_values().collect()
}

/// The parsed `-answers.json` for a step; empty when it is missing or corrupt.
pub fn step_answers(step: &StepRecord) -> Map<String, Value> {
    step.answers.as_deref().map(read_json).unwrap_or_default()
}

/// One run folder, read as far as it goes.
///
/// With no `run.json` the outcome is `"unknown"` and the step count is recovered by counting step
/// files, which is what a killed run leaves to count.
pub fn load_run(path: &Path) -> RunSummary {
    let data = read_json(&path.join("run.json"));
    let outcome = as_str(data.get("outcome")).unwrap_or(UNKNOWN);
    let steps_taken = match data.get("steps_taken") {
        Some(Value::Number(n)) if n.is_i64() => n.as_i64().unwrap_or_default(),
        _ => steps_of(path).len() as i64,
    };
    RunSummary {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        goal: as_str(data.get("goal")).unwrap_or("").to_string(),
        outcome: if outcome.is_empty() { UNKNOWN } else { outcome }.to_string(),
        answer: as_str(data.get("answer"))
            .filter(|a| !a.is_empty())
            .map(String::from),
        goal_achieved: data.get("goal_achieved").and_then(Value::as_bool),
        seconds: as_number(data.get("seconds")),
        steps_taken,
        acted: data.get("act") == Some(&Value::Bool(true)),
    }
}

/// Every run folder under `root`, newest first by folder name, which is a timestamp.
///
/// A missing `root` is an empty history, not an error, and a stray file beside the folders is
/// skipped rather than read.
pub fn list_runs(root: &Path) -> Vec<RunSummary> {
    let Ok(read) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    folders.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    folders.iter().map(|f| load_run(f)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete() -> Value {
        json!({
            "goal": "open the billing page",
            "act": true,
            "steps_taken": 3,
            "outcome": "done",
            "answer": "billing shows $42 due",
            "goal_achieved": true,
            "seconds": 12.5,
            "timing": {"total": 12.5},
            "history": ["click [4]", "click [9]", "type 'billing'"],
            "config": {"goal": "open the billing page"},
        })
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wcore-runs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    enum Summary {
        None,
        Raw(&'static str),
        Json(Value),
    }

    fn make_run(root: &Path, name: &str, summary: Summary, steps: u32) -> PathBuf {
        let folder = root.join(name);
        fs::create_dir_all(&folder).unwrap();
        match summary {
            Summary::None => {}
            Summary::Raw(text) => fs::write(folder.join("run.json"), text.as_bytes()).unwrap(),
            Summary::Json(v) => {
                fs::write(folder.join("run.json"), v.to_string().as_bytes()).unwrap()
            }
        }
        for n in 1..=steps {
            write_step(&folder, n, ALL_PARTS);
        }
        folder
    }

    const ALL_PARTS: &[&str] = &["-raw.png", ".png", "-payload.txt", "-answers.json"];

    fn write_step(folder: &Path, n: u32, parts: &[&str]) {
        for part in parts {
            let target = folder.join(format!("step-{n:03}{part}"));
            let bytes: Vec<u8> = match *part {
                "-answers.json" => json!({"kind": "click", "confidence": 0.9})
                    .to_string()
                    .into_bytes(),
                "-payload.txt" => b"payload".to_vec(),
                _ => b"\x89PNG".to_vec(),
            };
            fs::write(target, bytes).unwrap();
        }
    }

    fn names(runs: &[RunSummary]) -> Vec<String> {
        runs.iter().map(|r| r.name.clone()).collect()
    }

    #[test]
    fn test_complete_run_parses_fully() {
        let root = tmp("complete");
        let folder = make_run(&root, "2026-09-21-101500", Summary::Json(complete()), 3);
        let run = load_run(&folder);
        assert_eq!(run.name, "2026-09-21-101500");
        assert_eq!(run.path, folder);
        assert_eq!(run.goal, "open the billing page");
        assert_eq!(run.outcome, "done");
        assert_eq!(run.answer.as_deref(), Some("billing shows $42 due"));
        assert_eq!(run.goal_achieved, Some(true));
        assert_eq!(run.seconds, Some(12.5));
        assert_eq!(run.steps_taken, 3);
        assert!(run.acted);
    }

    #[test]
    fn test_list_runs_is_newest_first() {
        let root = tmp("newest");
        for name in [
            "2026-09-20-090000",
            "2026-09-21-101500",
            "2026-09-19-235959",
        ] {
            make_run(&root, name, Summary::Json(complete()), 0);
        }
        assert_eq!(
            names(&list_runs(&root)),
            [
                "2026-09-21-101500",
                "2026-09-20-090000",
                "2026-09-19-235959"
            ]
        );
    }

    /// Killed before the finally block: no run.json, but the step files are still there.
    #[test]
    fn test_run_without_summary_still_lists_as_unknown() {
        let root = tmp("no-summary");
        make_run(&root, "2026-09-21-110000", Summary::None, 2);
        let runs = list_runs(&root);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.outcome, "unknown");
        assert_eq!(run.goal, "");
        assert_eq!(run.answer, None);
        assert_eq!(run.goal_achieved, None);
        assert_eq!(run.seconds, None);
        assert!(!run.acted);
        assert_eq!(run.steps_taken, 2); // recovered by counting what was written
    }

    #[test]
    fn test_corrupt_summary_degrades_instead_of_raising() {
        let root = tmp("corrupt");
        let folder = make_run(
            &root,
            "2026-09-21-120000",
            Summary::Raw(r#"{"goal": "half a doc", "outcome": "do"#),
            1,
        );
        let run = load_run(&folder);
        assert_eq!(run.outcome, "unknown");
        assert_eq!(run.goal, "");
        assert_eq!(run.steps_taken, 1);
    }

    #[test]
    fn test_summary_of_the_wrong_shape_degrades() {
        let root = tmp("shape");
        let folder = make_run(&root, "2026-09-21-121000", Summary::Raw("[1, 2, 3]"), 0);
        assert_eq!(load_run(&folder).outcome, "unknown");
    }

    #[test]
    fn test_fields_of_the_wrong_type_degrade() {
        let root = tmp("types");
        let folder = make_run(
            &root,
            "2026-09-21-122000",
            Summary::Json(json!({
                "goal": 7, "outcome": "", "answer": "", "goal_achieved": 1,
                "seconds": true, "steps_taken": true, "act": 1
            })),
            2,
        );
        let run = load_run(&folder);
        assert_eq!(run.goal, "");
        assert_eq!(run.outcome, "unknown");
        assert_eq!(run.answer, None);
        assert_eq!(run.goal_achieved, None);
        assert_eq!(run.seconds, None);
        assert_eq!(run.steps_taken, 2);
        assert!(!run.acted);
    }

    #[test]
    fn test_stray_file_in_the_root_is_skipped() {
        let root = tmp("stray");
        make_run(&root, "2026-09-21-130000", Summary::Json(complete()), 0);
        fs::write(root.join("notes.txt"), "not a run").unwrap();
        fs::write(root.join("latest.json"), "{}").unwrap();
        assert_eq!(names(&list_runs(&root)), ["2026-09-21-130000"]);
    }

    #[test]
    fn test_missing_root_is_an_empty_history() {
        assert!(list_runs(&tmp("missing").join("runs")).is_empty());
    }

    #[test]
    fn test_root_that_is_a_file_is_an_empty_history() {
        let target = tmp("file-root").join("runs");
        fs::write(&target, "not a folder").unwrap();
        assert!(list_runs(&target).is_empty());
    }

    #[test]
    fn test_steps_sort_numerically_not_lexically() {
        let root = tmp("numeric");
        let folder = make_run(&root, "2026-09-21-140000", Summary::Json(complete()), 0);
        for n in [10, 2, 1, 100] {
            write_step(&folder, n, ALL_PARTS);
        }
        let numbers: Vec<u64> = steps_of(&folder).iter().map(|s| s.number).collect();
        assert_eq!(numbers, [1, 2, 10, 100]);
    }

    /// Older runs numbered step files without padding.
    #[test]
    fn test_steps_with_fewer_than_three_digits_are_read() {
        let root = tmp("short-digits");
        let folder = make_run(&root, "2026-09-21-141000", Summary::Json(complete()), 0);
        fs::write(folder.join("step-2-raw.png"), b"\x89PNG").unwrap();
        fs::write(folder.join("step-10.png"), b"\x89PNG").unwrap();
        let got: Vec<(u64, bool, bool)> = steps_of(&folder)
            .iter()
            .map(|s| (s.number, s.raw.is_some(), s.annotated.is_some()))
            .collect();
        assert_eq!(got, [(2, true, false), (10, false, true)]);
    }

    #[test]
    fn test_partial_step_set_reports_none_for_what_is_absent() {
        let root = tmp("partial");
        let folder = make_run(&root, "2026-09-21-150000", Summary::None, 0);
        write_step(&folder, 1, &["-raw.png", "-payload.txt"]);
        let steps = steps_of(&folder);
        assert_eq!(steps.len(), 1);
        let step = &steps[0];
        assert!(step.raw.is_some() && step.payload.is_some());
        assert_eq!(step.annotated, None);
        assert_eq!(step.answers, None);
        assert!(step_answers(step).is_empty());
    }

    #[test]
    fn test_step_answers_parses_and_tolerates_corruption() {
        let root = tmp("answers");
        let folder = make_run(&root, "2026-09-21-151000", Summary::None, 1);
        let steps = steps_of(&folder);
        let step = &steps[0];
        assert_eq!(
            Value::Object(step_answers(step)),
            json!({"kind": "click", "confidence": 0.9})
        );
        let answers = step.answers.as_ref().unwrap();
        fs::write(answers, r#"{"kind": "cli"#).unwrap();
        assert!(step_answers(step).is_empty());
        assert!(step_answers(&StepRecord {
            number: 7,
            ..Default::default()
        })
        .is_empty());
    }

    #[test]
    fn test_unrelated_files_in_a_run_folder_are_not_steps() {
        let root = tmp("unrelated");
        let folder = make_run(&root, "2026-09-21-160000", Summary::Json(complete()), 1);
        fs::write(folder.join("answer-raw.png"), b"\x89PNG").unwrap();
        fs::write(folder.join("run.log"), "log").unwrap();
        fs::write(folder.join("step-notes.txt"), "nope").unwrap();
        let numbers: Vec<u64> = steps_of(&folder).iter().map(|s| s.number).collect();
        assert_eq!(numbers, [1]);
    }

    /// Payloads hold screen text, and the Windows default encoding would fail on it.
    #[test]
    fn test_non_ascii_payload_reads_back() {
        let root = tmp("non-ascii-payload");
        let folder = make_run(&root, "2026-09-21-170000", Summary::None, 0);
        let text = "Rechnung fällig — 42 € · naïve café · 日本語";
        fs::write(folder.join("step-001-payload.txt"), text.as_bytes()).unwrap();
        let steps = steps_of(&folder);
        let payload = steps[0].payload.as_ref().unwrap();
        assert_eq!(String::from_utf8(fs::read(payload).unwrap()).unwrap(), text);
    }

    #[test]
    fn test_non_ascii_summary_and_answers_read_back() {
        let root = tmp("non-ascii-summary");
        let mut summary = complete();
        summary["goal"] = json!("öffne die Rechnung");
        summary["answer"] = json!("42 € fällig");
        let folder = make_run(&root, "2026-09-21-171000", Summary::Json(summary), 0);
        fs::write(
            folder.join("step-001-answers.json"),
            json!({"chosen": "click 'Löschen'"}).to_string().as_bytes(),
        )
        .unwrap();
        let run = load_run(&folder);
        assert_eq!(run.goal, "öffne die Rechnung");
        assert_eq!(run.answer.as_deref(), Some("42 € fällig"));
        let steps = steps_of(&folder);
        assert_eq!(
            Value::Object(step_answers(&steps[0])),
            json!({"chosen": "click 'Löschen'"})
        );
    }

    /// Not a test of behaviour: prints the real `runs/` folder in the same shape as the Python
    /// one-liner, for comparing the two by eye. `cargo test -p core -- --ignored --nocapture live`
    #[test]
    #[ignore]
    fn live_list_of_the_real_runs_folder() {
        // WCORE_RUNS_DIR overrides the folder, for running this from a copy of the crate.
        let root = std::env::var_os("WCORE_RUNS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../runs"));
        for r in list_runs(&root) {
            println!("LIVE {} {} {}", r.name, r.outcome, r.steps_taken);
        }
    }
}
