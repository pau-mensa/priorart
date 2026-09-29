mod common;

use priorart::{
    auth::{Grant, Operation as Op, RequestContext},
    config::{ServerMode, Settings},
    service::{DeleteOptions, ImportOptions, Service, ServiceError, WriteOptions, DATABASE_FILE},
    store::{ExportCursor, Store, StoreError, TransferRecord, Visibility},
};
use serde_json::{json, Value};
use std::sync::Arc;

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    service: Arc<Service>,
    encoder: Arc<common::FakeEncoder>,
    source: String,
    destination: String,
    public: String,
    alice: String,
    bob: String,
    alice_key: String,
    bob_key: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
        let alice = store.create_principal().unwrap();
        let bob = store.create_principal().unwrap();
        let source = store
            .create_collection(&alice, Visibility::Restricted)
            .unwrap();
        let destination = store
            .create_collection(&bob, Visibility::Restricted)
            .unwrap();
        let public = store.create_collection(&bob, Visibility::Public).unwrap();
        let permissions = [
            Op::Read,
            Op::Export,
            Op::Contribute,
            Op::Update,
            Op::Delete,
            Op::Admin,
        ];
        let alice_key = store
            .issue_local_credential(
                &alice,
                &permissions
                    .iter()
                    .map(|p| Grant::new(&source, *p))
                    .collect::<Vec<_>>(),
                None,
            )
            .unwrap()
            .into_secret();
        let bob_key = store
            .issue_local_credential(
                &bob,
                &[&destination, &public]
                    .into_iter()
                    .flat_map(|c| permissions.iter().map(move |p| Grant::new(c, *p)))
                    .collect::<Vec<_>>(),
                None,
            )
            .unwrap()
            .into_secret();
        for (text, expected) in [("first revision", None), ("second revision", Some(1))] {
            store
                .put(
                    &source,
                    text,
                    Some(json!({"kind":"test"}).as_object().unwrap()),
                    Some("record"),
                    &alice,
                    expected,
                )
                .unwrap();
        }
        let encoder = Arc::new(common::FakeEncoder::new());
        let service = Arc::new(
            Service::new(
                Settings {
                    data_dir: dir.path().into(),
                    mode: ServerMode::Authenticated,
                    ..Default::default()
                },
                Some(encoder.clone()),
            )
            .unwrap(),
        );
        Self {
            dir,
            store,
            service,
            encoder,
            source,
            destination,
            public,
            alice,
            bob,
            alice_key,
            bob_key,
        }
    }
    fn context(&self, key: &str) -> RequestContext {
        self.service.authenticate(key).unwrap()
    }
    fn records(&self) -> Vec<TransferRecord> {
        let context = self.context(&self.alice_key);
        let generation = self
            .service
            .export_generation(&context, &self.source)
            .unwrap();
        let mut after = ExportCursor::default();
        let mut rows = Vec::new();
        while let Some(row) = self
            .service
            .export_next(&context, &self.source, generation, &after)
            .unwrap()
        {
            after = ExportCursor {
                record_id: row.record_id.clone(),
                revision: row.revision,
            };
            rows.push(row);
        }
        rows
    }
}
fn options() -> ImportOptions<'static> {
    ImportOptions {
        batch_key: "transfer-once",
        visibility: Visibility::Restricted,
        publish: false,
    }
}

