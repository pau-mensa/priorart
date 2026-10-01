use priorart::auth::{Grant, Operation, RequestContext};
use priorart::config::{ServerMode, Settings};
use priorart::service::{Service, WriteOptions, DATABASE_FILE};
use priorart::store::{Store, Visibility, LOCAL_COLLECTION_ID};
mod common;

use axum::response::IntoResponse;
use axum::{http::StatusCode, Json};
use std::sync::{Arc, Mutex};

use priorart::mcp::{Config, PriorartMcp, INSTRUCTIONS};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Session {
    _directory: TempDir,
    client: RunningService<RoleClient, ()>,
}

async fn session() -> Session {
    let directory = tempfile::tempdir().unwrap();
    let service = common::service(&directory);
    service
        .put(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "CUDA illegal address in bf16 attention; fixed by padding head dim.",
            None,
            Some("cuda"),
            WriteOptions::default(),
        )
        .unwrap();
    service
        .put(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "NCCL worker never reached the barrier: stray exit() in loader.",
            None,
            Some("nccl"),
            WriteOptions::default(),
        )
        .unwrap();
    let url = common::spawn(service).await;
    Session {
        client: attach(config(&url, None, &[LOCAL_COLLECTION_ID], None)).await,
        _directory: directory,
    }
}

fn config(url: &str, key: Option<&str>, collections: &[&str], write: Option<&str>) -> Config {
    Config {
        url: url.to_owned(),
        key: key.map(str::to_owned),
        collections: collections.iter().map(|c| c.to_string()).collect(),
        write_collection: write.map(str::to_owned),
        allow_insecure_http: false,
    }
}

async fn attach(config: Config) -> RunningService<RoleClient, ()> {
    let server = PriorartMcp::connect(config).await.unwrap();
    let (server_side, client_side) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        server
            .serve(server_side)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    ().serve(client_side).await.unwrap()
}

async fn call(session: &Session, name: &'static str, arguments: Value) -> CallToolResult {
    invoke(&session.client, name, arguments).await
}

async fn invoke(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(name).with_arguments(arguments.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

fn structured(result: &CallToolResult) -> &Value {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    result.structured_content.as_ref().unwrap()
}

fn error_text(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true));
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn tools_annotations_and_instructions() {
    let session = session().await;
    let tools = session.client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "contribute_experience",
            "delete_experience",
            "get_experience",
            "rate_hits",
            "search_experiences"
        ]
    );
    let tool = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap();
    let search = tool("search_experiences");
    assert_eq!(
        search.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    let required = search.input_schema.get("required").unwrap();
    assert!(required.as_array().unwrap().contains(&json!("problem")));
    assert!(search.output_schema.is_some());
    assert_eq!(
        tool("delete_experience")
            .annotations
            .as_ref()
            .unwrap()
            .destructive_hint,
        Some(true)
    );
    let info = session.client.peer_info().unwrap();
    assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));
    assert!(!INSTRUCTIONS.contains("report_outcome"));
}

#[tokio::test]
async fn search_then_get() {
    let session = session().await;
    let found = call(
        &session,
        "search_experiences",
        json!({"problem": "worker never reached barrier"}),
    )
    .await;
    let body = structured(&found);
    assert_eq!(body["hits"][0]["id"], "nccl");
    assert_eq!(body.as_object().unwrap().len(), 1);

    let full = call(&session, "get_experience", json!({"id": "nccl"})).await;
    assert!(structured(&full)["text"]
        .as_str()
        .unwrap()
        .starts_with("NCCL"));
}

#[tokio::test]
async fn contribute_revise_and_delete() {
    let session = session().await;
    let created = call(
        &session,
        "contribute_experience",
        json!({"text": "torch.compile recompiles: mark_dynamic fixed it", "metadata": {"lang": "python", "gpu": true}}),
    )
    .await;
    let created = structured(&created).clone();
    assert_eq!(created["revision"], 1);
    let id = created["id"].as_str().unwrap();
    let revised = call(
        &session,
        "contribute_experience",
        json!({"text": "corrected account", "expected_revision": 1, "id": id, "metadata": {"lang": "python", "gpu": true}}),
    )
    .await;
    assert_eq!(
        structured(&revised),
        &json!({"collection_id": "local", "id": id, "revision": 2, "truncated": false})
    );
    let filtered = call(
        &session,
        "search_experiences",
        json!({"problem": "corrected account", "filters": {"lang": "python", "gpu": true}}),
    )
    .await;
    assert_eq!(structured(&filtered)["hits"].as_array().unwrap().len(), 1);
    let deleted = call(
        &session,
        "delete_experience",
        json!({"id": id, "expected_revision": 2}),
    )
    .await;
    assert_eq!(
        structured(&deleted),
        &json!({"collection_id": "local", "id": id, "deleted": true})
    );
    let gone = call(&session, "get_experience", json!({"id": id})).await;
    assert!(error_text(&gone).contains("record_deleted"));
}

