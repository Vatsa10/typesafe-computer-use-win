//! Run one real Pointer run-loop task against a headless Chrome. TEST ONLY.
//!
//!     headless_task --url <url> --goal "..." [--provider openai|claude] [--origin X,Y]
//!                   [--results http://127.0.0.1:8787/results] [--steps N] [--max-seconds N]
//!
//! Chrome runs with --headless=new in a throwaway profile: no window, and the user's mouse and
//! keyboard are never used. Models are real, so keys come from `POINTER_HOME/.env` (never printed).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use testkit::headless::HeadlessDesktop;
use wcore::runner::{self, Control, Events, Mark};
use wcore::{config, perception, sitepick, writer};

struct Quiet;

impl Events for Quiet {
    fn line(&self, text: &str) {
        println!("  | {text}");
    }
    fn highlight(&self, _marks: Vec<Mark>, _seconds: f64) {}
    fn answer(&self, text: &str) {
        println!("  answer: {text}");
    }
    fn state(&self, _running: bool, _paused: bool) {}
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn get(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("http:// only")?;
    let (host, p) = rest.split_once('/').unwrap_or((rest, ""));
    let path = format!("/{p}");
    let mut s = TcpStream::connect(host).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n").map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| e.to_string())?;
    Ok(out.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(url), Some(goal)) = (arg(&args, "--url"), arg(&args, "--goal")) else {
        eprintln!("usage: headless_task --url <url> --goal \"...\" [--provider openai|claude] [--origin X,Y] [--results URL] [--steps N]");
        std::process::exit(2);
    };
    if let Some(home) = std::env::var_os("POINTER_HOME").filter(|v| !v.is_empty()) {
        let _ = config::load_dotenv(&PathBuf::from(home).join(".env"));
    }
    if let Some(p) = arg(&args, "--provider") {
        match p.as_str() {
            "openai" | "claude" => std::env::set_var("POINTER_LLM", &p),
            _ => {
                eprintln!("--provider is openai or claude");
                std::process::exit(2);
            }
        }
    }
    let origin = arg(&args, "--origin")
        .and_then(|o| o.split_once(',').and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?))))
        .unwrap_or((0, 0));
    let results_url = arg(&args, "--results").unwrap_or_else(|| "http://127.0.0.1:8787/results".into());
    let Some(classifier) = api::typesafe::TypeSafe::from_env() else {
        eprintln!("a run needs TYPESAFE_API_KEY (POINTER_HOME/.env)");
        std::process::exit(2);
    };
    let scribe = writer::make_writer();

    let desk = match HeadlessDesktop::launch(origin) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("could not start headless Chrome: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = desk.navigate(&url) {
        eprintln!("could not open {url}: {e}");
        std::process::exit(1);
    }
    if args.iter().any(|a| a == "--assistant") {
        run_assistant(desk, &goal, &args, &results_url, classifier);
        return;
    }
    let out = std::env::temp_dir().join(format!("pointer-headless-run-{}", std::process::id()));
    let mut options = runner::RunOptions { out: Some(out.clone()), ..Default::default() };
    if let Some(n) = arg(&args, "--steps").and_then(|s| s.parse().ok()) {
        options.steps = n;
    }
    if let Some(c) = arg(&args, "--min-confidence").and_then(|s| s.parse().ok()) {
        options.min_confidence = c;
    }
    // The run loop has no time budget of its own: a watchdog aborts it.
    let control = Control::new();
    let max = arg(&args, "--max-seconds").and_then(|s| s.parse::<u64>().ok()).unwrap_or(300);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (c, d) = (control.clone(), done.clone());
        std::thread::spawn(move || {
            for _ in 0..max {
                std::thread::sleep(Duration::from_secs(1));
                if d.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }
            c.abort("time limit");
        });
    }
    let deps = runner::Deps {
        eyes: &desk as &dyn perception::Desktop,
        hands: &desk,
        classifier: &classifier,
        writer: scribe.as_ref().map(|w| w as &dyn writer::StructuredWriter),
        cursor: &|| (500, 500),
        browser: "chrome".into(),
        email: config::email(),
        sites: sitepick::sites_for(&goal, None),
        apps: Vec::new(),
        options,
    };
    println!("goal: {goal}\nstart: {url}  origin: {origin:?}  provider: {}", std::env::var("POINTER_LLM").unwrap_or_else(|_| "default".into()));
    let result = runner::run_goal(&goal, true, &control, &Quiet, &deps);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    println!("outcome: {}", result.outcome);
    if let Some(f) = &result.failure {
        println!("failure: {f}");
    }
    println!("steps: {}", result.history.len());
    for (i, h) in result.history.iter().enumerate() {
        println!("  {}. {h}", i + 1);
    }
    println!("seconds: {:.1}", result.seconds);
    println!("page now: {} ({})", desk.title(), desk.url());
    match get(&results_url) {
        Ok(body) => println!("results: {body}"),
        Err(e) => println!("results: unavailable ({e})"),
    }
}

