from __future__ import annotations

import hashlib
from collections.abc import Sequence

import numpy as np
from lateweave import Representation

from priorart.analyzer import tokens


class FakeEncoder:
    """Deterministic token vectors from hashed terms; enough for MaxSim to have signal."""

    def __init__(self, dimension: int = 16, name: str = "fake") -> None:
        self.representation = Representation(
            encoder=name, encoder_revision="test", dimension=dimension, normalized=True
        )
        self.calls = 0

    def _vector(self, term: str) -> np.ndarray:
        seed = int.from_bytes(hashlib.sha256(term.encode()).digest()[:8], "little")
        vector = np.random.default_rng(seed).standard_normal(self.representation.dimension)
        return (vector / np.linalg.norm(vector)).astype(np.float32)

    def _encode(self, texts: Sequence[str]) -> list[np.ndarray]:
        self.calls += 1
        output = []
        for text in texts:
            terms = tokens(text) or ["<empty>"]
            output.append(np.stack([self._vector(term) for term in terms]))
        return output

    def encode_queries(self, texts: Sequence[str]) -> list[np.ndarray]:
        return self._encode(texts)

    def encode_documents(self, texts: Sequence[str]) -> list[np.ndarray]:
        return self._encode(texts)
