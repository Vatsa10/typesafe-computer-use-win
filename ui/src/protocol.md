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
| `state` | — | `{ running, paused, hotkeys, stt, stt_lang, mode }` — `hotkeys` is a display string of the bindings; `stt` is the speech engine: `chrome`, `openai` or `windows`; `mode` is `assistant` or `quick` (see Assistant layer) |
| `displays` | — | `[ { index, left, top, right, bottom, primary } ]` in physical pixels |
| `capture` | `{ monitor? }` (default 0) | `{ width, height, origin: [x, y], bytes }` |
| `start` | `{ goal, act }` | `{ queued: bool }`; `act: null` uses the mode from `set_mode` |
| `say` | `{ text }` | `{ command, goal, confidence, heard, actionable, routed }` — what the runner made of a spoken line and what it did with it |
| `heard` | `{ text, purpose? }` | `purpose` is `"talk"` (the default when absent) or `"dictate"`, echoed from the `stt` event that started the recording. Talk: as `say` — a final transcript from the shell's browser recognizer (chrome engine), which counts as fully heard (`heard: 1.0`). Dictate: `{ typed: n }` — the text is typed into the focused field plus a trailing space and never routed ("stop" is typed, not obeyed); empty text types nothing (`typed: 0`); refused with `dictation is off while Pointer is acting` while a run is going in Act mode |
| `ask` | `{ question }` | `{ answer: string }` — talk mode, touches nothing |
| `pause` | — | `{ paused: bool }` — toggles; also emits `state` |
| `abort` | — | `{ ok: bool }` — whether there was a run; also emits `state` |
| `listen_start` | — | `{ listening: bool }` — false when already recording; error when voice is off, or on the chrome engine (the shell answers it there) |
| `listen_stop` | — | `{ heard: string }` — `""` for silence |
| `set_mode` | `{ act, voice, hide }` (any subset) | the same three, as applied. Act is the default, in Assistant mode and Quick alike; Dry run stays a choice |
| `get_answers` | — | `{ fields: [ { key, label, value } ] }` — the saved personal answers (`my-answers.json` under `POINTER_HOME`) |
| `set_answers` | `{ values: { key: text } }` | the same shape, as saved; an empty text removes the answer |
| `runs` | — | `[ { name, goal, outcome, answer, goal_achieved, seconds, steps_taken, acted } ]`, newest first |
| `steps` | `{ name }` | `[ { number, has_shot } ]` |
| `shot` | `{ name, number }` | `{ data_url }` — a `data:image/png;base64,...` URL, `""` when there is no capture |
| `settings` | — | `[ { key, label, value, fallback, secret } ]` |
| `save_settings` | `{ values: { KEY: value } }` | `{ saved: true, keys: [KEY...] }` — names only, never values |
| `hotkeys` | — | `{ names: [...], current: { name: spec }, defaults: { name: spec } }` for `talk`, `dictate`, `bar`, `goal`, `pause`, `abort`, `quit` (in that order) |
| `chat` | `{ text, source: "voice" | "typed" }` (`source` defaults to `typed`) | assistant mode: `{ queued: true, mode: "assistant" }` at once, the turn runs on its own thread and reports by events. Quick mode: the line is routed as `say` is, and the reply is that object plus `mode: "quick"`. Errors: empty `text`, an unknown `source` |
| `gate` | `{ heard, speaking? }` (`speaking` defaults to true) | `{ interrupt: bool, confidence }` — see Assistant layer. Quick mode, or `speaking: false`, is always `{ interrupt: true, confidence: 1 }` |
| `interrupt` | — | `{ ok }` — stops the speech only; the turn carries on |
| `stop_all` | — | `{ ok: true }` — cancels the turn, its `pointer_run`, the speech, the background tasks and any run |
| `new_conversation` | — | `{ ok }` — the old conversation stays on disk and is searchable; the next `chat` starts a new one |
| `tasks` | — | `[ { id, title, status, summary } ]`; `status` is `running`, `done`, `failed`, `stopped` or `interrupted` |
| `task_stop` | `{ id }` | `{ stopped: bool }` — false when it was not running; an error for an unknown id |
| `confirm_reply` | `{ id, allow: bool }` | `{ ok }` — false when the id is unknown, already answered or timed out (a late yes is refused) |
| `camera_frames` | `{ id, images?: [string], error?: string }` | `{ ok }` — answers a `camera` event; `images` are base64 JPEG or `data:image/...` URLs, at most 4 are used; `error` (non-empty) means the capture failed |
| `set_hotkeys` | `{ values: { name: "ctrl+alt+x", ... } }` (any subset of the names) | `{ applied: { name: spec } (all seven, as now in force), refused: [name...] }` — applied live, no restart; see below |

