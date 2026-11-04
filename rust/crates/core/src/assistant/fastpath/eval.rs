//! The labelled set: what the fast path should do with realistic lines, and the measurement.
//!
//! `expected` is what a sensible person would want: `app:Label`, `win:Title`, `tab:Title`,
//! `site:Label`, `stop`, `pause`, or `agent`. Lines in other languages carry the intent they mean,
//! so a safe miss (sent to the agent) shows up as a miss, not as a pass.
//!
//! The offline test checks that the guards do not reject the English commands a perfect
//! classifier would pick. The ignored live test asks TypeSafe and prints the report:
//!
//! POINTER_ENV_FILE=<.env> cargo test -p core --lib fastpath::eval -- --ignored --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::tests::{demo_world, pick};
use super::*;

pub(super) struct Row {
    pub text: &'static str,
    pub expected: &'static str,
    pub kind: &'static str,
}

const fn r(text: &'static str, expected: &'static str, kind: &'static str) -> Row {
    Row {
        text,
        expected,
        kind,
    }
}

pub(super) fn set() -> Vec<Row> {
    vec![
        // plain commands
        r("open calculator", "app:Calculator", "command"),
        r("open notepad", "app:Notepad", "command"),
        r("launch spotify", "app:Spotify", "command"),
        r("start paint", "app:Paint", "command"),
        r(
            "hey pointer open the calculator",
            "app:Calculator",
            "command",
        ),
        r(
            "can you open slack please",
            "win:general - Acme - Slack",
            "command",
        ),
        r("open visual studio code", "agent", "ambiguous"),
        r("launch file explorer", "app:File Explorer", "command"),
        r("open task manager", "app:Task Manager", "command"),
        r("open settings", "app:Settings", "command"),
        r("open discord", "app:Discord", "command"),
        r("open microsoft word", "app:Microsoft Word", "command"),
        r("open windows terminal", "app:Windows Terminal", "command"),
        r("could you launch the notepad app", "app:Notepad", "command"),
        r("switch to slack", "win:general - Acme - Slack", "command"),
        r(
            "go to the excel window",
            "win:Budget.xlsx - Excel",
            "command",
        ),
        r(
            "bring up visual studio code",
            "win:main.rs - pointer - Visual Studio Code",
            "command",
        ),
        r("open microsoft excel", "agent", "ambiguous"),
        r(
            "focus the budget window",
            "win:Budget.xlsx - Excel",
            "command",
        ),
        r("switch to the youtube tab", "tab:YouTube", "command"),
        r(
            "go to my quarterly plan tab",
            "tab:Quarterly plan - Google Docs",
            "command",
        ),
        r(
            "show me the pull requests tab",
            "tab:Pull requests - GitHub",
            "command",
        ),
        r("open youtube", "site:YouTube", "command"),
        r("open github", "site:GitHub", "command"),
        r("go to reddit", "site:Reddit", "command"),
        r("open wikipedia please", "site:Wikipedia", "command"),
        r("stop", "stop", "command"),
        r("cancel that", "stop", "command"),
        r("never mind", "stop", "command"),
        r("stop talking", "stop", "command"),
        r("pause", "pause", "command"),
        r("hold on", "pause", "command"),
        // questions
        r("what time is it", "agent", "question"),
        r("what's the weather like today", "agent", "question"),
        r("what is on my screen", "agent", "question"),
        r("who is the president of france", "agent", "question"),
        r("how do i open the calculator", "agent", "question"),
        r("is spotify installed", "agent", "question"),
        r(
            "what is fifteen percent of two hundred forty",
            "agent",
            "question",
        ),
        r("tell me a joke", "agent", "question"),
        r("which window am i in right now", "agent", "question"),
        r("read this page to me", "agent", "question"),
        // ambiguous
        r("open it", "agent", "ambiguous"),
        r("switch", "agent", "ambiguous"),
        r("open the other one", "agent", "ambiguous"),
        r("do the thing again", "agent", "ambiguous"),
        r("go back", "agent", "ambiguous"),
        r("open chrome", "agent", "ambiguous"),
        r("switch to gmail", "agent", "ambiguous"),
        r("open the doc", "agent", "ambiguous"),
        r("stop the music in spotify", "agent", "ambiguous"),
        r("open code", "agent", "ambiguous"),
        // multi-step
        r("open chrome and search for cats", "agent", "multi"),
        r("open notepad and type hello", "agent", "multi"),
        r(
            "open calculator and compute five times six",
            "agent",
            "multi",
        ),
        r("go to youtube and search for lofi beats", "agent", "multi"),
        r("open spotify and play some jazz", "agent", "multi"),
        r("switch to slack and send hi to Sam", "agent", "multi"),
        r("open github then open my pull requests", "agent", "multi"),
        r("open excel and then close paint", "agent", "multi"),
        r("search google for rust tutorials", "agent", "multi"),
        r("open notepad and calculator", "agent", "multi"),
        // other actions
        r("close the calculator", "agent", "other"),
        r("minimize all windows", "agent", "other"),
        r("mute the volume", "agent", "other"),
        r("click the submit button", "agent", "other"),
        r("scroll down a bit", "agent", "other"),
        r("take a screenshot", "agent", "other"),
        r("type my email address", "agent", "other"),
        r("delete the file on my desktop", "agent", "other"),
        r("send an email to John about lunch", "agent", "other"),
        r("set a timer for five minutes", "agent", "other"),
        r("install spotify", "agent", "other"),
        r("fill in this job application", "agent", "other"),
        // other languages
        r("abre la calculadora", "app:Calculator", "lang"),
        r("ouvre le bloc-notes", "app:Notepad", "lang"),
        r("offne den taschenrechner", "app:Calculator", "lang"),
        r(
            "\u{6253}\u{5f00}\u{8ba1}\u{7b97}\u{5668}",
            "app:Calculator",
            "lang",
        ),
        r("ouvre youtube", "site:YouTube", "lang"),
        r("cambia a slack", "win:general - Acme - Slack", "lang"),
        r("arr\u{ea}te", "stop", "lang"),
        r("pausa", "pause", "lang"),
        r("cual es la capital de espana", "agent", "lang"),
        r("abre el bloc de notas y escribe hola", "agent", "lang"),
        r("qu'est-ce que j'ai sur mon ecran", "agent", "lang"),
        r("kalkulator kholo", "app:Calculator", "lang"),
    ]
}

