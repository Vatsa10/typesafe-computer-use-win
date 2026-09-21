"""Windows adapter: synthetic input, app control, screen capture, OCR, and the UI Automation tree.

This is the only module that touches user32, UI Automation, or the Windows OCR engine. It exposes
exactly the names `windows.py` does, so everything above `host.py` is unchanged by which one is live.

Roles are reported with the AX names the rest of the project speaks: a UIA ButtonControl comes
back as "AXButton". The mapping is one dict, `UIA_TO_AX`.
"""

from __future__ import annotations

import asyncio
import ctypes
import os
import subprocess
import time
from collections.abc import Callable
from ctypes import wintypes

from PIL import Image, ImageGrab

from .axwalk import AX_PRESS, AxAttrs, AxNode, Frame, walk_actionable
from .config import ABORT_CORNER_PX
from .models import Abort, Field

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

# Per-monitor DPI awareness, so window rectangles, cursor positions and the capture all speak the
# same physical pixels. Without it Windows lies to the process about a scaled display and every
# click on a 150% monitor lands short. It must happen before the first window query.
try:
    ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)
except Exception:  # an older Windows, or a host that already set it
    user32.SetProcessDPIAware()

VK = {"return": 0x0D, "tab": 0x09, "escape": 0x1B, "a": 0x41, "delete": 0x2E, "back": 0x08}
VK_CONTROL = 0x11
MIN_WINDOW_SIDE_PX = 50  # anything smaller is a palette or a tooltip, not the window being worked in
POST_EVENT_SLEEP = 0.04
WHEEL_PER_LINE = 40  # a notch is 120 and scrolls three lines, so one "line" is 40

# ------------------------------------------------------------------ escape hatch


def mouse_location() -> tuple[float, float]:
    point = wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(point))
    return float(point.x), float(point.y)


def check_abort() -> None:
    x, y = mouse_location()
    if x <= ABORT_CORNER_PX and y <= ABORT_CORNER_PX:
        raise Abort("mouse in top-left corner")


def sleep_watching(seconds: float, check: Callable[[], None] | None = None) -> None:
    """Wait, polling an interrupt check every 100 ms. The default check is the corner escape hatch;
    the daemon passes one that also honours pause and abort."""
    check = check_abort if check is None else check
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        check()
        time.sleep(0.1)


def accessibility_trusted() -> bool:
    """Windows asks no permission for UI Automation or synthetic input."""
    return True


PERMISSION_HINT = ""  # nothing to grant on Windows

# ------------------------------------------------------------------ input

INPUT_MOUSE, INPUT_KEYBOARD = 0, 1
MOUSEEVENTF_MOVE_ABSOLUTE = 0x8001  # MOVE | ABSOLUTE
MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_WHEEL = 0x0002, 0x0004, 0x0800
KEYEVENTF_KEYUP, KEYEVENTF_UNICODE = 0x0002, 0x0004
ABSOLUTE_RANGE = 65535


class _MouseInput(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG)),
    ]


class _KeyInput(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG)),
    ]


class _InputUnion(ctypes.Union):
    _fields_ = [("mi", _MouseInput), ("ki", _KeyInput)]


class _Input(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("u", _InputUnion)]


def _send(event: _Input) -> None:
    user32.SendInput(1, ctypes.byref(event), ctypes.sizeof(_Input))
    time.sleep(POST_EVENT_SLEEP)


def _mouse(flags: int, dx: int = 0, dy: int = 0, data: int = 0) -> None:
    _send(_Input(type=INPUT_MOUSE, u=_InputUnion(mi=_MouseInput(dx, dy, data & 0xFFFFFFFF, flags, 0, None))))


def _key(vk: int, scan: int, flags: int) -> None:
    _send(_Input(type=INPUT_KEYBOARD, u=_InputUnion(ki=_KeyInput(vk, scan, flags, 0, None))))


def _absolute(point: tuple[float, float]) -> tuple[int, int]:
    """A physical pixel as the 0..65535 coordinate SendInput wants, over the whole virtual desktop."""
    left, top = user32.GetSystemMetrics(76), user32.GetSystemMetrics(77)  # SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN
    width, height = user32.GetSystemMetrics(78) or 1, user32.GetSystemMetrics(79) or 1
    x = round((point[0] - left) * ABSOLUTE_RANGE / width)
    y = round((point[1] - top) * ABSOLUTE_RANGE / height)
    return x, y


