//! The executor against a fake multi-page site, a scripted planner and a fake Jev. No network,
//! no screen. `live_simulation` (ignored) runs the same site with the real models.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api::chat::{Cancel, Chat};
use api::typesafe::{Answer, ApiError, ChoiceAnswer, Question};
use serde_json::{json, Value};

use super::*;
use crate::assistant::forms::{self, ClickReport, FormResult, Target};
use crate::assistant::pursuit::{Budget, GoalKind};
use crate::assistant::testing::{call, calling, FakeChat, Harness};
use crate::assistant::{Limits, Observation, Senses};
use crate::decide::Classifier;
use crate::documents::Work;

fn f(kind: &str, label: &str, group: &str, required: bool) -> FormField {
    FormField { kind: kind.into(), label: label.into(), group: group.into(), value: String::new(), checked: false, required, password: false }
}

/// A site with three pages, the way a careers site has them. Nothing in the executor knows it.
pub(super) struct Site {
    pages: Vec<(Vec<FormField>, Vec<&'static str>, Vec<&'static str>)>,
    pub page: Mutex<usize>,
    pub typed: Mutex<HashMap<String, String>>,
    pub fills: AtomicUsize,
    pub clicks: Mutex<Vec<String>>,
    pub scrolls: AtomicUsize,
    /// The first press of Next shows an error and stays.
    pub fail_next_once: Mutex<bool>,
    pub objection: Mutex<Option<String>>,
    pub password_page: bool,
}

impl Site {
    pub(super) fn new() -> Arc<Site> {
        let mut pw = f("text", "Password", "", true);
        pw.password = true;
        let _ = pw;
        Arc::new(Site {
            pages: vec![
                (
                    vec![f("text", "Full name *", "", true), f("text", "Email *", "", true), f("text", "Phone", "", false)],
                    vec!["Next"],
                    vec!["Step 1 of 2: about you"],
                ),
                (
                    vec![
                        f("text", "Why do you want this role? *", "", true),
                        f("radio", "0-1", "Years of experience *", true),
                        f("radio", "2-4", "Years of experience *", true),
                        f("radio", "5+", "Years of experience *", true),
                        f("text", "Expected salary", "", false),
                    ],
                    vec!["Back", "Submit application"],
                    vec!["Step 2 of 2: questions"],
                ),
                (vec![], vec![], vec!["Thank you, your application was submitted."]),
            ],
            page: Mutex::new(0),
            typed: Mutex::new(HashMap::new()),
            fills: AtomicUsize::new(0),
            clicks: Mutex::new(vec![]),
            scrolls: AtomicUsize::new(0),
            fail_next_once: Mutex::new(false),
            objection: Mutex::new(None),
            password_page: false,
        })
    }

    fn view(&self) -> PageView {
        let p = *self.page.lock().unwrap();
        let (fields, buttons, texts) = &self.pages[p];
        let typed = self.typed.lock().unwrap();
        let mut fields = fields.clone();
        if self.password_page && p == 0 {
            let mut pw = f("text", "Password", "", true);
            pw.password = true;
            fields.push(pw);
        }
        for fd in &mut fields {
            if fd.kind == "text" {
                fd.value = typed.get(&forms::norm(&fd.label)).cloned().unwrap_or_default();
            } else if fd.kind == "radio" {
                fd.checked = typed.get(&forms::norm(&fd.group)).is_some_and(|v| forms::norm(v) == forms::norm(&fd.label));
            }
        }
        PageView {
            hwnd: 77,
            fields,
            buttons: buttons.iter().map(|b| (b.to_string(), true)).collect(),
            texts: texts.iter().map(|t| t.to_string()).chain(self.objection.lock().unwrap().clone()).collect(),
        }
    }
}

