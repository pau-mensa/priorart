use priorart::auth::{AuthError, Grant, Operation as Op, RequestContext};
use priorart::config::Settings;
use priorart::policy::PolicyError;
use priorart::service::WriteOptions;
use priorart::service::{Service, ServiceError, DATABASE_FILE};
use priorart::store::{Metadata, Store, Visibility, LOCAL_COLLECTION_ID};
use serde_json::json;
use tempfile::TempDir;

const ALL: &[Op] = &[
    Op::Read,
    Op::Contribute,
    Op::Update,
    Op::Delete,
    Op::Moderate,
    Op::Admin,
];

struct Fixture {
    _dir: TempDir,
    store: Store,
    service: Service,
    alice: String,
    bob: String,
    a: String,
    b: String,
    public: String,
    alice_key: String,
    bob_key: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
        let alice = store.create_principal().unwrap();
        let bob = store.create_principal().unwrap();
        let a = store
            .create_collection(&alice, Visibility::Restricted)
            .unwrap();
        let b = store
            .create_collection(&bob, Visibility::Restricted)
            .unwrap();
        let public = store.create_collection(&alice, Visibility::Public).unwrap();
        let alice_grants: Vec<_> = [&a, &public]
            .iter()
            .flat_map(|id| ALL.iter().map(|op| Grant::new(*id, *op)))
            .collect();
        let bob_grants: Vec<_> = ALL.iter().map(|op| Grant::new(&b, *op)).collect();
        let alice_key = store
            .issue_local_credential(&alice, &alice_grants, None)
            .unwrap()
            .into_secret();
        let bob_key = store
            .issue_local_credential(&bob, &bob_grants, None)
            .unwrap()
            .into_secret();
        for (collection, principal, text) in [
            (&a, &alice, "alpha private sentinel"),
            (&b, &bob, "beta private sentinel"),
            (&public, &alice, "public sentinel"),
        ] {
            store
                .put(
                    collection,
                    text,
                    Some(json!({"tag": "shared"}).as_object().unwrap()),
                    Some("same"),
                    principal,
                    None,
                )
                .unwrap();
            store
                .put(
                    collection,
                    "previously deleted sentinel",
                    None,
                    Some("gone"),
                    principal,
                    None,
                )
                .unwrap();
            store.delete(collection, "gone", Some(1)).unwrap();
        }
        let service = Service::open(Settings {
            data_dir: dir.path().to_owned(),
            max_loaded_indexes: 1,
            ..Settings::default()
        })
        .unwrap();
        Self {
            _dir: dir,
            store,
            service,
            alice,
            bob,
            a,
            b,
            public,
            alice_key,
            bob_key,
        }
    }
    fn alice(&self) -> RequestContext {
        self.service.authenticate(&self.alice_key).unwrap()
    }
    fn bob(&self) -> RequestContext {
        self.service.authenticate(&self.bob_key).unwrap()
    }
    fn limited(&self, principal: &str, collection: &str, ops: &[Op]) -> String {
        let grants: Vec<_> = ops.iter().map(|op| Grant::new(collection, *op)).collect();
        self.store
            .issue_local_credential(principal, &grants, None)
            .unwrap()
            .into_secret()
    }
    fn context(&self, key: &str) -> RequestContext {
        self.service.authenticate(key).unwrap()
    }
}

fn unavailable<T: std::fmt::Debug>(result: Result<T, ServiceError>) -> String {
    let error = result.unwrap_err();
    assert!(matches!(
        error,
        ServiceError::Policy(PolicyError::Unavailable)
    ));
    error.to_string()
}
fn unauthenticated<T: std::fmt::Debug>(result: Result<T, ServiceError>) {
    assert!(matches!(
        result,
        Err(ServiceError::Policy(PolicyError::Authentication(
            AuthError::Unauthenticated
        )))
    ));
}

