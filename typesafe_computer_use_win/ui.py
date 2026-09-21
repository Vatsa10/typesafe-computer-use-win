"""A local control panel: type or speak a goal, watch the run, browse what happened.

Tkinter, because it ships with Python and because the alternative is worse than it looks: a browser
dashboard would live in the browser this program drives, so a run would read its own UI through OCR
and could click it. A desktop window has a milder version of the same problem, which is what the
"hide while acting" switch is for.

Threading rule, and the reason this module exists at all: Tk owns the main thread, so the hotkey
pump cannot. `daemon.Service` runs the pump, the input worker and the run worker on their own
threads, and everything they want to say arrives here through a queue that `_drain` empties on a
Tk timer. No widget is ever touched from another thread.
"""

from __future__ import annotations

import queue
import tkinter as tk
from pathlib import Path
from tkinter import ttk

from . import config, dotenv_io, runs_index
from . import daemon as daemon_module

DOTENV = Path.cwd() / ".env"
RUNS = Path.cwd() / "runs"

BG = "#0b1120"
PANEL = "#111c30"
INK = "#e8eefc"
MUTED = "#8ba3c4"
ACCENT = "#38bdf8"
DRAIN_MS = 100
MAX_LOG_LINES = 2000
PREVIEW_WIDTH = 820

# The settings the panel edits, as (env key, label, how to show the default when the value is blank).
SETTINGS: list[tuple[str, str, str]] = [
    ("CLICKER_BROWSER", "Browser", config.DEFAULT_BROWSER),
    ("CLICKER_EMAIL", "Email for type_email", "(unset: the action is not offered)"),
    ("CLICKER_HOTKEY_TALK", "Hotkey: push to talk", config.DEFAULT_HOTKEYS["talk"]),
    ("CLICKER_HOTKEY_GOAL", "Hotkey: typed goal", config.DEFAULT_HOTKEYS["goal"]),
    ("CLICKER_HOTKEY_PAUSE", "Hotkey: pause or resume", config.DEFAULT_HOTKEYS["pause"]),
    ("CLICKER_HOTKEY_ABORT", "Hotkey: abort the run", config.DEFAULT_HOTKEYS["abort"]),
    ("CLICKER_HOTKEY_QUIT", "Hotkey: quit", config.DEFAULT_HOTKEYS["quit"]),
    ("CLICKER_WHISPER_MODEL", "Whisper model", config.DEFAULT_WHISPER_MODEL),
    ("CLICKER_VOICE_MAX_SECONDS", "Longest utterance (s)", str(config.DEFAULT_VOICE_MAX_SECONDS)),
    ("CLICKER_VOICE_MIN_CONFIDENCE", "Voice confidence floor", str(config.DEFAULT_VOICE_MIN_CONFIDENCE)),
    ("CLICKER_OCR_ENGINE", "OCR engine", "windows"),
    ("CLICKER_OCR_LANGUAGE", "OCR language", "en-US"),
]


def apply_theme(root: tk.Tk) -> None:
    """Dark ttk chrome. The default Windows theme ignores background colours on notebooks and
    buttons, so it has to be `clam`, which honours them."""
    style = ttk.Style(root)
    style.theme_use("clam")
    style.configure("TNotebook", background=BG, borderwidth=0)
    style.configure("TNotebook.Tab", background=PANEL, foreground=MUTED, padding=(16, 8), borderwidth=0)
    style.map("TNotebook.Tab", background=[("selected", BG)], foreground=[("selected", ACCENT)])
    style.configure("TButton", background=PANEL, foreground=INK, borderwidth=0, padding=(14, 7), focuscolor=BG)
    style.map("TButton", background=[("active", "#1d2d४a".replace("४", "4")), ("pressed", ACCENT)])
    style.configure("Vertical.TScrollbar", background=PANEL, troughcolor=BG, borderwidth=0, arrowcolor=MUTED)


