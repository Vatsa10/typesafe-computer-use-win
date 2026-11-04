//! Goal pursuit: the runtime side of "finish the job". For an actionable request it records a goal
//! and a concrete done condition, tracks what was tried and what failed, refuses to let the turn
//! end while the goal is unmet and budget remains, notices loops and forces a change of strategy,
//! and on exhaustion writes a report that says only what is true.
//!
//! Pure bookkeeping: it never calls a tool or a model, so the whole thing is tested with plain
//! strings. `agent::run` feeds it every call and result.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use api::typesafe::{Answer, Question};
use serde_json::{json, Value};

use crate::decide::Classifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalKind {
    /// Fill and submit a web form or application.
    Form,
    /// Open or bring forward an app or tab.
    Open,
    /// Get to a page or an answer on a site or the web.
    Find,
    /// Files, shell and other work with a checkable result.
    Work,
    /// Any other task the decision model says needs the computer, which no keyword recognised.
    General,
}

/// What the decision model (Jev) said about the line: a task for the computer, or conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClass {
    Task,
    Chat,
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub steps: usize,
    pub seconds: f64,
    /// Prompt and output tokens used by the model in this goal: the counts the service reported,
    /// or characters / 4 for a call that reported none.
    pub tokens: usize,
}

/// Ordered ways to change approach when the current one is looping.
pub const NAV_STRATEGIES: [&str; 6] = [
    "open the site's main menu and look under its submenus",
    "scroll to the footer and use its links",
    "use the site's own search box with two or three plain keywords",
    "open the sitemap (/sitemap.xml or /sitemap) and scan it",
    "search the web with site:domain and your terms, in a new tab",
    "go back to a known good page and try a different route",
];

pub const FORM_STRATEGIES: [&str; 5] = [
    "form_read the page and fix every error and empty required field it names",
    "dismiss any banner or overlay, then press the button again",
    "form_fill the required fields one entry at a time and check each reply",
    "go back one page and re-enter it",
    "tell the user exactly what blocks the form",
];

/// Labels tried, least commitment first, to clear a cookie banner, popup or overlay.
pub const BLOCKER_LABELS: [&str; 9] = [
    "Reject all",
    "Reject",
    "Decline",
    "Only necessary",
    "No thanks",
    "Not now",
    "Close",
    "Dismiss",
    "Accept all",
];

const BLOCKER_WORDS: [&str; 11] = [
    "cookie",
    "consent",
    "overlay",
    "popup",
    "pop-up",
    "modal",
    "banner",
    "intercept",
    "obscured",
    "covered by",
    "subscribe to our newsletter",
];

/// Does a tool result suggest something is sitting on top of the page?
pub fn blocker_in(result: &str) -> bool {
    let r = result.to_lowercase();
    BLOCKER_WORDS.iter().any(|w| r.contains(w))
}

#[derive(Debug, Clone)]
pub struct Attempt {
    pub tool: String,
    pub detail: String,
    pub ok: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Finish {
    /// Let the turn end with the model's own words.
    Accept,
    /// Refuse once: send this to the model and go on.
    Nudge(String),
    /// The model ended twice with the goal unmet: use the truthful report instead of its claim.
    Unverified,
}

pub struct Pursuit {
    pub goal: String,
    pub kind: GoalKind,
    pub done_when: String,
    pub budget: Budget,
    pub attempts: Vec<Attempt>,
    /// Calls that were repeated without effect: never run again as they were.
    pub failed_moves: Vec<String>,
    strategy: usize,
    nudged: bool,
    verified: Option<String>,
    needs_user: bool,
    form_touched: bool,
    switch_warned: bool,
    progress: usize,
    since_progress: usize,
    seen: Vec<(String, u64)>,
    pub required_empty: Vec<String>,
    pub blockers_cleared: usize,
    /// The plan the pursuit executes against; empty until a planner supplies one.
    pub plan: Plan,
}

fn hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('~');
    }
    out
}

fn any(text: &str, words: &[&str]) -> bool {
    words.iter().any(|w| text.contains(w))
}

fn is_read_only(tool: &str) -> bool {
    matches!(
        tool,
        "observe" | "world" | "form_read" | "locate" | "list_dir" | "read_file" | "search_history"
            | "list_tasks" | "find_documents" | "get_profile" | "form_answer"
    )
}

fn changes_state(tool: &str) -> bool {
    matches!(
        tool,
        "pointer_run" | "open_app" | "switch_tab" | "form_fill" | "form_autofill" | "form_click"
            | "write_file" | "run_shell"
    )
}