#[tokio::test]
async fn server_errors_surface_to_the_model() {
    let session = session().await;
    let missing = call(&session, "get_experience", json!({"id": "nope"})).await;
    assert!(error_text(&missing).contains("not_found"));
    let empty = call(&session, "contribute_experience", json!({"text": "   "})).await;
    assert!(error_text(&empty).contains("invalid_input"));
}

#[tokio::test]
async fn ids_that_would_change_the_url_are_rejected() {
    let session = session().await;
    for id in ["..", ".", "x/reports", "cuda?revision=1", "a#b"] {
        for (tool, arguments) in [
            ("get_experience", json!({"id": id})),
            (
                "delete_experience",
                json!({"id": id, "expected_revision": 1}),
            ),
        ] {
            let result = call(&session, tool, arguments).await;
            assert!(error_text(&result).contains("invalid id"), "{tool} {id}");
        }
    }
    let cuda = call(&session, "get_experience", json!({"id": "cuda"})).await;
    assert_eq!(structured(&cuda)["id"], "cuda");
}

#[tokio::test]
async fn an_unreachable_server_fails_fast() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = PriorartMcp::connect(config(&url, None, &["local"], None))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("cannot reach priorart"));
}

struct Hosted {
    _directory: TempDir,
    url: String,
    secret: String,
    private: String,
    public: String,
}

/// An authenticated server: the key's principal owns `private`; `public` belongs
/// to someone else and is readable by anyone.
async fn hosted() -> Hosted {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
    let (me, other) = (
        store.create_principal().unwrap(),
        store.create_principal().unwrap(),
    );
    let private = store
        .create_collection(&me, Visibility::Restricted)
        .unwrap();
    let public = store.create_collection(&other, Visibility::Public).unwrap();
    store
        .put(
            &private,
            "private barrier notes",
            None,
            Some("mine"),
            &me,
            None,
        )
        .unwrap();
    store
        .put(
            &public,
            "public barrier fix",
            None,
            Some("shared"),
            &other,
            None,
        )
        .unwrap();
    let secret = store
        .issue_credential(
            &me,
            &[
                Grant::new(&private, Operation::Write),
                Grant::new(&public, Operation::Read),
            ],
            None,
        )
        .unwrap()
        .into_secret();
    let service = Service::open(Settings {
        data_dir: directory.path().to_owned(),
        mode: ServerMode::Authenticated,
        ..Settings::default()
    })
    .unwrap();
    Hosted {
        url: common::spawn(Arc::new(service)).await,
        _directory: directory,
        secret,
        private,
        public,
    }
}