impl Senses for Site {
    fn observe(&self) -> Result<Observation, String> {
        Err("no observe".into())
    }
    fn form_read(&self, t: &Target) -> Result<PageView, String> {
        match t {
            Target::Bound(h) if *h != 77 => Err("gone".into()),
            _ => Ok(self.view()),
        }
    }
    fn form_fill(&self, _target: isize, entries: &[FormEntry]) -> Result<Vec<FormResult>, String> {
        self.fills.fetch_add(1, Ordering::SeqCst);
        let view = self.view();
        let (planned, mut results) = forms::plan(&view.fields, entries);
        let mut typed = self.typed.lock().unwrap();
        for p in &planned {
            let fd = &view.fields[p.control];
            let key = if fd.kind == "radio" { forms::norm(&fd.group) } else { forms::norm(&fd.label) };
            typed.insert(key, match &p.op {
                forms::Op::Type(v) => v.clone(),
                forms::Op::Check(_) => fd.label.clone(),
            });
            results.push(FormResult { field: entries[p.entry].field.clone(), status: "ok".into() });
        }
        Ok(results)
    }
    fn form_click(&self, _target: isize, label: &str) -> Result<ClickReport, String> {
        self.clicks.lock().unwrap().push(label.to_string());
        let mut fail = self.fail_next_once.lock().unwrap();
        if label == "Next" && *fail {
            *fail = false;
            *self.objection.lock().unwrap() = Some("Phone is invalid".into());
            return Ok(ClickReport { clicked: label.into(), changed: true, ..Default::default() });
        }
        *self.objection.lock().unwrap() = None;
        let mut page = self.page.lock().unwrap();
        let before = *page;
        if label == "Next" {
            *page = 1;
        } else if label.starts_with("Submit") {
            *page = 2;
        }
        Ok(ClickReport {
            clicked: label.into(),
            changed: *page != before,
            new_text: if *page == 2 { vec!["Thank you, your application was submitted.".into()] } else { vec![] },
            ..Default::default()
        })
    }
    fn form_scroll(&self, _target: isize, _down: bool) -> Result<bool, String> {
        self.scrolls.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }
}

/// Jev, faked: the goal is met when the page says so; otherwise the first control is the move.
pub(super) struct Jev(pub AtomicUsize);
impl Classifier for Jev {
    fn system_one(&self, state: &Value, qs: &[(String, Question)]) -> Result<Vec<(String, Answer)>, ApiError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let page = state["page_text"].as_str().unwrap_or("").to_lowercase();
        let done = page.contains("thank");
        Ok(qs
            .iter()
            .map(|(n, q)| {
                let a = match q {
                    Question::Noul { .. } if state.get("request").is_some() => {
                        let said = state["request"].as_str().unwrap_or("").contains(state["answer"].as_str().unwrap_or("?"));
                        Answer::Noul(if said { 0.9 } else { 0.1 })
                    }
                    Question::Noul { .. } if n == "objected" => Answer::Noul(if page.contains("invalid") { 0.9 } else { 0.1 }),
                    Question::Noul { .. } => Answer::Noul(if done || !state.get("page_text").is_some() { 0.9 } else { 0.1 }),
                    Question::Choice { criteria, .. } => Answer::Choice(ChoiceAnswer {
                        choice: if n == "left" && done {
                            "all_done".into()
                        } else {
                            criteria.first().map(|c| c.0.clone()).unwrap_or_else(|| "stuck".into())
                        },
                        confidence: 0.9,
                        probabilities: vec![],
                    }),
                    _ => Answer::Noul(0.9),
                };
                (n.clone(), a)
            })
            .collect())
    }
}

pub(super) fn profile() -> Profile {
    Profile {
        name: "Ada Lovelace".into(),
        email: "ada@example.com".into(),
        phone: "5550100".into(),
        location: "London".into(),
        work: vec![Work { company: "Acme".into(), title: "Engineer".into(), start: "2019".into(), end: "present".into(), description: "built engines".into() }],
        skills: vec!["Rust".into()],
        ..Profile::default()
    }
}

fn plan_reply(entries: Value, advance: &str, is_final: bool) -> crate::assistant::testing::Script {
    calling(vec![call("p", "submit_plan", json!({ "done_when": "a thank you page", "entries": entries, "skipped": [], "advance": advance, "final": is_final }))])
}

struct Rig {
    h: Harness,
    site: Arc<Site>,
    chat: Arc<FakeChat>,
    jev: Arc<Jev>,
    tasks: Arc<Tasks>,
    memory: Mutex<Memory>,
}

