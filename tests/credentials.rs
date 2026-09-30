use priorart::auth::RequestContext;
use priorart::auth::{AuthError, Grant, Operation, MAX_GRANTS};
use priorart::config::Settings;
use priorart::service::WriteOptions;
use priorart::service::{Service, DATABASE_FILE};
use priorart::store::LOCAL_COLLECTION_ID;
use priorart::store::{
    Store, Visibility, LOCAL_COLLECTION_ID as LOCAL, LOCAL_PRINCIPAL_ID as PRINCIPAL,
};
use rusqlite::Connection;
use tempfile::TempDir;
use time::OffsetDateTime;

fn setup() -> (TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
    (directory, store)
}
fn grants(operations: &[Operation]) -> Vec<Grant> {
    operations.iter().map(|op| Grant::new(LOCAL, *op)).collect()
}
fn expiry() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp() + 3600
}
fn issue(store: &Store, operations: &[Operation]) -> (String, String) {
    let issued = store
        .issue_credential(PRINCIPAL, &grants(operations), None)
        .unwrap();
    (issued.info.id.clone(), issued.into_secret())
}

#[test]
fn secrets_are_random_returned_once_and_never_persisted_or_debugged() {
    let (directory, store) = setup();
    let issued = store
        .issue_credential(PRINCIPAL, &grants(&[Operation::Read]), None)
        .unwrap();
    let debug = format!("{issued:?}");
    let id = issued.info.id.clone();
    let metadata = serde_json::to_string(&issued.info).unwrap();
    let secret = issued.into_secret();
    assert!(secret.starts_with(&format!("pa1_{id}_")));
    assert_eq!(secret.len(), 101);
    assert!(debug.contains("[REDACTED]"));
    for value in [&debug, &metadata] {
        assert!(!value.contains(&secret));
    }
    let context = store.authenticate(&secret).unwrap();
    assert_eq!(context.principal_id(), Some(PRINCIPAL));
    assert!(context.has_grant(LOCAL, Operation::Read));
    assert!(!context.has_grant(LOCAL, Operation::Admin));
    assert!(!context.has_grant("other", Operation::Read));
    assert!(!format!("{context:?}").contains(&secret));
    let (_, second) = issue(&store, &[Operation::Read]);
    assert!(secret != second);
    let connection = Connection::open(store.path()).unwrap();
    let verifier: Vec<u8> = connection
        .query_row(
            "SELECT verifier FROM credentials WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(verifier.len(), 32);
    let listing = serde_json::to_string(&store.credentials(PRINCIPAL).unwrap()).unwrap();
    assert!(!listing.contains(&secret));
    assert!(!listing.contains("verifier"));
    // Scan DB, WAL and SHM while live, and the checkpointed DB after close.
    let check = || {
        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
            let suffix = secret.rsplit('_').next().unwrap();
            assert!(!bytes.windows(suffix.len()).any(|w| w == suffix.as_bytes()));
        }
    };
    check();
    drop(connection);
    drop(store);
    check();
    let reopened = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
    assert!(reopened.authenticate(&secret).is_ok());
}

#[test]
fn malformed_unknown_and_wrong_keys_have_one_redacted_error() {
    let (_directory, store) = setup();
    let (_, secret) = issue(&store, &[Operation::Read]);
    let mut wrong = secret.clone().into_bytes();
    let last = wrong.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    let wrong = String::from_utf8(wrong).unwrap();
    let unknown = format!("pa1_{}_{}", "0".repeat(32), "f".repeat(64));
    for value in [
        "",
        "secret",
        &wrong,
        &unknown,
        &format!("Bearer {secret}"),
        &format!("{secret}\n"),
        &"a".repeat(10_000),
    ] {
        let error = store.authenticate(value).unwrap_err();
        assert!(matches!(error, AuthError::Unauthenticated));
        assert_eq!(error.to_string(), "invalid or expired credential");
        assert!(!format!("{error:?}").contains(&secret));
    }
}

#[test]
fn expiry_is_exclusive_and_expired_contexts_are_rejected() {
    let (_directory, store) = setup();
    let issued = store
        .issue_credential(PRINCIPAL, &grants(&[Operation::Read]), Some(expiry()))
        .unwrap();
    let id = issued.info.id.clone();
    let secret = issued.into_secret();
    let context = store.authenticate(&secret).unwrap();
    let connection = Connection::open(store.path()).unwrap();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    connection
        .execute(
            "UPDATE credentials SET created_at = ?2 - 100, expires_at = ?2 WHERE id = ?1",
            (&id, now),
        )
        .unwrap();
    assert!(matches!(
        store.authenticate(&secret),
        Err(AuthError::Unauthenticated)
    ));
    assert!(store.validate_context(&context).is_err());
    for end in [now, now - 1, 0] {
        assert!(store
            .issue_credential(PRINCIPAL, &grants(&[Operation::Read]), Some(end))
            .is_err());
    }
}

