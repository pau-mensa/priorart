use serde_json::json;
use tempfile::TempDir;

use super::migrations::{run, stored_version, Migration};
use super::*;

const LOCAL: &str = LOCAL_COLLECTION_ID;

fn open() -> (TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("priorart.sqlite")).unwrap();
    (directory, store)
}

fn metadata(value: Value) -> Metadata {
    value.as_object().unwrap().clone()
}

fn put(store: &Store, collection: &str, text: &str, record: Option<&str>) -> RecordRef {
    store
        .put(
            collection,
            text,
            None,
            record,
            LOCAL_PRINCIPAL_ID,
            record
                .and_then(|id| store.get(collection, id, None).ok())
                .map(|r| r.revision),
        )
        .unwrap()
}

fn is_constraint(result: rusqlite::Result<usize>) -> bool {
    matches!(
        result,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

#[test]
fn put_creates_and_get_returns_latest() {
    let (_directory, store) = open();
    let tags = metadata(json!({"lang": "python"}));
    let created = store
        .put(LOCAL, "first", Some(&tags), None, LOCAL_PRINCIPAL_ID, None)
        .unwrap();
    assert_eq!(created.revision, 1);
    assert_eq!(created.record_id.len(), 32);
    let got = store.get(LOCAL, &created.record_id, None).unwrap();
    assert_eq!(got.text.as_deref(), Some("first"));
    assert_eq!(got.metadata, Some(tags));
    assert_eq!(got.text_sha256.len(), 64);
    assert!(got.created_at.ends_with('Z'));
}

#[test]
fn second_put_appends_a_revision() {
    let (_directory, store) = open();
    let first = put(&store, LOCAL, "first", None);
    let second = put(&store, LOCAL, "second", Some(&first.record_id));
    assert_eq!(second.record_id, first.record_id);
    assert_eq!(second.revision, 2);
    let latest = store.get(LOCAL, &first.record_id, None).unwrap();
    assert_eq!(latest.text.as_deref(), Some("second"));
    let original = store.get(LOCAL, &first.record_id, Some(1)).unwrap();
    assert_eq!(original.text.as_deref(), Some("first"));
    assert!(matches!(
        store.get(LOCAL, &first.record_id, Some(3)),
        Err(StoreError::RecordNotFound {
            revision: Some(3),
            ..
        })
    ));
    assert!(matches!(
        store.get(LOCAL, "nope", None),
        Err(StoreError::RecordNotFound { .. })
    ));
}

#[test]
fn delete_purges_revisions_and_blocks_reuse() {
    let (_directory, store) = open();
    let tags = metadata(json!({"a": 1}));
    let record = store
        .put(
            LOCAL,
            "secret",
            Some(&tags),
            Some("rec"),
            LOCAL_PRINCIPAL_ID,
            None,
        )
        .unwrap();
    put(&store, LOCAL, "secret 2", Some("rec"));
    assert!(store.delete(LOCAL, "rec", Some(2)).unwrap());
    assert!(!store.delete(LOCAL, "rec", Some(2)).unwrap());
    for revision in [None, Some(1)] {
        assert!(matches!(
            store.get(LOCAL, "rec", revision),
            Err(StoreError::RecordDeleted { .. })
        ));
    }
    assert!(matches!(
        store.put(LOCAL, "again", None, Some("rec"), LOCAL_PRINCIPAL_ID, None),
        Err(StoreError::RecordDeleted { .. })
    ));
    assert!(matches!(
        store.delete(LOCAL, "nope", Some(1)),
        Err(StoreError::RecordNotFound { .. })
    ));
    assert!(store.live_documents(LOCAL).unwrap().is_empty());
    let mut statement = store
        .connection
        .prepare("SELECT text, metadata, text_sha256 FROM revisions WHERE record_id = ?1")
        .unwrap();
    let rows = statement
        .query_map([&record.record_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn live_documents_are_latest_and_ordered() {
    let (_directory, store) = open();
    put(&store, LOCAL, "a1", Some("a"));
    put(&store, LOCAL, "b1", Some("b"));
    put(&store, LOCAL, "a2", Some("a"));
    put(&store, LOCAL, "c1", Some("c"));
    store.delete(LOCAL, "c", Some(1)).unwrap();
    let documents = store
        .live_documents(LOCAL)
        .unwrap()
        .into_iter()
        .map(|document| {
            (
                document.record_id,
                document.revision,
                document.text.unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        documents,
        [("a".into(), 2, "a2".into()), ("b".into(), 1, "b1".into())]
    );
}

struct Scoped {
    _directory: TempDir,
    store: Store,
    a: String,
    b: String,
    alice: String,
    bob: String,
}

fn scoped() -> Scoped {
    let (directory, store) = open();
    let alice = store.create_principal().unwrap();
    let bob = store.create_principal().unwrap();
    let a = store
        .create_collection(&alice, Visibility::Restricted)
        .unwrap();
    let b = store
        .create_collection(&bob, Visibility::Restricted)
        .unwrap();
    Scoped {
        _directory: directory,
        store,
        a,
        b,
        alice,
        bob,
    }
}

fn put_as(store: &Store, collection: &str, principal: &str, text: &str, record: &str) -> RecordRef {
    let tags = metadata(json!({"tag": "shared"}));
    store
        .put(
            collection,
            text,
            Some(&tags),
            Some(record),
            principal,
            store.get(collection, record, None).ok().map(|r| r.revision),
        )
        .unwrap()
}

#[test]
fn identity_ownership_and_defaults() {
    let Scoped {
        store,
        a,
        b,
        alice,
        bob,
        ..
    } = scoped();
    assert_ne!(a, b);
    assert_eq!(
        store.get_collection(&a).unwrap().visibility,
        Visibility::Restricted
    );
    assert_eq!(
        store.get_collection(LOCAL).unwrap().visibility,
        Visibility::Restricted
    );
    assert_eq!(
        store.get_collection(LOCAL).unwrap().owner_principal_id,
        LOCAL_PRINCIPAL_ID
    );
    assert_eq!(store.get_collection(&a).unwrap().owner_principal_id, alice);
    assert_eq!(store.get_collection(&b).unwrap().owner_principal_id, bob);
    assert!(matches!(
        store.get_collection("missing"),
        Err(StoreError::CollectionNotFound(_))
    ));
    let first = put_as(&store, &a, &alice, "alice", "same");
    let second = put_as(&store, &b, &bob, "bob", "same");
    assert_eq!(
        (first.collection_id.as_str(), first.revision),
        (a.as_str(), 1)
    );
    assert_eq!(second.collection_id, b);
    assert_eq!(
        store.get(&a, "same", None).unwrap().text.as_deref(),
        Some("alice")
    );
    assert_eq!(
        store.get(&b, "same", None).unwrap().text.as_deref(),
        Some("bob")
    );
    // Authorship is independent of ownership and survives another principal's update.
    put_as(&store, &a, &bob, "updated by another principal", "same");
    let latest = store.get(&a, "same", None).unwrap();
    assert_eq!(latest.author_principal_id.as_deref(), Some(alice.as_str()));
    assert_eq!(store.get(&b, "same", None).unwrap().revision, 1);
}

#[test]
fn visibility_and_ownership_constraints() {
    let Scoped { store, a, .. } = scoped();
    let owner = store.get_collection(&a).unwrap().owner_principal_id;
    assert!(is_constraint(store.connection.execute(
        "UPDATE collections SET visibility = 'public' WHERE id = ?1",
        [&a],
    )));
    assert!(matches!(
        store.create_collection("missing", Visibility::Restricted),
        Err(StoreError::PrincipalNotFound(_))
    ));
    assert!("invalid".parse::<Visibility>().is_err());
    let public = store.create_collection(&owner, Visibility::Public).unwrap();
    assert_eq!(
        store.get_collection(&public).unwrap().visibility,
        Visibility::Public
    );
    assert!(is_constraint(store.connection.execute(
        "UPDATE collections SET visibility = 'restricted' WHERE id = ?1",
        [&public],
    )));
}

#[test]
fn reads_and_deletes_are_scoped() {
    let Scoped {
        store,
        a,
        b,
        alice,
        bob,
        ..
    } = scoped();
    put_as(&store, &a, &alice, "alice", "same");
    put_as(&store, &b, &bob, "bob", "same");
    put_as(&store, &b, &bob, "bob second revision", "same");
    put_as(&store, &b, &bob, "text", "only-b");
    assert!(matches!(
        store.get(&a, "only-b", None),
        Err(StoreError::RecordNotFound { .. })
    ));
    let live = store.live_documents(&a).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].record_id, "same");
    store.delete(&a, "same", Some(1)).unwrap();
    assert!(store.live_documents(&a).unwrap().is_empty());
    assert_eq!(
        store.get(&b, "same", None).unwrap().text.as_deref(),
        Some("bob second revision")
    );
}

#[test]
fn cross_collection_references_fail_in_the_database() {
    let Scoped {
        store,
        a,
        b,
        alice,
        bob,
        ..
    } = scoped();
    put_as(&store, &a, &alice, "text", "same");
    put_as(&store, &b, &bob, "text", "same");
    put_as(&store, &b, &bob, "text", "same");
    put_as(&store, &b, &bob, "text", "only-b");
    let connection = &store.connection;
    assert!(is_constraint(connection.execute(
        "INSERT INTO revisions (collection_id, record_id, revision, text_sha256, created_at) \
         VALUES (?1, 'only-b', 1, 'hash', 'now')",
        [&a],
    )));
}

fn schema_version(connection: &Connection) -> Option<String> {
    stored_version(connection).unwrap()
}

fn dump(path: &Path) -> Vec<(String, String)> {
    let connection = Connection::open(path).unwrap();
    let mut statement = connection
        .prepare("SELECT name, coalesce(sql, '') FROM sqlite_master ORDER BY name")
        .unwrap();
    let mut rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows.push((
        "schema_version".into(),
        schema_version(&connection).unwrap_or_default(),
    ));
    rows
}

#[test]
fn fresh_database_and_repeated_startup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("priorart.sqlite");
    let store = Store::open(&path).unwrap();
    assert_eq!(
        schema_version(&store.connection).as_deref(),
        Some(SCHEMA_VERSION)
    );
    let foreign_keys: i64 = store
        .connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(foreign_keys, 1);
    let journal: String = store
        .connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    put(&store, LOCAL, "first", Some("record"));
    drop(store);
    let before = dump(&path);
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        assert_eq!(store.connection.total_changes(), 0);
        assert_eq!(
            store.get(LOCAL, "record", None).unwrap().text.as_deref(),
            Some("first")
        );
    }
    assert_eq!(dump(&path), before);
}

fn assert_rejected_unchanged(setup: &str, expected: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("priorart.sqlite");
    Connection::open(&path)
        .unwrap()
        .execute_batch(setup)
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = Store::open(&path).err().unwrap();
    assert!(matches!(
        error,
        StoreError::Schema(SchemaError::UnsupportedVersion(_))
    ));
    assert!(error.to_string().contains(expected), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_file_name("priorart.sqlite-wal").exists());
}

#[test]
fn unknown_schema_versions_are_rejected_without_modification() {
    assert_rejected_unchanged(
        "CREATE TABLE schema_version (version TEXT NOT NULL); \
         INSERT INTO schema_version VALUES ('v9.0.0');",
        "open it with priorart v9.0.0 or later",
    );
}

#[test]
fn unversioned_database_with_a_schema_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("priorart.sqlite");
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE records (id TEXT PRIMARY KEY)")
        .unwrap();
    let before = dump(&path);
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::Schema(SchemaError::Unrecognized(_)))
    ));
    assert_eq!(dump(&path), before);
}

