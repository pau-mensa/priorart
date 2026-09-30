mod common;

use priorart::auth::{Grant, Operation, RequestContext};
use priorart::config::{AdminToken, ServerMode, Settings};
use priorart::service::{Service, WriteOptions, DATABASE_FILE};
use priorart::store::{Store, Visibility};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

const TOKEN: &str = "operator-token-0123456789abcdef-0123456789";

struct Server {
    dir: TempDir,
    client: Client,
    url: String,
    collection: String,
    principals: [String; 2],
    keys: [String; 2],
}

impl Server {
    /// A public collection holding two records, and read keys for two principals.
    async fn start(search_log_days: Option<u32>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
        let principals = [(); 2].map(|_| store.create_principal().unwrap());
        let collection = store
            .create_collection(&principals[0], Visibility::Public)
            .unwrap();
        let keys = principals.clone().map(|principal| {
            let operation = if principal == principals[0] {
                Operation::Admin
            } else {
                Operation::Read
            };
            store
                .issue_credential(&principal, &[Grant::new(&collection, operation)], None)
                .unwrap()
                .into_secret()
        });
        let service = Service::open(Self::settings(&dir, search_log_days)).unwrap();
        let owner = service.authenticate(&keys[0]).unwrap();
        for (id, text) in [
            ("nccl", "NCCL worker never reached the barrier"),
            ("cuda", "CUDA illegal address at the barrier kernel"),
        ] {
            service
                .put(
                    &owner,
                    &collection,
                    text,
                    None,
                    Some(id),
                    WriteOptions::default(),
                )
                .unwrap();
        }
        let url = common::spawn(Arc::new(service)).await;
        Self {
            dir,
            client: Client::new(),
            url,
            collection,
            principals,
            keys,
        }
    }

    fn settings(dir: &TempDir, search_log_days: Option<u32>) -> Settings {
        Settings {
            data_dir: dir.path().to_owned(),
            mode: ServerMode::Authenticated,
            admin_token: Some(AdminToken::new(TOKEN).unwrap()),
            search_log_days,
            ..Settings::default()
        }
    }

