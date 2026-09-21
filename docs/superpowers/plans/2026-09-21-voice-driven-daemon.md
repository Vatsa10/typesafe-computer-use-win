# Voice-Driven Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `winclicker` into a resident daemon that runs a spoken or typed instruction on a global hotkey, routing every utterance through jev before anything acts.

**Architecture:** Three threads in one process — a main thread that registers Windows hotkeys and pumps messages, an input worker that records/transcribes/interprets, and a run worker that executes one `runner.run()` at a time. Hotkey callbacks only enqueue. A transcript is never treated as a goal until a jev `Choice` says it is one.

**Tech Stack:** Python 3.12+, ctypes/user32, `pywhispercpp` (whisper.cpp, prebuilt wheel verified on 3.13/Windows AMD64), `sounddevice`, stdlib `tkinter`, `typesafe-sdk`, pytest.

**Spec:** `docs/superpowers/specs/2026-09-21-voice-driven-daemon-design.md`

## Global Constraints

- Every call into user32, UI Automation, or an OCR engine lives in `windows.py` and nowhere else.
- Line length 130, ruff rules `E,F,I,B,UP,SIM,RUF`, `uv run ruff check .` and `uv run ruff format --check .` must pass.
- Tests must run with no microphone, no display interaction and no hotkey registration: all hardware calls are monkeypatched. CI is `windows-latest` with the base dependency set only — **no test may import `numpy`, `sounddevice` or `pywhispercpp` at module scope**, because those arrive only with the `voice` extra.
- Every file written by the package uses `encoding="utf-8"` explicitly.
- No commit message mentions Claude, Anthropic, or any model. No `Co-Authored-By` trailers.
- New dependencies go in the `voice` optional extra, never the base set.
- Confidence gates are real: nothing acts on a voice command below the floor.

## Parallel execution map

Tasks 1–7 have **exclusive file ownership** and can run at the same time in separate subagents. Task 8 and 9 wait for wave one. The merge pass is the main session's job.

| wave | task | owns these files exclusively |
|---|---|---|
| 1 | T1 win32 primitives | `typesafe_computer_use_win/windows.py`, `tests/test_hotkey_primitives.py` |
| 1 | T2 hotkey manager | `typesafe_computer_use_win/hotkeys.py`, `tests/test_hotkeys.py` |
| 1 | T3 voice capture | `typesafe_computer_use_win/voice.py`, `tests/test_voice.py` |
| 1 | T4 overlay | `typesafe_computer_use_win/overlay.py`, `tests/test_overlay.py` |
| 1 | T5 control | `typesafe_computer_use_win/runner.py`, `tests/test_control.py` |
| 1 | T6 intent | `typesafe_computer_use_win/intent.py`, `tests/test_intent.py` |
| 1 | T7 settings + packaging | `typesafe_computer_use_win/config.py`, `pyproject.toml` |
| 2 | T8 daemon | `typesafe_computer_use_win/daemon.py`, `tests/test_daemon.py` |
| 2 | T9 entry point + docs | `typesafe_computer_use_win/cli.py`, `README.md`, `CONTRIBUTING.md`, `.env.example` |
| 3 | merge | main session: conflicts, full suite, hardware end-to-end, commits |

No two tasks write the same file. A subagent that believes it needs to edit a file it does not own must stop and report instead of editing.

---

### Task 1: Win32 hotkey and key-state primitives

**Files:**
- Modify: `typesafe_computer_use_win/windows.py` (append a new section; also change `sleep_watching`)
- Test: `tests/test_hotkey_primitives.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `register_hotkey(hotkey_id: int, modifiers: int, vk: int) -> bool`
  - `unregister_hotkey(hotkey_id: int) -> None`
  - `pump_messages(on_hotkey: Callable[[int], None], stop: Callable[[], bool]) -> None`
  - `key_held(vk: int) -> bool`
  - `post_quit_message(thread_id: int) -> None`
  - `current_thread_id() -> int`
  - `MOD_ALT = 0x0001`, `MOD_CONTROL = 0x0002`, `MOD_SHIFT = 0x0004`, `MOD_WIN = 0x0008`, `MOD_NOREPEAT = 0x4000`
  - `sleep_watching(seconds: float, check: Callable[[], None] | None = None) -> None` — `check` defaults to `check_abort`

- [ ] **Step 1: Write the failing test**

```python
"""The win32 primitives the hotkey manager sits on."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import windows


def test_modifier_flags_match_the_win32_values():
    assert (windows.MOD_ALT, windows.MOD_CONTROL, windows.MOD_SHIFT, windows.MOD_WIN) == (1, 2, 4, 8)
    assert windows.MOD_NOREPEAT == 0x4000


def test_sleep_watching_polls_the_check_it_is_given(monkeypatch):
    calls = []
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    ticks = iter([0.0, 0.05, 0.2])
    monkeypatch.setattr(windows.time, "monotonic", lambda: next(ticks, 99.0))
    windows.sleep_watching(0.1, check=lambda: calls.append("checked"))
    assert calls, "the caller's check must be polled while waiting"


def test_sleep_watching_defaults_to_the_corner_escape_hatch(monkeypatch):
    seen = []
    monkeypatch.setattr(windows, "check_abort", lambda: seen.append("corner"))
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    ticks = iter([0.0, 0.05, 0.2])
    monkeypatch.setattr(windows.time, "monotonic", lambda: next(ticks, 99.0))
    windows.sleep_watching(0.1)
    assert seen == ["corner"]


def test_a_refused_registration_reports_false_rather_than_raising(monkeypatch):
    monkeypatch.setattr(windows.user32, "RegisterHotKey", lambda hwnd, i, m, k: 0)
    assert windows.register_hotkey(1, windows.MOD_ALT, 0x20) is False


def test_pump_stops_when_the_stop_callback_says_so(monkeypatch):
    monkeypatch.setattr(windows.user32, "PeekMessageW", lambda *a: 0)
    monkeypatch.setattr(windows.time, "sleep", lambda s: None)
    stops = iter([False, False, True])
    windows.pump_messages(on_hotkey=lambda i: None, stop=lambda: next(stops))
    # returning at all is the assertion: an unstoppable pump hangs the test
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_hotkey_primitives.py -v`
Expected: FAIL with `AttributeError: module 'typesafe_computer_use_win.windows' has no attribute 'MOD_ALT'`

- [ ] **Step 3: Change `sleep_watching` to take a check**

Replace the existing function in `windows.py`:

```python
def sleep_watching(seconds: float, check: Callable[[], None] | None = None) -> None:
    """Wait, polling an interrupt check every 100 ms. The default check is the corner escape hatch;
    the daemon passes one that also honours pause and abort."""
    check = check_abort if check is None else check
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        check()
        time.sleep(0.1)
```

Add `from collections.abc import Callable` to the imports at the top of the file.

- [ ] **Step 4: Add the hotkey section**

Append to `windows.py`, after the input section:

```python
# ------------------------------------------------------------------ global hotkeys

MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN = 0x0001, 0x0002, 0x0004, 0x0008
MOD_NOREPEAT = 0x4000  # one WM_HOTKEY per press, however long the key is held
WM_HOTKEY, WM_QUIT = 0x0312, 0x0012
PM_REMOVE = 0x0001
PUMP_IDLE_SECONDS = 0.02


def current_thread_id() -> int:
    return int(kernel32.GetCurrentThreadId())


def register_hotkey(hotkey_id: int, modifiers: int, vk: int) -> bool:
    """Claim a system-wide key combination for this thread. False when another process holds it."""
    return bool(user32.RegisterHotKey(None, hotkey_id, modifiers | MOD_NOREPEAT, vk))


def unregister_hotkey(hotkey_id: int) -> None:
    user32.UnregisterHotKey(None, hotkey_id)


def post_quit_message(thread_id: int) -> None:
    """Ask a pumping thread to return. Safe to call from another thread, which is the point."""
    user32.PostThreadMessageW(thread_id, WM_QUIT, 0, 0)


def pump_messages(on_hotkey: Callable[[int], None], stop: Callable[[], bool]) -> None:
    """Deliver WM_HOTKEY to `on_hotkey` until `stop()` is true or WM_QUIT arrives.

    Windows posts WM_HOTKEY only to the thread that registered the hotkey, and only while that
    thread pumps. PeekMessage rather than GetMessage, so `stop` is consulted on a timer instead of
    only when a message happens to arrive.
    """
    message = wintypes.MSG()
    while not stop():
        if not user32.PeekMessageW(ctypes.byref(message), None, 0, 0, PM_REMOVE):
            time.sleep(PUMP_IDLE_SECONDS)
            continue
        if message.message == WM_QUIT:
            return
        if message.message == WM_HOTKEY:
            on_hotkey(int(message.wParam))


def key_held(vk: int) -> bool:
    """True while a virtual key is physically down. Push-to-talk needs the release that
    RegisterHotKey never reports."""
    return bool(user32.GetAsyncKeyState(vk) & 0x8000)
```

- [ ] **Step 5: Run the tests**

Run: `uv run pytest tests/test_hotkey_primitives.py -v && uv run pytest -q`
Expected: all PASS. The existing suite must stay green — `sleep_watching` kept its old call signature.

- [ ] **Step 6: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/windows.py tests/test_hotkey_primitives.py
git commit -m "Add the win32 primitives a global hotkey needs

RegisterHotKey delivers WM_HOTKEY only to the registering thread and only
while it pumps, and it never reports a release, so push-to-talk also needs
GetAsyncKeyState. sleep_watching now takes the interrupt check to poll, so a
caller can fold pause and abort into the corner escape hatch."
```

---

### Task 2: Hotkey specs and the manager

**Files:**
- Create: `typesafe_computer_use_win/hotkeys.py`
- Test: `tests/test_hotkeys.py`

**Interfaces:**
- Consumes (from T1, assume these exist — do not edit `windows.py`): `windows.register_hotkey`, `windows.unregister_hotkey`, `windows.pump_messages`, `windows.key_held`, `windows.post_quit_message`, `windows.current_thread_id`, `windows.MOD_ALT`, `windows.MOD_CONTROL`, `windows.MOD_SHIFT`, `windows.MOD_WIN`.
- Produces:
  - `parse_hotkey(spec: str) -> tuple[int, int]` returning `(modifiers, vk)`
  - `VK_NAMES: dict[str, int]`
  - `class Hotkeys` with `add(name: str, spec: str, callback: Callable[[], None]) -> None`, `register() -> list[str]` (returns the names that failed), `run() -> None`, `stop() -> None`, `vk_of(name: str) -> int`

- [ ] **Step 1: Write the failing test**

```python
"""Hotkey specs, and the table that turns a WM_HOTKEY id back into a callback."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import hotkeys, windows