/// Classify a request: `None` for chat, questions and "show me" requests, where the answer or the
/// pointing is the whole job and there is nothing to chase.
pub fn classify(request: &str) -> Option<GoalKind> {
    let t = request.to_lowercase();
    if is_guidance(&t) {
        return None;
    }
    if any(
        &t,
        &[
            "form", "apply", "application", "submit", "sign up", "register", "fill in",
            "fill out", "checkout", "check out",
        ],
    ) {
        return Some(GoalKind::Form);
    }
    if any(&t, &["open ", "launch ", "start the ", "switch to", "bring up"]) {
        return Some(GoalKind::Open);
    }
    if any(
        &t,
        &[
            "find ", "go to", "navigate", "search for", "look up", "visit", "website", "http",
            "www.", ".com", " site", "download", "get me to", "pricing", "policy",
        ],
    ) {
        return Some(GoalKind::Find);
    }
    if any(
        &t,
        &["write ", "create ", "save ", "run ", "list ", "rename", "move ", "delete", "read the file", "make a file"],
    ) {
        return Some(GoalKind::Work);
    }
    None
}

fn done_condition(kind: GoalKind) -> &'static str {
    match kind {
        GoalKind::Form => {
            "the page shows the submission was received, or the Submit press changed the page with no \
             errors and no required field empty"
        }
        GoalKind::Open => "open_app or switch_tab succeeded and the app or tab is in front",
        GoalKind::Find => {
            "you reached the page or answer and read it back from the screen (observe, form_read or a \
             finished pointer_run)"
        }
        GoalKind::Work => "the file or command step succeeded and you read its result",
        GoalKind::General => "the thing asked for is done and you have seen proof on screen or in a tool result",
    }
}

fn is_guidance(t: &str) -> bool {
    any(t, &["show me", "guide me", "walk me through", "teach me", "point at", "point to", "where is the"])
}

impl Pursuit {
    /// A pursuit for an actionable request, or `None` for conversation.
    pub fn begin(request: &str, budget: Budget) -> Option<Pursuit> {
        Self::begin_as(request, budget, None)
    }

    /// Like [`Self::begin`], with the decision model's verdict when there is one. Jev's `Chat`
    /// ends the question; its `Task` is a task even when no keyword names the kind. Guidance
    /// requests ("show me") stay guidance.
    pub fn begin_as(request: &str, budget: Budget, class: Option<TaskClass>) -> Option<Pursuit> {
        let kind = match class {
            Some(TaskClass::Chat) => return None,
            Some(TaskClass::Task) if is_guidance(&request.to_lowercase()) => return None,
            Some(TaskClass::Task) => classify(request).unwrap_or(GoalKind::General),
            None => classify(request)?,
        };
        Some(Pursuit {
            goal: clip(request.trim(), 200),
            kind,
            done_when: done_condition(kind).to_string(),
            budget,
            attempts: Vec::new(),
            failed_moves: Vec::new(),
            strategy: 0,
            nudged: false,
            verified: None,
            needs_user: false,
            form_touched: false,
            switch_warned: false,
            progress: 0,
            since_progress: 0,
            seen: Vec::new(),
            required_empty: Vec::new(),
            blockers_cleared: 0,
            plan: Plan::default(),
        })
    }

    /// Record that code or Jev saw the done condition on screen.
    pub fn mark_verified(&mut self, why: &str) {
        self.verified = Some(why.to_string());
    }

    pub fn met(&self) -> bool {
        self.verified.is_some()
    }