`name` must be a bare run folder name; anything with a separator or `..` is refused.

### Secrets

`settings` lists `OPENAI_API_KEY`, `TYPESAFE_API_KEY` and `POINTER_AGENT_API_KEY` with `secret: true`,
an always-empty `value`, and a `fallback` that says only whether one is set. A secret's value never
leaves the core, in a reply, an event or a log line. In `save_settings` an empty secret means
"unchanged"; a non-empty one replaces the stored key. Keys not in the list are ignored. For the agent key `fallback` can read `not set (the OpenAI key is used)`, since `OPENAI_API_KEY` stands in for it. Saved values
take effect in the running core at once, except `CLICKER_HOTKEY_*` saved this way, which wait for a
restart; change hotkeys with `set_hotkeys` instead.

### Hotkeys

`set_hotkeys` checks every given name before anything is written: an unknown name, a non-string, a
spec that does not parse (`modifier+...+key`, modifiers `ctrl`/`alt`/`shift`/`win`, a letter, digit,
`f1`–`f12` or a named key such as `space`, `rightalt`, `pause`), or two names on the same keys
(compared parsed, so `Ctrl+Alt+X` equals `ctrl+alt+x`) fails the request with
`error: "hotkeys not saved: bar: <reason>; talk: <reason>"` and leaves `.env`, the environment and the
registered keys untouched. Otherwise the given names are written to `.env` as `CLICKER_HOTKEY_<NAME>`,
the environment is updated, and the core re-registers every hotkey on its pump thread (the old set is
released first). `refused` lists names whose keys another app already owns: those are saved but do
not fire until changed or freed, and each also gets a `line`. A `state` event with the new `hotkeys`
hint follows. Held keys (`talk`, `dictate`) switch at once too. With `CLICKER_NO_HOTKEYS=1` the keys
are saved and `refused` is always empty.

### Foreground handoff

Windows only lets the process that received the last input bring a window to the front. A global
hotkey is delivered to the core, not the shell, so before the core emits `hotkey` for `bar` or
`goal` it calls `AllowSetForegroundWindow(ASFW_ANY)`. The shell may then focus the window it opens
(`win.show(); win.focus()`) as soon as it receives the event; the grant lapses at the next input, so
focus promptly rather than after an animation.

### Voice engines: Deepgram and Murf

`CLICKER_STT` order is `deepgram` (when `DEEPGRAM_API_KEY` is set), `chrome`, `windows`; a named value forces
one (`openai` too). On **deepgram** the core records through `platform::audio` and streams 16 kHz PCM to
Deepgram (`nova-3`, interim results, endpointing 400 ms, `utterance_end_ms` 1000, key terms Claude, Claude
Code, Pointer, TypeSafe). The key stays in the core. `CLICKER_STT_LANG` picks the language (unset is `en`;
`auto` or `multi` is code-switching, which handles Hindi mixed with English but answers more slowly).
The shell asks for the session: `listen_start { conv?: bool, purpose?: "talk"|"dictate" }` (`conv` keeps
the microphone open across sentences) and `listen_stop` returns everything heard. Push-to-talk emits
`stt` start/stop exactly as on chrome and the shell answers with `listen_start`/`listen_stop`.
The core ends an utterance itself rather than wait for Deepgram's `speech_final` (which does not fire in a
noisy room): `Finalize` goes after 850 ms of quiet, or 1.6 s when the words stop mid-phrase ("open the", a
trailing "and"/preposition/comma). Two clocks drive it, the local gate's quiet and the age of Deepgram's own
text, and either can fire. When every word is already final the utterance is delivered at once, without
waiting for the server's answer to `Finalize`. The microphone starts recording as soon as a session starts,
while the socket opens (a spare socket is kept open with `KeepAlive`), and the last 2 s are replayed so the
first words are not clipped. A dropped socket in a conversation is reopened once before falling back.