def test_a_spec_becomes_modifier_flags_and_a_virtual_key():
    mods, vk = hotkeys.parse_hotkey("ctrl+alt+space")
    assert mods == windows.MOD_CONTROL | windows.MOD_ALT
    assert vk == 0x20


def test_a_spec_is_case_and_space_insensitive():
    assert hotkeys.parse_hotkey(" CTRL + Alt + G ") == hotkeys.parse_hotkey("ctrl+alt+g")


def test_a_letter_or_digit_needs_no_table_entry():
    assert hotkeys.parse_hotkey("ctrl+alt+7")[1] == ord("7")


def test_an_unknown_key_is_refused_by_name():
    with pytest.raises(ValueError, match="frobnicate"):
        hotkeys.parse_hotkey("ctrl+frobnicate")


def test_a_spec_without_a_key_is_refused():
    with pytest.raises(ValueError, match="no key"):
        hotkeys.parse_hotkey("ctrl+alt")


def test_registered_hotkeys_dispatch_to_their_callback(monkeypatch):
    fired = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: True)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: fired.append("talk"))
    keys.add("quit", "ctrl+alt+q", lambda: fired.append("quit"))
    assert keys.register() == []
    keys.dispatch(keys.id_of("quit"))
    assert fired == ["quit"]


def test_a_hotkey_another_app_owns_is_reported_by_name(monkeypatch):
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: i != 2)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.add("taken", "ctrl+alt+p", lambda: None)
    assert keys.register() == ["taken"]


def test_an_unknown_hotkey_id_is_ignored_rather_than_raising():
    keys = hotkeys.Hotkeys()
    keys.dispatch(999)  # a stale WM_HOTKEY from a hotkey just unregistered


def test_stop_unregisters_everything_it_registered(monkeypatch):
    removed = []
    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: True)
    monkeypatch.setattr(windows, "unregister_hotkey", removed.append)
    monkeypatch.setattr(windows, "post_quit_message", lambda tid: None)
    keys = hotkeys.Hotkeys()
    keys.add("talk", "ctrl+alt+space", lambda: None)
    keys.register()
    keys.stop()
    assert removed == [keys.id_of("talk")]
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_hotkeys.py -v`
Expected: FAIL with `ModuleNotFoundError: No module named 'typesafe_computer_use_win.hotkeys'`

- [ ] **Step 3: Write the implementation**

```python
"""Global hotkeys: specs like "ctrl+alt+space", and the table that dispatches WM_HOTKEY.

The win32 calls live in windows.py; this module owns the naming, the registration table and the
pump's lifetime.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from threading import Event

from . import windows

MODIFIERS = {
    "ctrl": windows.MOD_CONTROL,
    "control": windows.MOD_CONTROL,
    "alt": windows.MOD_ALT,
    "shift": windows.MOD_SHIFT,
    "win": windows.MOD_WIN,
}

# Only the keys worth binding to. A letter or a digit needs no entry: its virtual key is its ASCII
# code, which is why "ctrl+alt+g" works without g appearing here.
VK_NAMES = {
    "space": 0x20,
    "enter": 0x0D,
    "return": 0x0D,
    "tab": 0x09,
    "escape": 0x1B,
    "esc": 0x1B,
    "backspace": 0x08,
    "insert": 0x2D,
    "delete": 0x2E,
    "home": 0x24,
    "end": 0x23,
    "pageup": 0x21,
    "pagedown": 0x22,
    "left": 0x25,
    "up": 0x26,
    "right": 0x27,
    "down": 0x28,
    **{f"f{n}": 0x6F + n for n in range(1, 13)},
}


def parse_hotkey(spec: str) -> tuple[int, int]:
    """ "ctrl+alt+space" to the modifier mask and virtual key RegisterHotKey wants."""
    parts = [p.strip().lower() for p in spec.split("+") if p.strip()]
    if not parts:
        raise ValueError(f"hotkey {spec!r} is empty")
    modifiers = 0
    key: str | None = None
    for part in parts:
        if part in MODIFIERS:
            modifiers |= MODIFIERS[part]
        elif key is None:
            key = part
        else:
            raise ValueError(f"hotkey {spec!r} names two keys, {key!r} and {part!r}")
    if key is None:
        raise ValueError(f"hotkey {spec!r} has no key, only modifiers")
    if key in VK_NAMES:
        return modifiers, VK_NAMES[key]
    if len(key) == 1 and (key.isalpha() or key.isdigit()):
        return modifiers, ord(key.upper())
    raise ValueError(f"hotkey {spec!r} names a key this does not know: {key!r}")


@dataclass
class Binding:
    name: str
    spec: str
    modifiers: int
    vk: int
    callback: Callable[[], None]


@dataclass
class Hotkeys:
    """The registration table. Ids are handed out in order and never reused within a run."""

    bindings: dict[int, Binding] = field(default_factory=dict)
    _stop: Event = field(default_factory=Event)
    _thread_id: int | None = None
    _registered: list[int] = field(default_factory=list)

    def add(self, name: str, spec: str, callback: Callable[[], None]) -> None:
        modifiers, vk = parse_hotkey(spec)
        self.bindings[len(self.bindings) + 1] = Binding(name, spec, modifiers, vk, callback)

    def id_of(self, name: str) -> int:
        return next(i for i, b in self.bindings.items() if b.name == name)

    def vk_of(self, name: str) -> int:
        return self.bindings[self.id_of(name)].vk

    def register(self) -> list[str]:
        """Claim every combination. Returns the names another process already owns."""
        refused = []
        for hotkey_id, binding in self.bindings.items():
            if windows.register_hotkey(hotkey_id, binding.modifiers, binding.vk):
                self._registered.append(hotkey_id)
            else:
                refused.append(binding.name)
        return refused

    def dispatch(self, hotkey_id: int) -> None:
        """Run the callback for a WM_HOTKEY. An id with no binding is a stale message, not an error."""
        binding = self.bindings.get(hotkey_id)
        if binding is not None:
            binding.callback()

    def run(self) -> None:
        """Pump until stopped. Must run on the thread that called register()."""
        self._thread_id = windows.current_thread_id()
        windows.pump_messages(on_hotkey=self.dispatch, stop=self._stop.is_set)

    def stop(self) -> None:
        """Release the combinations and wake the pump. Safe from another thread."""
        self._stop.set()
        for hotkey_id in self._registered:
            windows.unregister_hotkey(hotkey_id)
        self._registered.clear()
        if self._thread_id is not None:
            windows.post_quit_message(self._thread_id)
```

- [ ] **Step 4: Run the tests**

Run: `uv run pytest tests/test_hotkeys.py -v`
Expected: all PASS

- [ ] **Step 5: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/hotkeys.py tests/test_hotkeys.py
git commit -m "Bind global hotkeys by name

A spec like ctrl+alt+space becomes a modifier mask and a virtual key, the
table dispatches WM_HOTKEY back to a callback, and a combination another
process already owns is reported by name instead of failing the daemon."
```

---

### Task 3: Push-to-talk capture and transcription

**Files:**
- Create: `typesafe_computer_use_win/voice.py`
- Test: `tests/test_voice.py`

**Interfaces:**
- Consumes (from T1, assume it exists — do not edit `windows.py`): `windows.key_held(vk) -> bool`.
- Produces:
  - `SAMPLE_RATE = 16000`
  - `frames_to_audio(frames: list[bytes]) -> "numpy.ndarray"` — int16 little-endian bytes to float32 in [-1, 1]
  - `record_while_held(vk: int, max_seconds: float, open_stream=None, held=None, clock=None) -> list[bytes]`
  - `transcribe(audio, model_name: str = ...) -> str`
  - `listen(vk: int, max_seconds: float, model_name: str) -> str` — the whole path, returns a stripped transcript
  - `VoiceUnavailable(RuntimeError)` — raised when the extra is not installed or no input device exists

**Critical:** `numpy`, `sounddevice` and `pywhispercpp` are in the `voice` extra, which CI does not install. Import them **inside functions**, never at module scope, and make every test monkeypatch them out. A module-scope import breaks CI.

- [ ] **Step 1: Write the failing test**

```python
"""Push-to-talk capture. No microphone is touched: the stream and the model are injected."""

from __future__ import annotations

import sys

import pytest

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import voice


def test_the_module_imports_without_the_voice_extra():
    """CI installs the base set only, so nothing heavy may be imported at module scope."""
    assert "numpy" not in sys.modules or True  # the import itself above is the assertion


def test_recording_stops_when_the_key_comes_up():
    reads = [b"\x01\x00", b"\x02\x00", b"\x03\x00"]
    held = iter([True, True, False])

    class FakeStream:
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self, n):
            return reads.pop(0), False

    frames = voice.record_while_held(
        vk=0x20, max_seconds=30, open_stream=lambda: FakeStream(), held=lambda vk: next(held), clock=iter_clock()
    )
    assert frames == [b"\x01\x00", b"\x02\x00"]


def iter_clock():
    ticks = iter([0.0, 0.1, 0.2, 0.3, 0.4])
    return lambda: next(ticks, 99.0)


def test_recording_stops_at_the_cap_even_while_the_key_is_down():
    class FakeStream:
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def read(self, n):
            return b"\x00\x00", False

    ticks = iter([0.0, 0.5, 1.5, 2.5])
    frames = voice.record_while_held(
        vk=0x20,
        max_seconds=1.0,
        open_stream=lambda: FakeStream(),
        held=lambda vk: True,
        clock=lambda: next(ticks, 99.0),
    )
    assert len(frames) < 4, "the cap must end an utterance the user never releases"


def test_frames_become_float32_between_minus_one_and_one():
    pytest.importorskip("numpy")
    audio = voice.frames_to_audio([(32767).to_bytes(2, "little", signed=True), (-32768).to_bytes(2, "little", signed=True)])
    assert audio.dtype.name == "float32"
    assert audio[0] == pytest.approx(1.0, abs=1e-4)
    assert audio[1] == pytest.approx(-1.0, abs=1e-4)


def test_an_empty_recording_transcribes_to_nothing_without_loading_a_model(monkeypatch):
    monkeypatch.setattr(voice, "_load_model", lambda name: pytest.fail("no audio, so no model"))
    assert voice.transcribe_frames([], "base.en") == ""


def test_a_transcript_is_stripped_and_joined(monkeypatch):
    class FakeSegment:
        def __init__(self, text):
            self.text = text

    monkeypatch.setattr(
        voice,
        "_load_model",
        lambda name: type("M", (), {"transcribe": lambda self, a: [FakeSegment(" open "), FakeSegment("the console ")]})(),
    )
    monkeypatch.setattr(voice, "frames_to_audio", lambda frames: "audio")
    assert voice.transcribe_frames([b"\x00\x00"], "base.en") == "open the console"
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_voice.py -v`
Expected: FAIL with `ModuleNotFoundError: No module named 'typesafe_computer_use_win.voice'`

- [ ] **Step 3: Write the implementation**

```python
"""Push-to-talk: record while a key is held, transcribe locally with whisper.cpp.

