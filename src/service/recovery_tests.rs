use super::*;
use crate::store::LOCAL_COLLECTION_ID as LOCAL;

const CALLER: RequestContext = RequestContext::local();
const PHASES: &[&str] = &[
    "before_record_commit",
    "after_record_commit",
    "before_index_activation",
    "after_index_activation",
    "after_mutation_complete",
];
fn settings(directory: &std::path::Path) -> Settings {
    Settings {
        data_dir: directory.into(),
        ..Settings::default()
    }
}
fn update(service: &Service) -> Result<Mutation<(String, i64)>> {
    service.put(
        &CALLER,
        LOCAL,
        "new searchable revision",
        None,
        Some("record"),
        WriteOptions {
            expected_revision: Some(1),
            idempotency_key: Some("retry-update"),
            publish: false,
        },
    )
}

#[test]
fn crash_helper() {
    let Some(directory) = std::env::var_os("PRIORART_CRASH_TEST_DIR") else {
        return;
    };
    let phase = std::env::var("PRIORART_CRASH_TEST_PHASE").unwrap();
    let phase = *PHASES.iter().find(|p| **p == phase).unwrap();
    let service = Service::new(settings(std::path::Path::new(&directory)), None).unwrap();
    service.health(&CALLER, LOCAL).unwrap();
    crate::fault::set(Some(phase));
    assert!(update(&service).is_err());
    // Exit without dropping Service, its index, or the directory lock.
    std::process::exit(17);
}

#[test]
fn every_commit_activation_boundary_recovers_without_duplicate_revision() {
    for phase in PHASES {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(settings(directory.path()), None).unwrap();
        service
            .put(
                &CALLER,
                LOCAL,
                "original searchable revision",
                None,
                Some("record"),
                WriteOptions::default(),
            )
            .unwrap();
        drop(service);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "service::recovery_tests::crash_helper",
                "--nocapture",
            ])
            .env("PRIORART_CRASH_TEST_DIR", directory.path())
            .env("PRIORART_CRASH_TEST_PHASE", phase)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(17), "{phase}");
        let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
        assert_eq!(
            store.get(LOCAL, "record", None).unwrap().revision,
            if *phase == "before_record_commit" {
                1
            } else {
                2
            },
            "{phase}"
        );
        let service = Service::new(settings(directory.path()), None).unwrap();
        let result = update(&service).unwrap();
        assert_eq!(result.value.1, 2);
        assert_eq!(update(&service).unwrap(), result);
        assert!(!store.has_pending_mutations(LOCAL).unwrap());
        assert_eq!(store.get(LOCAL, "record", None).unwrap().revision, 2);
        let hits = service
            .search(&CALLER, LOCAL, "new searchable", None, 10)
            .unwrap()
            .hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].revision, 2);
        assert!(hits[0].excerpt.contains("new searchable"));
        assert!(!hits[0].excerpt.contains("original"));
    }
}

#[test]
fn failed_delete_never_serves_the_previous_generation() {
    for phase in &PHASES[1..] {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(settings(directory.path()), None).unwrap();
        service
            .put(
                &CALLER,
                LOCAL,
                "secret sentinel",
                None,
                Some("record"),
                WriteOptions::default(),
            )
            .unwrap();
        crate::fault::set(Some(phase));
        let options = DeleteOptions {
            expected_revision: Some(1),
            idempotency_key: Some("delete-once"),
        };
        assert!(service.delete(&CALLER, LOCAL, "record", options).is_err());
        assert!(matches!(
            service.get(&CALLER, LOCAL, "record", None),
            Err(ServiceError::Store(StoreError::RecordDeleted { .. }))
        ));
        let search = service.search(&CALLER, LOCAL, "secret sentinel", None, 10);
        if let Ok(outcome) = search {
            assert!(outcome.hits.is_empty());
        }
        crate::fault::set(None);
        drop(service);
        let service = Service::new(settings(directory.path()), None).unwrap();
        let result = service.delete(&CALLER, LOCAL, "record", options).unwrap();
        assert_eq!(
            service.delete(&CALLER, LOCAL, "record", options).unwrap(),
            result
        );
        assert!(service
            .search(&CALLER, LOCAL, "secret sentinel", None, 10)
            .unwrap()
            .hits
            .is_empty());
    }
}

