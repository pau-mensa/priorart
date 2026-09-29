//! A pylate-onnx-export ColBERT artifact (the official `lightonai/mLateOn`
//! repository ships one) on ONNX Runtime's CPU provider, with the pylate
//! conventions reproduced outside the graph: prefix token inserted after
//! the leading special token, documents truncated to `PRIORART_MAX_TOKENS`, padding masked out, input
//! stripped, and skiplist tokens dropped from documents only.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lateweave::{Representation, TokenMatrix};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;
use tokenizers::{PostProcessor, Tokenizer, TruncationParams};

use super::{Encoder, EncoderError};

const CONFIG_FILE: &str = "onnx_config.json";
const TOKENIZER_FILE: &str = "tokenizer.json";
const BATCH_SIZE: usize = 8;

#[derive(Deserialize)]
struct ExportConfig {
    model_type: Option<String>,
    #[serde(default)]
    do_query_expansion: bool,
    #[serde(default)]
    uses_token_type_ids: bool,
    query_length: usize,
    document_length: usize,
    query_prefix_id: Option<u32>,
    document_prefix_id: Option<u32>,
    pad_token_id: Option<u32>,
    mask_token_id: Option<u32>,
    #[serde(default)]
    skiplist_words: Vec<String>,
    embedding_dim: usize,
    #[serde(default)]
    query_prefix: String,
    #[serde(default)]
    document_prefix: String,
}

struct Side {
    tokenizer: Tokenizer,
    prefix_id: Option<u32>,
    skiplist: HashSet<u32>,
}

pub struct OnnxEncoder {
    representation: Representation,
    query: Side,
    document: Side,
    pad_id: u32,
    session: Mutex<Session>,
}

fn unavailable(model_id: &str, error: impl std::fmt::Display) -> EncoderError {
    EncoderError::Unavailable(format!("encoder {model_id:?}: {error}"))
}

fn failed(error: impl std::fmt::Display) -> EncoderError {
    EncoderError::Failed(error.to_string())
}