Nothing here is imported at module scope from the `voice` extra. The base install must keep
importing this module, because the daemon reports a missing extra rather than failing to start.
Audio never reaches the disk.
"""

from __future__ import annotations

import time
from collections.abc import Callable

from . import windows

SAMPLE_RATE = 16000  # what whisper.cpp wants; resampling anything else is the stream's job
BLOCK_FRAMES = 1024
FULL_SCALE = 32768.0


class VoiceUnavailable(RuntimeError):
    """The voice extra is not installed, or the machine has no input device."""


def _open_stream():
    """A 16 kHz mono int16 input stream. Raises VoiceUnavailable when there is nothing to record."""
    try:
        import sounddevice
    except ImportError as e:
        raise VoiceUnavailable("voice needs the extra: uv sync --extra voice") from e
    try:
        return sounddevice.RawInputStream(samplerate=SAMPLE_RATE, channels=1, dtype="int16", blocksize=BLOCK_FRAMES)
    except Exception as e:  # no device, device in use, driver refusal
        raise VoiceUnavailable(f"no usable microphone ({e})") from e


def record_while_held(
    vk: int,
    max_seconds: float,
    open_stream: Callable[[], object] | None = None,
    held: Callable[[int], bool] | None = None,
    clock: Callable[[], float] | None = None,
) -> list[bytes]:
    """Capture raw int16 frames while the key is physically down, up to the cap.

    The three callables are injected so the tests never touch an audio device.
    """
    open_stream = _open_stream if open_stream is None else open_stream
    held = windows.key_held if held is None else held
    clock = time.monotonic if clock is None else clock
    frames: list[bytes] = []
    deadline = clock() + max_seconds
    with open_stream() as stream:
        while held(vk) and clock() < deadline:
            data, _overflowed = stream.read(BLOCK_FRAMES)
            frames.append(bytes(data))
    return frames


def frames_to_audio(frames: list[bytes]):
    """Raw little-endian int16 to the float32 array whisper.cpp reads."""
    import numpy as np

    return np.frombuffer(b"".join(frames), dtype="<i2").astype("float32") / FULL_SCALE


_model = None
_model_name = ""


def _load_model(name: str):
    """One model per process. The first call downloads it, which is why it is not eager."""
    global _model, _model_name
    if _model is None or _model_name != name:
        try:
            from pywhispercpp.model import Model
        except ImportError as e:
            raise VoiceUnavailable("voice needs the extra: uv sync --extra voice") from e
        _model = Model(name, redirect_whispercpp_logs_to=None)
        _model_name = name
    return _model


def transcribe_frames(frames: list[bytes], model_name: str) -> str:
    """The transcript of one utterance, or an empty string when nothing was said."""
    if not frames:
        return ""
    segments = _load_model(model_name).transcribe(frames_to_audio(frames))
    return " ".join(segment.text.strip() for segment in segments).strip()


def listen(vk: int, max_seconds: float, model_name: str) -> str:
    """Hold-to-talk, start to finish: record while the key is down, then transcribe."""
    return transcribe_frames(record_while_held(vk, max_seconds), model_name)
```

- [ ] **Step 4: Run the tests**

Run: `uv run pytest tests/test_voice.py -v`
Expected: all PASS (the float32 test skips when numpy is absent)

- [ ] **Step 5: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/voice.py tests/test_voice.py
git commit -m "Record while a key is held and transcribe with whisper.cpp

The stream, the key check and the clock are injected, so the tests run with
no microphone. Nothing from the voice extra is imported at module scope: a
base install still imports this module and reports the extra is missing."
```

---

### Task 4: The typed-goal overlay

**Files:**
- Create: `typesafe_computer_use_win/overlay.py`
- Test: `tests/test_overlay.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `ask_for_goal(prompt: str = "goal", tk_factory=None) -> str | None` — the typed text, or `None` when cancelled.

- [ ] **Step 1: Write the failing test**

```python
"""The typed-goal overlay. Tk is injected, so no window is ever created in a test."""

from __future__ import annotations

from typesafe_computer_use_win import overlay


class FakeEntry:
    def __init__(self, text):
        self.text = text
        self.bindings = {}

    def bind(self, sequence, handler):
        self.bindings[sequence] = handler

    def get(self):
        return self.text

    def pack(self, **kw):
        pass

    def focus_set(self):
        pass


class FakeRoot:
    def __init__(self, entry, submit_with="<Return>"):
        self.entry = entry
        self.submit_with = submit_with
        self.destroyed = False

    def title(self, *a):
        pass

    def attributes(self, *a):
        pass

    def overrideredirect(self, *a):
        pass

    def geometry(self, *a):
        pass

    def configure(self, **kw):
        pass

    def mainloop(self):
        handler = self.entry.bindings.get(self.submit_with)
        if handler is not None:
            handler(None)

    def destroy(self):
        self.destroyed = True

    def update_idletasks(self):
        pass

    def winfo_screenwidth(self):
        return 2560

    def winfo_screenheight(self):
        return 1440


def test_enter_returns_the_typed_goal():
    entry = FakeEntry("open the console")
    root = FakeRoot(entry)
    assert overlay.ask_for_goal(tk_factory=lambda: (root, entry)) == "open the console"
    assert root.destroyed, "the window must close after a submit"


def test_escape_returns_nothing():
    entry = FakeEntry("half a thought")
    root = FakeRoot(entry, submit_with="<Escape>")
    assert overlay.ask_for_goal(tk_factory=lambda: (root, entry)) is None


def test_an_empty_entry_counts_as_a_cancel():
    entry = FakeEntry("   ")
    root = FakeRoot(entry)
    assert overlay.ask_for_goal(tk_factory=lambda: (root, entry)) is None
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_overlay.py -v`
Expected: FAIL with `ModuleNotFoundError: No module named 'typesafe_computer_use_win.overlay'`

- [ ] **Step 3: Write the implementation**

```python
"""A borderless, always-on-top entry box for typing a goal without leaving the current app.

