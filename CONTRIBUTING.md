# Contributing

Bug reports with a run folder attached are the most useful thing you can send.
`runs/<timestamp>/` holds the capture, the exact payload, and every probability,
so a stall can be replayed offline with `--image`.

## Ground rules

- Keep the action set mutually exclusive. Two options that mean the same thing
  split the vote and read as low confidence.
- The classifier picks; code decides facts. Anything the model would have to
  compute (dates, URL validity, whether a field is focused) is computed in code
  and handed over as state.
- Free text only ever comes from `writer.py`, with a structured reply and a
  code-side guard.
- Platform calls live in `windows.py` only; everything else imports it as `from . import windows`.
  The tree walk in `axwalk.py` stays platform-free and is tested against a plain dict.
- Never add a path that types a password.
- A hotkey callback posts to a queue and returns. The thread that delivers a hotkey is the thread
  that delivers the next one, so any work done inside a callback stalls every other hotkey.
- Nothing from the `voice` extra is imported at module scope: CI installs the base set only.

## Before a pull request

```
uv run ruff check . && uv run ruff format .
uv run pytest -q
```

Add a replay-based note to the PR when a change alters what the model sees:
which run folder, which step, what the decision was before and after.
