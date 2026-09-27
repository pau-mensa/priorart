mod common;

use std::sync::Arc;

use common::FakeEncoder;
use priorart::config::Settings;
use priorart::encoder::Encoder;
use priorart::index::Index;
use priorart::service::{Service, ServiceError};
use priorart::store::{
    Metadata, Store, StoreError, Visibility, LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID,
};
use serde_json::json;
use tempfile::TempDir;

const DOCS: [(&str, &str); 3] = [
    (
        "cuda",
        "CUDA illegal address after switching attention to bf16. Fixed by padding heads.",
    ),
    (
        "pytest",
        "pytest fixture scope error when using async fixtures with session scope.",
    ),
    (
        "nccl",
        "NCCL timeout: one worker never reached the barrier because of a stray exit().",
    ),
];

fn metadata(value: serde_json::Value) -> Metadata {
    value.as_object().unwrap().clone()
}

fn settings(directory: &TempDir) -> Settings {
    Settings {
        data_dir: directory.path().to_path_buf(),
        ..Settings::default()
    }
}

fn fake() -> Option<Arc<dyn Encoder>> {
    Some(Arc::new(FakeEncoder::new()))
}

fn seeded() -> (TempDir, Service) {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(settings(&directory), fake()).unwrap();
    for (record, text) in DOCS {
        let tags = metadata(json!({"kind": "fix", "topic": record}));
        service.put(text, Some(&tags), Some(record)).unwrap();
    }
    (directory, service)
}

fn invalid<T: std::fmt::Debug>(result: Result<T, ServiceError>) -> bool {
    matches!(result, Err(ServiceError::InvalidInput(_)))
}

#[test]
fn put_and_search() {
    let (_directory, service) = seeded();
    let outcome = service
        .search("worker never reached barrier", None, 10)
        .unwrap();
    assert!(!outcome.search_id.is_empty());
    let hit = &outcome.hits[0];
    assert_eq!((hit.id.as_str(), hit.revision), ("nccl", 1));
    assert_eq!(hit.collection_id, LOCAL_COLLECTION_ID);
    assert_eq!(
        hit.metadata,
        Some(metadata(json!({"kind": "fix", "topic": "nccl"})))
    );
    assert_eq!(
        hit.score_semantics,
        "int8-reconstructed-approximate-full-maxsim"
    );
    assert_eq!(outcome.gatherer, "exhaustive");
    assert!(hit.excerpt.contains("barrier"));
    for key in ["gather_seconds", "rerank_seconds", "total_seconds"] {
        assert!(outcome.timings.contains_key(key));
    }
}

#[test]
fn an_update_supersedes() {
    let (_directory, service) = seeded();
    let (_, revision) = service
        .put(
            "completely different: rust borrow checker lifetime",
            None,
            Some("nccl"),
        )
        .unwrap();
    assert_eq!(revision, 2);
    let outcome = service.search("borrow checker lifetime", None, 10).unwrap();
    assert_eq!(
        (outcome.hits[0].id.as_str(), outcome.hits[0].revision),
        ("nccl", 2)
    );
    assert!(service
        .get("nccl", None)
        .unwrap()
        .text
        .unwrap()
        .starts_with("completely"));
    assert!(service
        .get("nccl", Some(1))
        .unwrap()
        .text
        .unwrap()
        .starts_with("NCCL"));
}

#[test]
fn delete_hides() {
    let (_directory, service) = seeded();
    service.delete("nccl").unwrap();
    let outcome = service
        .search("worker never reached barrier", None, 10)
        .unwrap();
    assert!(outcome.hits.iter().all(|hit| hit.id != "nccl"));
    assert!(matches!(
        service.get("nccl", None),
        Err(ServiceError::Store(StoreError::RecordDeleted { .. }))
    ));
    service.delete("nccl").unwrap();
    assert!(matches!(
        service.delete("missing"),
        Err(ServiceError::Store(StoreError::RecordNotFound { .. }))
    ));
}