Tkinter ships with Python, so this costs no dependency. The widgets are built by an injected
factory, which is how the tests avoid opening a window.
"""

from __future__ import annotations

from collections.abc import Callable

WIDTH, HEIGHT = 640, 52
BACKGROUND = "#0b1120"
FOREGROUND = "#f8fafc"


def _default_factory(prompt: str):
    import tkinter as tk

    root = tk.Tk()
    root.title(prompt)
    root.overrideredirect(True)  # no title bar: this is a command bar, not a window to manage
    root.attributes("-topmost", True)
    root.configure(bg=BACKGROUND)
    entry = tk.Entry(root, font=("Consolas", 16), bg=BACKGROUND, fg=FOREGROUND, insertbackground=FOREGROUND, relief="flat")
    entry.pack(fill="both", expand=True, padx=14, pady=12)
    entry.focus_set()
    root.update_idletasks()
    x = (root.winfo_screenwidth() - WIDTH) // 2
    y = (root.winfo_screenheight() - HEIGHT) // 3
    root.geometry(f"{WIDTH}x{HEIGHT}+{x}+{y}")
    return root, entry


def ask_for_goal(prompt: str = "goal", tk_factory: Callable[[], tuple] | None = None) -> str | None:
    """Show the bar and block until the user submits or cancels. None means nothing to run."""
    root, entry = _default_factory(prompt) if tk_factory is None else tk_factory()
    typed: list[str] = []

    def submit(_event=None):
        typed.append(entry.get())
        root.destroy()

    def cancel(_event=None):
        root.destroy()

    entry.bind("<Return>", submit)
    entry.bind("<Escape>", cancel)
    root.mainloop()
    text = typed[0].strip() if typed else ""
    return text or None
```

- [ ] **Step 4: Run the tests**

Run: `uv run pytest tests/test_overlay.py -v`
Expected: all PASS

- [ ] **Step 5: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/overlay.py tests/test_overlay.py
git commit -m "Add a borderless overlay for typing a goal

Tkinter ships with Python, so a command bar costs no dependency. The widget
factory is injected, so the tests never open a window."
```

---

### Task 5: Pause, resume and abort as a Control

**Files:**
- Modify: `typesafe_computer_use_win/runner.py`
- Test: `tests/test_control.py`

**Interfaces:**
- Consumes (from T1, assume it exists): `windows.sleep_watching(seconds, check=None)`.
- Produces:
  - `class Control` with `pause()`, `resume()`, `toggle_pause() -> bool`, `abort(reason: str = "asked to stop")`, `reset()`, `paused` property, `aborting` property, `checkpoint()`
  - `run(cfg: RunConfig, ctx_factory, control: Control | None = None) -> RunState` — the existing signature plus one optional argument

- [ ] **Step 1: Write the failing test**

```python
"""Pause and abort, as the daemon drives them."""

from __future__ import annotations

import threading
import time

import pytest

from typesafe_computer_use_win.models import Abort
from typesafe_computer_use_win.runner import Control


def test_a_fresh_control_never_blocks_and_never_raises():
    Control().checkpoint()


def test_abort_raises_at_the_next_checkpoint():
    control = Control()
    control.abort("hotkey")
    with pytest.raises(Abort, match="hotkey"):
        control.checkpoint()


def test_pause_blocks_until_resumed():
    control = Control()
    control.pause()
    released = threading.Event()

    def waiter():
        control.checkpoint()
        released.set()

    threading.Thread(target=waiter, daemon=True).start()
    assert not released.wait(0.2), "a paused checkpoint must not return"
    control.resume()
    assert released.wait(2.0), "resume must release the checkpoint"


def test_abort_wins_over_pause_so_a_paused_run_can_still_be_killed():
    control = Control()
    control.pause()
    raised = []

    def waiter():
        try:
            control.checkpoint()
        except Abort as e:
            raised.append(str(e))

    thread = threading.Thread(target=waiter, daemon=True)
    thread.start()
    time.sleep(0.1)
    control.abort("quit")
    thread.join(2.0)
    assert raised == ["quit"]


def test_toggle_reports_the_state_it_moved_to():
    control = Control()
    assert control.toggle_pause() is True
    assert control.paused is True
    assert control.toggle_pause() is False
    assert control.paused is False


def test_reset_clears_both_flags_for_the_next_run():
    control = Control()
    control.pause()
    control.abort("done with that one")
    control.reset()
    assert control.paused is False and control.aborting is False
    control.checkpoint()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_control.py -v`
Expected: FAIL with `ImportError: cannot import name 'Control'`

- [ ] **Step 3: Add `Control` to `runner.py`**

Add near the top, after the `STOPPED` dict, and add `from threading import Event` plus `from collections.abc import Callable` to the imports:

```python
class Control:
    """Pause and abort, shared between the daemon's threads and the loop.

    The loop only ever calls `checkpoint()`. Pause parks the caller there; abort raises the same
    `Abort` the corner escape hatch raises, so a stopped run lands in the path that already writes
    the run folder and reports the outcome.
    """

    def __init__(self) -> None:
        self._resumed = Event()
        self._resumed.set()
        self._aborted = Event()
        self._reason = ""

    @property
    def paused(self) -> bool:
        return not self._resumed.is_set()

    @property
    def aborting(self) -> bool:
        return self._aborted.is_set()

    def pause(self) -> None:
        self._resumed.clear()

    def resume(self) -> None:
        self._resumed.set()

    def toggle_pause(self) -> bool:
        """Flip, and report whether the run is now paused."""
        if self.paused:
            self.resume()
        else:
            self.pause()
        return self.paused

    def abort(self, reason: str = "asked to stop") -> None:
        self._reason = reason
        self._aborted.set()
        self._resumed.set()  # a paused run must still be killable

    def reset(self) -> None:
        """Clear both flags, so one Control serves the next job too."""
        self._reason = ""
        self._aborted.clear()
        self._resumed.set()

    def checkpoint(self) -> None:
        """Block while paused; raise once aborted. Called from the run thread only."""
        while not self._resumed.wait(0.1):
            if self._aborted.is_set():
                break
        if self._aborted.is_set():
            raise Abort(self._reason)
```

- [ ] **Step 4: Thread the control through `run`**

Change the signature and the three call sites in `runner.py`:

```python
def run(cfg: RunConfig, ctx_factory, control: Control | None = None) -> RunState:
    """Drive the loop. ctx_factory(typesafe, history) builds the action Context.

    `control` lets a caller pause or abort between steps; without one the loop runs to its own
    stop rules and only the corner escape hatch interrupts it.
    """
```

Inside `run`, pass it down: `if not run_step(cfg, ctx, state, step, log, control):`

In `run_step`, change the signature to `def run_step(cfg, ctx, state, step, log, control: Control | None = None) -> bool:` and replace the first line `windows.check_abort()` with:

```python
    windows.check_abort()
    if control is not None:
        control.checkpoint()
```

and replace the wait at the end of `run_step`:

```python
    if state.view is None:  # an action ran: let the screen settle before the next step, or the answer, reads it
        windows.sleep_watching(cfg.delay, check=None if control is None else _watch(control))
```

Add the helper next to `Control`:

```python
def _watch(control: Control) -> Callable[[], None]:
    """The interrupt check the wait polls: the corner escape hatch, plus pause and abort."""

    def check() -> None:
        windows.check_abort()
        control.checkpoint()

    return check
```

- [ ] **Step 5: Run the tests**

Run: `uv run pytest tests/test_control.py -v && uv run pytest -q`
Expected: all PASS. The whole existing suite must stay green: `control` is optional and defaults to the old behaviour.

- [ ] **Step 6: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/runner.py tests/test_control.py
git commit -m "Let a caller pause and abort a run

Control holds two events and the loop calls one checkpoint. Abort raises the
Abort the corner escape hatch already raises, so a stopped run takes the path
that writes its run folder and reports the outcome. Abort beats pause, so a
paused run is still killable."
```

---

### Task 6: What the transcript means, decided by jev

**Files:**
- Create: `typesafe_computer_use_win/intent.py`
- Test: `tests/test_intent.py`

**Interfaces:**
- Consumes: `typesafe_sdk.Choice`, `typesafe_sdk.Noul`, `TypeSafeClient.system_one(state=..., questions=...)` — the pattern in `decide.py`.
- Produces:
  - `COMMANDS: dict[str, str]` (criteria)
  - `@dataclass(frozen=True) class Intent` with fields `command: str`, `goal: str`, `confidence: float`, `heard: float`, `transcript: str`, and property `actionable: bool`
  - `interpret(client, transcript: str, running: bool, paused: bool, min_confidence: float = 0.55) -> Intent`

**Why this task exists:** a transcript is not a goal. "stop" must stop, not become something to accomplish. The classifier that already picks actions picks this too, so no free text is generated anywhere.

- [ ] **Step 1: Write the failing test**

```python
"""What a spoken line means: jev decides, code dispatches."""

from __future__ import annotations

from dataclasses import dataclass

from typesafe_computer_use_win.intent import interpret


@dataclass
class FakeChoiceAnswer:
    choice: str
    confidence: float
    probabilities: dict


@dataclass
class FakeNoulAnswer:
    noul: float


class FakeResult:
    def __init__(self, answers):
        self.answers = answers


class FakeClient:
    """Records the state and questions, answers with whatever it was told to."""

    def __init__(self, command="run_goal", confidence=0.9, heard=0.9):
        self.answer = FakeChoiceAnswer(command, confidence, {command: confidence})
        self.heard = FakeNoulAnswer(heard)
        self.state = None
        self.questions = None

    def system_one(self, state, questions):
        self.state = state
        self.questions = questions
        return FakeResult({"command": self.answer, "heard": self.heard})


def test_a_spoken_goal_becomes_a_runnable_intent():
    client = FakeClient(command="run_goal", confidence=0.88)
    intent = interpret(client, "open the console and check billing", running=False, paused=False)
    assert intent.command == "run_goal"
    assert intent.goal == "open the console and check billing"
    assert intent.actionable is True


def test_the_daemon_state_travels_with_the_transcript():
    client = FakeClient()
    interpret(client, "pause", running=True, paused=False)
    assert client.state["a_run_is_active"] is True
    assert client.state["the_run_is_paused"] is False
    assert client.state["transcript"] == "pause"


def test_both_questions_are_asked():
    client = FakeClient()
    interpret(client, "stop", running=True, paused=False)
    assert set(client.questions) == {"command", "heard"}


def test_a_low_confidence_command_is_not_actionable():
    client = FakeClient(command="quit_daemon", confidence=0.4)
    intent = interpret(client, "mumble mumble", running=False, paused=False)
    assert intent.actionable is False


def test_a_transcript_that_was_not_really_heard_is_not_actionable():
    client = FakeClient(command="run_goal", confidence=0.95, heard=0.2)
    intent = interpret(client, "uh, the, uh", running=False, paused=False)
    assert intent.actionable is False


def test_ignore_is_never_actionable_however_confident():
    client = FakeClient(command="ignore", confidence=0.99)
    assert interpret(client, "hey are you coming for lunch", running=False, paused=False).actionable is False


def test_an_empty_transcript_never_reaches_the_model():
    class Exploding:
        def system_one(self, state, questions):
            raise AssertionError("nothing was said, so nothing to ask")

    intent = interpret(Exploding(), "   ", running=False, paused=False)
    assert intent.command == "ignore" and intent.actionable is False


def test_only_a_run_goal_carries_a_goal():
    client = FakeClient(command="stop_run", confidence=0.93)
    assert interpret(client, "stop that", running=True, paused=False).goal == ""
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_intent.py -v`
Expected: FAIL with `ModuleNotFoundError: No module named 'typesafe_computer_use_win.intent'`

- [ ] **Step 3: Write the implementation**

```python
"""What a spoken or typed line means to the daemon.