impl OnnxEncoder {
    pub fn load(
        model_id: &str,
        filename: &str,
        revision: &str,
        threads: Option<usize>,
        max_tokens: usize,
    ) -> Result<Self, EncoderError> {
        let directory = resolve(model_id, filename, revision)?;
        let config: ExportConfig = serde_json::from_slice(
            &fs::read(directory.join(CONFIG_FILE)).map_err(|error| unavailable(model_id, error))?,
        )
        .map_err(|error| unavailable(model_id, error))?;
        if config.model_type.as_deref() != Some("ColBERT") {
            return Err(unavailable(model_id, "not an exported ColBERT model"));
        }
        if config.do_query_expansion {
            return Err(unavailable(model_id, "query expansion is not supported"));
        }
        if config.uses_token_type_ids {
            return Err(unavailable(
                model_id,
                "token_type_ids inputs are not supported",
            ));
        }

        let tokenizer = |length: usize, prefix: Option<u32>| {
            let mut tokenizer = Tokenizer::from_file(directory.join(TOKENIZER_FILE))
                .map_err(|error| unavailable(model_id, error))?;
            tokenizer.with_padding(None);
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length: length.saturating_sub(usize::from(prefix.is_some())),
                    ..TruncationParams::default()
                }))
                .map_err(|error| unavailable(model_id, error))?;
            Ok::<_, EncoderError>(tokenizer)
        };
        if max_tokens > config.document_length {
            return Err(unavailable(
                model_id,
                format!(
                    "PRIORART_MAX_TOKENS={max_tokens} exceeds the model's document length {}",
                    config.document_length
                ),
            ));
        }
        let document_tokenizer = tokenizer(max_tokens, config.document_prefix_id)?;
        let reserved = document_tokenizer
            .get_post_processor()
            .map_or(0, |processor| processor.added_tokens(false))
            + usize::from(config.document_prefix_id.is_some());
        if max_tokens <= reserved {
            return Err(unavailable(
                model_id,
                format!("PRIORART_MAX_TOKENS must exceed the {reserved} special tokens"),
            ));
        }
        // pylate resolves skiplist words with convert_tokens_to_ids, which maps
        // unknown words to [UNK].
        let unknown = document_tokenizer.token_to_id("[UNK]");
        let skiplist = config
            .skiplist_words
            .iter()
            .filter_map(|word| document_tokenizer.token_to_id(word).or(unknown))
            .collect();

        let mut builder = Session::builder()
            .map_err(|error| unavailable(model_id, error))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|error| unavailable(model_id, error))?;
        if let Some(threads) = threads {
            builder = builder
                .with_intra_threads(threads)
                .map_err(|error| unavailable(model_id, error))?;
        }
        let session = builder
            .commit_from_file(directory.join(filename))
            .map_err(|error| unavailable(model_id, error))?;

        let representation = Representation::new(model_id, revision, config.embedding_dim, true)
            .map_err(|error| unavailable(model_id, error))?
            .with_templates(config.query_prefix, config.document_prefix);
        Ok(Self {
            representation,
            query: Side {
                tokenizer: tokenizer(config.query_length, config.query_prefix_id)?,
                prefix_id: config.query_prefix_id,
                skiplist: HashSet::new(),
            },
            document: Side {
                tokenizer: document_tokenizer,
                prefix_id: config.document_prefix_id,
                skiplist,
            },
            pad_id: config.pad_token_id.or(config.mask_token_id).unwrap_or(0),
            session: Mutex::new(session),
        })
    }

    fn encode(&self, side: &Side, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        let mut output = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH_SIZE) {
            output.extend(self.encode_batch(side, chunk)?);
        }
        Ok(output)
    }

    fn encode_batch(&self, side: &Side, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        // pylate strips the text before tokenizing; mirror it so token counts match.
        let stripped: Vec<&str> = texts.iter().map(|text| text.trim()).collect();
        let encodings = side
            .tokenizer
            .encode_batch(stripped, true)
            .map_err(failed)?;
        let rows: Vec<(&[u32], &[u32])> = encodings
            .iter()
            .map(|encoding| (encoding.get_ids(), encoding.get_attention_mask()))
            .collect();
        let batch = build_batch(&rows, side.prefix_id, self.pad_id, &side.skiplist);
        let shape = [rows.len(), batch.width];
        let input_ids = Tensor::from_array((shape, batch.input_ids.clone())).map_err(failed)?;
        let attention = Tensor::from_array((shape, batch.attention.clone())).map_err(failed)?;
        let dimension = self.representation.dimension();

        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outputs = session
            .run(ort::inputs!["input_ids" => input_ids, "attention_mask" => attention])
            .map_err(failed)?;
        let (_, values) = outputs[0].try_extract_tensor::<f32>().map_err(failed)?;
        if values.len() != rows.len() * batch.width * dimension {
            return Err(failed(
                "model output does not have shape [batch, tokens, dimension]",
            ));
        }

        let mut matrices = Vec::with_capacity(rows.len());
        for (row, keep) in batch.keep.iter().enumerate() {
            let token = |position: usize| {
                let start = (row * batch.width + position) * dimension;
                &values[start..start + dimension]
            };
            let mut kept: Vec<usize> = (0..batch.width)
                .filter(|&position| keep[position])
                .collect();
            if kept.is_empty() {
                // Everything but structure tokens was skipped; keep them so the
                // document still has at least one vector.
                kept = (0..batch.width)
                    .filter(|&position| batch.attention[row * batch.width + position] == 1)
                    .collect();
            }
            let mut vectors = Vec::with_capacity(kept.len() * dimension);
            for position in kept {
                let vector = token(position);
                let norm = vector
                    .iter()
                    .map(|value| value * value)
                    .sum::<f32>()
                    .sqrt()
                    .max(1e-12);
                vectors.extend(vector.iter().map(|value| value / norm));
            }
            matrices.push(TokenMatrix::new(vectors, dimension).map_err(failed)?);
        }
        Ok(matrices)
    }
}

impl Encoder for OnnxEncoder {
    fn representation(&self) -> &Representation {
        &self.representation
    }

    fn encode_queries(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        self.encode(&self.query, texts)
    }

    fn encode_documents(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        self.encode(&self.document, texts)
    }

    fn fit_document<'a>(&self, text: &'a str) -> Result<&'a str, EncoderError> {
        fit(&self.document.tokenizer, text)
    }
}

/// Offsets are relative to the stripped text that [`OnnxEncoder::encode`] tokenizes.
fn fit<'a>(tokenizer: &Tokenizer, text: &'a str) -> Result<&'a str, EncoderError> {
    let encoding = tokenizer.encode(text.trim(), true).map_err(failed)?;
    if encoding.get_overflowing().is_empty() {
        return Ok(text);
    }
    let start = text.len() - text.trim_start().len();
    let end = encoding
        .get_offsets()
        .iter()
        .zip(encoding.get_special_tokens_mask())
        .filter(|(_, &special)| special == 0)
        .map(|(&(_, end), _)| start + end)
        .max()
        .unwrap_or(start);
    let end = (0..=end)
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .unwrap_or(0);
    Ok(&text[..end])
}

#[derive(Debug, PartialEq)]
struct Batch {
    input_ids: Vec<i64>,
    attention: Vec<i64>,
    width: usize,
    keep: Vec<Vec<bool>>,
}

