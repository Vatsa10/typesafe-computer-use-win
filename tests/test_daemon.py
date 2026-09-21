"""The daemon's wiring: what a hotkey queues, what the worker runs, and what is refused."""

from __future__ import annotations

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
    daemon, _ = make_daemon()
    daemon.on_talk()
    assert daemon.jobs.get_nowait().goal == "open the console"


def test_a_line_below_the_bar_queues_nothing():
    refused = Intent("run_goal", "open the console", 0.2, 0.9, "open the console", 0.55)
    daemon, _ = make_daemon(intent=refused)
    daemon.on_talk()
    assert daemon.jobs.empty()


def test_a_spoken_stop_aborts_the_run_in_flight():
    daemon, _ = make_daemon(intent=Intent("stop_run", "", 0.95, 0.95, "stop", 0.55))
    daemon.running = True
    daemon.on_talk()
    assert daemon.control.aborting is True
    assert daemon.jobs.empty()


def test_a_spoken_stop_with_nothing_running_leaves_no_flag_for_the_next_job():
    """An idle abort would be cleared by the next reset anyway, but the log would lie."""
    logged = []
    daemon, _ = make_daemon(intent=Intent("stop_run", "", 0.95, 0.95, "stop", 0.55))
    daemon.log = logged.append
    daemon.on_talk()
    assert daemon.control.aborting is False
    assert any("nothing is running" in line for line in logged)


def test_a_spoken_pause_pauses_and_a_spoken_resume_releases():
    daemon, _ = make_daemon(intent=Intent("pause_run", "", 0.95, 0.95, "pause", 0.55))
    daemon.running = True
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
