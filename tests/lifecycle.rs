use priorart::{
    auth::{Grant, Operation as Op, RequestContext},
    config::Settings,
    service::{DeleteOptions, Service, WriteOptions, DATABASE_FILE},
    store::{Store, StoreError, Visibility, LOCAL_COLLECTION_ID as LOCAL},
};
use std::path::Path;

fn service(path: &Path) -> Service {
    Service::open(Settings {
        data_dir: path.into(),
        ..Default::default()
    })
    .unwrap()
}
fn count(connection: &rusqlite::Connection, table: &str, collection: &str) -> i64 {
    connection
        .query_row(
            &format!("SELECT count(*) FROM {table} WHERE collection_id = ?1"),
            [collection],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn record_purge_erases_content_without_recreating_on_retry() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path());
    let caller = RequestContext::local();
    let options = WriteOptions {
        idempotency_key: Some("original-create"),
        ..Default::default()
    };
    let record = service
        .put(&caller, LOCAL, "secret text", None, None, options)
        .unwrap()
        .value
        .0;
    service
        .put(
            &caller,
            LOCAL,
            "surviving text",
            None,
            Some("survivor"),
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        service
            .search(&caller, &[LOCAL], "secret", None, 10)
            .unwrap()[0]
            .id,
        record
    );
    let options = DeleteOptions {
        expected_revision: Some(1),
        idempotency_key: Some("delete-once"),
    };
    let deleted = service.delete(&caller, LOCAL, &record, options).unwrap();
    assert_eq!(
        service.delete(&caller, LOCAL, &record, options).unwrap(),
        deleted
    );
    let db = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
    assert_eq!(count(&db, "revisions", LOCAL), 1);
    let cleared: i64 = db
        .query_row(
            "SELECT count(*) FROM mutations WHERE payload_digest = '' AND result = 'null'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cleared, 1);
    assert!(service
        .put(
            &caller,
            LOCAL,
            "secret text",
            None,
            None,
            WriteOptions {
                idempotency_key: Some("original-create"),
                ..Default::default()
            }
        )
        .is_err());
    let hits = service.search(&caller, &[LOCAL], "text", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "survivor");
    assert!(service
        .search(&caller, &[LOCAL], "secret", None, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn collection_purge_revokes_only_its_grants_and_never_recreates_local() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
    let principal = store.create_principal().unwrap();
    let public = store
        .create_collection(&principal, Visibility::Public)
        .unwrap();
    let other = store
        .create_collection(&principal, Visibility::Restricted)
        .unwrap();
    let grants: Vec<_> = [&public, &other]
        .into_iter()
        .flat_map(|c| [Grant::new(c, Op::Admin)])
        .collect();
    let key = store
        .issue_credential(&principal, &grants, None)
        .unwrap()
        .into_secret();
    let service = service(dir.path());
    let caller = service.authenticate(&key).unwrap();
    service
        .put(
            &caller,
            &other,
            "other collection",
            None,
            Some("same"),
            Default::default(),
        )
        .unwrap();
    service
        .put(
            &caller,
            &public,
            "public record",
            None,
            Some("same"),
            WriteOptions {
                ..Default::default()
            },
        )
        .unwrap();
    assert!(service
        .delete_collection(&RequestContext::anonymous(), &public)
        .is_err());
    service.delete_collection(&caller, &public).unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    for table in ["records", "revisions", "credential_grants", "mutations"] {
        assert_eq!(count(&db, table, &public), 0, "{table}");
    }
    assert!(service.get(&caller, &other, "same", None).is_err());
    let fresh = service.authenticate(&key).unwrap();
    assert_eq!(
        service
            .get(&fresh, &other, "same", None)
            .unwrap()
            .text
            .as_deref(),
        Some("other collection")
    );
    assert!(service
        .search(&fresh, &[&public], "public", None, 10)
        .is_err());
    assert!(db
        .execute(
            "INSERT INTO collections (id, owner_principal_id, visibility, created_at) VALUES (?1, ?2, 'public', 'now')",
            (&public, &principal)
        )
        .is_err());
    service
        .delete_collection(&RequestContext::local(), LOCAL)
        .unwrap();
    drop(service);
    let _reopened = self::service(dir.path());
    assert!(matches!(
        store.get_collection(LOCAL),
        Err(StoreError::CollectionNotFound(_))
    ));
}

#[cfg(unix)]
#[test]
fn data_directory_and_databases_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let data = parent.path().join("data");
    let service = Service::open(Settings {
        data_dir: data.clone(),
        search_log_days: Some(1),
        ..Default::default()
    })
    .unwrap();
    service
        .put(
            &RequestContext::local(),
            LOCAL,
            "text",
            None,
            None,
            WriteOptions::default(),
        )
        .unwrap();
    let mode = |name: &str| {
        std::fs::metadata(data.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(""), 0o700);
    for file in [
        DATABASE_FILE,
        "priorart.sqlite-wal",
        "searchlog.sqlite",
        "writer.lock",
    ] {
        assert_eq!(mode(file), 0o600, "{file}");
    }
}
