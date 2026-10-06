# Rewrite in Rust with an Electron UI — design

**Status:** approved 2026-10-06
**Goal that drives every decision below:** someone downloads one file, runs it, and it works. No
Python, no uv, no terminal.

## Why this is a rewrite and not a port

The measurements say plainly that speed is not the reason. Across 18 real steps on the development
machine: 50% of a step is the network round trip to TypeSafe, 21% is UI Automation COM round trips,
5% is Windows OCR which is already C++, 5% is the screen grab which is already C, and about 7% is
Python's own glue. A perfect rewrite saves roughly 120 ms of 1,764 ms.

The reason is distribution. A Rust binary is a file you can send someone. A Python program is an
install guide.

That means the rewrite is judged on whether a stranger can run it, not on whether it is faster, and
anything that trades away correctness for speed here is trading the wrong thing.

## What must survive the rewrite

Every item below cost real debugging in the Python implementation, and every one is invisible until
it bites. They are the actual deliverable of the last few days; the code is just where they are
written down. The Rust implementation inherits each one, with the test that proves it.

| rule | why it exists |
|---|---|
| Declare per-monitor DPI awareness before any window query | without it every rectangle is a lie on a scaled display, and clicks land short |
| A capture is one display; items carry that display's origin | this desk has a monitor at x=5120 and one at y=-1440 |
| Subtract the origin when converting a control's frame to capture pixels | accessibility frames are virtual-desktop coordinates; without this a click misses by a whole monitor |
| Add the origin back when converting an item to a click point | the same arithmetic in reverse, and doing it twice is the same bug |
| Walk the window being looked at, not every window of the process | Chrome is one process for every window; a run liked a song in another window |
| Off-screen controls are the dangerous ones | nothing on the capture contradicts them, so a wrong one is pressed with high confidence |
| Keep walking a repeated subtree, only suppress emitting it | UIA wraps a window in panes of its own size and label; pruning there loses the whole app |
| A minimized window reports a rect near -32000 | ask the monitor API which display it belongs to, not its rectangle |
| `WS_EX_TRANSPARENT`, `TOOLWINDOW`, `NOACTIVATE` on the drawing overlay, never `WS_EX_LAYERED` | a layered style with no layer surface paints it opaque black |
| Switching to the window already in front is a no-op, not a success | otherwise the loop reports progress, the screen does not change, and it repeats until aborted |
| The run must not read its own UI or its own log | its panel's buttons and its own printed lines came back as things to click |
| Every file write is UTF-8 | the Windows default is cp1252 and screen text is not ASCII |
| Confidence gates are real: a goal must be heard cleanly, a command need only be recognised | short commands score low on any "is this complete" question, and a run that cannot be stopped by voice is worse than one that mishears |

## Shape

```
winclicker/
  crates/
    platform/   windows-rs only: monitors, windows, input, capture, OCR, the UIA walk
    core/       the loop: perception, the decision request, actions, the runner
    api/        TypeSafe and OpenAI clients
    app/        the binary: daemon, global hotkeys, the IPC server
  ui/           Electron: the panel, the command bar, the drawing overlay
```

**One binary, one UI process.** Electron spawns the Rust binary as a sidecar and talks to it over
newline-delimited JSON on stdin and stdout. No local port, so nothing to firewall, authenticate or
collide with. The protocol is the same shape the current panel already uses — the page asks, the
core answers — so the existing HTML, CSS and JS port with their event handlers intact.

## What Electron buys, honestly

It is a 150 MB Chromium next to a WebView2 that already ships with Windows. What it buys is a real
packaging and auto-update story, devtools, and a path to macOS later. That is the trade, taken
deliberately: the goal is shipping to other people, and Electron's installer story is the part of
this that is genuinely solved.

The drawing overlay is the exception and stays native. It needs a transparent, click-through,
never-focused window, and the Python implementation proved a webview will not give one on Windows:
pywebview's transparent window rendered opaque twice. Rust draws it directly.

## Order of work

1. **`platform`**, because it holds every rule in the table above and all the risk. Nothing else
   can be trusted until a Rust UIA walk returns what the Python one returns on the same screen.
2. **`api`**, which is two HTTP clients and is mostly types.
3. **`core`**, which is the loop, and is a transcription of logic that already has 569 tests
   describing it.
4. **`app`** and **`ui`** together, because the IPC contract is only real once both ends exist.

The Python implementation stays on `main` and keeps working throughout. It is the reference: every
Rust component is checked against it on the same screen before the Python one is retired.

## How this is judged

- A stranger downloads one installer, runs it, and presses the hotkey.
- On the development machine's three monitors, the Rust walk returns the same controls as the
  Python one for the same window, and a click computed from an item lands on it.
- Every rule in the table has a test that fails when the rule is broken.
