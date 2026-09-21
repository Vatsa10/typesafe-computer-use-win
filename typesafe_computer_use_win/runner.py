"""The step loop and the run folder."""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from pathlib import Path
from threading import Event

import anthropic
from typesafe_sdk import TypeSafeClient

from . import windows
from .actions import Context, is_noop, perform
from .config import DEFAULT_DELAY, DEFAULT_MIN_CONFIDENCE, DEFAULT_STEPS, MAX_OPTIONS
from .decide import Decision, decide, offscreen_records
from .models import Abort, Item, Screen
from .perception import OcrCache, capture, perceive
from .report import Log, annotate, ax_count, render_payload, top
from .timing import format_timing, phase, summarize
from .writer import Answer, compose_answer

MAX_CONSECUTIVE_NOOPS = 2

# The outcomes that end with an answer, each in words the writer can pass on. A dry run took no
# action and an abort is the user's own stop, so neither has anything to report.
STOPPED = {
    "done": "the classifier judged the goal already achieved on this screen",
    "nothing helps": "the classifier found nothing on this screen that helps with the goal",
    "low confidence": "the classifier was not confident enough in any next action",
    "stalled": "the last actions changed nothing",
    "step limit": "the run used every step it was allowed",
}


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


def _watch(control: Control) -> Callable[[], None]:
    """The interrupt check the wait polls: the corner escape hatch, plus pause and abort."""

    def check() -> None:
        windows.check_abort()
        control.checkpoint()

    return check


@dataclass
class RunConfig:
    goal: str
    out: Path
    act: bool = False
    steps: int = DEFAULT_STEPS
    min_confidence: float = DEFAULT_MIN_CONFIDENCE
    delay: float = DEFAULT_DELAY
    image: Path | None = None  # replay a saved capture (never acts)
    app: str | None = None  # frontmost app to report during replay
    url: str | None = None  # browser URL to report during replay

    @property
    def replay(self) -> bool:
        return self.image is not None


@dataclass
class RunState:
    history: list[str] = field(default_factory=list)
    timings: list[dict[str, float]] = field(default_factory=list)
    consecutive_noops: int = 0
    last_url: str | None = None
    outcome: str = "crashed"  # every way out of the loop names its own; only an exception leaves this
    ocr_cache: OcrCache = field(default_factory=OcrCache)  # carries one step's OCR into the next
    view: tuple[Screen, list[Item]] | None = None  # the latest capture, until an action makes it stale
    answer: Answer | None = None