pub(super) fn describe(p: &Plan) -> String {
    match p {
        Plan::OpenApp(a) => format!("app:{}", a.label),
        Plan::SwitchWindow { title } => format!("win:{title}"),
        Plan::SwitchTab { title } => format!("tab:{title}"),
        Plan::OpenSite { label, .. } => format!("site:{label}"),
        Plan::Stop => "stop".into(),
        Plan::Pause => "pause".into(),
    }
}

/// The fragment of the option description a perfect classifier would choose for `expected`.
fn oracle(expected: &str) -> Option<(&'static str, String)> {
    let (kind, rest) = expected.split_once(':').unwrap_or((expected, ""));
    Some(match kind {
        "app" => ("open_app", format!("Installed app: {rest}")),
        "win" => ("switch_window", "Open window: ".to_string()),
        "tab" => ("switch_tab", format!("Open browser tab: {rest}")),
        "site" => ("open_site", format!("Website: {rest}")),
        "stop" => ("stop", "none".into()),
        "pause" => ("pause", "none".into()),
        _ => return None,
    })
}

#[test]
fn the_set_is_big_enough_and_labelled() {
    let rows = set();
    assert!(rows.len() >= 60, "{}", rows.len());
    for kind in ["command", "question", "ambiguous", "multi", "other", "lang"] {
        assert!(rows.iter().any(|r| r.kind == kind), "{kind}");
    }
}

