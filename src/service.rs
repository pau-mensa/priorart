//! The protocol operations, independent of transport.
//!
//! This service is the local single-collection interface: every operation is
//! bound to the `local` collection and attributed to the local principal.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use lateweave::SearchRequest;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::analyzer::tokens;
use crate::config::Settings;
use crate::encoder::{load_encoder, Encoder, EncoderError};
use crate::excerpt::{excerpt, DEFAULT_WIDTH};
use crate::index::{Index, IndexError};
use crate::store::{
    Metadata, Report, Revision, SearchHit, Store, StoreError, LOCAL_COLLECTION_ID,
    LOCAL_PRINCIPAL_ID,
};

pub const MAX_LIMIT: i64 = 100;
pub const DATABASE_FILE: &str = "priorart.sqlite";

static RECORD_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.:-]{1,128}$").expect("valid record ID regex"));

pub const RECORD_ID_RULE: &str = "id must match ^[A-Za-z0-9_.:-]{1,128}$ and not be . or ..";

/// Whether `id` can name a record. `.` and `..` are excluded because URL
/// parsers resolve them as path segments, so `/v1/records/{id}` cannot reach them.
pub fn is_record_id(id: &str) -> bool {
    RECORD_ID.is_match(id) && id != "." && id != ".."
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The request is well-formed but violates a protocol rule.
    #[error("{0}")]
    InvalidInput(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Index(#[from] IndexError),
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Encoder(#[from] EncoderError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = ServiceError> = std::result::Result<T, E>;

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(ServiceError::InvalidInput(message.into()))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Hit {
    pub collection_id: String,
    pub id: String,
    pub revision: i64,
    pub score: f64,
    pub score_semantics: String,
    pub excerpt: String,
    pub metadata: Option<Metadata>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutcome {
    pub search_id: String,
    pub hits: Vec<Hit>,
    /// Empty when there was nothing to search.
    pub timings: BTreeMap<String, f64>,
    pub gatherer: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub document_count: usize,
    pub encoder: Option<String>,
    pub gather_limit: usize,
}

struct State {
    store: Store,
    index: Index,
}

pub struct Service {
    settings: Settings,
    encoder_name: Option<String>,
    state: Mutex<State>,
}

impl Service {
    pub fn open(settings: Settings) -> Result<Self, OpenError> {
        let encoder = load_encoder(&settings)?;
        Self::new(settings, encoder)
    }

    pub fn new(settings: Settings, encoder: Option<Arc<dyn Encoder>>) -> Result<Self, OpenError> {
        std::fs::create_dir_all(&settings.data_dir)?;
        let store = Store::open(settings.data_dir.join(DATABASE_FILE))?;
        let encoder_name = encoder
            .as_ref()
            .map(|encoder| encoder.representation().encoder().to_owned());
        let index = Index::open(&settings.data_dir, &store, encoder, LOCAL_COLLECTION_ID)?;
        Ok(Self {
            settings,
            encoder_name,
            state: Mutex::new(State { store, index }),
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn put(
        &self,
        text: &str,
        metadata: Option<&Metadata>,
        record_id: Option<&str>,
    ) -> Result<(String, i64)> {
        if text.trim().is_empty() {
            return invalid("text must be a non-empty string");
        }
        if text.len() > self.settings.max_text_bytes {
            return invalid(format!(
                "text is {} bytes; the limit is {}",
                text.len(),
                self.settings.max_text_bytes
            ));
        }
        if record_id.is_some_and(|record_id| !is_record_id(record_id)) {
            return invalid(RECORD_ID_RULE);
        }
        let mut state = self.state();
        let State { store, index } = &mut *state;
        let encoded = index.encode(text)?;
        let created = store.put(
            LOCAL_COLLECTION_ID,
            text,
            metadata,
            record_id,
            LOCAL_PRINCIPAL_ID,
        )?;
        index.upsert(store, &created.record_id, created.revision, text, encoded)?;
        Ok((created.record_id, created.revision))
    }

    pub fn get(&self, record_id: &str, revision: Option<i64>) -> Result<Revision> {
        Ok(self
            .state()
            .store
            .get(LOCAL_COLLECTION_ID, record_id, revision)?)
    }

    pub fn delete(&self, record_id: &str) -> Result<()> {
        let mut state = self.state();
        let State { store, index } = &mut *state;
        if store.delete(LOCAL_COLLECTION_ID, record_id)? {
            index.remove(store, record_id)?;
        }
        Ok(())
    }

    pub fn search(
        &self,
        text: &str,
        filters: Option<&Metadata>,
        limit: i64,
    ) -> Result<SearchOutcome> {
        if text.trim().is_empty() {
            return invalid("query text must be a non-empty string");
        }
        if !(1..=MAX_LIMIT).contains(&limit) {
            return invalid(format!("limit must be between 1 and {MAX_LIMIT}"));
        }
        let limit = limit as usize;
        for (key, value) in filters.into_iter().flatten() {
            if matches!(value, Value::Null | Value::Array(_) | Value::Object(_)) {
                return invalid(format!(
                    "filter {key:?} must be a string, number, or boolean"
                ));
            }
        }
        let filters = filters.filter(|filters| !filters.is_empty());

        let mut state = self.state();
        let State { store, index } = &mut *state;
        index.ensure_current(store)?;
        let subset = match filters {
            Some(filters) => {
                let matching: HashSet<String> =
                    store.matching_record_ids(LOCAL_COLLECTION_ID, filters)?;
                Some(index.internal_ids(&matching))
            }
            None => None,
        };
        let eligible = subset.as_ref().map_or(index.document_count(), Vec::len);
        let mut hits = Vec::new();
        let mut timings = BTreeMap::new();
        let mut gatherer = "none".to_owned();
        if eligible > 0 {
            let gather_limit = self.settings.gather_limit.max(limit);
            let pipeline = index.pipeline(eligible, gather_limit)?;
            let mut request = SearchRequest::new(gather_limit, limit);
            if let Some(subset) = &subset {
                request = request.with_subset(subset);
            }
            let result = pipeline
                .search(&index.query(text), &request)
                .map_err(IndexError::from)?;
            gatherer = pipeline
                .gatherer()
                .score_semantics()
                .split('-')
                .next()
                .unwrap_or_default()
                .to_owned();
            timings.insert(
                "gather_seconds".to_owned(),
                result.timings.gather.as_secs_f64(),
            );
            timings.insert(
                "rerank_seconds".to_owned(),
                result.timings.rerank.as_secs_f64(),
            );
            timings.insert(
                "total_seconds".to_owned(),
                result.timings.total.as_secs_f64(),
            );
            let query_tokens = tokens(text);
            for ranked in &result.documents {
                let position = ranked.document_id as usize;
                let revision = store.get(
                    LOCAL_COLLECTION_ID,
                    &index.record_ids()[position],
                    Some(index.revisions()[position]),
                )?;
                hits.push(Hit {
                    excerpt: excerpt(
                        revision.text.as_deref().unwrap_or_default(),
                        &query_tokens,
                        DEFAULT_WIDTH,
                    ),
                    collection_id: revision.collection_id,
                    id: revision.record_id,
                    revision: revision.revision,
                    score: f64::from(ranked.score),
                    score_semantics: result.diagnostics.score_semantics.clone(),
                    metadata: revision.metadata,
                });
            }
        }
        let logged: Vec<SearchHit> = hits
            .iter()
            .map(|hit| SearchHit {
                record_id: hit.id.clone(),
                revision: hit.revision,
                score: hit.score,
            })
            .collect();
        let encoded_timings: Metadata = timings
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(*value)))
            .collect();
        let search_id = store.log_search(
            LOCAL_COLLECTION_ID,
            text,
            filters,
            &logged,
            &encoded_timings,
            LOCAL_PRINCIPAL_ID,
        )?;
        Ok(SearchOutcome {
            search_id,
            hits,
            timings,
            gatherer,
        })
    }

    pub fn report(
        &self,
        record_id: &str,
        text: &str,
        revision: Option<i64>,
        search_id: Option<&str>,
    ) -> Result<String> {
        if text.trim().is_empty() {
            return invalid("report text must be a non-empty string");
        }
        Ok(self.state().store.add_report(
            LOCAL_COLLECTION_ID,
            record_id,
            revision,
            search_id,
            text,
            LOCAL_PRINCIPAL_ID,
        )?)
    }

    pub fn reports(&self, record_id: &str) -> Result<Vec<Report>> {
        Ok(self
            .state()
            .store
            .reports_for(LOCAL_COLLECTION_ID, record_id)?)
    }

    pub fn health(&self) -> Health {
        Health {
            status: "ok",
            document_count: self.state().index.document_count(),
            encoder: self.encoder_name.clone(),
            gather_limit: self.settings.gather_limit,
        }
    }
}
