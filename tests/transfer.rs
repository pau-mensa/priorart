mod common;

use priorart::{
    auth::{Grant, Operation as Op, RequestContext},
    config::{ServerMode, Settings},
    service::{DeleteOptions, Service, ServiceError, WriteOptions, DATABASE_FILE},
    store::{ExportCursor, Store, StoreError, TransferRecord, Visibility},
};
use serde_json::{json, Value};
use std::sync::Arc;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    service: Arc<Service>,
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
        let permissions = [Op::Admin];
        let alice_key = store
            .issue_credential(
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
            .issue_credential(
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
        let service = Arc::new(
            Service::open(Settings {
                data_dir: dir.path().into(),
                mode: ServerMode::Authenticated,
                ..Default::default()
            })
            .unwrap(),
        );
        Self {
            _dir: dir,
            store,
            service,
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
const BATCH: &str = "transfer-once";

#[test]
fn export_is_scoped_and_generation_checked() {
    let f = Fixture::new();
    let rows = f.records();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].revision, 1);
    assert_eq!(rows[1].revision, 2);
    assert_eq!(
        rows[0].author_principal_id.as_deref(),
        Some(f.alice.as_str())
    );
    let bob = f.context(&f.bob_key);
    assert!(f.service.export_generation(&bob, &f.source).is_err());
    assert!(f
        .service
        .export_generation(&RequestContext::anonymous(), &f.public)
        .is_err());
    let read_only = f
        .store
        .issue_credential(&f.bob, &[Grant::new(&f.public, Op::Read)], None)
        .unwrap()
        .into_secret();
    assert!(f
        .service
        .export_generation(&f.context(&read_only), &f.public)
        .is_err());
    let writer = f
        .store
        .issue_credential(&f.bob, &[Grant::new(&f.destination, Op::Write)], None)
        .unwrap()
        .into_secret();
    assert!(f
        .service
        .export_generation(&f.context(&writer), &f.destination)
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
        .revoke_credential(&alice.credential().unwrap().id)
        .unwrap();
    assert!(f
        .service
        .export_next(&alice, &f.source, generation, &ExportCursor::default())
        .is_err());
}

#[test]
fn import_keeps_ids_and_history_and_retries_without_duplicates() {
    let f = Fixture::new();
    let records = f.records();
    let bob = f.context(&f.bob_key);
    let import = |row: &TransferRecord| {
        f.service
            .import_revision(&bob, &f.destination, row, BATCH, false)
    };
    let first = import(&records[0]).unwrap();
    let second = import(&records[1]).unwrap();
    assert_eq!(first.value, ("record".to_owned(), 1));
    assert_eq!(second.value, ("record".to_owned(), 2));
    assert_eq!(import(&records[0]).unwrap(), first);
    assert_eq!(import(&records[1]).unwrap(), second);
    let imported = f
        .service
        .get(&bob, &f.destination, "record", Some(1))
        .unwrap();
    assert_eq!(
        imported.author_principal_id.as_deref(),
        Some(f.bob.as_str())
    );
    assert_eq!(imported.text.as_deref(), Some("first revision"));
    let mut changed = records[0].clone();
    changed.text = "changed retry".into();
    assert!(matches!(
        import(&changed),
        Err(ServiceError::Store(StoreError::IdempotencyConflict))
    ));
    let hits = f
        .service
        .search(&bob, &[&f.destination], "second", None, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].revision, 2);
    f.service
        .delete(&bob, &f.destination, "record", DeleteOptions::default())
        .unwrap();
    assert!(import(&records[0]).is_err());
    let mut unseen = records[1].clone();
    unseen.revision = 3;
    for overwrite in [false, true] {
        assert!(matches!(
            f.service
                .import_revision(&bob, &f.destination, &unseen, BATCH, overwrite),
            Err(ServiceError::Store(StoreError::RecordDeleted { .. }))
        ));
    }
    assert!(f.store.get(&f.source, "record", Some(1)).is_ok());
}

#[test]
fn existing_ids_conflict_unless_overwriting() {
    let f = Fixture::new();
    let records = f.records();
    let bob = f.context(&f.bob_key);
    f.service
        .put(
            &bob,
            &f.destination,
            "local note",
            None,
            Some("record"),
            WriteOptions::default(),
        )
        .unwrap();
    assert!(matches!(
        f.service
            .import_revision(&bob, &f.destination, &records[0], BATCH, false),
        Err(ServiceError::Store(StoreError::RevisionConflict))
    ));
    for (row, revision) in records.iter().zip([2, 3]) {
        let imported = f
            .service
            .import_revision(&bob, &f.destination, row, "overwrite", true)
            .unwrap();
        assert_eq!(imported.value, ("record".to_owned(), revision));
    }
    let text = |revision| {
        f.service
            .get(&bob, &f.destination, "record", revision)
            .unwrap()
            .text
    };
    assert_eq!(text(None).as_deref(), Some("second revision"));
    assert_eq!(text(Some(1)).as_deref(), Some("local note"));
    f.service
        .import_revision(&bob, &f.public, &records[0], BATCH, false)
        .unwrap();
    assert!(matches!(
        f.service
            .import_revision(&bob, &f.public, &records[0], BATCH, true),
        Err(ServiceError::Store(StoreError::IdempotencyConflict))
    ));
    let writer = f
        .store
        .issue_credential(&f.alice, &[Grant::new(&f.public, Op::Write)], None)
        .unwrap()
        .into_secret();
    assert!(matches!(
        f.service
            .import_revision(&f.context(&writer), &f.public, &records[1], BATCH, true),
        Err(ServiceError::Policy(_))
    ));
}

#[test]
fn import_never_trusts_source_authority() {
    let f = Fixture::new();
    let mut row = f.records()[0].clone();
    let bob = f.context(&f.bob_key);
    assert!(f
        .service
        .import_revision(&bob, &f.source, &row, BATCH, false)
        .is_err());
    row.collection_id = "forged-source".into();
    row.author_principal_id = Some("forged-author".into());
    f.service
        .import_revision(&bob, &f.public, &row, BATCH, false)
        .unwrap();
    let record = f
        .service
        .get(&RequestContext::anonymous(), &f.public, "record", None)
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
    let read_only = f
        .store
        .issue_credential(&f.bob, &[Grant::new(&f.destination, Op::Read)], None)
        .unwrap()
        .into_secret();
    assert!(f
        .service
        .import_revision(&f.context(&read_only), &f.destination, &row, BATCH, false)
        .is_err());
    row.version = 99;
    assert!(f
        .service
        .import_revision(&bob, &f.destination, &row, BATCH, false)
        .is_err());
}

fn frames(body: &str) -> Vec<Value> {
    body.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn paginated_streams_round_trip_and_reject_incomplete_imports() {
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
    let import = format!("{url}/v1/collections/{}/import", f.destination);
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
    let replayed = send(first.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(replayed, accepted);
    let second_result = send(second).send().await.unwrap().text().await.unwrap();
    assert_eq!(frames(&second_result)[0]["revision"], 2);
    let overwritten = client
        .post(format!("{import}?overwrite=true"))
        .bearer_auth(&f.bob_key)
        .header("content-type", "application/x-ndjson")
        .header("idempotency-key", "http-overwrite")
        .body(first.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(frames(&overwritten)[0]["record_id"], "record");
    assert_eq!(frames(&overwritten)[0]["revision"], 3);
    let mut fresh = first_rows[0].clone();
    fresh["record"]["record_id"] = json!("fresh");
    let truncated = client
        .post(&import)
        .bearer_auth(&f.bob_key)
        .header("content-type", "application/x-ndjson")
        .header("idempotency-key", "http-truncated")
        .body(format!("{fresh}\n"))
        .send()
        .await
        .unwrap();
    assert_eq!(truncated.status(), 400);
    let mut blank = fresh.clone();
    blank["record"]["record_id"] = json!("blank");
    blank["record"]["text"] = json!("  ");
    let refused = client
        .post(&import)
        .bearer_auth(&f.bob_key)
        .header("content-type", "application/x-ndjson")
        .header("idempotency-key", "http-blank")
        .body(format!(
            "{fresh}\n{blank}\n{}\n",
            json!({"type": "end", "count": 2, "generation": 0, "next_cursor": null})
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let unwritten = client
        .get(format!(
            "{url}/v1/collections/{}/records/fresh",
            f.destination
        ))
        .bearer_auth(&f.bob_key)
        .send()
        .await
        .unwrap();
    assert_eq!(unwritten.status(), 404);
    let invalid = send("{private malformed payload}\n".into())
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 422);
    assert!(!invalid
        .text()
        .await
        .unwrap()
        .contains("private malformed payload"));
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

/// nginx and Cloudflare stop forwarding a request body once the upstream starts its response.
#[tokio::test]
async fn import_reads_the_whole_body_before_responding() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = Fixture::new();
    let url = common::spawn(f.service.clone()).await;
    let mut row = serde_json::to_value(&f.records()[0]).unwrap();
    let rows = 2000;
    let mut body = String::new();
    for i in 0..rows {
        row["record_id"] = json!(format!("r{i}"));
        body += &json!({"type": "revision", "record": row}).to_string();
        body.push('\n');
    }
    body +=
        &json!({"type": "end", "count": rows, "generation": 0, "next_cursor": null}).to_string();
    body.push('\n');
    let address = url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "POST /v1/collections/{}/import HTTP/1.1\r\nhost: {address}\r\n\
                 authorization: Bearer {}\r\ncontent-type: application/x-ndjson\r\n\
                 idempotency-key: whole-body\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                f.destination,
                f.bob_key,
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    for chunk in body.as_bytes().chunks(4096) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut buffer = [0; 4096];
        match stream.try_read(&mut buffer) {
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
        stream.write_all(chunk).await.unwrap();
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        stream.read_to_end(&mut response),
    )
    .await
    .expect("the response must not start before the body is read")
    .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert_eq!(response.matches("\"type\":\"imported\"").count(), rows);
    assert!(response.contains(&format!("{{\"count\":{rows},\"type\":\"end\"}}")));
}
