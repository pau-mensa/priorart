mod common;

use priorart::auth::{Grant, Operation};
use priorart::config::{AdminToken, ServerMode, Settings};
use priorart::service::{Service, DATABASE_FILE};
use priorart::store::Store;
use reqwest::{Client, StatusCode};
use serde_json::json;
use std::sync::Arc;

const TOKEN: &str = "operator-token-0123456789abcdef-0123456789";

async fn start(mode: ServerMode, token: Option<&str>) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::open(Settings {
        data_dir: dir.path().to_owned(),
        mode,
        admin_token: token.map(|token| AdminToken::new(token).unwrap()),
        ..Settings::default()
    })
    .unwrap();
    let url = common::spawn(Arc::new(service)).await;
    (dir, url)
}

async fn put_and_search(client: &Client, url: &str) {
    let put = client
        .post(format!("{url}/v1/collections/local/records"))
        .json(&json!({"text": "retry the flaky migration", "id": "r1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);
    for text in ["flaky migration", "retry"] {
        let search = client
            .post(format!("{url}/v1/search"))
            .json(&json!({"collections": ["local"], "text": text}))
            .send()
            .await
            .unwrap();
        assert_eq!(search.status(), StatusCode::OK);
    }
    let missing = client
        .post(format!("{url}/v1/search"))
        .json(&json!({"collections": ["missing"], "text": "retry"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

async fn scrape(client: &Client, url: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    if status == StatusCode::OK && url.ends_with("metrics") {
        assert_eq!(
            response.headers()["content-type"],
            priorart::metrics::CONTENT_TYPE
        );
    }
    (status, response.text().await.unwrap())
}

#[tokio::test]
async fn public_metrics_show_only_server_wide_search_latency() {
    let (_dir, url) = start(ServerMode::Local, None).await;
    let client = Client::new();
    let (_, idle) = scrape(&client, &format!("{url}/metrics"), None).await;
    assert!(idle.contains("priorart_search_duration_recent_seconds{quantile=\"0.99\"} NaN"));

    put_and_search(&client, &url).await;
    let (status, body) = scrape(&client, &format!("{url}/metrics"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("priorart_search_duration_seconds_count 2\n"));
    assert!(body.contains("priorart_search_duration_seconds_bucket{le=\"+Inf\"} 2\n"));
    assert!(body.contains("priorart_searches_total{outcome=\"ok\"} 2\n"));
    assert!(body.contains("priorart_searches_total{outcome=\"client_error\"} 1\n"));
    for quantile in ["0.5", "0.95", "0.99"] {
        let line = body
            .lines()
            .find(|line| {
                line.starts_with(&format!(
                    "priorart_search_duration_recent_seconds{{quantile=\"{quantile}\"}}"
                ))
            })
            .unwrap();
        let value: f64 = line.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(value.is_finite() && value > 0.0);
    }
    for leak in ["local", "missing", "route=", "priorart_http_requests_total"] {
        assert!(!body.contains(leak), "public metrics mention {leak}");
    }
}

#[tokio::test]
async fn public_metrics_are_anonymous_in_authenticated_mode() {
    let (_dir, url) = start(ServerMode::Authenticated, None).await;
    let client = Client::new();
    let (status, _) = scrape(&client, &format!("{url}/metrics"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = scrape(&client, &format!("{url}/metrics"), Some("pa1_bogus")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn operator_metrics_need_the_admin_token() {
    let (_dir, url) = start(ServerMode::Local, None).await;
    let client = Client::new();
    let (status, _) = scrape(&client, &format!("{url}/v1/admin/metrics"), Some(TOKEN)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_dir, url) = start(ServerMode::Local, Some(TOKEN)).await;
    let metrics = format!("{url}/v1/admin/metrics");
    assert_eq!(
        scrape(&client, &metrics, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        scrape(
            &client,
            &metrics,
            Some("wrong-token-0123456789abcdef-0123456789")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn operator_metrics_break_down_routes_and_collections() {
    let (_dir, url) = start(ServerMode::Local, Some(TOKEN)).await;
    let client = Client::new();
    put_and_search(&client, &url).await;
    let delete = client
        .delete(format!("{url}/v1/collections/local/records/r1"))
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    client.get(format!("{url}/nowhere")).send().await.unwrap();

    let (status, body) = scrape(&client, &format!("{url}/v1/admin/metrics"), Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    for line in [
        "priorart_search_duration_seconds_count 2",
        "priorart_http_requests_total{route=\"/v1/search\",method=\"POST\",status=\"200\"} 2",
        "priorart_http_requests_total{route=\"/v1/search\",method=\"POST\",status=\"404\"} 1",
        "priorart_http_requests_total{route=\"/v1/collections/{collection}/records\",method=\"POST\",status=\"201\"} 1",
        "priorart_http_requests_total{route=\"unmatched\",method=\"GET\",status=\"404\"} 1",
        "priorart_http_request_duration_seconds_count{route=\"/v1/search\"} 3",
        "priorart_collection_search_duration_seconds_count{collection=\"local\"} 2",
        "priorart_collection_writes_total{collection=\"local\",operation=\"put\"} 1",
        "priorart_collection_writes_total{collection=\"local\",operation=\"delete\"} 1",
        "priorart_indexed_documents{collection=\"local\"} 0",
        "priorart_index_load_duration_seconds_count 1",
        "priorart_cached_collections 1",
        "priorart_index_evictions_total 0",
        "priorart_resource_limited_total 0",
        "priorart_rate_limited_total 0",
    ] {
        assert!(body.contains(&format!("{line}\n")), "missing {line}\n{body}");
    }
    assert!(!body.contains(TOKEN));
}

#[tokio::test]
async fn rate_limited_keys_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let key = Store::open(dir.path().join(DATABASE_FILE))
        .unwrap()
        .issue_credential(
            "local-principal",
            &[Grant::new("local", Operation::Read)],
            None,
        )
        .unwrap()
        .into_secret();
    let service = Service::open(Settings {
        data_dir: dir.path().to_owned(),
        admin_token: Some(AdminToken::new(TOKEN).unwrap()),
        key_requests_per_minute: Some(1),
        ..Settings::default()
    })
    .unwrap();
    let url = common::spawn(Arc::new(service)).await;
    let client = Client::new();
    let records = format!("{url}/v1/collections/local/records");
    assert_eq!(
        scrape(&client, &records, Some(&key)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        scrape(&client, &records, Some(&key)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let (_, body) = scrape(&client, &format!("{url}/v1/admin/metrics"), Some(TOKEN)).await;
    assert!(body.contains("priorart_rate_limited_total 1\n"));
}
