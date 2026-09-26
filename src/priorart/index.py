"""The retrieval index: vector store, lexical index, and corpus identity.

Documents are the latest live revision of every non-deleted record, one
document per record. Internal IDs are dense and mirrored in the store's
``index_documents`` table; on open the mirror, the live records, and the vector
store are checked against each other and the index is rebuilt from the records
when they disagree. That is how a crash between the two writes recovers.

Zero documents means no ``vectors/`` directory: lateweave stores cannot be
empty, so absence is the empty state.
"""

from __future__ import annotations

import shutil
from collections.abc import Iterable
from pathlib import Path

import numpy as np
from lateweave import (
    CorpusManifest,
    Feature,
    Int8VectorStore,
    MaxSimReranker,
    Query,
    SearchPipeline,
    document_ids_digest,
    open_vector_store,
)

from .encoder import Encoder, pack
from .gather import LexicalGatherer, choose_gatherer
from .store import LOCAL_COLLECTION_ID, Store

VECTORS_DIRECTORY = "vectors"
ENCODE_BATCH = 32


class Index:
    def __init__(
        self, data_dir: str | Path, store: Store, encoder: Encoder | None, *, collection_id: str
    ) -> None:
        if collection_id != LOCAL_COLLECTION_ID:
            raise ValueError(
                "only the local collection can be indexed until index isolation exists"
            )
        self.collection_id = collection_id
        self.data_dir = Path(data_dir)
        self.vectors_path = self.data_dir / VECTORS_DIRECTORY
        self.store = store
        self.encoder = encoder
        self.vectors: Int8VectorStore | None = None
        self.record_ids: list[str] = []
        self.revisions: list[int] = []
        self.texts: list[str] = []
        self.generation = 0
        self.manifest: CorpusManifest | None = None
        self._lexical: LexicalGatherer | None = None
        self.open()

    # -- lifecycle --------------------------------------------------------------

    @property
    def document_count(self) -> int:
        return len(self.record_ids)

    def open(self) -> None:
        live = {
            doc.record_id: doc
            for doc in self.store.live_documents(collection_id=self.collection_id)
        }
        mirror = self.store.index_documents(collection_id=self.collection_id)
        consistent = {(record_id, revision) for _, record_id, revision in mirror} == {
            (doc.record_id, doc.revision) for doc in live.values()
        } and [row[0] for row in mirror] == list(range(len(mirror)))
        if consistent and self.encoder is not None:
            consistent = self._vectors_match(len(mirror))
        if not consistent:
            self.rebuild()
            return
        self.record_ids = [record_id for _, record_id, _ in mirror]
        self.revisions = [revision for _, _, revision in mirror]
        self.texts = [live[record_id].text or "" for record_id in self.record_ids]
        if self.encoder is not None and self.record_ids:
            self.vectors = open_vector_store(self.vectors_path)  # type: ignore[assignment]
        else:
            self.vectors = None
        self._refresh()

    def _vectors_match(self, expected_count: int) -> bool:
        assert self.encoder is not None
        if expected_count == 0:
            return not self.vectors_path.exists()
        if not self.vectors_path.exists():
            return False
        try:
            store = open_vector_store(self.vectors_path)
        except (OSError, ValueError, KeyError):
            return False
        return (
            isinstance(store, Int8VectorStore)
            and store.document_count == expected_count
            and store.representation == self.encoder.representation
        )

    def rebuild(self) -> None:
        documents = self.store.live_documents(collection_id=self.collection_id)
        self._drop_vectors()
        self.record_ids = [doc.record_id for doc in documents]
        self.revisions = [doc.revision for doc in documents]
        self.texts = [doc.text or "" for doc in documents]
        if self.encoder is not None and documents:
            embeddings, lengths = pack(self._encode(self.texts))
            self.vectors = Int8VectorStore.create(  # type: ignore[assignment]
                self.vectors_path, embeddings, lengths, self.encoder.representation
            )
        self._write_mirror()
        self._refresh()

    # -- mutation ---------------------------------------------------------------

    def upsert(self, record_id: str, revision: int, text: str) -> None:
        if record_id in self._positions():
            self._remove_position(self._positions()[record_id])
        if self.encoder is not None:
            embeddings, lengths = pack(self._encode([text]))
            if self.vectors is None:
                self._drop_vectors()
                self.vectors = Int8VectorStore.create(  # type: ignore[assignment]
                    self.vectors_path, embeddings, lengths, self.encoder.representation
                )
            else:
                self.vectors.append(embeddings, lengths)
        self.record_ids.append(record_id)
        self.revisions.append(revision)
        self.texts.append(text)
        self._write_mirror()
        self._refresh()

    def remove(self, record_id: str) -> None:
        position = self._positions().get(record_id)
        if position is None:
            return
        self._remove_position(position)
        self._write_mirror()
        self._refresh()

    def _remove_position(self, position: int) -> None:
        if self.vectors is not None:
            if self.vectors.document_count == 1:
                self._drop_vectors()
            else:
                self.vectors.delete([position])
        del self.record_ids[position]
        del self.revisions[position]
        del self.texts[position]

    def _drop_vectors(self) -> None:
        self.vectors = None
        if self.vectors_path.exists():
            shutil.rmtree(self.vectors_path)

    def _encode(self, texts: list[str]) -> list[np.ndarray]:
        assert self.encoder is not None
        output: list[np.ndarray] = []
        for start in range(0, len(texts), ENCODE_BATCH):
            output.extend(self.encoder.encode_documents(texts[start : start + ENCODE_BATCH]))
        return output

    def _write_mirror(self) -> None:
        encoder = None if self.encoder is None else self.encoder.representation.encoder
        self.store.replace_index_documents(
            list(zip(self.record_ids, self.revisions, strict=True)),
            encoder,
            collection_id=self.collection_id,
        )

    def _refresh(self) -> None:
        self.generation += 1
        self._lexical = None
        if not self.record_ids:
            self.manifest = None
            return
        self.manifest = CorpusManifest(
            corpus_id="priorart",
            corpus_version=str(self.data_dir),
            document_count=len(self.record_ids),
            document_ids_sha256=document_ids_digest(self.record_ids),
            generation=self.generation,
        )

    # -- search -----------------------------------------------------------------

    def _positions(self) -> dict[str, int]:
        return {record_id: position for position, record_id in enumerate(self.record_ids)}

    def internal_ids(self, record_ids: Iterable[str]) -> np.ndarray:
        positions = self._positions()
        found = sorted(positions[item] for item in record_ids if item in positions)
        return np.asarray(found, dtype=np.int64)

    def query(self, text: str) -> Query:
        if self.encoder is None:
            return Query(text)
        encoder = self.encoder
        return Query(
            text,
            multi_vector=Feature(
                encoder.representation, provider=lambda: encoder.encode_queries([text])[0]
            ),
        )

    def pipeline(self, eligible_count: int, gather_limit: int) -> SearchPipeline:
        if self.manifest is None:
            raise RuntimeError("the index is empty")
        if self.encoder is None or eligible_count > gather_limit:
            if self._lexical is None:
                self._lexical = LexicalGatherer(self.manifest, self.texts)
            gatherer = self._lexical
        else:
            gatherer = choose_gatherer(self.manifest, self.texts, eligible_count, gather_limit)
        reranker = None
        if self.vectors is not None:
            reranker = MaxSimReranker(self.vectors, self.manifest)
        return SearchPipeline(gatherer, reranker)
