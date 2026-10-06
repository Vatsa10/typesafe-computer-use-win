//! The core: one binary that owns the hotkeys, the screen and the loop.
//!
//! Electron runs this as a child process and talks the line protocol to it. The UI owns windows
//! and nothing else, which is what keeps a crash in the shell from stranding a run that is already
//! driving the machine.
//!
//! Threads: the main thread reads stdin; each request is handled on its own short-lived thread so
//! a transcription never holds up an abort; the hotkey pump registers and pumps and only enqueues;
//! the input worker drains that queue. All output goes through one locked writer.

mod daemon;
mod emit;
mod ipc;
mod runner;
mod settings;
mod voice;

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use daemon::{Daemon, Modes, Paths};
use emit::Emitter;

const HOTKEY_NAMES: [&str; 6] = ["talk", "bar", "goal", "pause", "abort", "quit"];

enum Pumped {
    Fired(String),
    Refused(Vec<String>),
}

/// Register every hotkey on a thread of its own and pump it. Callbacks only send on a channel.
fn start_hotkeys(daemon: Arc<Daemon>) {
    let (tx, rx) = mpsc::channel::<Pumped>();
    let mut keys = platform::hotkeys::Hotkeys::new();
    for name in HOTKEY_NAMES {
        let Some(spec) = wcore::config::hotkey(name) else {
            continue;
        };
        let sender = tx.clone();
        let owned = name.to_string();
        if let Err(e) = keys.add(name, &spec, move || {
            let _ = sender.send(Pumped::Fired(owned.clone()));
        }) {
            daemon
                .out
                .line(format!("hotkey for {name} does not parse ({spec}): {e}"));
        }
    }
    let ready = tx;
    let spawned = std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            keys.serve(move |refused| {
                let _ = ready.send(Pumped::Refused(refused));
            });
        });
    if let Err(e) = spawned {
        daemon
            .out
            .line(format!("the hotkey pump would not start: {e}"));
        return;
    }
    let _ = std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            for message in rx {
                match message {
                    Pumped::Fired(name) => daemon.on_hotkey(&name),
                    Pumped::Refused(names) => {
                        for name in names {
                            let spec = wcore::config::hotkey(&name).unwrap_or_default();
                            daemon
                                .out
                                .line(format!("hotkey for {name} is taken by another app: {spec}"));
                        }
                    }
                }
            }
        });
}

/// Where `.env` and `runs/` live. An installed copy starts in its install folder, which is neither
/// writable nor where anyone would look, so: POINTER_HOME if set, else the working folder when it
/// already holds a `.env` (a checkout), else `%LOCALAPPDATA%\pointer`.
fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("POINTER_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join(".env").is_file() {
        return cwd;
    }
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("pointer")).unwrap_or(cwd)
}

fn main() {
    // Before any window or monitor is asked about anything. Without it every rectangle Windows
    // reports is wrong on a scaled display, and a click computed from one lands short.
    platform::display::declare_dpi_aware();

    let cwd = home();
    std::fs::create_dir_all(&cwd).ok();
    let dotenv = cwd.join(".env");
    let out = Emitter::new(std::io::stdout());
    if let Err(e) = wcore::config::load_dotenv(&dotenv) {
        out.line(format!("could not read .env: {e}"));
    }

    let talk_vk = wcore::config::hotkey("talk")
        .and_then(|spec| platform::hotkeys::parse_hotkey(&spec).ok())
        .map(|(_, vk)| vk);
    let daemon = Arc::new(Daemon {
        runner: Box::new(runner::Wired::live()),
        listener: Box::new(voice::Microphone::default()),
        out: out.clone(),
        speaker: Arc::new(runner::speak),
        paths: Paths {
            runs: cwd.join("runs"),
            dotenv,
        },
        hotkeys: daemon::hotkey_hint(),
        talk_vk,
        modes: Mutex::new(Modes::default()),
    });

    let screens = platform::display::monitors().len();
    out.line(format!("core ready: {screens} displays"));
    // CLICKER_NO_HOTKEYS=1 runs the protocol without touching the global keyboard: for smoke tests
    // and for a second copy that must not fight the first over the keys.
    if std::env::var("CLICKER_NO_HOTKEYS")
        .map(|v| v != "1")
        .unwrap_or(true)
    {
        start_hotkeys(daemon.clone());
    }
    daemon.events().state(daemon.runner.state());

    let stdin = std::io::stdin();
    let mut workers = Vec::new();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Some(request) = ipc::parse(&line) else {
            continue; // not a request: the shell may be mid-upgrade, and exiting would strand a run
        };
        let daemon = daemon.clone();
        workers.push(std::thread::spawn(move || {
            let reply = daemon.handle(&request);
            daemon.out.reply(&reply);
        }));
        workers.retain(|w| !w.is_finished());
    }
    // The shell closed the pipe: let what was asked finish answering, then stop the run with it.
    for worker in workers {
        let _ = worker.join();
    }
    let _ = daemon.runner.abort();
}
