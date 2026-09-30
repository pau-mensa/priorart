mod common;

use priorart::{
    auth::{Grant, Operation as Op},
    config::{ServerMode, Settings},
    service::{Service, DATABASE_FILE},
    store::{Store, Visibility},
};
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

const OWNER: &[Op] = &[Op::Admin];
struct Api {
    dir: TempDir,
    store: Store,
    client: Client,
    url: String,
    a: String,
    b: String,
    public: String,
    alice: String,
    bob: String,
    alice_key: String,
    bob_key: String,
}
impl Api {
    async fn start() -> Self {
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
        let grants: Vec<_> = [&a, &public]
            .into_iter()
            .flat_map(|c| OWNER.iter().map(move |op| Grant::new(c, *op)))
            .collect();
        let alice_key = store
            .issue_local_credential(&alice, &grants, None)
            .unwrap()
            .into_secret();
        let bob_key = store
            .issue_local_credential(
                &bob,
                &OWNER
                    .iter()
                    .map(|op| Grant::new(&b, *op))
                    .collect::<Vec<_>>(),
                None,
            )
            .unwrap()
            .into_secret();
        for (c, author, text) in [
            (&a, &alice, "alpha private sentinel"),
            (&b, &bob, "beta private sentinel"),
            (&public, &alice, "public sentinel"),
        ] {
            store
                .put(
                    c,
                    text,
                    Some(json!({"tag": "shared"}).as_object().unwrap()),
                    Some("same"),
                    author,
                    None,
                )
                .unwrap();
            store
                .put(
                    c,
                    "deleted secret sentinel",
                    None,
                    Some("gone"),
                    author,
                    None,
                )
                .unwrap();
            store.delete(c, "gone", Some(1)).unwrap();
        }
        let service = Service::open(Settings {
            data_dir: dir.path().to_owned(),
            mode: ServerMode::Authenticated,
            ..Settings::default()
        })
        .unwrap();
        let url = common::spawn(Arc::new(service)).await;
        Self {
            dir,
            store,
            client: Client::new(),
            url,
            a,
            b,
            public,
            alice,
            bob,
            alice_key,
            bob_key,
        }
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.url));
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        (
            response.status(),
            response.json().await.unwrap_or(Value::Null),
        )
    }
    async fn get(&self, path: &str, key: Option<&str>) -> (StatusCode, Value) {
        self.request(Method::GET, path, key, None).await
    }
    async fn post(&self, path: &str, key: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.request(Method::POST, path, key, Some(body)).await
    }
    fn records(&self, collection: &str) -> String {
        format!("/v1/collections/{collection}/records")
    }
    fn issue(&self, principal: &str, grants: Vec<Grant>) -> String {
        self.store
            .issue_local_credential(principal, &grants, None)
            .unwrap()
            .into_secret()
    }
}

