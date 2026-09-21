"""The Windows OCR adapter: engine choice and the shape each engine's lines come back in."""

from __future__ import annotations

import sys

import pytest
from PIL import Image

if sys.platform != "win32":
    pytest.skip("the Windows adapter only imports on Windows", allow_module_level=True)

from typesafe_computer_use_win import windows


def test_an_unknown_engine_name_is_refused_rather_than_silently_ignored(monkeypatch):
    monkeypatch.setattr(windows, "OCR_ENGINE", "tesseract")
    with pytest.raises(ValueError, match="tesseract"):
        windows.ocr_recognize(Image.new("RGB", (10, 10)))


def test_rapidocr_lines_keep_their_score_and_reduce_a_quad_to_its_bounding_box(monkeypatch):
    quad = [[12.0, 20.0], [60.0, 18.0], [61.0, 44.0], [13.0, 46.0]]  # slightly rotated text
    monkeypatch.setattr(windows, "OCR_ENGINE", "rapidocr")
    monkeypatch.setattr(windows, "_rapidocr_engine", lambda: lambda array: ([(quad, "hello", 0.87)], 0.1))
    assert windows.ocr_recognize(Image.new("RGB", (100, 80))) == [("hello", 0.87, (12.0, 18.0, 61.0, 46.0))]


def test_rapidocr_reading_nothing_is_no_lines_rather_than_a_crash(monkeypatch):
    monkeypatch.setattr(windows, "OCR_ENGINE", "rapidocr")
    monkeypatch.setattr(windows, "_rapidocr_engine", lambda: lambda array: (None, 0.1))
    assert windows.ocr_recognize(Image.new("RGB", (10, 10))) == []


def test_the_windows_engine_scores_every_line_it_returns_the_same(monkeypatch):
    """The engine reports no confidence, so the number is a constant the threshold always clears."""
    from typesafe_computer_use_win.config import MIN_OCR_CONFIDENCE

    assert windows.WINDOWS_OCR_CONFIDENCE > MIN_OCR_CONFIDENCE
