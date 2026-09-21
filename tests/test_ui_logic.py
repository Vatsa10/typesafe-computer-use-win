"""The parts of the panel that are logic rather than widgets."""

from __future__ import annotations

from typesafe_computer_use_win.ui import preview_factor, state_label


def test_a_full_display_capture_shrinks_to_fit_the_preview():
    assert preview_factor(2560, target=820) == 4  # 640 px wide, inside the pane
    assert 2560 // preview_factor(2560, target=820) <= 820


def test_an_image_already_small_enough_is_not_shrunk():
    assert preview_factor(640, target=820) == 1


def test_an_exact_fit_is_not_shrunk():
    assert preview_factor(820, target=820) == 1


def test_a_nonsense_width_still_gives_a_usable_factor():
    """Tk raises on subsample(0), so a corrupt image header must not produce one."""
    assert preview_factor(0) == 1
    assert preview_factor(-5) == 1


def test_paused_only_reads_as_paused_while_something_runs():
    assert state_label(running=True, paused=True) == "paused"
    assert state_label(running=True, paused=False) == "running"
    assert state_label(running=False, paused=True) == "idle"
    assert state_label(running=False, paused=False) == "idle"