#[tokio::test]
async fn scope_is_required_and_forbidden_objects_are_indistinguishable() {
    let api = Api::start().await;
    let hidden = api
        .get(
            &format!("{}/same", api.records(&api.b)),
            Some(&api.alice_key),
        )
        .await;
    for collection in [&api.b, "unknown"] {
        for record in ["same", "gone", "missing"] {
            assert_eq!(
                api.get(
                    &format!("{}/{record}", api.records(collection)),
                    Some(&api.alice_key)
                )
                .await,
                hidden
            );
            assert_eq!(
                api.request(
                    Method::DELETE,
                    &format!("{}/{record}", api.records(collection)),
                    Some(&api.alice_key),
                    None
                )
                .await,
                hidden
            );
        }
        for collections in [json!([collection]), json!([api.a, collection])] {
            assert_eq!(api.post("/v1/search", Some(&api.alice_key), json!({"collections": collections, "text": "sentinel", "filters": {"tag": "shared"}})).await, hidden);
        }
        assert_eq!(
            api.post(
                &api.records(collection),
                Some(&api.alice_key),
                json!({"id": "same", "text": "overwrite"})
            )
            .await,
            hidden
        );
    }
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);
    for collections in [
        json!([]),
        json!([api.a, api.a]),
        json!(["*"]),
        json!((0..9).map(|i| format!("c{i}")).collect::<Vec<_>>()),
    ] {
        assert_eq!(
            api.post(
                "/v1/search",
                Some(&api.alice_key),
                json!({"collections": collections, "text": "sentinel"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        api.post(
            "/v1/search",
            Some(&api.alice_key),
            json!({"text": "sentinel"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for path in [
        "/v2/search",
        "/v2/collections/local/records",
        "/v1/records",
        "/v1/reports",
    ] {
        assert_eq!(
            api.post(path, Some(&api.alice_key), json!({"text": "sentinel"}))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    let found = api
        .post(
            "/v1/search",
            Some(&api.alice_key),
            json!({"collections": [api.a], "text": "sentinel", "filters": {"tag": "shared"}}),
        )
        .await;
    assert_eq!(found.0, StatusCode::OK);
    assert_eq!(found.1["hits"][0]["collection_id"], api.a);
    assert!(!found.1.to_string().contains("beta private"));
}

#[tokio::test]
async fn public_reads_do_not_downgrade_bad_credentials() {
    let api = Api::start().await;
    let path = format!("{}/same", api.records(&api.public));
    assert_eq!(api.get(&path, None).await.0, StatusCode::OK);
    assert_eq!(api.get(&path, Some(&api.bob_key)).await.0, StatusCode::OK);
    let root = api
        .store
        .issue_local_credential(&api.alice, &[Grant::new(&api.public, Op::Read)], None)
        .unwrap();
    api.store.revoke_local_credential(&root.info.id).unwrap();
    for key in ["invalid-secret-sentinel", &root.into_secret()] {
        let result = api.get(&path, Some(key)).await;
        assert_eq!(result.0, StatusCode::UNAUTHORIZED);
        assert!(!result.1.to_string().contains(key));
    }
    for authorization in [
        "Basic secret-sentinel",
        "Bearer",
        "Bearer bad extra",
        "Bearer ",
        "Bearer invalid, Bearer invalid",
    ] {
        let response = api
            .client
            .get(format!("{}{path}", api.url))
            .header("Authorization", authorization)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.text().await.unwrap().contains("secret-sentinel"));
    }
    let response = api
        .client
        .get(format!("{}{path}", api.url))
        .header("Authorization", format!("Bearer {}", api.bob_key))
        .header("Authorization", format!("Bearer {}", api.bob_key))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        api.post(&api.records(&api.public), None, json!({"text": "x"}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let search = api
        .post(
            "/v1/search",
            None,
            json!({"collections": [api.public], "text": "sentinel"}),
        )
        .await;
    assert_eq!(search.0, StatusCode::OK);
    assert!(search.1.get("search_id").is_none());
}

#[tokio::test]
async fn collection_discovery_and_diagnostics_follow_grants() {
    let api = Api::start().await;
    let rows = api.get("/v1/collections", Some(&api.alice_key)).await.1;
    let rows = rows["collections"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r["id"] == api.a || r["id"] == api.public));
    assert!(rows.iter().all(|r| r.get("owner_principal_id").is_none()));
    let anonymous = api.get("/v1/collections", None).await.1;
    assert_eq!(anonymous["collections"].as_array().unwrap().len(), 1);
    assert_eq!(anonymous["collections"][0]["id"], api.public);
    let first = api
        .get("/v1/collections?limit=1", Some(&api.alice_key))
        .await
        .1;
    let after = first["collections"][0]["id"].as_str().unwrap();
    let second = api
        .get(
            &format!("/v1/collections?limit=1&after={after}"),
            Some(&api.alice_key),
        )
        .await
        .1;
    assert_ne!(
        first["collections"][0]["id"],
        second["collections"][0]["id"]
    );
    let reader = api.issue(&api.alice, vec![Grant::new(&api.a, Op::Read)]);
    let path = format!("/v1/collections/{}/diagnostics", api.a);
    assert_eq!(api.get(&path, Some(&reader)).await.0, StatusCode::NOT_FOUND);
    let admin = api.issue(&api.alice, vec![Grant::new(&api.a, Op::Admin)]);
    assert_eq!(
        api.get(&format!("/v1/collections/{}", api.a), Some(&admin))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        api.get(&format!("{}/same", api.records(&api.a)), Some(&admin))
            .await
            .0,
        StatusCode::OK
    );
    let diagnostics = api.get(&path, Some(&admin)).await;
    assert_eq!(diagnostics.0, StatusCode::OK);
    assert_eq!(diagnostics.1, json!({"status": "ok", "document_count": 1}));
    assert_eq!(api.get("/healthz", None).await.1, json!({"status": "ok"}));
}

#[tokio::test]
async fn public_writes_need_only_the_grant_and_authorship_is_enforced() {
    let api = Api::start().await;
    let bob_public = api.issue(&api.bob, vec![Grant::new(&api.public, Op::Write)]);
    let records = api.records(&api.public);
    assert_eq!(
        api.post(
            &records,
            Some(&bob_public),
            json!({"id": "bob", "text": "public text", "publish": true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        api.post(
            &records,
            Some(&bob_public),
            json!({"id": "bob", "text": "public text"})
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        api.post(
            &records,
            Some(&bob_public),
            json!({"id": "same", "text": "overwrite", "expected_revision": 1})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.request(
            Method::DELETE,
            &format!("{records}/same?expected_revision=1"),
            Some(&bob_public),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn revisions_are_compared_only_when_supplied() {
    let api = Api::start().await;
    let records = api.records(&api.a);
    let unconditional = api
        .post(
            &records,
            Some(&api.alice_key),
            json!({"id": "same", "text": "update"}),
        )
        .await;
    assert_eq!(unconditional.0, StatusCode::CREATED);
    assert_eq!(unconditional.1["revision"], 2);
    assert_eq!(
        api.post(
            &records,
            Some(&api.alice_key),
            json!({"id": "same", "text": "collision", "expected_revision": 0})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        api.post(
            &records,
            Some(&api.alice_key),
            json!({"id": "missing", "text": "update", "expected_revision": 1})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (left, right) = tokio::join!(
        api.post(
            &records,
            Some(&api.alice_key),
            json!({"id": "same", "text": "first writer", "expected_revision": 2})
        ),
        api.post(
            &records,
            Some(&api.alice_key),
            json!({"id": "same", "text": "second writer", "expected_revision": 2})
        )
    );
    let mut statuses = [left.0.as_u16(), right.0.as_u16()];
    statuses.sort();
    assert_eq!(statuses, [201, 409]);
    assert_eq!(
        api.get(&format!("{records}/same"), Some(&api.alice_key))
            .await
            .1["revision"],
        3
    );
    for (query, status) in [
        ("?expected_revision=2", StatusCode::CONFLICT),
        ("?expected_revision=3", StatusCode::NO_CONTENT),
        ("?expected_revision=3", StatusCode::NO_CONTENT),
        ("", StatusCode::NO_CONTENT),
    ] {
        assert_eq!(
            api.request(
                Method::DELETE,
                &format!("{records}/same{query}"),
                Some(&api.alice_key),
                None
            )
            .await
            .0,
            status
        );
    }
    assert_eq!(
        api.get(&format!("{records}/same"), Some(&api.alice_key))
            .await
            .0,
        StatusCode::GONE
    );
    assert_eq!(
        api.get(&format!("{records}/same"), Some(&api.bob_key))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn input_limits_and_errors_do_not_reflect_payloads_or_credentials() {
    let api = Api::start().await;
    let records = api.records(&api.a);
    for body in [
        json!({"text": "secret-sentinel", "credential": api.alice_key}),
        json!({"text": "secret-sentinel", "metadata": ["secret-sentinel"]}),
        json!({"text": "secret-sentinel", "expected_revision": "secret-sentinel"}),
    ] {
        let (status, error) = api.post(&records, Some(&api.alice_key), body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!error.to_string().contains("secret-sentinel"));
        assert!(!error.to_string().contains(&api.alice_key));
    }
    let mut nested = json!("secret-sentinel");
    for _ in 0..10 {
        nested = json!([nested]);
    }
    for body in [
        json!({"text": "x", "metadata": {"nested": nested}}),
        json!({"text": "x", "metadata": {"large": "x".repeat(65_537)}}),
    ] {
        assert_eq!(
            api.post(&records, Some(&api.alice_key), body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        api.post(
            "/v1/search",
            Some(&api.alice_key),
            json!({"collections": [api.a], "text": "x".repeat(16_385)})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let error = api
        .get(
            &format!("{records}/same?revision=secret-sentinel"),
            Some(&api.alice_key),
        )
        .await;
    assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!error.1.to_string().contains("secret-sentinel"));
    assert_eq!(
        api.get(
            &format!("{records}/same?credential={}", api.alice_key),
            None
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let response = api
        .client
        .post(format!("{}{records}", api.url))
        .bearer_auth(&api.alice_key)
        .header("Content-Type", "application/json")
        .body("x".repeat(3_000_000))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "payload_too_large"
    );
    let response = api
        .client
        .get(format!("{}/healthz", api.url))
        .header("Forwarded", "for=secret-sentinel")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!api.dir.path().join("indexes").exists());
}

#[tokio::test]
async fn expiry_revocation_and_backend_failures_have_safe_responses() {
    let api = Api::start().await;
    let path = format!("{}/same", api.records(&api.public));
    let expiring = api
        .store
        .issue_local_credential(&api.alice, &[Grant::new(&api.public, Op::Read)], None)
        .unwrap();
    let connection = rusqlite::Connection::open(api.dir.path().join(DATABASE_FILE)).unwrap();
    connection
        .execute(
            "UPDATE credentials SET created_at = 0, expires_at = 1 WHERE id = ?1",
            [&expiring.info.id],
        )
        .unwrap();
    assert_eq!(
        api.get(&path, Some(&expiring.into_secret())).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Warm the index, then revoke the key: loaded state must not grant access.
    let request = json!({"collections": [api.a], "text": "sentinel"});
    assert_eq!(
        api.post("/v1/search", Some(&api.alice_key), request.clone())
            .await
            .0,
        StatusCode::OK
    );
    let context = api.store.authenticate(&api.alice_key).unwrap();
    api.store
        .revoke_local_credential(&context.credential().unwrap().id)
        .unwrap();
    assert_eq!(
        api.post("/v1/search", Some(&api.alice_key), request)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api.get(&path, Some(&api.alice_key)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(api.get("/healthz", None).await.1, json!({"status": "ok"}));
}

async fn keyed(
    api: &Api,
    method: Method,
    path: &str,
    credential: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let mut request = api
        .client
        .request(method, format!("{}{path}", api.url))
        .bearer_auth(credential)
        .header("Idempotency-Key", key);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let mutation = response
        .headers()
        .get("mutation-id")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (
        status,
        response.json().await.unwrap_or(Value::Null),
        mutation,
    )
}

#[tokio::test]
async fn concurrent_retries_return_one_mutation_and_conflicting_payloads_fail() {
    let api = Api::start().await;
    let records = api.records(&api.a);
    let body = json!({"text": "private mutation sentinel", "metadata": {"tag": "hidden"}});
    let (left, right) = tokio::join!(
        keyed(
            &api,
            Method::POST,
            &records,
            &api.alice_key,
            "create-once",
            Some(body.clone())
        ),
        keyed(
            &api,
            Method::POST,
            &records,
            &api.alice_key,
            "create-once",
            Some(body.clone())
        )
    );
    assert_eq!(left, right);
    assert_eq!(left.0, StatusCode::CREATED);
    assert_eq!(left.2.len(), 32);
    assert_eq!(left.1["revision"], 1);
    let id = left.1["id"].as_str().unwrap();
    let conflict = keyed(
        &api,
        Method::POST,
        &records,
        &api.alice_key,
        "create-once",
        Some(json!({"text": "different private sentinel"})),
    )
    .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.1["error"]["code"], "idempotency_conflict");
    assert!(!conflict.1.to_string().contains("private"));
    let update = json!({"id": id, "text": "corrected", "expected_revision": 1});
    let updated = keyed(
        &api,
        Method::POST,
        &records,
        &api.alice_key,
        "update-once",
        Some(update.clone()),
    )
    .await;
    assert_eq!(updated.0, StatusCode::CREATED);
    assert_eq!(updated.1["revision"], 2);
    assert_eq!(
        keyed(
            &api,
            Method::POST,
            &records,
            &api.alice_key,
            "update-once",
            Some(update)
        )
        .await,
        updated
    );
    let path = format!("{records}/{id}?expected_revision=2");
    let deleted = keyed(
        &api,
        Method::DELETE,
        &path,
        &api.alice_key,
        "create-once",
        None,
    )
    .await;
    assert_eq!(deleted.0, StatusCode::NO_CONTENT);
    assert_eq!(
        keyed(
            &api,
            Method::DELETE,
            &path,
            &api.alice_key,
            "create-once",
            None
        )
        .await,
        deleted
    );
    assert_eq!(
        keyed(
            &api,
            Method::POST,
            &records,
            &api.alice_key,
            "create-once",
            Some(body)
        )
        .await
        .0,
        StatusCode::GONE
    );
    let connection = rusqlite::Connection::open(api.dir.path().join(DATABASE_FILE)).unwrap();
    let journal: String = connection
        .query_row(
            "SELECT group_concat(idempotency_digest || payload_digest || result) FROM mutations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for secret in ["private mutation sentinel", "create-once", &api.alice_key] {
        assert!(!journal.contains(secret));
    }
}

#[tokio::test]
async fn idempotency_is_principal_and_collection_scoped_and_rechecks_authority() {
    let api = Api::start().await;
    let writer = api.issue(&api.bob, vec![Grant::new(&api.public, Op::Write)]);
    let records = api.records(&api.public);
    let body = json!({"text": "public contribution"});
    let created = keyed(
        &api,
        Method::POST,
        &records,
        &writer,
        "same-key",
        Some(body.clone()),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert_eq!(
        keyed(
            &api,
            Method::POST,
            &records,
            &writer,
            "same-key",
            Some(body.clone())
        )
        .await,
        created
    );
    let alice = keyed(
        &api,
        Method::POST,
        &records,
        &api.alice_key,
        "same-key",
        Some(body.clone()),
    )
    .await;
    assert_eq!(alice.0, StatusCode::CREATED);
    assert_ne!(alice.1["id"], created.1["id"]);
    let bob = keyed(
        &api,
        Method::POST,
        &api.records(&api.b),
        &api.bob_key,
        "same-key",
        Some(body.clone()),
    )
    .await;
    assert_eq!(bob.0, StatusCode::CREATED);
    assert_ne!(bob.2, created.2);
    let restricted = keyed(
        &api,
        Method::POST,
        &api.records(&api.a),
        &api.alice_key,
        "same-key",
        Some(json!({"text": "restricted copy"})),
    )
    .await;
    assert_eq!(restricted.0, StatusCode::CREATED);
    assert_ne!(restricted.2, alice.2); // same principal and operation, different collection
    let context = api.store.authenticate(&writer).unwrap();
    api.store
        .revoke_local_credential(&context.credential().unwrap().id)
        .unwrap();
    assert_eq!(
        keyed(
            &api,
            Method::POST,
            &records,
            &writer,
            "same-key",
            Some(body)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let response = api
        .client
        .post(format!("{}{records}", api.url))
        .bearer_auth(&api.alice_key)
        .header("Idempotency-Key", "one")
        .header("Idempotency-Key", "two")
        .json(&json!({"text": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        keyed(
            &api,
            Method::POST,
            &records,
            &api.alice_key,
            "not a valid key",
            Some(json!({"text": "x"}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn collection_deletion_is_authorized_and_removes_discovery_and_content() {
    let api = Api::start().await;
    let collection = format!("/v1/collections/{}", api.public);
    let hidden = api
        .request(Method::DELETE, &collection, Some(&api.bob_key), None)
        .await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);
    assert_eq!(
        hidden,
        api.request(
            Method::DELETE,
            "/v1/collections/unknown",
            Some(&api.bob_key),
            None
        )
        .await
    );
    assert_eq!(
        api.request(Method::DELETE, &collection, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api.request(Method::DELETE, &collection, Some(&api.alice_key), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(api.get(&collection, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        api.get(&format!("{collection}/records/same"), None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.post(
            "/v1/search",
            None,
            json!({"collections": [api.public], "text": "sentinel"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let listing = api.get("/v1/collections", None).await.1.to_string();
    assert!(!listing.contains(&api.public));
    assert_eq!(
        api.get(
            &format!("{}/same", api.records(&api.a)),
            Some(&api.alice_key)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        api.request(Method::DELETE, &collection, Some(&api.alice_key), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn own_records_follow_the_principal_across_key_rotation_and_revocation() {
    let api = Api::start().await;
    let records = format!("/v1/collections/{}/records", api.public);
    let grants = [Grant::new(&api.public, Op::Write)];
    let issued = api
        .store
        .issue_local_credential(&api.bob, &grants, None)
        .unwrap();
    let lookup = issued.info.id.clone();
    let bob_public = issued.into_secret();
    let (status, created) = api
        .request(
            Method::POST,
            &records,
            Some(&bob_public),
            Some(json!({"id": "bobs", "text": "bob public note"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["revision"], 1);

    let ids = |body: &Value| -> Vec<String> {
        body["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let mine = format!("{records}?author=me");
    assert_eq!(ids(&api.get(&mine, Some(&api.alice_key)).await.1), ["same"]);
    assert_eq!(ids(&api.get(&mine, Some(&bob_public)).await.1), ["bobs"]);
    assert_eq!(ids(&api.get(&records, None).await.1), ["bobs", "same"]);
    assert_eq!(api.get(&mine, None).await.0, StatusCode::BAD_REQUEST);
    let private = format!("/v1/collections/{}/records?author=me", api.a);
    assert_eq!(api.get(&private, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        api.get(&private, Some(&api.bob_key)).await.0,
        StatusCode::NOT_FOUND
    );

    let rotated = api
        .store
        .rotate_local_credential(&lookup)
        .unwrap()
        .into_secret();
    assert_eq!(
        api.get(&mine, Some(&bob_public)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(ids(&api.get(&mine, Some(&rotated)).await.1), ["bobs"]);
    for credential in api.store.local_credentials(&api.bob).unwrap() {
        api.store.revoke_local_credential(&credential.id).unwrap();
    }
    assert_eq!(
        api.get(&mine, Some(&rotated)).await.0,
        StatusCode::UNAUTHORIZED
    );
    let reissued = api
        .store
        .issue_local_credential(&api.bob, &grants, None)
        .unwrap()
        .into_secret();
    assert_eq!(ids(&api.get(&mine, Some(&reissued)).await.1), ["bobs"]);
    let (status, _) = api
        .request(
            Method::POST,
            &records,
            Some(&reissued),
            Some(json!({"id": "bobs", "text": "edited", "expected_revision": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}
