"""The resident process: hotkeys in, one run at a time out.

Three threads, because Windows requires it. RegisterHotKey delivers WM_HOTKEY only to the thread
that registered it and only while that thread pumps messages, so one worker thread registers and
pumps and does nothing else. Recording, transcription, the overlay and the classifier run on the
input worker, and the loop itself on the run worker, so a three-second utterance never stalls the
pump. The main thread is left free for a UI; `Service` is what such a UI starts and stops.

Every collaborator is injected, which is what makes this testable without a microphone.
"""

from __future__ import annotations

import queue
import threading
import time
import traceback
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
    act: bool = True  # False is a dry run: it decides and reports, and never touches the machine


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
        self.act = True  # the panel flips this; the headless daemon always acts
        self.voice_enabled = True

    # ------------------------------------------------------------------ hotkey handlers

    def on_talk(self) -> None:
        """Record, transcribe, and let the classifier say what it was."""
        if not self.voice_enabled:
            self.log("voice is off")
            return
        try:
            said = self.listen_now()
        except VoiceUnavailable as e:
            self.log(f"voice unavailable: {e}")
            return
        except Exception as e:
            self.log(f"voice failed: {e}")
            return
        if not said or not said.strip():
            self.log("heard nothing")
            return
        self.log(f"heard: {said!r}")
        self.handle_line(said)

    def on_goal(self) -> None:
        typed = self.ask_goal()
        if not typed or not typed.strip():
            return
        self.queue_goal(typed.strip())

    def probe_voice(self):
        """Hear one line and say what it was taken for, without acting on it.

        The honest way to try a microphone, a model and a phrasing: it runs the whole path up to
        the dispatch and stops there, so nothing is queued and no flag is flipped.
        """
        try:
            said = self.listen_now()
        except VoiceUnavailable as e:
            self.log(f"voice unavailable: {e}")
            return None
        except Exception as e:
            self.log(f"voice failed: {e}")
            return None
        if not said or not said.strip():
            self.log("test: heard nothing")
            return None
        decided = self.interpret_line(said, self.running, self.control.paused)
        verdict = "would act" if decided.actionable else "would be ignored"
        self.log(f"test: heard {said!r} -> {decided.command} ({decided.confidence:.2f}, heard {decided.heard:.2f}); {verdict}")
        return decided

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
        elif decided.command == "quit_daemon":
            self.on_quit()
        elif not self.running:
            # stop, pause and resume all talk about a run. Said while idle they are a misread of
            # something else in the room, and acting on them leaves flags set for the next job.
            self.log(f"  nothing is running to {decided.command.split('_')[0]}")
        elif decided.command == "stop_run":
            self.control.abort("asked to stop")
            self.log("aborting the run")
        elif decided.command == "pause_run":
            self.control.pause()
            self.log("paused")
        elif decided.command == "resume_run":
            self.control.resume()
            self.log("resumed")

    def queue_goal(self, goal: str) -> None:
        self.jobs.put(Job(goal, act=self.act))
        self.log(f"  queued: {goal!r}" + ("" if self.act else "  (dry run: it will decide, not act)"))

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