#[test]
fn unrelated_principals_cannot_read_mutate_or_inspect_forbidden_scope() {
    let f = Fixture::new();
    let tags: Metadata = json!({"tag": "shared"}).as_object().unwrap().clone();
    let context = f.bob();
    let expected = "requested resource is unavailable";
    for collection in [&f.a, "unknown"] {
        for record in ["same", "gone", "missing"] {
            for revision in [None, Some(1), Some(999)] {
                assert_eq!(
                    unavailable(f.service.get(&context, collection, record, revision)),
                    expected
                );
            }
            assert_eq!(
                unavailable(f.service.put(
                    &context,
                    collection,
                    "replacement",
                    None,
                    Some(record),
                    WriteOptions::default()
                )),
                expected
            );
            assert_eq!(
                unavailable(f.service.delete(
                    &context,
                    collection,
                    record,
                    priorart::service::DeleteOptions {
                        expected_revision: Some(1),
                        idempotency_key: None
                    }
                )),
                expected
            );
        }
        assert_eq!(
            unavailable(
                f.service
                    .search(&context, &[collection], "sentinel", Some(&tags), 10)
            ),
            expected
        );
        assert_eq!(
            unavailable(f.service.health(&context, collection)),
            expected
        );
    }
    assert_eq!(f.store.get(&f.a, "same", None).unwrap().revision, 1);
    let outcome = f
        .service
        .search(&context, &[&f.b], "sentinel", Some(&tags), 10)
        .unwrap();
    assert_eq!(outcome.len(), 1);
    assert_eq!(outcome[0].collection_id, f.b);
    assert!(outcome[0].excerpt.contains("beta"));
}

#[test]
fn read_scope_includes_old_revisions_but_not_mutations_or_diagnostics() {
    let f = Fixture::new();
    let key = f.limited(&f.alice, &f.a, &[Op::Read]);
    f.service
        .put(
            &f.alice(),
            &f.a,
            "second version",
            None,
            Some("same"),
            WriteOptions {
                idempotency_key: None,
                publish: false,
                expected_revision: Some(1),
            },
        )
        .unwrap();
    let reader = f.context(&key);
    assert_eq!(
        f.service
            .get(&reader, &f.a, "same", Some(1))
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        f.service.get(&reader, &f.a, "same", None).unwrap().revision,
        2
    );
    assert!(f
        .service
        .search(&reader, &[&f.a], "second", None, 10)
        .unwrap()[0]
        .excerpt
        .contains("second"));
    unavailable(
        f.service
            .put(&reader, &f.a, "new", None, None, WriteOptions::default()),
    );
    unavailable(f.service.delete(
        &reader,
        &f.a,
        "same",
        priorart::service::DeleteOptions {
            expected_revision: Some(1),
            idempotency_key: None,
        },
    ));
    unavailable(f.service.health(&reader, &f.a));
    unavailable(f.service.get(&reader, &f.b, "same", None));
}

#[test]
fn anonymous_public_reads_succeed_and_mutations_require_credentials() {
    let f = Fixture::new();
    let anonymous = RequestContext::anonymous();
    assert_eq!(
        f.service
            .get(&anonymous, &f.public, "same", Some(1))
            .unwrap()
            .record_id,
        "same"
    );
    let outcome = f
        .service
        .search(&anonymous, &[&f.public], "sentinel", None, 10)
        .unwrap();
    assert_eq!(outcome.len(), 1);
    for context in [&anonymous, &f.bob()] {
        unavailable(f.service.put(
            context,
            &f.public,
            "unauthorized",
            None,
            None,
            WriteOptions {
                idempotency_key: None,
                publish: true,
                expected_revision: None,
            },
        ));
        unavailable(f.service.delete(
            context,
            &f.public,
            "same",
            priorart::service::DeleteOptions {
                expected_revision: Some(1),
                idempotency_key: None,
            },
        ));
        unavailable(f.service.health(context, &f.public));
    }
    unavailable(f.service.get(&anonymous, &f.a, "same", None));
}

