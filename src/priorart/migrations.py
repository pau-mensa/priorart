"""Transactional SQLite schema upgrades, independent of record operations.

Historical v1 databases have user_version=0. Migration 1 recognizes that
layout or initializes an empty database, without changing record semantics.
Append future migrations; never edit an already released migration.
"""

from __future__ import annotations

import json
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


def _scope_collections(connection: sqlite3.Connection) -> None:
    # Freeze these bootstrap IDs as part of the on-disk migration contract.
    local_collection, local_account, local_principal = "local", "local-account", "local-principal"
    tables = ("records", "revisions", "reports", "searches", "index_documents", "index_state")
    for table in tables:
        connection.execute(f"ALTER TABLE {table} RENAME TO legacy_{table}")
    connection.execute("DROP INDEX reports_record")
    statements = (
        """CREATE TABLE principals (id TEXT PRIMARY KEY, created_at TEXT NOT NULL)""",
        """CREATE TABLE accounts (
            id TEXT PRIMARY KEY,
            owner_principal_id TEXT NOT NULL REFERENCES principals(id),
            created_at TEXT NOT NULL
        )""",
        """CREATE TABLE collections (
            id TEXT PRIMARY KEY,
            owner_account_id TEXT NOT NULL REFERENCES accounts(id),
            visibility TEXT NOT NULL DEFAULT 'restricted'
                CHECK (visibility IN ('public', 'restricted')),
            created_at TEXT NOT NULL
        )""",
        """CREATE TRIGGER collection_visibility_immutable
            BEFORE UPDATE OF visibility ON collections
            WHEN NEW.visibility != OLD.visibility
            BEGIN SELECT RAISE(ABORT, 'collection visibility is immutable'); END""",
        """CREATE TABLE records (
            collection_id TEXT NOT NULL REFERENCES collections(id),
            id TEXT NOT NULL,
            author_principal_id TEXT REFERENCES principals(id),
            created_at TEXT NOT NULL,
            deleted_at TEXT,
            PRIMARY KEY (collection_id, id)
        )""",
        """CREATE TABLE revisions (
            collection_id TEXT NOT NULL,
            record_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            text TEXT,
            metadata TEXT,
            text_sha256 TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (collection_id, record_id, revision),
            FOREIGN KEY (collection_id, record_id) REFERENCES records(collection_id, id)
        )""",
        """CREATE TABLE searches (
            collection_id TEXT NOT NULL REFERENCES collections(id),
            id TEXT NOT NULL,
            requester_principal_id TEXT REFERENCES principals(id),
            text TEXT NOT NULL,
            filters TEXT,
            timings TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (collection_id, id)
        )""",
        """CREATE TABLE search_hits (
            collection_id TEXT NOT NULL,
            search_id TEXT NOT NULL,
            position INTEGER NOT NULL CHECK (position >= 0),
            record_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            score REAL NOT NULL,
            PRIMARY KEY (collection_id, search_id, position),
            FOREIGN KEY (collection_id, search_id) REFERENCES searches(collection_id, id),
            FOREIGN KEY (collection_id, record_id, revision)
                REFERENCES revisions(collection_id, record_id, revision)
        )""",
        """CREATE TABLE reports (
            collection_id TEXT NOT NULL,
            id TEXT NOT NULL,
            reporter_principal_id TEXT REFERENCES principals(id),
            record_id TEXT NOT NULL,
            revision INTEGER,
            search_id TEXT,
            text TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (collection_id, id),
            FOREIGN KEY (collection_id, record_id) REFERENCES records(collection_id, id),
            FOREIGN KEY (collection_id, record_id, revision)
                REFERENCES revisions(collection_id, record_id, revision),
            FOREIGN KEY (collection_id, search_id) REFERENCES searches(collection_id, id)
        )""",
        "CREATE INDEX reports_record ON reports(collection_id, record_id, created_at)",
        """CREATE TABLE index_documents (
            collection_id TEXT NOT NULL,
            internal_id INTEGER NOT NULL CHECK (internal_id >= 0),
            record_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            PRIMARY KEY (collection_id, internal_id),
            UNIQUE (collection_id, record_id),
            FOREIGN KEY (collection_id, record_id, revision)
                REFERENCES revisions(collection_id, record_id, revision)
        )""",
        """CREATE TABLE index_state (
            collection_id TEXT NOT NULL REFERENCES collections(id),
            key TEXT NOT NULL,
            value TEXT,
            PRIMARY KEY (collection_id, key)
        )""",
    )
    for statement in statements:
        connection.execute(statement)
    (now,) = connection.execute("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')").fetchone()
    connection.execute("INSERT INTO principals VALUES (?, ?)", (local_principal, now))
    connection.execute(
        "INSERT INTO accounts VALUES (?, ?, ?)", (local_account, local_principal, now)
    )
    connection.execute(
        "INSERT INTO collections VALUES (?, ?, 'restricted', ?)",
        (local_collection, local_account, now),
    )
    # Unknown authorship/requester identity stays NULL; ownership is not authorship.
    copies = (
        ("records", "id, created_at, deleted_at"),
        ("revisions", "record_id, revision, text, metadata, text_sha256, created_at"),
        ("searches", "id, text, filters, timings, created_at"),
        ("reports", "id, record_id, revision, search_id, text, created_at"),
        ("index_documents", "internal_id, record_id, revision"),
        ("index_state", "key, value"),
    )
    try:
        for table, columns in copies:
            connection.execute(
                f"INSERT INTO {table} (collection_id, {columns})"
                f" SELECT ?, {columns} FROM legacy_{table}",
                (local_collection,),
            )
        for search_id, encoded in connection.execute("SELECT id, hits FROM legacy_searches"):
            hits = json.loads(encoded)
            if not isinstance(hits, list):
                raise ValueError("legacy hits must be a list")
            for position, hit in enumerate(hits):
                # V1's persisted hit contract has exactly these three fields.
                # Fail rather than silently strip unknown data on migration.
                if set(hit) != {"id", "revision", "score"}:
                    raise ValueError("unrecognized legacy hit fields")
                connection.execute(
                    "INSERT INTO search_hits VALUES (?, ?, ?, ?, ?, ?)",
                    (
                        local_collection,
                        search_id,
                        position,
                        hit["id"],
                        hit["revision"],
                        hit["score"],
                    ),
                )
    except (sqlite3.IntegrityError, ValueError, TypeError, KeyError) as error:
        raise SchemaError(
            "Collection migration cannot preserve invalid legacy references or search hits. "
            "The upgrade was rolled back; inspect reports, revisions, searches, and index mirror "
            "in a backup before retrying."
        ) from error
    for table in ("reports", "index_documents", "index_state", "searches", "revisions", "records"):
        connection.execute(f"DROP TABLE legacy_{table}")


_MIGRATIONS = (_initialize_v1, _scope_collections)
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
