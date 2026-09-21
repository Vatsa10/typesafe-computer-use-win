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

    def run_job(job: Job, control: Control) -> None:
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
    for binding in keys.bindings.values():
        if binding.name not in refused:
            log(f"  {binding.spec:<16} {binding.name}")

    def input_worker() -> None:
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
