# The line protocol

Electron spawns the Rust core as a child process and they speak newline-delimited JSON over stdin
and stdout. No port, so nothing to firewall, authenticate, or collide with another copy of the app.

Every message is one JSON object on one line. Two kinds:

    --> request   { "id": 7, "method": "start", "params": { "goal": "open youtube" } }
    <-- reply     { "id": 7, "ok": true, "result": { ... } }
    <-- reply     { "id": 7, "ok": false, "error": "no writer is configured" }
    <-- event     { "event": "line", "text": "step 1: app='chrome' ..." }

A reply always carries the `id` of its request. An event never does — it is the core talking on its
own, and the UI is expected to be listening rather than asking.

## Methods the UI calls

Every request is answered on its own thread, so a slow one (`listen_stop` transcribing, `start`)
never holds up a `pause` or `abort` sent after it. Replies can therefore arrive out of order: match
them by `id`. A missing or `null` `params` is the same as `{}`.

| method | params | result |
|---|---|---|
| `state` | — | `{ running, paused, hotkeys, stt }` — `hotkeys` is a display string of the bindings; `stt` is the speech engine: `chrome`, `openai` or `windows` |
| `displays` | — | `[ { index, left, top, right, bottom, primary } ]` in physical pixels |
| `capture` | `{ monitor? }` (default 0) | `{ width, height, origin: [x, y], bytes }` |
| `start` | `{ goal, act }` | `{ queued: bool }`; `act: null` uses the mode from `set_mode` |
| `say` | `{ text }` | `{ command, goal, confidence, heard, actionable, routed }` — what the runner made of a spoken line and what it did with it |
| `heard` | `{ text }` | as `say` — a final transcript from the shell's browser recognizer (chrome engine), which counts as fully heard (`heard: 1.0`) |
| `ask` | `{ question }` | `{ answer: string }` — talk mode, touches nothing |
| `pause` | — | `{ paused: bool }` — toggles; also emits `state` |
| `abort` | — | `{ ok: bool }` — whether there was a run; also emits `state` |
| `listen_start` | — | `{ listening: bool }` — false when already recording; error when voice is off, or on the chrome engine (the shell answers it there) |
| `listen_stop` | — | `{ heard: string }` — `""` for silence |
| `set_mode` | `{ act, voice, hide }` (any subset) | the same three, as applied |
| `runs` | — | `[ { name, goal, outcome, answer, goal_achieved, seconds, steps_taken, acted } ]`, newest first |
| `steps` | `{ name }` | `[ { number, has_shot } ]` |
| `shot` | `{ name, number }` | `{ data_url }` — a `data:image/png;base64,...` URL, `""` when there is no capture |
| `settings` | — | `[ { key, label, value, fallback, secret } ]` |
| `save_settings` | `{ values: { KEY: value } }` | `{ saved: true, keys: [KEY...] }` — names only, never values |

`name` must be a bare run folder name; anything with a separator or `..` is refused.

### Secrets

`settings` lists `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` and `TYPESAFE_API_KEY` with `secret: true`,
an always-empty `value`, and a `fallback` that says only whether one is set. A secret's value never
leaves the core, in a reply, an event or a log line. In `save_settings` an empty secret means
"unchanged"; a non-empty one replaces the stored key. Keys not in the list are ignored. Saved values
take effect in the running core at once, except hotkeys, which need a restart.

### Voice

`listen_start` records the default microphone on a worker thread until `listen_stop`, the longest
utterance (`CLICKER_VOICE_MAX_SECONDS`), or a pause after speech (`CLICKER_BAR_SILENCE`).
`CLICKER_STT` (a non-secret setting) picks the engine: `auto` (default) and `chrome` use Google's
recognizer through the user's own Chrome (or Edge); `openai` and `windows` force those engines.

On **chrome** the core does not record. The shell (`src/stt/bridge.js`) serves `src/stt/stt.html`
on 127.0.0.1 behind a random per-session token and opens it with `webkitSpeechRecognition` in a
dedicated, off-screen Chrome/Edge app window on its own profile (`%LOCALAPPDATA%\pointer\speech-profile`),
started on first use and killed on quit. (Inside Electron the same API fails with `network`.) The
shell answers the panel's and bar's `listen_start` / `listen_stop` itself, in the same reply shapes;
push-to-talk is the core emitting `stt` `start`/`stop` around the held key, after which the shell
sends what it heard as `heard`. The speech window is titled exactly `Pointer speech`, and the core's
window survey skips that title, since its pid is the browser's and unknown to the core.

The Windows engine
(free, keyless) records and recognizes in one go with the built-in recognizer; when it is not
available (speech privacy off, no speech language) `listen_start` and push-to-talk fail with a line
saying which setting to change. On the OpenAI engine the audio
is transcribed by OpenAI (`OPENAI_API_KEY`, model `CLICKER_TRANSCRIBE_MODEL`) with a vocabulary
prompt naming Claude Code, VS Code, Chrome and the commands, so "Claude" is not heard as "cloud";
`CLICKER_VOICE_PROMPT` overrides it and empty disables it.

## Events the core sends

| event | payload | when |
|---|---|---|
| `line` | `{ text }` | anything the run would have printed |
| `state` | `{ running, paused, hotkeys }` | at startup, and when a run starts, pauses, resumes or ends |
| `hotkey` | `{ name }` | a global hotkey fired: `bar`, `talk`, `goal`, `pause`, `abort` or `quit` (the bar opens on `bar`) |
| `highlight` | `{ marks: [ { x, y, w, h, label? } ], seconds }` | point at something on screen for `seconds` |
| `stt` | `{ action: "start" \| "stop" }` | chrome engine only: the talk key went down / up; the shell records and replies with `heard` |
| `answer` | `{ text, spoken }` | an answer is ready, and whether it was read aloud (`CLICKER_SPEAK`) |

`highlight` marks are in PHYSICAL pixels on the virtual desktop — what the core captures and clicks
in, and negative on a monitor above or left of the primary. The shell converts each one to DIPs
for the monitor it is on (`screen.screenToDipRect`); one global scale factor is wrong on a desk
with mixed scaling.

The core handles `pause`, `abort` and `quit` hotkeys itself (quit aborts the run; closing the app
is the shell's call) and `talk` records while the key is held and routes what it heard, as `say`
does. `bar` and `goal` only open windows, which is the shell's job.

All output is UTF-8, one object per line, written under one lock so an event never splits a reply.
Set `CLICKER_NO_HOTKEYS=1` to run the protocol without registering global hotkeys.

## Why a sidecar rather than one process

The core owns the hotkeys, and on Windows a global hotkey is delivered only to the thread that
registered it, while that thread pumps messages. Electron's main thread is already running its own
loop. Keeping them in separate processes means neither has to give up its loop, and a crash in the
UI leaves a run that is already driving the machine able to finish or be aborted.
