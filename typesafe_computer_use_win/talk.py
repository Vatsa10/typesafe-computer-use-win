"""Talk mode: answer a question about the screen, and act on nothing.

This module is safe by construction, and that is the whole point of it. It looks at the screen and
it speaks; it never clicks, types, scrolls, switches windows or opens anything. Nothing here calls
`actions.perform`, `runner.run`, or any other function from `actions.py`, and nothing here may ever
be changed to do so: the user's reason for asking a question instead of giving a goal is that they
want to be told something, not to have their machine driven. A question that turns out to be a task
is the classifier's problem, not this path's -- `intent.ask_screen` decides which of the two a line
is, and this path only ever gets the questions.

So the whole of talk mode is one capture, one perception pass, one writer call, and a string.
"""

from __future__ import annotations

from .config import MAX_OPTIONS
from .perception import capture, perceive
from .writer import WriterUnavailable, compose_explanation


def answer_question(writer, question: str, browser: str = "", log=print) -> str:
    """Look at the screen, answer the question about it, and return what was said.

    A writer with no credit or a rejected key comes back as a sentence the user can read: talk mode
    is something a person is waiting on an answer from, and a traceback is not an answer.
    """
    screen = capture(browser=browser)
    items = perceive(screen, MAX_OPTIONS, question)
    try:
        answer = compose_explanation(writer, question, screen, items)
    except WriterUnavailable as e:
        answer = f"I could not answer that: the writer is unavailable ({e})."
    log(answer)
    return answer
