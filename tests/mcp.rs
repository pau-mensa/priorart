use priorart::auth::RequestContext;
use priorart::service::WriteOptions;
use priorart::store::LOCAL_COLLECTION_ID;
mod common;

use priorart::mcp::{PriorartMcp, INSTRUCTIONS};
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
    let server = PriorartMcp::connect(Some(url)).await.unwrap();
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
    Session {
        _directory: directory,
        client: ().serve(client_side).await.unwrap(),
    }
}

async fn call(session: &Session, name: &'static str, arguments: Value) -> CallToolResult {
    session
        .client
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
    assert_eq!(structured(&revised), &json!({"id": id, "revision": 2}));
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
    assert_eq!(structured(&deleted), &json!({"id": id, "deleted": true}));
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
        for tool in ["get_experience", "delete_experience"] {
            let result = call(&session, tool, json!({"id": id, "expected_revision": 1})).await;
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
    let error = PriorartMcp::connect(Some(url)).await.err().unwrap();
    assert!(error.to_string().contains("cannot reach priorart"));
}
