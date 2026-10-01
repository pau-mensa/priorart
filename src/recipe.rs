//! The retrieval seam: how collections are indexed and how a search across
//! them is ranked. The built-in recipe is [`Bm25`](crate::gather::Bm25Recipe);
//! another can be passed to [`Service::open_with`](crate::service::Service::open_with)
//! or [`cli::run`](crate::cli::run).
//!
//! These traits and the lateweave types in them may change in any minor
//! release before 1.0; depend on an exact priorart version.
use std::any::Any;

use lateweave::{Query, SearchPipeline};

pub type RecipeError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = RecipeError> = std::result::Result<T, E>;

/// The latest revision of a live record.
pub struct IndexDocument {
    pub record_id: String,
    pub revision: i64,
    pub text: String,
}

pub trait Recipe: Send + Sync {
    /// Builds a collection's index from its live records. Called on first
    /// search, and again after an eviction, a restart, or a failed update.
    fn load(
        &self,
        collection: &str,
        documents: Vec<IndexDocument>,
    ) -> Result<Box<dyn CollectionIndex>>;

    /// The query the pipeline receives; a recipe with query features computes
    /// them here.
    fn query(&self, text: &str) -> Result<Query> {
        Ok(Query::new(text))
    }

    /// How many candidates to gather for `limit` results; a reranker wants more.
    fn gather_limit(&self, limit: usize) -> usize {
        limit
    }

    /// One pipeline over every selected collection, given in collection ID
    /// order. Keys are (collection ID, record ID) of indexed records; scores
    /// must compare across collections, and ties must break deterministically.
    fn pipeline(
        &self,
        query: &Query,
        indexes: &[(&str, &dyn CollectionIndex)],
    ) -> Result<SearchPipeline>;
}

/// One collection's in-memory index, owned by the service and updated after
/// every committed write. Searches on a collection share it concurrently;
/// updates are exclusive.
pub trait CollectionIndex: Any + Send + Sync {
    /// An error discards the index so the next search rebuilds it from
    /// SQLite; the write has already committed and still succeeds.
    fn upsert(&mut self, record_id: &str, revision: i64, text: &str) -> Result<()>;
    fn remove(&mut self, record_id: &str) -> Result<()>;
}
