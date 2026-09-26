import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

from priorart import migrations
from priorart.store import RecordDeleted, Store


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


def test_fresh_database_and_repeat_startup(tmp_path, monkeypatch):
    path = tmp_path / "priorart.sqlite"
    store = Store(path)
    assert store._connection.execute("PRAGMA user_version").fetchone() == (
        migrations.SCHEMA_VERSION,
    )
    assert store._connection.execute("PRAGMA foreign_keys").fetchone() == (1,)
    assert store._connection.execute("PRAGMA journal_mode").fetchone() == ("wal",)
    store.put("first", None, "record")
    store.close()
    before = snapshot(path)

    def already_migrated(connection):
        pytest.fail("startup reran an applied migration")

    monkeypatch.setattr(migrations, "_MIGRATIONS", (already_migrated,))
    for _ in range(2):
        store = Store(path)
        assert store.get("record", None).text == "first"
        assert store._connection.total_changes == 0
        store.close()
    assert snapshot(path) == before


def test_legacy_upgrade_preserves_all_data_and_behavior(legacy_database):
    before = snapshot(legacy_database)
    store = Store(legacy_database)
    assert store._connection.execute("PRAGMA user_version").fetchone() == (1,)
    assert snapshot(legacy_database) == before
    assert store.get("live", 1).text == "CUDA watchdog timeout"
    assert store.get("live", None).text == "CUDA watchdog fixed — café"
    assert store.get("live", 1).metadata == {"lang": "python", "nested": {"gpu": True}}
    assert store.get("other", None).metadata is None
    assert [(r.record_id, r.revision) for r in store.live_documents()] == [
        ("live", 2),
        ("other", 1),
    ]
    assert store.index_documents() == [(0, "other", 1), (1, "live", 2)]
    assert store.index_encoder() == "legacy-encoder"
    assert store.has_search("search-v1")
    assert store.reports_for("live")[0].search_id == "search-v1"
    assert store.reports_for("deleted")[0].revision is None
    with pytest.raises(RecordDeleted):
        store.get("deleted", None)
    with pytest.raises(RecordDeleted):
        store.put("reuse", None, "deleted")
    assert store.put("third revision", None, "live") == ("live", 3)
    store.close()
    reopened = Store(legacy_database)
    assert reopened.get("live", None).revision == 3
    reopened.close()


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
    assert store.get("live", None).text == "CUDA watchdog fixed — café"
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
    assert recovered._connection.execute("PRAGMA user_version").fetchone() == (1,)
    recovered.close()


@pytest.mark.parametrize("journal_mode", ["DELETE", "WAL"])
@pytest.mark.parametrize("phase", ["changes", "version"])
def test_process_interruption_and_recovery(legacy_database, journal_mode, phase):
    connection = sqlite3.connect(legacy_database)
    connection.execute(f"PRAGMA journal_mode = {journal_mode}")
    connection.close()
    before = snapshot(legacy_database)
    # os._exit skips finally blocks, connection close, and Python rollback.
    script = """
import os
import sqlite3
import sys
from priorart import migrations
from priorart.store import Store

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
    assert recovered.get("live", None).text == "CUDA watchdog fixed — café"
    assert recovered._connection.execute("PRAGMA user_version").fetchone() == (1,)
    recovered.close()
    assert snapshot(legacy_database) == before
