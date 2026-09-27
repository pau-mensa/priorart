use priorart::service::WriteOptions;
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::FakeEncoder;
use lateweave::{Representation, TokenMatrix};
use priorart::auth::{AuthError, Grant, Operation as Op, RequestContext};
use priorart::config::Settings;
use priorart::encoder::{Encoder, EncoderError};
use priorart::index::collection_index_path;
use priorart::policy::PolicyError;
use priorart::service::{Service, ServiceError, DATABASE_FILE};
use priorart::store::{Metadata, Store, Visibility, LOCAL_COLLECTION_ID};
use serde_json::json;
use tempfile::TempDir;

const ALL: &[Op] = &[
    Op::Read,
    Op::Contribute,
    Op::Update,
    Op::Delete,
    Op::Report,
    Op::FeedbackRead,
    Op::Moderate,
    Op::Admin,
    Op::Delegate,
];

struct Fixture {
    dir: TempDir,
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
    fn new(encoder: Option<Arc<dyn Encoder>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
        let alice = store.create_principal().unwrap();
        let bob = store.create_principal().unwrap();
        let aa = store.create_account(&alice).unwrap();
        let ba = store.create_account(&bob).unwrap();
        let a = store
            .create_collection(&aa, Visibility::Restricted)
            .unwrap();
        let b = store
            .create_collection(&ba, Visibility::Restricted)
            .unwrap();
        let public = store.create_collection(&aa, Visibility::Public).unwrap();
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
        let service = Service::new(
            Settings {
                data_dir: dir.path().to_owned(),
                max_loaded_indexes: 1,
                ..Settings::default()
            },
            encoder,
        )
        .unwrap();
        Self {
            dir,
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
    fn delegated(&self, principal: &str, collection: &str, ops: &[Op]) -> String {
        let grants: Vec<_> = ops.iter().map(|op| Grant::new(collection, *op)).collect();
        self.store
            .delegate_credential_to(&self.alice(), principal, &grants, None)
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
fn unrelated_accounts_cannot_read_mutate_report_or_inspect_forbidden_scope() {
    let f = Fixture::new(None);
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
                unavailable(f.service.delete(&context, collection, record, Some(1))),
                expected
            );
            assert_eq!(
                unavailable(f.service.report(
                    &context,
                    collection,
                    record,
                    "report",
                    Some(1),
                    None
                )),
                expected
            );
            assert_eq!(
                unavailable(f.service.reports(&context, collection, record)),
                expected
            );
        }
        assert_eq!(
            unavailable(
                f.service
                    .search(&context, collection, "sentinel", Some(&tags), 10)
            ),
            expected
        );
        assert_eq!(
            unavailable(f.service.health(&context, collection)),
            expected
        );
        assert!(!collection_index_path(f.dir.path(), collection).exists());
    }
    assert_eq!(f.store.get(&f.a, "same", None).unwrap().revision, 1);
    let outcome = f
        .service
        .search(&context, &f.b, "sentinel", Some(&tags), 10)
        .unwrap();
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.hits[0].collection_id, f.b);
    assert!(outcome.hits[0].excerpt.contains("beta"));
}