fn rig(tag: &str, site: Arc<Site>, script: Vec<crate::assistant::testing::Script>) -> Rig {
    let mut script = script;
    script.insert(0, crate::assistant::testing::saying(r#"["apply and submit"]"#));
    let chat = FakeChat::scripted(script);
    let h = Harness::with_senses(tag, chat.clone(), Limits::default(), site.clone());
    h.act.store(true, Ordering::SeqCst);
    let tasks = Tasks::load(h.svc.clone());
    let memory = Mutex::new(Memory::default());
    memory.lock().unwrap().profile = Some(profile());
    Rig { h, site, chat, jev: Arc::new(Jev(AtomicUsize::new(0))), tasks, memory }
}

fn go(r: &Rig) -> (ExecEnd, ExecStats) {
    let cancel = Cancel::new();
    let verifier = JevVerifier::new(r.jev.clone(), Duration::from_secs(2), cancel.clone());
    let cfg = ExecCfg {
        svc: &r.h.svc,
        chat: r.chat.as_ref() as &dyn Chat,
        tasks: &r.tasks,
        memory: &r.memory,
        cancel: &cancel,
        jev: Some(&verifier),
        started: Instant::now(),
        time: Duration::from_secs(60),
    };
    let mut p = Pursuit::begin("fill out the application form", Budget { steps: 20, seconds: 60.0, tokens: 1_000_000 }).unwrap();
    assert_eq!(p.kind, GoalKind::Form);
    let mut ctx = TurnCtx::default();
    run_form(&cfg, &mut ctx, &mut p)
}

#[test]
fn a_two_page_form_takes_one_model_call_per_page_and_none_for_the_rest() {
    let site = Site::new();
    let r = rig(
        "exec-happy",
        site,
        vec![
            plan_reply(json!([]), "Next", false),
            plan_reply(json!([{ "field": "Why do you want this role?", "value": "I build engines and want to build yours." },
                              { "field": "Expected salary", "value": "999999" }]), "Submit application", true),
            crate::assistant::testing::saying("DONE"),
        ],
    );
    let (end, stats) = go(&r);
    match &end {
        ExecEnd::Final(t) => assert!(t.starts_with("Done, and checked"), "{t}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.chat.calls().len(), 4, "the parts, one plan per page and one check of done, nothing per field");
    assert_eq!((stats.plan_calls, stats.replan_calls), (3, 0));
    let typed = r.site.typed.lock().unwrap().clone();
    assert_eq!(typed.get("full name").map(String::as_str), Some("Ada Lovelace"));
    assert_eq!(typed.get("email").map(String::as_str), Some("ada@example.com"));
    assert_eq!(typed.get("years of experience").map(String::as_str), Some("5+"), "from work dates, by code");
    assert!(typed.get("why do you want this role").is_some_and(|v| v.contains("engines")));
    assert!(!typed.contains_key("expected salary"), "salary only from saved answers, never the model: {typed:?}");
    assert_eq!(*r.site.clicks.lock().unwrap(), vec!["Next", "Submit application"]);
    assert!(stats.left_blank.iter().any(|b| b.to_lowercase().contains("salary")), "{:?}", stats.left_blank);
    assert!(r.jev.0.load(Ordering::SeqCst) > 0, "Jev decides every step");
}

#[test]
fn a_saved_answer_is_typed_for_a_sensitive_field() {
    let site = Site::new();
    let r = rig("exec-saved", site, vec![plan_reply(json!([]), "Next", false), plan_reply(json!([{ "field": "Why do you want this role?", "value": "Because." }]), "Submit application", true), crate::assistant::testing::saying("DONE")]);
    crate::assistant::answers::learn(&r.h.svc.home, "Expected salary", "FAKE-77").unwrap();
    let (end, _) = go(&r);
    assert!(matches!(end, ExecEnd::Final(_)), "{end:?}");
    assert_eq!(r.site.typed.lock().unwrap().get("expected salary").map(String::as_str), Some("FAKE-77"));
}

#[test]
fn a_failed_next_is_one_replan_and_then_goes_on() {
    let site = Site::new();
    *site.fail_next_once.lock().unwrap() = true;
    let r = rig(
        "exec-replan",
        site,
        vec![
            plan_reply(json!([]), "Next", false),
            plan_reply(json!([]), "Next", false),
            plan_reply(json!([{ "field": "Why do you want this role?", "value": "Because." }]), "Submit application", true),
            crate::assistant::testing::saying("DONE"),
        ],
    );
    let (end, stats) = go(&r);
    assert!(matches!(end, ExecEnd::Final(_)), "{end:?}");
    assert_eq!((stats.plan_calls, stats.replan_calls), (3, 1), "the model is called again only because the press failed");
    let seen = r.chat.calls();
    assert!(seen[2].messages[1]["content"].as_str().unwrap().contains("problems_with_the_last_attempt"));
}

#[test]
fn a_password_page_is_never_typed_into_and_goes_to_the_user() {
    let mut site = Site::new();
    Arc::get_mut(&mut site).unwrap().password_page = true;
    let r = rig("exec-password", site, vec![plan_reply(json!([]), "Next", false)]);
    let (end, _) = go(&r);
    assert!(matches!(&end, ExecEnd::NeedsUser(t) if t.contains("password")), "{end:?}");
    assert_eq!(r.site.fills.load(Ordering::SeqCst), 0);
    assert!(r.site.clicks.lock().unwrap().is_empty());
    assert!(r.chat.calls().is_empty());
}

#[test]
fn dry_mode_previews_one_page_and_touches_nothing() {
    let site = Site::new();
    let r = rig("exec-dry", site, vec![plan_reply(json!([]), "Next", false)]);
    r.h.act.store(false, Ordering::SeqCst);
    let (end, _) = go(&r);
    assert!(matches!(&end, ExecEnd::Final(t) if t.starts_with("Dry mode")), "{end:?}");
    assert_eq!(r.site.fills.load(Ordering::SeqCst), 0);
    assert!(r.site.typed.lock().unwrap().is_empty());
}

#[test]
fn a_planner_that_fails_hands_the_job_to_the_agent_loop() {
    let site = Site::new();
    let r = rig("exec-fail", site, vec![crate::assistant::testing::Script::Fail(api::chat::ChatError::Http(500, "x".into()))]);
    let (end, _) = go(&r);
    assert!(matches!(end, ExecEnd::Handoff(_)), "{end:?}");
}

#[test]
fn the_verifier_decides_from_the_result_before_asking_jev() {
    use crate::assistant::pursuit::{Verdict, Verifier};
    let jev = JevVerifier::new(Arc::new(Jev(AtomicUsize::new(0))), Duration::from_secs(1), Cancel::new());
    assert_eq!(jev.verify("x", r#"{"page_changed":true,"errors":[]}"#), Verdict::Holds);
    assert!(matches!(jev.verify("x", r#"{"page_changed":false,"errors":[]}"#), Verdict::Fails(_)));
    assert!(matches!(jev.verify("x", r#"{"results":[{"field":"a","status":"mismatch"}]}"#), Verdict::Fails(_)));
    assert_eq!(jev.calls(), 0);
    assert_eq!(jev.verify("the page says thanks", "Thank you"), Verdict::Holds);
    assert_eq!(jev.calls(), 1);
}

// ---------------------------------------------------------------- the same site, real models

fn load_env() {
    let path = std::env::var("POINTER_ENV_FILE").unwrap_or_else(|_| r"D:\Files\Vatsa\Projects\typesafe-computer-use\.env".into());
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if !k.trim().is_empty() && !k.trim().starts_with('#') && std::env::var(k.trim()).is_err() {
                    std::env::set_var(k.trim(), v.trim().trim_matches('"'));
                }
            }
        }
    }
}

#[test]
#[ignore]
fn live_jev() {
    load_env();
    let ts = Arc::new(api::typesafe::TypeSafe::from_env().expect("a TypeSafe key in .env"));
    let j = JevVerifier::new(ts, Duration::from_secs(5), Cancel::new());
    let t = Instant::now();
    println!("JEV login page {:?} in {:?}", j.expected_page("apply for the job", "Sign in to continue. Email. Password. Forgot password?"), t.elapsed());
    println!("JEV step page {:?}", j.expected_page("apply for the job", "Step 2 of 3. Years of experience. Why this role? Next"));
    println!("JEV submitted {:?} / {:?}", j.submitted("apply", "Thank you, we received your application"), j.submitted("apply", "Step 2 of 3 Phone is invalid"));
}

/// Run with `cargo test -p core live_simulation -- --ignored --nocapture`.
#[test]
#[ignore]
fn live_simulation() {
    load_env();
    let chat = Arc::new(api::chat::ChatClient::from_env().expect("a chat key in .env"));
    let ts = Arc::new(api::typesafe::TypeSafe::from_env().expect("a TypeSafe key in .env"));
    let site = Site::new();
    let fake = FakeChat::scripted(vec![]);
    let h = Harness::with_senses("exec-live", fake, Limits::default(), site.clone());
    h.act.store(true, Ordering::SeqCst);
    let tasks = Tasks::load(h.svc.clone());
    let memory = Mutex::new(Memory::default());
    memory.lock().unwrap().profile = Some(profile());
    let cancel = Cancel::new();
    let verifier = JevVerifier::new(ts, Duration::from_secs(5), cancel.clone());
    let cfg = ExecCfg {
        svc: &h.svc,
        chat: chat.as_ref(),
        tasks: &tasks,
        memory: &memory,
        cancel: &cancel,
        jev: Some(&verifier),
        started: Instant::now(),
        time: Duration::from_secs(120),
    };
    let mut p = Pursuit::begin("fill out the application form", Budget { steps: 20, seconds: 120.0, tokens: 1_000_000 }).unwrap();
    let t = Instant::now();
    let (end, stats) = run_form(&cfg, &mut TurnCtx::default(), &mut p);
    println!("SIM end: {end:?}");
    println!(
        "SIM seconds {:.1}; pages {}; plan calls {}; replans {}; jev calls {}; tool calls {}; tokens {}; typed {:?}",
        t.elapsed().as_secs_f64(), stats.pages, stats.plan_calls, stats.replan_calls, verifier.calls(), stats.tool_calls,
        stats.usage.to_json(), site.typed.lock().unwrap()
    );
    assert!(matches!(end, ExecEnd::Final(ref t) if t.starts_with("Done")), "{end:?}");
}
