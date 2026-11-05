//! What Pointer knows about the page it is working on, kept up to date after every move.
//!
//! Each look at the page is compared with the last, so the decision is made from a running picture
//! rather than a snapshot: what appeared, what went away, what has been entered, and which moves
//! were already tried on this very screen and what they did. Nothing here knows any site or word.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Value};

use super::forms::PageView;

const MAX_DIFF_LINES: usize = 20;
const MAX_LINE: usize = 120;

fn clip(s: &str) -> String {
    s.chars().take(MAX_LINE).collect()
}

/// A screen's identity: its controls and inputs. Text can tick along without it being a new screen.
fn screen_of(view: &PageView) -> String {
    let inputs = view.fields.iter().map(|f| format!("{}|{}|{}", f.kind, f.group, f.label));
    let controls = view.buttons.iter().map(|(l, _)| l.clone());
    inputs.chain(controls).collect::<Vec<_>>().join("\n")
}

#[derive(Debug, Default)]
pub struct PageState {
    screen: String,
    text: HashSet<String>,
    /// What the last look found that the one before did not, and what went away.
    appeared: Vec<String>,
    gone: Vec<String>,
    /// What has been entered, across every page of the task.
    entered: BTreeMap<String, String>,
    /// Screen, then move, then what it did.
    tried: HashMap<String, HashMap<String, String>>,
    screens_seen: HashSet<String>,
    looks: usize,
}

impl PageState {
    /// Take in a fresh look at the page.
    pub fn look(&mut self, view: &PageView) {
        let now: HashSet<String> = view.texts.iter().map(|t| clip(t)).collect();
        self.appeared = view.texts.iter().map(|t| clip(t)).filter(|t| !self.text.contains(t)).take(MAX_DIFF_LINES).collect();
        self.gone = self.text.difference(&now).take(MAX_DIFF_LINES).cloned().collect();
        self.gone.sort();
        self.text = now;
        self.screen = screen_of(view);
        self.screens_seen.insert(self.screen.clone());
        self.looks += 1;
    }

    /// Entering something changes what a press can do, so what was tried on this screen starts over.
    pub fn entered(&mut self, field: &str, value: &str) {
        self.entered.insert(field.to_string(), value.to_string());
        self.tried.remove(&self.screen);
    }

    /// Remember what a move did on the current screen.
    pub fn tried(&mut self, action: &str, outcome: &str) {
        self.tried.entry(self.screen.clone()).or_default().insert(action.to_string(), outcome.to_string());
    }

    /// The page asked for a fix, so what was tried here may work now.
    pub fn forget_tried(&mut self) {
        self.tried.remove(&self.screen);
    }

    /// Controls that are still worth pressing here: a move that changed nothing on this exact screen
    /// will not change anything the second time.
    pub fn worth_trying(&self, controls: &[String]) -> Vec<String> {
        let here = self.tried.get(&self.screen);
        controls
            .iter()
            .filter(|c| here.and_then(|t| t.get(c.as_str())).is_none_or(|o| o != NOTHING_CHANGED))
            .cloned()
            .collect()
    }

    /// The running picture, as Jev and the planner read it.
    pub fn summary(&self) -> Value {
        let here: Vec<Value> = self
            .tried
            .get(&self.screen)
            .map(|t| t.iter().map(|(a, o)| json!({ "move": a, "result": o })).collect())
            .unwrap_or_default();
        json!({
            "appeared_since_last_look": self.appeared,
            "gone_since_last_look": self.gone,
            "entered_so_far": self.entered,
            "already_tried_on_this_screen": here,
            "screens_seen": self.screens_seen.len(),
        })
    }
}

pub const NOTHING_CHANGED: &str = "nothing changed";

#[cfg(test)]
mod tests {
    use super::*;

    fn view(buttons: &[&str], texts: &[&str]) -> PageView {
        PageView {
            buttons: buttons.iter().map(|b| (b.to_string(), true)).collect(),
            texts: texts.iter().map(|t| t.to_string()).collect(),
            ..PageView::default()
        }
    }

    #[test]
    fn each_look_is_compared_with_the_last() {
        let mut p = PageState::default();
        p.look(&view(&["Go"], &["Results 1 of 3", "Alpha"]));
        p.look(&view(&["Go"], &["Results 2 of 3", "Alpha", "Beta"]));
        let s = p.summary();
        assert_eq!(s["appeared_since_last_look"], json!(["Results 2 of 3", "Beta"]));
        assert_eq!(s["gone_since_last_look"], json!(["Results 1 of 3"]));
    }

    #[test]
    fn a_move_that_did_nothing_here_is_not_offered_again_here() {
        let mut p = PageState::default();
        let controls = vec!["Go".to_string(), "Stop".to_string()];
        p.look(&view(&["Go", "Stop"], &[]));
        p.tried("Go", NOTHING_CHANGED);
        assert_eq!(p.worth_trying(&controls), vec!["Stop".to_string()]);
        // On a different screen the same label is a fresh move.
        p.look(&view(&["Go", "Stop", "Back"], &[]));
        assert_eq!(p.worth_trying(&controls), controls);
    }

    #[test]
    fn what_was_entered_is_kept_across_pages() {
        let mut p = PageState::default();
        p.look(&view(&["Next"], &[]));
        p.entered("Name", "Asha");
        p.look(&view(&["Submit"], &[]));
        assert_eq!(p.summary()["entered_so_far"]["Name"], "Asha");
    }
}
