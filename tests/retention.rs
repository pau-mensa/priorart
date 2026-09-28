use priorart::{
    auth::{Grant, Operation as Op, RequestContext},
    config::{ServerMode, Settings},
    service::{ReportOptions, Service, WriteOptions, DATABASE_FILE},
    store::{FeedbackKind, RetentionKind, Store, Visibility, LOCAL_COLLECTION_ID as LOCAL},
};
use serde_json::{json, Value};
use std::sync::Arc;

struct Fixture {
    dir: tempfile::TempDir,
    service: Arc<Service>,
    db: rusqlite::Connection,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(
            Service::new(
                Settings {
                    data_dir: dir.path().into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap(),
        );
        let db = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        db.pragma_update(None, "foreign_keys", true).unwrap();
        Self { dir, service, db }
    }
    fn old(&self, table: &str) {
        self.db.execute(&format!("UPDATE {table} SET created_at = '2000-01-01T00:00:00.000000Z' WHERE collection_id = 'local'"), []).unwrap();
    }
    fn count(&self, table: &str) -> i64 {
        self.db
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE collection_id = 'local'"),
                [],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn job(&self, kind: RetentionKind) -> priorart::store::RetentionJob {
        self.service
            .create_retention_job(&RequestContext::local(), LOCAL, "job", kind, 1_000_000_000)
            .unwrap();
        self.service
            .run_retention_batch(&RequestContext::local(), LOCAL, "job")
            .unwrap()
    }
}
const CALLER: RequestContext = RequestContext::local();

#[test]
fn old_revisions_remove_dependents_and_index_generations_but_keep_latest_and_other_scopes() {
    let f = Fixture::new();
    let s = &f.service;
    s.put(
        &CALLER,
        LOCAL,
        "old secret",
        None,
        Some("r"),
        WriteOptions {
            idempotency_key: Some("create"),
            ..Default::default()
        },
    )
    .unwrap();
    let receipt = s
        .search(&CALLER, LOCAL, "secret", None, 10)
        .unwrap()
        .search_id
        .unwrap();
    s.report(
        &CALLER,
        LOCAL,
        "r",
        "quoted secret",
        ReportOptions {
            revision: Some(1),
            search_id: Some(&receipt),
            idempotency_key: Some("report"),
        },
    )
    .unwrap();
    s.put(
        &CALLER,
        LOCAL,
        "latest survives",
        None,
        Some("r"),
        WriteOptions {
            expected_revision: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    f.db.execute("INSERT INTO import_provenance VALUES ('local','r',1,'local-principal','source','source-r',1,'restricted','claimed','old')", []).unwrap();
    let store = Store::open(f.dir.path().join(DATABASE_FILE)).unwrap();
    let other = store
        .create_collection(priorart::store::LOCAL_ACCOUNT_ID, Visibility::Restricted)
        .unwrap();
    store
        .put(
            &other,
            "other",
            None,
            Some("r"),
            priorart::store::LOCAL_PRINCIPAL_ID,
            None,
        )
        .unwrap();
    f.old("revisions");
    let generation = s.export_generation(&CALLER, LOCAL).unwrap();
    let job = f.job(RetentionKind::Revisions);
    assert!(job.complete);
    assert_eq!(job.processed, 1);
    assert!(s.get(&CALLER, LOCAL, "r", Some(1)).is_err());
    assert_eq!(s.get(&CALLER, LOCAL, "r", None).unwrap().revision, 2);
    assert_eq!(f.count("reports"), 0);
    assert_eq!(f.count("import_provenance"), 0);
    assert!(s
        .search_receipt(&CALLER, LOCAL, &receipt)
        .unwrap()
        .hits
        .is_empty());
    assert!(s.export_generation(&CALLER, LOCAL).unwrap() > generation);
    assert!(!priorart::index::collection_index_path(f.dir.path(), LOCAL).exists());
    assert_eq!(
        s.search(&CALLER, LOCAL, "survives", None, 10).unwrap().hits[0].revision,
        2
    );
    assert_eq!(
        store.get(&other, "r", None).unwrap().text.as_deref(),
        Some("other")
    );
    assert!(s
        .put(
            &CALLER,
            LOCAL,
            "old secret",
            None,
            Some("r"),
            WriteOptions {
                idempotency_key: Some("create"),
                ..Default::default()
            }
        )
        .is_err());
    assert_eq!(
        s.run_retention_batch(&CALLER, LOCAL, "job")
            .unwrap()
            .processed,
        1
    );
    assert!(f
        .db
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query([])
        .unwrap()
        .next()
        .unwrap()
        .is_none());
}

#[test]
fn receipt_jobs_resume_in_bounded_batches_and_detach_surviving_reports() {
    let f = Fixture::new();
    f.service
        .put(
            &CALLER,
            LOCAL,
            "record",
            None,
            Some("r"),
            Default::default(),
        )
        .unwrap();
    for i in 0..101 {
        f.db.execute("INSERT INTO searches VALUES ('local', ?1, 'local-principal', '2000-01-01T00:00:00.000000Z')", [format!("s{i:03}")]).unwrap();
        f.db.execute(
            "INSERT INTO search_hits VALUES ('local', ?1, 0, 'r', 1)",
            [format!("s{i:03}")],
        )
        .unwrap();
    }
    f.service
        .report(
            &CALLER,
            LOCAL,
            "r",
            "retained report",
            ReportOptions {
                revision: Some(1),
                search_id: Some("s000"),
                ..Default::default()
            },
        )
        .unwrap();
    let job = f.job(RetentionKind::Receipts);
    assert_eq!(job.processed, 100);
    assert!(!job.complete);
    assert_eq!(f.count("searches"), 1);
    assert_eq!(f.count("reports"), 1);
    assert!(f.service.reports(&CALLER, LOCAL, "r").unwrap()[0]
        .search_id
        .is_none());
    assert!(f
        .service
        .create_retention_job(&CALLER, LOCAL, "job", RetentionKind::Reports, 1_000_000_000)
        .is_err());
    let Fixture { dir, service, db } = f;
    drop(service);
    let s = Service::new(
        Settings {
            data_dir: dir.path().into(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let job = s.run_retention_batch(&CALLER, LOCAL, "job").unwrap();
    assert_eq!(job.processed, 101);
    assert!(job.complete);
    assert_eq!(
        db.query_row("SELECT count(*) FROM search_hits", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(s
        .create_retention_job(&CALLER, LOCAL, "future", RetentionKind::Receipts, i64::MAX)
        .is_err());
}

#[test]
fn mutation_retention_keeps_replay_barriers_without_removing_content() {
    let f = Fixture::new();
    let options = WriteOptions {
        idempotency_key: Some("key"),
        ..Default::default()
    };
    let record = f
        .service
        .put(&CALLER, LOCAL, "kept", None, None, options)
        .unwrap()
        .value
        .0;
    f.service
        .report(
            &CALLER,
            LOCAL,
            &record,
            "kept report",
            ReportOptions {
                revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    f.old("mutations");
    let job = f.job(RetentionKind::Mutations);
    assert_eq!(job.processed, 2);
    assert_eq!(f.count("mutations"), 1);
    assert_eq!(f.count("reports"), 1);
    assert!(f
        .service
        .put(&CALLER, LOCAL, "kept", None, None, options)
        .is_err());
    assert_eq!(f.count("records"), 1);
    let row: (String, String, Option<String>) =
        f.db.query_row(
            "SELECT payload_digest,result,target_record_id FROM mutations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("".into(), "null".into(), None));
}

#[tokio::test]
async fn feedback_deletion_enforces_authorship_moderation_and_transport_contract() {
    let f = Fixture::new();
    let store = Store::open(f.dir.path().join(DATABASE_FILE)).unwrap();
    let owner = store.create_principal().unwrap();
    let visitor = store.create_principal().unwrap();
    let account = store.create_account(&owner).unwrap();
    let public = store
        .create_collection(&account, Visibility::Public)
        .unwrap();
    let ops = [
        Op::Read,
        Op::Contribute,
        Op::Report,
        Op::ReportPublish,
        Op::FeedbackRead,
        Op::FeedbackDelete,
        Op::Moderate,
        Op::Admin,
        Op::Delegate,
    ];
    let owner_key = store
        .issue_local_credential(&owner, &ops.map(|op| Grant::new(&public, op)), None)
        .unwrap()
        .into_secret();
    let owner_ctx = store.authenticate(&owner_key).unwrap();
    let visitor_key = store
        .delegate_credential_to(
            &owner_ctx,
            &visitor,
            &[
                Op::Report,
                Op::ReportPublish,
                Op::FeedbackRead,
                Op::FeedbackDelete,
            ]
            .map(|op| Grant::new(&public, op)),
            None,
        )
        .unwrap()
        .into_secret();
    let visitor_ctx = store.authenticate(&visitor_key).unwrap();
    f.service
        .put(
            &owner_ctx,
            &public,
            "public",
            None,
            Some("r"),
            WriteOptions {
                publish: true,
                ..Default::default()
            },
        )
        .unwrap();
    let receipt = f
        .service
        .search(&visitor_ctx, &public, "public", None, 10)
        .unwrap()
        .search_id
        .unwrap();
    let report = f
        .service
        .report(
            &visitor_ctx,
            &public,
            "r",
            "private",
            ReportOptions {
                revision: Some(1),
                search_id: Some(&receipt),
                idempotency_key: Some("feedback"),
            },
        )
        .unwrap()
        .value;
    let publication = f
        .service
        .publish_report(
            &visitor_ctx,
            &public,
            &report,
            "selected",
            Some("publication"),
        )
        .unwrap()
        .value;
    for (id, kind) in [
        (&receipt, FeedbackKind::Receipt),
        (&report, FeedbackKind::Report),
    ] {
        assert!(f
            .service
            .delete_feedback(&owner_ctx, &public, id, kind)
            .is_err());
        assert!(f
            .service
            .delete_feedback(&RequestContext::anonymous(), &public, id, kind)
            .is_err());
    }
    assert!(f
        .service
        .create_retention_job(&visitor_ctx, &public, "denied", RetentionKind::Reports, 0)
        .is_err());
    f.service
        .delete_feedback(&visitor_ctx, &public, &receipt, FeedbackKind::Receipt)
        .unwrap();
    assert!(f.service.reports(&visitor_ctx, &public, "r").unwrap()[0]
        .search_id
        .is_none());
    f.service
        .delete_feedback(&visitor_ctx, &public, &report, FeedbackKind::Report)
        .unwrap();
    // Publication is an independent copy, still removable after its source is gone.
    assert_eq!(
        f.service
            .published_reports(&RequestContext::anonymous(), &public, "r")
            .unwrap()
            .len(),
        1
    );
    f.service
        .delete_feedback(&owner_ctx, &public, &publication, FeedbackKind::Publication)
        .unwrap();
    assert!(f
        .service
        .published_reports(&visitor_ctx, &public, "r")
        .unwrap()
        .is_empty());
    assert!(f
        .service
        .report(
            &visitor_ctx,
            &public,
            "r",
            "private",
            ReportOptions {
                revision: Some(1),
                search_id: Some(&receipt),
                idempotency_key: Some("feedback")
            }
        )
        .is_err());
    let report = f
        .service
        .report(
            &owner_ctx,
            &public,
            "r",
            "expires",
            ReportOptions {
                revision: Some(1),
                idempotency_key: Some("expires"),
                ..Default::default()
            },
        )
        .unwrap()
        .value;
    let publication = f
        .service
        .publish_report(
            &owner_ctx,
            &public,
            &report,
            "expires publicly",
            Some("expires-public"),
        )
        .unwrap()
        .value;
    f.db.execute(
        "UPDATE mutations SET created_at = '2000-01-01T00:00:00.000000Z' WHERE collection_id = ?1",
        [&public],
    )
    .unwrap();
    f.service
        .create_retention_job(
            &owner_ctx,
            &public,
            "journal",
            RetentionKind::Mutations,
            1_000_000_000,
        )
        .unwrap();
    f.service
        .run_retention_batch(&owner_ctx, &public, "journal")
        .unwrap();
    f.service
        .delete_feedback(&owner_ctx, &public, &publication, FeedbackKind::Publication)
        .unwrap();
    f.service
        .publish_report(&owner_ctx, &public, &report, "expires publicly", None)
        .unwrap();
    for table in ["reports", "published_reports"] {
        f.db.execute(&format!("UPDATE {table} SET created_at = '2000-01-01T00:00:00.000000Z' WHERE collection_id = ?1"), [&public]).unwrap();
    }
    f.service
        .create_retention_job(
            &owner_ctx,
            &public,
            "feedback",
            RetentionKind::Reports,
            1_000_000_000,
        )
        .unwrap();
    assert_eq!(
        f.service
            .run_retention_batch(&owner_ctx, &public, "feedback")
            .unwrap()
            .processed,
        2
    );
    assert!(f
        .service
        .reports(&owner_ctx, &public, "r")
        .unwrap()
        .is_empty());
    assert!(f
        .service
        .published_reports(&owner_ctx, &public, "r")
        .unwrap()
        .is_empty());
    let revoked = store.authenticate(&visitor_key).unwrap();
    store
        .revoke_local_credential(&revoked.credential().unwrap().id)
        .unwrap();
    assert!(f
        .service
        .delete_feedback(&revoked, &public, &report, FeedbackKind::Report)
        .is_err());

    let Fixture { dir, service, db } = f;
    drop(service);
    let service = Arc::new(
        Service::new(
            Settings {
                data_dir: dir.path().into(),
                mode: ServerMode::Authenticated,
                ..Default::default()
            },
            None,
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/collections/{public}",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            priorart::api::router(service)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{url}/retention-jobs"))
        .bearer_auth(&owner_key)
        .json(&json!({"id":"http-job","kind":"reports","before_unix":1_000_000_000}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let job: Value = client
        .post(format!("{url}/retention-jobs/http-job/run"))
        .bearer_auth(&owner_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(job["complete"], true);
    assert_eq!(
        client
            .get(format!("{url}/retention-jobs/http-job"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let report = store
        .add_report(&public, "r", Some(1), None, "delete via HTTP", &owner)
        .unwrap();
    assert_eq!(
        client
            .delete(format!("{url}/reports/{report}"))
            .bearer_auth(&owner_key)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        client
            .delete(format!("{url}/reports/{report}"))
            .bearer_auth(&owner_key)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .post(format!("{url}/retention-jobs"))
            .bearer_auth(&owner_key)
            .json(&json!({"id":"bad","kind":"unknown","before_unix":0}))
            .send()
            .await
            .unwrap()
            .status(),
        422
    );
    server.abort();
    drop(db);
}

#[test]
fn revision_batches_advance_past_retained_latest_rows() {
    let f = Fixture::new();
    for i in 0..101 {
        let id = format!("r{i:03}");
        f.db.execute("INSERT INTO records VALUES ('local',?1,'local-principal','2000-01-01T00:00:00.000000Z',NULL,NULL)", [&id]).unwrap();
        f.db.execute("INSERT INTO revisions VALUES ('local',?1,1,'keep',NULL,'hash','2000-01-01T00:00:00.000000Z')", [&id]).unwrap();
    }
    f.db.execute("INSERT INTO revisions VALUES ('local','r100',2,'latest',NULL,'hash','2000-01-01T00:00:00.000000Z')", []).unwrap();
    let job = f.job(RetentionKind::Revisions);
    assert!(!job.complete);
    assert_eq!(job.processed, 0);
    let job = f
        .service
        .run_retention_batch(&CALLER, LOCAL, "job")
        .unwrap();
    assert!(job.complete);
    assert_eq!(job.processed, 1);
    assert_eq!(f.count("revisions"), 101);
}