    fn strategies(&self) -> &'static [&'static str] {
        if self.kind == GoalKind::Form {
            &FORM_STRATEGIES
        } else {
            &NAV_STRATEGIES
        }
    }

    pub fn strategies_left(&self) -> bool {
        self.strategy < self.strategies().len()
    }

    /// Why the budget is gone, if it is.
    pub fn out_of_budget(&self, steps: usize, seconds: f64, tokens: usize) -> Option<&'static str> {
        if steps >= self.budget.steps {
            Some("steps")
        } else if seconds >= self.budget.seconds {
            Some("time")
        } else if tokens >= self.budget.tokens {
            Some("cost")
        } else {
            None
        }
    }

    pub fn sig(tool: &str, args: &Value) -> String {
        format!("{tool} {}", args)
    }

    /// Called before a tool runs. `Some(text)` means: do not run it, give the model this as the
    /// result.
    pub fn before_call(&mut self, tool: &str, args: &Value) -> Option<String> {
        let sig = Self::sig(tool, args);
        if self.failed_moves.contains(&sig) {
            return Some(format!(
                "error: this exact call was repeated without effect, so it is blocked. Try: {}.",
                self.next_strategy_text()
            ));
        }
        if tool == "switch_tab" && self.kind == GoalKind::Form && self.form_touched && !self.met() && !self.switch_warned {
            self.switch_warned = true;
            return Some(
                "error: you are in the middle of a form and switching tabs loses your place. Try: \
                 form_read the current page and form_click its Next or Submit; call world first if \
                 the form really moved to another tab, then switch_tab again."
                    .to_string(),
            );
        }
        None
    }

    fn next_strategy_text(&self) -> &'static str {
        let s = self.strategies();
        s[self.strategy.min(s.len() - 1)]
    }

    /// Record a finished call. Returns text to append to its result when the runtime has something
    /// to say (a loop, a forced strategy change, a verified step).
    pub fn after_call(&mut self, tool: &str, args: &Value, result: &str) -> Option<String> {
        let sig = Self::sig(tool, args);
        let v: Option<Value> = serde_json::from_str(result).ok();
        let errored = result.trim_start().starts_with("error");
        let page_stuck = v
            .as_ref()
            .and_then(|v| v.get("page_changed"))
            .and_then(Value::as_bool)
            == Some(false);
        let ok = !errored && !page_stuck;
        if tool.starts_with("form_") {
            self.form_touched = true;
        }
        if result.contains("needs_user") {
            self.needs_user = true;
        }
        if let Some(v) = &v {
            if let Some(a) = v.get("required_still_empty").and_then(Value::as_array) {
                self.required_empty = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
            }
        }
        let rh = hash(result);
        let fresh = !self.seen.iter().any(|(s, h)| s.starts_with(tool) && *h == rh);
        let repeats = self.seen.iter().filter(|(s, _)| *s == sig).count();
        let same_result_run = self
            .seen
            .iter()
            .rev()
            .filter(|(s, _)| s.starts_with(tool))
            .take(2)
            .filter(|(_, h)| *h == rh)
            .count();
        self.seen.push((sig.clone(), rh));
        self.attempts.push(Attempt {
            tool: tool.to_string(),
            detail: clip(&args.to_string(), 80),
            ok,
            note: if ok { String::new() } else { clip(result, 100) },
        });

        if let Some(why) = self.verifies(tool, args, v.as_ref(), ok) {
            self.verified = Some(why);
        }

        if ok && changes_state(tool) && fresh {
            self.progress += 1;
            self.since_progress = 0;
        } else if tool != "interact" {
            self.since_progress += 1;
        }

        let limit = if is_read_only(tool) { 3 } else { 2 };
        let looping = repeats >= limit
            || (tool != "interact" && !ok && same_result_run >= 2)
            || (tool != "interact" && !is_read_only(tool) && same_result_run >= 2)
            || self.since_progress >= 6;
        if looping && !self.met() {
            self.failed_moves.push(sig.clone());
            let strategy = self.next_strategy_text();
            let left = self.strategies_left();
            self.strategy += 1;
            self.since_progress = 0;
            return Some(format!(
                "\n[runtime] Loop: you are repeating yourself with no change on screen ({tool}). That call \
                 is now blocked. {} Change strategy now: {strategy}.",
                if left { "" } else { "You have run out of new strategies: say plainly what blocks you." }
            ));
        }
        None
    }

    fn verifies(&self, tool: &str, args: &Value, v: Option<&Value>, ok: bool) -> Option<String> {
        if !ok {
            return None;
        }
        let dry = v.and_then(|v| v.get("dry_mode")).is_some()
            || v.and_then(|v| v.get("would_launch")).is_some();
        match self.kind {
            GoalKind::Form if tool == "form_click" => {
                let v = v?;
                if v.get("page_changed").and_then(Value::as_bool) != Some(true) {
                    return None;
                }
                let empty = |k: &str| v.get(k).and_then(Value::as_array).is_none_or(|a| a.is_empty());
                if !(empty("errors") && empty("required_still_empty")) {
                    return None;
                }
                let new_text = v.get("new_on_page").map(|x| x.to_string().to_lowercase()).unwrap_or_default();
                let label = args.get("label").and_then(Value::as_str).unwrap_or("").to_lowercase();
                let submit = any(&label, &["submit", "apply", "send", "finish", "place order", "register", "complete", "confirm"]);
                let thanks = any(&new_text, &["thank", "submitted", "received", "confirmation", "success", "application sent"]);
                (submit || thanks).then(|| format!("{label} changed the page with no errors"))
            }
            GoalKind::Form => None,
            GoalKind::Open if matches!(tool, "open_app" | "switch_tab") => {
                Some(if dry { "previewed in Dry mode".into() } else { format!("{tool} succeeded") })
            }
            GoalKind::Open => None,
            GoalKind::Find => {
                let looked = matches!(tool, "observe" | "form_read" | "world") && self.progress > 0;
                let finished = tool == "pointer_run"
                    && v.and_then(|v| v.get("outcome")).and_then(Value::as_str).is_some_and(|o| o == "done" || o == "dry run");
                (looked || finished).then(|| "reached and read the page".to_string())
            }
            GoalKind::Work | GoalKind::General => (matches!(
                tool,
                "write_file" | "run_shell" | "read_file" | "list_dir" | "find_documents" | "start_task"
            ) || (self.kind == GoalKind::General && changes_state(tool)))
            .then(|| format!("{tool} succeeded")),
        }
    }

    /// The model wants to end the turn with `text`.
    pub fn on_final(&mut self, text: &str) -> Finish {
        if self.met() || self.needs_user || text.trim_end().ends_with('?') {
            return Finish::Accept;
        }
        if !self.nudged {
            self.nudged = true;
            let tried: Vec<String> = self
                .attempts
                .iter()
                .rev()
                .take(4)
                .map(|a| format!("{} {}", a.tool, if a.ok { "ok" } else { "failed" }))
                .collect();
            let left = if self.required_empty.is_empty() {
                format!("not yet seen: {}", self.done_when)
            } else {
                format!("required fields still empty: {}", self.required_empty.join(", "))
            };
            return Finish::Nudge(format!(
                "[runtime] You stopped, but the goal is not met. Goal: {}. Done when: {}. Left: {left}. \
                 Last calls: {}. Keep going with a different move; if you are truly blocked, say exactly \
                 what blocks you.",
                self.goal,
                self.done_when,
                if tried.is_empty() { "none".to_string() } else { tried.join(", ") }
            ));
        }
        Finish::Unverified
    }

    /// One line of state for the model, refreshed each request.
    pub fn status(&self, steps: usize) -> String {
        let mut s = format!(
            "Goal: {}. Done when: {}. Step {steps} of {}, {} progress.",
            self.goal, self.done_when, self.budget.steps, self.progress
        );
        if !self.failed_moves.is_empty() {
            s.push_str(&format!(" Failed moves (do not repeat): {}.", self.failed_moves.iter().map(|m| clip(m, 60)).collect::<Vec<_>>().join("; ")));
        }
        s
    }

    /// Whether the recovery playbook should be injected now.
    pub fn needs_recovery(&self) -> bool {
        !self.failed_moves.is_empty() || self.attempts.iter().rev().take(2).filter(|a| !a.ok).count() == 2
    }

    /// The truthful account: what was asked, whether it is verified, what was tried, how far it got.
    pub fn report(&self, why: &str) -> String {
        let mut out = String::new();
        if let Some(v) = &self.verified {
            out.push_str(&format!("Done, and checked: {v}. "));
            out.push_str(&format!("Goal: {}.", self.goal));
            return out;
        }
        out.push_str(&format!("I did not finish: {}. ", self.goal));
        out.push_str(&match why {
            "steps" => "I used all the steps I am allowed.".to_string(),
            "time" => "That took longer than I am allowed.".to_string(),
            "cost" => "I reached the cost limit for one goal.".to_string(),
            "strategies" => "I ran out of new approaches.".to_string(),
            other => other.to_string(),
        });
        out.push_str(&format!(
            " I could not confirm that {}.",
            self.done_when
        ));
        let ok = self.attempts.iter().filter(|a| a.ok).count();
        out.push_str(&format!(
            " I made {} calls, {} worked and {} failed; {} changed something.",
            self.attempts.len(),
            ok,
            self.attempts.len() - ok,
            self.progress
        ));
        let failed: Vec<String> = self
            .attempts
            .iter()
            .filter(|a| !a.ok)
            .rev()
            .take(3)
            .map(|a| format!("{} {}", a.tool, a.detail))
            .collect();
        if !failed.is_empty() {
            out.push_str(&format!(" Last failures: {}.", failed.join("; ")));
        }
        if !self.required_empty.is_empty() {
            out.push_str(&format!(" Required fields still empty: {}.", self.required_empty.join(", ")));
        }
        if self.strategy > 0 {
            out.push_str(&format!(" I changed approach {} time(s).", self.strategy));
        }
        out
    }
}