def run(cfg: RunConfig, ctx_factory, control: Control | None = None, echo=print) -> RunState:
    """Drive the loop. ctx_factory(typesafe, history) builds the action Context.

    `control` lets a caller pause or abort between steps; without one the loop runs to its own
    stop rules and only the corner escape hatch interrupts it.

    `echo` receives every line the loop logs, so a UI can show a live step feed while the same
    lines still reach stdout and the run folder. It stays a parameter rather than a RunConfig
    field, because RunConfig is serialised into run.json with `asdict()`.
    """
    cfg.out.mkdir(parents=True, exist_ok=True)
    log = Log(cfg.out / "run.log", echo)
    log(f"run folder: {cfg.out}")
    if cfg.act:
        log("driving the machine. abort: Ctrl-C, or slam the mouse into the top-left corner.")

    state = RunState()
    started = time.time()
    try:
        with TypeSafeClient() as typesafe:
            ctx = ctx_factory(typesafe, state.history)
            for step in range(1, cfg.steps + 1):
                if not run_step(cfg, ctx, state, step, log, control):
                    break
            else:
                log(f"\nstopped after {cfg.steps} steps")
                state.outcome = "step limit"
            conclude(cfg, ctx, state, log)
    except (KeyboardInterrupt, Abort) as e:
        state.outcome = f"aborted ({e or 'Ctrl-C'})"
        log(f"\n{state.outcome} after {len(state.history)} actions")
    finally:
        summary = {
            "goal": cfg.goal,
            "act": cfg.act,
            "steps_taken": len(state.history),
            "outcome": state.outcome,
            "answer": state.answer.text if state.answer else None,
            "goal_achieved": state.answer.achieved if state.answer else None,
            "seconds": round(time.time() - started, 1),
            "timing": summarize(state.timings),
            "history": state.history,
            "config": {k: str(v) for k, v in asdict(cfg).items()},
        }
        (cfg.out / "run.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
        log(f"run folder: {cfg.out}")
    return state


def conclude(cfg: RunConfig, ctx: Context, state: RunState, log: Log) -> None:
    """Hand the screen the run ended on to the writer, for the answer the classifier cannot put into words.

    The last step's capture serves when nothing acted after it. An action makes it stale, so the
    screen is captured again, and saved so the answer can be checked against what it was read from.
    """
    stopped = STOPPED.get(state.outcome)
    if stopped is None:
        return
    if ctx.writer is None:
        log("\nno answer: the writer is disabled (set ANTHROPIC_API_KEY)")
        return
    started = time.perf_counter()
    if state.view is None:
        windows.check_abort()
        screen = capture(cfg.image, cfg.app, cfg.url, ctx.browser)
        screen.image.save(cfg.out / "answer-raw.png")
        state.view = (screen, perceive(screen, MAX_OPTIONS, cfg.goal))
    screen, items = state.view
    try:
        state.answer = compose_answer(ctx.writer, cfg.goal, screen, items, state.history, stopped)
    except anthropic.APIError as e:
        log(f"\nno answer: the writer failed ({e})")
        return
    verdict = "goal achieved" if state.answer.achieved else "goal not achieved"
    log(f"\nanswer ({verdict}, {time.perf_counter() - started:.1f}s):\n  {state.answer.text}")


def run_step(cfg: RunConfig, ctx: Context, state: RunState, step: int, log: Log, control: Control | None = None) -> bool:
    windows.check_abort()
    if control is not None:
        control.checkpoint()
    timing: dict[str, float] = {}
    started = time.perf_counter()
    with phase(timing, "capture"):
        screen = capture(cfg.image, cfg.app, cfg.url, ctx.browser, timing)
    items = perceive(screen, MAX_OPTIONS, cfg.goal, timing, None if cfg.replay else state.ocr_cache)
    state.view = (screen, items)
    prefix = cfg.out / f"step-{step:03d}"  # three digits, so a run of 100 steps still lists in order
    screen.image.save(prefix.with_name(prefix.name + "-raw.png"))
    prefix.with_name(prefix.name + "-payload.txt").write_text(
        render_payload(cfg.goal, screen, items, state.history, ctx.browser, ctx.email), encoding="utf-8"
    )

    with phase(timing, "decide"):
        decision = decide(ctx.typesafe, cfg.goal, screen, items, state.history, ctx.browser, ctx.email)
    by_index = {str(it.index): it for it in items}
    annotate(screen, items, decision.chosen, prefix.with_suffix(".png"))

    field_desc = f" field={screen.field.role}:{screen.field.label!r}" if screen.field else ""
    log(
        f"\nstep {step}: app={screen.app!r}{field_desc} url={screen.url!r} items={len(items)} ax={ax_count(items)} "
        f"offscreen={len(screen.offscreen)} kind={decision.kind.choice} ({decision.kind.confidence:.2f}) "
        f"site={decision.site.choice}"
    )
    for key, p in top(decision.kind, 4):
        log(f"  {p:5.2f}  {key}")
    if decision.item is not None:
        log(f"  item ({decision.item.confidence:.2f}):")
        for key, p in top(decision.item, 4):
            log(f"  {p:5.2f}  [{key}] {by_index[key].text!r}")
    if decision.offscreen is not None:
        log(f"  offscreen ({decision.offscreen.confidence:.2f}):")
        for key, p in top(decision.offscreen, 3):
            log(f"  {p:5.2f}  [{key}] {screen.offscreen[int(key)].label!r}")

    keep_going = resolve(cfg, ctx, state, screen, items, decision, timing, log)
    timing.setdefault("act", 0.0)
    timing["total"] = round(time.perf_counter() - started, 3)
    state.timings.append(timing)

    prefix.with_name(prefix.name + "-answers.json").write_text(
        json.dumps(answers(decision, screen, items, timing), indent=2), encoding="utf-8"
    )
    log(f"  files: {prefix.name}-raw.png, {prefix.name}.png, {prefix.name}-payload.txt, {prefix.name}-answers.json")
    log(format_timing(timing))

    if state.view is None:  # an action ran: let the screen settle before the next step, or the answer, reads it
        windows.sleep_watching(cfg.delay, check=None if control is None else _watch(control))
    return keep_going


def resolve(
    cfg: RunConfig,
    ctx: Context,
    state: RunState,
    screen: Screen,
    items: list[Item],
    decision: Decision,
    timing: dict[str, float],
    log: Log,
) -> bool:
    """Apply the stop rules, then the action. True to keep looping."""
    if decision.stops:
        log(f"  model says {decision.kind.choice!r}; stopping")
        state.outcome = "done" if decision.kind.choice == "done" else "nothing helps"
        return False
    if decision.confidence < cfg.min_confidence:
        log(f"  confidence {decision.confidence:.2f} below {cfg.min_confidence}; stopping")
        state.outcome = "low confidence"
        return False
    if not cfg.act or cfg.replay:
        log(f"  would do: {decision.chosen}. dry run (pass --act without --image to drive the machine)")
        state.outcome = "dry run"
        return False

    with phase(timing, "act"):
        what = perform(decision, screen, items, ctx)
    state.view = None
    repeated = bool(state.history) and state.history[-1] == what and screen.url == state.last_url
    state.last_url = screen.url
    state.history.append(what)
    log(f"  did: {what}")
    if is_noop(what) or repeated:
        state.consecutive_noops += 1
        if state.consecutive_noops >= MAX_CONSECUTIVE_NOOPS:
            log(f"  {MAX_CONSECUTIVE_NOOPS} consecutive no-ops; stopping")
            state.outcome = "stalled"
            return False
    else:
        state.consecutive_noops = 0
    return True


def answers(decision: Decision, screen: Screen, items: list[Item], timing: dict[str, float]) -> dict:
    """What the classifier returned for this step, plus what it cost."""
    return {
        "kind": decision.kind.choice,
        "kind_confidence": decision.kind.confidence,
        "kind_probabilities": decision.kind.probabilities,
        "item": decision.item.choice if decision.item else None,
        "item_confidence": decision.item.confidence if decision.item else None,
        "item_probabilities": decision.item.probabilities if decision.item else None,
        "site": decision.site.choice,
        "site_probabilities": decision.site.probabilities,
        "offscreen": decision.offscreen.choice if decision.offscreen else None,
        "offscreen_probabilities": decision.offscreen.probabilities if decision.offscreen else None,
        "offscreen_controls": offscreen_records(screen.offscreen),
        "chosen": decision.chosen,
        "confidence": decision.confidence,
        "timing": timing,
        "items": [asdict(it) for it in items],
        "field": screen.field.record() if screen.field else None,
        "app": screen.app,
        "url": screen.url,
    }