#[test]
fn public_authorship_publication_and_moderation_are_explicit() {
    let f = Fixture::new();
    let contributor = f.limited(&f.bob, &f.public, &[Op::Contribute, Op::Update, Op::Delete]);
    let author = f.context(&contributor);
    assert!(matches!(
        f.service.put(
            &author,
            &f.public,
            "public text",
            None,
            Some("bob-record"),
            WriteOptions::default()
        ),
        Err(ServiceError::InvalidInput(_))
    ));
    f.service
        .put(
            &author,
            &f.public,
            "public text",
            None,
            Some("bob-record"),
            WriteOptions {
                idempotency_key: None,
                publish: true,
                expected_revision: None,
            },
        )
        .unwrap();
    assert_eq!(
        f.service
            .get(&author, &f.public, "bob-record", None)
            .unwrap()
            .author_principal_id
            .as_deref(),
        Some(f.bob.as_str())
    );
    f.service
        .put(
            &author,
            &f.public,
            "corrected public text",
            None,
            Some("bob-record"),
            WriteOptions {
                idempotency_key: None,
                publish: true,
                expected_revision: Some(1),
            },
        )
        .unwrap();
    unavailable(f.service.put(
        &author,
        &f.public,
        "overwrite Alice",
        None,
        Some("same"),
        WriteOptions {
            idempotency_key: None,
            publish: true,
            expected_revision: None,
        },
    ));
    unavailable(f.service.delete(
        &author,
        &f.public,
        "same",
        priorart::service::DeleteOptions {
            expected_revision: Some(1),
            idempotency_key: None,
        },
    ));
    let owner_limited = f.limited(&f.alice, &f.public, &[Op::Update, Op::Delete, Op::Admin]);
    let owner_limited = f.context(&owner_limited);
    unavailable(f.service.put(
        &owner_limited,
        &f.public,
        "owner overwrite",
        None,
        Some("bob-record"),
        WriteOptions {
            idempotency_key: None,
            publish: true,
            expected_revision: None,
        },
    ));
    unavailable(f.service.delete(
        &owner_limited,
        &f.public,
        "bob-record",
        priorart::service::DeleteOptions {
            expected_revision: Some(1),
            idempotency_key: None,
        },
    ));
    f.service
        .put(
            &f.alice(),
            &f.public,
            "moderated",
            None,
            Some("bob-record"),
            WriteOptions {
                idempotency_key: None,
                publish: true,
                expected_revision: Some(2),
            },
        )
        .unwrap();
    assert_eq!(
        f.store
            .get(&f.public, "bob-record", None)
            .unwrap()
            .author_principal_id
            .as_deref(),
        Some(f.bob.as_str())
    );
    f.service
        .delete(
            &f.alice(),
            &f.public,
            "bob-record",
            priorart::service::DeleteOptions {
                expected_revision: Some(3),
                idempotency_key: None,
            },
        )
        .unwrap();
    let author = f.context(&contributor);
    f.service
        .delete(
            &author,
            &f.public,
            "bob-record",
            priorart::service::DeleteOptions {
                expected_revision: Some(3),
                idempotency_key: None,
            },
        )
        .unwrap(); // tombstone retains ownership
    assert!(f
        .service
        .put(
            &author,
            &f.public,
            "resurrect",
            None,
            Some("bob-record"),
            WriteOptions {
                idempotency_key: None,
                publish: true,
                expected_revision: None
            }
        )
        .is_err());
}

#[test]
fn contribute_does_not_grant_update_even_to_the_original_author() {
    let f = Fixture::new();
    let key = f.limited(&f.alice, &f.a, &[Op::Contribute]);
    let writer = f.context(&key);
    f.service
        .put(
            &writer,
            &f.a,
            "Bob's record",
            None,
            Some("bob"),
            WriteOptions::default(),
        )
        .unwrap();
    unavailable(f.service.put(
        &writer,
        &f.a,
        "correction",
        None,
        Some("bob"),
        WriteOptions::default(),
    ));
    unavailable(f.service.get(&writer, &f.a, "bob", None));
    unavailable(f.service.delete(
        &writer,
        &f.a,
        "bob",
        priorart::service::DeleteOptions {
            expected_revision: Some(1),
            idempotency_key: None,
        },
    ));
}

