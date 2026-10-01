//! A collection's in-memory index: the recipe's index plus the revision and
//! metadata of each indexed record, so hits name the revision that was ranked
//! and filters never read SQLite. Built from SQLite on first use and updated
//! after each committed write; nothing is on disk.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::recipe::{self, CollectionIndex, IndexDocument, Recipe};
use crate::store::{Metadata, Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("retrieval failed: {0}")]
    Recipe(recipe::RecipeError),
}

impl From<recipe::RecipeError> for IndexError {
    fn from(error: recipe::RecipeError) -> Self {
        Self::Recipe(error)
    }
}

struct Entry {
    revision: i64,
    metadata: Option<Metadata>,
}

pub struct Index {
    entries: HashMap<String, Entry>,
    inner: Box<dyn CollectionIndex>,
}

impl Index {
    /// The latest revision of every live record in `collection_id`.
    pub fn load(
        store: &Store,
        collection_id: &str,
        recipe: &dyn Recipe,
    ) -> Result<Self, IndexError> {
        let mut entries = HashMap::new();
        let documents: Vec<IndexDocument> = store
            .live_documents(collection_id)?
            .into_iter()
            .map(|revision| {
                entries.insert(
                    revision.record_id.clone(),
                    Entry {
                        revision: revision.revision,
                        metadata: revision.metadata,
                    },
                );
                IndexDocument {
                    record_id: revision.record_id,
                    revision: revision.revision,
                    text: revision.text.unwrap_or_default(),
                }
            })
            .collect();
        Ok(Self {
            entries,
            inner: recipe.load(collection_id, documents)?,
        })
    }

    /// Replaces any indexed revision of `record_id`.
    pub fn upsert(
        &mut self,
        record_id: &str,
        revision: i64,
        text: &str,
        metadata: Option<&Metadata>,
    ) -> recipe::Result<()> {
        self.inner.upsert(record_id, revision, text)?;
        let metadata = metadata.cloned();
        self.entries
            .insert(record_id.to_owned(), Entry { revision, metadata });
        Ok(())
    }

    pub fn remove(&mut self, record_id: &str) -> recipe::Result<()> {
        self.inner.remove(record_id)?;
        self.entries.remove(record_id);
        Ok(())
    }

    /// The indexed revision of `record_id`.
    pub fn revision(&self, record_id: &str) -> Option<i64> {
        self.entries.get(record_id).map(|entry| entry.revision)
    }

    /// Records whose metadata has every `filters` key with an equal value.
    pub fn matching(&self, filters: &Metadata) -> HashSet<String> {
        self.entries
            .iter()
            .filter(|(_, entry)| {
                filters.iter().all(|(key, wanted)| {
                    entry
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get(key))
                        .is_some_and(|value| scalar_eq(value, wanted))
                })
            })
            .map(|(record_id, _)| record_id.clone())
            .collect()
    }

    pub fn document_count(&self) -> usize {
        self.entries.len()
    }

    pub fn recipe_index(&self) -> &dyn CollectionIndex {
        self.inner.as_ref()
    }
}

fn scalar_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
            (Some(left), Some(right)) => left == right,
            _ => left.as_f64() == right.as_f64(),
        },
        _ => left == right,
    }
}
