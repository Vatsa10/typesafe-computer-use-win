//! Deterministic date handling.
//!
//! The classifier does no calendar math, so dates found in screen text are parsed here and handed
//! over as offsets from today. A port of `dates.py`.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{Datelike, Local, NaiveDate};
use regex::Regex;

use crate::models::{Item, Screen};

pub const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const MONTH: &str = "jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec";
const DASHES: &str = "[-\u{2013}\u{2014}]"; // hyphen, en dash, em dash between the days of a range
pub const NEAR_ROWS_PT: f64 = 60.0;

fn date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pattern = format!(
            r"(?i)\b(?:(?P<mon>{MONTH})[a-z]*\.?\s+(?P<day>\d{{1,2}})(?:\s*{DASHES}\s*\d{{1,2}})?(?:,?\s+(?P<year>\d{{4}}))?|(?P<day2>\d{{1,2}})\s+(?P<mon2>{MONTH})[a-z]*\.?(?:,?\s+(?P<year2>\d{{4}}))?|(?P<iso>\d{{4}}-\d{{2}}-\d{{2}})|(?P<m>\d{{1,2}})/(?P<d>\d{{1,2}})/(?P<y>\d{{4}}))\b"
        );
        Regex::new(&pattern).expect("DATE_RE compiles")
    })
}

fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

/// Local time, timezone and today's date, for the payload. The Python's `%Z` gives a zone name on
/// Windows; chrono's local clock carries only the offset, so that is what is reported here.
pub fn now_context() -> serde_json::Value {
    let now = Local::now();
    serde_json::json!({
        "local_time": now.format("%Y-%m-%d %H:%M %A").to_string(),
        "timezone": now.format("%:z").to_string(),
        "today": now.date_naive().format("%Y-%m-%d").to_string(),
    })
}

/// The first date mentioned in the text, or None. A missing year is assumed current or next.
pub fn first_date(text: &str, today: Option<NaiveDate>) -> Option<NaiveDate> {
    let today = today.unwrap_or_else(today_local);
    let m = date_re().captures(text)?;
    let num = |name: &str| -> Option<i32> { m.name(name)?.as_str().parse().ok() };
    if let Some(iso) = m.name("iso") {
        return NaiveDate::parse_from_str(iso.as_str(), "%Y-%m-%d").ok();
    }
    if m.name("m").is_some() {
        return NaiveDate::from_ymd_opt(num("y")?, num("m")? as u32, num("d")? as u32);
    }
    let mon_text = m.name("mon").or_else(|| m.name("mon2"))?.as_str();
    let mon = mon_text.get(..3)?.to_lowercase();
    let month = MONTHS.iter().position(|x| *x == mon)? as u32 + 1;
    let day = num("day").or_else(|| num("day2"))? as u32;
    let year = num("year").or_else(|| num("year2"));
    let found = NaiveDate::from_ymd_opt(year.unwrap_or(today.year()), month, day)?;
    if year.is_none() && (today - found).num_days() > 60 {
        // An invalid date in the following year (Feb 29) is no date at all, as in the Python.
        return found.with_year(today.year() + 1);
    }
    Some(found)
}

pub fn describe_offset(d: NaiveDate, today: Option<NaiveDate>) -> String {
    let delta = (d - today.unwrap_or_else(today_local)).num_days();
    let iso = d.format("%Y-%m-%d");
    if delta == 0 {
        format!("{iso} (today)")
    } else if delta > 0 {
        format!("{iso} (in {delta} days)")
    } else {
        format!("{iso} ({} days ago)", -delta)
    }
}

