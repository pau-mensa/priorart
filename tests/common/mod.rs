#![allow(dead_code)]

/// Serves `service` on an ephemeral loopback port and returns its base URL.
pub async fn spawn(service: std::sync::Arc<priorart::service::Service>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            priorart::api::router(service)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{address}")
}

pub fn service(directory: &tempfile::TempDir) -> std::sync::Arc<priorart::service::Service> {
    let settings = priorart::config::Settings {
        data_dir: directory.path().to_path_buf(),
        ..priorart::config::Settings::default()
    };
    std::sync::Arc::new(priorart::service::Service::open(settings).unwrap())
}
