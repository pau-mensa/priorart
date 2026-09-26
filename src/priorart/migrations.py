"""Transactional SQLite schema upgrades, independent of record operations.

Historical v1 databases have user_version=0. Migration 1 recognizes that
layout or initializes an empty database, without changing record semantics.
Append future migrations; never edit an already released migration.
"""

from __future__ import annotations

import sqlite3


class SchemaError(RuntimeError):
    """The database schema cannot safely be opened by this version."""


class UnsupportedSchemaVersion(SchemaError):
    """The database requires a different version of the application."""


_V1_STATEMENTS = (
    """CREATE TABLE records (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    deleted_at TEXT
)""",
    """CREATE TABLE revisions (
    record_id TEXT NOT NULL REFERENCES records(id),
    revision INTEGER NOT NULL,
    text TEXT,
    metadata TEXT,
    text_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (record_id, revision)
)""",
    """CREATE TABLE reports (
    id TEXT PRIMARY KEY,
    record_id TEXT NOT NULL REFERENCES records(id),
    revision INTEGER,
    search_id TEXT,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL
)""",
    "CREATE INDEX reports_record ON reports(record_id, created_at)",
    """CREATE TABLE searches (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    filters TEXT,
    hits TEXT NOT NULL,
    timings TEXT NOT NULL,
    created_at TEXT NOT NULL
)""",
    """CREATE TABLE index_documents (
    internal_id INTEGER PRIMARY KEY,
    record_id TEXT NOT NULL UNIQUE,
    revision INTEGER NOT NULL
)""",
    """CREATE TABLE index_state (
    key TEXT PRIMARY KEY,
    value TEXT
)""",
)


def _schema_signature(connection: sqlite3.Connection) -> list[tuple[str, str, str, str]]:
    # Conservative recognition of our own legacy DDL, not arbitrary SQLite files.
    # SQLite removes IF NOT EXISTS from stored DDL. Ignore whitespace and case,
    # but preserve constraints and reject extra application tables/indexes/triggers.
    rows = connection.execute(
        "SELECT type, name, tbl_name, sql FROM sqlite_master"
        " WHERE substr(name, 1, 7) != 'sqlite_' ORDER BY type, name"
    ).fetchall()
    return [
        (kind, name, table, " ".join(sql.split()).casefold()) for kind, name, table, sql in rows
    ]


def _initialize_v1(connection: sqlite3.Connection) -> None:
    existing = _schema_signature(connection)
    if not existing:
        for statement in _V1_STATEMENTS:
            connection.execute(statement)
        return

    reference = sqlite3.connect(":memory:")
    try:
        for statement in _V1_STATEMENTS:
            reference.execute(statement)
        expected = _schema_signature(reference)
    finally:
        reference.close()
    if existing != expected:
        raise SchemaError(
            "Unrecognized unversioned database schema; expected the complete legacy v1 layout. "
            "Restore a known-good backup or inspect the database before retrying."
        )


_MIGRATIONS = (_initialize_v1,)
SCHEMA_VERSION = len(_MIGRATIONS)


def migrate(connection: sqlite3.Connection) -> None:
    """Apply pending migrations and version stamps in one write transaction.

    The caller supplies a dedicated startup connection with foreign keys enabled
    and no active transaction. Migrations must use execute/executemany, never
    executescript (which can commit the transaction), or external file changes.
    """
    with connection:
        connection.execute("BEGIN IMMEDIATE")
        # Read under the write lock so concurrent openers cannot migrate twice.
        (version,) = connection.execute("PRAGMA user_version").fetchone()
        if version > SCHEMA_VERSION:
            raise UnsupportedSchemaVersion(
                f"Database schema version {version} is newer than supported version "
                f"{SCHEMA_VERSION}; upgrade priorart before opening it."
            )
        if version < 0:
            raise UnsupportedSchemaVersion(f"Invalid database schema version {version}.")
        for index in range(version, SCHEMA_VERSION):
            _MIGRATIONS[index](connection)
            connection.execute(f"PRAGMA user_version = {index + 1}")