#[tokio::test]
async fn a_key_searches_mixed_scopes_and_writes_to_its_collection() {
    let h = hosted().await;
    let client = attach(config(
        &h.url,
        Some(&h.secret),
        &[&h.private, &h.public],
        Some(&h.private),
    ))
    .await;
    let found = invoke(&client, "search_experiences", json!({"problem": "barrier"})).await;
    let mut hits: Vec<_> = structured(&found)["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["collection_id"].as_str().unwrap().to_owned(),
                hit["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    hits.sort();
    let mut expected = vec![
        (h.private.clone(), "mine".to_owned()),
        (h.public.clone(), "shared".to_owned()),
    ];
    expected.sort();
    assert_eq!(hits, expected);
    let only_public = invoke(
        &client,
        "search_experiences",
        json!({"problem": "barrier", "collections": [h.public]}),
    )
    .await;
    assert_eq!(
        structured(&only_public)["hits"].as_array().unwrap().len(),
        1
    );
    let shared = invoke(
        &client,
        "get_experience",
        json!({"id": "shared", "collection_id": h.public}),
    )
    .await;
    assert_eq!(structured(&shared)["text"], "public barrier fix");
    let written = invoke(
        &client,
        "contribute_experience",
        json!({"text": "new note"}),
    )
    .await;
    assert_eq!(structured(&written)["collection_id"], h.private.as_str());
    let refused = invoke(
        &client,
        "contribute_experience",
        json!({"text": "not mine", "collection_id": h.public}),
    )
    .await;
    assert!(error_text(&refused).contains("not_found"));
    for (tool, arguments) in [
        (
            "get_experience",
            json!({"id": "shared", "collection": h.public}),
        ),
        (
            "delete_experience",
            json!({"id": "mine", "collection": h.private}),
        ),
        (
            "search_experiences",
            json!({"problem": "barrier", "scope": "all"}),
        ),
    ] {
        let rejected = client
            .call_tool(
                CallToolRequestParams::new(tool)
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await;
        let message = match rejected {
            Ok(result) => error_text(&result),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("unknown field"), "{tool}: {message}");
    }
    assert_eq!(
        structured(&invoke(&client, "get_experience", json!({"id": "mine"})).await)["id"],
        "mine"
    );

    let mixed = attach(config(
        &h.url,
        Some(&h.secret),
        &[&h.private, &h.public],
        None,
    ))
    .await;
    let undirected = invoke(&mixed, "contribute_experience", json!({"text": "where?"})).await;
    assert!(error_text(&undirected).contains("no default collection"));

    let anonymous = attach(config(&h.url, None, &[&h.public], None)).await;
    let public = invoke(
        &anonymous,
        "search_experiences",
        json!({"problem": "barrier"}),
    )
    .await;
    assert_eq!(structured(&public)["hits"][0]["id"], "shared");
    let private = invoke(
        &anonymous,
        "search_experiences",
        json!({"problem": "barrier", "collections": [h.private]}),
    )
    .await;
    assert!(error_text(&private).contains("not_found"));
    let write = invoke(&anonymous, "contribute_experience", json!({"text": "anon"})).await;
    assert!(error_text(&write).contains("unauthenticated"));
}

#[tokio::test]
async fn the_key_never_reaches_schemas_output_or_errors() {
    let h = hosted().await;
    let client = attach(config(&h.url, Some(&h.secret), &[&h.private], None)).await;
    let tools = client.list_all_tools().await.unwrap();
    let mut seen = serde_json::to_string(&tools).unwrap();
    let schemas = seen.to_lowercase();
    for word in ["priorart_key", "authorization", "bearer"] {
        assert!(!schemas.contains(word), "{word}");
    }
    for (name, arguments) in [
        ("search_experiences", json!({"problem": "barrier"})),
        ("get_experience", json!({"id": "mine"})),
        ("get_experience", json!({"id": "absent"})),
        ("contribute_experience", json!({"text": "   "})),
    ] {
        seen += &serde_json::to_string(&invoke(&client, name, arguments).await).unwrap();
    }
    assert!(seen.contains("barrier") && seen.contains("not_found"));
    assert!(!seen.contains(&h.secret));

    let wrong = format!("{}x", h.secret);
    let error = PriorartMcp::connect(config(&h.url, Some(&wrong), &[&h.private], None))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("rejected PRIORART_KEY"), "{error}");
    assert!(!error.contains(&wrong));
}

#[tokio::test]
async fn urls_keys_and_collections_are_checked_before_connecting() {
    for (url, key, collections) in [
        ("http://example.com", None, &["local"][..]),
        ("http://10.0.0.1:8000", Some("k"), &["local"]),
        ("ftp://127.0.0.1", None, &["local"]),
        ("https://user:pass@example.com", None, &["local"]),
        ("not a url", None, &["local"]),
        ("http://127.0.0.1:1", Some("two words"), &["local"]),
        ("http://127.0.0.1:1", None, &[]),
        ("http://127.0.0.1:1", None, &["a/b"]),
    ] {
        let error = PriorartMcp::connect(config(url, key, collections, None))
            .await
            .err()
            .unwrap();
        assert!(
            !error.to_string().contains("cannot reach"),
            "{url} {collections:?}: {error}"
        );
    }
    for (url, allow_insecure_http) in [
        ("http://localhost:1", false),
        ("http://[::1]:1", false),
        ("https://127.0.0.1:1", false),
        ("http://0.0.0.0:1", true),
    ] {
        let error = PriorartMcp::connect(Config {
            allow_insecure_http,
            ..config(url, None, &["local"], None)
        })
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("cannot reach"), "{url}: {error}");
    }
}

type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

/// Records method, bearer, and idempotency key; fails each write once with 503
/// and redirects `/moved` elsewhere.
async fn flaky(elsewhere: Option<String>) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let log = log.clone();
        let elsewhere = elsewhere.clone();
        async move {
            let header = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .map(|value| value.to_str().unwrap().to_owned())
            };
            let entry = (
                format!("{} {}", request.method(), request.uri().path()),
                header("authorization"),
                header("idempotency-key"),
            );
            let mut log = log.lock().unwrap();
            let retry = log.iter().any(|seen| seen.0 == entry.0);
            log.push(entry);
            let path = request.uri().path().to_owned();
            if let (Some(target), true) = (&elsewhere, path.contains("moved")) {
                return (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", format!("{target}{path}"))],
                    String::new(),
                )
                    .into_response();
            }
            match (request.method().as_str(), retry) {
                ("GET", _) if path == "/healthz" => Json(json!({"status": "ok"})).into_response(),
                ("POST" | "DELETE", false) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                ("POST", true) => (
                    StatusCode::CREATED,
                    Json(
                        json!({"collection_id": "c", "id": "r", "revision": 1, "truncated": false}),
                    ),
                )
                    .into_response(),
                _ => StatusCode::NO_CONTENT.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn retries_reuse_one_idempotency_key_per_write() {
    let (url, seen) = flaky(None).await;
    let client = attach(config(&url, Some("pa1_key"), &["c"], None)).await;
    let written = invoke(&client, "contribute_experience", json!({"text": "note"})).await;
    assert_eq!(structured(&written)["id"], "r");
    let deleted = invoke(&client, "delete_experience", json!({"id": "r"})).await;
    assert_eq!(
        structured(&deleted),
        &json!({"collection_id": "c", "id": "r", "deleted": true})
    );
    invoke(&client, "contribute_experience", json!({"text": "again"})).await;
    let seen = seen.lock().unwrap().clone();
    let keys = |route: &str| -> Vec<String> {
        seen.iter()
            .filter(|entry| entry.0 == route)
            .map(|entry| entry.2.clone().unwrap())
            .collect()
    };
    let posts = keys("POST /v1/collections/c/records");
    assert_eq!(posts.len(), 3);
    assert_eq!(posts[0], posts[1]);
    assert_ne!(posts[1], posts[2]);
    let deletes = keys("DELETE /v1/collections/c/records/r");
    assert_eq!(deletes.len(), 2);
    assert_eq!(deletes[0], deletes[1]);
    assert_ne!(deletes[0], posts[0]);
    assert!(seen
        .iter()
        .all(|entry| entry.1.as_deref() == Some("Bearer pa1_key")));
    assert!(seen
        .iter()
        .filter(|entry| entry.0.starts_with("GET"))
        .all(|entry| entry.2.is_none()));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (elsewhere, reached) = flaky(None).await;
    let (url, _) = flaky(Some(elsewhere)).await;
    let client = attach(config(&url, Some("pa1_key"), &["c"], None)).await;
    let result = invoke(&client, "get_experience", json!({"id": "moved"})).await;
    assert!(error_text(&result).contains("HTTP 307"));
    assert!(reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn searches_carry_an_id_to_rate_when_the_server_logs_them() {
    let session = session().await;
    let found = call(
        &session,
        "search_experiences",
        json!({"problem": "barrier"}),
    )
    .await;
    assert!(structured(&found).get("search_id").is_none());

    let directory = tempfile::tempdir().unwrap();
    let service = Arc::new(
        Service::open(Settings {
            data_dir: directory.path().to_owned(),
            search_log_days: Some(7),
            ..Settings::default()
        })
        .unwrap(),
    );
    service
        .put(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "NCCL worker never reached the barrier: stray exit() in loader.",
            None,
            Some("nccl"),
            WriteOptions::default(),
        )
        .unwrap();
    let client = attach(config(
        &common::spawn(service.clone()).await,
        None,
        &[LOCAL_COLLECTION_ID],
        None,
    ))
    .await;
    let found = invoke(&client, "search_experiences", json!({"problem": "barrier"})).await;
    let search_id = structured(&found)["search_id"].as_str().unwrap().to_owned();
    let rated = invoke(
        &client,
        "rate_hits",
        json!({"search_id": search_id, "ratings": [{"collection_id": "local", "id": "nccl", "useful": true}]}),
    )
    .await;
    assert_eq!(
        structured(&rated),
        &json!({"search_id": search_id, "rated": 1})
    );
    let log = service.search_log().unwrap();
    for _ in 0..200 {
        let (rows, _) = log.export(0, 10).unwrap();
        if rows
            .first()
            .is_some_and(|row| row["feedback"][0]["useful"] == json!(true))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the rating was never logged");
}