| event | payload | meaning |
|---|---|---|
| `transcript` | `{ text, final, purpose }` | `final: false` is everything of the current utterance so far; `final: true` is the finished utterance. Never logged |
| `stt_fallback` | `{ engine: "chrome", reason }` | Deepgram could not connect or the socket dropped; `state.stt` reads `chrome` for two minutes and the shell moves an open conversation to the browser recognizer. A line says so |
| `stt_stats` | `{ floor, threshold, peak_rms, heard_speech, seconds }` | levels of a finished session, for diagnosing the silence gate; no words |
| `speech` with `real: true` | `{ action: "start"|"end", real, engine, cut, first_audio_ms? }` | sound really started / ended at the audio device (`cut` when interrupted). `first_audio_ms` on a start is the time from the request to first sound. The plain `speech` events still bracket the voice call and carry the text |
| `speech` with `real: true, ack: true` | `{ action, real, ack, text?, engine, cut? }` | an acknowledgement clip ("Hmm.", "Okay.", "Let's see."), played the moment a voice request ends. Lowest priority: the real reply, `interrupt`, Stop or more speech cuts it. It carries its `text` so the shell adds it to the echo history; it is **not** a reply. Skipped when replies are muted. Clips are made once in the chosen voice under `POINTER_HOME/voice/ack/<engine>-<voice hash>/` |

**Voices.** Auto order is Rumik (`RUMIK_API_KEY`), then Murf (`MURF_API_KEY`), then Windows SAPI;
`POINTER_TTS=rumik|murf|sapi` forces one, and a failing cloud voice falls back down the chain with a line.
Rumik is `POST silk-api.rumik.ai/v1/tts` with `audio_format: "pcm"` (24 kHz mono), `RUMIK_MODEL`
(`mulberry` default, or `muga`) and `RUMIK_SPEAKER` (default `emma`). A reply is split at sentence
boundaries and two sentences are synthesised at a time into one gap-free playback, over one pooled,
kept-warm connection. While the mic is open the core cancels the echo of its own playback
(`POINTER_AEC=0` turns that off).

**Murf** (`POINTER_TTS=murf`): `POINTER_VOICE` is a Murf voice
id (default `en-US-natalie`) or, on SAPI, part of an installed voice name. Audio streams as raw PCM and is
played by the core as bytes arrive (about 0.25 s to first sound); `interrupt` resets the device, so sound
ends within tens of milliseconds.

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
| `hotkey` | `{ name }` | a global hotkey fired: `bar`, `talk`, `dictate`, `goal`, `pause`, `abort` or `quit` (the bar opens on `bar`) |
| `highlight` | `{ marks: [ { x, y, w, h, label? } ], seconds }` | point at something on screen for `seconds` |
| `stt` | `{ action: "start" | "stop", purpose: "talk" | "dictate" }` | chrome engine only: the talk or dictate key went down / up; the shell records and replies with `heard`, passing the same `purpose` back |
| `answer` | `{ text, spoken }` | an answer is ready, and whether it was read aloud (`CLICKER_SPEAK`) |
| `point` | `{ x, y, w, h, label, tone, step?, of?, hold, shape? }` or `{ clear: true }` | the buddy points at one box for `hold` seconds; see below. `shape` is set by the assistant's `interact` |
| `assistant` | `{ state, detail }` | `state` is `idle`, `listening`, `thinking`, `working` (`detail` names the tool), `speaking` or `control` (see below) |
| `reply` | `{ text }` | text for the user to read: from `interact`, or the closing text of a turn that never called it (then nothing is spoken) |
| `speech` | `{ action: "start" | "end", text }` | the assistant's voice began / finished (or was cut off) a line |
| `task` | `{ id, title, status, summary }` | a background task started, finished, failed or was stopped |
| `confirm` | `{ id, summary }` | the assistant needs a yes or no before it writes a file or runs a shell command; answer with `confirm_reply`. No answer in 60 s is a no |
| `camera` | `{ action: "capture", id, seconds }` | the assistant wants the camera for about `seconds`; answer with `camera_frames` within 10 s |

