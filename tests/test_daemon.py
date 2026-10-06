"""The daemon's wiring: what a hotkey queues, what the worker runs, and what is refused."""

from __future__ import annotations

import threading

from typesafe_computer_use_win.daemon import Daemon, Job, Service
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


# ---------------------------------------------------------------- the service wrapper


def make_service(monkeypatch, **kw):
    """A Service over a fake daemon, with every win32 call replaced by a fake."""
    import time as _time

    from typesafe_computer_use_win import hotkeys as hotkeys_module
    from typesafe_computer_use_win import windows
    from typesafe_computer_use_win.daemon import Service

    registered: list[int] = []
    unregistered: list[int] = []
    pumping = threading.Event()

    def pump(on_hotkey, stop):
        pumping.set()
        while not stop():
            _time.sleep(0.01)

    monkeypatch.setattr(windows, "register_hotkey", lambda i, m, k: registered.append(i) or True)
    monkeypatch.setattr(windows, "unregister_hotkey", unregistered.append)
    monkeypatch.setattr(windows, "post_quit_message", lambda tid: None)
    monkeypatch.setattr(windows, "current_thread_id", lambda: 1)
    monkeypatch.setattr(windows, "pump_messages", pump)

    daemon, ran = make_daemon(**kw)
    daemon.keys = hotkeys_module.Hotkeys()
    service = Service(daemon, log=lambda *a: None)
    return service, ran, registered, unregistered, pumping


def test_the_service_starts_three_threads_without_blocking(monkeypatch):
    service, _, registered, _, pumping = make_service(monkeypatch)
    service.start()
    try:
        assert pumping.wait(2.0), "the hotkey thread must register and then pump"
        assert len(registered) == len(Service.HOTKEY_HANDLERS), "every binding is claimed on the pumping thread"
        assert sorted(t.name for t in service.threads) == ["hotkeys", "input", "runs"]
    finally:
        service.stop()


def test_stopping_the_service_leaves_no_thread_alive(monkeypatch):
    service, _, _, unregistered, pumping = make_service(monkeypatch)
    service.start()
    assert pumping.wait(2.0)
    service.stop()
    assert service.daemon.stopped.is_set()
    assert len(unregistered) == len(Service.HOTKEY_HANDLERS)
    for thread in service.threads:
        assert not thread.is_alive(), f"{thread.name} outlived stop()"


def test_stopping_twice_is_harmless(monkeypatch):
    service, _, _, _, pumping = make_service(monkeypatch)
    service.start()
    assert pumping.wait(2.0)
    service.stop()
    service.stop()  # the Tk thread may stop a service that already quit itself
    for thread in service.threads:
        assert not thread.is_alive()


def test_starting_twice_starts_one_set_of_threads(monkeypatch):
    service, _, registered, _, pumping = make_service(monkeypatch)
    service.start()
    assert pumping.wait(2.0)
    service.start()
    try:
        assert len(service.threads) == 3
        assert len(registered) == len(Service.HOTKEY_HANDLERS)
    finally:
        service.stop()


def test_a_hotkey_callback_posts_and_the_input_worker_runs_it(monkeypatch):
    done = threading.Event()
    service, ran, _, _, pumping = make_service(monkeypatch)
    service.daemon.run_job = lambda job, control: (ran.append(job.goal), done.set())
    service.start()
    try:
        assert pumping.wait(2.0)
        keys = service.daemon.keys
        keys.dispatch(keys.id_of("goal"))  # as the pump would, on the pumping thread
        assert done.wait(2.0), "the input worker must pick the posted handler up"
        assert ran == ["open the console"]
    finally:
        service.stop()


def test_the_service_join_returns_once_the_daemon_quits(monkeypatch):
    service, _, _, _, pumping = make_service(monkeypatch)
    service.start()
    try:
        assert pumping.wait(2.0)
        service.join(timeout=0.3)  # still running: the timeout is what ends the wait
        assert not service.daemon.stopped.is_set()
        service.daemon.on_quit()
        service.join(timeout=2.0)
        assert service.daemon.stopped.is_set()
    finally:
        service.stop()


def test_a_service_without_hotkeys_still_runs_its_two_workers(monkeypatch):
    from typesafe_computer_use_win.daemon import Service

    daemon, _ = make_daemon()
    service = Service(daemon, log=lambda *a: None)
    service.start()
    try:
        assert sorted(t.name for t in service.threads) == ["input", "runs"]
    finally:
        service.stop()
    for thread in service.threads:
        assert not thread.is_alive()


# ------------------------------------------------------------------ act / voice toggles


def test_a_queued_goal_carries_the_act_flag_the_daemon_is_set_to():
    daemon, _ = make_daemon()
    daemon.act = False
    daemon.queue_goal("look but do not touch")
    assert daemon.jobs.get_nowait() == Job("look but do not touch", act=False)