#[test]
fn read_scope_includes_old_revisions_but_not_mutations_feedback_or_diagnostics() {
    let f = Fixture::new(None);
    let key = f.delegated(&f.bob, &f.a, &[Op::Read]);
    f.service
        .put(
            &f.alice(),
            &f.a,
            "second version",
            None,
            Some("same"),
            WriteOptions {
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
        .search(&reader, &f.a, "second", None, 10)
        .unwrap()
        .hits[0]
        .excerpt
        .contains("second"));
    unavailable(
        f.service
            .put(&reader, &f.a, "new", None, None, WriteOptions::default()),
    );
    unavailable(f.service.delete(&reader, &f.a, "same", Some(1)));
    unavailable(
        f.service
            .report(&reader, &f.a, "same", "report", None, None),
    );
    unavailable(f.service.reports(&reader, &f.a, "same"));
    unavailable(f.service.health(&reader, &f.a));
    unavailable(f.service.get(&reader, &f.b, "same", None));
}

#[test]
fn anonymous_public_reads_never_log_and_mutations_require_credentials() {
    let f = Fixture::new(None);
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
        .search(&anonymous, &f.public, "sentinel", None, 10)
        .unwrap();
    assert_eq!(outcome.hits.len(), 1);
    assert!(outcome.search_id.is_none());
    let count: i64 = rusqlite::Connection::open(f.store.path())
        .unwrap()
        .query_row("SELECT count(*) FROM searches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    for context in [&anonymous, &f.bob()] {
        unavailable(f.service.put(
            context,
            &f.public,
            "unauthorized",
            None,
            None,
            WriteOptions {
                publish: true,
                expected_revision: None,
            },
        ));
        unavailable(f.service.delete(context, &f.public, "same", Some(1)));
        unavailable(
            f.service
                .report(context, &f.public, "same", "report", Some(1), None),
        );
        unavailable(f.service.reports(context, &f.public, "same"));
        unavailable(f.service.health(context, &f.public));
    }
    unavailable(f.service.get(&anonymous, &f.a, "same", None));
}

#[test]
fn public_authorship_publication_and_moderation_are_explicit() {
    let f = Fixture::new(None);
    let contributor = f.delegated(&f.bob, &f.public, &[Op::Contribute, Op::Update, Op::Delete]);
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
            publish: true,
            expected_revision: None,
        },
    ));
    unavailable(f.service.delete(&author, &f.public, "same", Some(1)));
    let owner_limited = f.delegated(&f.alice, &f.public, &[Op::Update, Op::Delete, Op::Admin]);
    let owner_limited = f.context(&owner_limited);
    unavailable(f.service.put(
        &owner_limited,
        &f.public,
        "owner overwrite",
        None,
        Some("bob-record"),
        WriteOptions {
            publish: true,
            expected_revision: None,
        },
    ));
    unavailable(
        f.service
            .delete(&owner_limited, &f.public, "bob-record", Some(1)),
    );
    f.service
        .put(
            &f.alice(),
            &f.public,
            "moderated",
            None,
            Some("bob-record"),
            WriteOptions {
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
        .delete(&f.alice(), &f.public, "bob-record", Some(3))
        .unwrap();
    let author = f.context(&contributor);
    f.service
        .delete(&author, &f.public, "bob-record", Some(3))
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
                publish: true,
                expected_revision: None
            }
        )
        .is_err());
}

#[test]
fn contribute_does_not_grant_update_even_to_the_original_author() {
    let f = Fixture::new(None);
    let key = f.delegated(&f.bob, &f.a, &[Op::Contribute]);
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
    unavailable(f.service.delete(&writer, &f.a, "bob", Some(1)));
}

#[test]
fn reports_and_attached_receipts_belong_to_the_requesting_principal() {
    let f = Fixture::new(None);
    let key = f.delegated(&f.bob, &f.public, &[Op::Report, Op::FeedbackRead]);
    let alice = f.alice();
    let bob = f.context(&key);
    let receipt = f
        .service
        .search(&alice, &f.public, "sentinel", None, 10)
        .unwrap()
        .search_id
        .unwrap();
    f.service
        .report(
            &alice,
            &f.public,
            "same",
            "Alice private feedback",
            Some(1),
            Some(&receipt),
        )
        .unwrap();
    f.service
        .report(
            &bob,
            &f.public,
            "same",
            "Bob private feedback",
            Some(1),
            None,
        )
        .unwrap();
    let alice_reports = f.service.reports(&alice, &f.public, "same").unwrap();
    let bob_reports = f.service.reports(&bob, &f.public, "same").unwrap();
    assert_eq!(alice_reports.len(), 1);
    assert_eq!(bob_reports.len(), 1);
    assert_eq!(bob_reports[0].text, "Bob private feedback");
    assert_eq!(alice_reports[0].text, "Alice private feedback");
    for id in [&receipt, "unknown-receipt"] {
        let error = f
            .service
            .report(&bob, &f.public, "same", "attack", Some(1), Some(id))
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::Store(priorart::store::StoreError::SearchNotFound { .. })
        ));
    }
    unavailable(
        f.service
            .reports(&RequestContext::anonymous(), &f.public, "same"),
    );
    f.service
        .delete(&alice, &f.public, "same", Some(1))
        .unwrap();
    assert!(f.service.reports(&bob, &f.public, "same").is_err());
}

