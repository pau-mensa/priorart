"""Query-aware excerpts chosen by lexical overlap; no model involved."""

from __future__ import annotations

from collections.abc import Sequence

from .analyzer import tokens


def excerpt(text: str, query_tokens: Sequence[str], width: int = 400) -> str:
    text = text.strip()
    if len(text) <= width:
        return text
    wanted = set(query_tokens)
    step = max(1, width // 4)
    best_start, best_hits = 0, -1
    for start in range(0, len(text) - width + step, step):
        start = min(start, len(text) - width)
        hits = sum(1 for term in tokens(text[start : start + width]) if term in wanted)
        if hits > best_hits:
            best_start, best_hits = start, hits
        if start == len(text) - width:
            break
    return _trim(text, best_start, width)


def _trim(text: str, start: int, width: int) -> str:
    end = min(len(text), start + width)
    if start > 0:
        cut = text.find(" ", start, end)
        if cut != -1 and cut - start < width // 4:
            start = cut + 1
    if end < len(text):
        cut = text.rfind(" ", start, end)
        if cut != -1 and end - cut < width // 4:
            end = cut
    piece = text[start:end].strip()
    return ("…" if start > 0 else "") + piece + ("…" if end < len(text) else "")
