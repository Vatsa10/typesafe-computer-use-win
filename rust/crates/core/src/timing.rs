//! Phase stopwatches: seconds per phase in a plain map. A port of `timing.py`.

use std::time::Instant;

use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;

pub const PHASE_ORDER: [&str; 11] = [
    "capture",
    "screenshot",
    "app",
    "window",
    "field",
    "url",
    "ocr",
    "ax",
    "decide",
    "act",
    "total",
];
// Neither of these is seconds: both print on the ocr phase rather than as phases of their own.
pub const OCR_REGION_PCT: &str = "ocr_region_pct"; // share of the capture handed to the OCR engine
pub const OCR_RECTS: &str = "ocr_rects"; // how many rectangles it took, 0 for a full read or for nothing to read
pub const EXTRAS: [&str; 2] = [OCR_REGION_PCT, OCR_RECTS];

/// Name to seconds, in insertion order like the Python dict, since "anything unexpected" is shown
/// in the order it was recorded. Setting a name already present keeps its place.
///
/// Equality ignores order, as Python dict equality does.
#[derive(Debug, Clone, Default)]
pub struct Timing(Vec<(String, f64)>);

impl PartialEq for Timing {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(n, v)| other.get(n) == Some(v))
    }
}

impl Timing {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &str, value: f64) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name.to_string(), value)),
        }
    }

    pub fn get(&self, name: &str) -> Option<f64> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, f64)> {
        self.0.iter().map(|(n, v)| (n.as_str(), *v))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<S: AsRef<str>> FromIterator<(S, f64)> for Timing {
    fn from_iter<I: IntoIterator<Item = (S, f64)>>(iter: I) -> Self {
        let mut t = Timing::new();
        for (n, v) in iter {
            t.set(n.as_ref(), v);
        }
        t
    }
}

impl Serialize for Timing {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (n, v) in &self.0 {
            map.serialize_entry(n, v)?;
        }
        map.end()
    }
}

/// Python's `round(x, 3)`, near enough for seconds.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Records the seconds from its creation to its drop under `name`. A `None` map makes this a
/// no-op. Dropping is what records, so a block that returns early, fails with `?` or panics is
/// still timed, as the Python's `finally` does.
pub struct Phase<'a> {
    timing: Option<&'a mut Timing>,
    name: &'a str,
    started: Instant,
}

impl<'a> Phase<'a> {
    pub fn start(timing: Option<&'a mut Timing>, name: &'a str) -> Self {
        Self {
            timing,
            name,
            started: Instant::now(),
        }
    }
}

impl Drop for Phase<'_> {
    fn drop(&mut self) {
        if let Some(timing) = self.timing.as_deref_mut() {
            timing.set(self.name, round3(self.started.elapsed().as_secs_f64()));
        }
    }
}

/// Record the seconds spent in `block` under `name`. A None map makes this a no-op.
pub fn phase<R>(timing: Option<&mut Timing>, name: &str, block: impl FnOnce() -> R) -> R {
    let _stopwatch = Phase::start(timing, name);
    block()
}

/// Known phases first, in pipeline order, then anything unexpected.
pub fn ordered(timing: &Timing) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = PHASE_ORDER
        .iter()
        .filter_map(|name| timing.get(name).map(|s| (name.to_string(), s)))
        .collect();
    out.extend(
        timing
            .iter()
            .filter(|(name, _)| !PHASE_ORDER.contains(name))
            .map(|(name, s)| (name.to_string(), s)),
    );
    out
}

/// One log line. A zero `act` means the step never acted, so it is left out.
///
/// What the OCR engine was given rides on the `ocr` phase: `ocr 0.31s (22% of screen, 2 rects)`.
/// The rect count is left off a full read and a step with nothing to re-read, where it says
/// nothing.
pub fn format_timing(timing: &Timing) -> String {
    let parts: Vec<String> = ordered(timing)
        .into_iter()
        .filter(|(name, s)| !(EXTRAS.contains(&name.as_str()) || (name == "act" && *s == 0.0)))
        .map(|(name, seconds)| {
            let note = if name == "ocr" {
                ocr_note(timing)
            } else {
                String::new()
            };
            format!("{name} {seconds:.2}s{note}")
        })
        .collect();
    format!("  timing: {}", parts.join("  "))
}

/// What the ocr phase read, in parentheses, or nothing when the step did not record it.
pub fn ocr_note(timing: &Timing) -> String {
    let Some(pct) = timing.get(OCR_REGION_PCT) else {
        return String::new();
    };
    let rects = timing.get(OCR_RECTS).unwrap_or(0.0) as i64;
    if rects != 0 {
        let plural = if rects == 1 { "" } else { "s" };
        format!(" ({pct:.0}% of screen, {rects} rect{plural})")
    } else {
        format!(" ({pct:.0}% of screen)")
    }
}

/// Mean and max per phase over the steps that recorded it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub steps_timed: usize,
    pub mean: Timing,
    pub max: Timing,
}