#[test]
fn revoked_and_reduced_credentials_cannot_reuse_loaded_indexes_or_public_read_access() {
    let f = Fixture::new(None);
    let key = f.delegated(&f.bob, &f.a, &[Op::Read]);
    let old = f.context(&key);
    f.service.search(&old, &f.a, "sentinel", None, 10).unwrap();
    let id = &old.credential().unwrap().id;
    f.store.replace_local_credential_grants(id, &[]).unwrap();
    unauthenticated(f.service.search(&old, &f.a, "sentinel", None, 10));
    let reduced = f.context(&key);
    unavailable(f.service.search(&reduced, &f.a, "sentinel", None, 10));
    f.service
        .search(&reduced, &f.public, "sentinel", None, 10)
        .unwrap();
    f.store.revoke_local_credential(id).unwrap();
    unauthenticated(f.service.get(&reduced, &f.public, "same", None));
    unauthenticated(f.service.search(&reduced, &f.public, "sentinel", None, 10));
    unauthenticated(f.service.health(&reduced, &f.public));
    assert!(f.service.authenticate(&key).is_err());
}

#[test]
fn local_context_is_neither_anonymous_nor_a_cross_collection_admin() {
    let f = Fixture::new(None);
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
        unavailable(f.service.search(&local, collection, "sentinel", None, 10));
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
    let f = Fixture::new(None);
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

struct RevokingEncoder {
    fake: FakeEncoder,
    action: Mutex<Option<(PathBuf, String, bool)>>,
}
impl RevokingEncoder {
    fn revoke(&self, query: bool) {
        let mut action = self.action.lock().unwrap();
        if action
            .as_ref()
            .is_some_and(|(_, _, on_query)| *on_query == query)
        {
            let (path, id, _) = action.take().unwrap();
            Store::open(path)
                .unwrap()
                .revoke_local_credential(&id)
                .unwrap();
        }
    }
}
impl Encoder for RevokingEncoder {
    fn representation(&self) -> &Representation {
        self.fake.representation()
    }
    fn encode_queries(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        self.revoke(true);
        self.fake.encode_queries(texts)
    }
    fn encode_documents(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        self.revoke(false);
        self.fake.encode_documents(texts)
    }
}

#[test]
fn revocation_during_encoding_blocks_search_results_and_record_commits() {
    for query in [false, true] {
        let encoder = Arc::new(RevokingEncoder {
            fake: FakeEncoder::new(),
            action: Mutex::new(None),
        });
        let f = Fixture::new(Some(encoder.clone()));
        // Load vectors before arming the hook, so the write check runs after the
        // new document is encoded and the search check after query encoding.
        f.service.health(&f.alice(), &f.a).unwrap();
        let context = f.alice();
        *encoder.action.lock().unwrap() = Some((
            f.store.path().to_owned(),
            context.credential().unwrap().id.clone(),
            query,
        ));
        if query {
            unauthenticated(f.service.search(&context, &f.a, "sentinel", None, 10));
            let count: i64 = rusqlite::Connection::open(f.store.path())
                .unwrap()
                .query_row("SELECT count(*) FROM searches", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        } else {
            unauthenticated(f.service.put(
                &context,
                &f.a,
                "denied update",
                None,
                Some("same"),
                WriteOptions {
                    publish: false,
                    expected_revision: Some(1),
                },
            ));
            assert_eq!(f.store.get(&f.a, "same", None).unwrap().revision, 1);
        }
    }
}

#[test]
fn expired_credentials_fail_even_for_public_data() {
    let f = Fixture::new(None);
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
    unauthenticated(f.service.search(&context, &f.public, "sentinel", None, 10));
    unauthenticated(f.service.put(
        &context,
        &f.public,
        "text",
        None,
        None,
        WriteOptions {
            publish: true,
            expected_revision: None,
        },
    ));
    unauthenticated(f.service.delete(&context, &f.public, "same", Some(1)));
    unauthenticated(
        f.service
            .report(&context, &f.public, "same", "feedback", None, None),
    );
    unauthenticated(f.service.reports(&context, &f.public, "same"));
    unauthenticated(f.service.health(&context, &f.public));
}

#[test]
fn revocation_during_index_recovery_blocks_diagnostics_and_deletes() {
    for diagnostics in [false, true] {
        let encoder = Arc::new(RevokingEncoder {
            fake: FakeEncoder::new(),
            action: Mutex::new(None),
        });
        let f = Fixture::new(Some(encoder.clone()));
        let context = f.alice();
        *encoder.action.lock().unwrap() = Some((
            f.store.path().to_owned(),
            context.credential().unwrap().id.clone(),
            false,
        ));
        if diagnostics {
            unauthenticated(f.service.health(&context, &f.a));
        } else {
            unauthenticated(f.service.delete(&context, &f.a, "same", Some(1)));
        }
        assert!(f.store.get(&f.a, "same", None).is_ok());
    }
}