/// The real assistant (agent model, safety guards, fast path, pursuit, form tools) on the page.
fn run_assistant(desk: HeadlessDesktop, goal: &str, args: &[String], results_url: &str, classifier: api::typesafe::TypeSafe) {
    use std::sync::{Arc, Mutex};
    use testkit::assistant::{HeadlessHands, HeadlessSenses};
    use wcore::assistant::gate::Gate;
    use wcore::assistant::pending::Pending;
    use wcore::assistant::testing::{FakeEmit, FakeShell, FakeVoice};
    use wcore::assistant::{Assistant, Limits, Services};

    let desk = Arc::new(Mutex::new(desk));
    // A fresh home each run, so no earlier conversation leaks in; the user's saved answers (if any)
    // are copied in, since "answers from what it knows about the user" is part of the test.
    let real = std::env::var_os("POINTER_HOME").map(PathBuf::from);
    let home = std::env::temp_dir().join(format!("pointer-headless-home-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    if let Some(r) = real {
        let _ = std::fs::copy(r.join("my-answers.json"), home.join("my-answers.json"));
    }
    let profile = std::env::temp_dir().join(format!("pointer-headless-profile-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&profile);
    let emit = Arc::new(FakeEmit::default());
    let svc = Arc::new(Services {
        chat: Arc::new(|| {
            api::chat::ChatClient::from_env()
                .map(|c| Arc::new(c) as Arc<dyn api::chat::Chat>)
                .ok_or_else(|| "no agent key".to_string())
        }),
        emit: emit.clone(),
        voice: Arc::new(FakeVoice::default()),
        senses: Arc::new(HeadlessSenses { desk: desk.clone() }),
        hands: Arc::new(HeadlessHands { desk: desk.clone() }),
        shell: Arc::new(FakeShell::default()),
        act_mode: Arc::new(|| true),
        camera_enabled: Arc::new(|| false),
        home: home.clone(),
        // Documents: no roots, so nothing of the user's is ever indexed or read.
        profile: profile.clone(),
        limits: Limits::default(),
        confirms: Pending::new(),
        cameras: Pending::new(),
        documents: Arc::new(wcore::documents::Documents::new(home, Vec::new(), "")),
        writer: Arc::new(|| {
            wcore::writer::make_writer()
                .map(|w| Box::new(w) as Box<dyn wcore::writer::StructuredWriter>)
                .ok_or_else(|| "no writer key".to_string())
        }),
        autonomy_full: Arc::new(|| true),
    });
    let assistant = Assistant::new(svc, Gate::new(Arc::new(classifier)));
    let started = std::time::Instant::now();
    println!("goal: {goal}
mode: assistant  provider: {}", std::env::var("POINTER_LLM").unwrap_or_else(|_| "default".into()));
    let cancel = api::chat::Cancel::new();
    let max = arg(args, "--max-seconds").and_then(|s| s.parse::<u64>().ok()).unwrap_or(300);
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(max));
        c2.cancel();
    });
    assistant.run_turn(goal, "typed", &cancel);
    let events = emit.0.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut tools = 0;
    for (name, payload) in &events {
        let text = payload.to_string();
        match name.as_str() {
            "reply" => println!("reply: {}", text.chars().take(400).collect::<String>()),
            "line" | "point" | "speech" => {}
            other => {
                tools += 1;
                println!("event {other}: {}", text.chars().take(160).collect::<String>());
            }
        }
    }
    println!("events: {}  other: {tools}", events.len());
    println!("seconds: {:.1}", started.elapsed().as_secs_f64());
    match get(results_url) {
        Ok(body) => println!("results: {body}"),
        Err(e) => println!("results: unavailable ({e})"),
    }
    let _ = std::fs::remove_dir_all(&profile);
}
