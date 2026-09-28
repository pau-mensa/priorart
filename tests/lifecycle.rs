mod common;

use priorart::{
    auth::{Grant, Operation as Op, RequestContext},
    config::Settings,
    index::collection_index_path,
    service::{DeleteOptions, ReportOptions, Service, WriteOptions, DATABASE_FILE},
    store::{Store, StoreError, Visibility, LOCAL_COLLECTION_ID as LOCAL},
};
use std::{path::Path, sync::Arc};

fn service(path: &Path) -> Service {
    Service::new(
        Settings {
            data_dir: path.into(),
            ..Default::default()
        },
        Some(Arc::new(common::FakeEncoder::new())),
    )
    .unwrap()
}
fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
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
fn record_purge_erases_feedback_and_old_generations_without_recreating_on_retry() {
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
    let receipt = service
        .search(&caller, LOCAL, "secret", None, 10)
        .unwrap()
        .search_id
        .unwrap();
    service
        .report(
            &caller,
            LOCAL,
            &record,
            "quotes secret text",
            ReportOptions {
                revision: Some(1),
                search_id: Some(&receipt),
                idempotency_key: Some("private-report"),
            },
        )
        .unwrap();
    let path = collection_index_path(dir.path(), LOCAL);
    let previous: Vec<_> = std::fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "CURRENT")
        .collect();
    let options = DeleteOptions {
        expected_revision: Some(1),
        idempotency_key: Some("delete-once"),
    };
    let deleted = service.delete(&caller, LOCAL, &record, options).unwrap();
    assert_eq!(
        service.delete(&caller, LOCAL, &record, options).unwrap(),
        deleted
    );
    for name in previous {
        assert!(!path.join(name).exists());
    }
    let db = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
    for table in ["reports", "published_reports"] {
        assert_eq!(count(&db, table, LOCAL), 0);
    }
    assert_eq!(count(&db, "revisions", LOCAL), 1);
    assert_eq!(
        service
            .search_receipt(&caller, LOCAL, &receipt)
            .unwrap()
            .hits
            .len(),
        1
    );
    let cleared: i64 = db
        .query_row(
            "SELECT count(*) FROM mutations WHERE payload_digest = '' AND result = 'null'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cleared, 2);
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
    let hits = service
        .search(&caller, LOCAL, "text", None, 10)
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "survivor");
}

#[test]
fn collection_purge_revokes_only_its_grants_and_never_recreates_local() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
    let principal = store.create_principal().unwrap();
    let account = store.create_account(&principal).unwrap();
    let public = store
        .create_collection(&account, Visibility::Public)
        .unwrap();
    let other = store
        .create_collection(&account, Visibility::Restricted)
        .unwrap();
    let grants: Vec<_> = [&public, &other]
        .into_iter()
        .flat_map(|c| {
            [
                Op::Read,
                Op::Contribute,
                Op::Admin,
                Op::Report,
                Op::ReportPublish,
            ]
            .map(|op| Grant::new(c, op))
        })
        .collect();
    let key = store
        .issue_local_credential(&principal, &grants, None)
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
                publish: true,
                ..Default::default()
            },
        )
        .unwrap();
    let report = service
        .report(
            &caller,
            &public,
            "same",
            "private text",
            ReportOptions {
                revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    service
        .publish_report(&caller, &public, &report.value, "published text", None)
        .unwrap();
    assert!(service
        .delete_collection(&RequestContext::anonymous(), &public)
        .is_err());
    let other_path = collection_index_path(dir.path(), &other);
    let other_current = std::fs::read(other_path.join("CURRENT")).unwrap();
    service.delete_collection(&caller, &public).unwrap();
    assert!(!collection_index_path(dir.path(), &public).exists());
    let db = rusqlite::Connection::open(store.path()).unwrap();
    for table in [
        "records",
        "revisions",
        "reports",
        "published_reports",
        "searches",
        "search_hits",
        "index_documents",
        "index_state",
        "credential_grants",
        "mutations",
        "purge_jobs",
    ] {
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
    assert_eq!(
        std::fs::read(other_path.join("CURRENT")).unwrap(),
        other_current
    );
    assert!(service.search(&fresh, &public, "public", None, 10).is_err());
    assert!(db
        .execute(
            "INSERT INTO collections VALUES (?1, ?2, 'public', 'now')",
            (&public, &account)
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

#[test]
fn restored_old_indexes_cannot_resurrect_a_purged_record() {
    let dir = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let caller = RequestContext::local();
    let service = service(dir.path());
    service
        .put(
            &caller,
            LOCAL,
            "secret",
            None,
            Some("gone"),
            Default::default(),
        )
        .unwrap();
    let path = collection_index_path(dir.path(), LOCAL);
    copy_tree(&path, backup.path());
    service
        .delete(
            &caller,
            LOCAL,
            "gone",
            DeleteOptions {
                expected_revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    drop(service);
    std::fs::remove_dir_all(&path).unwrap();
    copy_tree(backup.path(), &path);
    let service = self::service(dir.path());
    assert!(service
        .search(&caller, LOCAL, "secret", None, 10)
        .unwrap()
        .hits
        .is_empty());
    for entry in std::fs::read_dir(backup.path()).unwrap() {
        let name = entry.unwrap().file_name();
        if name != "CURRENT" {
            assert!(!path.join(name).exists());
        }
    }
}
