//! The protocol operations, independent of transport.
//!
//! Every content operation requires an explicit request context and collection.
//! HTTP remains a trusted local transport until its authenticated contract lands.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use lateweave::SearchRequest;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::analyzer::tokens;
use crate::auth::{AuthError, Operation, RequestContext};
use crate::config::Settings;
use crate::encoder::{load_encoder, Encoder, EncoderError};
use crate::excerpt::{excerpt, DEFAULT_WIDTH};
use crate::index::{CollectionIndexManager, IndexError};
use crate::policy::{self, PolicyError};
use crate::store::{Metadata, Report, Revision, SearchHit, Store, StoreError, Visibility};

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
    #[error(transparent)]
    Policy(#[from] PolicyError),
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
    pub search_id: Option<String>,
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
    indexes: CollectionIndexManager,
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
        let indexes = CollectionIndexManager::new(
            &settings.data_dir,
            encoder,
            settings.max_loaded_indexes.try_into().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "max_loaded_indexes must be positive",
                )
            })?,
        );
        Ok(Self {
            settings,
            encoder_name,
            state: Mutex::new(State { store, indexes }),
        })
    }

    /// Mint a context; each operation revalidates it against current storage.
    pub fn authenticate(&self, bearer: &str) -> Result<RequestContext, AuthError> {
        self.state().store.authenticate(bearer)
    }

    pub fn validate_context(&self, context: &RequestContext) -> Result<(), AuthError> {
        self.state().store.validate_context(context)
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
        context: &RequestContext,
        collection_id: &str,
        text: &str,
        metadata: Option<&Metadata>,
        record_id: Option<&str>,
        publish: bool,
    ) -> Result<(String, i64)> {
        let mut state = self.state();
        let State { store, indexes } = &mut *state;
        authorize_put(store, context, collection_id, record_id, publish)?;
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
        let index = indexes.get(store, collection_id);
        authorize_put(store, context, collection_id, record_id, publish)?;
        let index = index?;
        let encoded = index.encode(text);
        authorize_put(store, context, collection_id, record_id, publish)?;
        let encoded = encoded?;
        let created = store.put(
            collection_id,
            text,
            metadata,
            record_id,
            context
                .principal_id()
                .expect("mutation policy requires a principal"),
        )?;
        let result = index.upsert(store, &created.record_id, created.revision, text, encoded);
        policy::validate(store, context)?;
        result?;
        Ok((created.record_id, created.revision))
    }

    pub fn get(
        &self,
        context: &RequestContext,
        collection_id: &str,
        record_id: &str,
        revision: Option<i64>,
    ) -> Result<Revision> {
        let state = self.state();
        policy::collection(&state.store, context, collection_id, Operation::Read)?;
        let result = state.store.get(collection_id, record_id, revision);
        policy::collection(&state.store, context, collection_id, Operation::Read)?;
        Ok(result?)
    }

    pub fn delete(
        &self,
        context: &RequestContext,
        collection_id: &str,
        record_id: &str,
    ) -> Result<()> {
        let mut state = self.state();
        let State { store, indexes } = &mut *state;
        policy::mutation(store, context, collection_id, record_id, Operation::Delete)?;
        let index = indexes.get(store, collection_id);
        policy::mutation(store, context, collection_id, record_id, Operation::Delete)?;
        let index = index?;
        if store.delete(collection_id, record_id)? {
            let result = index.remove(store, record_id);
            policy::validate(store, context)?;
            result?;
        }
        policy::validate(store, context)?;
        Ok(())
    }

    pub fn search(
        &self,
        context: &RequestContext,
        collection_id: &str,
        text: &str,
        filters: Option<&Metadata>,
        limit: i64,
    ) -> Result<SearchOutcome> {
        let mut state = self.state();
        let State { store, indexes } = &mut *state;
        policy::collection(store, context, collection_id, Operation::Read)?;
        if text.trim().is_empty() {
            return invalid("query text must be a non-empty string");
        }
        if !(1..=MAX_LIMIT).contains(&limit) {
            return invalid(format!("limit must be between 1 and {MAX_LIMIT}"));
        }
        let limit = limit as usize;
        for value in filters.into_iter().flatten().map(|(_, value)| value) {
            if matches!(value, Value::Null | Value::Array(_) | Value::Object(_)) {
                return invalid("filter values must be strings, numbers, or booleans");
            }
        }
        let filters = filters.filter(|filters| !filters.is_empty());

        let index = indexes.get(store, collection_id);
        policy::collection(store, context, collection_id, Operation::Read)?;
        let index = index?;
        let subset = match filters {
            Some(filters) => {
                let matching: HashSet<String> =
                    store.matching_record_ids(collection_id, filters)?;
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
            let result = pipeline.search(&index.query(text), &request);
            policy::collection(store, context, collection_id, Operation::Read)?;
            let result = result.map_err(IndexError::from)?;
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
                    collection_id,
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
        policy::collection(store, context, collection_id, Operation::Read)?;
        let search_id = context
            .principal_id()
            .map(|principal| {
                store.log_search(
                    collection_id,
                    text,
                    filters,
                    &logged,
                    &encoded_timings,
                    principal,
                )
            })
            .transpose()?;
        policy::collection(store, context, collection_id, Operation::Read)?;
        Ok(SearchOutcome {
            search_id,
            hits,
            timings,
            gatherer,
        })
    }

    pub fn report(
        &self,
        context: &RequestContext,
        collection_id: &str,
        record_id: &str,
        text: &str,
        revision: Option<i64>,
        search_id: Option<&str>,
    ) -> Result<String> {
        let state = self.state();
        let store = &state.store;
        policy::collection(store, context, collection_id, Operation::Report)?;
        policy::collection(store, context, collection_id, Operation::Read)?;
        if text.trim().is_empty() {
            return invalid("report text must be a non-empty string");
        }
        let principal = context
            .principal_id()
            .expect("report policy requires a principal");
        let target = store.get(collection_id, record_id, revision)?;
        if let Some(search_id) = search_id {
            if !store.search_owned_by(collection_id, search_id, principal)? {
                return Err(StoreError::SearchNotFound {
                    collection_id: collection_id.to_owned(),
                    search_id: search_id.to_owned(),
                }
                .into());
            }
        }
        policy::collection(store, context, collection_id, Operation::Report)?;
        policy::collection(store, context, collection_id, Operation::Read)?;
        let id = store.add_report(
            collection_id,
            record_id,
            Some(target.revision),
            search_id,
            text,
            principal,
        )?;
        policy::validate(store, context)?;
        Ok(id)
    }

    pub fn reports(
        &self,
        context: &RequestContext,
        collection_id: &str,
        record_id: &str,
    ) -> Result<Vec<Report>> {
        let state = self.state();
        let store = &state.store;
        policy::collection(store, context, collection_id, Operation::FeedbackRead)?;
        policy::collection(store, context, collection_id, Operation::Read)?;
        // A tombstone does not expose reports that may quote deleted content.
        store.get(collection_id, record_id, None)?;
        let principal = context
            .principal_id()
            .expect("feedback policy requires a principal");
        let reports = store.reports_for_principal(collection_id, record_id, principal)?;
        policy::collection(store, context, collection_id, Operation::FeedbackRead)?;
        policy::collection(store, context, collection_id, Operation::Read)?;
        Ok(reports)
    }

    /// Scoped diagnostics require admin, including before index loading/recovery.
    pub fn health(&self, context: &RequestContext, collection_id: &str) -> Result<Health> {
        let mut state = self.state();
        let State { store, indexes } = &mut *state;
        policy::collection(store, context, collection_id, Operation::Admin)?;
        let index = indexes.get(store, collection_id);
        policy::collection(store, context, collection_id, Operation::Admin)?;
        let document_count = index?.document_count();
        Ok(Health {
            status: "ok",
            document_count,
            encoder: self.encoder_name.clone(),
            gather_limit: self.settings.gather_limit,
        })
    }
}

fn authorize_put(
    store: &Store,
    context: &RequestContext,
    collection_id: &str,
    record_id: Option<&str>,
    publish: bool,
) -> Result<()> {
    // Check some write authority before examining record existence/authorship.
    let collection = match policy::collection(store, context, collection_id, Operation::Contribute)
    {
        Err(PolicyError::Unavailable) => {
            policy::collection(store, context, collection_id, Operation::Update)?
        }
        result => result?,
    };
    let existing = record_id
        .map(|id| store.record_author(collection_id, id))
        .transpose()?
        .flatten();
    if existing.is_some() {
        policy::mutation(
            store,
            context,
            collection_id,
            record_id.expect("existing record has an ID"),
            Operation::Update,
        )?;
    } else {
        policy::collection(store, context, collection_id, Operation::Contribute)?;
    }
    if collection.visibility == Visibility::Public && !publish {
        return invalid("public writes require explicit publication intent");
    }
    Ok(())
}