#[test]
fn independent_revocation_and_versioned_contexts() {
    let (_directory, store) = setup();
    let (first_id, first) = issue(&store, &[Operation::Read]);
    let (_, second) = issue(&store, &[Operation::Write]);
    let first_context = store.authenticate(&first).unwrap();
    let second_context = store.authenticate(&second).unwrap();
    store.revoke_credential(&first_id).unwrap();
    store.revoke_credential(&first_id).unwrap(); // idempotent
    assert!(store.authenticate(&first).is_err());
    assert!(store.validate_context(&first_context).is_err());
    assert!(store.validate_context(&second_context).is_err()); // principal version changed
    let refreshed = store.authenticate(&second).unwrap();
    assert!(refreshed.principal_version() > second_context.principal_version());
    assert!(store.validate_context(&refreshed).is_ok());
    assert!(!refreshed.has_grant(LOCAL, Operation::Read));
}

#[test]
fn explicit_grants_are_deduplicated_bounded_and_owner_scoped() {
    let (_directory, store) = setup();
    let other = store.create_principal().unwrap();
    let collection = store
        .create_collection(&other, Visibility::Restricted)
        .unwrap();
    for collection in [&collection, "missing", "*"] {
        assert!(matches!(
            store.issue_credential(PRINCIPAL, &[Grant::new(collection, Operation::Read)], None),
            Err(AuthError::Forbidden)
        ));
    }
    assert!(store.issue_credential("missing", &[], None).is_err());
    let issued = store
        .issue_credential(
            PRINCIPAL,
            &grants(&[Operation::Read, Operation::Read]),
            None,
        )
        .unwrap();
    assert_eq!(issued.info.grants.len(), 1);
    assert!(store
        .issue_credential(PRINCIPAL, &grants(&[Operation::Read; MAX_GRANTS + 1]), None)
        .is_err());
    let empty = store.issue_credential(PRINCIPAL, &[], None).unwrap();
    assert!(!store
        .authenticate(&empty.into_secret())
        .unwrap()
        .has_grant(LOCAL, Operation::Read));
    assert!("local:unknown".parse::<Grant>().is_err());
    assert!(":read".parse::<Grant>().is_err());
    assert!("read".parse::<Grant>().is_err());
}

#[test]
fn grant_changes_invalidate_contexts_and_stay_grantable() {
    let (_directory, store) = setup();
    let (parent_id, parent) = issue(&store, &[Operation::Read]);
    let before = store.authenticate(&parent).unwrap();
    let other = store.create_principal().unwrap();
    let foreign = store
        .create_collection(&other, Visibility::Restricted)
        .unwrap();
    assert!(matches!(
        store.replace_credential_grants(&parent_id, &[Grant::new(&foreign, Operation::Read)]),
        Err(AuthError::Forbidden)
    ));
    assert!(store.validate_context(&before).is_ok());
    store.replace_credential_grants(&parent_id, &[]).unwrap();
    assert!(store.validate_context(&before).is_err());
    let after = store.authenticate(&parent).unwrap();
    assert!(after.credential().unwrap().grant_version > before.credential().unwrap().grant_version);
    assert!(!after.has_grant(LOCAL, Operation::Read));
    store
        .replace_credential_grants(&parent_id, &grants(&[Operation::Admin]))
        .unwrap();
    let restored = store.authenticate(&parent).unwrap();
    assert!(restored.has_grant(LOCAL, Operation::Admin));
    assert!(!restored.has_grant(LOCAL, Operation::Read));
}

#[test]
fn rotation_is_atomic_preserves_limits_and_revokes_the_old_key() {
    let (_directory, store) = setup();
    let end = expiry();
    let parent = store
        .issue_credential(
            PRINCIPAL,
            &grants(&[Operation::Read, Operation::Write]),
            Some(end),
        )
        .unwrap();
    let id = parent.info.id.clone();
    let old = parent.into_secret();
    let before = store.authenticate(&old).unwrap();
    let rotated = store.rotate_credential(&id).unwrap();
    assert_ne!(rotated.info.id, id);
    assert_eq!(rotated.info.expires_at, Some(end));
    assert_eq!(rotated.info.grants, before.credential().unwrap().grants);
    assert!(store.authenticate(&old).is_err());
    assert!(store.validate_context(&before).is_err());
    assert!(store.authenticate(&rotated.into_secret()).is_ok());
    assert!(store.rotate_credential(&id).is_err());
}