/// Mean and max per phase over the steps that recorded it.
pub fn summarize(timings: &[Timing]) -> Summary {
    let seen_names: Timing = timings
        .iter()
        .flat_map(|t| t.iter().map(|(n, _)| (n.to_string(), 0.0)))
        .collect();
    let mut mean = Timing::new();
    let mut peak = Timing::new();
    for (name, _) in ordered(&seen_names) {
        let seen: Vec<f64> = timings.iter().filter_map(|t| t.get(&name)).collect();
        let sum: f64 = seen.iter().sum();
        mean.set(&name, round3(sum / seen.len() as f64));
        peak.set(&name, round3(seen.iter().copied().fold(f64::MIN, f64::max)));
    }
    Summary {
        steps_timed: timings.len(),
        mean,
        max: peak,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(&str, f64)]) -> Timing {
        pairs.iter().map(|(n, v)| (*n, *v)).collect()
    }

    #[test]
    fn test_phase_records_seconds_and_tolerates_none() {
        let mut timing = Timing::new();
        phase(Some(&mut timing), "ocr", || {});
        assert!(timing.get("ocr").unwrap() >= 0.0);
        phase(None, "ocr", || {});
    }

    #[test]
    fn test_phase_records_even_when_the_block_raises() {
        let mut timing = Timing::new();
        let result: Result<(), String> = phase(Some(&mut timing), "act", || Err("boom".into()));
        assert!(result.is_err());
        assert!(timing.contains("act"));
    }

    #[test]
    fn test_phase_records_even_when_the_block_panics() {
        let mut timing = Timing::new();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            phase(Some(&mut timing), "act", || panic!("boom"))
        }));
        assert!(caught.is_err());
        assert!(timing.contains("act"));
    }

    #[test]
    fn test_ordered_puts_pipeline_phases_first_then_extras() {
        let timing = t(&[
            ("total", 1.4),
            ("mystery", 0.1),
            ("ocr", 0.8),
            ("capture", 0.3),
        ]);
        let names: Vec<String> = ordered(&timing).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["capture", "ocr", "total", "mystery"]);
    }

    #[test]
    fn test_format_line_shows_two_decimals_in_pipeline_order() {
        let line = format_timing(&t(&[
            ("total", 1.45),
            ("ocr", 0.823),
            ("capture", 0.31),
            ("decide", 0.21),
            ("act", 0.05),
        ]));
        assert_eq!(
            line,
            "  timing: capture 0.31s  ocr 0.82s  decide 0.21s  act 0.05s  total 1.45s"
        );
    }

    #[test]
    fn test_format_line_shows_the_share_of_the_screen_that_was_ocred() {
        let line = format_timing(&t(&[
            ("capture", 0.31),
            ("ocr", 0.31),
            ("ocr_region_pct", 22.4),
            ("ocr_rects", 0.0),
            ("total", 0.9),
        ]));
        assert_eq!(
            line,
            "  timing: capture 0.31s  ocr 0.31s (22% of screen)  total 0.90s"
        );
    }

    #[test]
    fn test_format_line_counts_the_rectangles_when_the_read_was_split() {
        let line = format_timing(&t(&[
            ("ocr", 0.31),
            ("ocr_region_pct", 22.4),
            ("ocr_rects", 2.0),
            ("total", 0.9),
        ]));
        assert_eq!(
            line,
            "  timing: ocr 0.31s (22% of screen, 2 rects)  total 0.90s"
        );
    }

    #[test]
    fn test_format_line_says_one_rect_in_the_singular() {
        assert_eq!(
            format_timing(&t(&[
                ("ocr", 0.31),
                ("ocr_region_pct", 8.0),
                ("ocr_rects", 1.0)
            ])),
            "  timing: ocr 0.31s (8% of screen, 1 rect)"
        );
    }

    #[test]
    fn test_format_line_omits_act_when_the_step_did_not_act() {
        assert!(!format_timing(&t(&[
            ("capture", 0.3),
            ("ocr", 0.8),
            ("decide", 0.2),
            ("act", 0.0),
            ("total", 1.3)
        ]))
        .contains("act"));
    }

    #[test]
    fn test_summarize_means_and_maxes_each_phase() {
        let got = summarize(&[
            t(&[("ocr", 0.8), ("total", 1.0)]),
            t(&[("ocr", 0.4), ("total", 2.0)]),
        ]);
        assert_eq!(
            got,
            Summary {
                steps_timed: 2,
                mean: t(&[("ocr", 0.6), ("total", 1.5)]),
                max: t(&[("ocr", 0.8), ("total", 2.0)]),
            }
        );
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            r#"{"steps_timed":2,"mean":{"ocr":0.6,"total":1.5},"max":{"ocr":0.8,"total":2.0}}"#
        );
    }

    #[test]
    fn test_summarize_averages_a_phase_over_the_steps_that_recorded_it() {
        let got = summarize(&[t(&[("ocr", 0.8)]), t(&[("ocr", 0.4), ("url", 0.6)])]);
        assert_eq!(got.mean, t(&[("ocr", 0.6), ("url", 0.6)]));
        assert_eq!(got.max.get("url"), Some(0.6));
    }

    #[test]
    fn test_summarize_of_no_steps() {
        assert_eq!(
            summarize(&[]),
            Summary {
                steps_timed: 0,
                mean: Timing::new(),
                max: Timing::new()
            }
        );
    }
}
