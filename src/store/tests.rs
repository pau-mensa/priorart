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

fn store_constraint<T: fmt::Debug>(result: Result<T>) -> bool {
    matches!(
        result,
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
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
fn delete_nulls_text_and_blocks_reuse() {
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
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(text, metadata, digest)| text.is_none()
        && metadata.is_none()
        && !digest.is_empty()));
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

#[test]
fn filters_match_latest_metadata() {
    let (_directory, store) = open();
    let add = |text, tags: Value, record| {
        store
            .put(
                LOCAL,
                text,
                Some(&metadata(tags)),
                Some(record),
                LOCAL_PRINCIPAL_ID,
                store.get(LOCAL, record, None).ok().map(|r| r.revision),
            )
            .unwrap();
    };
    add("x", json!({"lang": "python", "gpu": true}), "p");
    add("y", json!({"lang": "rust", "size": 2}), "r");
    add("z", json!({"lang": "python"}), "p");
    put(&store, LOCAL, "w", Some("n"));
    let matching = |filters: Value| {
        let mut ids = store
            .matching_record_ids(LOCAL, &metadata(filters))
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>();
        ids.sort();
        ids
    };
    assert_eq!(matching(json!({"lang": "python"})), ["p"]);
    assert!(matching(json!({"lang": "python", "gpu": true})).is_empty());
    assert_eq!(matching(json!({"lang": "rust"})), ["r"]);
    assert_eq!(matching(json!({"size": 2.0})), ["r"]);
    assert!(matching(json!({"missing": 1})).is_empty());
}

#[test]
fn reports_round_trip_including_deleted_records() {
    let (_directory, store) = open();
    put(&store, LOCAL, "x", Some("rec"));
    let hit = SearchHit {
        record_id: "rec".into(),
        revision: 1,
        score: 1.0,
    };
    let search = store
        .log_search(
            LOCAL,
            "q",
            None,
            &[hit],
            &Metadata::new(),
            LOCAL_PRINCIPAL_ID,
        )
        .unwrap();
    assert!(store.has_search(LOCAL, &search).unwrap());
    assert!(!store.has_search(LOCAL, "nope").unwrap());
    let report = store
        .add_report(
            LOCAL,
            "rec",
            Some(1),
            Some(&search),
            "worked",
            LOCAL_PRINCIPAL_ID,
        )
        .unwrap();
    store.delete(LOCAL, "rec", Some(1)).unwrap();
    let reports = store.reports_for(LOCAL, "rec").unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].id, report);
    assert_eq!(reports[0].search_id.as_deref(), Some(search.as_str()));
    assert_eq!(reports[0].revision, Some(1));
    assert_eq!(reports[0].text, "worked");
    assert!(matches!(
        store.add_report(LOCAL, "nope", None, None, "x", LOCAL_PRINCIPAL_ID),
        Err(StoreError::RecordNotFound { .. })
    ));
    assert!(matches!(
        store.add_report(LOCAL, "rec", Some(9), None, "x", LOCAL_PRINCIPAL_ID),
        Err(StoreError::RecordNotFound {
            revision: Some(9),
            ..
        })
    ));
    assert!(matches!(
        store.add_report(LOCAL, "rec", None, Some("bogus"), "x", LOCAL_PRINCIPAL_ID),
        Err(StoreError::SearchNotFound { .. })
    ));
}

#[test]
fn index_mirror_round_trips() {
    let (_directory, store) = open();
    for record in ["a", "b"] {
        put(&store, LOCAL, "text", Some(record));
        put(&store, LOCAL, "text", Some(record));
    }
    assert!(store.index_documents(LOCAL).unwrap().is_empty());
    store
        .replace_index_documents(LOCAL, &[("b", 1), ("a", 2)], Some("fake"))
        .unwrap();
    let entries = store.index_documents(LOCAL).unwrap();
    assert_eq!(
        entries,
        [
            IndexEntry {
                internal_id: 0,
                record_id: "b".into(),
                revision: 1
            },
            IndexEntry {
                internal_id: 1,
                record_id: "a".into(),
                revision: 2
            },
        ]
    );
    assert_eq!(store.index_encoder(LOCAL).unwrap().as_deref(), Some("fake"));
    store.replace_index_documents(LOCAL, &[], None).unwrap();
    assert!(store.index_documents(LOCAL).unwrap().is_empty());
    assert_eq!(store.index_encoder(LOCAL).unwrap(), None);
}