#[test]
fn pending_migrations_commit_or_roll_back_as_one_batch() {
    fn probe(transaction: &Transaction<'_>) -> Result<()> {
        transaction.execute_batch(
            "CREATE TABLE migration_probe (value TEXT); \
             INSERT INTO migration_probe VALUES ('preserved');",
        )?;
        Ok(())
    }
    fn add_row(transaction: &Transaction<'_>) -> Result<()> {
        transaction.execute("INSERT INTO migration_probe VALUES ('rolled back')", [])?;
        Ok(())
    }
    fn fail(transaction: &Transaction<'_>) -> Result<()> {
        transaction.execute_batch("CREATE TABLE partial (value TEXT)")?;
        Err(SchemaError::Unrecognized("third migration failed".into()).into())
    }
    fn reran(_: &Transaction<'_>) -> Result<()> {
        panic!("reran")
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("priorart.sqlite");
    let connection = Connection::open(&path).unwrap();
    let applied = [
        Migration {
            version: "v1.0.0",
            apply: |_| Ok(()),
        },
        Migration {
            version: "v1.1.0",
            apply: probe,
        },
    ];
    run(&connection, &applied).unwrap();
    assert_eq!(schema_version(&connection).as_deref(), Some("v1.1.0"));
    let before = dump(&path);

    let failing = [
        Migration {
            version: "v1.0.0",
            apply: reran,
        },
        Migration {
            version: "v1.1.0",
            apply: reran,
        },
        Migration {
            version: "v1.2.0",
            apply: add_row,
        },
        Migration {
            version: "v1.3.0",
            apply: fail,
        },
    ];
    let error = run(&connection, &failing).unwrap_err();
    assert!(error.to_string().contains("third migration failed"));
    assert_eq!(dump(&path), before);
    let values: String = connection
        .query_row(
            "SELECT group_concat(value) FROM migration_probe",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(values, "preserved");
    assert_eq!(schema_version(&connection).as_deref(), Some("v1.1.0"));
}

#[test]
fn revision_preconditions_are_atomic_across_connections() {
    let (directory, store) = open();
    put(&store, LOCAL, "original", Some("same"));
    assert!(matches!(
        store.put(
            LOCAL,
            "create collision",
            None,
            Some("same"),
            LOCAL_PRINCIPAL_ID,
            Some(0)
        ),
        Err(StoreError::RevisionConflict)
    ));
    assert!(matches!(
        store.put(
            LOCAL,
            "missing target",
            None,
            Some("missing"),
            LOCAL_PRINCIPAL_ID,
            Some(1)
        ),
        Err(StoreError::RevisionConflict)
    ));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = ["left", "right"]
        .into_iter()
        .map(|text| {
            let barrier = barrier.clone();
            let path = directory.path().join("priorart.sqlite");
            std::thread::spawn(move || {
                let store = Store::open(path).unwrap();
                barrier.wait();
                store.put(LOCAL, text, None, Some("same"), LOCAL_PRINCIPAL_ID, Some(1))
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(StoreError::RevisionConflict)))
            .count(),
        1
    );
    assert_eq!(store.get(LOCAL, "same", None).unwrap().revision, 2);
    assert!(matches!(
        store.delete(LOCAL, "same", Some(1)),
        Err(StoreError::RevisionConflict)
    ));
    assert!(store.delete(LOCAL, "same", Some(2)).unwrap());
    assert!(!store.delete(LOCAL, "same", Some(2)).unwrap());
    assert!(matches!(
        store.delete(LOCAL, "same", Some(1)),
        Err(StoreError::RevisionConflict)
    ));
}

#[test]
fn omitted_preconditions_write_unconditionally() {
    let (_directory, store) = open();
    for revision in 1..=2 {
        let written = store
            .put(LOCAL, "text", None, Some("same"), LOCAL_PRINCIPAL_ID, None)
            .unwrap();
        assert_eq!(written.revision, revision);
    }
    assert!(store.delete(LOCAL, "same", None).unwrap());
    assert!(!store.delete(LOCAL, "same", None).unwrap());
    assert!(matches!(
        store.put(LOCAL, "again", None, Some("same"), LOCAL_PRINCIPAL_ID, None),
        Err(StoreError::RecordDeleted { .. })
    ));
}

#[test]
fn record_pages_are_keyed_by_id_and_filter_by_author() {
    let (_directory, store) = open();
    let other = store.create_principal().unwrap();
    for id in ["a", "c", "e"] {
        put(&store, LOCAL, id, Some(id));
    }
    store
        .put(LOCAL, "theirs", None, Some("d"), &other, None)
        .unwrap();
    let ids = |rows: Vec<RecordSummary>| -> Vec<String> {
        rows.into_iter().map(|row| row.latest.record_id).collect()
    };
    let first = store.records(LOCAL, None, "", 2, false).unwrap();
    assert!(first.iter().all(|row| row.latest.text.is_none()));
    assert_eq!(ids(first), ["a", "c"]);
    put(&store, LOCAL, "b", Some("b"));
    put(&store, LOCAL, "a updated", Some("a"));
    store.delete(LOCAL, "e", Some(1)).unwrap();
    assert_eq!(
        ids(store.records(LOCAL, None, "c", 2, false).unwrap()),
        ["d"]
    );
    let mine = store
        .records(LOCAL, Some(LOCAL_PRINCIPAL_ID), "", 10, true)
        .unwrap();
    assert_eq!(mine[0].latest.revision, 2);
    assert_eq!(mine[0].latest.text.as_deref(), Some("a updated"));
    assert_ne!(mine[0].created_at, mine[0].latest.created_at);
    assert_eq!(ids(mine), ["a", "b", "c"]);
    assert_eq!(
        ids(store.records(LOCAL, Some(&other), "", 10, false).unwrap()),
        ["d"]
    );
}
