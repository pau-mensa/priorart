//! Encoder boundary: raw text to per-token unit vectors.
//!
//! The trait is the only thing the rest of the server knows about a model.
//! [`OnnxEncoder`] is the implementation, behind the default `onnx` feature.

#[cfg(feature = "onnx")]
mod onnx;

use std::sync::Arc;

use lateweave::{Representation, TokenMatrix};

#[cfg(feature = "onnx")]
pub use onnx::OnnxEncoder;

use crate::config::Settings;

#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    /// The configured encoder cannot be loaded in this build or environment.
    #[error("{0}")]
    Unavailable(String),
    #[error("encoding failed: {0}")]
    Failed(String),
}

pub trait Encoder: Send + Sync {
    fn representation(&self) -> &Representation;

    fn encode_queries(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError>;

    fn encode_documents(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError>;
}

/// Concatenates `[tokens, dimension]` matrices into lateweave's packed form.
pub fn pack(matrices: &[TokenMatrix]) -> (Vec<f32>, Vec<usize>) {
    let values = matrices
        .iter()
        .flat_map(|matrix| matrix.values().iter().copied())
        .collect();
    let lengths = matrices.iter().map(TokenMatrix::tokens).collect();
    (values, lengths)
}

/// `None` for lexical-only operation (`PRIORART_ENCODER=none`).
pub fn load_encoder(settings: &Settings) -> Result<Option<Arc<dyn Encoder>>, EncoderError> {
    if settings.lexical_only() {
        return Ok(None);
    }
    #[cfg(feature = "onnx")]
    {
        let encoder = OnnxEncoder::load(
            &settings.encoder,
            &settings.encoder_file,
            &settings.encoder_revision,
            settings.encoder_threads,
        )?;
        Ok(Some(Arc::new(encoder)))
    }
    #[cfg(not(feature = "onnx"))]
    Err(EncoderError::Unavailable(format!(
        "encoder {:?} needs a priorart build with the `onnx` feature",
        settings.encoder
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_concatenates_and_reports_lengths() {
        let first = TokenMatrix::new(vec![1.0; 8], 4).unwrap();
        let second = TokenMatrix::new(vec![0.0; 12], 4).unwrap();
        let (values, lengths) = pack(&[first, second]);
        assert_eq!(values.len(), 20);
        assert_eq!(lengths, [2, 3]);
    }

    #[test]
    fn none_means_lexical_only() {
        for name in ["none", "NONE"] {
            let settings = Settings {
                encoder: name.to_owned(),
                ..Settings::default()
            };
            assert!(load_encoder(&settings).unwrap().is_none());
        }
    }
}