#[test]
fn index_mirror_updates_shift_later_ids() {
    let (_directory, store) = open();
    for record in ["a", "b", "c", "d"] {
        put(&store, LOCAL, "text", Some(record));
    }
    put(&store, LOCAL, "text", Some("b"));
    let rows = |store: &Store| {
        store
            .index_documents(LOCAL)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.internal_id, entry.record_id, entry.revision))
            .collect::<Vec<_>>()
    };
    store
        .replace_index_documents(LOCAL, &[("a", 1), ("b", 1), ("c", 1), ("d", 1)], None)
        .unwrap();
    store
        .update_index_documents(LOCAL, Some(1), Some(("b", 2)), Some("fake"))
        .unwrap();
    assert_eq!(
        rows(&store),
        [
            (0, "a".into(), 1),
            (1, "c".into(), 1),
            (2, "d".into(), 1),
            (3, "b".into(), 2)
        ]
    );
    assert_eq!(store.index_encoder(LOCAL).unwrap().as_deref(), Some("fake"));
    store
        .update_index_documents(LOCAL, Some(3), None, None)
        .unwrap();
    store
        .update_index_documents(LOCAL, Some(0), None, None)
        .unwrap();
    assert_eq!(rows(&store), [(0, "c".into(), 1), (1, "d".into(), 1)]);
    for _ in 0..2 {
        store
            .update_index_documents(LOCAL, Some(0), None, None)
            .unwrap();
    }
    assert!(store.index_documents(LOCAL).unwrap().is_empty());
    assert_eq!(store.index_encoder(LOCAL).unwrap(), None);
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
    let account_a = store.create_account(&alice).unwrap();
    let account_b = store.create_account(&bob).unwrap();
    let a = store
        .create_collection(&account_a, Visibility::Restricted)
        .unwrap();
    let b = store
        .create_collection(&account_b, Visibility::Restricted)
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
        store.get_collection(LOCAL).unwrap().owner_account_id,
        LOCAL_ACCOUNT_ID
    );
    assert_ne!(
        store.get_collection(&a).unwrap().owner_account_id,
        store.get_collection(&b).unwrap().owner_account_id
    );
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
    let owner = store.get_collection(&a).unwrap().owner_account_id;
    assert!(is_constraint(store.connection.execute(
        "UPDATE collections SET visibility = 'public' WHERE id = ?1",
        [&a],
    )));
    assert!(store_constraint(
        store.create_collection("missing", Visibility::Restricted)
    ));
    assert!(store_constraint(store.create_account("missing")));
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
fn reads_deletes_reports_and_mirrors_are_scoped() {
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
    for scope in [&a, &b] {
        store
            .replace_index_documents(scope, &[("same", 1)], Some(scope))
            .unwrap();
        store
            .add_report(scope, "same", Some(1), None, scope, &alice)
            .unwrap();
    }
    assert!(matches!(
        store.get(&a, "only-b", None),
        Err(StoreError::RecordNotFound { .. })
    ));
    let live = store.live_documents(&a).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].record_id, "same");
    let filters = metadata(json!({"tag": "shared"}));
    assert_eq!(
        store.matching_record_ids(&a, &filters).unwrap(),
        HashSet::from(["same".to_owned()])
    );
    let reports = store.reports_for(&a, "same").unwrap();
    assert_eq!(
        (reports[0].collection_id.as_str(), reports[0].text.as_str()),
        (a.as_str(), a.as_str())
    );
    store.delete(&a, "same", Some(1)).unwrap();
    store.replace_index_documents(&a, &[], None).unwrap();
    assert!(store.live_documents(&a).unwrap().is_empty());
    assert_eq!(
        store.get(&b, "same", None).unwrap().text.as_deref(),
        Some("bob second revision")
    );
    assert_eq!(store.index_documents(&b).unwrap().len(), 1);
    assert_eq!(store.index_encoder(&b).unwrap(), Some(b.clone()));
    assert_eq!(store.index_encoder(&a).unwrap(), None);
    assert!(matches!(
        store.add_report(&a, "same", Some(2), None, "wrong revision", &alice),
        Err(StoreError::RecordNotFound { .. })
    ));
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
    let search = store
        .log_search(&b, "q", None, &[], &Metadata::new(), &bob)
        .unwrap();
    let connection = &store.connection;
    assert!(is_constraint(connection.execute(
        "INSERT INTO revisions (collection_id, record_id, revision, text_sha256, created_at) \
         VALUES (?1, 'only-b', 1, 'hash', 'now')",
        [&a],
    )));
    assert!(is_constraint(connection.execute(
        "INSERT INTO reports (collection_id, id, record_id, revision, text, created_at) \
         VALUES (?1, 'bad-revision', 'same', 2, 'text', 'now')",
        [&a],
    )));
    assert!(is_constraint(connection.execute(
        "INSERT INTO reports (collection_id, id, record_id, search_id, text, created_at) \
         VALUES (?1, 'bad-search', 'same', ?2, 'text', 'now')",
        [&a, &search],
    )));
    store
        .replace_index_documents(&a, &[("same", 1)], Some("old"))
        .unwrap();
    assert!(store_constraint(store.replace_index_documents(
        &a,
        &[("same", 2)],
        None
    )));
    assert_eq!(store.index_documents(&a).unwrap().len(), 1);
    assert_eq!(store.index_encoder(&a).unwrap().as_deref(), Some("old"));
    let foreign = SearchHit {
        record_id: "same".into(),
        revision: 2,
        score: 1.0,
    };
    assert!(store_constraint(store.log_search(
        &a,
        "q",
        None,
        &[foreign],
        &Metadata::new(),
        &alice
    )));
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM searches WHERE collection_id = ?1",
            [&a],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert!(!store.has_search(&a, &search).unwrap());
    assert!(is_constraint(connection.execute(
        "INSERT INTO search_hits VALUES (?1, ?2, 0, 'same', 1, 1.0)",
        [&a, &search],
    )));
}

fn user_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
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
    rows.push(("user_version".into(), user_version(&connection).to_string()));
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
    assert_eq!(user_version(&store.connection), 0);
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
fn python_databases_are_rejected_without_modification() {
    for version in [1, 2] {
        assert_rejected_unchanged(
            &format!("CREATE TABLE records (id TEXT); PRAGMA user_version = {version};"),
            "open it with priorart v0.1.0",
        );
    }
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
            "missing precondition",
            None,
            Some("same"),
            LOCAL_PRINCIPAL_ID,
            None
        ),
        Err(StoreError::RevisionRequired)
    ));
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
    assert!(matches!(
        store.delete(LOCAL, "same", None),
        Err(StoreError::RevisionRequired)
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