def move_mouse(point: tuple[float, float]) -> None:
    x, y = _absolute(point)
    _mouse(MOUSEEVENTF_MOVE_ABSOLUTE, x, y)


def click_at(point: tuple[float, float]) -> None:
    move_mouse(point)
    _mouse(MOUSEEVENTF_LEFTDOWN)
    _mouse(MOUSEEVENTF_LEFTUP)


def press(key: str, command: bool = False) -> None:
    """`command` is the Mac name for the modifier the shortcut hangs off; here that is Control."""
    code = VK[key]
    if command:
        _key(VK_CONTROL, 0, 0)
    _key(code, 0, 0)
    _key(code, 0, KEYEVENTF_KEYUP)
    if command:
        _key(VK_CONTROL, 0, KEYEVENTF_KEYUP)


def type_text(text: str) -> None:
    """Unicode scan codes, so the text lands whatever keyboard layout is active."""
    units = text.encode("utf-16-le")
    for i in range(0, len(units), 2):  # one UTF-16 code unit per event; an emoji goes as its two halves
        unit = int.from_bytes(units[i : i + 2], "little")
        for flags in (KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP):
            _key(0, unit, flags)


def clear_field() -> None:
    press("a", command=True)
    press("delete")


def scroll(lines: int) -> None:
    """Wheel events go to the window under the cursor, so park it over the frontmost window first."""
    center = frontmost_window_center()
    if center is not None:
        move_mouse(center)
    _mouse(MOUSEEVENTF_WHEEL, data=ctypes.c_int32(lines * WHEEL_PER_LINE).value)


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


# ------------------------------------------------------------------ apps and windows

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
MAX_PATH_LONG = 1024


def _foreground_hwnd() -> int:
    return user32.GetForegroundWindow()


def _pid_of(hwnd: int) -> int:
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    return int(pid.value)


def _process_name(pid: int) -> str:
    """The executable's name without its extension: "chrome", "explorer", "Code"."""
    handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        return ""
    try:
        size = wintypes.DWORD(MAX_PATH_LONG)
        buffer = ctypes.create_unicode_buffer(MAX_PATH_LONG)
        if not kernel32.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size)):
            return ""
        return os.path.splitext(os.path.basename(buffer.value))[0]
    finally:
        kernel32.CloseHandle(handle)


def frontmost_app() -> str:
    return _process_name(_pid_of(_foreground_hwnd()))


def frontmost_app_and_pid() -> tuple[str, int]:
    pid = _pid_of(_foreground_hwnd())
    return _process_name(pid), pid


def frontmost_pid() -> int:
    return _pid_of(_foreground_hwnd())


def _window_title(hwnd: int) -> str:
    length = user32.GetWindowTextLengthW(hwnd)
    buffer = ctypes.create_unicode_buffer(length + 1)
    user32.GetWindowTextW(hwnd, buffer, length + 1)
    return buffer.value


def _top_level_windows() -> list[int]:
    found: list[int] = []
    proc = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)(lambda hwnd, _param: found.append(hwnd) or True)
    user32.EnumWindows(proc, 0)
    return found


def _app_matches(app: str, pid: int, hwnd: int) -> bool:
    """An app is named loosely here: "Google Chrome", "chrome" and a window title all have to hit."""
    wanted = app.lower().replace(" ", "")
    name = _process_name(pid).lower().replace(" ", "")
    title = _window_title(hwnd).lower().replace(" ", "")
    return bool(name) and (name in wanted or wanted in name or wanted in title)


def _find_window(app: str) -> int | None:
    for hwnd in _top_level_windows():
        if not user32.IsWindowVisible(hwnd) or not _window_title(hwnd):
            continue
        if _app_matches(app, _pid_of(hwnd), hwnd):
            return hwnd
    return None


SW_RESTORE = 9


def _raise(hwnd: int) -> None:
    """SetForegroundWindow is refused unless the caller owns the foreground, so borrow its input
    queue with AttachThreadInput first. That is the documented way, not a trick."""
    if user32.IsIconic(hwnd):
        user32.ShowWindow(hwnd, SW_RESTORE)
    current = kernel32.GetCurrentThreadId()
    target = user32.GetWindowThreadProcessId(_foreground_hwnd(), None)
    attached = current != target and user32.AttachThreadInput(current, target, True)
    try:
        user32.SetForegroundWindow(hwnd)
        user32.BringWindowToTop(hwnd)
    finally:
        if attached:
            user32.AttachThreadInput(current, target, False)