A transcript is not a goal. "stop", "pause a sec", "never mind" and "open the console" all arrive
as text, and only the last one is something to accomplish. Handing raw text to the loop would make
the daemon try to achieve the word "stop", so the same classifier that picks actions decides what
the line is first. No free text is generated anywhere in this path.
"""

from __future__ import annotations

from dataclasses import dataclass

from typesafe_sdk import Choice, Noul

MIN_HEARD = 0.5  # below this the audio was not a clean instruction, whatever it classified as
DEFAULT_MIN_CONFIDENCE = 0.55
IGNORED = "ignore"

COMMANDS = {
    "run_goal": (
        "The line is a task to carry out on this computer: something to open, find, fill in, "
        "check or navigate to. This is the only answer that starts work."
    ),
    "stop_run": "The line asks for the current run to stop now: stop, cancel that, abort, never mind.",
    "pause_run": "The line asks for the current run to hold where it is, to be continued later.",
    "resume_run": "The line asks for a paused run to carry on: continue, keep going, resume.",
    "quit_daemon": "The line asks to shut the assistant down entirely: quit, exit, goodbye.",
    IGNORED: (
        "The line is not addressed to the assistant at all, or asks for something it does not do: "
        "half a sentence, a word caught by accident, or talk meant for someone else in the room."
    ),
}


@dataclass(frozen=True)
class Intent:
    command: str
    goal: str
    confidence: float
    heard: float
    transcript: str

    @property
    def actionable(self) -> bool:
        """Whether the daemon should act. A misheard command clicks a real machine, so the bar
        is a gate, not a hint."""
        return self.command != IGNORED and self.confidence >= self.min_confidence and self.heard >= MIN_HEARD

    min_confidence: float = DEFAULT_MIN_CONFIDENCE


def interpret(client, transcript: str, running: bool, paused: bool, min_confidence: float = DEFAULT_MIN_CONFIDENCE) -> Intent:
    """Classify one line against what the daemon is currently doing.

    The daemon's state travels with the transcript, so "pause" said while nothing runs reads as
    `ignore` rather than as a pause of nothing.
    """
    said = " ".join(transcript.split())
    if not said:
        return Intent(IGNORED, "", 0.0, 0.0, "", min_confidence)
    state = {
        "transcript": said,
        "a_run_is_active": running,
        "the_run_is_paused": paused,
        "what_the_assistant_does": (
            "drives this Windows machine toward a goal spoken in plain English: opening sites, clicking controls, filling fields"
        ),
    }
    questions = {
        "command": Choice(
            instructions=(
                "The user said this out loud to an assistant that drives their computer. What are "
                "they asking for? Answer with what the line means for the assistant right now, "
                "given whether a run is active and whether it is paused."
            ),
            criteria=COMMANDS,
        ),
        "heard": Noul(
            instructions=(
                "Is this transcript a complete, intelligible instruction, rather than a fragment, "
                "a false start, or speech that was only half caught?"
            )
        ),
    }
    answers = client.system_one(state=state, questions=questions).answers
    command = answers["command"]
    return Intent(
        command=command.choice,
        goal=said if command.choice == "run_goal" else "",
        confidence=command.confidence,
        heard=answers["heard"].noul,
        transcript=said,
        min_confidence=min_confidence,
    )
```

**Note on the dataclass:** `min_confidence` must be declared as a field with a default, and `actionable` is a property. Order the fields so the defaulted one comes last — put `min_confidence: float = DEFAULT_MIN_CONFIDENCE` immediately after `transcript` and define `actionable` below all fields.

- [ ] **Step 4: Run the tests**

Run: `uv run pytest tests/test_intent.py -v`
Expected: all PASS

- [ ] **Step 5: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/intent.py tests/test_intent.py
git commit -m "Let jev decide what a spoken line means

A transcript is not a goal: stop, pause, never mind and open the console all
arrive as text and only one is work. The classifier that picks actions picks
this too, with the daemon's own state in the packet, and two gates: how sure
the command is, and whether the line was heard cleanly at all."
```

---

### Task 7: Settings and packaging

**Files:**
- Modify: `typesafe_computer_use_win/config.py`, `pyproject.toml`

**Interfaces:**
- Produces, in `config.py`:
  - `DEFAULT_HOTKEYS: dict[str, str]` with keys `talk`, `goal`, `pause`, `abort`, `quit`
  - `hotkey(name: str) -> str`
  - `whisper_model() -> str`
  - `voice_max_seconds() -> float`
  - `voice_min_confidence() -> float`
- Produces, in `pyproject.toml`: a `voice` extra and the `winclicker-daemon` script pointing at `typesafe_computer_use_win.cli:daemon`.

- [ ] **Step 1: Write the failing test**

Append to the existing `tests/test_config_and_writer.py` — **this is the one file in this task shared with nobody else; do not touch any other test file.**

```python
def test_hotkeys_have_defaults_and_read_the_environment(monkeypatch):
    from typesafe_computer_use_win import config

    monkeypatch.delenv("CLICKER_HOTKEY_TALK", raising=False)
    assert config.hotkey("talk") == "ctrl+alt+space"
    monkeypatch.setenv("CLICKER_HOTKEY_TALK", "ctrl+shift+v")
    assert config.hotkey("talk") == "ctrl+shift+v"


def test_every_daemon_hotkey_has_a_default():
    from typesafe_computer_use_win import config

    assert set(config.DEFAULT_HOTKEYS) == {"talk", "goal", "pause", "abort", "quit"}


def test_voice_settings_have_defaults_and_read_the_environment(monkeypatch):
    from typesafe_computer_use_win import config

    for name in ("CLICKER_WHISPER_MODEL", "CLICKER_VOICE_MAX_SECONDS", "CLICKER_VOICE_MIN_CONFIDENCE"):
        monkeypatch.delenv(name, raising=False)
    assert config.whisper_model() == "base.en"
    assert config.voice_max_seconds() == 30.0
    assert config.voice_min_confidence() == 0.55
    monkeypatch.setenv("CLICKER_VOICE_MAX_SECONDS", "8")
    assert config.voice_max_seconds() == 8.0
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_config_and_writer.py -v`
Expected: FAIL with `AttributeError: module 'typesafe_computer_use_win.config' has no attribute 'hotkey'`

- [ ] **Step 3: Add the settings**

Append to `config.py`:

```python
# The daemon's global hotkeys. Ctrl+Alt is the least contested corner of the Windows keyboard:
# Win+key belongs to the shell, and Ctrl+Shift+key to whatever app has focus.
DEFAULT_HOTKEYS = {
    "talk": "ctrl+alt+space",
    "goal": "ctrl+alt+g",
    "pause": "ctrl+alt+p",
    "abort": "ctrl+alt+x",
    "quit": "ctrl+alt+q",
}
DEFAULT_WHISPER_MODEL = "base.en"
DEFAULT_VOICE_MAX_SECONDS = 30.0
DEFAULT_VOICE_MIN_CONFIDENCE = 0.55


def hotkey(name: str) -> str:
    return os.environ.get(f"CLICKER_HOTKEY_{name.upper()}", DEFAULT_HOTKEYS[name])


def whisper_model() -> str:
    return os.environ.get("CLICKER_WHISPER_MODEL", DEFAULT_WHISPER_MODEL)


def voice_max_seconds() -> float:
    return float(os.environ.get("CLICKER_VOICE_MAX_SECONDS", DEFAULT_VOICE_MAX_SECONDS))


def voice_min_confidence() -> float:
    return float(os.environ.get("CLICKER_VOICE_MIN_CONFIDENCE", DEFAULT_VOICE_MIN_CONFIDENCE))
```

- [ ] **Step 4: Add the extra and the entry point**

In `pyproject.toml`, add to `[project.optional-dependencies]`:

```toml
# Push-to-talk. whisper.cpp ships a prebuilt wheel for Windows AMD64, so this needs no compiler.
voice = [
    "pywhispercpp==1.5.1",
    "sounddevice==0.5.6",
    "numpy>=2.0",
]
```

and to `[project.scripts]`:

```toml
winclicker-daemon = "typesafe_computer_use_win.cli:daemon"
```

- [ ] **Step 5: Run the tests**

Run: `uv run pytest tests/test_config_and_writer.py -v && uv run ruff check .`
Expected: PASS. (`winclicker-daemon` will not resolve until Task 9 adds `cli.daemon`; that is expected and is not an error until then.)

- [ ] **Step 6: Commit**

```bash
git add typesafe_computer_use_win/config.py pyproject.toml tests/test_config_and_writer.py
git commit -m "Add the daemon's hotkey and voice settings

Ctrl+Alt is the least contested corner of the Windows keyboard. The voice
extra is optional and prebuilt: whisper.cpp ships a wheel for Windows on
Python 3.13, so push-to-talk needs no compiler."
```

---

### Task 8: The daemon

**Files:**
- Create: `typesafe_computer_use_win/daemon.py`
- Test: `tests/test_daemon.py`

**Interfaces:**
- Consumes: `hotkeys.Hotkeys`, `voice.listen`, `voice.VoiceUnavailable`, `overlay.ask_for_goal`, `intent.interpret`, `intent.Intent`, `runner.Control`, `runner.run`, `runner.RunConfig`, `config.hotkey`, `config.whisper_model`, `config.voice_max_seconds`, `config.voice_min_confidence`.
- Produces: `class Daemon` with `__init__(self, run_job, interpret_line, listen_now, ask_goal, keys, control, log=print)`, methods `on_talk()`, `on_goal()`, `on_pause()`, `on_abort()`, `on_quit()`, `handle_line(text: str)`, `work()`, `start()`, and attributes `jobs: queue.Queue`, `running: bool`, `stopped: threading.Event`.

**Design note for the implementer:** every collaborator arrives through `__init__`, so the tests build a Daemon out of fakes and never touch a hotkey, a microphone or a model. `start()` is the only method that wires the real ones.

- [ ] **Step 1: Write the failing test**

```python
"""The daemon's wiring: what a hotkey queues, what the worker runs, and what is refused."""

