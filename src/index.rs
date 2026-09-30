//! A collection's BM25 index. It lives in memory, is built from SQLite on first
//! use, and is updated after each committed write; there is nothing on disk.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lateweave::{document_ids_digest, CorpusManifest, SearchPipeline};

use crate::gather::{Bm25, LexicalGatherer, Statistics};
use crate::store::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Retrieval(#[from] lateweave::Error),
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

pub struct Index {
    collection_id: String,
    record_ids: Vec<String>,
    positions: HashMap<String, usize>,
    revisions: Vec<i64>,
    lexical: Arc<Bm25>,
    generation: u64,
}

impl Index {
    /// The latest revision of every live record in `collection_id`.
    pub fn load(store: &Store, collection_id: &str) -> Result<Self> {
        let documents = store.live_documents(collection_id)?;
        let mut index = Self {
            collection_id: collection_id.to_owned(),
            record_ids: Vec::with_capacity(documents.len()),
            positions: HashMap::with_capacity(documents.len()),
            revisions: Vec::with_capacity(documents.len()),
            lexical: Arc::default(),
            generation: 0,
        };
        for document in documents {
            index.push(
                document.record_id,
                document.revision,
                document.text.as_deref().unwrap_or_default(),
            );
        }
        Ok(index)
    }

    /// Replaces any indexed revision of `record_id`.
    pub fn upsert(&mut self, record_id: &str, revision: i64, text: &str) {
        self.remove(record_id);
        self.push(record_id.to_owned(), revision, text);
    }

    pub fn remove(&mut self, record_id: &str) {
        let Some(position) = self.positions.remove(record_id) else {
            return;
        };
        self.record_ids.remove(position);
        self.revisions.remove(position);
        for later in &self.record_ids[position..] {
            *self.positions.get_mut(later).expect("indexed record") -= 1;
        }
        Arc::make_mut(&mut self.lexical).remove(position);
        self.generation += 1;
    }

    fn push(&mut self, record_id: String, revision: i64, text: &str) {
        self.positions
            .insert(record_id.clone(), self.record_ids.len());
        self.record_ids.push(record_id);
        self.revisions.push(revision);
        Arc::make_mut(&mut self.lexical).push(text);
        self.generation += 1;
    }

    pub fn document_count(&self) -> usize {
        self.record_ids.len()
    }

    /// The record ID and revision at an internal document ID.
    pub fn document(&self, internal_id: u64) -> (&str, i64) {
        let position = internal_id as usize;
        (&self.record_ids[position], self.revisions[position])
    }

    /// Ascending internal IDs of the indexed records among `record_ids`.
    pub fn internal_ids(&self, record_ids: &HashSet<String>) -> Vec<u64> {
        self.record_ids
            .iter()
            .enumerate()
            .filter(|(_, record_id)| record_ids.contains(*record_id))
            .map(|(position, _)| position as u64)
            .collect()
    }

    pub fn lexical(&self) -> &Bm25 {
        &self.lexical
    }

    /// BM25 candidates scored against `statistics`, with no reranker; a
    /// non-empty index is required.
    pub fn pipeline(&self, statistics: Arc<Statistics>) -> Result<SearchPipeline> {
        let manifest = CorpusManifest::new(
            self.collection_id.clone(),
            "bm25",
            self.record_ids.len() as u64,
            document_ids_digest(&self.record_ids),
        )?
        .with_generation(self.generation);
        let gatherer = LexicalGatherer::new(manifest, self.lexical.clone(), statistics)?;
        Ok(SearchPipeline::new(Arc::new(gatherer), None)?)
    }
}