// ---------------------------------------------------------------- the plan (planner/executor seam)

/// One executable step of a plan: a tool call plus the claim Jev checks against the result.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStep {
    pub tool: String,
    pub args: Value,
    /// What must be true after the step, in plain words, for the verifier.
    pub expect: String,
    pub status: StepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Verified,
    Failed,
    Skipped,
}

/// What the LLM produces once per task: goal, done condition, every answer, and the steps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub goal: String,
    pub done_when: String,
    /// Field label or question to answer, all decided up front (form mappings and so on).
    pub answers: std::collections::BTreeMap<String, String>,
    pub steps: Vec<PlanStep>,
    /// Index of the next step to run.
    pub cursor: usize,
}

impl Plan {
    pub fn next_step(&self) -> Option<&PlanStep> {
        self.steps.get(self.cursor)
    }
    pub fn record(&mut self, status: StepStatus) {
        if let Some(s) = self.steps.get_mut(self.cursor) {
            s.status = status;
            if status == StepStatus::Verified || status == StepStatus::Skipped {
                self.cursor += 1;
            }
        }
    }
    pub fn finished(&self) -> bool {
        self.cursor >= self.steps.len()
    }
}

/// Jev's seat: a fast check of an observed result against a step's claim.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Holds,
    /// The result contradicts the claim.
    Fails(String),
    /// The page is not one the plan expected.
    Surprise(String),
}

