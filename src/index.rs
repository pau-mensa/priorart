//! The retrieval index: vector store, lexical index, and corpus identity.
//!
//! Documents are the latest live revision of every non-deleted record, one per
//! record. Internal IDs are dense and mirrored in the store's
//! `index_documents` table. A record write and the matching vector-store
//! publish are separate steps, and lateweave publishes a mutation as several
//! file renames, so neither is atomic. On open the mirror, the live records,
//! and the vector store are compared, and any disagreement rebuilds the index
//! from the records; that is how every crash between the steps recovers. A
//! failed index step in a running process rebuilds the same way.
//!
//! Zero documents means no `vectors/` directory: lateweave stores cannot be
//! empty, so absence is the empty state.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lateweave::{
    document_ids_digest, CandidateGenerator, CorpusManifest, Feature, MaxSimReranker, Query,
    Reranker, SearchPipeline, StoreFormat, TokenMatrix, VectorStore, DEFAULT_FEATURE,
};

use crate::encoder::{pack, Encoder, EncoderError};
use crate::gather::{Bm25, ExhaustiveGatherer, LexicalGatherer};
use crate::store::{Store, StoreError};

pub const VECTORS_DIRECTORY: &str = "vectors";
/// Storage components are derived from identity, never interpreted as paths.
pub fn collection_index_path(data_dir: &Path, collection_id: &str) -> PathBuf {
    data_dir.join("indexes").join(
        Sha256::digest(collection_id.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
}

/// Bounded LRU of loaded indexes. The mutable borrow prevents eviction while an
/// index is in use. Callers must serialize store mutations with index operations.
pub struct CollectionIndexManager {
    data_dir: PathBuf,
    encoder: Option<Arc<dyn Encoder>>,
    capacity: NonZeroUsize,
    loaded: HashMap<String, Index>,
    lru: VecDeque<String>,
}

impl CollectionIndexManager {
    pub fn new(data_dir: &Path, encoder: Option<Arc<dyn Encoder>>, capacity: NonZeroUsize) -> Self {
        Self {
            data_dir: data_dir.to_owned(),
            encoder,
            capacity,
            loaded: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    pub fn get(&mut self, store: &Store, collection_id: &str) -> Result<&mut Index> {
        store.get_collection(collection_id)?;
        if !self.loaded.contains_key(collection_id) {
            // Evict before loading, so even a rebuild respects the loaded-count bound.
            if self.loaded.len() == self.capacity.get() {
                let oldest = self
                    .lru
                    .pop_front()
                    .expect("a full cache has an oldest index");
                self.loaded.remove(&oldest);
            }
            let index = Index::open(&self.data_dir, store, self.encoder.clone(), collection_id)?;
            self.loaded.insert(collection_id.to_owned(), index);
        }
        self.lru.retain(|id| id != collection_id);
        self.lru.push_back(collection_id.to_owned());
        let index = self.loaded.get_mut(collection_id).expect("loaded above");
        index.ensure_current(store)?;
        Ok(index)
    }

    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    pub fn loaded(&self, collection_id: &str) -> Option<&Index> {
        self.loaded.get(collection_id)
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    collection_id: String,
    version: String,
    recipe: String,
    generation: u64,
    representation: Option<lateweave::Representation>,
    vector_generation: Option<u64>,
    records: Vec<(String, i64)>,
}

// Version of the current single-record indexing and lexical recipe.
const INDEX_RECIPE: &str = "priorart-record-bm25-v1";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Manifest(#[from] serde_json::Error),
    #[error("the index requires recovery before retrieval")]
    Stale,
    #[error("the index is empty")]
    Empty,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Encoder(#[from] EncoderError),
    #[error(transparent)]
    Retrieval(#[from] lateweave::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

pub struct Index {
    collection_id: String,
    corpus_version: String,
    manifest_path: PathBuf,
    vectors_path: PathBuf,
    encoder: Option<Arc<dyn Encoder>>,
    vectors: Option<Arc<VectorStore>>,
    record_ids: Vec<String>,
    positions: HashMap<String, usize>,
    revisions: Vec<i64>,
    generation: u64,
    manifest: Option<CorpusManifest>,
    lexical: Arc<Bm25>,
    stale: bool,
}

/// A document's vectors, computed before its record is written.
pub struct EncodedDocument(Option<Vec<TokenMatrix>>);

impl Index {
    pub fn open(
        data_dir: &Path,
        store: &Store,
        encoder: Option<Arc<dyn Encoder>>,
        collection_id: &str,
    ) -> Result<Self> {
        store.get_collection(collection_id)?;
        let directory = collection_index_path(data_dir, collection_id);
        std::fs::create_dir_all(&directory)?;
        let mut index = Self {
            collection_id: collection_id.to_owned(),
            corpus_version: uuid::Uuid::new_v4().to_string(),
            manifest_path: directory.join("manifest.json"),
            vectors_path: directory.join(VECTORS_DIRECTORY),
            encoder,
            vectors: None,
            record_ids: Vec::new(),
            positions: HashMap::new(),
            revisions: Vec::new(),
            generation: 0,
            manifest: None,
            lexical: Arc::default(),
            stale: false,
        };
        index.load(store)?;
        Ok(index)
    }

    fn load(&mut self, store: &Store) -> Result<()> {
        let mut live: HashMap<String, (i64, String)> = store
            .live_documents(&self.collection_id)?
            .into_iter()
            .map(|document| {
                (
                    document.record_id,
                    (document.revision, document.text.unwrap_or_default()),
                )
            })
            .collect();
        let mirror = store.index_documents(&self.collection_id)?;
        let mirrored: HashSet<(&str, i64)> = mirror
            .iter()
            .map(|entry| (entry.record_id.as_str(), entry.revision))
            .collect();
        let snapshot = std::fs::read(&self.manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok());
        let snapshot_matches = snapshot.as_ref().is_some_and(|saved| {
            saved.collection_id == self.collection_id
                && saved.recipe == INDEX_RECIPE
                && !saved.version.is_empty()
                && saved.representation == self.encoder.as_ref().map(|e| e.representation().clone())
                && saved.records
                    == mirror
                        .iter()
                        .map(|e| (e.record_id.clone(), e.revision))
                        .collect::<Vec<_>>()
        });
        if let Some(saved) = snapshot.as_ref().filter(|_| snapshot_matches) {
            self.corpus_version = saved.version.clone();
            self.generation = saved.generation;
        }
        let consistent = snapshot_matches
            && mirror.len() == live.len()
            && mirror
                .iter()
                .enumerate()
                .all(|(position, entry)| entry.internal_id == position as u64)
            && live.iter().all(|(record_id, (revision, _))| {
                mirrored.contains(&(record_id.as_str(), *revision))
            });
        let vectors = if consistent {
            self.matching_vectors(
                store,
                mirror.len(),
                snapshot.as_ref().and_then(|s| s.vector_generation),
            )?
        } else {
            None
        };
        let Some(vectors) = vectors else {
            return self.rebuild(store);
        };
        let texts: Vec<String> = mirror
            .iter()
            .map(|entry| {
                live.remove(&entry.record_id)
                    .map(|(_, text)| text)
                    .unwrap_or_default()
            })
            .collect();
        self.lexical = Arc::new(Bm25::new(&texts));
        self.revisions = mirror.iter().map(|entry| entry.revision).collect();
        self.record_ids = mirror.into_iter().map(|entry| entry.record_id).collect();
        self.index_positions();
        self.vectors = vectors.map(Arc::new);
        self.refresh();
        Ok(())
    }

    /// The vector store for a consistent mirror of `expected` documents, if
    /// there should be one, or `None` when the stored vectors disagree.
    fn matching_vectors(
        &self,
        store: &Store,
        expected: usize,
        generation: Option<u64>,
    ) -> Result<Option<Option<VectorStore>>> {
        let Some(encoder) = &self.encoder else {
            return Ok(Some(None));
        };
        if expected == 0 {
            return Ok((!self.vectors_path.exists()).then_some(None));
        }
        // The mirror names the encoder that last wrote it; a lexical-only run
        // records none, so vectors left from before it are never trusted.
        if store.index_encoder(&self.collection_id)?.as_deref()
            != Some(encoder.representation().encoder())
        {
            return Ok(None);
        }
        Ok(VectorStore::open(&self.vectors_path)
            .ok()
            .and_then(|vectors| {
                (Some(vectors.generation()) == generation
                    && vectors.format() == StoreFormat::Int8
                    && vectors.document_count() == expected as u64
                    && vectors.representation() == encoder.representation())
                .then_some(Some(vectors))
            }))
    }

    /// Re-encodes every live record and replaces the vector store and mirror.
    pub fn rebuild(&mut self, store: &Store) -> Result<()> {
        self.stale = true;
        let documents = store.live_documents(&self.collection_id)?;
        self.drop_vectors()?;
        self.record_ids = documents
            .iter()
            .map(|document| document.record_id.clone())
            .collect();
        self.index_positions();
        self.revisions = documents.iter().map(|document| document.revision).collect();
        let texts: Vec<String> = documents
            .into_iter()
            .map(|document| document.text.unwrap_or_default())
            .collect();
        self.lexical = Arc::new(Bm25::new(&texts));
        if let Some(encoder) = &self.encoder {
            if !texts.is_empty() {
                let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
                let encoded = encoder.encode_documents(&texts)?;
                self.vectors = Some(Arc::new(create_vectors(
                    &self.vectors_path,
                    encoder.as_ref(),
                    &encoded,
                )?));
            }
        }
        store.replace_index_documents(
            &self.collection_id,
            &self
                .record_ids
                .iter()
                .map(String::as_str)
                .zip(self.revisions.iter().copied())
                .collect::<Vec<_>>(),
            self.encoder_name(),
        )?;
        self.publish()?;
        self.stale = false;
        Ok(())
    }

    /// Encodes a document before its record is written, so an encoder failure
    /// leaves nothing to reconcile.
    pub fn encode(&self, text: &str) -> Result<EncodedDocument> {
        Ok(EncodedDocument(match &self.encoder {
            Some(encoder) => Some(encoder.encode_documents(&[text])?),
            None => None,
        }))
    }

    /// Indexes a written revision. The record is already durable, so a failed
    /// index step rebuilds from the store rather than leave the two diverged.
    pub fn upsert(
        &mut self,
        store: &Store,
        record_id: &str,
        revision: i64,
        text: &str,
        encoded: EncodedDocument,
    ) -> Result<()> {
        self.ensure_current(store)?;
        let result = self.apply_upsert(store, record_id, revision, text, encoded);
        self.reconcile(store, result)
    }

    pub fn remove(&mut self, store: &Store, record_id: &str) -> Result<()> {
        self.ensure_current(store)?;
        let result = self.apply_remove(store, record_id);
        self.reconcile(store, result)
    }

    /// Rebuilds first if an earlier index step failed and its rebuild did too.
    pub fn ensure_current(&mut self, store: &Store) -> Result<()> {
        if self.stale {
            self.rebuild(store)?;
        }
        Ok(())
    }

    fn reconcile(&mut self, store: &Store, result: Result<()>) -> Result<()> {
        let Err(error) = result else {
            return Ok(());
        };
        eprintln!("priorart: index update failed ({error}); rebuilding from the store");
        self.stale = true;
        self.rebuild(store)
    }

    fn apply_upsert(
        &mut self,
        store: &Store,
        record_id: &str,
        revision: i64,
        text: &str,
        encoded: EncodedDocument,
    ) -> Result<()> {
        let removed = self.positions.get(record_id).copied();
        if let Some(position) = removed {
            self.remove_position(position)?;
        }
        if let (Some(encoder), Some(encoded)) = (self.encoder.clone(), encoded.0) {
            match &self.vectors {
                Some(vectors) => {
                    let (values, lengths) = pack(&encoded);
                    let dimension = encoder.representation().dimension();
                    vectors.append(&values, dimension, &lengths, None)?;
                }
                None => {
                    self.drop_vectors()?;
                    self.vectors = Some(Arc::new(create_vectors(
                        &self.vectors_path,
                        encoder.as_ref(),
                        &encoded,
                    )?));
                }
            }
        }
        self.positions
            .insert(record_id.to_owned(), self.record_ids.len());
        self.record_ids.push(record_id.to_owned());
        self.revisions.push(revision);
        Arc::make_mut(&mut self.lexical).push(text);
        store.update_index_documents(
            &self.collection_id,
            removed.map(|position| position as u64),
            Some((record_id, revision)),
            self.encoder_name(),
        )?;
        self.publish()?;
        Ok(())
    }

    fn apply_remove(&mut self, store: &Store, record_id: &str) -> Result<()> {
        let Some(position) = self.positions.get(record_id).copied() else {
            return Ok(());
        };
        self.remove_position(position)?;
        store.update_index_documents(
            &self.collection_id,
            Some(position as u64),
            None,
            self.encoder_name(),
        )?;
        self.publish()?;
        Ok(())
    }

    fn remove_position(&mut self, position: usize) -> Result<()> {
        if let Some(vectors) = &self.vectors {
            if vectors.document_count() == 1 {
                self.drop_vectors()?;
            } else {
                vectors.delete(&[position as u64])?;
            }
        }
        let removed = self.record_ids.remove(position);
        self.positions.remove(&removed);
        for later in &self.record_ids[position..] {
            *self
                .positions
                .get_mut(later)
                .expect("every indexed record has a position") -= 1;
        }
        self.revisions.remove(position);
        Arc::make_mut(&mut self.lexical).remove(position);
        Ok(())
    }

    fn drop_vectors(&mut self) -> Result<()> {
        self.vectors = None;
        if self.vectors_path.exists() {
            std::fs::remove_dir_all(&self.vectors_path)?;
        }
        Ok(())
    }

    fn encoder_name(&self) -> Option<&str> {
        self.encoder
            .as_ref()
            .map(|encoder| encoder.representation().encoder())
    }

    fn index_positions(&mut self) {
        self.positions = self
            .record_ids
            .iter()
            .enumerate()
            .map(|(position, record_id)| (record_id.clone(), position))
            .collect();
    }

    fn publish(&mut self) -> Result<()> {
        self.generation += 1;
        let snapshot = Snapshot {
            collection_id: self.collection_id.clone(),
            version: self.corpus_version.clone(),
            recipe: INDEX_RECIPE.to_owned(),
            generation: self.generation,
            representation: self.encoder.as_ref().map(|e| e.representation().clone()),
            vector_generation: self.vectors.as_ref().map(|v| v.generation()),
            records: self
                .record_ids
                .iter()
                .cloned()
                .zip(self.revisions.iter().copied())
                .collect(),
        };
        let temporary = self.manifest_path.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec(&snapshot)?)?;
        std::fs::rename(temporary, &self.manifest_path)?;
        self.refresh();
        Ok(())
    }

    fn refresh(&mut self) {
        self.manifest = (!self.record_ids.is_empty()).then(|| {
            CorpusManifest::new(
                self.collection_id.clone(),
                self.corpus_version.clone(),
                self.record_ids.len() as u64,
                document_ids_digest(&self.record_ids),
            )
            .expect("the corpus identity fields are non-empty")
            .with_generation(self.generation)
        });
    }

    pub fn document_count(&self) -> usize {
        self.record_ids.len()
    }

    pub fn record_ids(&self) -> &[String] {
        &self.record_ids
    }

    pub fn revisions(&self) -> &[i64] {
        &self.revisions
    }

    pub fn vectors(&self) -> Option<&VectorStore> {
        self.vectors.as_deref()
    }

    pub fn manifest(&self) -> Option<&CorpusManifest> {
        self.manifest.as_ref()
    }

    pub fn encoder(&self) -> Option<&Arc<dyn Encoder>> {
        self.encoder.as_ref()
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

    /// A query whose multi-vector feature is encoded only if a stage reads it.
    pub fn query(&self, text: &str) -> Query {
        let query = Query::new(text);
        let Some(encoder) = self.encoder.clone() else {
            return query;
        };
        let representation = encoder.representation().clone();
        let text = text.to_owned();
        query.with_feature(
            DEFAULT_FEATURE,
            Feature::lazy(representation, move || {
                encoder
                    .encode_queries(&[&text])
                    .map_err(|error| lateweave::Error::External(Box::new(error)))?
                    .pop()
                    .ok_or_else(|| {
                        lateweave::Error::InvalidInput("the encoder returned no query".into())
                    })
            }),
        )
    }

    /// Exhaustive MaxSim while `eligible` fits in `gather_limit`, BM25
    /// candidates beyond it or without an encoder.
    pub fn pipeline(&self, eligible: usize, gather_limit: usize) -> Result<SearchPipeline> {
        if self.stale {
            return Err(IndexError::Stale);
        }
        let manifest = self.manifest.clone().ok_or(IndexError::Empty)?;
        let gatherer: Arc<dyn CandidateGenerator> =
            if self.encoder.is_none() || eligible > gather_limit {
                Arc::new(LexicalGatherer::new(
                    manifest.clone(),
                    self.lexical.clone(),
                )?)
            } else {
                Arc::new(ExhaustiveGatherer::new(manifest.clone()))
            };
        let reranker = match &self.vectors {
            Some(vectors) => Some(Arc::new(MaxSimReranker::new(
                vectors.clone(),
                manifest,
                DEFAULT_FEATURE,
            )?) as Arc<dyn Reranker>),
            None => None,
        };
        Ok(SearchPipeline::new(gatherer, reranker)?)
    }
}

fn create_vectors(
    path: &Path,
    encoder: &dyn Encoder,
    encoded: &[TokenMatrix],
) -> Result<VectorStore> {
    let (values, lengths) = pack(encoded);
    Ok(VectorStore::create(
        path,
        StoreFormat::Int8,
        &values,
        encoder.representation().dimension(),
        &lengths,
        encoder.representation().clone(),
        None,
    )?)
}
