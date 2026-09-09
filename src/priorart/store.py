"""SQLite persistence: records, revisions, reports, searches, index mirror.

A record is an id plus an append-only list of revisions. Deleting a record
nulls the text and metadata of every revision and stamps a tombstone; the id,
the revision numbers, and each revision's text digest remain, so reports stay
linked and the id cannot be reused.

``index_documents`` mirrors the vector store's internal-ID order. Row
``internal_id`` *is* store document ``internal_id``; the ``Index`` owns both
and this table is how it checks them against each other on open.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
import uuid
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


class RecordNotFound(KeyError):
    """No record, revision, or report target with that id."""


class RecordDeleted(KeyError):
    """The record exists only as a tombstone."""


class SearchNotFound(KeyError):
    """No logged search with that id."""


@dataclass(frozen=True)
class Revision:
    record_id: str
    revision: int
    text: str | None
    metadata: dict[str, Any] | None
    text_sha256: str
    created_at: str


@dataclass(frozen=True)
class Report:
    id: str
    record_id: str
    revision: int | None
    search_id: str | None
    text: str
    created_at: str


_SCHEMA = """
CREATE TABLE IF NOT EXISTS records (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    deleted_at TEXT
);
CREATE TABLE IF NOT EXISTS revisions (
    record_id TEXT NOT NULL REFERENCES records(id),
    revision INTEGER NOT NULL,
    text TEXT,
    metadata TEXT,
    text_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (record_id, revision)
);
CREATE TABLE IF NOT EXISTS reports (
    id TEXT PRIMARY KEY,
    record_id TEXT NOT NULL REFERENCES records(id),
    revision INTEGER,
    search_id TEXT,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS reports_record ON reports(record_id, created_at);
CREATE TABLE IF NOT EXISTS searches (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    filters TEXT,
    hits TEXT NOT NULL,
    timings TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS index_documents (
    internal_id INTEGER PRIMARY KEY,
    record_id TEXT NOT NULL UNIQUE,
    revision INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS index_state (
    key TEXT PRIMARY KEY,
    value TEXT
);
"""


def _now() -> str:
    return datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def _digest(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _dumps(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


class Store:
    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._connection = sqlite3.connect(self.path, isolation_level=None, check_same_thread=False)
        self._connection.execute("PRAGMA journal_mode=WAL")
        self._connection.execute("PRAGMA foreign_keys=ON")
        self._connection.executescript(_SCHEMA)

    def close(self) -> None:
        self._connection.close()

    # -- records ----------------------------------------------------------------

    def put(
        self, text: str, metadata: Mapping[str, Any] | None, record_id: str | None
    ) -> tuple[str, int]:
        now = _now()
        connection = self._connection
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            if record_id is None:
                record_id = uuid.uuid4().hex
                connection.execute(
                    "INSERT INTO records (id, created_at) VALUES (?, ?)", (record_id, now)
                )
                revision = 1
            else:
                row = connection.execute(
                    "SELECT deleted_at FROM records WHERE id = ?", (record_id,)
                ).fetchone()
                if row is None:
                    connection.execute(
                        "INSERT INTO records (id, created_at) VALUES (?, ?)", (record_id, now)
                    )
                    revision = 1
                elif row[0] is not None:
                    raise RecordDeleted(record_id)
                else:
                    (latest,) = connection.execute(
                        "SELECT MAX(revision) FROM revisions WHERE record_id = ?", (record_id,)
                    ).fetchone()
                    revision = int(latest) + 1
            connection.execute(
                "INSERT INTO revisions (record_id, revision, text, metadata, text_sha256,"
                " created_at) VALUES (?, ?, ?, ?, ?, ?)",
                (
                    record_id,
                    revision,
                    text,
                    None if metadata is None else _dumps(dict(metadata)),
                    _digest(text),
                    now,
                ),
            )
        return record_id, revision

    def _record_state(self, record_id: str) -> None:
        row = self._connection.execute(
            "SELECT deleted_at FROM records WHERE id = ?", (record_id,)
        ).fetchone()
        if row is None:
            raise RecordNotFound(record_id)
        if row[0] is not None:
            raise RecordDeleted(record_id)

    def get(self, record_id: str, revision: int | None) -> Revision:
        self._record_state(record_id)
        if revision is None:
            row = self._connection.execute(
                "SELECT record_id, revision, text, metadata, text_sha256, created_at"
                " FROM revisions WHERE record_id = ? ORDER BY revision DESC LIMIT 1",
                (record_id,),
            ).fetchone()
        else:
            row = self._connection.execute(
                "SELECT record_id, revision, text, metadata, text_sha256, created_at"
                " FROM revisions WHERE record_id = ? AND revision = ?",
                (record_id, revision),
            ).fetchone()
        if row is None:
            raise RecordNotFound(f"{record_id}@{revision}")
        return _revision(row)

    def delete(self, record_id: str) -> bool:
        connection = self._connection
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute(
                "SELECT deleted_at FROM records WHERE id = ?", (record_id,)
            ).fetchone()
            if row is None:
                raise RecordNotFound(record_id)
            if row[0] is not None:
                return False
            connection.execute(
                "UPDATE revisions SET text = NULL, metadata = NULL WHERE record_id = ?",
                (record_id,),
            )
            connection.execute(
                "UPDATE records SET deleted_at = ? WHERE id = ?", (_now(), record_id)
            )
        return True

    def live_documents(self) -> list[Revision]:
        rows = self._connection.execute(
            "SELECT r.record_id, r.revision, r.text, r.metadata, r.text_sha256, r.created_at"
            " FROM revisions r JOIN records rec ON rec.id = r.record_id"
            " WHERE rec.deleted_at IS NULL AND r.revision ="
            "   (SELECT MAX(revision) FROM revisions WHERE record_id = r.record_id)"
            " ORDER BY r.record_id"
        ).fetchall()
        return [_revision(row) for row in rows]

    def matching_record_ids(self, filters: Mapping[str, Any]) -> set[str]:
        wanted = dict(filters)
        matches: set[str] = set()
        for document in self.live_documents():
            metadata = document.metadata or {}
            if all(key in metadata and metadata[key] == value for key, value in wanted.items()):
                matches.add(document.record_id)
        return matches

    # -- reports and searches ---------------------------------------------------

    def add_report(
        self, record_id: str, revision: int | None, search_id: str | None, text: str
    ) -> str:
        exists = self._connection.execute(
            "SELECT 1 FROM records WHERE id = ?", (record_id,)
        ).fetchone()
        if exists is None:
            raise RecordNotFound(record_id)
        if search_id is not None and not self.has_search(search_id):
            raise SearchNotFound(search_id)
        report_id = uuid.uuid4().hex
        with self._connection:
            self._connection.execute(
                "INSERT INTO reports (id, record_id, revision, search_id, text, created_at)"
                " VALUES (?, ?, ?, ?, ?, ?)",
                (report_id, record_id, revision, search_id, text, _now()),
            )
        return report_id

    def reports_for(self, record_id: str) -> list[Report]:
        exists = self._connection.execute(
            "SELECT 1 FROM records WHERE id = ?", (record_id,)
        ).fetchone()
        if exists is None:
            raise RecordNotFound(record_id)
        rows = self._connection.execute(
            "SELECT id, record_id, revision, search_id, text, created_at FROM reports"
            " WHERE record_id = ? ORDER BY created_at, id",
            (record_id,),
        ).fetchall()
        return [Report(*row) for row in rows]

    def log_search(
        self,
        text: str,
        filters: Mapping[str, Any] | None,
        hits: Sequence[Mapping[str, Any]],
        timings: Mapping[str, float],
    ) -> str:
        search_id = uuid.uuid4().hex
        with self._connection:
            self._connection.execute(
                "INSERT INTO searches (id, text, filters, hits, timings, created_at)"
                " VALUES (?, ?, ?, ?, ?, ?)",
                (
                    search_id,
                    text,
                    None if filters is None else _dumps(dict(filters)),
                    _dumps([dict(hit) for hit in hits]),
                    _dumps(dict(timings)),
                    _now(),
                ),
            )
        return search_id

    def has_search(self, search_id: str) -> bool:
        row = self._connection.execute(
            "SELECT 1 FROM searches WHERE id = ?", (search_id,)
        ).fetchone()
        return row is not None

    # -- index mirror -----------------------------------------------------------

    def index_documents(self) -> list[tuple[int, str, int]]:
        rows = self._connection.execute(
            "SELECT internal_id, record_id, revision FROM index_documents ORDER BY internal_id"
        ).fetchall()
        return [(int(a), str(b), int(c)) for a, b, c in rows]

    def index_encoder(self) -> str | None:
        row = self._connection.execute(
            "SELECT value FROM index_state WHERE key = 'encoder'"
        ).fetchone()
        return None if row is None else row[0]

    def replace_index_documents(self, rows: Sequence[tuple[str, int]], encoder: str | None) -> None:
        connection = self._connection
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            connection.execute("DELETE FROM index_documents")
            connection.executemany(
                "INSERT INTO index_documents (internal_id, record_id, revision) VALUES (?, ?, ?)",
                [
                    (position, record_id, revision)
                    for position, (record_id, revision) in enumerate(rows)
                ],
            )
            connection.execute("DELETE FROM index_state WHERE key = 'encoder'")
            connection.execute(
                "INSERT INTO index_state (key, value) VALUES ('encoder', ?)", (encoder,)
            )


def _revision(row: Sequence[Any]) -> Revision:
    record_id, revision, text, metadata, text_sha256, created_at = row
    return Revision(
        record_id=str(record_id),
        revision=int(revision),
        text=text,
        metadata=None if metadata is None else json.loads(metadata),
        text_sha256=str(text_sha256),
        created_at=str(created_at),
    )
