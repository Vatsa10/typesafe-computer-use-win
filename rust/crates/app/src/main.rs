//! The core: one binary that owns the hotkeys, the screen and the loop.
//!
//! Electron runs this as a child process and talks the line protocol to it. The UI owns windows
//! and nothing else, which is what keeps a crash in the shell from stranding a run that is already
//! driving the machine.

mod ipc;

use std::io::{BufRead, Write};

use serde_json::json;

/// Say something nobody asked for. The shell is listening from the moment it spawns this.
fn emit(out: &mut impl Write, name: &str, payload: serde_json::Value) {
    let message = ipc::event(name, payload);
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn main() {
    // Before any window or monitor is asked about anything. Without it every rectangle Windows
    // reports is wrong on a scaled display, and a click computed from one lands short.
    platform::display::declare_dpi_aware();

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let screens = platform::display::monitors().len();
    emit(&mut stdout, "line", json!({ "text": format!("core ready: {screens} displays") }));
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Some(request) = ipc::parse(&line) else {
            continue; // not a request: the shell may be mid-upgrade, and exiting would strand a run
        };
        let reply = handle(&request);
        let encoded = serde_json::to_string(&reply).unwrap_or_else(|_| String::from("{}"));
        if writeln!(stdout, "{encoded}").is_err() || stdout.flush().is_err() {
            break; // the shell is gone
        }
    }
}

fn handle(request: &ipc::Request) -> ipc::Reply {
    match request.method.as_str() {
        "state" => ipc::Reply::ok(request.id, json!({ "running": false, "paused": false, "hotkeys": "" })),
        "capture" => {
            // Which display to read. Not always the primary: the one being worked on is the one
            // that matters, and this desk has a monitor above the primary and one to the right.
            let wanted = request.params.get("monitor").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let screens = platform::display::monitors();
            let Some(screen) = screens.get(wanted) else {
                return ipc::Reply::failed(request.id, format!("no display {wanted}"));
            };
            match platform::capture::capture_monitor(screen) {
                Some(shot) => ipc::Reply::ok(
                    request.id,
                    json!({ "width": shot.width, "height": shot.height,
                            "origin": [shot.origin.0, shot.origin.1], "bytes": shot.bgra.len() }),
                ),
                None => ipc::Reply::failed(request.id, "the display would not capture"),
            }
        }
        "displays" => {
            let screens: Vec<_> = platform::display::monitors()
                .into_iter()
                .map(|m| json!({ "index": m.index, "left": m.left, "top": m.top,
                                 "right": m.right, "bottom": m.bottom, "primary": m.primary }))
                .collect();
            ipc::Reply::ok(request.id, json!(screens))
        }
        other => ipc::Reply::failed(request.id, format!("no method {other}")),
    }
}