pub trait Verifier {
    fn verify(&self, expect: &str, observed: &str) -> Verdict;
}

/// Why the executor goes back to the LLM.
#[derive(Debug, Clone, PartialEq)]
pub enum Escalation {
    UnexpectedPage(String),
    FailedVerification { step: usize, why: String },
    StrategyExhausted,
    BudgetExhausted(String),
}

/// The escalate-to-LLM hook: given the state and why, return a revised plan, or None to stop and
/// report. Called only on a surprise, never per step.
pub trait Replan {
    fn replan(&mut self, why: &Escalation, state: &Pursuit) -> Option<Plan>;
}

impl Pursuit {
    /// Adopt a plan from the planner: its goal and done condition become the pursuit's.
    pub fn adopt(&mut self, plan: Plan) {
        if !plan.goal.is_empty() {
            self.goal = clip(&plan.goal, 200);
        }
        if !plan.done_when.is_empty() {
            self.done_when = plan.done_when.clone();
        }
        self.plan = plan;
    }

    /// Record a step's verdict; returns the escalation the executor must raise, if any.
    pub fn step_verdict(&mut self, v: Verdict) -> Option<Escalation> {
        match v {
            Verdict::Holds => {
                self.plan.record(StepStatus::Verified);
                None
            }
            Verdict::Fails(why) => {
                self.plan.record(StepStatus::Failed);
                Some(Escalation::FailedVerification { step: self.plan.cursor, why })
            }
            Verdict::Surprise(what) => Some(Escalation::UnexpectedPage(what)),
        }
    }

    /// The escalation owed right now for strategy exhaustion, if any.
    pub fn pending_escalation(&self) -> Option<Escalation> {
        self.out_of_strategies().then_some(Escalation::StrategyExhausted)
    }

    pub fn out_of_strategies(&self) -> bool {
        self.strategy > self.strategies().len()
    }
}

// ---------------------------------------------------------------- Jev as the verifier

/// Probability at or above which Jev's yes counts.
pub const YES: f64 = 0.6;
/// At or below which its no counts; between the two the page is a surprise, not a failure.
pub const NO: f64 = 0.35;

/// The verifier: exact code checks first, then TypeSafe (a Noul or a Choice, about 0.4 s) for what
/// code cannot read exactly. It never calls the large model.
pub struct JevVerifier {
    classifier: Arc<dyn Classifier + Send + Sync>,
    timeout: Duration,
    cancel: api::chat::Cancel,
    calls: AtomicUsize,
}

impl JevVerifier {
    pub fn new(classifier: Arc<dyn Classifier + Send + Sync>, timeout: Duration, cancel: api::chat::Cancel) -> Self {
        Self { classifier, timeout, cancel, calls: AtomicUsize::new(0) }
    }

