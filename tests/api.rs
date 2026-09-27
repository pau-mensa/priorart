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
        Self::with_max_text_bytes(priorart::config::Settings::default().max_text_bytes).await
    }

    async fn with_max_text_bytes(max_text_bytes: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let settings = priorart::config::Settings {
            data_dir: directory.path().to_path_buf(),
            max_text_bytes,
            ..priorart::config::Settings::default()
        };
        let encoder: std::sync::Arc<dyn priorart::encoder::Encoder> =
            std::sync::Arc::new(common::FakeEncoder::new());
        let service = priorart::service::Service::new(settings, Some(encoder)).unwrap();
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
            "/v1/records",
            json!({"text": "NCCL barrier timeout fixed", "metadata": {"k": "v"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["revision"], 1);
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, got) = api.get(&format!("/v1/records/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["text"], "NCCL barrier timeout fixed");
    assert_eq!(got["metadata"], json!({"k": "v"}));
    assert!(got["created_at"].as_str().unwrap().ends_with('Z'));

    let (_, updated) = api
        .post("/v1/records", json!({"text": "second", "id": id}))
        .await;
    assert_eq!(updated, json!({"id": id, "revision": 2}));
    assert_eq!(
        api.get(&format!("/v1/records/{id}?revision=1")).await.1["revision"],
        1
    );
    assert_eq!(
        api.get(&format!("/v1/records/{id}?revision=9")).await.0,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        api.delete(&format!("/v1/records/{id}")).await,
        StatusCode::NO_CONTENT
    );
    let (status, gone) = api.get(&format!("/v1/records/{id}")).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(gone["error"]["code"], "record_deleted");
    assert_eq!(
        api.delete(&format!("/v1/records/{id}")).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.delete("/v1/records/missing").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn reports_reject_a_nonexistent_revision() {
    let api = Api::start().await;
    api.post(
        "/v1/records",
        json!({"id": "record", "text": "local content"}),
    )
    .await;
    let (status, body) = api
        .post(
            "/v1/reports",
            json!({"record_id": "record", "revision": 99, "text": "feedback"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "record_not_found");
    assert_eq!(
        api.get("/v1/records/record/reports").await.1,
        json!({"reports": []})
    );
}

#[tokio::test]
async fn search_and_report() {
    let api = Api::start().await;
    api.post(
        "/v1/records",
        json!({"text": "CUDA illegal address in bf16 attention", "id": "a"}),
    )
    .await;
    api.post(
        "/v1/records",
        json!({"text": "NCCL worker never reached the barrier", "id": "b"}),
    )
    .await;
    let (status, found) = api
        .post("/v1/search", json!({"text": "worker barrier", "limit": 2}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found["hits"][0]["id"], "b");
    assert_eq!(found["gatherer"], "exhaustive");
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
            "excerpt",
            "id",
            "metadata",
            "revision",
            "score",
            "score_semantics"
        ]
    );
    let search_id = found["search_id"].as_str().unwrap();

    let (status, reported) = api
        .post(
            "/v1/reports",
            json!({"record_id": "b", "text": "worked", "revision": 1, "search_id": search_id}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let listed = api.get("/v1/records/b/reports").await.1;
    assert_eq!(listed["reports"][0]["id"], reported["id"]);
    assert_eq!(listed["reports"][0]["search_id"], search_id);

    let (status, bad) = api
        .post(
            "/v1/reports",
            json!({"record_id": "b", "text": "x", "search_id": "nope"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(bad["error"]["code"], "search_not_found");
}

#[tokio::test]
async fn error_envelopes() {
    let api = Api::start().await;
    let (status, empty) = api.post("/v1/records", json!({"text": "   "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(empty["error"]["code"], "invalid_input");
    let (status, malformed) = api.post("/v1/records", json!({"metadata": {}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(malformed["error"]["code"], "validation_error");
    assert!(malformed["error"]["message"]
        .as_str()
        .unwrap()
        .contains("text"));
    let (status, _) = api
        .post("/v1/records", json!({"text": "x", "metadata": [1]}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        api.get("/v1/records/nope").await.1["error"]["code"],
        "record_not_found"
    );
    assert_eq!(
        api.get("/v1/records/nope/reports").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.get("/v1/records/nope?revision=x").await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, _) = api
        .post("/v1/search", json!({"text": "x", "limit": 0}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn healthz() {
    let api = Api::start().await;
    let (status, health) = api.get("/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        health,
        json!({"status": "ok", "document_count": 0, "encoder": "fake", "gather_limit": 500})
    );
}

#[tokio::test]
async fn the_configured_text_limit_governs_request_size() {
    let api = Api::with_max_text_bytes(3_000_000).await;
    let (status, _) = api
        .post("/v1/records", json!({"text": "word ".repeat(560_000)}))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = api
        .post("/v1/records", json!({"text": "x".repeat(3_000_001)}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("the limit is 3000000"), "{body}");
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
    assert!(store
        .local_credentials("local-principal")
        .unwrap()
        .is_empty());
}