def test_a_job_defaults_to_acting_so_the_headless_daemon_is_unchanged():
    daemon, _ = make_daemon()
    daemon.queue_goal("go")
    assert daemon.jobs.get_nowait().act is True


def test_turning_voice_off_stops_the_talk_hotkey_reaching_the_microphone():
    daemon, _ = make_daemon()
    daemon.voice_enabled = False

    def recorded():
        raise AssertionError("voice is off, so nothing should have been recorded")

    daemon.listen_now = recorded
    daemon.on_talk()
    assert daemon.jobs.empty()


def test_probe_voice_reports_the_verdict_without_queueing_or_flipping_anything():
    daemon, _ = make_daemon(intent=Intent("run_goal", "open the console", 0.9, 0.9, "open the console", 0.55))
    logged = []
    daemon.log = logged.append
    decided = daemon.probe_voice()
    assert decided.command == "run_goal"
    assert daemon.jobs.empty(), "a test must never start a run"
    assert daemon.control.aborting is False and daemon.control.paused is False
    assert any("would act" in line for line in logged)


def test_probe_voice_says_when_a_line_would_be_ignored():
    daemon, _ = make_daemon(intent=Intent("ignore", "", 0.9, 0.9, "lunch?", 0.55))
    logged = []
    daemon.log = logged.append
    daemon.probe_voice()
    assert any("would be ignored" in line for line in logged)
    assert daemon.jobs.empty()


def test_probe_voice_survives_a_missing_microphone():
    from typesafe_computer_use_win.voice import VoiceUnavailable

    logged = []
    daemon, _ = make_daemon()
    daemon.log = logged.append
    daemon.listen_now = lambda: (_ for _ in ()).throw(VoiceUnavailable("no microphone"))
    assert daemon.probe_voice() is None
    assert any("no microphone" in line for line in logged)


def test_the_bar_hotkey_queues_what_was_said_or_typed():
    """Right alt opens the bar; whether the words were spoken into it or typed, the daemon sees
    the same thing: a goal."""
    daemon, _ = make_daemon()
    daemon.ask_goal = lambda: "open youtube"
    daemon.on_bar()
    assert daemon.jobs.get_nowait().goal == "open youtube"


def test_every_hotkey_in_the_table_has_a_handler():
    """A name in the table with no on_<name> would fail at registration, in a thread, at startup."""
    daemon, _ = make_daemon()
    for name in Service.HOTKEY_HANDLERS:
        assert callable(getattr(daemon, f"on_{name}", None)), f"no handler for {name}"


# ------------------------------------------------------------------ talk mode


def test_a_question_is_answered_and_never_run():
    """The whole value of talk mode is that it cannot act, so this pins it at the daemon too."""
    from typesafe_computer_use_win.intent import Intent

    daemon, ran = make_daemon(intent=Intent("ask_screen", "", 0.95, 0.9, "what does this say", 0.55))
    daemon.writer = object()
    answered = []
    daemon.answer = lambda question: answered.append(question) or "it says hello"
    daemon.on_talk()
    assert answered == ["what does this say"]
    assert daemon.jobs.empty() and ran == [], "a question must not start a run"


def test_a_question_without_a_writer_says_so_rather_than_failing():
    daemon, _ = make_daemon()
    daemon.writer = None
    logged = []
    daemon.log = logged.append
    assert daemon.answer("what is this") == ""
    assert any("no writer" in line for line in logged)


def test_an_answer_is_spoken_when_speaking_is_on(monkeypatch):
    from typesafe_computer_use_win import speech

    said = []
    monkeypatch.setenv("CLICKER_SPEAK", "1")
    monkeypatch.setattr(speech, "speak", lambda text: said.append(text) or True)
    monkeypatch.setattr(speech, "speakable", lambda text: text[:50])
    daemon, _ = make_daemon()
    daemon.say("the billing page shows the Starter plan")
    assert said == ["the billing page shows the Starter plan"]


def test_speaking_can_be_turned_off(monkeypatch):
    from typesafe_computer_use_win import speech

    monkeypatch.setenv("CLICKER_SPEAK", "0")

    def spoke(_text):
        raise AssertionError("speaking is off, so nothing should reach the engine")

    monkeypatch.setattr(speech, "speak", spoke)
    daemon, _ = make_daemon()
    daemon.say("something")


def test_a_speech_engine_that_fails_does_not_take_the_answer_with_it(monkeypatch):
    from typesafe_computer_use_win import speech

    monkeypatch.setenv("CLICKER_SPEAK", "1")
    monkeypatch.setattr(speech, "speak", lambda text: (_ for _ in ()).throw(RuntimeError("no audio device")))
    daemon, _ = make_daemon()
    daemon.say("still fine")
