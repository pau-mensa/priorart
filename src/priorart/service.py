"""The five operations, independent of transport."""

from __future__ import annotations

import re
import threading
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any

from .analyzer import tokens
from .config import Settings
from .encoder import Encoder, load_encoder
from .excerpt import excerpt
from .index import Index
from .store import Report, Revision, Store

RECORD_ID = re.compile(r"^[A-Za-z0-9_.:-]{1,128}$")
MAX_LIMIT = 100
_SCALAR = (str, int, float, bool)


class InvalidInput(ValueError):
    """The request is well-formed but violates a protocol rule."""


@dataclass(frozen=True)
class Hit:
    id: str
    revision: int
    score: float
    score_semantics: str
    excerpt: str
    metadata: dict[str, Any] | None


@dataclass(frozen=True)
class SearchOutcome:
    search_id: str
    hits: list[Hit]
    timings: dict[str, float]
    gatherer: str


class Service:
    def __init__(self, settings: Settings, encoder: Encoder | None) -> None:
        self.settings = settings
        settings.data_dir.mkdir(parents=True, exist_ok=True)
        self.store = Store(settings.data_dir / "priorart.sqlite")
        self.encoder = encoder
        self.index = Index(settings.data_dir, self.store, encoder)
        self._lock = threading.Lock()

    @classmethod
    def open(cls, settings: Settings) -> Service:
        return cls(settings, load_encoder(settings))

    def close(self) -> None:
        self.store.close()

    # -- records ----------------------------------------------------------------

    def put(
        self,
        text: str,
        metadata: Mapping[str, Any] | None = None,
        record_id: str | None = None,
    ) -> tuple[str, int]:
        if not isinstance(text, str) or not text.strip():
            raise InvalidInput("text must be a non-empty string")
        size = len(text.encode("utf-8"))
        if size > self.settings.max_text_bytes:
            raise InvalidInput(
                f"text is {size:,} bytes; the limit is {self.settings.max_text_bytes:,}"
            )
        if metadata is not None and not isinstance(metadata, Mapping):
            raise InvalidInput("metadata must be a JSON object")
        if record_id is not None and not RECORD_ID.match(record_id):
            raise InvalidInput("id must match ^[A-Za-z0-9_.:-]{1,128}$")
        with self._lock:
            record_id, revision = self.store.put(text, metadata, record_id)
            self.index.upsert(record_id, revision, text)
        return record_id, revision

    def get(self, record_id: str, revision: int | None = None) -> Revision:
        return self.store.get(record_id, revision)

    def delete(self, record_id: str) -> None:
        with self._lock:
            if self.store.delete(record_id):
                self.index.remove(record_id)

    # -- search -----------------------------------------------------------------

    def search(
        self,
        text: str,
        filters: Mapping[str, Any] | None = None,
        limit: int = 10,
    ) -> SearchOutcome:
        if not isinstance(text, str) or not text.strip():
            raise InvalidInput("query text must be a non-empty string")
        if not isinstance(limit, int) or not 1 <= limit <= MAX_LIMIT:
            raise InvalidInput(f"limit must be between 1 and {MAX_LIMIT}")
        if filters is not None:
            if not isinstance(filters, Mapping):
                raise InvalidInput("filters must be a JSON object")
            for key, value in filters.items():
                if not isinstance(value, _SCALAR):
                    raise InvalidInput(f"filter {key!r} must be a string, number, or boolean")
        filters = None if not filters else dict(filters)

        hits: list[Hit] = []
        timings: dict[str, float] = {}
        gatherer = "none"
        with self._lock:
            subset = None
            eligible = self.index.document_count
            if filters is not None:
                subset = self.index.internal_ids(self.store.matching_record_ids(filters))
                eligible = len(subset)
            if eligible > 0:
                gather_limit = max(self.settings.gather_limit, limit)
                pipeline = self.index.pipeline(eligible, gather_limit)
                result = pipeline.search(
                    self.index.query(text), gather_limit=gather_limit, limit=limit, subset=subset
                )
                gatherer = pipeline.gatherer.score_semantics.split("-")[0]
                timings = {
                    "gather_seconds": result.timings.gather_seconds,
                    "rerank_seconds": result.timings.rerank_seconds,
                    "total_seconds": result.timings.total_seconds,
                }
                query_tokens = tokens(text)
                semantics = str(result.diagnostics["score_semantics"])
                for ranked in result.documents:
                    record_id = self.index.record_ids[ranked.document_id]
                    revision = self.store.get(record_id, self.index.revisions[ranked.document_id])
                    hits.append(
                        Hit(
                            id=record_id,
                            revision=revision.revision,
                            score=float(ranked.score),
                            score_semantics=semantics,
                            excerpt=excerpt(revision.text or "", query_tokens),
                            metadata=revision.metadata,
                        )
                    )
            search_id = self.store.log_search(
                text,
                filters,
                [{"id": hit.id, "revision": hit.revision, "score": hit.score} for hit in hits],
                timings,
            )
        return SearchOutcome(search_id=search_id, hits=hits, timings=timings, gatherer=gatherer)

    # -- reports ----------------------------------------------------------------

    def report(
        self,
        record_id: str,
        text: str,
        revision: int | None = None,
        search_id: str | None = None,
    ) -> str:
        if not isinstance(text, str) or not text.strip():
            raise InvalidInput("report text must be a non-empty string")
        return self.store.add_report(record_id, revision, search_id, text)

    def reports(self, record_id: str) -> list[Report]:
        return self.store.reports_for(record_id)

    # -- health -----------------------------------------------------------------

    def health(self) -> dict[str, Any]:
        return {
            "status": "ok",
            "document_count": self.index.document_count,
            "encoder": None if self.encoder is None else self.encoder.representation.encoder,
            "gather_limit": self.settings.gather_limit,
        }
