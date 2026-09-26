import json
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

from priorart import migrations
from priorart.store import LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID, RecordDeleted, Store


@pytest.fixture
def legacy_database(tmp_path):
    path = tmp_path / "priorart.sqlite"
    connection = sqlite3.connect(path)
    connection.executescript((Path(__file__).parent / "fixtures/v1.sql").read_text())
    assert connection.execute("PRAGMA user_version").fetchone() == (0,)
    connection.close()
    return path


def snapshot(path):
    connection = sqlite3.connect(path)
    try:
        return list(connection.iterdump())
    finally:
        connection.close()


def legacy_content(path):
    """Compare every legacy field, projecting through the new scoped layout."""
    connection = sqlite3.connect(path)
    try:
        scoped = connection.execute("PRAGMA user_version").fetchone()[0] >= 2
        columns = {
            "records": "id, created_at, deleted_at",
            "revisions": "record_id, revision, text, metadata, text_sha256, created_at",
            "reports": "id, record_id, revision, search_id, text, created_at",
            "searches": "id, text, filters, timings, created_at",
            "index_documents": "internal_id, record_id, revision",
            "index_state": "key, value",
        }
        output = {}
        for table, fields in columns.items():
            query = f"SELECT {fields} FROM {table}"
            if scoped:
                query += " WHERE collection_id = 'local'"
            output[table] = connection.execute(query + " ORDER BY 1, 2").fetchall()
        if scoped:
            output["hits"] = connection.execute(
                "SELECT search_id, position, record_id, revision, score FROM search_hits"
                " WHERE collection_id = 'local' ORDER BY search_id, position"
            ).fetchall()
        else:
            output["hits"] = [
                (search_id, position, hit["id"], hit["revision"], hit["score"])
                for search_id, encoded in connection.execute(
                    "SELECT id, hits FROM searches ORDER BY id"
                )
                for position, hit in enumerate(json.loads(encoded))
            ]
        return output
    finally:
        connection.close()