class Service:
    """The daemon's threads, startable and stoppable from anywhere.

    `start()` returns at once, so the caller's thread stays free — for a Tk `mainloop()`, or for
    the headless `serve()` below, which simply blocks on `join()`. The hotkeys are registered AND
    pumped on one worker thread, because Windows delivers WM_HOTKEY nowhere else.
    """

    HOTKEY_HANDLERS = ("talk", "goal", "pause", "abort", "quit")
    JOIN_TIMEOUT = 3.0

    def __init__(self, daemon: Daemon, log=print) -> None:
        self.daemon = daemon
        self.log = log
        self.commands: queue.Queue = queue.Queue()
        self.threads: list[threading.Thread] = []
        self._lock = threading.Lock()
        self._started = False
        self._stopping = False

    # ------------------------------------------------------------------ wiring

    def post(self, handler) -> None:
        """Run something on the input worker. For work that blocks — recording holds until the key
        comes up — called from a hotkey callback or from the Tk thread, neither of which may wait."""
        self.commands.put(handler)

    def bind_hotkeys(self) -> None:
        """Give every hotkey a callback that posts and returns: the pump must never block."""
        keys = self.daemon.keys
        if keys is None or keys.bindings:
            return
        for name in self.HOTKEY_HANDLERS:
            handler = getattr(self.daemon, f"on_{name}")
            keys.add(name, config.hotkey(name), (lambda h=handler: self.commands.put(h)))

    def report(self, refused: list[str]) -> None:
        keys = self.daemon.keys
        if keys is None:
            return
        for name in refused:
            self.log(f"hotkey for {name} is taken by another app: {config.hotkey(name)}")
        for binding in keys.bindings.values():
            if binding.name not in refused:
                self.log(f"  {binding.spec:<16} {binding.name}")

    # ------------------------------------------------------------------ the threads

    def _pump(self) -> None:
        keys = self.daemon.keys
        assert keys is not None
        try:
            keys.serve(on_ready=self.report)
        except Exception:
            self.log("the hotkey pump failed:\n" + traceback.format_exc())

    def _input_worker(self) -> None:
        while not self.daemon.stopped.is_set():
            try:
                handler = self.commands.get(timeout=0.2)
            except queue.Empty:
                continue
            if handler is None:
                return
            try:
                handler()
            except Exception:
                self.log("a hotkey handler failed:\n" + traceback.format_exc())

    # ------------------------------------------------------------------ lifecycle

    def start(self) -> None:
        """Start the threads and return. Never blocks."""
        with self._lock:
            if self._started:
                return
            self._started = True
        self.bind_hotkeys()
        self.threads = [
            threading.Thread(target=self._input_worker, name="input", daemon=True),
            threading.Thread(target=self.daemon.work, name="runs", daemon=True),
        ]
        if self.daemon.keys is not None:
            self.threads.insert(0, threading.Thread(target=self._pump, name="hotkeys", daemon=True))
        for thread in self.threads:
            thread.start()

    def stop(self) -> None:
        """Quit the daemon, wake every queue, and join. Idempotent, and safe from any thread."""
        with self._lock:
            first = not self._stopping
            self._stopping = True
        if first and not self.daemon.stopped.is_set():
            self.daemon.on_quit()
        self.daemon.stopped.set()
        self.commands.put(None)
        self.daemon.jobs.put(None)
        if self.daemon.keys is not None:
            self.daemon.keys.stop()
        current = threading.current_thread()
        for thread in self.threads:
            if thread is not current:
                thread.join(timeout=self.JOIN_TIMEOUT)

    def join(self, timeout: float | None = None) -> None:
        """Block until the daemon is asked to quit, or until `timeout` runs out."""
        deadline = None if timeout is None else time.monotonic() + timeout
        while not self.daemon.stopped.is_set():
            # A short wait rather than one long one, so Ctrl-C lands on the main thread.
            if deadline is not None and time.monotonic() >= deadline:
                return
            self.daemon.stopped.wait(0.2)


def build(log=print) -> Daemon:
    """Wire the real collaborators: hotkeys, the microphone, the model, the loop."""
    control = Control()
    keys = hotkeys.Hotkeys()
    writer = make_writer()

    def run_job(job: Job, control: Control) -> None:
        cfg = RunConfig(goal=job.goal, out=Path("runs") / time.strftime("%Y%m%d-%H%M%S"), act=job.act)

        def ctx_factory(typesafe, history):
            return Context(
                goal=job.goal,
                browser=config.browser(),
                email=config.email(),
                typesafe=typesafe,
                writer=writer,
                history=history,
            )

        # echo=log is what puts the step lines in front of whoever is watching: the terminal for
        # the headless daemon, the live feed for the panel.
        run(cfg, ctx_factory, control, echo=log)

    def interpret_line(text: str, running: bool, paused: bool):
        from typesafe_sdk import TypeSafeClient

        with TypeSafeClient() as client:
            return intent.interpret(client, text, running, paused, config.voice_min_confidence())

    def listen_now() -> str:
        return voice.listen(keys.vk_of("talk"), config.voice_max_seconds(), config.whisper_model())

    return Daemon(
        run_job=run_job,
        interpret_line=interpret_line,
        listen_now=listen_now,
        ask_goal=overlay.ask_for_goal,
        keys=keys,
        control=control,
        log=log,
    )


def serve(log=print) -> None:
    """Start the daemon headless and block until it is asked to quit."""
    daemon = build(log)
    service = Service(daemon, log)
    service.start()
    log("listening. hold the talk hotkey and say what you want done.")
    try:
        service.join()
    except KeyboardInterrupt:
        pass
    finally:
        service.stop()
    log("stopped")
