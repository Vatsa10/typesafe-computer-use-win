"""Read and rewrite a ``.env`` file in place, preserving everything we were not asked to change.

The parsing rules here mirror :func:`typesafe_computer_use_win.config.load_dotenv` exactly:
``KEY=VALUE`` lines, ``#`` comment lines, surrounding quotes stripped, whitespace stripped.
"""

from __future__ import annotations

from pathlib import Path

_QUOTES = "'\""
# Characters that make a bare value ambiguous or lossy once ``load_dotenv`` strips it.
_NEEDS_QUOTING = (" ", "\t", "#", "'", '"')


def _split(line: str) -> tuple[str, str] | None:
    """Return ``(key, value)`` for a data line, or ``None`` for a comment/blank/non-assignment."""
    stripped = line.strip()
    if not stripped or stripped.startswith("#") or "=" not in stripped:
        return None
    key, value = stripped.split("=", 1)
    return key.strip(), value.strip().strip(_QUOTES)


def quote(value: str) -> str:
    """Render a value for the right-hand side of ``KEY=``.

    Wrapped in double quotes when it contains a space, tab, ``#`` or a quote character, so that
    ``load_dotenv`` reads back exactly what was written. A value that itself begins or ends with a
    quote character is written bare: ``load_dotenv`` strips such characters either way, so adding
    wrapping quotes would only lose more.
    """
    if not value:
        return ""
    if value[0] in _QUOTES or value[-1] in _QUOTES:
        return value
    if any(char in value for char in _NEEDS_QUOTING):
        return f'"{value}"'
    return value


def merge_lines(existing: list[str], values: dict[str, str]) -> list[str]:
    """Return ``existing`` with ``values`` applied: known keys rewritten in place, new keys appended.

    Lines carry no line endings. Comments, blanks, ordering and any key absent from ``values``
    survive untouched. An empty value is written as ``KEY=`` rather than deleted.
    """
    remaining = dict(values)
    merged: list[str] = []
    for line in existing:
        parsed = _split(line)
        if parsed is not None and parsed[0] in remaining:
            key = parsed[0]
            merged.append(f"{key}={quote(remaining.pop(key))}")
        else:
            merged.append(line)

    if remaining:
        # Do not glue a new key onto trailing blank lines we would otherwise have kept.
        while merged and not merged[-1].strip():
            merged.pop()
        merged.extend(f"{key}={quote(value)}" for key, value in remaining.items())
    return merged


def read_env(path: Path) -> dict[str, str]:
    """Return the ``KEY=VALUE`` pairs in ``path``, or ``{}`` when the file is absent."""
    if not path.is_file():
        return {}
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        parsed = _split(line)
        if parsed is not None:
            values[parsed[0]] = parsed[1]
    return values


def write_env(path: Path, values: dict[str, str]) -> None:
    """Apply ``values`` to ``path``, leaving every other line of the file exactly as it was.

    Only the keys in ``values`` are ever written, so a secret that was not handed to us cannot be
    rewritten or lost. The file's existing line ending style is kept; a new file gets LF.
    """
    if path.is_file():
        # newline="" keeps the real line endings visible instead of translating them.
        raw = path.read_text(encoding="utf-8", newline="")
        newline = "\r\n" if "\r\n" in raw else "\n"
        existing = raw.splitlines()
    else:
        newline = "\n"
        existing = []

    merged = merge_lines(existing, values)
    text = newline.join(merged) + newline if merged else ""
    path.write_text(text, encoding="utf-8", newline="")
