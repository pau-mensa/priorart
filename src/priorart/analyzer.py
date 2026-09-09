"""Text to terms, identically for documents, queries, and excerpts.

Casefold, strip combining marks, take ``\\w+`` runs. No stemming, so the
chain is language-agnostic and needs no persisted state.
"""

from __future__ import annotations

import re
import unicodedata

_TOKEN = re.compile(r"\w+")


def tokens(text: str) -> list[str]:
    normalized = unicodedata.normalize("NFKD", text.casefold())
    folded = "".join(ch for ch in normalized if not unicodedata.combining(ch))
    return _TOKEN.findall(folded)