#[test]
fn filters_restrict() {
    let (_directory, service) = seeded();
    let pytest = metadata(json!({"topic": "pytest"}));
    let outcome = service.search("error fixed", Some(&pytest), 10).unwrap();
    assert_eq!(
        outcome
            .hits
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>(),
        ["pytest"]
    );
    let nothing = metadata(json!({"topic": "nothing"}));
    assert!(service
        .search("error", Some(&nothing), 10)
        .unwrap()
        .hits
        .is_empty());
}

#[test]
fn the_gather_limit_switches_to_bm25() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(
        Settings {
            gather_limit: 2,
            ..settings(&directory)
        },
        fake(),
    )
    .unwrap();
    for (record, text) in DOCS {
        service.put(text, None, Some(record)).unwrap();
    }
    let outcome = service
        .search("worker never reached barrier", None, 1)
        .unwrap();
    assert_eq!(outcome.gatherer, "bm25");
    assert_eq!(
        outcome
            .hits
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>(),
        ["nccl"]
    );
    assert!(!service
        .search("worker never reached barrier", None, 3)
        .unwrap()
        .hits
        .is_empty());
}

#[test]
fn lexical_only_mode() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(settings(&directory), None).unwrap();
    service.put(DOCS[0].1, None, Some("cuda")).unwrap();
    service.put(DOCS[2].1, None, Some("nccl")).unwrap();
    let outcome = service.search("barrier", None, 10).unwrap();
    assert_eq!(outcome.hits[0].id, "nccl");
    assert_eq!(outcome.hits[0].score_semantics, "bm25-lucene");
    assert_eq!(outcome.gatherer, "bm25");
    assert_eq!(service.health().encoder, None);
}

#[test]
fn an_empty_corpus_still_logs_the_search() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(settings(&directory), fake()).unwrap();
    let outcome = service.search("anything", None, 10).unwrap();
    assert!(outcome.hits.is_empty());
    assert!(outcome.timings.is_empty());
    assert_eq!(outcome.gatherer, "none");
    assert!(!outcome.search_id.is_empty());
}

#[test]
fn reports() {
    let (_directory, service) = seeded();
    let outcome = service.search("cuda illegal address", None, 10).unwrap();
    let report = service
        .report(
            "cuda",
            "applied the padding fix, tests pass",
            Some(1),
            Some(&outcome.search_id),
        )
        .unwrap();
    let reports = service.reports("cuda").unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].id, report);
    assert_eq!(
        reports[0].search_id.as_deref(),
        Some(outcome.search_id.as_str())
    );
    assert!(matches!(
        service.report("cuda", "x", None, Some("bogus")),
        Err(ServiceError::Store(StoreError::SearchNotFound { .. }))
    ));
    assert!(matches!(
        service.report("missing", "x", None, None),
        Err(ServiceError::Store(StoreError::RecordNotFound { .. }))
    ));
    assert!(invalid(service.report("cuda", "   ", None, None)));
}

#[test]
fn validation() {
    let (_directory, service) = seeded();
    assert!(invalid(service.put("   ", None, None)));
    assert!(invalid(service.put(&"x".repeat(300_000), None, None)));
    assert!(invalid(service.put("ok", None, Some("bad id with spaces"))));
    assert!(invalid(service.put("ok", None, Some(&"x".repeat(129)))));
    assert!(invalid(service.put("ok", None, Some("."))));
    assert!(invalid(service.put("ok", None, Some(".."))));
    service.put("ok", None, Some("...")).unwrap();
    assert!(invalid(service.search("   ", None, 10)));
    assert!(invalid(service.search("ok", None, 0)));
    assert!(invalid(service.search("ok", None, 101)));
    for bad in [
        json!({"nested": {"a": 1}}),
        json!({"list": [1]}),
        json!({"null": null}),
    ] {
        assert!(invalid(service.search("ok", Some(&metadata(bad)), 10)));
    }
}