#[test]
fn failed_rotation_and_grant_replacement_roll_back() {
    let (_directory, store) = setup();
    let (id, secret) = issue(&store, &[Operation::Read]);
    let before = store.authenticate(&secret).unwrap();
    let connection = Connection::open(store.path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_revoke BEFORE UPDATE OF revoked_at ON credentials BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    assert!(store.rotate_credential(&id).is_err());
    assert_eq!(store.credentials(PRINCIPAL).unwrap().len(), 1);
    assert!(store.validate_context(&before).is_ok());
    connection.execute_batch("CREATE TRIGGER fail_grant BEFORE INSERT ON credential_grants BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
    assert!(store
        .replace_credential_grants(&id, &grants(&[Operation::Write]))
        .is_err());
    assert!(store.validate_context(&before).is_ok());
}

#[test]
fn service_context_revalidation_observes_local_administration() {
    let (directory, store) = setup();
    let (id, secret) = issue(&store, &[Operation::Read]);
    let service = Service::open(Settings {
        data_dir: directory.path().to_owned(),
        ..Settings::default()
    })
    .unwrap();
    let context = service.authenticate(&secret).unwrap();
    service.validate_context(&context).unwrap();
    store.revoke_credential(&id).unwrap();
    assert!(service.authenticate(&secret).is_err());
    assert!(service.validate_context(&context).is_err());
    // Existing operations are still explicitly local, pending step 06 policy.
    service
        .put(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "local",
            None,
            Some("record"),
            WriteOptions::default(),
        )
        .unwrap();
    assert_eq!(
        service
            .get(
                &RequestContext::local(),
                LOCAL_COLLECTION_ID,
                "record",
                None
            )
            .unwrap()
            .author_principal_id
            .as_deref(),
        Some(PRINCIPAL)
    );
}

#[test]
fn principals_are_independent_and_revocation_does_not_cross_them() {
    let (_directory, store) = setup();
    let second_collection = store
        .create_collection(PRINCIPAL, Visibility::Restricted)
        .unwrap();
    let (_, own) = issue(&store, &[Operation::Read]);
    let both = store
        .issue_credential(
            PRINCIPAL,
            &[
                Grant::new(LOCAL, Operation::Read),
                Grant::new(&second_collection, Operation::Admin),
            ],
            None,
        )
        .unwrap()
        .into_secret();
    assert!(store
        .authenticate(&both)
        .unwrap()
        .has_grant(&second_collection, Operation::Admin));
    let other_principal = store.create_principal().unwrap();
    let other_collection = store
        .create_collection(&other_principal, Visibility::Restricted)
        .unwrap();
    let other = store
        .issue_credential(
            &other_principal,
            &[Grant::new(&other_collection, Operation::Read)],
            None,
        )
        .unwrap();
    let other_id = other.info.id.clone();
    let other_context = store.authenticate(&other.into_secret()).unwrap();
    assert_eq!(other_context.principal_id(), Some(other_principal.as_str()));
    assert!(!other_context.has_grant(LOCAL, Operation::Read));
    let own_context = store.authenticate(&own).unwrap();
    store.revoke_credential(&other_id).unwrap();
    assert!(store.validate_context(&own_context).is_ok());
    assert!(store.validate_context(&other_context).is_err());
}

#[test]
fn public_collections_accept_record_grants_from_any_principal() {
    let (_directory, store) = setup();
    let owner = store.create_principal().unwrap();
    let public = store.create_collection(&owner, Visibility::Public).unwrap();
    let restricted = store
        .create_collection(&owner, Visibility::Restricted)
        .unwrap();
    let record_operations = [Operation::Read, Operation::Write];
    let issued = store
        .issue_credential(
            PRINCIPAL,
            &record_operations
                .iter()
                .map(|op| Grant::new(&public, *op))
                .collect::<Vec<_>>(),
            None,
        )
        .unwrap();
    let context = store.authenticate(&issued.into_secret()).unwrap();
    for operation in record_operations {
        assert!(context.has_grant(&public, operation));
    }
    assert!(matches!(
        store.issue_credential(PRINCIPAL, &[Grant::new(&public, Operation::Admin)], None),
        Err(AuthError::Forbidden)
    ));
    for operation in record_operations {
        assert!(matches!(
            store.issue_credential(PRINCIPAL, &[Grant::new(&restricted, operation)], None),
            Err(AuthError::Forbidden)
        ));
    }
    assert!(store
        .issue_credential(&owner, &[Grant::new(&public, Operation::Admin)], None)
        .is_ok());
}

#[test]
fn concurrent_rotation_has_exactly_one_successful_replacement() {
    let (_directory, store) = setup();
    let (id, secret) = issue(&store, &[Operation::Read]);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = store.path().to_owned();
            let id = id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(path).unwrap();
                barrier.wait();
                store.rotate_credential(&id)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(results
        .iter()
        .any(|r| matches!(r, Err(AuthError::Unauthenticated))));
    let replacement = results.into_iter().find_map(Result::ok).unwrap();
    assert!(store.authenticate(&secret).is_err());
    assert!(store.authenticate(&replacement.into_secret()).is_ok());
    assert_eq!(store.credentials(PRINCIPAL).unwrap().len(), 2);
}
