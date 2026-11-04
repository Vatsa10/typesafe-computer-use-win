//! The core: one binary that owns the hotkeys, the screen and the loop.
//!
//! Electron runs this as a child process and talks the line protocol to it. The UI owns windows
//! and nothing else, which is what keeps a crash in the shell from stranding a run that is already
//! driving the machine.
//!
//! Threads: the main thread reads stdin; each request is handled on its own short-lived thread so
//! a transcription never holds up an abort; the hotkey pump registers and pumps and only enqueues;
//! the input worker drains that queue. All output goes through one locked writer.

mod brain;
mod daemon;
mod emit;
mod hotkeys;
mod ipc;
mod live;
mod runner;
mod settings;
mod voice;

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use daemon::{vk_slot, Daemon, Modes, Paths};
use emit::Emitter;
use hotkeys::{NoPump, Pumped, Rebind};

/// Start the pump on its own thread (registration and pumping both happen there) and the input
/// worker that drains it. The pump's callbacks only send on the channel; `set_hotkeys` re-registers
/// through the returned pump without a restart, feeding the same channel.
fn start_pump(out: &Emitter) -> Option<(hotkeys::Pump, mpsc::Receiver<Pumped>)> {
    let (tx, rx) = mpsc::channel::<Pumped>();
    match hotkeys::Pump::start(hotkeys::current(), tx, out.clone()) {
        Ok(pump) => Some((pump, rx)),
        Err(e) => {
            out.line(e);
            None
        }
    }
}

/// Double-Escape stops an acting run. It polls the Escape key's state only while Pointer is acting
/// in Act mode, and never registers a hotkey or a hook, so Escape still reaches every app; see
/// `wcore::assistant::control`.
fn start_escape_watcher(daemon: Arc<Daemon>) {
    let _ = std::thread::Builder::new()
        .name("escape".into())
        .spawn(move || {
            let mut watch = wcore::assistant::control::DoubleEscape::new();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(25));
                let armed = daemon.escape_armed();
                // Not armed: the key is not even read.
                let down = armed && platform::input::key_held(0x1B);
                if watch.poll(armed, down, std::time::Instant::now()) {
                    daemon.stop_everything("stopped: Esc pressed twice");
                }
            }
        });
}

fn start_input_worker(daemon: Arc<Daemon>, rx: mpsc::Receiver<Pumped>) {
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
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("pointer"))
        .unwrap_or(cwd)
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

    // CLICKER_NO_HOTKEYS=1 runs the protocol without touching the global keyboard: for smoke tests
    // and for a second copy that must not fight the first over the keys.
    let wanted = std::env::var("CLICKER_NO_HOTKEYS")
        .map(|v| v != "1")
        .unwrap_or(true);
    let pumped = if wanted { start_pump(&out) } else { None };
    let (rebinder, fired): (Box<dyn Rebind>, _) = match pumped {
        Some((pump, rx)) => (Box::new(pump), Some(rx)),
        None => (Box::new(NoPump), None),
    };
    let specs = hotkeys::current();
    let runner: Arc<dyn runner::Runner> = Arc::new(runner::Wired::live());
    let modes = Arc::new(Mutex::new(Modes::default()));
    let services = live::services(out.clone(), runner.clone(), modes.clone(), cwd.clone());
    let assistant = wcore::assistant::Assistant::new(
        services,
        wcore::assistant::gate::Gate::new(Arc::new(live::LiveClassifier)),
    );
    let daemon = Arc::new(Daemon {
        runner,
        listener: Box::new(voice::Microphone::default()),
        out: out.clone(),
        speaker: Arc::new(runner::speak),
        paths: Paths {
            runs: cwd.join("runs"),
            dotenv,
        },
        hotkeys: Mutex::new(daemon::hotkey_hint()),
        talk_vk: vk_slot(hotkeys::vk_of(&specs, "talk")),
        dictate_vk: vk_slot(hotkeys::vk_of(&specs, "dictate")),
        typer: Box::new(daemon::Keyboard),
        modes,
        rebinder,
        rebinding: Mutex::new(()),
        brain: brain::Brain::live(assistant),
    });

    let screens = platform::display::monitors().len();
    out.line(format!("core ready: {screens} displays"));
    if let Some(rx) = fired {
        start_input_worker(daemon.clone(), rx);
    }
    if wanted {
        start_escape_watcher(daemon.clone());
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
