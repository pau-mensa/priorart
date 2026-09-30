mod common;

use priorart::config::{AdminToken, ServerMode, Settings};
use priorart::service::Service;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

const TOKEN: &str = "operator-token-0123456789abcdef-0123456789";

struct Api {
    _dir: TempDir,
    client: Client,
    url: String,
}

impl Api {
    async fn start(token: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::open(Settings {
            data_dir: dir.path().to_owned(),
            mode: ServerMode::Authenticated,
            admin_token: token.map(|token| AdminToken::new(token).unwrap()),
            ..Settings::default()
        })
        .unwrap();
        let url = common::spawn(Arc::new(service)).await;
        Self {
            _dir: dir,
            client: Client::new(),
            url,
        }
    }

    async fn send(
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

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send(method, &format!("/v1/admin{path}"), Some(TOKEN), body)
            .await
    }

    async fn principal(&self) -> String {
        let (status, body) = self.admin(Method::POST, "/principals", None).await;
        assert_eq!(status, StatusCode::CREATED);
        body["id"].as_str().unwrap().to_owned()
    }

    async fn collection(&self, owner: &str, visibility: &str) -> String {
        let (status, body) = self
            .admin(
                Method::POST,
                "/collections",
                Some(json!({"owner": owner, "visibility": visibility})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["owner_principal_id"], owner);
        body["id"].as_str().unwrap().to_owned()
    }

    async fn issue(&self, principal: &str, grants: Value) -> (StatusCode, Value) {
        self.admin(
            Method::POST,
            &format!("/principals/{principal}/credentials"),
            Some(json!({"grants": grants})),
        )
        .await
    }
}

#[tokio::test]
async fn admin_endpoints_do_not_exist_without_a_configured_token() {
    let api = Api::start(None).await;
    for key in [None, Some(TOKEN)] {
        assert_eq!(
            api.send(Method::POST, "/v1/admin/principals", key, None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn only_the_operator_token_opens_admin_endpoints() {
    let api = Api::start(Some(TOKEN)).await;
    let principal = api.principal().await;
    let collection = api.collection(&principal, "restricted").await;
    let (_, issued) = api
        .issue(
            &principal,
            json!([{"collection_id": collection, "operation": "admin"}]),
        )
        .await;
    let key = issued["secret"].as_str().unwrap();
    let almost = &TOKEN[..TOKEN.len() - 1];
    for credential in [None, Some(key), Some(almost), Some("x")] {
        assert_eq!(
            api.send(Method::POST, "/v1/admin/principals", credential, None)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{credential:?}"
        );
    }
    assert_eq!(
        api.send(
            Method::GET,
            &format!("/v1/collections/{collection}"),
            Some(TOKEN),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let doubled = api
        .client
        .post(format!("{}/v1/admin/principals", api.url))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(doubled.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn keys_are_issued_rotated_and_revoked_without_changing_ownership() {
    let api = Api::start(Some(TOKEN)).await;
    let principal = api.principal().await;
    let collection = api.collection(&principal, "restricted").await;
    let (status, issued) = api
        .issue(
            &principal,
            json!([{"collection_id": collection, "operation": "write"}]),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let first = issued["secret"].as_str().unwrap().to_owned();
    let first_id = issued["credential"]["id"].as_str().unwrap().to_owned();
    let records = format!("/v1/collections/{collection}/records");
    let written = api
        .send(
            Method::POST,
            &records,
            Some(&first),
            Some(json!({"id": "note", "text": "operator provisioned"})),
        )
        .await;
    assert_eq!(written.0, StatusCode::CREATED);

    let (status, listed) = api
        .admin(
            Method::GET,
            &format!("/principals/{principal}/credentials"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["credentials"].as_array().unwrap().len(), 1);
    assert!(!listed.to_string().contains(&first));
    assert!(listed["credentials"][0].get("secret").is_none());

    let response = api
        .client
        .post(format!(
            "{}/v1/admin/credentials/{first_id}/rotate",
            api.url
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let rotated: Value = response.json().await.unwrap();
    let second = rotated["secret"].as_str().unwrap().to_owned();
    let second_id = rotated["credential"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        rotated["credential"]["grants"],
        issued["credential"]["grants"]
    );
    let note = format!("{records}/note");
    assert_eq!(
        api.send(Method::GET, &note, Some(&first), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, record) = api.send(Method::GET, &note, Some(&second), None).await;
    assert_eq!(status, StatusCode::OK);
    let mine = api
        .send(
            Method::GET,
            &format!("{records}?author=me"),
            Some(&second),
            None,
        )
        .await;
    assert_eq!(mine.1["records"][0]["id"], "note");
    assert_eq!(record["text"], "operator provisioned");

    let revoked = api
        .admin(Method::DELETE, &format!("/credentials/{second_id}"), None)
        .await;
    assert_eq!(revoked.0, StatusCode::NO_CONTENT);
    assert_eq!(
        api.send(Method::GET, &note, Some(&second), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api.admin(
            Method::POST,
            &format!("/credentials/{second_id}/rotate"),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, reissued) = api
        .issue(
            &principal,
            json!([{"collection_id": collection, "operation": "write"}]),
        )
        .await;
    let third = reissued["secret"].as_str().unwrap();
    let edited = api
        .send(
            Method::POST,
            &records,
            Some(third),
            Some(json!({"id": "note", "text": "still mine"})),
        )
        .await;
    assert_eq!(edited.0, StatusCode::CREATED);
    let listed = api
        .admin(
            Method::GET,
            &format!("/principals/{principal}/credentials"),
            None,
        )
        .await
        .1;
    let credentials = listed["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 3);
    assert_eq!(
        credentials
            .iter()
            .filter(|c| c["revoked_at"].is_null())
            .count(),
        1
    );
}

#[tokio::test]
async fn grant_replacement_takes_effect_immediately_and_follows_issuance_rules() {
    let api = Api::start(Some(TOKEN)).await;
    let alice = api.principal().await;
    let bob = api.principal().await;
    let collection = api.collection(&bob, "restricted").await;
    let other = api.collection(&alice, "restricted").await;
    let (_, issued) = api
        .issue(
            &bob,
            json!([{"collection_id": collection, "operation": "read"}]),
        )
        .await;
    let key = issued["secret"].as_str().unwrap();
    let id = issued["credential"]["id"].as_str().unwrap();
    let grants = format!("/credentials/{id}/grants");
    let records = format!("/v1/collections/{collection}/records");
    let write = || {
        api.send(
            Method::POST,
            &records,
            Some(key),
            Some(json!({"text": "note"})),
        )
    };
    assert_eq!(write().await.0, StatusCode::NOT_FOUND);
    let replaced = api
        .admin(
            Method::PUT,
            &grants,
            Some(json!({"grants": [{"collection_id": collection, "operation": "write"}]})),
        )
        .await;
    assert_eq!(replaced.0, StatusCode::NO_CONTENT);
    assert_eq!(write().await.0, StatusCode::CREATED);
    let listed = api
        .admin(Method::GET, &format!("/principals/{bob}/credentials"), None)
        .await
        .1;
    assert_eq!(listed["credentials"][0]["grants"][0]["operation"], "write");
    assert_eq!(listed["credentials"][0]["grant_version"], 2);
    assert_eq!(
        api.admin(
            Method::PUT,
            &grants,
            Some(json!({"grants": [{"collection_id": other, "operation": "read"}]})),
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(write().await.0, StatusCode::CREATED);
    assert_eq!(
        api.admin(Method::PUT, &grants, Some(json!({"grants": []})))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(write().await.0, StatusCode::NOT_FOUND);
    api.admin(Method::DELETE, &format!("/credentials/{id}"), None)
        .await;
    assert_eq!(
        api.admin(Method::PUT, &grants, Some(json!({"grants": []})))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.send(
            Method::PUT,
            &format!("/v1/admin{grants}"),
            Some(key),
            Some(json!({"grants": []}))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn provisioning_rejects_unknown_principals_and_unownable_grants() {
    let api = Api::start(Some(TOKEN)).await;
    let alice = api.principal().await;
    let bob = api.principal().await;
    let private = api.collection(&alice, "restricted").await;
    let public = api.collection(&alice, "public").await;
    assert_eq!(
        api.admin(
            Method::POST,
            "/collections",
            Some(json!({"owner": "missing"}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for grants in [
        json!([{"collection_id": private, "operation": "read"}]),
        json!([{"collection_id": public, "operation": "admin"}]),
        json!([{"collection_id": "missing", "operation": "read"}]),
    ] {
        assert_eq!(api.issue(&bob, grants).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        api.issue(
            "missing",
            json!([{"collection_id": public, "operation": "read"}])
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.issue(
            &bob,
            json!([{"collection_id": public, "operation": "write"}])
        )
        .await
        .0,
        StatusCode::CREATED
    );
    for body in [
        json!({"grants": [{"collection_id": public, "operation": "moderate"}]}),
        json!({"grants": [], "extra": true}),
    ] {
        assert_eq!(
            api.admin(
                Method::POST,
                &format!("/principals/{bob}/credentials"),
                Some(body)
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        api.admin(Method::DELETE, "/credentials/unknown", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
