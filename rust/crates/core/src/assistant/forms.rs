//! Filling a form field by field, in the page itself: text fields, radio groups, checkboxes and
//! selects, with each result checked by reading the page back. The matching is pure, so it is
//! tested with fake fields; the real page is read and driven through [`super::Senses`].

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct FormField {
    /// `text`, `radio`, `checkbox` or `select`.
    pub kind: String,
    pub label: String,
    /// The question a radio or checkbox belongs to.
    pub group: String,
    pub value: String,
    pub checked: bool,
    pub required: bool,
}

/// What the front page holds: its fields, its buttons (label, enabled) and its text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageView {
    pub fields: Vec<FormField>,
    pub buttons: Vec<(String, bool)>,
    pub texts: Vec<String>,
}

/// What one click on the page did, after the page settled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClickReport {
    pub clicked: String,
    /// Whether the page's content differs from before the click.
    pub changed: bool,
    pub settle_ms: u64,
    /// Visible text that looks like a validation error.
    pub errors: Vec<String>,
    /// Required fields still empty, by label.
    pub required_empty: Vec<String>,
    /// Text that appeared on the page because of the click (first few, clipped).
    pub new_text: Vec<String>,
}

/// Text that reads as a validation message.
pub fn looks_like_error(t: &str) -> bool {
    let l = t.to_lowercase();
    ["required", "invalid", "error", "must ", "please ", "incorrect", "try again", "cannot be", "can't be", "pick the"]
        .iter()
        .any(|w| l.contains(w))
}

