use std::process::{Command, Output};

use priorart::service::DATABASE_FILE;
use priorart::store::Store;
use serde_json::Value;
use tempfile::TempDir;

fn admin(directory: &TempDir, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_priorart"))
        .env("PRIORART_DATA_DIR", directory.path())
        // Admin commands must never try loading an encoder, even if configured.
        .env("PRIORART_ENCODER", "/nonexistent/model")
        .args(["admin"])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn local_bootstrap_listing_rotation_and_revocation() {
    let directory = tempfile::tempdir().unwrap();
    let issued = admin(
        &directory,
        &[
            "issue",
            "--grant",
            "local:read",
            "--grant",
            "local:contribute",
        ],
    );
    assert!(issued.status.success());
    assert!(issued.stderr.is_empty());
    let value: Value = serde_json::from_slice(&issued.stdout).unwrap();
    let secret = value["secret"].as_str().unwrap();
    let id = value["credential"]["id"].as_str().unwrap();
    let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
    assert!(store.authenticate(secret).is_ok());
    assert_eq!(
        store
            .get_collection("local")
            .unwrap()
            .visibility
            .to_string(),
        "restricted"
    );
    let listed = admin(&directory, &["list"]);
    assert!(listed.status.success());
    let listing = String::from_utf8(listed.stdout).unwrap();
    assert!(!listing.contains(secret));
    assert!(!listing.contains("secret"));
    assert!(!listing.contains("verifier"));
    assert!(listed.stderr.is_empty());
    let rotated = admin(&directory, &["rotate", "--credential-id", id]);
    assert!(rotated.status.success());
    assert!(rotated.stderr.is_empty());
    let replacement: Value = serde_json::from_slice(&rotated.stdout).unwrap();
    let new_secret = replacement["secret"].as_str().unwrap();
    let new_id = replacement["credential"]["id"].as_str().unwrap();
    assert!(store.authenticate(secret).is_err());
    assert!(store.authenticate(new_secret).is_ok());
    let cleared = admin(&directory, &["set-grants", "--credential-id", new_id]);
    assert!(cleared.status.success());
    assert!(cleared.stdout.is_empty());
    assert!(store
        .authenticate(new_secret)
        .unwrap()
        .credential()
        .unwrap()
        .grants
        .is_empty());
    let revoked = admin(&directory, &["revoke", "--credential-id", new_id]);
    assert!(revoked.status.success());
    assert!(revoked.stdout.is_empty());
    assert!(store.authenticate(new_secret).is_err());
}

#[test]
fn local_admin_requires_explicit_scope_and_does_not_echo_invalid_ids() {
    let directory = tempfile::tempdir().unwrap();
    assert!(!admin(&directory, &["issue"]).status.success());
    assert!(!directory.path().join(DATABASE_FILE).exists());
    let unknown = admin(&directory, &["issue", "--grant", "missing:read"]);
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    let sentinel = "potential-secret-mistaken-for-id";
    let invalid = admin(&directory, &["rotate", "--credential-id", sentinel]);
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    assert!(!String::from_utf8(invalid.stderr)
        .unwrap()
        .contains(sentinel));
}

#[test]
fn local_collection_provisioning_defaults_to_restricted() {
    let directory = tempfile::tempdir().unwrap();
    for (arguments, visibility) in [
        (vec!["create-collection"], "restricted"),
        (
            vec!["create-collection", "--visibility", "public"],
            "public",
        ),
    ] {
        let output = admin(&directory, &arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let id = String::from_utf8(output.stdout).unwrap();
        let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
        let collection = store.get_collection(id.trim()).unwrap();
        assert_eq!(collection.visibility.as_str(), visibility);
        assert_eq!(
            collection.owner_principal_id,
            priorart::store::LOCAL_PRINCIPAL_ID
        );
    }
    assert!(!admin(
        &directory,
        &["create-collection", "--visibility", "unknown"]
    )
    .status
    .success());
}

#[test]
fn created_principals_own_collections_and_hold_their_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let output = admin(&directory, &["create-principal"]);
    assert!(output.status.success());
    let principal = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let output = admin(&directory, &["create-collection", "--owner", &principal]);
    assert!(output.status.success());
    let collection = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
    assert_eq!(
        store
            .get_collection(&collection)
            .unwrap()
            .owner_principal_id,
        principal
    );
    let grant = format!("{collection}:admin");
    let issued = admin(
        &directory,
        &["issue", "--principal", &principal, "--grant", &grant],
    );
    assert!(issued.status.success());
    let value: Value = serde_json::from_slice(&issued.stdout).unwrap();
    let context = store
        .authenticate(value["secret"].as_str().unwrap())
        .unwrap();
    assert_eq!(context.principal_id(), Some(principal.as_str()));
    // The default local principal does not own the new restricted collection.
    assert!(!admin(&directory, &["issue", "--grant", &grant])
        .status
        .success());
    assert!(
        !admin(&directory, &["create-collection", "--owner", "missing"])
            .status
            .success()
    );
}