#[test]
fn the_guards_pass_every_english_command_a_perfect_classifier_would_pick() {
    for row in set().iter().filter(|r| r.kind == "command") {
        let (action, target) = oracle(row.expected).unwrap();
        let target: &'static str = match row.expected.split_once(':') {
            Some(("win", title)) => {
                let app = super::tests::demo_snapshot()
                    .windows
                    .iter()
                    .find(|w| w.title == title)
                    .map(|w| w.app.clone())
                    .unwrap();
                Box::leak(format!("Open window: {app}: {title}").into_boxed_str())
            }
            _ => Box::leak(target.into_boxed_str()),
        };
        let c = pick(action, 1.0, target, 1.0);
        let d = decide(
            c,
            row.text,
            &demo_world(row.text),
            0.9,
            Duration::from_secs(2),
            &api::chat::Cancel::new(),
        );
        let got = d
            .plan
            .as_ref()
            .map(describe)
            .unwrap_or_else(|| format!("agent ({})", d.why));
        assert_eq!(got, row.expected, "{:?}", row.text);
    }
}

#[test]
fn a_perfectly_confident_classifier_that_always_says_open_the_first_app_is_never_fast_on_non_commands(
) {
    // The worst case for the guards: the model is certain and picks a listed target every time.
    for row in set().iter().filter(|r| r.expected == "agent") {
        for (action, target) in [
            ("open_app", "Installed app: Calculator"),
            ("open_app", "Installed app: Notepad"),
            ("switch_window", "Open window: slack"),
            ("switch_tab", "Open browser tab: YouTube"),
            ("open_site", "Website: YouTube"),
            ("stop", "none"),
            ("pause", "none"),
        ] {
            let c = pick(action, 1.0, target, 1.0);
            let d = decide(
                c,
                row.text,
                &demo_world(row.text),
                0.9,
                Duration::from_secs(2),
                &api::chat::Cancel::new(),
            );
            // The only lines that may pass are ones that really are that command; none here are.
            // ("open chrome" and the like name no such target.)
            assert!(
                d.plan.is_none(),
                "{:?} with {action}/{target} -> {:?}",
                row.text,
                d.plan
            );
        }
    }
}

// ---------------------------------------------------------------- live measurement

fn load_env() {
    let Ok(path) = std::env::var("POINTER_ENV_FILE") else {
        return;
    };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if !k.starts_with('#') && !k.is_empty() && std::env::var(k).is_err() {
                std::env::set_var(k, v);
            }
        }
    }
}

struct Live(api::typesafe::TypeSafe);
impl Classifier for Live {
    fn system_one(
        &self,
        state: &serde_json::Value,
        q: &[(String, Question)],
    ) -> Result<Vec<(String, Answer)>, ApiError> {
        self.0.system_one(state, q)
    }
}