    /// How many TypeSafe requests this verifier has made.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn ask(&self, state: Value, questions: Vec<(String, Question)>) -> Option<Vec<(String, Answer)>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        super::fastpath::ask(self.classifier.clone(), state, questions, self.timeout, &self.cancel).ok()
    }

    fn noul(&self, state: Value, instructions: &str) -> Option<f64> {
        let q = vec![("ok".to_string(), Question::Noul { instructions: instructions.to_string() })];
        self.ask(state, q)?.into_iter().find_map(|(_, a)| match a {
            Answer::Noul(p) => Some(p),
            _ => None,
        })
    }

    /// Exact checks on a tool result: `Some` when the result itself decides.
    pub fn code_check(observed: &str) -> Option<Verdict> {
        let v: Value = serde_json::from_str(observed).ok()?;
        let non_empty = |k: &str| v.get(k).and_then(Value::as_array).is_some_and(|a| !a.is_empty());
        if let Some(rows) = v.get("results").and_then(Value::as_array) {
            let bad: Vec<String> = rows
                .iter()
                .filter(|r| r["status"] != "ok")
                .map(|r| format!("{} {}", r["field"].as_str().unwrap_or("?"), r["status"].as_str().unwrap_or("?")))
                .collect();
            if !bad.is_empty() {
                return Some(Verdict::Fails(format!("not read back as typed: {}", bad.join(", "))));
            }
            return Some(Verdict::Holds);
        }
        if let Some(changed) = v.get("page_changed").and_then(Value::as_bool) {
            if non_empty("errors") {
                return Some(Verdict::Fails(format!("the page shows errors: {}", v["errors"])));
            }
            return Some(if changed {
                Verdict::Holds
            } else {
                Verdict::Fails("the page did not change".into())
            });
        }
        None
    }

    /// Is this page one the task expects? `page` is its visible text and labels.
    pub fn expected_page(&self, goal: &str, page: &str) -> Verdict {
        let state = json!({ "goal": goal, "page": page.chars().take(1500).collect::<String>() });
        match self.noul(
            state,
            "Is this page part of getting the goal done: a step of the form or application, a review \
             page, or the confirmation that follows it? Say no for a login wall, an error page, an \
             unrelated site or a page that asks for payment the goal did not mention.",
        ) {
            Some(p) if p <= NO => Verdict::Surprise(format!("this page does not look like part of the task ({p:.2})")),
            _ => Verdict::Holds,
        }
    }

    /// Of the visible buttons, the one that moves the task forward: `Some((index, confidence))`.
    pub fn pick_button(&self, goal: &str, buttons: &[String], hint: &str) -> Option<(usize, f64)> {
        if buttons.is_empty() {
            return None;
        }
        let mut criteria: Vec<(String, Option<String>)> =
            buttons.iter().enumerate().map(|(i, b)| (format!("btn_{i}"), Some(format!("Button: {b}")))).collect();
        criteria.push(("none".into(), Some("No listed button moves the task forward.".into())));
        let q = Question::Choice {
            instructions: "Which button moves the goal forward: Next, Continue, Review or the final Submit? \
                           Never Back, Cancel, Save for later, Sign in or a social login."
                .into(),
            criteria,
        };
        let state = json!({ "goal": goal, "planner_hint": hint, "buttons": buttons });
        let answers = self.ask(state, vec![("button".to_string(), q)])?;
        answers.into_iter().find_map(|(_, a)| match a {
            Answer::Choice(c) => c.choice.strip_prefix("btn_")?.parse::<usize>().ok().map(|i| (i, c.confidence)),
            _ => None,
        })
    }

    /// Does this text say the form was received? None when Jev could not answer.
    pub fn submitted(&self, goal: &str, page: &str) -> Option<bool> {
        let state = json!({ "goal": goal, "page": page.chars().take(1500).collect::<String>() });
        self.noul(
            state,
            "Does the page say the submission or application was received, such as a thank you or a \
             confirmation? Say no for a form that is still being filled or that shows errors.",
        )
        .map(|p| p >= YES)
    }
}

impl Verifier for JevVerifier {
    fn verify(&self, expect: &str, observed: &str) -> Verdict {
        if let Some(v) = Self::code_check(observed) {
            return v;
        }
        let state = json!({ "expected": expect, "observed": observed.chars().take(1500).collect::<String>() });
        match self.noul(state, "Does what was observed show that the expected result happened?") {
            Some(p) if p >= YES => Verdict::Holds,
            Some(p) if p <= NO => Verdict::Fails(format!("the result does not show: {expect}")),
            Some(_) => Verdict::Surprise(format!("unclear whether: {expect}")),
            None => Verdict::Surprise("the verifier was unavailable".into()),
        }
    }
}

