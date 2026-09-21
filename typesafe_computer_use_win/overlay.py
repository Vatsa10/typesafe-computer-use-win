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
