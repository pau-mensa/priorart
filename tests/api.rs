mod common;

use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Api {
    _directory: TempDir,
    url: String,
    client: Client,
}

impl Api {
    async fn start() -> Self {
        Self::with_max_tokens(priorart::config::Settings::default().max_tokens).await
    }

    async fn with_max_tokens(max_tokens: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let settings = priorart::config::Settings {
            data_dir: directory.path().to_path_buf(),
            max_tokens,
            ..priorart::config::Settings::default()
        };
        let service = priorart::service::Service::open(settings).unwrap();
        let url = common::spawn(std::sync::Arc::new(service)).await;
        Self {
            _directory: directory,
            url,
            client: Client::new(),
        }
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let response = self
            .client
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap();
        (
            response.status(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.url))
            .json(&body)
            .send()
            .await
            .unwrap();
        (
            response.status(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn delete(&self, path: &str) -> StatusCode {
        self.client
            .delete(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap()
            .status()
    }
}

#[tokio::test]
async fn record_lifecycle() {
    let api = Api::start().await;
    let (status, created) = api
        .post(
            "/v1/collections/local/records",
            json!({"text": "NCCL barrier timeout fixed", "metadata": {"k": "v"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["revision"], 1);
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, got) = api
        .get(&format!("/v1/collections/local/records/{id}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["text"], "NCCL barrier timeout fixed");
    assert_eq!(got["metadata"], json!({"k": "v"}));
    assert!(got["created_at"].as_str().unwrap().ends_with('Z'));

    let (_, updated) = api
        .post(
            "/v1/collections/local/records",
            json!({"text": "second", "id": id, "expected_revision": 1}),
        )
        .await;
    assert_eq!(
        updated,
        json!({"collection_id": "local", "id": id, "revision": 2, "truncated": false})
    );
    assert_eq!(
        api.get(&format!("/v1/collections/local/records/{id}?revision=1"))
            .await
            .1["revision"],
        1
    );
    assert_eq!(
        api.get(&format!("/v1/collections/local/records/{id}?revision=9"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        api.delete(&format!(
            "/v1/collections/local/records/{id}?expected_revision=2"
        ))
        .await,
        StatusCode::NO_CONTENT
    );
    let (status, gone) = api
        .get(&format!("/v1/collections/local/records/{id}"))
        .await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(gone["error"]["code"], "record_deleted");
    assert_eq!(
        api.delete(&format!(
            "/v1/collections/local/records/{id}?expected_revision=2"
        ))
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.delete("/v1/collections/local/records/missing").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn search_returns_qualified_hits_without_receipts() {
    let api = Api::start().await;
    api.post(
        "/v1/collections/local/records",
        json!({"text": "CUDA illegal address in bf16 attention", "id": "a"}),
    )
    .await;
    api.post(
        "/v1/collections/local/records",
        json!({"text": "NCCL worker never reached the barrier", "id": "b"}),
    )
    .await;
    let (status, found) = api
        .post(
            "/v1/search",
            json!({"collections": ["local"], "text": "worker barrier", "limit": 2}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["hits"][0]["id"], "b");
    assert_eq!(found["collections"], json!(["local"]));
    assert!(found.get("gatherer").is_none());
    let mut keys: Vec<&str> = found["hits"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "collection_id",
            "excerpt",
            "id",
            "metadata",
            "revision",
            "score"
        ]
    );
    let mut top: Vec<&str> = found
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    top.sort();
    assert_eq!(top, ["collections", "hits"]);
}

#[tokio::test]
async fn error_envelopes() {
    let api = Api::start().await;
    let (status, empty) = api
        .post("/v1/collections/local/records", json!({"text": "   "}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(empty["error"]["code"], "invalid_input");
    let (status, malformed) = api
        .post("/v1/collections/local/records", json!({"metadata": {}}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(malformed["error"]["code"], "validation_error");
    assert!(malformed["error"]["message"]
        .as_str()
        .unwrap()
        .contains("schema"));
    let (status, _) = api
        .post(
            "/v1/collections/local/records",
            json!({"text": "x", "metadata": [1]}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        api.get("/v1/collections/local/records/nope").await.1["error"]["code"],
        "not_found"
    );
    assert_eq!(
        api.get("/v1/collections/local/records/nope/reports")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.get("/v1/collections/local/records/nope?revision=x")
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, _) = api
        .post(
            "/v1/search",
            json!({"collections": ["local"], "text": "x", "limit": 0}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn healthz() {
    let api = Api::start().await;
    let (status, health) = api.get("/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health, json!({"status": "ok"}));
}

#[tokio::test]
async fn text_beyond_the_token_cutoff_is_truncated() {
    let api = Api::with_max_tokens(4).await;
    let (status, body) = api
        .post(
            "/v1/collections/local/records",
            json!({"id": "long", "text": "one two three four five six"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["truncated"], true);
    let (_, record) = api.get("/v1/collections/local/records/long").await;
    assert_eq!(record["text"], "one two three four");

    let (status, body) = api
        .post(
            "/v1/collections/local/records",
            json!({"id": "long", "text": "one two three", "expected_revision": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["truncated"], false);

    let limit = priorart::config::Settings {
        max_tokens: 4,
        ..Default::default()
    }
    .max_body_bytes();
    let (status, _) = api
        .post(
            "/v1/collections/local/records",
            json!({"text": "x".repeat(limit)}),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn records_are_listed_in_id_order_without_tombstones() {
    let api = Api::with_max_tokens(4).await;
    let records = "/v1/collections/local/records";
    for (id, text) in [
        ("c", "three"),
        ("a", "one two three four five"),
        ("b", "two"),
    ] {
        let (status, _) = api
            .post(
                records,
                json!({"id": id, "text": text, "metadata": {"k": id}}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    assert_eq!(
        api.delete(&format!("{records}/b?expected_revision=1"))
            .await,
        StatusCode::NO_CONTENT
    );

    let (status, page) = api.get(&format!("{records}?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    let first = &page["records"][0];
    assert_eq!(page["records"].as_array().unwrap().len(), 1);
    assert_eq!(first["id"], "a");
    assert_eq!(first["truncated"], true);
    assert_eq!(first["metadata"], json!({"k": "a"}));
    assert_eq!(first["created_at"], first["updated_at"]);
    assert!(first.get("text").is_none());
    let (_, page) = api
        .get(&format!("{records}?limit=1&after=a&author=me&include=text"))
        .await;
    assert_eq!(page["records"][0]["id"], "c");
    assert_eq!(page["records"][0]["text"], "three");
    assert_eq!(page["records"][0]["truncated"], false);
    let (_, page) = api.get(&format!("{records}?after=c")).await;
    assert_eq!(page["records"], json!([]));
    let (_, record) = api.get(&format!("{records}/a")).await;
    assert_eq!(record["truncated"], true);

    for query in [
        "author=you",
        "include=metadata",
        "limit=0",
        "limit=101",
        "after=%20",
    ] {
        assert_eq!(
            api.get(&format!("{records}?{query}")).await.0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
}

#[tokio::test]
async fn credential_administration_has_no_remote_endpoint() {
    let api = Api::start().await;
    for path in [
        "/v1/credentials",
        "/v1/admin/issue",
        "/admin/issue",
        "/v1/bootstrap",
    ] {
        let (status, _) = api
            .post(
                path,
                json!({"principal": "local-principal", "grants": ["local:admin"]}),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let store =
        priorart::store::Store::open(api._directory.path().join(priorart::service::DATABASE_FILE))
            .unwrap();
    assert!(store.credentials("local-principal").unwrap().is_empty());
}

#[tokio::test]
async fn local_authority_never_overrides_a_supplied_bad_key_or_leaves_local_scope() {
    let api = Api::start().await;
    let store =
        priorart::store::Store::open(api._directory.path().join(priorart::service::DATABASE_FILE))
            .unwrap();
    let other = store
        .create_collection(
            priorart::store::LOCAL_PRINCIPAL_ID,
            priorart::store::Visibility::Public,
        )
        .unwrap();
    store
        .put(
            &other,
            "public sentinel",
            None,
            Some("record"),
            priorart::store::LOCAL_PRINCIPAL_ID,
            None,
        )
        .unwrap();
    assert_eq!(
        api.get(&format!("/v1/collections/{other}/records/record"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let response = api
        .client
        .post(format!("{}/v1/collections/local/records", api.url))
        .bearer_auth("invalid-secret-sentinel")
        .json(&json!({"text": "must not persist"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!response
        .text()
        .await
        .unwrap()
        .contains("invalid-secret-sentinel"));
    assert!(store.live_documents("local").unwrap().is_empty());
    assert_eq!(api.get("/v1/records/record").await.0, StatusCode::NOT_FOUND);
}
