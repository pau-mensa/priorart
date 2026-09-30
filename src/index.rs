//! A collection's in-memory index: the recipe's index plus the revision of
//! each indexed record, so hits name the revision that was ranked. Built from
//! SQLite on first use and updated after each committed write; nothing is on disk.

use std::collections::HashMap;

use crate::recipe::{self, CollectionIndex, IndexDocument, Recipe};
use crate::store::{Store, StoreError};

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

pub struct Index {
    revisions: HashMap<String, i64>,
    inner: Box<dyn CollectionIndex>,
}

impl Index {
    /// The latest revision of every live record in `collection_id`.
    pub fn load(
        store: &Store,
        collection_id: &str,
        recipe: &dyn Recipe,
    ) -> Result<Self, IndexError> {
        let documents: Vec<IndexDocument> = store
            .live_documents(collection_id)?
            .into_iter()
            .map(|revision| IndexDocument {
                record_id: revision.record_id,
                revision: revision.revision,
                text: revision.text.unwrap_or_default(),
            })
            .collect();
        let revisions = documents
            .iter()
            .map(|document| (document.record_id.clone(), document.revision))
            .collect();
        Ok(Self {
            revisions,
            inner: recipe.load(collection_id, documents)?,
        })
    }

    /// Replaces any indexed revision of `record_id`.
    pub fn upsert(&mut self, record_id: &str, revision: i64, text: &str) -> recipe::Result<()> {
        self.inner.upsert(record_id, revision, text)?;
        self.revisions.insert(record_id.to_owned(), revision);
        Ok(())
    }

    pub fn remove(&mut self, record_id: &str) -> recipe::Result<()> {
        self.inner.remove(record_id)?;
        self.revisions.remove(record_id);
        Ok(())
    }

    /// The indexed revision of `record_id`.
    pub fn revision(&self, record_id: &str) -> Option<i64> {
        self.revisions.get(record_id).copied()
    }

    pub fn document_count(&self) -> usize {
        self.revisions.len()
    }

    pub fn recipe_index(&self) -> &dyn CollectionIndex {
        self.inner.as_ref()
    }
}
