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
