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
