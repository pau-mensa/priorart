"""Candidate generators for the lateweave pipeline.

``ExhaustiveGatherer`` hands every eligible document to the reranker, which at
small scale makes the search exact MaxSim over the corpus. ``LexicalGatherer``
is bm25s over the same documents, used once the corpus exceeds the gather
limit. Both consume only query text and honour ``subset``.
"""

from __future__ import annotations

from collections.abc import Sequence

import numpy as np
from lateweave import Candidate, CorpusManifest, Query, Representation

from .analyzer import tokens

LEXICAL_METHOD = "lucene"
LEXICAL_K1 = 1.5
LEXICAL_B = 0.75


class ExhaustiveGatherer:
    requires: dict[str, Representation] = {}
    score_semantics = "exhaustive"

    def __init__(self, corpus: CorpusManifest) -> None:
        self.corpus = corpus

    def gather(
        self, query: Query, limit: int, *, subset: np.ndarray | None = None
    ) -> tuple[Candidate, ...]:
        ids = range(self.corpus.document_count) if subset is None else subset.tolist()
        return tuple(
            Candidate(int(document_id), 0.0, rank, "exhaustive")
            for rank, document_id in enumerate(list(ids)[:limit])
        )


class LexicalGatherer:
    requires: dict[str, Representation] = {}
    score_semantics = "bm25s-lucene"

    def __init__(self, corpus: CorpusManifest, texts: Sequence[str]) -> None:
        import bm25s

        if len(texts) != corpus.document_count:
            raise ValueError("lexical texts do not match the corpus manifest")
        self.corpus = corpus
        self.index = bm25s.BM25(method=LEXICAL_METHOD, k1=LEXICAL_K1, b=LEXICAL_B)
        self.index.index([tokens(text) for text in texts], show_progress=False)

    def gather(
        self, query: Query, limit: int, *, subset: np.ndarray | None = None
    ) -> tuple[Candidate, ...]:
        terms = tokens(query.text)
        if not terms or self.corpus.document_count == 0:
            return ()
        weight_mask = None
        if subset is not None:
            if len(subset) == 0:
                return ()
            weight_mask = np.zeros(self.corpus.document_count, dtype=np.float32)
            weight_mask[subset] = 1.0
        documents, scores = self.index.retrieve(
            [terms],
            k=min(limit, self.corpus.document_count),
            show_progress=False,
            weight_mask=weight_mask,
        )
        candidates: list[Candidate] = []
        seen: set[int] = set()
        for raw_id, raw_score in zip(documents[0].tolist(), scores[0].tolist(), strict=True):
            score = float(raw_score)
            if score != score or score <= 0.0:
                continue
            document_id = int(raw_id)
            if document_id in seen:
                raise RuntimeError(f"bm25s returned duplicate ID {document_id}")
            seen.add(document_id)
            candidates.append(Candidate(document_id, score, len(candidates), "bm25s"))
        return tuple(candidates)


def choose_gatherer(
    corpus: CorpusManifest,
    texts: Sequence[str],
    eligible_count: int,
    gather_limit: int,
) -> ExhaustiveGatherer | LexicalGatherer:
    if eligible_count <= gather_limit:
        return ExhaustiveGatherer(corpus)
    return LexicalGatherer(corpus, texts)
