# Voice-driven daemon — design

**Status:** approved 2026-09-21
**Scope:** pieces 1 (control plane) and 2 (voice) of the four-piece split. Session state and
cross-session memory are explicitly out.

## Goal

Turn `winclicker` from a one-shot command into a resident process that waits for a global hotkey,
hears a spoken instruction, asks jev what that instruction means, and runs it — without the user
ever switching to a terminal.

## Why a daemon

`main()` today parses a goal, runs the loop, and exits. A hotkey has nowhere to live in that shape,
and Windows makes the requirement concrete: `RegisterHotKey` delivers `WM_HOTKEY` only to the thread
that registered the hotkey, and only while that thread pumps messages. So something must stay
resident and keep pumping.

## Architecture

Three threads in one process:

```
main thread    register hotkeys -> GetMessage pump -> put Command on the input queue
input worker   push-to-talk: record while held -> whisper.cpp -> transcript
               overlay: Tk entry -> typed text
               transcript/text -> jev interpret -> Intent
               Intent -> flip Control flags, or put Job(goal) on the run queue
run worker     runner.run(cfg, ctx_factory, control), one job at a time
```

Hotkey callbacks enqueue and return immediately: a three-second utterance must never stall the
message pump, or Windows stops delivering hotkeys.

### Why the transcript goes through jev

A transcript is not a goal. "stop stop stop", "pause for a second", "actually cancel that", and
"open the console and check billing" all arrive the same way, and only the last is a goal. Sending
raw text to `run()` would make the daemon act on the word "stop" by trying to accomplish stopping.

So the transcript becomes a typed decision first, through the same model the loop already uses:

- `command`: a `Choice` over `run_goal`, `stop_run`, `pause_run`, `resume_run`, `quit_daemon`,
  `ignore` — with the daemon's live state (is a run active, is it paused) in the state packet, so
  "pause" while idle reads as `ignore` rather than a pause of nothing.
- `heard`: a `Noul` scoring whether the transcript is a complete, intelligible instruction rather
  than a half-caught fragment or room noise.

Both are gated. Below `VOICE_MIN_CONFIDENCE` (0.55) or with `heard` under 0.5, the daemon prints
what it heard and does nothing. Acting on a misheard command is worse than asking again, because
the actions are real clicks on a real machine.

This keeps the project's rule: the classifier decides, code executes, and no free text is generated
anywhere in the path. The writer model is not involved.

## Components

| file | responsibility |
|---|---|
| `windows.py` (extend) | the only place calling user32: `register_hotkey`, `unregister_hotkey`, `pump_messages`, `key_held`, `post_quit`, and a `check` parameter on `sleep_watching` |
| `hotkeys.py` | parse `"ctrl+alt+space"` into modifiers and a virtual key, hold the registration table, run the pump, dispatch to callbacks |
| `voice.py` | record while a key is held, convert frames to 16 kHz mono float32, transcribe with whisper.cpp, return a transcript |
| `intent.py` | transcript plus daemon state to a typed `Intent` through jev |
| `overlay.py` | a borderless topmost Tk entry: Enter submits, Escape cancels |
| `runner.py` (extend) | `Control`: pause, resume, abort, and the checkpoints the loop calls |
| `daemon.py` | the queues, the three threads, the wiring, the status line |
| `cli.py` (extend) | the `winclicker-daemon` entry point |

## Control flow for pause and abort

`Control` holds two `threading.Event`s. `Control.checkpoint()` blocks while paused and raises the
existing `Abort` when abort is set. The loop calls it at the top of `run_step` and through the
`check` callback passed into `sleep_watching`, which already polls the top-left-corner escape hatch
every 100 ms. Abort therefore reuses the path `Abort` already has: outcome `aborted`, run folder
written, daemon returns to idle. No new exit path, no thread killing.

## Voice

Push-to-talk, no wake word: hold `ctrl+alt+space`, speak, release. The recorder opens a 16 kHz mono
stream and reads frames while `key_held` reports the key down, capped at `VOICE_MAX_SECONDS` (30).
Transcription is whisper.cpp through `pywhispercpp`, model `base.en` by default, loaded lazily on
first use and kept loaded. Probed on this machine: `pywhispercpp` 1.5.1 installs as a prebuilt
wheel on Python 3.13 / Windows AMD64, no compiler needed.

Recordings live in memory only, and are never written to disk.

## Defaults

| setting | default | env |
|---|---|---|
| push to talk | `ctrl+alt+space` | `CLICKER_HOTKEY_TALK` |
| typed goal overlay | `ctrl+alt+g` | `CLICKER_HOTKEY_GOAL` |
| pause / resume | `ctrl+alt+p` | `CLICKER_HOTKEY_PAUSE` |
| abort the run | `ctrl+alt+x` | `CLICKER_HOTKEY_ABORT` |
| quit the daemon | `ctrl+alt+q` | `CLICKER_HOTKEY_QUIT` |
| whisper model | `base.en` | `CLICKER_WHISPER_MODEL` |
| max utterance | `30` seconds | `CLICKER_VOICE_MAX_SECONDS` |
| voice confidence floor | `0.55` | `CLICKER_VOICE_MIN_CONFIDENCE` |

## Failure handling

- A hotkey another app already owns: named in the startup report, the others still register, the
  daemon runs.
- No microphone, missing model, transcription error: one line printed, daemon alive.
- A run that raises: logged with its traceback, daemon returns to idle.
- Interpretation below the confidence floor: the transcript is printed and nothing runs.
- Quit: unregisters hotkeys, aborts any live run, waits for the run worker, exits.

Every job writes its own run folder exactly as the one-shot CLI does today.

## Testing

Every win32, audio and Tk call sits behind a function the tests monkeypatch, so the suite needs no
hardware and stays green on `windows-latest` in CI. Covered: hotkey string parsing including bad
specs, `Control` semantics (pause blocks until resumed, abort raises, abort wins over pause),
intent gating (low confidence and low `heard` both refuse), daemon dispatch against a fake runner
(queue order, one run at a time, quit drains), and the recorder's frame-to-float32 conversion.

End-to-end on real hardware is the merge step's job, not CI's: press the hotkey, speak a goal,
watch a run folder appear.

## Out of scope

Tray icon, wake word, follow-up goals against a previous run's history, cross-session memory, live
world model, resumable runs.