struct Result1 {
    row: usize,
    got: Option<String>,
    min_conf: f64,
    ms: u128,
    why: String,
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

#[test]
#[ignore = "live: calls TypeSafe (and the agent model for a latency sample)"]
fn live_measure() {
    load_env();
    let rows = set();
    let client = api::typesafe::TypeSafe::from_env().expect("TYPESAFE_API_KEY");
    let live: Arc<dyn Classifier + Send + Sync> = Arc::new(Live(client));
    let cancel = api::chat::Cancel::new();
    // Warm the connection once so the first row does not carry the TLS handshake.
    let _ = decide(
        live.clone(),
        "open calculator",
        &demo_world("open calculator"),
        0.0,
        Duration::from_secs(10),
        &cancel,
    );
    let mut results = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let t = Instant::now();
        let d = decide(
            live.clone(),
            row.text,
            &demo_world(row.text),
            0.0,
            Duration::from_secs(10),
            &cancel,
        );
        let ms = t.elapsed().as_millis();
        let needs_target = !matches!(d.plan, Some(Plan::Stop | Plan::Pause));
        let min_conf = if needs_target {
            d.action_conf.min(d.target_conf)
        } else {
            d.action_conf
        };
        println!(
            "[{:>2}] {:>5} ms  {:<46} want {:<40} got {:<40} a={:.2} t={:.2} {}",
            i,
            ms,
            row.text,
            row.expected,
            d.plan
                .as_ref()
                .map(describe)
                .unwrap_or_else(|| "agent".into()),
            d.action_conf,
            d.target_conf,
            d.why
        );
        results.push(Result1 {
            row: i,
            got: d.plan.as_ref().map(describe),
            min_conf,
            ms,
            why: d.why,
        });
    }
    println!(
        "\nthreshold sweep over {} lines ({} expected fast, {} expected agent)",
        rows.len(),
        rows.iter().filter(|r| r.expected != "agent").count(),
        rows.iter().filter(|r| r.expected == "agent").count()
    );
    println!(
        "{:>5} {:>9} {:>9} {:>9} {:>11} {:>9}",
        "T", "fast", "correct", "falsefast", "missedfast", "accuracy"
    );
    for t in [0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95, 0.97, 0.99] {
        let (mut fast, mut ok, mut false_fast, mut missed, mut right) = (0, 0, 0, 0, 0);
        for x in &results {
            let want = rows[x.row].expected;
            let went_fast = x.got.is_some() && x.min_conf >= t;
            let got = if went_fast { x.got.clone() } else { None };
            if went_fast {
                fast += 1;
                if got.as_deref() == Some(want) {
                    ok += 1;
                    right += 1
                } else {
                    false_fast += 1
                }
            } else if want == "agent" {
                right += 1
            } else {
                missed += 1
            }
        }
        println!(
            "{t:>5.2} {fast:>9} {ok:>9} {false_fast:>9} {missed:>11} {:>8.1}%",
            pct(right, rows.len())
        );
    }
    for t in [0.9] {
        println!("\nfalse-fast lines at T={t}:");
        for x in &results {
            let want = rows[x.row].expected;
            if x.got.is_some() && x.min_conf >= t && x.got.as_deref() != Some(want) {
                println!(
                    "  {:?} want {want} got {:?} conf {:.2}",
                    rows[x.row].text, x.got, x.min_conf
                );
            }
        }
        println!("missed fast (went to the agent, wanted fast):");
        for x in &results {
            let want = rows[x.row].expected;
            if want != "agent" && !(x.got.is_some() && x.min_conf >= t) {
                println!(
                    "  {:?} want {want} ({}) conf {:.2}",
                    rows[x.row].text, x.why, x.min_conf
                );
            }
        }
    }
    let mut lat: Vec<u128> = results.iter().map(|x| x.ms).collect();
    lat.sort();
    let q = |p: f64| lat[((lat.len() as f64 - 1.0) * p).round() as usize];
    println!(
        "\nfast-path classify latency ms: p50 {} p90 {} max {} mean {}",
        q(0.5),
        q(0.9),
        lat[lat.len() - 1],
        lat.iter().sum::<u128>() / lat.len() as u128
    );
    agent_latency(&rows);
}

/// One real call to the agent model with the real system prompt and tool list: the least an
/// agent turn can cost before any tool runs.
fn agent_latency(rows: &[Row]) {
    let Some(chat) = api::chat::ChatClient::from_env() else {
        println!("no agent key: agent latency not measured");
        return;
    };
    use api::chat::Chat;
    let system = super::super::agent::system_prompt(true, false, true);
    let tools = super::super::tools::schemas(super::super::tools::Kind::Foreground);
    let mut ms = Vec::new();
    for row in rows
        .iter()
        .filter(|r| r.kind == "command")
        .step_by(3)
        .take(8)
    {
        let msgs = vec![
            serde_json::json!({"role": "system", "content": system}),
            serde_json::json!({"role": "user", "content": row.text}),
        ];
        let t = Instant::now();
        let _ = chat.complete(&msgs, &tools, &api::chat::Cancel::new());
        ms.push(t.elapsed().as_millis());
        println!(
            "agent first call {:>5} ms  {}",
            ms.last().unwrap(),
            row.text
        );
    }
    ms.sort();
    if !ms.is_empty() {
        println!(
            "agent first-call latency ms: p50 {} max {} mean {}",
            ms[ms.len() / 2],
            ms[ms.len() - 1],
            ms.iter().sum::<u128>() / ms.len() as u128
        );
    }
}
