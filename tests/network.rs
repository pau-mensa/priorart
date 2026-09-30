mod common;

use priorart::auth::{Grant, Operation};
use priorart::config::{AdminToken, ServerMode, Settings};
use priorart::service::{Service, DATABASE_FILE};
use priorart::store::{Store, Visibility};
use reqwest::{header::HeaderMap, Client, StatusCode};
use std::sync::Arc;
use tempfile::TempDir;

const TOKEN: &str = "operator-token-0123456789abcdef-0123456789";

struct Server {
    _dir: TempDir,
    client: Client,
    url: String,
    keys: [String; 2],
    records: String,
}

impl Server {
    async fn start(peer: &str, settings: Settings) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join(DATABASE_FILE)).unwrap();
        let principal = store.create_principal().unwrap();
        let collection = store
            .create_collection(&principal, Visibility::Restricted)
            .unwrap();
        let grants = [Grant::new(&collection, Operation::Read)];
        let keys = [(); 2].map(|_| {
            store
                .issue_credential(&principal, &grants, None)
                .unwrap()
                .into_secret()
        });
        let service = Service::open(Settings {
            data_dir: dir.path().to_owned(),
            ..settings
        })
        .unwrap();
        let url = common::spawn_as(Arc::new(service), peer.parse().unwrap()).await;
        Self {
            _dir: dir,
            client: Client::new(),
            url,
            keys,
            records: format!("/v1/collections/{collection}/records"),
        }
    }

    async fn get(&self, path: &str, key: Option<&str>, headers: &[(&str, &str)]) -> StatusCode {
        self.request(path, key, headers).await.status()
    }

    async fn request(
        &self,
        path: &str,
        key: Option<&str>,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut request = self.client.get(format!("{}{path}", self.url));
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap()
    }
}

fn authenticated() -> Settings {
    Settings {
        mode: ServerMode::Authenticated,
        ..Settings::default()
    }
}

fn behind(proxies: &str) -> Settings {
    Settings {
        trusted_proxies: proxies.split(',').map(|p| p.parse().unwrap()).collect(),
        ..authenticated()
    }
}

#[tokio::test]
async fn by_default_only_loopback_peers_are_served() {
    for peer in ["127.0.0.1:9", "[::1]:9", "[::ffff:127.0.0.1]:9"] {
        let server = Server::start(peer, authenticated()).await;
        assert_eq!(
            server
                .get(&server.records, Some(&server.keys[0]), &[])
                .await,
            StatusCode::OK,
            "{peer}"
        );
    }
    for settings in [Settings::default(), authenticated()] {
        let server = Server::start("203.0.113.9:9", settings).await;
        for key in [None, Some(server.keys[0].as_str())] {
            let response = server.request("/healthz", key, &[]).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], "https_required");
        }
    }
}

#[tokio::test]
async fn a_trusted_proxy_must_report_https() {
    for peer in ["10.1.2.3:9", "[::ffff:10.1.2.3]:9"] {
        let server = Server::start(peer, behind("10.0.0.0/8")).await;
        let key = Some(server.keys[0].as_str());
        let https = [
            ("X-Forwarded-Proto", "https"),
            ("X-Forwarded-For", "198.51.100.4"),
        ];
        assert_eq!(
            server.get(&server.records, key, &https).await,
            StatusCode::OK
        );
        assert_eq!(
            server
                .get(&server.records, key, &[("X-Forwarded-Proto", "HTTPS")])
                .await,
            StatusCode::OK
        );
        for headers in [
            &[][..],
            &[("X-Forwarded-Proto", "http")],
            &[("X-Forwarded-Proto", "https, http")],
            &[
                ("X-Forwarded-Proto", "https"),
                ("X-Forwarded-Proto", "https"),
            ],
            &[("Forwarded", "proto=https")],
        ] {
            assert_eq!(
                server.get(&server.records, key, headers).await,
                StatusCode::FORBIDDEN,
                "{peer} {headers:?}"
            );
        }
    }
}

#[tokio::test]
async fn forwarding_headers_from_untrusted_peers_are_rejected() {
    for peer in ["10.1.2.3:9", "127.0.0.1:9"] {
        let server = Server::start(peer, behind("192.168.0.1/32")).await;
        let forged = [("X-Forwarded-Proto", "https")];
        assert_eq!(
            server
                .get(&server.records, Some(&server.keys[0]), &forged)
                .await,
            StatusCode::BAD_REQUEST,
            "{peer}"
        );
    }
    let server = Server::start("10.1.2.3:9", behind("192.168.0.1/32")).await;
    assert_eq!(
        server
            .get(&server.records, Some(&server.keys[0]), &[])
            .await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn plain_http_from_remote_peers_needs_an_explicit_opt_in() {
    let server = Server::start(
        "203.0.113.9:9",
        Settings {
            allow_insecure_http: true,
            ..authenticated()
        },
    )
    .await;
    assert_eq!(
        server
            .get(&server.records, Some(&server.keys[0]), &[])
            .await,
        StatusCode::OK
    );
    assert_eq!(
        server.get(&server.records, None, &[]).await,
        StatusCode::NOT_FOUND
    );
    for settings in [
        Settings {
            allow_insecure_http: true,
            ..Settings::default()
        },
        Settings {
            host: "0.0.0.0".into(),
            ..authenticated()
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        assert!(Service::open(Settings {
            data_dir: dir.path().to_owned(),
            ..settings
        })
        .is_err());
    }
}

#[tokio::test]
async fn key_limits_apply_per_key_and_only_when_configured() {
    let unlimited = Server::start("127.0.0.1:9", authenticated()).await;
    for _ in 0..5 {
        assert_eq!(
            unlimited
                .get(&unlimited.records, Some(&unlimited.keys[0]), &[])
                .await,
            StatusCode::OK
        );
    }
    let server = Server::start(
        "127.0.0.1:9",
        Settings {
            key_requests_per_minute: Some(2),
            admin_token: Some(AdminToken::new(TOKEN).unwrap()),
            ..authenticated()
        },
    )
    .await;
    let [first, second] = &server.keys;
    for _ in 0..2 {
        assert_eq!(
            server.get(&server.records, Some(first), &[]).await,
            StatusCode::OK
        );
    }
    let limited = server.request(&server.records, Some(first), &[]).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = retry_after(limited.headers());
    assert!((1..=30).contains(&retry), "{retry}");
    let body: serde_json::Value = limited.json().await.unwrap();
    assert_eq!(body["error"]["code"], "rate_limited");
    assert_eq!(
        server.get(&server.records, Some(second), &[]).await,
        StatusCode::OK
    );
    for _ in 0..3 {
        assert_eq!(server.get("/healthz", None, &[]).await, StatusCode::OK);
        assert_eq!(
            server
                .get("/v1/admin/principals/none/credentials", Some(TOKEN), &[])
                .await,
            StatusCode::OK
        );
    }
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers["retry-after"].to_str().unwrap().parse().unwrap()
}