def activate(app: str, timeout: float = 3.0) -> bool:
    """Bring an app to the front and confirm it got there."""
    hwnd = _find_window(app)
    if hwnd is None:
        return False
    _raise(hwnd)
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if _foreground_hwnd() == hwnd or _app_matches(app, frontmost_pid(), _foreground_hwnd()):
            return True
        time.sleep(0.1)
    return False


def open_url(browser: str, url: str) -> bool:
    """Open a page in the named browser, or in the default one when that browser is not installed."""
    executable = {"google chrome": "chrome", "microsoft edge": "msedge", "firefox": "firefox"}.get(browser.lower(), browser)
    try:
        subprocess.Popen([executable, url], shell=False)
    except OSError:
        os.startfile(url)
    time.sleep(1.0)
    return activate(browser) or activate(executable)


ADDRESS_BAR_NAMES = ("address and search bar", "address field", "search or enter address")


def browser_url(browser: str) -> str | None:
    """The address bar's text, read out of the browser window's UIA tree. Best effort: a browser
    that is not in front, or one that hides its address bar, reports nothing."""
    try:
        import uiautomation as uia

        window = uia.ControlFromHandle(_foreground_hwnd())
        if window is None or not _app_matches(browser, frontmost_pid(), _foreground_hwnd()):
            return None
        for edit in _descendant_edits(window):
            name = (edit.Name or "").lower()
            value = _value_of(edit) or ""
            if value and (any(hint in name for hint in ADDRESS_BAR_NAMES) or value.startswith(("http://", "https://"))):
                return value if "://" in value else f"https://{value}"
    except Exception:
        return None
    return None


URL_CONTROLS = ("EditControl", "DocumentControl")


def _descendant_edits(root, depth: int = 9) -> list:
    """The controls that carry a URL, within a few levels of the window: the omnibox, and the page
    document itself, which Chromium and Edge both give a value of the address they are showing.

    Depth is capped because the page's own tree below the document is unbounded and holds no URL.
    """
    out, queue = [], [(root, 0)]
    while queue:
        node, level = queue.pop(0)
        if level >= depth:
            continue
        for child in _children(node):
            if child.ControlTypeName in URL_CONTROLS:
                out.append(child)
            queue.append((child, level + 1))
    return out


def frontmost_window_bounds(pid: int | None = None) -> tuple[float, float, float, float] | None:
    """The frontmost window as x, y, w, h in physical pixels, which are this adapter's points."""
    hwnd = _foreground_hwnd()
    if pid is not None and _pid_of(hwnd) != pid:
        hwnd = next((h for h in _top_level_windows() if user32.IsWindowVisible(h) and _pid_of(h) == pid), 0)
    if not hwnd:
        return None
    rect = wintypes.RECT()
    if not user32.GetWindowRect(hwnd, ctypes.byref(rect)):
        return None
    w, h = rect.right - rect.left, rect.bottom - rect.top
    if w <= MIN_WINDOW_SIDE_PX or h <= MIN_WINDOW_SIDE_PX:
        return None
    return float(rect.left), float(rect.top), float(w), float(h)


def frontmost_window_center(pid: int | None = None) -> tuple[float, float] | None:
    bounds = frontmost_window_bounds(pid)
    if bounds is None:
        return None
    x, y, w, h = bounds
    return x + w / 2, y + h / 2


# ------------------------------------------------------------------ capture and OCR


def screenshot() -> Image.Image:
    return ImageGrab.grab(all_screens=False).convert("RGB")


def display_scale(image: Image.Image) -> float:
    """Capture pixels per point. The process is DPI aware, so both are physical pixels and this is 1,
    except on a capture that was scaled elsewhere (a replayed file)."""
    width = user32.GetSystemMetrics(0) or image.width
    return image.width / float(width)


OCR_LANGUAGE = os.environ.get("CLICKER_OCR_LANGUAGE", "en-US")
OCR_ENGINE = os.environ.get("CLICKER_OCR_ENGINE", "windows").strip().lower()
WINDOWS_OCR_CONFIDENCE = 1.0  # the engine reports none; see ocr_recognize
Read = tuple[str, float, tuple[float, float, float, float]]