/// Item index -> 'dated ...' for items containing a date, or 'near a line dated ...' for close
/// neighbours.
pub fn date_hints(
    items: &[Item],
    screen: &Screen,
    today: Option<NaiveDate>,
) -> HashMap<usize, String> {
    // In item order: the nearest-date search keeps the first of a tie, as Python's min() does.
    let dated: Vec<(usize, NaiveDate)> = items
        .iter()
        .filter_map(|it| first_date(&it.text, today).map(|d| (it.index, d)))
        .collect();
    let mut hints: HashMap<usize, String> = dated
        .iter()
        .map(|(i, d)| (*i, format!("dated {}", describe_offset(*d, today))))
        .collect();
    if dated.is_empty() {
        return hints;
    }
    let by_index: HashMap<usize, &Item> = items.iter().map(|it| (it.index, it)).collect();
    for it in items {
        if hints.contains_key(&it.index) {
            continue;
        }
        let cy = it.center().1;
        let distance = |i: usize| (by_index[&i].center().1 - cy).abs();
        let mut nearest = dated[0];
        for &candidate in &dated[1..] {
            if distance(candidate.0) < distance(nearest.0) {
                nearest = candidate;
            }
        }
        if distance(nearest.0) < NEAR_ROWS_PT * screen.scale {
            hints.insert(
                it.index,
                format!("near a line dated {}", describe_offset(nearest.1, today)),
            );
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const TODAY: Option<NaiveDate> = NaiveDate::from_ymd_opt(2026, 9, 16);

    /// tests/conftest.py's `screen` fixture: a 2000x1200 capture at scale 2.
    fn screen() -> Screen {
        Screen {
            bgra: Vec::new(),
            width: 2000,
            height: 1200,
            scale: 2.0,
            app: "Google Chrome".into(),
            field: None,
            url: None,
            pid: None,
            window: None,
            origin: (0.0, 0.0),
            monitor: 0,
            windows: Vec::new(),
            monitors: Vec::new(),
            ax_refs: Default::default(),
            offscreen: Vec::new(),
            pointer: None,
        }
    }

    /// tests/conftest.py's `make_item`, with x1/x2 at their defaults.
    fn make_item(index: usize, text: &str, y1: f64, y2: f64) -> Item {
        Item {
            index,
            text: text.into(),
            ocr_confidence: 1.0,
            x1: 100.0,
            y1,
            x2: 400.0,
            y2,
            role: String::new(),
            source: "ocr".into(),
        }
    }

    #[test]
    fn test_parses_common_forms() {
        assert_eq!(
            first_date("October 13 - 15, 2026", TODAY),
            Some(d(2026, 10, 13))
        );
        assert_eq!(first_date("November 4, 2026", TODAY), Some(d(2026, 11, 4)));
        assert_eq!(
            first_date("Last day to book Sept 18", TODAY),
            Some(d(2026, 9, 18))
        );
        assert_eq!(first_date("Posted 9/1/2026", TODAY), Some(d(2026, 9, 1)));
        assert_eq!(
            first_date("2026-12-01 release", TODAY),
            Some(d(2026, 12, 1))
        );
        assert_eq!(first_date("4 Nov 2026", TODAY), Some(d(2026, 11, 4)));
        assert_eq!(
            first_date("Oct 13\u{2013}15, 2026", TODAY),
            Some(d(2026, 10, 13))
        );
    }

    #[test]
    fn test_no_date_and_invalid_date() {
        assert_eq!(first_date("Register Now", TODAY), None);
        assert_eq!(first_date("Feb 30", TODAY), None);
    }

    #[test]
    fn test_missing_year_rolls_forward_when_well_past() {
        assert_eq!(first_date("Jan 5", TODAY), Some(d(2027, 1, 5)));
        assert_eq!(first_date("Sep 1", TODAY), Some(d(2026, 9, 1)));
    }

    #[test]
    fn test_describe_offset() {
        assert_eq!(describe_offset(d(2026, 9, 16), TODAY), "2026-09-16 (today)");
        assert_eq!(
            describe_offset(d(2026, 10, 13), TODAY),
            "2026-10-13 (in 27 days)"
        );
        assert_eq!(
            describe_offset(d(2026, 9, 1), TODAY),
            "2026-09-01 (15 days ago)"
        );
    }

    #[test]
    fn test_neighbours_inherit_nearest_date() {
        let items = [
            make_item(
                0,
                "TechCrunch Disrupt 2026 | October 13 - 15, 2026",
                1325.0,
                1355.0,
            ),
            make_item(1, "Register Now", 1359.0, 1389.0),
            make_item(2, "Founder Summit | November 4, 2026", 1601.0, 1631.0),
            make_item(3, "Register Now", 1631.0, 1661.0),
            make_item(4, "Footer", 2400.0, 2430.0),
        ];
        let hints = date_hints(&items, &screen(), TODAY);
        assert!(hints[&0].starts_with("dated 2026-10-13"));
        assert_eq!(hints[&1], "near a line dated 2026-10-13 (in 27 days)");
        assert_eq!(hints[&3], "near a line dated 2026-11-04 (in 49 days)");
        assert!(!hints.contains_key(&4));
    }

    #[test]
    fn test_now_context_has_the_three_keys() {
        let ctx = now_context();
        for key in ["local_time", "timezone", "today"] {
            assert!(ctx[key].is_string(), "{key}");
        }
    }
}