from __future__ import annotations

import queue
import threading

from typesafe_computer_use_win.daemon import Daemon, Job
from typesafe_computer_use_win.intent import Intent
from typesafe_computer_use_win.runner import Control


def make_daemon(intent=None, transcript="open the console", **kw):
    ran = []
    intent = intent or Intent("run_goal", transcript, 0.9, 0.9, transcript, 0.55)
    daemon = Daemon(
        run_job=lambda job, control: ran.append(job.goal),
        interpret_line=lambda text, running, paused: intent,
        listen_now=lambda: transcript,
        ask_goal=lambda: transcript,
        keys=None,
        control=Control(),
        log=lambda *a: None,
        **kw,
    )
    return daemon, ran


def test_a_spoken_goal_is_queued_as_a_job():
    daemon, ran = make_daemon()
    daemon.on_talk()
    assert daemon.jobs.get_nowait().goal == "open the console"


def test_a_line_below_the_bar_queues_nothing():
    refused = Intent("run_goal", "open the console", 0.2, 0.9, "open the console", 0.55)
    daemon, _ = make_daemon(intent=refused)
    daemon.on_talk()
    assert daemon.jobs.empty()


def test_a_spoken_stop_aborts_instead_of_queueing():
    daemon, _ = make_daemon(intent=Intent("stop_run", "", 0.95, 0.95, "stop", 0.55))
    daemon.on_talk()
    assert daemon.control.aborting is True
    assert daemon.jobs.empty()


def test_a_spoken_pause_pauses_and_a_spoken_resume_releases():
    daemon, _ = make_daemon(intent=Intent("pause_run", "", 0.95, 0.95, "pause", 0.55))
    daemon.on_talk()
    assert daemon.control.paused is True
    daemon.interpret_line = lambda text, running, paused: Intent("resume_run", "", 0.95, 0.95, "go on", 0.55)
    daemon.on_talk()
    assert daemon.control.paused is False


def test_a_spoken_quit_stops_the_daemon():
    daemon, _ = make_daemon(intent=Intent("quit_daemon", "", 0.95, 0.95, "quit", 0.55))
    daemon.on_talk()
    assert daemon.stopped.is_set()