def ocr_recognize(image: Image.Image) -> list[Read]:
    """Read one image. Text, confidence, and the box in the image's own pixels, one entry per line.

    Two engines, chosen by CLICKER_OCR_ENGINE:

    `windows` (the default) is Windows.Media.Ocr, which reads a full screen in about 0.05 s but
    reports no per-line confidence, so every line it returns is scored WINDOWS_OCR_CONFIDENCE. That
    is honest rather than useful: the engine has already dropped what it could not read, and the
    pipeline's only use of the number is a threshold such a line always clears.

    `rapidocr` is RapidOCR on onnxruntime, which does score each line, at about 4 s for a full
    screen on this machine. Worth it when garbage lines are costing decisions, not otherwise.
    """
    if OCR_ENGINE == "rapidocr":
        return _rapidocr_recognize(image)
    if OCR_ENGINE != "windows":
        raise ValueError(f"CLICKER_OCR_ENGINE={OCR_ENGINE!r} is not a known engine (windows, rapidocr)")
    return asyncio.run(_windows_recognize(image))


async def _windows_recognize(image: Image.Image) -> list[Read]:
    from winrt.windows.globalization import Language
    from winrt.windows.graphics.imaging import BitmapPixelFormat, SoftwareBitmap
    from winrt.windows.media.ocr import OcrEngine
    from winrt.windows.security.cryptography import CryptographicBuffer

    engine = OcrEngine.try_create_from_language(Language(OCR_LANGUAGE)) or OcrEngine.try_create_from_user_profile_languages()
    if engine is None:
        raise RuntimeError(f"no Windows OCR language pack for {OCR_LANGUAGE!r}; add one in Settings > Language")
    rgba = image.convert("RGBA")
    buffer = CryptographicBuffer.create_from_byte_array(rgba.tobytes("raw", "BGRA"))
    bitmap = SoftwareBitmap.create_copy_from_buffer(buffer, BitmapPixelFormat.BGRA8, rgba.width, rgba.height)
    result = await engine.recognize_async(bitmap)
    lines: list[Read] = []
    for line in result.lines:
        boxes = [w.bounding_rect for w in line.words]
        if not boxes:
            continue
        x1 = min(b.x for b in boxes)
        y1 = min(b.y for b in boxes)
        x2 = max(b.x + b.width for b in boxes)
        y2 = max(b.y + b.height for b in boxes)
        lines.append((line.text, WINDOWS_OCR_CONFIDENCE, (float(x1), float(y1), float(x2), float(y2))))
    return lines


_rapidocr = None


def _rapidocr_engine():
    """One engine for the process: loading the ONNX models costs about a second."""
    global _rapidocr
    if _rapidocr is None:
        from rapidocr_onnxruntime import RapidOCR

        _rapidocr = RapidOCR()
    return _rapidocr


def _rapidocr_recognize(image: Image.Image) -> list[Read]:
    """Read one image with RapidOCR, which scores every line it returns.

    Boxes come back as four corners of a quadrilateral, because the detector allows rotated text;
    the pipeline wants an upright rectangle, so each one is reduced to its bounding box.

    RapidOCR loads a PIL image itself, so nothing here needs numpy: the extra stays optional and
    the tests below run on a bare install.
    """
    result, _elapsed = _rapidocr_engine()(image.convert("RGB"))
    lines: list[Read] = []
    for corners, text, score in result or []:
        xs = [float(p[0]) for p in corners]
        ys = [float(p[1]) for p in corners]
        lines.append((text, float(score), (min(xs), min(ys), max(xs), max(ys))))
    return lines


# ------------------------------------------------------------------ UI Automation

# UIA control types, under the AX names the rest of the project already speaks.
UIA_TO_AX = {
    "ButtonControl": "AXButton",
    "CheckBoxControl": "AXCheckBox",
    "ComboBoxControl": "AXComboBox",
    "DataItemControl": "AXRow",
    "DocumentControl": "AXTextArea",
    "EditControl": "AXTextField",
    "HyperlinkControl": "AXLink",
    "ImageControl": "AXImage",
    "ListItemControl": "AXCell",
    "MenuItemControl": "AXMenuBarItem",
    "RadioButtonControl": "AXRadioButton",
    "SliderControl": "AXSlider",
    "SplitButtonControl": "AXMenuButton",
    "TabItemControl": "AXTab",
    "TextControl": "AXStaticText",
    "TreeItemControl": "AXRow",
    "GroupControl": "AXGroup",
    "PaneControl": "AXGroup",
}
UIA_TIMEOUT = 0.2
VALUE_CHARS = 120