/// The enabled button `label` names: exact first, else a single partial match.
pub fn pick_button(buttons: &[(String, bool)], label: &str) -> Result<usize, String> {
    let w = norm(label);
    let all: Vec<(usize, String)> = buttons.iter().enumerate().map(|(i, (l, _))| (i, l.clone())).collect();
    let i = match pick(&all, &w) {
        Found::One(i) => i,
        Found::Many => return Err(format!("more than one button matches {label:?}")),
        Found::None => return Err(format!("no button named {label:?} on the page")),
    };
    if !buttons[i].1 {
        return Err(format!("the button {:?} is disabled", buttons[i].0));
    }
    Ok(i)
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormEntry {
    /// A field's label, or a radio or checkbox group's question.
    pub field: String,
    /// The text to type, the option to choose, or true/false for a checkbox.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Type(String),
    Check(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub entry: usize,
    pub control: usize,
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormResult {
    pub field: String,
    /// ok, skipped, not_found, ambiguous or mismatch.
    pub status: String,
}

pub fn norm(s: &str) -> String {
    s.to_lowercase()
        .replace("required", " ")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truthy(v: &str) -> bool {
    matches!(norm(v).as_str(), "true" | "yes" | "y" | "checked" | "on" | "1" | "tick")
}

enum Found {
    One(usize),
    None,
    Many,
}

/// The one of `candidates` whose label is `want` exactly, else the one that contains it or is
/// contained by it.
fn pick(candidates: &[(usize, String)], want: &str) -> Found {
    let w = norm(want);
    if w.is_empty() {
        return Found::None;
    }
    let exact: Vec<usize> = candidates.iter().filter(|(_, l)| norm(l) == w).map(|(i, _)| *i).collect();
    match exact.len() {
        1 => return Found::One(exact[0]),
        n if n > 1 => return Found::Many,
        _ => {}
    }
    let part: Vec<usize> = candidates
        .iter()
        .filter(|(_, l)| {
            let l = norm(l);
            !l.is_empty() && w.len() >= 3 && (l.contains(&w) || (l.len() >= 3 && w.contains(&l)))
        })
        .map(|(i, _)| *i)
        .collect();
    match part.len() {
        0 => Found::None,
        1 => Found::One(part[0]),
        _ => Found::Many,
    }
}

/// What to do for each entry, or why it cannot be done.
pub fn plan(fields: &[FormField], entries: &[FormEntry]) -> (Vec<Planned>, Vec<FormResult>) {
    let mut planned = Vec::new();
    let mut failed = Vec::new();
    for (e, entry) in entries.iter().enumerate() {
        let fail = |status: &str| FormResult { field: entry.field.clone(), status: status.into() };
        if entry.value.trim().is_empty() {
            failed.push(fail("skipped"));
            continue;
        }
        let of = |kinds: &[&str], by_group: bool| -> Vec<(usize, String)> {
            fields
                .iter()
                .enumerate()
                .filter(|(_, f)| kinds.contains(&f.kind.as_str()))
                .map(|(i, f)| (i, if by_group { f.group.clone() } else { f.label.clone() }))
                .collect()
        };
        // 1. A text field or select by its label.
        match pick(&of(&["text", "select"], false), &entry.field) {
            Found::One(i) => {
                planned.push(Planned { entry: e, control: i, op: Op::Type(entry.value.clone()) });
                continue;
            }
            Found::Many => {
                failed.push(fail("ambiguous"));
                continue;
            }
            Found::None => {}
        }
        // 2. A radio group by its question: the value names the option.
        let radios = of(&["radio"], true);
        let in_group: Vec<(usize, String)> = match pick(&radios, &entry.field) {
            Found::One(i) => {
                let g = norm(&fields[i].group);
                fields
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.kind == "radio" && norm(&f.group) == g)
                    .map(|(i, f)| (i, f.label.clone()))
                    .collect()
            }
            _ => {
                // The same question repeats on every option: Many is fine if they share a group.
                let hits: Vec<&(usize, String)> = radios.iter().filter(|(_, g)| norm(g) == norm(&entry.field)).collect();
                hits.iter().map(|(i, _)| (*i, fields[*i].label.clone())).collect()
            }
        };
        if !in_group.is_empty() {
            match pick(&in_group, &entry.value) {
                Found::One(i) => planned.push(Planned { entry: e, control: i, op: Op::Check(true) }),
                Found::Many => failed.push(fail("ambiguous")),
                Found::None => failed.push(fail("not_found")),
            }
            continue;
        }
        // 3. A checkbox by its own label, or by its group with the option as the value.
        match pick(&of(&["checkbox"], false), &entry.field) {
            Found::One(i) => {
                planned.push(Planned { entry: e, control: i, op: Op::Check(truthy(&entry.value)) });
                continue;
            }
            Found::Many => {
                failed.push(fail("ambiguous"));
                continue;
            }
            Found::None => {}
        }
        let boxes: Vec<(usize, String)> = fields
            .iter()
            .enumerate()
            .filter(|(_, f)| f.kind == "checkbox" && norm(&f.group) == norm(&entry.field))
            .map(|(i, f)| (i, f.label.clone()))
            .collect();
        if boxes.is_empty() {
            failed.push(fail("not_found"));
            continue;
        }
        match pick(&boxes, &entry.value) {
            Found::One(i) => planned.push(Planned { entry: e, control: i, op: Op::Check(true) }),
            Found::Many => failed.push(fail("ambiguous")),
            Found::None => failed.push(fail("not_found")),
        }
    }
    (planned, failed)
}

/// Whether the page now holds what was planned.
pub fn holds(after: &[FormField], p: &Planned) -> bool {
    let Some(f) = after.get(p.control) else { return false };
    match &p.op {
        Op::Type(v) => norm(&f.value) == norm(v),
        Op::Check(want) => f.checked == *want,
    }
}

pub fn fields_json(view: &PageView) -> Value {
    let fields = &view.fields;
    let rows: Vec<Value> = fields
        .iter()
        .map(|f| match f.kind.as_str() {
            "radio" | "checkbox" => json!({ "kind": f.kind, "label": f.label, "question": f.group,
                "checked": f.checked, "required": f.required }),
            _ => json!({ "kind": f.kind, "label": f.label, "value": f.value, "required": f.required }),
        })
        .collect();
    let buttons: Vec<Value> = view.buttons.iter().map(|(l, e)| json!({ "label": l, "enabled": e })).collect();
    let texts: Vec<&String> = view.texts.iter().take(40).collect();
    json!({ "fields": rows, "buttons": buttons, "page_text": texts })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(kind: &str, label: &str, group: &str) -> FormField {
        FormField { kind: kind.into(), label: label.into(), group: group.into(), value: String::new(), checked: false, required: false }
    }

    fn page() -> Vec<FormField> {
        vec![
            f("text", "Full name *", ""),
            f("text", "Email *", ""),
            f("text", "Expected salary", ""),
            f("radio", "0-1", "Years of experience *"),
            f("radio", "2-4", "Years of experience *"),
            f("radio", "5+", "Years of experience *"),
            f("checkbox", "Python", "Which of these do you know?"),
            f("checkbox", "Rust", "Which of these do you know?"),
            f("checkbox", "COBOL", "Which of these do you know?"),
            f("radio", "Yes", "Do you need visa sponsorship?"),
            f("radio", "No", "Do you need visa sponsorship?"),
            f("checkbox", "I certify that everything above is true", "COBOL"),
        ]
    }

    fn e(field: &str, value: &str) -> FormEntry {
        FormEntry { field: field.into(), value: value.into() }
    }

    #[test]
    fn text_radio_and_checkbox_entries_find_their_controls() {
        let (p, failed) = plan(
            &page(),
            &[
                e("Full name", "A B"),
                e("Years of experience", "5+"),
                e("Rust", "true"),
                e("Which of these do you know", "Python"),
                e("Do you need visa sponsorship?", "No"),
                e("I certify that everything above is true", "yes"),
            ],
        );
        assert!(failed.is_empty(), "{failed:?}");
        let got: Vec<(usize, Op)> = p.iter().map(|x| (x.control, x.op.clone())).collect();
        assert_eq!(
            got,
            vec![
                (0, Op::Type("A B".into())),
                (5, Op::Check(true)),
                (7, Op::Check(true)),
                (6, Op::Check(true)),
                (10, Op::Check(true)),
                (11, Op::Check(true)),
            ]
        );
    }

    #[test]
    fn what_is_not_there_or_blank_is_reported_never_guessed() {
        let (p, failed) = plan(&page(), &[e("Favourite colour", "green"), e("Expected salary", " "), e("Years of experience", "9")]);
        assert!(p.is_empty());
        let st: Vec<&str> = failed.iter().map(|r| r.status.as_str()).collect();
        assert_eq!(st, vec!["not_found", "skipped", "not_found"]);
    }

    #[test]
    fn a_checkbox_is_unticked_only_when_told_so_and_cobol_stays_off() {
        let (p, _) = plan(&page(), &[e("Rust", "no")]);
        assert_eq!(p[0].op, Op::Check(false));
        let (p, _) = plan(&page(), &[e("Rust", "yes")]);
        assert!(p.iter().all(|x| x.control != 8), "COBOL is never touched");
    }

    #[test]
    fn buttons_and_errors_are_recognised() {
        let b = vec![("Back".to_string(), true), ("Next".to_string(), true), ("Next results".to_string(), false)];
        assert_eq!(pick_button(&b, "next"), Ok(1));
        assert!(pick_button(&b, "Next results").unwrap_err().contains("disabled"));
        assert!(pick_button(&b, "Submit").is_err());
        assert!(looks_like_error("This is a required question") && !looks_like_error("Page 2 of 3"));
    }

    #[test]
    fn holds_reads_the_page_back() {
        let mut after = page();
        after[0].value = "A  B".into();
        after[5].checked = true;
        let (p, _) = plan(&page(), &[e("Full name", "A B"), e("Years of experience", "5+"), e("Email", "x@y.z")]);
        assert!(holds(&after, &p[0]) && holds(&after, &p[1]) && !holds(&after, &p[2]));
    }
}
