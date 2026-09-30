use priorart::auth::RequestContext;
use priorart::service::WriteOptions;
mod common;

use priorart::config::Settings;
use priorart::index::Index;
use priorart::service::{Service, ServiceError};
use priorart::store::{
    Metadata, Store, StoreError, Visibility, LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID,
};
use serde_json::json;
use tempfile::TempDir;

const CALLER: RequestContext = RequestContext::local();
const LOCAL: &str = LOCAL_COLLECTION_ID;

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

fn seeded() -> (TempDir, Service) {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::open(settings(&directory)).unwrap();
    for (record, text) in DOCS {
        let tags = metadata(json!({"kind": "fix", "topic": record}));
        service
            .put(
                &CALLER,
                LOCAL,
                text,
                Some(&tags),
                Some(record),
                WriteOptions::default(),
            )
            .unwrap();
    }
    (directory, service)
}

fn invalid<T: std::fmt::Debug>(result: Result<T, ServiceError>) -> bool {
    matches!(result, Err(ServiceError::InvalidInput(_)))
}

#[test]
fn put_and_search() {
    let (_directory, service) = seeded();
    let hits = service
        .search(&CALLER, &[LOCAL], "worker never reached barrier", None, 10)
        .unwrap();
    let hit = &hits[0];
    assert_eq!((hit.id.as_str(), hit.revision), ("nccl", 1));
    assert_eq!(hit.collection_id, LOCAL_COLLECTION_ID);
    assert_eq!(
        hit.metadata,
        Some(metadata(json!({"kind": "fix", "topic": "nccl"})))
    );
    assert!(hit.excerpt.contains("barrier"));
}

#[test]
fn an_update_supersedes() {
    let (_directory, service) = seeded();
    let mutation = service
        .put(
            &CALLER,
            LOCAL,
            "completely different: rust borrow checker lifetime",
            None,
            Some("nccl"),
            WriteOptions {
                idempotency_key: None,
                expected_revision: Some(1),
            },
        )
        .unwrap();
    assert_eq!(mutation.value.1, 2);
    let outcome = service
        .search(&CALLER, &[LOCAL], "borrow checker lifetime", None, 10)
        .unwrap();
    assert_eq!((outcome[0].id.as_str(), outcome[0].revision), ("nccl", 2));
    assert!(service
        .get(&CALLER, LOCAL, "nccl", None)
        .unwrap()
        .text
        .unwrap()
        .starts_with("completely"));
    assert!(service
        .get(&CALLER, LOCAL, "nccl", Some(1))
        .unwrap()
        .text
        .unwrap()
        .starts_with("NCCL"));
}

#[test]
fn delete_hides() {
    let (_directory, service) = seeded();
    service
        .delete(
            &CALLER,
            LOCAL,
            "nccl",
            priorart::service::DeleteOptions {
                expected_revision: Some(1),
                idempotency_key: None,
            },
        )
        .unwrap();
    let outcome = service
        .search(&CALLER, &[LOCAL], "worker never reached barrier", None, 10)
        .unwrap();
    assert!(outcome.iter().all(|hit| hit.id != "nccl"));
    assert!(matches!(
        service.get(&CALLER, LOCAL, "nccl", None),
        Err(ServiceError::Store(StoreError::RecordDeleted { .. }))
    ));
    service
        .delete(
            &CALLER,
            LOCAL,
            "nccl",
            priorart::service::DeleteOptions {
                expected_revision: Some(1),
                idempotency_key: None,
            },
        )
        .unwrap();
    assert!(matches!(
        service.delete(
            &CALLER,
            LOCAL,
            "missing",
            priorart::service::DeleteOptions {
                expected_revision: Some(1),
                idempotency_key: None
            }
        ),
        Err(ServiceError::Store(StoreError::RecordNotFound { .. }))
    ));
}

#[test]
fn filters_restrict() {
    let (_directory, service) = seeded();
    let pytest = metadata(json!({"topic": "pytest"}));
    let outcome = service
        .search(&CALLER, &[LOCAL], "error fixed", Some(&pytest), 10)
        .unwrap();
    assert_eq!(
        outcome
            .iter()
            .map(|hit| hit.id.as_str())
            .collect::<Vec<_>>(),
        ["pytest"]
    );
    let nothing = metadata(json!({"topic": "nothing"}));
    assert!(service
        .search(&CALLER, &[LOCAL], "error", Some(&nothing), 10)
        .unwrap()
        .is_empty());
}

#[test]
fn truncation_counts_analyzer_terms() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::open(Settings {
        max_tokens: 3,
        ..settings(&directory)
    })
    .unwrap();
    let options = WriteOptions {
        idempotency_key: Some("retry"),
        ..WriteOptions::default()
    };
    let text = "  Straße café, naïve résumé";
    let first = service
        .put(&CALLER, LOCAL, text, None, Some("long"), options)
        .unwrap();
    assert!(first.value.2);
    let retried = service
        .put(&CALLER, LOCAL, text, None, Some("long"), options)
        .unwrap();
    assert_eq!(retried, first);
    let stored = service.get(&CALLER, LOCAL, "long", None).unwrap();
    assert_eq!(stored.text.as_deref(), Some("  Straße café, naïve"));
    assert!(service
        .search(&CALLER, &[LOCAL], "resume", None, 10)
        .unwrap()
        .is_empty());
    assert_eq!(
        service
            .search(&CALLER, &[LOCAL], "naive", None, 10)
            .unwrap()[0]
            .id,
        "long"
    );
    let exact = service
        .put(
            &CALLER,
            LOCAL,
            "one two three",
            None,
            Some("long"),
            WriteOptions {
                expected_revision: Some(1),
                ..WriteOptions::default()
            },
        )
        .unwrap();
    assert!(!exact.value.2);
}