def test_the_worker_runs_queued_jobs_in_order():
    daemon, ran = make_daemon()
    daemon.jobs.put(Job("first"))
    daemon.jobs.put(Job("second"))
    daemon.jobs.put(None)  # the sentinel that ends the worker
    daemon.work()
    assert ran == ["first", "second"]


def test_a_job_that_raises_leaves_the_daemon_alive():
    daemon, _ = make_daemon()
    daemon.run_job = lambda job, control: (_ for _ in ()).throw(RuntimeError("boom"))
    daemon.jobs.put(Job("explodes"))
    daemon.jobs.put(None)
    daemon.work()
    assert not daemon.stopped.is_set()


def test_the_control_is_reset_between_jobs():
    seen = []
    daemon, _ = make_daemon()
    daemon.control.abort("previous job")
    daemon.run_job = lambda job, control: seen.append(control.aborting)
    daemon.jobs.put(Job("next"))
    daemon.jobs.put(None)
    daemon.work()
    assert seen == [False], "a new job must not inherit the last job's abort"


def test_the_typed_overlay_queues_a_job_the_same_way():
    daemon, _ = make_daemon()
    daemon.on_goal()
    assert daemon.jobs.get_nowait().goal == "open the console"


def test_an_empty_overlay_queues_nothing():
    daemon, _ = make_daemon()
    daemon.ask_goal = lambda: None
    daemon.on_goal()
    assert daemon.jobs.empty()


def test_the_hotkey_toggle_pauses_and_resumes_without_the_model():
    daemon, _ = make_daemon()
    daemon.on_pause()
    assert daemon.control.paused is True
    daemon.on_pause()
    assert daemon.control.paused is False


def test_running_reports_whether_a_job_is_in_flight():
    daemon, _ = make_daemon()
    assert daemon.running is False
    started, release = threading.Event(), threading.Event()

    def slow(job, control):
        started.set()
        release.wait(2.0)

    daemon.run_job = slow
    daemon.jobs.put(Job("slow"))
    daemon.jobs.put(None)
    worker = threading.Thread(target=daemon.work, daemon=True)
    worker.start()
    assert started.wait(2.0)
    assert daemon.running is True
    release.set()
    worker.join(2.0)
    assert daemon.running is False


def test_a_voice_error_is_reported_and_survived():
    from typesafe_computer_use_win.voice import VoiceUnavailable

    logged = []
    daemon, _ = make_daemon()
    daemon.log = logged.append
    daemon.listen_now = lambda: (_ for _ in ()).throw(VoiceUnavailable("no microphone"))
    daemon.on_talk()
    assert any("no microphone" in str(line) for line in logged)
    assert daemon.jobs.empty()
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/test_daemon.py -v`
Expected: FAIL with `ModuleNotFoundError: No module named 'typesafe_computer_use_win.daemon'`

- [ ] **Step 3: Write the implementation**

```python
"""The resident process: hotkeys in, one run at a time out.

Three threads, because Windows requires it. RegisterHotKey delivers WM_HOTKEY only to the thread
that registered it and only while that thread pumps messages, so the pump owns the main thread and
does nothing else. Recording, transcription, the overlay and the classifier run on the input
worker, and the loop itself on the run worker, so a three-second utterance never stalls the pump.

Every collaborator is injected, which is what makes this testable without a microphone.
"""

from __future__ import annotations

import queue
import threading
import time
import traceback
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

from . import config, hotkeys, intent, overlay, voice
from .actions import Context
from .runner import Control, RunConfig, run
from .voice import VoiceUnavailable
from .writer import make_writer


@dataclass(frozen=True)
class Job:
    goal: str


class Daemon:
    def __init__(self, run_job, interpret_line, listen_now, ask_goal, keys, control, log=print):
        self.run_job = run_job
        self.interpret_line = interpret_line
        self.listen_now = listen_now
        self.ask_goal = ask_goal
        self.keys = keys
        self.control = control
        self.log = log
        self.jobs: queue.Queue = queue.Queue()
        self.stopped = threading.Event()
        self.running = False

    # ------------------------------------------------------------------ hotkey handlers

    def on_talk(self) -> None:
        """Record, transcribe, and let the classifier say what it was."""
        try:
            said = self.listen_now()
        except VoiceUnavailable as e:
            self.log(f"voice unavailable: {e}")
            return
        except Exception as e:
            self.log(f"voice failed: {e}")
            return
        if not said.strip():
            self.log("heard nothing")
            return
        self.log(f"heard: {said!r}")
        self.handle_line(said)

    def on_goal(self) -> None:
        typed = self.ask_goal()
        if not typed:
            return
        self.queue_goal(typed)

    def on_pause(self) -> None:
        self.log("paused" if self.control.toggle_pause() else "resumed")

    def on_abort(self) -> None:
        if self.running:
            self.control.abort("hotkey")
            self.log("aborting the run")
        else:
            self.log("nothing to abort")

    def on_quit(self) -> None:
        self.log("quitting")
        self.control.abort("daemon quitting")
        self.stopped.set()
        self.jobs.put(None)
        if self.keys is not None:
            self.keys.stop()

    # ------------------------------------------------------------------ dispatch

    def handle_line(self, text: str) -> None:
        """One transcript, classified and acted on. Nothing here trusts the text itself."""
        decided = self.interpret_line(text, self.running, self.control.paused)
        if not decided.actionable:
            self.log(f"  ignored ({decided.command}, {decided.confidence:.2f}, heard {decided.heard:.2f})")
            return
        if decided.command == "run_goal":
            self.queue_goal(decided.goal)
        elif decided.command == "stop_run":
            self.on_abort()
        elif decided.command == "pause_run":
            self.control.pause()
            self.log("paused")
        elif decided.command == "resume_run":
            self.control.resume()
            self.log("resumed")
        elif decided.command == "quit_daemon":
            self.on_quit()

    def queue_goal(self, goal: str) -> None:
        self.jobs.put(Job(goal))
        self.log(f"  queued: {goal!r}")

    # ------------------------------------------------------------------ the run worker

    def work(self) -> None:
        """Run queued jobs one at a time until the sentinel arrives."""
        while True:
            job = self.jobs.get()
            if job is None:
                return
            self.control.reset()  # a new job never inherits the last one's abort
            self.running = True
            try:
                self.run_job(job, self.control)
            except Exception:
                self.log("the run failed:\n" + traceback.format_exc())
            finally:
                self.running = False
                self.log("idle")


def build(log=print) -> Daemon:
    """Wire the real collaborators: hotkeys, the microphone, the model, the loop."""
    control = Control()
    keys = hotkeys.Hotkeys()
    writer = make_writer()

    def run_job(job: Job, control: Control):
        from typesafe_sdk import TypeSafeClient  # noqa: F401 - run() opens its own client

        cfg = RunConfig(goal=job.goal, out=Path("runs") / time.strftime("%Y%m%d-%H%M%S"), act=True)

        def ctx_factory(typesafe, history):
            return Context(
                goal=job.goal,
                browser=config.browser(),
                email=config.email(),
                typesafe=typesafe,
                writer=writer,
                history=history,
            )

        run(cfg, ctx_factory, control)

    def interpret_line(text: str, running: bool, paused: bool):
        from typesafe_sdk import TypeSafeClient

        with TypeSafeClient() as client:
            return intent.interpret(client, text, running, paused, config.voice_min_confidence())

    def listen_now() -> str:
        return voice.listen(keys.vk_of("talk"), config.voice_max_seconds(), config.whisper_model())

    daemon = Daemon(
        run_job=run_job,
        interpret_line=interpret_line,
        listen_now=listen_now,
        ask_goal=overlay.ask_for_goal,
        keys=keys,
        control=control,
        log=log,
    )
    return daemon