#[test]
fn export_is_scoped_generation_checked_and_never_loads_an_encoder() {
    let f = Fixture::new();
    f.encoder.set_failing(true);
    let rows = f.records();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].revision, 1);
    assert_eq!(rows[1].revision, 2);
    assert_eq!(
        rows[0].author_principal_id.as_deref(),
        Some(f.alice.as_str())
    );
    assert_eq!(f.encoder.calls(), 0);
    let bob = f.context(&f.bob_key);
    assert!(f.service.export_generation(&bob, &f.source).is_err());
    assert!(f
        .service
        .export_generation(&RequestContext::anonymous(), &f.public)
        .is_err());
    let read_only = f
        .store
        .issue_local_credential(&f.bob, &[Grant::new(&f.public, Op::Read)], None)
        .unwrap()
        .into_secret();
    assert!(f
        .service
        .export_generation(&f.context(&read_only), &f.public)
        .is_err());
    let export_only = f
        .store
        .issue_local_credential(&f.bob, &[Grant::new(&f.destination, Op::Export)], None)
        .unwrap()
        .into_secret();
    assert!(f
        .service
        .export_generation(&f.context(&export_only), &f.destination)
        .is_err());
    let alice = f.context(&f.alice_key);
    let generation = f.service.export_generation(&alice, &f.source).unwrap();
    f.store
        .put(
            &f.source,
            "concurrent edit",
            None,
            Some("record"),
            &f.alice,
            Some(2),
        )
        .unwrap();
    assert!(matches!(
        f.service
            .export_next(&alice, &f.source, generation, &ExportCursor::default()),
        Err(ServiceError::Store(StoreError::ExportChanged))
    ));
    let generation = f.service.export_generation(&alice, &f.source).unwrap();
    f.store
        .revoke_local_credential(&alice.credential().unwrap().id)
        .unwrap();
    assert!(f
        .service
        .export_next(&alice, &f.source, generation, &ExportCursor::default())
        .is_err());
}

#[test]
fn import_preserves_history_as_new_authored_records_and_retries_without_duplicates() {
    let f = Fixture::new();
    let records = f.records();
    let bob = f.context(&f.bob_key);
    let first = f
        .service
        .import_revision(&bob, &f.destination, &records[0], options())
        .unwrap();
    let second = f
        .service
        .import_revision(&bob, &f.destination, &records[1], options())
        .unwrap();
    assert_eq!(first.value.0, second.value.0);
    assert_eq!((first.value.1, second.value.1), (1, 2));
    assert_eq!(
        f.service
            .import_revision(&bob, &f.destination, &records[0], options())
            .unwrap(),
        first
    );
    assert_eq!(
        f.service
            .import_revision(&bob, &f.destination, &records[1], options())
            .unwrap(),
        second
    );
    let imported = f
        .service
        .get(&bob, &f.destination, &first.value.0, Some(1))
        .unwrap();
    assert_eq!(
        imported.author_principal_id.as_deref(),
        Some(f.bob.as_str())
    );
    assert_eq!(imported.text.as_deref(), Some("first revision"));
    let db = rusqlite::Connection::open(f.dir.path().join(DATABASE_FILE)).unwrap();
    let claim: String = db
        .query_row(
            "SELECT source_author_principal_id FROM import_provenance LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claim, f.alice);
    let mut changed = records[0].clone();
    changed.text = "changed retry".into();
    assert!(matches!(
        f.service
            .import_revision(&bob, &f.destination, &changed, options()),
        Err(ServiceError::Store(StoreError::IdempotencyConflict))
    ));
    let hits = f
        .service
        .search(&bob, &f.destination, "second", None, 10)
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].revision, 2);
    f.service
        .delete(
            &bob,
            &f.destination,
            &first.value.0,
            DeleteOptions {
                expected_revision: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM import_provenance", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(f
        .service
        .import_revision(&bob, &f.destination, &records[0], options())
        .is_err());
    let mut unseen = records[1].clone();
    unseen.revision = 3;
    assert!(matches!(
        f.service
            .import_revision(&bob, &f.destination, &unseen, options()),
        Err(ServiceError::Store(StoreError::RecordDeleted { .. }))
    ));
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM records WHERE collection_id = ?1 AND deleted_at IS NULL",
            [&f.destination],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(f.store.get(&f.source, "record", Some(1)).is_ok());
}