#[test]
fn a_loaded_index_follows_later_writes() {
    let (_directory, service) = seeded();
    let ids = |query: &str| -> Vec<String> {
        service
            .search(&CALLER, &[LOCAL], query, None, 10)
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect()
    };
    assert_eq!(ids("barrier"), ["nccl"]);
    service
        .put(
            &CALLER,
            LOCAL,
            "a new barrier story",
            None,
            Some("new"),
            WriteOptions::default(),
        )
        .unwrap();
    service
        .put(
            &CALLER,
            LOCAL,
            "nothing relevant anymore",
            None,
            Some("nccl"),
            WriteOptions {
                expected_revision: Some(1),
                ..WriteOptions::default()
            },
        )
        .unwrap();
    assert_eq!(ids("barrier"), ["new"]);
    service
        .delete(
            &CALLER,
            LOCAL,
            "new",
            priorart::service::DeleteOptions {
                expected_revision: Some(1),
                idempotency_key: None,
            },
        )
        .unwrap();
    assert!(ids("barrier").is_empty());
    assert_eq!(ids("relevant"), ["nccl"]);
}

#[test]
fn an_empty_corpus_returns_no_hits() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::open(settings(&directory)).unwrap();
    assert!(service
        .search(&CALLER, &[LOCAL], "anything", None, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn validation() {
    let (_directory, service) = seeded();
    assert!(invalid(service.put(
        &CALLER,
        LOCAL,
        "   ",
        None,
        None,
        WriteOptions::default()
    )));
    assert!(invalid(service.put(
        &CALLER,
        LOCAL,
        "ok",
        None,
        Some("bad id with spaces"),
        WriteOptions::default()
    )));
    assert!(invalid(service.put(
        &CALLER,
        LOCAL,
        "ok",
        None,
        Some(&"x".repeat(129)),
        WriteOptions::default()
    )));
    assert!(invalid(service.put(
        &CALLER,
        LOCAL,
        "ok",
        None,
        Some("."),
        WriteOptions::default()
    )));
    assert!(invalid(service.put(
        &CALLER,
        LOCAL,
        "ok",
        None,
        Some(".."),
        WriteOptions::default()
    )));
    service
        .put(
            &CALLER,
            LOCAL,
            "ok",
            None,
            Some("..."),
            WriteOptions::default(),
        )
        .unwrap();
    assert!(invalid(service.search(&CALLER, &[LOCAL], "   ", None, 10)));
    assert!(invalid(service.search(&CALLER, &[LOCAL], "ok", None, 0)));
    assert!(invalid(service.search(&CALLER, &[LOCAL], "ok", None, 101)));
    for bad in [
        json!({"nested": {"a": 1}}),
        json!({"list": [1]}),
        json!({"null": null}),
    ] {
        assert!(invalid(service.search(
            &CALLER,
            &[LOCAL],
            "ok",
            Some(&metadata(bad)),
            10
        )));
    }
}

#[test]
fn health() {
    let (_directory, service) = seeded();
    let health = service.health(&CALLER, LOCAL).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.document_count, 3);
}

#[test]
fn reopening_keeps_data() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::open(settings(&directory)).unwrap();
    service
        .put(
            &CALLER,
            LOCAL,
            DOCS[2].1,
            None,
            Some("nccl"),
            WriteOptions::default(),
        )
        .unwrap();
    drop(service);
    let reopened = Service::open(settings(&directory)).unwrap();
    assert_eq!(
        reopened
            .search(&CALLER, &[LOCAL], "barrier", None, 10)
            .unwrap()[0]
            .id,
        "nccl"
    );
}

#[test]
fn the_local_service_never_reads_other_collections() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("priorart.sqlite")).unwrap();
    let other = store
        .create_collection(LOCAL_PRINCIPAL_ID, Visibility::Restricted)
        .unwrap();
    store
        .put(
            &other,
            "private sentinel",
            None,
            Some("hidden"),
            LOCAL_PRINCIPAL_ID,
            None,
        )
        .unwrap();
    drop(store);

    let service = Service::open(settings(&directory)).unwrap();
    service
        .put(
            &CALLER,
            LOCAL,
            "local searchable sentinel",
            None,
            Some("local-record"),
            WriteOptions::default(),
        )
        .unwrap();
    assert_eq!(service.health(&CALLER, LOCAL).unwrap().document_count, 1);
    let hits = service
        .search(&CALLER, &[LOCAL], "sentinel", None, 10)
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>(),
        ["local-record"]
    );
    assert_eq!(
        service
            .get(&CALLER, LOCAL, "local-record", None)
            .unwrap()
            .collection_id,
        LOCAL_COLLECTION_ID
    );
    assert!(matches!(
        service.get(&CALLER, LOCAL, "hidden", None),
        Err(ServiceError::Store(StoreError::RecordNotFound { .. }))
    ));
    drop(service);

    let store = Store::open(directory.path().join("priorart.sqlite")).unwrap();
    let index = Index::load(&store, LOCAL_COLLECTION_ID).unwrap();
    assert_eq!(index.document_count(), 1);
    assert_eq!(index.document(0), ("local-record", 1));
}
