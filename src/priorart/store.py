"""Collection-scoped SQLite persistence.

All content operations require an explicit collection. This is a persistence
boundary, not authorization: trusted local callers supply principal IDs until
service policy is implemented. Legacy authorship remains unknown (NULL).
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

from .migrations import migrate

LOCAL_COLLECTION_ID = "local"
LOCAL_ACCOUNT_ID = "local-account"
LOCAL_PRINCIPAL_ID = "local-principal"


class RecordNotFound(KeyError):
    """No record, revision, or report target in the selected collection."""


class RecordDeleted(KeyError):
    """The record exists only as a tombstone."""


class SearchNotFound(KeyError):
    """No logged search in the selected collection."""


class CollectionNotFound(KeyError):
    """No collection with that identity."""


@dataclass(frozen=True)
class Collection:
    id: str
    owner_account_id: str
    visibility: str
    created_at: str


@dataclass(frozen=True)
class RecordRef:
    collection_id: str
    record_id: str
    revision: int


@dataclass(frozen=True)
class Revision:
    collection_id: str
    record_id: str
    revision: int
    text: str | None
    metadata: dict[str, Any] | None
    text_sha256: str
    created_at: str
    author_principal_id: str | None


@dataclass(frozen=True)
class Report:
    collection_id: str
    id: str
    record_id: str
    revision: int | None
    search_id: str | None
    text: str
    created_at: str
    reporter_principal_id: str | None


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
        try:
            self._connection.execute("PRAGMA foreign_keys=ON")
            migrate(self._connection)
            self._connection.execute("PRAGMA journal_mode=WAL")
        except BaseException:
            self._connection.close()
            raise

    def close(self) -> None:
        self._connection.close()

    # -- identities (trusted local persistence API, not remote provisioning) -----

    def create_principal(self) -> str:
        principal_id = uuid.uuid4().hex
        self._connection.execute("INSERT INTO principals VALUES (?, ?)", (principal_id, _now()))
        return principal_id

    def create_account(self, *, owner_principal_id: str) -> str:
        account_id = uuid.uuid4().hex
        self._connection.execute(
            "INSERT INTO accounts VALUES (?, ?, ?)", (account_id, owner_principal_id, _now())
        )
        return account_id

    def create_collection(self, *, owner_account_id: str, visibility: str = "restricted") -> str:
        if visibility not in {"public", "restricted"}:
            raise ValueError("visibility must be public or restricted")
        collection_id = uuid.uuid4().hex
        self._connection.execute(
            "INSERT INTO collections VALUES (?, ?, ?, ?)",
            (collection_id, owner_account_id, visibility, _now()),
        )
        return collection_id

    def get_collection(self, collection_id: str) -> Collection:
        row = self._connection.execute(
            "SELECT id, owner_account_id, visibility, created_at FROM collections WHERE id = ?",
            (collection_id,),
        ).fetchone()
        if row is None:
            raise CollectionNotFound(collection_id)
        return Collection(*row)

    # -- records ----------------------------------------------------------------

    def put(
        self,
        text: str,
        metadata: Mapping[str, Any] | None,
        record_id: str | None,
        *,
        collection_id: str,
        author_principal_id: str,
    ) -> RecordRef:
        now = _now()
        record_id = uuid.uuid4().hex if record_id is None else record_id
        connection = self._connection
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute(
                "SELECT deleted_at FROM records WHERE collection_id = ? AND id = ?",
                (collection_id, record_id),
            ).fetchone()
            if row is None:
                connection.execute(
                    "INSERT INTO records (collection_id, id, author_principal_id, created_at)"
                    " VALUES (?, ?, ?, ?)",
                    (collection_id, record_id, author_principal_id, now),
                )
                revision = 1
            elif row[0] is not None:
                raise RecordDeleted((collection_id, record_id))
            else:
                (latest,) = connection.execute(
                    "SELECT MAX(revision) FROM revisions WHERE collection_id = ? AND record_id = ?",
                    (collection_id, record_id),
                ).fetchone()
                revision = int(latest) + 1
            connection.execute(
                "INSERT INTO revisions (collection_id, record_id, revision, text, metadata,"
                " text_sha256, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                (
                    collection_id,
                    record_id,
                    revision,
                    text,
                    None if metadata is None else _dumps(dict(metadata)),
                    _digest(text),
                    now,
                ),
            )
        return RecordRef(collection_id, record_id, revision)

    def _record_state(self, record_id: str, *, collection_id: str) -> None:
        row = self._connection.execute(
            "SELECT deleted_at FROM records WHERE collection_id = ? AND id = ?",
            (collection_id, record_id),
        ).fetchone()
        if row is None:
            raise RecordNotFound((collection_id, record_id))
        if row[0] is not None:
            raise RecordDeleted((collection_id, record_id))

    def get(self, record_id: str, revision: int | None, *, collection_id: str) -> Revision:
        self._record_state(record_id, collection_id=collection_id)
        sql = (
            "SELECT r.collection_id, r.record_id, r.revision, r.text, r.metadata, r.text_sha256,"
            " r.created_at, rec.author_principal_id FROM revisions r JOIN records rec"
            " ON rec.collection_id = r.collection_id AND rec.id = r.record_id"
            " WHERE r.collection_id = ? AND r.record_id = ?"
        )
        parameters: tuple[Any, ...] = (collection_id, record_id)
        if revision is not None:
            sql += " AND r.revision = ?"
            parameters += (revision,)
        row = self._connection.execute(
            sql + " ORDER BY r.revision DESC LIMIT 1", parameters
        ).fetchone()
        if row is None:
            raise RecordNotFound((collection_id, record_id, revision))
        return _revision(row)

    def delete(self, record_id: str, *, collection_id: str) -> bool:
        connection = self._connection
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute(
                "SELECT deleted_at FROM records WHERE collection_id = ? AND id = ?",
                (collection_id, record_id),
            ).fetchone()
            if row is None:
                raise RecordNotFound((collection_id, record_id))
            if row[0] is not None:
                return False
            connection.execute(
                "UPDATE revisions SET text = NULL, metadata = NULL"
                " WHERE collection_id = ? AND record_id = ?",
                (collection_id, record_id),
            )
            connection.execute(
                "UPDATE records SET deleted_at = ? WHERE collection_id = ? AND id = ?",
                (_now(), collection_id, record_id),
            )
        return True

    def live_documents(self, *, collection_id: str) -> list[Revision]:
        rows = self._connection.execute(
            "SELECT r.collection_id, r.record_id, r.revision, r.text, r.metadata, r.text_sha256,"
            " r.created_at, rec.author_principal_id FROM revisions r JOIN records rec"
            " ON rec.collection_id = r.collection_id AND rec.id = r.record_id"
            " WHERE r.collection_id = ? AND rec.deleted_at IS NULL AND r.revision ="
            " (SELECT MAX(latest.revision) FROM revisions latest"
            " WHERE latest.collection_id = r.collection_id AND latest.record_id = r.record_id)"
            " ORDER BY r.record_id",
            (collection_id,),
        ).fetchall()
        return [_revision(row) for row in rows]

    def matching_record_ids(self, filters: Mapping[str, Any], *, collection_id: str) -> set[str]:
        matches: set[str] = set()
        for document in self.live_documents(collection_id=collection_id):
            metadata = document.metadata or {}
            if all(key in metadata and metadata[key] == value for key, value in filters.items()):
                matches.add(document.record_id)
        return matches

    # -- reports and searches ---------------------------------------------------

    def add_report(
        self,
        record_id: str,
        revision: int | None,
        search_id: str | None,
        text: str,
        *,
        collection_id: str,
        reporter_principal_id: str,
    ) -> str:
        connection = self._connection
        report_id = uuid.uuid4().hex
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            exists = connection.execute(
                "SELECT 1 FROM records WHERE collection_id = ? AND id = ?",
                (collection_id, record_id),
            ).fetchone()
            if exists is None:
                raise RecordNotFound((collection_id, record_id))
            if (
                revision is not None
                and connection.execute(
                    "SELECT 1 FROM revisions WHERE collection_id = ?"
                    " AND record_id = ? AND revision = ?",
                    (collection_id, record_id, revision),
                ).fetchone()
                is None
            ):
                raise RecordNotFound((collection_id, record_id, revision))
            if search_id is not None and not self.has_search(
                search_id, collection_id=collection_id
            ):
                raise SearchNotFound((collection_id, search_id))
            connection.execute(
                "INSERT INTO reports (collection_id, id, reporter_principal_id, record_id,"
                " revision, search_id, text, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    collection_id,
                    report_id,
                    reporter_principal_id,
                    record_id,
                    revision,
                    search_id,
                    text,
                    _now(),
                ),
            )
        return report_id

    def reports_for(self, record_id: str, *, collection_id: str) -> list[Report]:
        exists = self._connection.execute(
            "SELECT 1 FROM records WHERE collection_id = ? AND id = ?", (collection_id, record_id)
        ).fetchone()
        if exists is None:
            raise RecordNotFound((collection_id, record_id))
        rows = self._connection.execute(
            "SELECT collection_id, id, record_id, revision, search_id, text, created_at,"
            " reporter_principal_id FROM reports WHERE collection_id = ? AND record_id = ?"
            " ORDER BY created_at, id",
            (collection_id, record_id),
        ).fetchall()
        return [Report(*row) for row in rows]

    def log_search(
        self,
        text: str,
        filters: Mapping[str, Any] | None,
        hits: Sequence[Mapping[str, Any]],
        timings: Mapping[str, float],
        *,
        collection_id: str,
        requester_principal_id: str,
    ) -> str:
        search_id = uuid.uuid4().hex
        with self._connection:
            self._connection.execute("BEGIN IMMEDIATE")
            self._connection.execute(
                "INSERT INTO searches (collection_id, id, requester_principal_id, text, filters,"
                " timings, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                (
                    collection_id,
                    search_id,
                    requester_principal_id,
                    text,
                    None if filters is None else _dumps(dict(filters)),
                    _dumps(dict(timings)),
                    _now(),
                ),
            )
            for position, hit in enumerate(hits):
                if hit["collection_id"] != collection_id:
                    raise ValueError("search hit belongs to another collection")
                self._connection.execute(
                    "INSERT INTO search_hits VALUES (?, ?, ?, ?, ?, ?)",
                    (collection_id, search_id, position, hit["id"], hit["revision"], hit["score"]),
                )
        return search_id

    def has_search(self, search_id: str, *, collection_id: str) -> bool:
        return (
            self._connection.execute(
                "SELECT 1 FROM searches WHERE collection_id = ? AND id = ?",
                (collection_id, search_id),
            ).fetchone()
            is not None
        )

    # -- index mirror -----------------------------------------------------------

    def index_documents(self, *, collection_id: str) -> list[tuple[int, str, int]]:
        return self._connection.execute(
            "SELECT internal_id, record_id, revision FROM index_documents"
            " WHERE collection_id = ? ORDER BY internal_id",
            (collection_id,),
        ).fetchall()

    def index_encoder(self, *, collection_id: str) -> str | None:
        row = self._connection.execute(
            "SELECT value FROM index_state WHERE collection_id = ? AND key = 'encoder'",
            (collection_id,),
        ).fetchone()
        return None if row is None else row[0]

    def replace_index_documents(
        self, rows: Sequence[tuple[str, int]], encoder: str | None, *, collection_id: str
    ) -> None:
        connection = self._connection
        with connection:
            connection.execute("BEGIN IMMEDIATE")
            connection.execute(
                "DELETE FROM index_documents WHERE collection_id = ?", (collection_id,)
            )
            connection.executemany(
                "INSERT INTO index_documents (collection_id, internal_id, record_id, revision)"
                " VALUES (?, ?, ?, ?)",
                [
                    (collection_id, position, record_id, revision)
                    for position, (record_id, revision) in enumerate(rows)
                ],
            )
            connection.execute(
                "INSERT INTO index_state (collection_id, key, value)"
                " VALUES (?, 'encoder', ?) ON CONFLICT (collection_id, key)"
                " DO UPDATE SET value = excluded.value",
                (collection_id, encoder),
            )


def _revision(row: Sequence[Any]) -> Revision:
    collection_id, record_id, revision, text, metadata, digest, created_at, author = row
    return Revision(
        collection_id,
        record_id,
        revision,
        text,
        None if metadata is None else json.loads(metadata),
        digest,
        created_at,
        author,
    )