#[test]
fn revoked_and_reduced_credentials_cannot_reuse_loaded_indexes_or_public_read_access() {
    let f = Fixture::new();
    let key = f.limited(&f.alice, &f.a, &[Op::Read]);
    let old = f.context(&key);
    f.service
        .search(&old, &[&f.a], "sentinel", None, 10)
        .unwrap();
    let id = &old.credential().unwrap().id;
    f.store.replace_local_credential_grants(id, &[]).unwrap();
    unauthenticated(f.service.search(&old, &[&f.a], "sentinel", None, 10));
    let reduced = f.context(&key);
    unavailable(f.service.search(&reduced, &[&f.a], "sentinel", None, 10));
    f.service
        .search(&reduced, &[&f.public], "sentinel", None, 10)
        .unwrap();
    f.store.revoke_local_credential(id).unwrap();
    unauthenticated(f.service.get(&reduced, &f.public, "same", None));
    unauthenticated(
        f.service
            .search(&reduced, &[&f.public], "sentinel", None, 10),
    );
    unauthenticated(f.service.health(&reduced, &f.public));
    assert!(f.service.authenticate(&key).is_err());
}

#[test]
fn local_context_is_neither_anonymous_nor_a_cross_collection_admin() {
    let f = Fixture::new();
    let local = RequestContext::local();
    f.service
        .put(
            &local,
            LOCAL_COLLECTION_ID,
            "local text",
            None,
            Some("same"),
            WriteOptions::default(),
        )
        .unwrap();
    assert_eq!(
        f.service
            .health(&local, LOCAL_COLLECTION_ID)
            .unwrap()
            .document_count,
        1
    );
    for collection in [&f.a, &f.b, &f.public, "unknown"] {
        unavailable(f.service.get(&local, collection, "same", None));
        unavailable(
            f.service
                .search(&local, &[collection], "sentinel", None, 10),
        );
        unavailable(f.service.health(&local, collection));
    }
    unavailable(f.service.get(
        &RequestContext::anonymous(),
        LOCAL_COLLECTION_ID,
        "same",
        None,
    ));
}

#[test]
fn diagnostics_count_only_the_authorized_collection_after_eviction() {
    let f = Fixture::new();
    f.service
        .put(
            &f.alice(),
            &f.a,
            "extra",
            None,
            Some("extra"),
            WriteOptions::default(),
        )
        .unwrap();
    assert_eq!(
        f.service.health(&f.alice(), &f.a).unwrap().document_count,
        2
    );
    assert_eq!(f.service.health(&f.bob(), &f.b).unwrap().document_count, 1);
    assert_eq!(
        f.service.health(&f.alice(), &f.a).unwrap().document_count,
        2
    );
    unavailable(f.service.health(&f.bob(), &f.a));
}

#[test]
fn expired_credentials_fail_even_for_public_data() {
    let f = Fixture::new();
    let context = f.bob();
    let id = &context.credential().unwrap().id;
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
    rusqlite::Connection::open(f.store.path())
        .unwrap()
        .execute(
            "UPDATE credentials SET created_at = ?2 - 100, expires_at = ?2 WHERE id = ?1",
            (id, timestamp),
        )
        .unwrap();
    unauthenticated(f.service.get(&context, &f.public, "same", None));
    unauthenticated(
        f.service
            .search(&context, &[&f.public], "sentinel", None, 10),
    );
    unauthenticated(f.service.put(
        &context,
        &f.public,
        "text",
        None,
        None,
        WriteOptions {
            idempotency_key: None,
            publish: true,
            expected_revision: None,
        },
    ));
    unauthenticated(f.service.delete(
        &context,
        &f.public,
        "same",
        priorart::service::DeleteOptions {
            expected_revision: Some(1),
            idempotency_key: None,
        },
    ));
    unauthenticated(f.service.health(&context, &f.public));
}