def preview_factor(image_width: int, target: int = PREVIEW_WIDTH) -> int:
    """The integer shrink Tk needs to fit a full-display capture into the preview pane.

    Tk only subsamples by whole numbers, so this rounds up: a 2560 px capture into an 820 px pane
    is a factor of 4, not 3.1, and lands at 640 px rather than overflowing.
    """
    if image_width <= 0:
        return 1
    return max(1, -(-image_width // target))


def state_label(running: bool, paused: bool) -> str:
    """What the status bar says. Paused only means anything while something is running."""
    if running:
        return "paused" if paused else "running"
    return "idle"


def _text_widget(parent, height: int = 10) -> tk.Text:
    widget = tk.Text(
        parent,
        height=height,
        wrap="word",
        bg=PANEL,
        fg=INK,
        insertbackground=INK,
        relief="flat",
        font=("Cascadia Mono", 10),
        padx=10,
        pady=8,
    )
    widget.configure(state="disabled")
    return widget


def _append(widget: tk.Text, line: str) -> None:
    """Append one line and keep the view at the bottom, trimming what has scrolled out of use."""
    widget.configure(state="normal")
    widget.insert("end", line + "\n")
    excess = int(widget.index("end-1c").split(".")[0]) - MAX_LOG_LINES
    if excess > 0:
        widget.delete("1.0", f"{excess}.0")
    widget.see("end")
    widget.configure(state="disabled")


class Panel:
    """The window. Owns the Tk main thread and drives a `daemon.Service` behind it."""

    def __init__(self, root: tk.Tk, service, messages: queue.Queue) -> None:
        self.root = root
        self.service = service
        self.daemon = service.daemon
        self.messages = messages
        self.hidden_for_run = False
        self.runs: list[runs_index.RunSummary] = []
        self.preview_image: tk.PhotoImage | None = None  # Tk drops an image that nothing references

        root.title("winclicker")
        root.configure(bg=BG)
        root.geometry("1040x760")
        root.minsize(880, 560)
        apply_theme(root)
        root.protocol("WM_DELETE_WINDOW", self.close)

        self.status = tk.Label(root, text="idle", anchor="w", bg=PANEL, fg=MUTED, font=("Cascadia Mono", 9), padx=12, pady=6)
        self.status.pack(side="bottom", fill="x")  # packed first, so the notebook cannot squeeze it out

        notebook = ttk.Notebook(root)
        notebook.pack(fill="both", expand=True, padx=10, pady=(10, 6))
        self._build_run_tab(notebook)
        self._build_history_tab(notebook)
        self._build_settings_tab(notebook)

        self.root.after(DRAIN_MS, self._drain)

    # ------------------------------------------------------------------ run tab

    def _build_run_tab(self, notebook: ttk.Notebook) -> None:
        frame = tk.Frame(notebook, bg=BG)
        notebook.add(frame, text="Run")

        entry_row = tk.Frame(frame, bg=BG)
        entry_row.pack(fill="x", padx=12, pady=12)
        self.goal_entry = tk.Entry(entry_row, bg=PANEL, fg=INK, insertbackground=INK, relief="flat", font=("Segoe UI", 12))
        self.goal_entry.pack(side="left", fill="x", expand=True, ipady=8, padx=(0, 8))
        self.goal_entry.bind("<Return>", lambda _e: self.start())
        self.goal_entry.focus_set()
        ttk.Button(entry_row, text="Start", command=self.start).pack(side="left", padx=2)
        self.pause_button = ttk.Button(entry_row, text="Pause", command=self.toggle_pause)
        self.pause_button.pack(side="left", padx=2)
        ttk.Button(entry_row, text="Abort", command=self.abort).pack(side="left", padx=2)

        options = tk.Frame(frame, bg=BG)
        options.pack(fill="x", padx=12)
        self.hide_while_acting = tk.BooleanVar(value=True)
        tk.Checkbutton(
            options,
            text="hide this window while a run acts (it is on screen, so the run would read it)",
            variable=self.hide_while_acting,
            bg=BG,
            fg=MUTED,
            selectcolor=PANEL,
            activebackground=BG,
            activeforeground=INK,
            font=("Segoe UI", 9),
        ).pack(side="left")

        feed_box = tk.Frame(frame, bg=BG)
        feed_box.pack(fill="both", expand=True, padx=12, pady=12)
        self.feed = _text_widget(feed_box, height=24)
        scroll = ttk.Scrollbar(feed_box, orient="vertical", command=self.feed.yview, style="Vertical.TScrollbar")
        self.feed.configure(yscrollcommand=scroll.set)
        scroll.pack(side="right", fill="y")
        self.feed.pack(side="left", fill="both", expand=True)
        _append(self.feed, "ready. type a goal, or hold the talk hotkey and say one.")

    def start(self) -> None:
        goal = self.goal_entry.get().strip()
        if not goal:
            return
        self.goal_entry.delete(0, "end")
        self.daemon.queue_goal(goal)

    def toggle_pause(self) -> None:
        self.daemon.on_pause()

    def abort(self) -> None:
        self.daemon.on_abort()

    # ------------------------------------------------------------------ history tab

    def _build_history_tab(self, notebook: ttk.Notebook) -> None:
        frame = tk.Frame(notebook, bg=BG)
        notebook.add(frame, text="History")

        left = tk.Frame(frame, bg=BG)
        left.pack(side="left", fill="y", padx=(12, 6), pady=12)
        ttk.Button(left, text="Refresh", command=self.refresh_history).pack(fill="x", pady=(0, 6))
        self.run_list = tk.Listbox(
            left, width=34, bg=PANEL, fg=INK, relief="flat", font=("Cascadia Mono", 9), selectbackground=ACCENT
        )
        self.run_list.pack(fill="y", expand=True)
        self.run_list.bind("<<ListboxSelect>>", lambda _e: self.show_run())

        right = tk.Frame(frame, bg=BG)
        right.pack(side="left", fill="both", expand=True, padx=(6, 12), pady=12)
        self.run_detail = _text_widget(right, height=6)
        self.run_detail.pack(fill="x")
        self.step_list = tk.Listbox(
            right, height=4, bg=PANEL, fg=INK, relief="flat", font=("Cascadia Mono", 9), selectbackground=ACCENT
        )
        self.step_list.pack(fill="x", pady=(8, 8))
        self.step_list.bind("<<ListboxSelect>>", lambda _e: self.show_step())
        self.preview = tk.Label(right, bg=PANEL, text="select a step to see what it saw", fg=MUTED)
        self.preview.pack(fill="both", expand=True)

        self.refresh_history()

    def refresh_history(self) -> None:
        self.runs = runs_index.list_runs(RUNS)
        self.run_list.delete(0, "end")
        for run in self.runs:
            self.run_list.insert("end", f"{run.name}  {run.outcome}")
        if not self.runs:
            self.run_list.insert("end", "(no runs yet)")

    def _selected_run(self) -> runs_index.RunSummary | None:
        selection = self.run_list.curselection()
        if not selection or selection[0] >= len(self.runs):
            return None
        return self.runs[selection[0]]

    def show_run(self) -> None:
        run = self._selected_run()
        if run is None:
            return
        self.run_detail.configure(state="normal")
        self.run_detail.delete("1.0", "end")
        self.run_detail.configure(state="disabled")
        for line in (
            f"goal:     {run.goal}",
            f"outcome:  {run.outcome}" + (f"   ({run.seconds:.0f}s)" if run.seconds is not None else ""),
            f"steps:    {run.steps_taken}" + ("   acted" if run.acted else "   dry run"),
            f"answer:   {run.answer or '(none)'}",
            f"folder:   {run.path}",
        ):
            _append(self.run_detail, line)
        self.steps = runs_index.steps_of(run.path)
        self.step_list.delete(0, "end")
        for step in self.steps:
            self.step_list.insert("end", f"step {step.number:03d}")
        if not self.steps:
            self.step_list.insert("end", "(no steps recorded)")

    def show_step(self) -> None:
        selection = self.step_list.curselection()
        if not selection or selection[0] >= len(getattr(self, "steps", [])):
            return
        step = self.steps[selection[0]]
        image_path = step.annotated or step.raw
        if image_path is None:
            self.preview.configure(image="", text="this step wrote no capture")
            return
        try:
            image = tk.PhotoImage(file=str(image_path))
        except tk.TclError as e:  # a truncated PNG from a killed run
            self.preview.configure(image="", text=f"cannot read {image_path.name}: {e}")
            return
        # A capture is the whole display, so it needs shrinking by an integer factor to fit.
        factor = preview_factor(image.width())
        self.preview_image = image.subsample(factor, factor)
        self.preview.configure(image=self.preview_image, text="")

    # ------------------------------------------------------------------ settings tab

    def _build_settings_tab(self, notebook: ttk.Notebook) -> None:
        frame = tk.Frame(notebook, bg=BG)
        notebook.add(frame, text="Settings")
        current = dotenv_io.read_env(DOTENV)

        grid = tk.Frame(frame, bg=BG)
        grid.pack(fill="both", expand=True, padx=16, pady=16)
        self.setting_vars: dict[str, tk.StringVar] = {}
        for row, (key, label, default) in enumerate(SETTINGS):
            tk.Label(grid, text=label, bg=BG, fg=INK, anchor="w", font=("Segoe UI", 10)).grid(
                row=row, column=0, sticky="w", pady=4
            )
            var = tk.StringVar(value=current.get(key, ""))
            self.setting_vars[key] = var
            tk.Entry(
                grid, textvariable=var, bg=PANEL, fg=INK, insertbackground=INK, relief="flat", font=("Cascadia Mono", 10)
            ).grid(row=row, column=1, sticky="ew", padx=10, ipady=4)
            tk.Label(grid, text=f"default: {default}", bg=BG, fg=MUTED, anchor="w", font=("Segoe UI", 8)).grid(
                row=row, column=2, sticky="w"
            )
        grid.columnconfigure(1, weight=1)

        footer = tk.Frame(frame, bg=BG)
        footer.pack(fill="x", padx=16, pady=(0, 16))
        ttk.Button(footer, text="Save to .env", command=self.save_settings).pack(side="left")
        self.settings_note = tk.Label(footer, text="", bg=BG, fg=MUTED, font=("Segoe UI", 9))
        self.settings_note.pack(side="left", padx=12)

    def save_settings(self) -> None:
        """Write the file. Hotkeys are read when the daemon registers them, so they need a restart."""
        dotenv_io.write_env(DOTENV, {key: var.get().strip() for key, var in self.setting_vars.items()})
        self.settings_note.configure(text=f"saved to {DOTENV.name}. hotkey changes need a restart.")

    # ------------------------------------------------------------------ the pump

    def _drain(self) -> None:
        """Move whatever the worker threads said into the feed. The only place widgets are written
        on behalf of another thread, and it runs on the Tk thread by construction."""
        for _ in range(200):  # bounded, so a chatty run cannot starve the UI
            try:
                line = self.messages.get_nowait()
            except queue.Empty:
                break
            _append(self.feed, str(line))
        self._refresh_status()
        self.root.after(DRAIN_MS, self._drain)

    def _refresh_status(self) -> None:
        running, paused = self.daemon.running, self.daemon.control.paused
        state = state_label(running, paused)
        hotkeys = "  ".join(f"{config.hotkey(name)} {name}" for name in ("talk", "goal", "pause", "abort"))
        self.status.configure(text=f"{state}    {hotkeys}")
        self.pause_button.configure(text="Resume" if paused else "Pause")
        if running and self.hide_while_acting.get() and not self.hidden_for_run:
            self.hidden_for_run = True
            self.root.iconify()
        elif not running and self.hidden_for_run:
            self.hidden_for_run = False
            self.root.deiconify()
            self.refresh_history()  # the run just finished: it belongs in the list

    def close(self) -> None:
        self.service.stop()
        self.root.destroy()


def launch() -> None:
    """Build the daemon, start its threads, and hand the main thread to Tk."""
    messages: queue.Queue = queue.Queue()
    worker = daemon_module.build(log=messages.put)
    service = daemon_module.Service(worker, log=messages.put)
    service.start()
    root = tk.Tk()
    Panel(root, service, messages)
    try:
        root.mainloop()
    finally:
        service.stop()
