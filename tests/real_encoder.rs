#![cfg(feature = "onnx")]
//! Smoke tests against the official LateOn-Code ONNX artifact, ignored by
//! default: `cargo test --release --test real_encoder -- --ignored`.
//! The first run downloads about 150 MB.
use priorart::auth::RequestContext;
use priorart::store::LOCAL_COLLECTION_ID;

use std::sync::{Arc, OnceLock};

use priorart::config::Settings;
use priorart::encoder::{Encoder, OnnxEncoder};
use priorart::service::Service;

const MODEL: &str = "lightonai/LateOn-Code";

fn encoder() -> Arc<OnnxEncoder> {
    static ENCODER: OnceLock<Arc<OnnxEncoder>> = OnceLock::new();
    ENCODER
        .get_or_init(|| {
            Arc::new(OnnxEncoder::load(MODEL, "model_int8.onnx", "main", None).unwrap())
        })
        .clone()
}

#[test]
#[ignore = "downloads LateOn-Code"]
fn representation_and_vectors() {
    let encoder = encoder();
    let representation = encoder.representation();
    assert_eq!(representation.dimension(), 128);
    assert!(representation.normalized());
    assert_eq!(representation.query_template(), "[Q] ");
    let vectors = encoder
        .encode_documents(&["def f(): return 1"])
        .unwrap()
        .remove(0);
    for token in vectors.values().chunks(128) {
        let norm = token.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3);
    }
    // Bare punctuation tokens are on the skiplist and dropped from documents only.
    let text = "foo(bar); x = y[0]";
    let document = encoder.encode_documents(&[text]).unwrap().remove(0);
    let query = encoder.encode_queries(&[text]).unwrap().remove(0);
    assert_eq!(query.tokens() - document.tokens(), 3);
    let stripped = encoder.encode_documents(&["  padded  "]).unwrap().remove(0);
    let plain = encoder.encode_documents(&["padded"]).unwrap().remove(0);
    assert_eq!(stripped.tokens(), plain.tokens());
    assert!(stripped
        .values()
        .iter()
        .zip(plain.values())
        .all(|(left, right)| (left - right).abs() < 1e-5));
}

#[test]
#[ignore = "downloads LateOn-Code"]
fn end_to_end() {
    let encoder = encoder();
    let directory = tempfile::tempdir().unwrap();
    let settings = Settings {
        data_dir: directory.path().to_path_buf(),
        encoder: MODEL.to_owned(),
        ..Settings::default()
    };
    let service = Service::new(settings, Some(encoder as Arc<dyn Encoder>)).unwrap();
    service
        .put(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "torch.compile recompiles every step because a Python int changes; mark it dynamic.",
            None,
            Some("compile"),
            false,
        )
        .unwrap();
    service
        .put(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "NCCL watchdog timeout: rank 3 exited early from a stray sys.exit in the data loader.",
            None,
            Some("nccl"),
            false,
        )
        .unwrap();
    let outcome = service
        .search(
            &RequestContext::local(),
            LOCAL_COLLECTION_ID,
            "distributed training hangs at the end of the first epoch",
            None,
            10,
        )
        .unwrap();
    assert_eq!(outcome.hits[0].id, "nccl");
    assert_eq!(outcome.gatherer, "exhaustive");
}
