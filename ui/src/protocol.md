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

| method | params | result |
|---|---|---|
| `state` | — | `{ running, paused, hotkeys }` |
| `start` | `{ goal, act }` | `{ queued: bool }` |
| `pause` | — | `{ paused: bool }` |
| `abort` | — | `{ ok: bool }` |
| `ask` | `{ question }` | `{ answer: string }` — talk mode, touches nothing |
| `listen_start` | — | `{ listening: bool }` |
| `listen_stop` | — | `{ heard: string }` |
| `set_mode` | `{ act, voice, hide }` | the same three, as applied |
| `runs` | — | `[ { name, goal, outcome, answer, seconds, steps, acted } ]` |
| `steps` | `{ name }` | `[ { number, has_shot } ]` |
| `shot` | `{ name, number }` | `{ data_url }` |
| `settings` | — | `[ { key, label, fallback, value } ]` |
| `save_settings` | `{ values }` | `{ saved: bool }` |

## Events the core sends

| event | payload | when |
|---|---|---|
| `line` | `{ text }` | anything the run would have printed |
| `state` | `{ running, paused }` | a run starts, pauses or ends |
| `hotkey` | `{ name }` | a global hotkey fired, so the UI can react (the bar opens on `bar`) |
| `answer` | `{ text, spoken }` | an answer is ready, and whether it was read aloud |

## Why a sidecar rather than one process

The core owns the hotkeys, and on Windows a global hotkey is delivered only to the thread that
registered it, while that thread pumps messages. Electron's main thread is already running its own
loop. Keeping them in separate processes means neither has to give up its loop, and a crash in the
UI leaves a run that is already driving the machine able to finish or be aborted.