/// What to try next after a failed tool, for a result that gave no advice of its own.
pub fn next_hint(tool: &str, result: &str) -> Option<String> {
    let r = result.trim_start();
    if !r.starts_with("error") || (r.to_lowercase().contains("try ") || r.to_lowercase().contains("try:")) {
        return None;
    }
    let hint = match tool {
        "observe" | "locate" => "call world to see which window is in front, then observe again",
        "open_app" => "call world for the exact app key, or pointer_run to open it from the Start menu",
        "switch_tab" => "call world for the exact tab title and try again",
        "pointer_run" => "reword the goal in one plain sentence, or use form_read and form_click for a web form; dismiss any overlay first",
        "form_read" | "form_fill" | "form_autofill" | "form_click" => {
            "call form_read to see the page, use the exact labels it shows, and dismiss any banner or overlay first"
        }
        "read_file" | "list_dir" => "call find_documents, or list the parent folder",
        "write_file" | "run_shell" => "if the user refused, carry on without it and say so",
        "interact" => "give at least one of speech, text or marks, using item numbers from your latest observe",
        _ => "use a different tool, or ask the user once",
    };
    Some(format!("\n(try next: {hint})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn budget() -> Budget {
        Budget { steps: 10, seconds: 60.0, tokens: 100_000 }
    }

    fn form() -> Pursuit {
        Pursuit::begin("fill out the application form", budget()).unwrap()
    }

    #[test]
    fn chat_and_guidance_have_no_pursuit() {
        assert!(Pursuit::begin("what is the capital of France", budget()).is_none());
        assert!(Pursuit::begin("show me how to save", budget()).is_none());
        assert_eq!(classify("open notepad"), Some(GoalKind::Open));
        assert_eq!(classify("find the privacy policy on this site"), Some(GoalKind::Find));
    }

    #[test]
    fn the_same_call_three_times_is_a_loop_and_then_blocked() {
        let mut p = Pursuit::begin("find the pricing page", budget()).unwrap();
        let a = json!({"goal": "open pricing"});
        assert!(p.after_call("pointer_run", &a, "{\"outcome\":\"failed\"}").is_none());
        assert!(p.after_call("pointer_run", &a, "{\"outcome\":\"failed\"}").is_none());
        let note = p.after_call("pointer_run", &a, "{\"outcome\":\"failed\"}");
        assert!(note.unwrap().contains("Change strategy now: open the site's main menu"));
        assert!(p.before_call("pointer_run", &a).unwrap().starts_with("error: this exact call"));
        // The next loop moves down the list.
        let b = json!({"goal": "again"});
        p.after_call("pointer_run", &b, "x");
        p.after_call("pointer_run", &b, "x");
        let note = p.after_call("pointer_run", &b, "x").unwrap();
        assert!(note.contains("footer"), "{note}");
    }

    #[test]
    fn no_screen_change_for_six_calls_is_a_loop() {
        let mut p = Pursuit::begin("find the pricing page", budget()).unwrap();
        let mut said = None;
        for i in 0..6 {
            said = p.after_call("observe", &json!({"n": i}), &format!("page {i}"));
        }
        assert!(said.unwrap().contains("Loop"));
    }

    #[test]
    fn ending_early_is_refused_once_with_what_is_left() {
        let mut p = form();
        p.after_call("form_click", &json!({"label": "Next"}), &json!({"page_changed": true, "errors": [], "required_still_empty": ["Phone"]}).to_string());
        match p.on_final("All done!") {
            Finish::Nudge(m) => {
                assert!(m.contains("Phone") && m.contains("Goal:"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(p.on_final("All done!"), Finish::Unverified);
    }

    #[test]
    fn a_verified_submit_lets_the_turn_end() {
        let mut p = form();
        let r = json!({"page_changed": true, "errors": [], "required_still_empty": [], "new_on_page": ["Thank you"]});
        p.after_call("form_click", &json!({"label": "Submit"}), &r.to_string());
        assert!(p.met());
        assert_eq!(p.on_final("Submitted."), Finish::Accept);
        assert!(p.report("steps").starts_with("Done, and checked"));
    }

    #[test]
    fn a_next_that_changed_nothing_is_not_verification() {
        let mut p = form();
        let r = json!({"page_changed": false, "errors": ["Email"], "required_still_empty": []});
        p.after_call("form_click", &json!({"label": "Submit"}), &r.to_string());
        assert!(!p.met());
    }

    #[test]
    fn a_question_to_the_user_may_end_the_turn() {
        let mut p = form();
        assert_eq!(p.on_final("What is your notice period?"), Finish::Accept);
    }

    #[test]
    fn switching_tabs_mid_form_is_refused_once() {
        let mut p = form();
        p.after_call("form_read", &json!({}), "{}");
        let first = p.before_call("switch_tab", &json!({"title": "x"}));
        assert!(first.unwrap().contains("middle of a form"));
        assert!(p.before_call("switch_tab", &json!({"title": "x"})).is_none());
    }

    #[test]
    fn the_report_is_truthful() {
        let mut p = Pursuit::begin("find the pricing page", budget()).unwrap();
        p.after_call("pointer_run", &json!({"goal": "g"}), "error: nothing found");
        let r = p.report("steps");
        assert!(r.starts_with("I did not finish"), "{r}");
        assert!(r.contains("1 failed") && r.contains("could not confirm"), "{r}");
        assert!(!r.to_lowercase().contains("done"), "{r}");
    }

    #[test]
    fn budgets_report_which_limit_was_hit() {
        let p = form();
        assert_eq!(p.out_of_budget(10, 0.0, 0), Some("steps"));
        assert_eq!(p.out_of_budget(1, 61.0, 0), Some("time"));
        assert_eq!(p.out_of_budget(1, 1.0, 100_000), Some("cost"));
        assert_eq!(p.out_of_budget(1, 1.0, 1), None);
    }

    #[test]
    fn failures_say_what_to_try_next() {
        assert!(next_hint("switch_tab", "error: no tab").unwrap().contains("world"));
        assert!(next_hint("switch_tab", "{}").is_none());
        assert!(next_hint("observe", "error: x, try again").is_none());
        for t in ["observe", "open_app", "pointer_run", "form_click", "read_file", "interact", "zzz"] {
            assert!(next_hint(t, "error: bad").is_some(), "{t}");
        }
    }

    struct Once(Option<Plan>);
    impl Replan for Once {
        fn replan(&mut self, _: &Escalation, _: &Pursuit) -> Option<Plan> {
            self.0.take()
        }
    }

    #[test]
    fn a_plan_runs_and_escalates_only_on_a_surprise() {
        let mut p = form();
        let step = |t: &str| PlanStep { tool: t.into(), args: json!({}), expect: "ok".into(), status: StepStatus::Pending };
        p.adopt(Plan { goal: "apply".into(), done_when: "thanks page".into(), steps: vec![step("form_autofill"), step("form_click")], ..Plan::default() });
        assert_eq!(p.done_when, "thanks page");
        assert!(p.step_verdict(Verdict::Holds).is_none());
        let esc = p.step_verdict(Verdict::Surprise("login wall".into())).unwrap();
        assert_eq!(esc, Escalation::UnexpectedPage("login wall".into()));
        let mut llm = Once(Some(Plan { goal: "apply".into(), ..Plan::default() }));
        assert!(llm.replan(&esc, &p).is_some());
        assert!(llm.replan(&esc, &p).is_none());
        assert!(matches!(p.step_verdict(Verdict::Fails("x".into())), Some(Escalation::FailedVerification { .. })));
        assert!(!p.plan.finished());
    }

    #[test]
    fn jev_decides_task_or_chat_and_keywords_only_name_the_kind() {
        // No keyword here, but Jev says it is a task.
        let line = "tidy up my desktop icons";
        assert!(Pursuit::begin(line, budget()).is_some() || classify(line).is_none());
        assert_eq!(Pursuit::begin_as(line, budget(), Some(TaskClass::Task)).unwrap().kind, GoalKind::General);
        // A keyword that would start a pursuit does not when Jev says chat.
        assert!(Pursuit::begin_as("what does a submit button do", budget(), Some(TaskClass::Chat)).is_none());
        assert!(Pursuit::begin_as("what does a submit button do", budget(), None).is_some());
        // Guidance stays guidance.
        assert!(Pursuit::begin_as("show me how to save", budget(), Some(TaskClass::Task)).is_none());
        // A task Jev named keeps its keyword kind.
        assert_eq!(Pursuit::begin_as("apply to this job", budget(), Some(TaskClass::Task)).unwrap().kind, GoalKind::Form);
    }

    #[test]
    fn blockers_are_recognised() {
        assert!(blocker_in("click intercepted by the cookie banner"));
        assert!(!blocker_in("{\"page_changed\":true}"));
    }
}