def test_fresh_database_and_repeat_startup(tmp_path, monkeypatch):
    path = tmp_path / "priorart.sqlite"
    store = Store(path)
    assert store._connection.execute("PRAGMA user_version").fetchone() == (
        migrations.SCHEMA_VERSION,
    )
    assert store._connection.execute("PRAGMA foreign_keys").fetchone() == (1,)
    assert store._connection.execute("PRAGMA journal_mode").fetchone() == ("wal",)
    store.put(
        "first",
        None,
        "record",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    store.close()
    before = snapshot(path)

    def already_migrated(connection):
        pytest.fail("startup reran an applied migration")

    monkeypatch.setattr(migrations, "_MIGRATIONS", (already_migrated,))
    for _ in range(2):
        store = Store(path)
        assert store.get("record", None, collection_id=LOCAL_COLLECTION_ID).text == "first"
        assert store._connection.total_changes == 0
        store.close()
    assert snapshot(path) == before


@pytest.mark.parametrize("version", [0, 1])
def test_legacy_upgrade_preserves_all_data_and_behavior(legacy_database, version):
    connection = sqlite3.connect(legacy_database)
    connection.execute(f"PRAGMA user_version = {version}")
    connection.close()
    before = legacy_content(legacy_database)
    store = Store(legacy_database)
    assert store._connection.execute("PRAGMA user_version").fetchone() == (
        migrations.SCHEMA_VERSION,
    )
    assert legacy_content(legacy_database) == before
    assert store.get_collection(LOCAL_COLLECTION_ID).visibility == "restricted"
    assert store.get("live", None, collection_id=LOCAL_COLLECTION_ID).author_principal_id is None
    assert store._connection.execute("PRAGMA foreign_key_check").fetchall() == []
    assert store._connection.execute("SELECT count(*) FROM collections").fetchone() == (1,)
    assert (
        store.reports_for("live", collection_id=LOCAL_COLLECTION_ID)[0].reporter_principal_id
        is None
    )
    assert store._connection.execute(
        "SELECT requester_principal_id FROM searches WHERE collection_id = ? AND id = ?",
        (LOCAL_COLLECTION_ID, "search-v1"),
    ).fetchone() == (None,)
    assert store.get("live", 1, collection_id=LOCAL_COLLECTION_ID).text == "CUDA watchdog timeout"
    assert (
        store.get("live", None, collection_id=LOCAL_COLLECTION_ID).text
        == "CUDA watchdog fixed — café"
    )
    assert store.get("live", 1, collection_id=LOCAL_COLLECTION_ID).metadata == {
        "lang": "python",
        "nested": {"gpu": True},
    }
    assert store.get("other", None, collection_id=LOCAL_COLLECTION_ID).metadata is None
    assert [
        (r.record_id, r.revision) for r in store.live_documents(collection_id=LOCAL_COLLECTION_ID)
    ] == [
        ("live", 2),
        ("other", 1),
    ]
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == [
        (0, "other", 1),
        (1, "live", 2),
    ]
    assert store.index_encoder(collection_id=LOCAL_COLLECTION_ID) == "legacy-encoder"
    assert store.has_search("search-v1", collection_id=LOCAL_COLLECTION_ID)
    assert store.reports_for("live", collection_id=LOCAL_COLLECTION_ID)[0].search_id == "search-v1"
    assert store.reports_for("deleted", collection_id=LOCAL_COLLECTION_ID)[0].revision is None
    with pytest.raises(RecordDeleted):
        store.get("deleted", None, collection_id=LOCAL_COLLECTION_ID)
    with pytest.raises(RecordDeleted):
        store.put(
            "reuse",
            None,
            "deleted",
            collection_id=LOCAL_COLLECTION_ID,
            author_principal_id=LOCAL_PRINCIPAL_ID,
        )
    assert (
        store.put(
            "third revision",
            None,
            "live",
            collection_id=LOCAL_COLLECTION_ID,
            author_principal_id=LOCAL_PRINCIPAL_ID,
        ).revision
        == 3
    )
    store.close()
    reopened = Store(legacy_database)
    assert reopened.get("live", None, collection_id=LOCAL_COLLECTION_ID).revision == 3
    reopened.close()


@pytest.mark.parametrize(
    "sql",
    [
        "UPDATE reports SET revision = 99 WHERE id = 'report-live'",
        "UPDATE reports SET search_id = 'missing' WHERE id = 'report-live'",
        "UPDATE index_documents SET revision = 99",
        'UPDATE searches SET hits = \'[{"id":"missing","revision":1,"score":1}]\'',
        "UPDATE searches SET hits = 'not-json'",
    ],
)
def test_invalid_legacy_references_roll_back_collection_upgrade(legacy_database, sql):
    connection = sqlite3.connect(legacy_database)
    connection.execute(sql)
    connection.execute("PRAGMA user_version = 1")
    connection.commit()
    connection.close()
    before = snapshot(legacy_database)
    with pytest.raises(migrations.SchemaError, match="rolled back"):
        Store(legacy_database)
    assert snapshot(legacy_database) == before
    connection = sqlite3.connect(legacy_database)
    assert connection.execute("PRAGMA user_version").fetchone() == (1,)
    connection.close()


@pytest.mark.parametrize(
    "phase",
    [
        "ALTER TABLE revisions RENAME TO legacy_revisions",
        "INSERT INTO revisions (collection_id,",
        "DROP TABLE legacy_records",
        "PRAGMA user_version = 2",
    ],
)
@pytest.mark.parametrize("journal_mode", ["DELETE", "WAL"])
def test_collection_migration_process_interruption(legacy_database, phase, journal_mode):
    connection = sqlite3.connect(legacy_database)
    connection.execute("PRAGMA user_version = 1")
    connection.execute(f"PRAGMA journal_mode = {journal_mode}")
    connection.close()
    before = snapshot(legacy_database)
    content = legacy_content(legacy_database)
    script = """
import os
import sqlite3
import sys
from priorart.store import Store

class InterruptedConnection(sqlite3.Connection):
    def execute(self, sql, parameters=()):
        result = super().execute(sql, parameters)
        if sql.startswith(sys.argv[2]):
            os._exit(73)
        return result

original_connect = sqlite3.connect
def connect(*args, **kwargs):
    connection = original_connect(*args, **kwargs, factory=InterruptedConnection)
    connection.execute('PRAGMA cache_size = 5')
    return connection
sqlite3.connect = connect
Store(sys.argv[1])
"""
    result = subprocess.run(
        [sys.executable, "-c", script, str(legacy_database), phase],
        capture_output=True,
        text=True,
        timeout=20,
    )
    assert result.returncode == 73, result.stderr
    assert snapshot(legacy_database) == before
    connection = sqlite3.connect(legacy_database)
    assert connection.execute("PRAGMA user_version").fetchone() == (1,)
    assert connection.execute("PRAGMA integrity_check").fetchone() == ("ok",)
    connection.close()
    recovered = Store(legacy_database)
    assert recovered._connection.execute("PRAGMA foreign_key_check").fetchall() == []
    recovered.close()
    assert legacy_content(legacy_database) == content


def test_newer_schema_rejected_without_modification(legacy_database):
    connection = sqlite3.connect(legacy_database)
    connection.execute(f"PRAGMA user_version = {migrations.SCHEMA_VERSION + 1}")
    connection.close()
    before = legacy_database.read_bytes()
    with pytest.raises(migrations.UnsupportedSchemaVersion, match="newer"):
        Store(legacy_database)
    assert legacy_database.read_bytes() == before
    assert not legacy_database.with_name("priorart.sqlite-wal").exists()


def test_pending_migrations_commit_or_rollback_as_one_batch(legacy_database, monkeypatch):
    before = snapshot(legacy_database)
    baseline = migrations._MIGRATIONS[0]

    def second(connection):
        assert connection.execute("PRAGMA user_version").fetchone() == (1,)
        connection.execute("CREATE TABLE migration_probe (value TEXT)")
        connection.execute("INSERT INTO migration_probe VALUES ('preserved')")

    def failing_third(connection):
        assert connection.execute("PRAGMA user_version").fetchone() == (2,)
        connection.execute("UPDATE revisions SET text = 'changed'")
        raise RuntimeError("third migration failed")

    monkeypatch.setattr(migrations, "SCHEMA_VERSION", 3)
    monkeypatch.setattr(migrations, "_MIGRATIONS", (baseline, second, failing_third))
    with pytest.raises(RuntimeError, match="third migration failed"):
        Store(legacy_database)
    assert snapshot(legacy_database) == before

    def third(connection):
        assert connection.execute("PRAGMA user_version").fetchone() == (2,)
        assert connection.execute("SELECT value FROM migration_probe").fetchone() == ("preserved",)

    monkeypatch.setattr(migrations, "_MIGRATIONS", (baseline, second, third))
    store = Store(legacy_database)
    assert store._connection.execute("PRAGMA user_version").fetchone() == (3,)
    assert store._connection.execute(
        "SELECT text FROM revisions WHERE record_id = 'live' AND revision = 2"
    ).fetchone() == ("CUDA watchdog fixed — café",)
    store.close()


def test_serve_reports_unsupported_schema(legacy_database, monkeypatch, capsys):
    from priorart.__main__ import main

    connection = sqlite3.connect(legacy_database)
    connection.execute(f"PRAGMA user_version = {migrations.SCHEMA_VERSION + 1}")
    connection.close()
    monkeypatch.setenv("PRIORART_DATA_DIR", str(legacy_database.parent))
    monkeypatch.setenv("PRIORART_ENCODER", "none")
    assert main(["serve"]) == 2
    assert "newer than supported" in capsys.readouterr().err


@pytest.mark.parametrize(
    "sql",
    [
        "CREATE TABLE unrelated (id INTEGER)",
        "DROP TABLE index_state",
        "ALTER TABLE revisions ADD COLUMN unexpected TEXT",
    ],
)
def test_unknown_or_partial_unversioned_database_rejected(legacy_database, sql):
    connection = sqlite3.connect(legacy_database)
    connection.execute(sql)
    connection.close()
    before = snapshot(legacy_database)
    with pytest.raises(migrations.SchemaError, match="unversioned"):
        Store(legacy_database)
    assert snapshot(legacy_database) == before
    connection = sqlite3.connect(legacy_database)
    assert connection.execute("PRAGMA user_version").fetchone() == (0,)
    connection.close()


@pytest.mark.parametrize("existing", [False, True])
def test_exception_after_version_write_rolls_back_and_closes(
    legacy_database, tmp_path, monkeypatch, existing
):
    path = legacy_database if existing else tmp_path / "fresh.sqlite"
    before = snapshot(path)
    connections = []
    original_connect = sqlite3.connect
    baseline = migrations._MIGRATIONS[0]

    def migrate_with_changes(connection):
        baseline(connection)
        connection.execute("CREATE TABLE migration_probe (id INTEGER)")
        connection.execute("UPDATE revisions SET text = 'changed'")

    class FailingConnection(sqlite3.Connection):
        def execute(self, sql, parameters=()):
            result = super().execute(sql, parameters)
            if sql == "PRAGMA user_version = 1":
                raise RuntimeError("injected failure after version write")
            return result

    def connect(*args, **kwargs):
        connection = original_connect(*args, **kwargs, factory=FailingConnection)
        connections.append(connection)
        return connection

    with monkeypatch.context() as patch:
        patch.setattr(migrations, "_MIGRATIONS", (migrate_with_changes,))
        patch.setattr(sqlite3, "connect", connect)
        with pytest.raises(RuntimeError, match="injected failure"):
            Store(path)
    with pytest.raises(sqlite3.ProgrammingError, match="closed"):
        connections[0].execute("SELECT 1")
    assert snapshot(path) == before
    connection = sqlite3.connect(path)
    assert connection.execute("PRAGMA user_version").fetchone() == (0,)
    connection.close()
    recovered = Store(path)
    assert recovered._connection.execute("PRAGMA user_version").fetchone() == (
        migrations.SCHEMA_VERSION,
    )
    recovered.close()


@pytest.mark.parametrize("journal_mode", ["DELETE", "WAL"])
@pytest.mark.parametrize("phase", ["changes", "version"])
def test_process_interruption_and_recovery(legacy_database, journal_mode, phase):
    connection = sqlite3.connect(legacy_database)
    connection.execute(f"PRAGMA journal_mode = {journal_mode}")
    connection.close()
    before = snapshot(legacy_database)
    before_content = legacy_content(legacy_database)
    # os._exit skips finally blocks, connection close, and Python rollback.
    script = """
import os
import sqlite3
import sys
from priorart import migrations
from priorart.store import LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID, Store

baseline = migrations._MIGRATIONS[0]
def interrupted(connection):
    baseline(connection)
    connection.execute('PRAGMA cache_size = 5')
    connection.execute('CREATE TABLE migration_probe (id INTEGER)')
    # Exceed the page cache so the crash exercises on-disk transaction recovery.
    connection.execute('INSERT INTO migration_probe VALUES (zeroblob(262144))')
    connection.execute("UPDATE revisions SET text = 'changed'")
    if sys.argv[2] == 'changes':
        os._exit(73)

class InterruptedConnection(sqlite3.Connection):
    def execute(self, sql, parameters=()):
        result = super().execute(sql, parameters)
        if sql == 'PRAGMA user_version = 1':
            os._exit(73)
        return result

original_connect = sqlite3.connect
def connect(*args, **kwargs):
    return original_connect(*args, **kwargs, factory=InterruptedConnection)
sqlite3.connect = connect
migrations._MIGRATIONS = (interrupted,)
Store(sys.argv[1])
"""
    result = subprocess.run(
        [sys.executable, "-c", script, str(legacy_database), phase],
        capture_output=True,
        text=True,
        timeout=20,
    )
    assert result.returncode == 73, result.stderr
    connection = sqlite3.connect(legacy_database)
    assert connection.execute("PRAGMA user_version").fetchone() == (0,)
    assert connection.execute("PRAGMA integrity_check").fetchone() == ("ok",)
    connection.close()
    assert snapshot(legacy_database) == before
    recovered = Store(legacy_database)
    assert (
        recovered.get("live", None, collection_id=LOCAL_COLLECTION_ID).text
        == "CUDA watchdog fixed — café"
    )
    assert recovered._connection.execute("PRAGMA user_version").fetchone() == (
        migrations.SCHEMA_VERSION,
    )
    recovered.close()
    assert legacy_content(legacy_database) == before_content