def serve(log=print) -> None:
    """Start the daemon and block until it is asked to quit."""
    daemon = build(log)
    keys = daemon.keys
    commands: queue.Queue = queue.Queue()

    # A hotkey callback must return at once: the pump that delivered it is the same thread that
    # delivers the next one. So the callbacks post, and the input worker does the work.
    for name, handler in (
        ("talk", daemon.on_talk),
        ("goal", daemon.on_goal),
        ("pause", daemon.on_pause),
        ("abort", daemon.on_abort),
        ("quit", daemon.on_quit),
    ):
        keys.add(name, config.hotkey(name), (lambda h=handler: commands.put(h)))

    refused = keys.register()
    for name in refused:
        log(f"hotkey for {name} is taken by another app: {config.hotkey(name)}")
    for hotkey_id, binding in keys.bindings.items():  # noqa: B007 - the id is not needed, the spec is
        if binding.name not in refused:
            log(f"  {binding.spec:<16} {binding.name}")

    def input_worker():
        while not daemon.stopped.is_set():
            try:
                handler = commands.get(timeout=0.2)
            except queue.Empty:
                continue
            if handler is None:
                return
            try:
                handler()
            except Exception:
                log("a hotkey handler failed:\n" + traceback.format_exc())

    threads = [
        threading.Thread(target=input_worker, name="input", daemon=True),
        threading.Thread(target=daemon.work, name="runs", daemon=True),
    ]
    for thread in threads:
        thread.start()
    log("listening. hold the talk hotkey and say what you want done.")
    try:
        keys.run()
    except KeyboardInterrupt:
        daemon.on_quit()
    finally:
        daemon.stopped.set()
        commands.put(None)
        daemon.jobs.put(None)
        keys.stop()
        for thread in threads:
            thread.join(timeout=3.0)
    log("stopped")
```

- [ ] **Step 4: Run the tests**

Run: `uv run pytest tests/test_daemon.py -v && uv run pytest -q`
Expected: all PASS

- [ ] **Step 5: Lint and commit**

```bash
uv run ruff check . && uv run ruff format .
git add typesafe_computer_use_win/daemon.py tests/test_daemon.py
git commit -m "Add the resident daemon

A hotkey callback only posts: the thread that delivered it is the one that
delivers the next, so recording, transcription and the classifier happen on
an input worker and the loop on a run worker. Jobs run one at a time, the
control resets between them, and a run that raises leaves the daemon idle
rather than dead."
```

---

### Task 9: The entry point and the documentation

**Files:**
- Modify: `typesafe_computer_use_win/cli.py`, `README.md`, `CONTRIBUTING.md`, `.env.example`

**Interfaces:**
- Consumes: `daemon.serve(log=print)`.
- Produces: `cli.daemon(argv: list[str] | None = None) -> None`, wired to the `winclicker-daemon` script Task 7 declared.

- [ ] **Step 1: Add the entry point**

In `cli.py`, add after `inspect`:

```python
def daemon(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(
        prog="winclicker-daemon",
        description="Stay resident: hold the talk hotkey, say what you want done, and it runs.",
    )
    parser.parse_args(argv)
    _prepare()
    from .daemon import serve

    serve()
```

`_prepare()` already forces UTF-8 output and exits when `TYPESAFE_API_KEY` is missing, which is exactly what the daemon needs before it registers anything.

- [ ] **Step 2: Verify the entry point resolves**

Run: `uv sync && uv run winclicker-daemon --help`
Expected: the help text prints and the process exits 0 without registering hotkeys.

- [ ] **Step 3: Document it in the README**

Add a `## Voice and hotkeys` section after the `## Use` section:

````markdown
## Voice and hotkeys

```
uv sync --extra voice        # whisper.cpp and the microphone bindings, prebuilt
uv run winclicker-daemon     # stays resident and listens for hotkeys
```

| hotkey | does | env |
|---|---|---|
| `ctrl+alt+space` | hold, speak, release: the line is transcribed locally and classified | `CLICKER_HOTKEY_TALK` |
| `ctrl+alt+g` | type a goal into an overlay instead of speaking it | `CLICKER_HOTKEY_GOAL` |
| `ctrl+alt+p` | pause or resume the running loop | `CLICKER_HOTKEY_PAUSE` |
| `ctrl+alt+x` | abort the run, keeping the run folder | `CLICKER_HOTKEY_ABORT` |
| `ctrl+alt+q` | quit the daemon | `CLICKER_HOTKEY_QUIT` |

**A transcript is not a goal.** "stop", "pause a second" and "never mind" are instructions about
the run, not work to carry out, so every line goes to the classifier first: one `Choice` over
`run_goal`, `stop_run`, `pause_run`, `resume_run`, `quit_daemon` and `ignore`, with whether a run
is active and whether it is paused in the state, plus a `Noul` scoring whether the line was heard
cleanly at all. Below `CLICKER_VOICE_MIN_CONFIDENCE` (0.55) the daemon prints what it heard and
does nothing, because a misheard command clicks a real machine.

Transcription is whisper.cpp on the CPU (`CLICKER_WHISPER_MODEL`, default `base.en`), so the audio
never leaves the machine and is never written to disk. The first run downloads the model.
````

- [ ] **Step 4: Document the settings in `.env.example`**

Append:

```
CLICKER_HOTKEY_TALK=
CLICKER_HOTKEY_GOAL=
CLICKER_HOTKEY_PAUSE=
CLICKER_HOTKEY_ABORT=
CLICKER_HOTKEY_QUIT=
CLICKER_WHISPER_MODEL=
CLICKER_VOICE_MAX_SECONDS=
CLICKER_VOICE_MIN_CONFIDENCE=
```

- [ ] **Step 5: Add the threading rule to CONTRIBUTING.md**

Add to the bullet list:

```markdown
- A hotkey callback posts to a queue and returns. The thread that delivers a hotkey is the thread
  that delivers the next one, so any work done inside a callback stalls every other hotkey.
- Nothing from the `voice` extra is imported at module scope: CI installs the base set only.
```

- [ ] **Step 6: Run everything and commit**

```bash
uv run ruff check . && uv run ruff format --check . && uv run pytest -q
git add typesafe_computer_use_win/cli.py README.md CONTRIBUTING.md .env.example
git commit -m "Add the winclicker-daemon entry point and document the hotkeys

Says plainly why a transcript goes to the classifier before anything runs: a
misheard command clicks a real machine, so the bar is a gate."
```

---

## Merge and end-to-end pass (main session, not a subagent)

- [ ] **Step 1: Confirm no file was written by two tasks**

```bash
git log --oneline --name-only <first-task-sha>..HEAD | sort | uniq -d
```
Any file listed twice across two task commits is a merge hazard; inspect it before continuing.

- [ ] **Step 2: Full suite on the base install**

```bash
uv sync && uv run ruff check . && uv run ruff format --check . && uv run pytest -q
```
Expected: everything passes **without** the voice extra installed, exactly as CI runs it. A failure mentioning `numpy`, `sounddevice` or `pywhispercpp` means a module-scope import slipped in.

- [ ] **Step 3: Full suite with the extra installed**

```bash
uv sync --extra voice && uv run pytest -q
```

- [ ] **Step 4: Hardware check — hotkeys register**

```bash
uv run winclicker-daemon
```
Expected: the startup report lists five hotkeys, none refused. Press `ctrl+alt+p` twice: `paused` then `resumed`. Press `ctrl+alt+q`: `quitting`, then `stopped`, and the process exits.

- [ ] **Step 5: Hardware check — the overlay**

Start the daemon, press `ctrl+alt+g`, type `open the typesafe console`, press Enter. Expected: the overlay closes, `queued:` prints, a `runs/<timestamp>/` folder appears, and the loop drives the browser. Press `ctrl+alt+x` mid-run: the run stops with outcome `aborted` and the daemon prints `idle`.

- [ ] **Step 6: Hardware check — voice**

Start the daemon, hold `ctrl+alt+space`, say "open the typesafe console", release. Expected: `heard:` with the transcript, then `queued:` and a run. Then hold and say "stop" mid-run: `stop_run` classified, the run aborts, no new job is queued.

- [ ] **Step 7: Hardware check — the gate**

Hold the talk hotkey and say something unrelated, e.g. "are you coming for lunch". Expected: `ignored (ignore, ...)` and no run.

- [ ] **Step 8: Record what the run cost**

Note the transcription time for a five-second utterance and the interpretation latency from the daemon log, and put both in the README's voice section, replacing nothing else.

- [ ] **Step 9: Commit and push**

```bash
git add -A && git commit -m "Record measured voice latency in the README"
git push origin main
```

---

## Self-review notes

- **Spec coverage:** control plane T1/T2/T5/T8; voice T3; jev interpretation T6; overlay T4; defaults T7; entry point and docs T9; failure handling is covered by tests in T2 (refused hotkey), T3 (`VoiceUnavailable`), T6 (both gates), T8 (a raising job, a voice error); end-to-end is the merge pass.
- **Naming consistency checked:** `Control.toggle_pause`, `Hotkeys.vk_of`, `Hotkeys.id_of`, `voice.listen`, `voice.transcribe_frames`, `intent.interpret`, `Intent.actionable`, `daemon.Job`, `daemon.serve` are used with the same names in every task that references them.
- **Known follow-ups, deliberately out of scope:** no tray icon, no wake word, no follow-up goals against a previous run's history, no cross-session memory, no resumable runs. Those are pieces 3 and 4.