`highlight` marks are in PHYSICAL pixels on the virtual desktop — what the core captures and clicks
in, and negative on a monitor above or left of the primary. The shell converts each one to DIPs
for the monitor it is on (`screen.screenToDipRect`); one global scale factor is wrong on a desk
with mixed scaling.

### `point`

One event for every kind of pointing. `x, y, w, h` are the target's box in PHYSICAL virtual-desktop
pixels, the captured display's origin added once (the same coordinates as `highlight` and as a
click). `label` is what to show beside it, `tone` is `point` (the thing to use) or `note`, `hold` is
seconds to stay, and `step` / `of` (1-based) appear only for a teach step. `{ "clear": true }` ends
pointing. Who points:

- **Dry run:** besides the `highlight` box, `point` at the chosen target, label `would press: <text>`,
  hold 5 s.
- **Act preview:** before pressing an on-screen item the core sends `point` (label `press: <text>`;
  `type into: <label>` before typing into a focused field with a box), then waits
  `CLICKER_POINT_LEAD_MS` (default 350, 0 disables) so the buddy gets there first. No
  acknowledgement is awaited. Actions with nothing on screen (open a URL, launch an app, scroll,
  keys) are neither pointed at nor delayed.
- **Teach (`ask`, or a spoken question):** one capture, then `answer` with a short answer, then for
  each of up to 5 steps: `point` at the step's item (`step` i `of` n, label = the step, hold = the
  pause below), a `line` `step i/n: <text>`, the step read aloud, and a pause of 60 ms a character
  (1.5–6 s) so the voice finishes before the next. A step naming no listed item is only said, with
  no `point`. Teaching ends with `{ clear: true }`. A spoken "stop" (or the abort hotkey) ends it
  between steps; the steps are about the capture taken when the question was asked.

### Assistant tools that reach the machine (no new events)

The assistant's `locate{what}` asks TypeSafe's box primitive where something is on the screen under the
mouse and returns a mark (`L1`, ...) that `interact` can point at (`marks: [{ located: "L1" }]`); the
box becomes an ordinary `point` event in physical virtual-desktop pixels, display origin added once.
`locate` only looks: it never clicks. An item that `observe` already lists is preferred. `world`
returns what Pointer knows of the machine (windows per display, Chrome/Edge tab titles and the active
address without query or fragment, the app catalog, recent changes, Pointer's own actions); a short
form is added to every turn. `open_app{key}` and `switch_tab{title}` act only in Act mode; in Dry mode
they change nothing and return `{ acted: false, dry_mode: true, would_launch | would_select }`.
`POINTER_JEV_VISION` is `auto` (the default: the downscaled screen image goes to TypeSafe only when the
screen is sparse in readable items, fewer than 8 OCR or accessibility items), `on` (always) or `off`
(never). Measured: the image tripled decision latency (1.2 s against 0.42 s) with no better decisions on
a screen that reads well as text. `locate` always uses the image.
Only the app catalog (`apps-v2.json`) and Pointer's own action log (`world-actions.json` in
`POINTER_HOME`) are written to disk; window and tab titles never are.

### Pointer context

Every capture records the mouse: its position (physical virtual-desktop pixels), its display,
seconds since the last input, and the control under it (UI Automation `ElementFromPoint`, never one
of Pointer's own windows: the shell's pid in `POINTER_UI_PID`, or the titles `Pointer buddy` and
`Pointer speech`). Questions (`ask`, spoken questions, teach) read the display **under the mouse**,
since people point at what they ask about; goals read the display of the foreground window, and when
the mouse moved in the last 10 s on another display, that display's top window is named too. The
models are told `the mouse is at (x, y) on display N over <role> '<label>'`, that "this", "here" and
"that" mean what is under the mouse, and which listed item contains the mouse (`under_mouse: true`),
so "click this" or "what does this do?" resolve to it. Answers also get every display and the
windows open on all of them, so "what's on my other screen" is answered at least by window titles.
None of this adds an event or a field to the protocol.