def _uia():
    import uiautomation as uia

    uia.SetGlobalSearchTimeout(UIA_TIMEOUT)
    return uia


def _children(node) -> list:
    try:
        return node.GetChildren()
    except Exception:
        return []


def _pattern(node, name: str):
    getter = getattr(node, f"Get{name}Pattern", None)
    if getter is None:
        return None
    try:
        return getter()
    except Exception:
        return None


def _value_of(node) -> str | None:
    pattern = _pattern(node, "Value")
    if pattern is None:
        return None
    try:
        return pattern.Value
    except Exception:
        return None


def _frame(node) -> Frame | None:
    try:
        rect = node.BoundingRectangle
    except Exception:
        return None
    if rect is None:
        return None
    return float(rect.left), float(rect.top), float(rect.width()), float(rect.height())


def _label(node) -> str:
    """The control's name, or a short value when it has no name (an address bar, a filled field)."""
    try:
        name = node.Name or ""
    except Exception:
        name = ""
    if name.strip():
        return " ".join(name.split())
    value = _value_of(node) or ""
    if 0 < len(value.strip()) <= VALUE_CHARS:
        return " ".join(value.split())
    return ""


def _attrs(node) -> AxAttrs:
    try:
        control = node.ControlTypeName
    except Exception:
        control = ""
    return AxAttrs(UIA_TO_AX.get(control, control), _label(node), _frame(node))


PRESS_PATTERNS = ("Invoke", "Toggle", "SelectionItem", "ExpandCollapse")


def _actions(node) -> list[str]:
    """A node that carries any pattern that amounts to "activate this" is pressable."""
    for name in PRESS_PATTERNS:
        if _pattern(node, name) is not None:
            return [AX_PRESS]
    return []


def ax_press(ref) -> bool:
    """Activate an element through whichever pattern it offers."""
    for name, call in (
        ("Invoke", "Invoke"),
        ("Toggle", "Toggle"),
        ("SelectionItem", "Select"),
        ("ExpandCollapse", "Expand"),
    ):
        pattern = _pattern(ref, name)
        if pattern is None:
            continue
        try:
            getattr(pattern, call)()
            return True
        except Exception:
            continue
    return False


def ax_focus(ref) -> bool:
    try:
        ref.SetFocus()
        return True
    except Exception:
        return False


def ax_set_value(ref, text: str) -> bool:
    """Write an element's value. A read-only or unwilling element reports an error."""
    pattern = _pattern(ref, "Value")
    if pattern is None:
        return False
    try:
        pattern.SetValue(text)
        return True
    except Exception:
        return False


def ax_value(ref) -> str | None:
    return _value_of(ref)


def focused_field() -> Field | None:
    try:
        uia = _uia()
        element = uia.GetFocusedControl()
    except Exception:
        return None
    if element is None:
        return None
    role, label, frame = _attrs(element)
    x, y, w, h = frame or (0.0, 0.0, 0.0, 0.0)
    value = _value_of(element)
    return Field(
        role=role,
        label=label,
        placeholder=str(getattr(element, "HelpText", "") or ""),
        value=value if isinstance(value, str) else "",
        x=x,
        y=y,
        w=w,
        h=h,
        ref=element,
    )


class _ProcessRoot:
    """A stand-in parent for one process's top-level windows, so the walk starts where AX does."""

    def __init__(self, windows: list) -> None:
        self.windows = windows
        self.ControlTypeName = "PaneControl"
        self.Name = ""
        self.BoundingRectangle = None


def _root_children(node) -> list:
    return node.windows if isinstance(node, _ProcessRoot) else _children(node)


def actionable_elements(pid: int, display_w_pt: float, display_h_pt: float) -> tuple[list[AxNode], list[AxNode], bool]:
    """Labelled controls of one process: the on-screen ones, the pressable off-screen ones, and
    whether a cap cut the walk short."""
    uia = _uia()
    desktop = uia.GetRootControl()
    windows = [w for w in _children(desktop) if getattr(w, "ProcessId", None) == pid]
    root = _ProcessRoot(windows)
    return walk_actionable(root, _root_children, _attrs, _actions, display_w_pt, display_h_pt)


# ------------------------------------------------------------------ files


def open_file(path: str, as_text: bool = False) -> None:
    """Show a file the way the desktop would: the text editor for text, the shell default otherwise."""
    if as_text:
        subprocess.Popen(["notepad.exe", path])
        return
    os.startfile(path)