#[test]
fn health() {
    let (_directory, service) = seeded();
    let health = service.health();
    assert_eq!(health.status, "ok");
    assert_eq!(health.document_count, 3);
    assert_eq!(health.encoder.as_deref(), Some("fake"));
    assert_eq!(health.gather_limit, 500);
}

#[test]
fn reopening_keeps_data() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(settings(&directory), fake()).unwrap();
    service.put(DOCS[2].1, None, Some("nccl")).unwrap();
    drop(service);
    let reopened = Service::new(settings(&directory), fake()).unwrap();
    assert_eq!(
        reopened.search("barrier", None, 10).unwrap().hits[0].id,
        "nccl"
    );
}

#[test]
fn the_local_service_never_reads_other_collections() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("priorart.sqlite")).unwrap();
    let account = store.create_account(LOCAL_PRINCIPAL_ID).unwrap();
    let other = store
        .create_collection(&account, Visibility::Restricted)
        .unwrap();
    store
        .put(
            &other,
            "private sentinel",
            None,
            Some("hidden"),
            LOCAL_PRINCIPAL_ID,
        )
        .unwrap();
    drop(store);

    let service = Service::new(settings(&directory), None).unwrap();
    service
        .put("local searchable sentinel", None, Some("local-record"))
        .unwrap();
    assert_eq!(service.health().document_count, 1);
    let hits = service.search("sentinel", None, 10).unwrap().hits;
    assert_eq!(
        hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
        ["local-record"]
    );
    assert_eq!(
        service.get("local-record", None).unwrap().collection_id,
        LOCAL_COLLECTION_ID
    );
    assert!(matches!(
        service.get("hidden", None),
        Err(ServiceError::Store(StoreError::RecordNotFound { .. }))
    ));
    drop(service);

    let store = Store::open(directory.path().join("priorart.sqlite")).unwrap();
    let mut index = Index::open(directory.path(), &store, None, LOCAL_COLLECTION_ID).unwrap();
    index.rebuild(&store).unwrap();
    assert_eq!(index.record_ids(), ["local-record"]);
}

#[test]
fn an_encoder_failure_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let encoder = Arc::new(FakeEncoder::new());
    let service = Service::new(settings(&directory), Some(encoder.clone())).unwrap();
    service.put(DOCS[0].1, None, Some("cuda")).unwrap();
    encoder.set_failing(true);
    assert!(matches!(
        service.put("replacement text", None, Some("cuda")),
        Err(ServiceError::Index(_))
    ));
    assert!(service.put(DOCS[1].1, None, Some("pytest")).is_err());
    encoder.set_failing(false);
    assert_eq!(service.get("cuda", None).unwrap().revision, 1);
    assert!(matches!(
        service.get("pytest", None),
        Err(ServiceError::Store(StoreError::RecordNotFound { .. }))
    ));
    assert_eq!(service.health().document_count, 1);
}

#[cfg(unix)]
#[test]
fn a_failed_index_step_is_rebuilt_before_the_next_search() {
    use std::os::unix::fs::PermissionsExt;

    let (directory, service) = seeded();
    let vectors = priorart::index::collection_index_path(directory.path(), LOCAL_COLLECTION_ID)
        .join(priorart::index::VECTORS_DIRECTORY);
    let set_mode =
        |mode| std::fs::set_permissions(&vectors, std::fs::Permissions::from_mode(mode)).unwrap();
    set_mode(0o555);
    let deleted = service.delete("nccl");
    let search = service.search("worker never reached barrier", None, 10);
    set_mode(0o755);
    assert!(matches!(deleted, Err(ServiceError::Index(_))));
    assert!(matches!(search, Err(ServiceError::Index(_))));
    let outcome = service
        .search("worker never reached barrier", None, 10)
        .unwrap();
    assert!(!outcome.hits.is_empty());
    assert!(outcome.hits.iter().all(|hit| hit.id != "nccl"));
    assert_eq!(service.health().document_count, 2);
}