The shell's buddy window is titled exactly `Pointer buddy`; the core's window survey skips that
title, as it does `Pointer speech`.

The core handles `pause`, `abort` and `quit` hotkeys itself (quit aborts the run; closing the app
is the shell's call) and `talk` records while the key is held and routes what it heard, as `say`
does. `bar` and `goal` only open windows, which is the shell's job (after the foreground handoff
above).

## Assistant layer

Two modes, the user's choice (Settings): `POINTER_MODE=assistant` (the default when an agent key
exists) and `quick`. With no agent key (`POINTER_AGENT_API_KEY`, else `OPENAI_API_KEY`) the core is in
quick mode whatever `POINTER_MODE` says; that is not an error. `state.mode` reports the one in force.
Quick mode is exactly the behaviour described above: nothing in this section happens in it, and
`chat` is `say`.

In assistant mode `say`, spoken lines (`talk` key, `heard` as talk) and `chat` all go to the parent
agent as `chat`, `source: "voice"` for the first two. `heard` with `purpose: "dictate"` is still only
typed. The talk key also emits `assistant { state: "listening" }`. `start` and `ask` are unchanged.
`POINTER_LLM` picks the provider: `openai` (agent model `gpt-6-luna`; any OpenAI-compatible server through
`POINTER_AGENT_BASE_URL`) or `claude` (`claude-haiku-5-5`). `POINTER_AGENT_MODEL` overrides the model,
`POINTER_AGENT_API_KEY` (else `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) is the key.

**Tools.** `interact`, `observe`, `locate`, `world`, `open_app`, `switch_tab`, `pointer_run`, `read_file`,
`list_dir`, `write_file`, `run_shell`, `camera_look`, `start_task`, `list_tasks`, `stop_task`,
`resume_task`, `search_history`, and for documents and forms: `find_documents` (names, paths and dates
of resumes and other files, never contents), `get_profile` (a resume read into a structured profile),
`form_answer` (one field from the profile or from the saved answers), `save_answer`, `form_read` and
`form_fill` (the front window's fields read from the page, and filled with real focus and typing,
each result read back), and `confirm`. Dry mode: `form_fill` types nothing and returns what it would fill.

**Autonomy.** `POINTER_AUTONOMY=full` (the default; Settings can switch to `ask`). In full the assistant
acts without asking: files are written, shell commands run (a parsed read-only PowerShell allowlist
always runs), resumes are read and forms submitted. It sends `confirm` only before a payment or purchase,
before deleting or overwriting the user's files (a shell command that does so, or `write_file` over an
existing file) and before entering a password. Forbidden paths stay blocked. In `ask` every file write,
every shell command outside the read-only allowlist and the first `get_profile` of a session need a yes.

**My answers.** What only the user can say on a form (expected salary, notice period, start date, work
authorisation, visa sponsorship, gender, disability, race, veteran status, relocation, criminal history,
attestation) lives in `my-answers.json` under `POINTER_HOME`, filled once in Settings ("My answers") or
learned when the user answers. `form_answer` uses it with no question; it never infers these from a
resume. With no saved answer it returns `needs_user`: an optional field is left empty, and for a required
one the assistant asks once, in one message, and calls `save_answer`.

**One foreground turn at a time.** A new `chat` cancels the unfinished turn (the model call, a
running `pointer_run`, a pending `confirm`, the speech) and carries on in the same conversation.
Conversations are saved under `POINTER_HOME\conversations`; no API key and no camera image is ever
written there. A long conversation is compacted into a summary.

**Only `interact` reaches the user.** It speaks, draws marks and shows text in one call. The core
validates the whole call first; one bad mark rejects all of it and nothing happens. An identical
repeat within a turn is a no-op. On success the events come in this order: a `point { clear: true }`,
one `point` per mark, `reply { text }`, `assistant { state: "speaking" }`, `speech { action: "start" }`,
the voice, `speech { action: "end" }`. Each mark is a `point` with the usual physical-pixel box (origin
added once) and a `shape`: `box`, `circle`, `arrow`, `underline`, `note` or `step` (an unknown shape
is drawn as a box; the core already sends only these). A `note` mark has `tone: "note"`; marks with
`shape: "step"` carry `step` and `of`. Several marks arrive as several `point` events back to back,
so the shell should draw all of them until the next `point { clear: true }` or `hold` runs out. In
quick mode `compose_teach` also returns a `shape` per step (an invalid one becomes `box`), but the
quick-mode teach `point` events do not carry it yet.

**`control`.** While the assistant runs `pointer_run` in Act mode it emits
`assistant { state: "control" }` (it really is operating the computer); the next state replaces it.
**Double-Escape** stops it: two presses of Escape within 1.5 s, only while Pointer is acting in Act
mode (an assistant turn or a run going). The core does not register Escape as a hotkey or hook
the keyboard, so Escape still reaches whatever app has focus; it polls the key's physical state
every 25 ms, and only while armed. In Dry mode or when idle it does not read the key at all. The
effect is `stop_all`, and a `line` says `stopped: Esc pressed twice`.

**`gate`.** While the assistant is speaking, the shell may ask whether what the microphone just
caught is meant for it, before it interrupts. The core asks TypeSafe a Choice, `interrupt` or `ignore`,
with the transcript, the request being worked on and the last line spoken. A transcript starting with
`no`, `wait`, `stop` or `actually` always interrupts; the same text heard again within 15 s passes
(interrupts); an error or no answer within 1.5 s interrupts; `CLICKER_TURN_GATE=off` makes every
line interrupt. An empty transcript never does. To act on an interrupt the shell calls `chat`.

**Confirmations.** Reading a file or folder inside the user profile is free, except `.env` files,
browser password and cookie stores and SSH keys, which are refused, as is anything outside the
profile. What needs a yes (see Autonomy for when) sends `confirm { id, summary }` and waits for
`confirm_reply`; no reply in 60 s, a cancel or `allow: false` is a no, and the model is told so.
`run_shell` runs without a card when the command parses as read-only: a pipeline of `Get-ChildItem`,
`Get-Item`, `Get-Content`, `Select-String`, `Measure-Object`, `Sort-Object`, `Select-Object`,
`Where-Object`, `Format-*`, `Test-Path`, `Get-Process` and `Get-Date` with known parameters and literal
arguments; any redirect, `;`, `&`, backtick, `$(...)`, variable, `Invoke-*`, `iex`, other cmdlet or
filter block that calls anything is not read-only, nor is a path that is forbidden or outside the
profile, or files piped into a file reader.

**Background tasks** (at most three) have their own conversations and only the file and shell tools,
never the screen, the voice, the camera or the UI. Their only voice is the `task` event. The
registry is saved to `tasks.json`; one that was running when the app closed is `interrupted` on the
next start and is resumed only when the user asks the assistant to.

**Camera.** `camera_look` is refused unless `POINTER_CAMERA` is `on` (default off). It emits `camera`
and waits up to 10 s for `camera_frames`; the frames go to the model request and nowhere else.

## Dictation

`dictate` (default `ctrl+alt+d`, `CLICKER_HOTKEY_DICTATE`) is held like `talk`, but what is heard is
typed into whatever field has focus when the key comes up, with a trailing space so consecutive
dictations join. It never reaches intent routing. On openai/windows the core records and types; on
chrome the core emits `stt` with `purpose: "dictate"` and the shell must send the transcript back as
`heard { text, purpose: "dictate" }` (the shell should not focus any of its windows meanwhile). The
core waits ~120 ms after release and for Ctrl/Alt/Shift/Win to be up before typing, so the hotkey's
modifiers do not turn letters into shortcuts. The log gets `dictated N characters`, never the text.
Dictation works beside a dry run but is refused, with the line `dictation is off while Pointer is
acting`, while a run is going in Act mode, since its keystrokes would land among the run's.

All output is UTF-8, one object per line, written under one lock so an event never splits a reply.
Set `CLICKER_NO_HOTKEYS=1` to run the protocol without registering global hotkeys.

## Why a sidecar rather than one process

The core owns the hotkeys, and on Windows a global hotkey is delivered only to the thread that
registered it, while that thread pumps messages. Electron's main thread is already running its own
loop. Keeping them in separate processes means neither has to give up its loop, and a crash in the
UI leaves a run that is already driving the machine able to finish or be aborted.