/// Model inputs and keep-masks from tokenizer rows `(ids, attention_mask)`.
///
/// The prefix ID goes right after `[CLS]`; a position is kept when it is
/// attended and not on the skiplist. Queries pass an empty skiplist.
fn build_batch(
    rows: &[(&[u32], &[u32])],
    prefix_id: Option<u32>,
    pad_id: u32,
    skiplist: &HashSet<u32>,
) -> Batch {
    let rows: Vec<(Vec<u32>, Vec<u32>)> = rows
        .iter()
        .map(|&(ids, mask)| {
            let (mut ids, mut mask) = (ids.to_vec(), mask.to_vec());
            if let Some(prefix_id) = prefix_id {
                ids.insert(1, prefix_id);
                mask.insert(1, 1);
            }
            (ids, mask)
        })
        .collect();
    let width = rows.iter().map(|(ids, _)| ids.len()).max().unwrap_or(0);
    let mut batch = Batch {
        input_ids: Vec::with_capacity(rows.len() * width),
        attention: Vec::with_capacity(rows.len() * width),
        width,
        keep: Vec::with_capacity(rows.len()),
    };
    for (mut ids, mut mask) in rows {
        ids.resize(width, pad_id);
        mask.resize(width, 0);
        batch.keep.push(
            ids.iter()
                .zip(&mask)
                .map(|(id, &attended)| attended == 1 && !skiplist.contains(id))
                .collect(),
        );
        batch.input_ids.extend(ids.iter().map(|&id| i64::from(id)));
        batch
            .attention
            .extend(mask.iter().map(|&attended| i64::from(attended)));
    }
    batch
}

fn resolve(model_id: &str, filename: &str, revision: &str) -> Result<PathBuf, EncoderError> {
    let local = match model_id.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map_or_else(
            || PathBuf::from(model_id),
            |home| Path::new(&home).join(rest),
        ),
        None => PathBuf::from(model_id),
    };
    if local.is_dir() {
        return Ok(local);
    }
    let (owner, name) = hf_hub::split_id(model_id);
    hf_hub::HFClientSync::new()
        .map_err(|error| unavailable(model_id, error))?
        .model(owner, name)
        .snapshot_download()
        .revision(revision)
        .allow_patterns(vec![
            filename.to_owned(),
            TOKENIZER_FILE.to_owned(),
            CONFIG_FILE.to_owned(),
        ])
        .send()
        .map_err(|error| unavailable(model_id, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_inserted_padding_masked_and_skiplist_dropped() {
        let first: (&[u32], &[u32]) = (&[1, 10, 11, 2], &[1, 1, 1, 1]);
        let second: (&[u32], &[u32]) = (&[1, 99, 2], &[1, 1, 1]);
        let batch = build_batch(&[first, second], Some(7), 0, &HashSet::from([99]));
        assert_eq!(batch.width, 5);
        assert_eq!(batch.input_ids, [1, 7, 10, 11, 2, 1, 7, 99, 2, 0]);
        assert_eq!(batch.attention, [1, 1, 1, 1, 1, 1, 1, 1, 1, 0]);
        assert_eq!(batch.keep[0], [true; 5]);
        assert_eq!(batch.keep[1], [true, true, false, true, false]);
    }

    #[test]
    fn fit_cuts_after_the_last_kept_token() {
        let mut tokenizer: Tokenizer = r#"{
            "version": "1.0", "truncation": null, "padding": null, "added_tokens": [],
            "normalizer": null, "decoder": null,
            "pre_tokenizer": {"type": "WhitespaceSplit"},
            "post_processor": {"type": "TemplateProcessing",
                "single": [{"SpecialToken": {"id": "<bos>", "type_id": 0}},
                           {"Sequence": {"id": "A", "type_id": 0}},
                           {"SpecialToken": {"id": "<eos>", "type_id": 0}}],
                "pair": [],
                "special_tokens": {
                    "<bos>": {"id": "<bos>", "ids": [0], "tokens": ["<bos>"]},
                    "<eos>": {"id": "<eos>", "ids": [1], "tokens": ["<eos>"]}}},
            "model": {"type": "WordLevel", "unk_token": "<unk>",
                "vocab": {"<bos>": 0, "<eos>": 1, "<unk>": 2}}
        }"#
        .parse()
        .unwrap();
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 4,
                ..TruncationParams::default()
            }))
            .unwrap();
        assert_eq!(fit(&tokenizer, "  café au lait ").unwrap(), "  café au");
        assert_eq!(fit(&tokenizer, " café au ").unwrap(), " café au ");
    }

    #[test]
    fn without_prefix_queries_keep_every_attended_token() {
        let row: (&[u32], &[u32]) = (&[1, 99, 2], &[1, 1, 1]);
        let batch = build_batch(&[row], None, 0, &HashSet::new());
        assert_eq!(batch.input_ids, [1, 99, 2]);
        assert_eq!(batch.keep[0], [true; 3]);
    }
}
