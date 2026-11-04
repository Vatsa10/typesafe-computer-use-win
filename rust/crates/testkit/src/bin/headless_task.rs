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
    let out = std::env::temp_dir().join(format!("pointer-headless-run-{}", std::process::id()));
    let mut options = runner::RunOptions { out: Some(out.clone()), ..Default::default() };
    if let Some(n) = arg(&args, "--steps").and_then(|s| s.parse().ok()) {
        options.steps = n;
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