#[test]
fn purge_interruptions_resume_on_restart() {
    for collection_delete in [false, true] {
        let phases: &[&str] = if collection_delete {
            &[
                "before_collection_purge_commit",
                "after_collection_purge_commit",
                "after_purge_files",
            ]
        } else {
            &[
                "before_record_commit",
                "after_record_commit",
                "after_purge_files",
                "before_index_activation",
                "after_index_activation",
            ]
        };
        for phase in phases {
            let dir = tempfile::tempdir().unwrap();
            let service = Service::new(settings(dir.path()), None).unwrap();
            service
                .put(
                    &CALLER,
                    LOCAL,
                    "secret",
                    None,
                    Some("record"),
                    Default::default(),
                )
                .unwrap();
            crate::fault::set(Some(phase));
            let result = if collection_delete {
                service.delete_collection(&CALLER, LOCAL)
            } else {
                service
                    .delete(
                        &CALLER,
                        LOCAL,
                        "record",
                        DeleteOptions {
                            expected_revision: Some(1),
                            idempotency_key: Some("purge"),
                        },
                    )
                    .map(|_| ())
            };
            assert!(result.is_err(), "{phase}");
            crate::fault::set(None);
            drop(service);
            let service = Service::new(settings(dir.path()), None).unwrap();
            let before = phase.starts_with("before_") && *phase != "before_index_activation";
            if before {
                assert!(service.get(&CALLER, LOCAL, "record", None).is_ok());
            } else if collection_delete {
                assert!(service.get(&CALLER, LOCAL, "record", None).is_err());
                assert!(!crate::index::collection_index_path(dir.path(), LOCAL).exists());
            } else {
                assert!(service
                    .search(&CALLER, LOCAL, "secret", None, 10)
                    .unwrap()
                    .hits
                    .is_empty());
                service
                    .delete(
                        &CALLER,
                        LOCAL,
                        "record",
                        DeleteOptions {
                            expected_revision: Some(1),
                            idempotency_key: Some("purge"),
                        },
                    )
                    .unwrap();
            }
            assert!(service
                .connect()
                .unwrap()
                .pending_purges("")
                .unwrap()
                .iter()
                .all(|c| collection_delete && c == LOCAL));
        }
    }
}

#[test]
fn interrupted_import_recovers_content_and_provenance_together() {
    for phase in [
        "before_record_commit",
        "after_record_commit",
        "before_index_activation",
        "after_index_activation",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(settings(dir.path()), None).unwrap();
        service.health(&CALLER, LOCAL).unwrap();
        let row = crate::store::TransferRecord {
            version: 1,
            collection_id: "uploaded-source".into(),
            visibility: Visibility::Restricted,
            record_id: "original".into(),
            revision: 9,
            author_principal_id: Some("claimed-author".into()),
            created_at: "2026-01-01T00:00:00Z".into(),
            text: "imported text".into(),
            metadata: None,
        };
        let options = ImportOptions {
            batch_key: "resume-import",
            visibility: Visibility::Restricted,
            publish: false,
        };
        crate::fault::set(Some(phase));
        assert!(
            service
                .import_revision(&CALLER, LOCAL, &row, options)
                .is_err(),
            "{phase}"
        );
        crate::fault::set(None);
        drop(service);
        let service = Service::new(settings(dir.path()), None).unwrap();
        let result = service
            .import_revision(&CALLER, LOCAL, &row, options)
            .unwrap();
        assert_eq!(
            result,
            service
                .import_revision(&CALLER, LOCAL, &row, options)
                .unwrap()
        );
        assert_eq!(result.value.1, 1);
        assert_eq!(
            service
                .search(&CALLER, LOCAL, "imported", None, 10)
                .unwrap()
                .hits
                .len(),
            1
        );
        let db = rusqlite::Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM import_provenance", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT source_revision FROM import_provenance", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            9
        );
    }
}