#[test]
fn import_never_trusts_source_authority_or_overwrites_intervening_edits() {
    let f = Fixture::new();
    let mut row = f.records()[0].clone();
    let bob = f.context(&f.bob_key);
    assert!(f
        .service
        .import_revision(&bob, &f.source, &row, options())
        .is_err());
    assert!(f
        .service
        .import_revision(&bob, &f.public, &row, options())
        .is_err());
    assert!(f
        .service
        .import_revision(
            &bob,
            &f.public,
            &row,
            ImportOptions {
                visibility: Visibility::Public,
                ..options()
            }
        )
        .is_err());
    row.collection_id = "forged-source".into();
    row.author_principal_id = Some("forged-author".into());
    let public = f
        .service
        .import_revision(
            &bob,
            &f.public,
            &row,
            ImportOptions {
                visibility: Visibility::Public,
                publish: true,
                ..options()
            },
        )
        .unwrap();
    let record = f
        .service
        .get(
            &RequestContext::anonymous(),
            &f.public,
            &public.value.0,
            None,
        )
        .unwrap();
    assert_eq!(record.author_principal_id.as_deref(), Some(f.bob.as_str()));
    let exported = f
        .service
        .export_next(
            &bob,
            &f.public,
            f.service.export_generation(&bob, &f.public).unwrap(),
            &ExportCursor::default(),
        )
        .unwrap()
        .unwrap();
    assert!(!serde_json::to_string(&exported).unwrap().contains("forged"));
    let copied = f
        .service
        .import_revision(&bob, &f.destination, &row, options())
        .unwrap();
    f.service
        .put(
            &bob,
            &f.destination,
            "manual edit",
            None,
            Some(&copied.value.0),
            WriteOptions {
                expected_revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    row.revision = 2;
    assert!(matches!(
        f.service
            .import_revision(&bob, &f.destination, &row, options()),
        Err(ServiceError::Store(StoreError::RevisionConflict))
    ));
    let contribute_only = f
        .store
        .issue_local_credential(&f.bob, &[Grant::new(&f.destination, Op::Contribute)], None)
        .unwrap()
        .into_secret();
    let limited = f.context(&contribute_only);
    assert!(f
        .service
        .import_revision(&limited, &f.destination, &row, options())
        .is_err());
    let bob = f.context(&f.bob_key);
    row.version = 99;
    assert!(f
        .service
        .import_revision(&bob, &f.destination, &row, options())
        .is_err());
    f.service.delete_collection(&bob, &f.public).unwrap();
    let db = rusqlite::Connection::open(f.dir.path().join(DATABASE_FILE)).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM import_provenance WHERE collection_id = ?1",
            [&f.public],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM import_targets WHERE collection_id = ?1",
            [&f.public],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

fn frames(body: &str) -> Vec<Value> {
    body.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn paginated_streams_round_trip_and_report_partial_imports() {
    let f = Fixture::new();
    let url = common::spawn(f.service.clone()).await;
    let client = reqwest::Client::new();
    let export = format!("{url}/v1/collections/{}/export", f.source);
    let first = client
        .get(format!("{export}?limit=1"))
        .bearer_auth(&f.alice_key)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["content-type"], "application/x-ndjson");
    let first = first.text().await.unwrap();
    let first_rows = frames(&first);
    assert_eq!(first_rows.len(), 2);
    assert_eq!(first_rows[0]["record"]["revision"], 1);
    let footer = first_rows.last().unwrap();
    let second = client
        .get(format!(
            "{export}?limit=1&after_record=record&after_revision=1&generation={}",
            footer["generation"]
        ))
        .bearer_auth(&f.alice_key)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(frames(&second).last().unwrap()["next_cursor"].is_null());
    let import = format!(
        "{url}/v1/collections/{}/import?visibility=restricted",
        f.destination
    );
    let send = |body: String| {
        client
            .post(&import)
            .bearer_auth(&f.bob_key)
            .header("content-type", "application/x-ndjson")
            .header("idempotency-key", "http-batch")
            .body(body)
    };
    let accepted = send(first.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(frames(&accepted).last().unwrap()["type"], "end");
    let replayed = send(first).send().await.unwrap().text().await.unwrap();
    assert_eq!(replayed, accepted);
    let second_result = send(second).send().await.unwrap().text().await.unwrap();
    assert_eq!(frames(&second_result)[0]["revision"], 2);
    let truncated = format!("{}\n", first_rows[0]);
    let partial = send(truncated).send().await.unwrap().text().await.unwrap();
    assert_eq!(frames(&partial)[0]["type"], "imported");
    assert_eq!(frames(&partial).last().unwrap()["type"], "error");
    let invalid = send("{private malformed payload}\n".into())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!invalid.contains("private malformed payload"));
    assert_eq!(frames(&invalid)[0]["type"], "error");
    assert_eq!(client.get(&export).send().await.unwrap().status(), 404);
    assert_eq!(
        client
            .post(&import)
            .bearer_auth(&f.bob_key)
            .header("content-type", "application/x-ndjson")
            .body(accepted)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}
