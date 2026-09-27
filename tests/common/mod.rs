#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lateweave::{Representation, TokenMatrix};
use priorart::analyzer::tokens;
use priorart::encoder::{Encoder, EncoderError};
use sha2::{Digest, Sha256};

/// Deterministic unit vectors per hashed term; enough for MaxSim to have signal.
pub struct FakeEncoder {
    representation: Representation,
    calls: AtomicUsize,
    failing: AtomicBool,
}

impl FakeEncoder {
    pub fn new() -> Self {
        Self::with("fake", 16)
    }

    pub fn with(name: &str, dimension: usize) -> Self {
        Self {
            representation: Representation::new(name, "test", dimension, true).unwrap(),
            calls: AtomicUsize::new(0),
            failing: AtomicBool::new(false),
        }
    }

    /// Makes document encoding fail until reset.
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn vector(&self, term: &str) -> Vec<f32> {
        let mut state =
            u64::from_le_bytes(Sha256::digest(term.as_bytes())[..8].try_into().unwrap());
        let vector: Vec<f32> = (0..self.representation.dimension())
            .map(|_| {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut mixed = state;
                mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                mixed ^= mixed >> 31;
                (mixed as f64 / u64::MAX as f64 * 2.0 - 1.0) as f32
            })
            .collect();
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        vector.into_iter().map(|value| value / norm).collect()
    }

    fn encode(&self, texts: &[&str]) -> Vec<TokenMatrix> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        texts
            .iter()
            .map(|text| {
                let mut terms = tokens(text);
                if terms.is_empty() {
                    terms.push("<empty>".to_owned());
                }
                let values = terms.iter().flat_map(|term| self.vector(term)).collect();
                TokenMatrix::new(values, self.representation.dimension()).unwrap()
            })
            .collect()
    }
}

impl Encoder for FakeEncoder {
    fn representation(&self) -> &Representation {
        &self.representation
    }

    fn encode_queries(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        Ok(self.encode(texts))
    }

    fn encode_documents(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(EncoderError::Failed("injected".to_owned()));
        }
        Ok(self.encode(texts))
    }
}

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
    let encoder: std::sync::Arc<dyn Encoder> = std::sync::Arc::new(FakeEncoder::new());
    std::sync::Arc::new(priorart::service::Service::new(settings, Some(encoder)).unwrap())
}