    async fn post(&self, path: &str, key: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut request = self.client.post(format!("{}{path}", self.url)).json(&body);
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.unwrap();
        (
            response.status(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn search(&self, key: Option<&str>, text: &str) -> Value {
        let (status, body) = self
            .post(
                "/v1/search",
                key,
                json!({"collections": [self.collection], "text": text, "filters": {}, "limit": 5}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    async fn rate(&self, key: Option<&str>, search_id: &str, ratings: Value) -> StatusCode {
        self.post(
            &format!("/v1/search/{search_id}/feedback"),
            key,
            json!({ "ratings": ratings }),
        )
        .await
        .0
    }

    fn rating(&self, id: &str, useful: bool) -> Value {
        json!({"collection_id": self.collection, "id": id, "useful": useful})
    }

    async fn export(&self, query: &str) -> (StatusCode, Vec<Value>) {
        let response = self
            .client
            .get(format!("{}/v1/admin/search-log{query}", self.url))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let lines = response
            .text()
            .await
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or(Value::Null))
            .collect();
        (status, lines)
    }

    /// Exports until `done` holds; the log is written in the background.
    async fn export_when(&self, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        for _ in 0..200 {
            let (status, lines) = self.export("").await;
            assert_eq!(status, StatusCode::OK);
            if done(&lines) {
                return lines;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the search log never reached the expected state");
    }

    async fn dropped(&self, reason: &str) -> u64 {
        let body = self
            .client
            .get(format!("{}/v1/admin/metrics", self.url))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let prefix = format!("priorart_search_log_dropped_total{{reason=\"{reason}\"}} ");
        body.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn dropped_when(&self, reason: &str, count: u64) {
        for _ in 0..200 {
            if self.dropped(reason).await == count {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{reason} never reached {count}");
    }
}

#[tokio::test]
async fn without_the_setting_nothing_is_logged() {
    let server = Server::start(None).await;
    let found = server.search(Some(&server.keys[0]), "barrier").await;
    assert!(found.get("search_id").is_none());
    assert_eq!(
        server
            .rate(
                Some(&server.keys[0]),
                "0123456789abcdef",
                json!([server.rating("nccl", true)])
            )
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(server.export("").await.0, StatusCode::NOT_FOUND);
    assert!(!server.dir.path().join(priorart::searchlog::FILE).exists());
}

#[tokio::test]
async fn searchers_rate_the_hits_they_were_shown() {
    let server = Server::start(Some(30)).await;
    let key = Some(server.keys[0].as_str());
    let found = server.search(key, "barrier").await;
    let search_id = found["search_id"].as_str().unwrap();
    let hit_ids: Vec<&str> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_str().unwrap())
        .collect();
    assert_eq!(hit_ids.len(), 2);
    let anonymous = server.search(None, "kernel").await;
    assert!(anonymous["search_id"].is_string());

    let rate = |key, ratings| server.rate(key, search_id, ratings);
    for ratings in [
        json!([server.rating(hit_ids[0], false)]),
        json!([
            server.rating(hit_ids[0], true),
            server.rating(hit_ids[1], false)
        ]),
    ] {
        assert_eq!(rate(key, ratings).await, StatusCode::ACCEPTED);
    }
    assert_eq!(rate(key, json!([])).await, StatusCode::BAD_REQUEST);
    assert_eq!(
        rate(None, json!([server.rating(hit_ids[0], true)])).await,
        StatusCode::UNAUTHORIZED
    );
    // Accepted, then discarded by the writer: not a hit, not the searcher,
    // and no such search.
    let discarded = [
        rate(key, json!([server.rating("elsewhere", true)])).await,
        rate(
            Some(&server.keys[1]),
            json!([server.rating(hit_ids[0], false)]),
        )
        .await,
        server
            .rate(
                key,
                "ffffffffffffffff",
                json!([server.rating("nccl", false)]),
            )
            .await,
    ];
    assert_eq!(discarded, [StatusCode::ACCEPTED; 3]);
    server.dropped_when("rejected_feedback", 3).await;

    let lines = server.export_when(|lines| lines.len() == 3).await;
    let logged = &lines[0];
    assert_eq!(logged["id"], json!(search_id));
    assert_eq!(logged["query"], json!("barrier"));
    assert_eq!(logged["collections"], json!([server.collection]));
    assert_eq!(logged["filters"], json!({}));
    assert_eq!(logged["limit"], json!(5));
    assert_eq!(logged["hits"].as_array().unwrap().len(), 2);
    assert_eq!(logged["hits"][0]["id"], json!(hit_ids[0]));
    assert_eq!(logged["hits"][0]["revision"], json!(1));
    let mut feedback: Vec<(String, bool)> = logged["feedback"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["id"].as_str().unwrap().to_owned(),
                f["useful"].as_bool().unwrap(),
            )
        })
        .collect();
    feedback.sort();
    let mut expected = vec![
        (hit_ids[0].to_owned(), true),
        (hit_ids[1].to_owned(), false),
    ];
    expected.sort();
    assert_eq!(feedback, expected);
    let principal = logged["principal"].as_str().unwrap();
    assert_eq!(principal.len(), 32);
    assert_ne!(principal, server.principals[0]);
    assert_eq!(lines[1]["principal"], Value::Null);
    assert_eq!(lines[1]["feedback"], json!([]));
    assert_eq!(
        lines[2],
        json!({"type": "end", "count": 2, "next_cursor": null})
    );
    let text = serde_json::to_string(&lines).unwrap();
    assert!(
        !text.contains("NCCL worker"),
        "the log stores no record text"
    );
    assert!(!text.contains(&server.principals[0]));
    assert_eq!(server.dropped("queue_full").await, 0);
    assert_eq!(server.dropped("write_failed").await, 0);
}

#[tokio::test]
async fn the_export_pages_and_deleted_collections_are_forgotten() {
    let server = Server::start(Some(30)).await;
    let key = Some(server.keys[0].as_str());
    for text in ["barrier", "kernel", "worker"] {
        server.search(key, text).await;
    }
    server.export_when(|lines| lines.len() == 4).await;
    let (_, first) = server.export("?limit=2").await;
    assert_eq!(first.len(), 3);
    let next = first[2]["next_cursor"].as_i64().unwrap();
    let (_, rest) = server.export(&format!("?limit=2&after={next}")).await;
    assert_eq!(rest[0]["query"], json!("worker"));
    assert_eq!(
        rest[1],
        json!({"type": "end", "count": 1, "next_cursor": null})
    );
    assert_eq!(server.export("?limit=0").await.0, StatusCode::BAD_REQUEST);

    let deleted = server
        .client
        .delete(format!(
            "{}/v1/collections/{}",
            server.url, server.collection
        ))
        .bearer_auth(&server.keys[0])
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    server.export_when(|lines| lines.len() == 1).await;
}

#[tokio::test]
async fn log_failures_never_reach_searches() {
    let server = Server::start(Some(30)).await;
    rusqlite::Connection::open(server.dir.path().join(priorart::searchlog::FILE))
        .unwrap()
        .execute_batch("DROP TABLE feedback; DROP TABLE hits; DROP TABLE searches;")
        .unwrap();
    for _ in 0..2 {
        let found = server.search(Some(&server.keys[0]), "barrier").await;
        assert_eq!(found["hits"].as_array().unwrap().len(), 2);
    }
    server.dropped_when("write_failed", 2).await;
}

#[test]
fn restarts_keep_pseudonyms_and_prune_expired_searches() {
    let dir = tempfile::tempdir().unwrap();
    let settings = || Settings {
        data_dir: dir.path().to_owned(),
        search_log_days: Some(1),
        ..Settings::default()
    };
    let context = RequestContext::local();
    let search = |service: &Service| {
        service
            .put(
                &context,
                "local",
                "flaky barrier",
                None,
                Some("r1"),
                WriteOptions::default(),
            )
            .unwrap();
        let hits = service
            .search(&context, &["local"], "barrier", None, 5)
            .unwrap();
        service
            .log_search(&context, &["local"], "barrier", None, 5, &hits)
            .unwrap()
    };
    // Dropping a service waits for its log to be written.
    let (old, recent) = {
        let service = Service::open(settings()).unwrap();
        (search(&service), search(&service))
    };
    rusqlite::Connection::open(dir.path().join(priorart::searchlog::FILE))
        .unwrap()
        .execute(
            "UPDATE searches SET at = '2000-01-01T00:00:00.000000Z' WHERE id = ?1",
            [&old],
        )
        .unwrap();

    let rating = priorart::searchlog::Rating {
        collection_id: "local".into(),
        id: "r1".into(),
        useful: true,
    };
    {
        let service = Service::open(settings()).unwrap();
        for search_id in [&recent, &old] {
            service
                .rate_hits(&context, search_id, vec![rating.clone()])
                .unwrap();
        }
        assert!(service.rate_hits(&context, &recent, Vec::new()).is_err());
    }
    let service = Service::open(settings()).unwrap();
    let (rows, next) = service.search_log().unwrap().export(0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!(recent));
    assert_eq!(rows[0]["feedback"][0]["useful"], json!(true));
    assert_eq!(next, None);
}
