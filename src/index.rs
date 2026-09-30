//! A collection's BM25 index. It lives in memory, is built from SQLite on first
//! use, and is updated after each committed write; there is nothing on disk.

use std::collections::HashMap;
use std::sync::Arc;

use lateweave::SearchPipeline;

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
    collection_id: Arc<str>,
    record_ids: Arc<Vec<Arc<str>>>,
    positions: HashMap<Arc<str>, usize>,
    revisions: Vec<i64>,
    lexical: Arc<Bm25>,
}

impl Index {
    /// The latest revision of every live record in `collection_id`.
    pub fn load(store: &Store, collection_id: &str) -> Result<Self> {
        let documents = store.live_documents(collection_id)?;
        let mut index = Self {
            collection_id: collection_id.into(),
            record_ids: Arc::new(Vec::with_capacity(documents.len())),
            positions: HashMap::with_capacity(documents.len()),
            revisions: Vec::with_capacity(documents.len()),
            lexical: Arc::default(),
        };
        for document in documents {
            index.push(
                document.record_id.into(),
                document.revision,
                document.text.as_deref().unwrap_or_default(),
            );
        }
        Ok(index)
    }

    /// Replaces any indexed revision of `record_id`.
    pub fn upsert(&mut self, record_id: &str, revision: i64, text: &str) {
        self.remove(record_id);
        self.push(record_id.into(), revision, text);
    }

    pub fn remove(&mut self, record_id: &str) {
        let Some(position) = self.positions.remove(record_id) else {
            return;
        };
        Arc::make_mut(&mut self.record_ids).remove(position);
        self.revisions.remove(position);
        for later in &self.record_ids[position..] {
            *self.positions.get_mut(later).expect("indexed record") -= 1;
        }
        Arc::make_mut(&mut self.lexical).remove(position);
    }

    fn push(&mut self, record_id: Arc<str>, revision: i64, text: &str) {
        self.positions
            .insert(record_id.clone(), self.record_ids.len());
        Arc::make_mut(&mut self.record_ids).push(record_id);
        self.revisions.push(revision);
        Arc::make_mut(&mut self.lexical).push(text);
    }

    pub fn document_count(&self) -> usize {
        self.record_ids.len()
    }

    /// The indexed revision of `record_id`.
    pub fn revision(&self, record_id: &str) -> Option<i64> {
        self.positions
            .get(record_id)
            .map(|&position| self.revisions[position])
    }

    pub fn lexical(&self) -> &Bm25 {
        &self.lexical
    }

    /// BM25 candidates scored against `statistics`, with no reranker. Documents
    /// are keyed `(collection ID, record ID)`.
    pub fn pipeline(&self, statistics: Arc<Statistics>) -> SearchPipeline {
        let gatherer = LexicalGatherer::new(
            self.collection_id.clone(),
            self.record_ids.clone(),
            self.lexical.clone(),
            statistics,
        );
        SearchPipeline::new(Arc::new(gatherer), None)
    }
}
