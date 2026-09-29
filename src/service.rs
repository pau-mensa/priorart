//! The protocol operations, independent of transport.
//!
//! Every content operation requires an explicit request context and collection.
//! Revision preconditions are checked again inside the storage transaction.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use crate::store::mutations::{Intent, Mutation};
use lateweave::SearchRequest;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::analyzer::tokens;
use crate::auth::{AuthError, Operation, RequestContext};
use crate::config::Settings;
use crate::encoder::{load_encoder, Encoder, EncoderError};
use crate::excerpt::{excerpt, DEFAULT_WIDTH};
use crate::index::{CollectionIndexManager, IndexError};
use crate::policy::{self, PolicyError};
use crate::store::{Metadata, RecordSummary, Revision, Store, StoreError, Visibility};

mod transfer;
pub use transfer::ImportOptions;

pub const MAX_LIMIT: i64 = 100;
pub const DATABASE_FILE: &str = "priorart.sqlite";

static RECORD_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.:-]{1,128}$").expect("valid record ID regex"));

pub const RECORD_ID_RULE: &str = "id must match ^[A-Za-z0-9_.:-]{1,128}$ and not be . or ..";

/// Whether `id` can name a record. `.` and `..` are excluded because URL
/// parsers resolve them as path segments, so `/v1/collections/{collection}/records/{id}` cannot reach them.
pub fn is_record_id(id: &str) -> bool {
    RECORD_ID.is_match(id) && id != "." && id != ".."
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("all collection execution slots are busy")]
    Busy,
    #[error("collection state requires a restart after a panic")]
    Poisoned,
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
    Config(#[from] crate::config::ConfigError),
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

#[derive(Clone, Copy, Debug, Default)]
pub struct WriteOptions<'a> {
    pub idempotency_key: Option<&'a str>,
    pub publish: bool,
    /// None creates a new ID; 0 explicitly requires absence; positive values compare revisions.
    pub expected_revision: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeleteOptions<'a> {
    pub expected_revision: Option<i64>,
    pub idempotency_key: Option<&'a str>,
}

pub struct Service {
    settings: Settings,
    encoder_name: Option<String>,
    states: Mutex<States>,
    encoder: Option<Arc<dyn Encoder>>,
    // Declared last so collection state is dropped before releasing ownership.
    _owner: crate::ownership::DirectoryOwner,
}

#[derive(Default)]
struct States {
    loaded: HashMap<String, Arc<Mutex<State>>>,
    lru: VecDeque<String>,
}

impl Service {
    pub fn open(settings: Settings) -> Result<Self, OpenError> {
        settings.validate()?;
        let owner = crate::ownership::DirectoryOwner::acquire(&settings.data_dir)?;
        let encoder = load_encoder(&settings)?;
        Self::owned(settings, encoder, owner)
    }

    pub fn new(settings: Settings, encoder: Option<Arc<dyn Encoder>>) -> Result<Self, OpenError> {
        settings.validate()?;
        let owner = crate::ownership::DirectoryOwner::acquire(&settings.data_dir)?;
        Self::owned(settings, encoder, owner)
    }

    fn owned(
        settings: Settings,
        encoder: Option<Arc<dyn Encoder>>,
        owner: crate::ownership::DirectoryOwner,
    ) -> Result<Self, OpenError> {
        let store = Store::open(settings.data_dir.join(DATABASE_FILE))?;
        let mut after = String::new();
        loop {
            let pending = store.pending_purges(&after)?;
            if pending.is_empty() {
                break;
            }
            for collection in pending {
                crate::index::purge_index_files(&settings.data_dir, &store, &collection)?;
                after = collection;
            }
        }
        let encoder_name = encoder
            .as_ref()
            .map(|e| e.representation().encoder().to_owned());
        Ok(Self {
            settings,
            encoder_name,
            encoder,
            states: Mutex::default(),
            _owner: owner,
        })
    }

    fn connect(&self) -> Result<Store, StoreError> {
        Store::connect(&self.settings.data_dir.join(DATABASE_FILE))
    }

    /// Mint a context; each operation revalidates it against current storage.
    pub fn authenticate(&self, bearer: &str) -> Result<RequestContext, AuthError> {
        self.connect()?.authenticate(bearer)
    }

    pub fn validate_context(&self, context: &RequestContext) -> Result<(), AuthError> {
        self.connect()?.validate_context(context)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Authorization precedes admission. Each cached collection owns its connection
    /// and mutex. Pinned entries cannot be evicted or replaced while a request uses them.
    fn state(
        &self,
        context: &RequestContext,
        collection: &str,
        operation: Operation,
    ) -> Result<Arc<Mutex<State>>> {
        let store = self.connect()?;
        match policy::collection(&store, context, collection, operation) {
            Err(PolicyError::Unavailable) if operation == Operation::Contribute => {
                policy::collection(&store, context, collection, Operation::Update)?;
            }
            result => {
                result?;
            }
        }
        let mut states = self.states.lock().map_err(|_| ServiceError::Poisoned)?;
        if !states.loaded.contains_key(collection) {
            if states.loaded.len() == self.settings.max_loaded_indexes {
                let idle = states
                    .lru
                    .iter()
                    .position(|id| Arc::strong_count(&states.loaded[id]) == 1)
                    .ok_or(ServiceError::Busy)?;
                let id = states.lru.remove(idle).expect("existing LRU entry");
                states.loaded.remove(&id);
            }
            let indexes = CollectionIndexManager::new(
                &self.settings.data_dir,
                self.encoder.clone(),
                std::num::NonZeroUsize::new(1).unwrap(),
            );
            states.loaded.insert(
                collection.into(),
                Arc::new(Mutex::new(State { store, indexes })),
            );
        }
        states.lru.retain(|id| id != collection);
        states.lru.push_back(collection.into());
        Ok(states.loaded[collection].clone())
    }

    /// Text beyond `PRIORART_MAX_TOKENS` is cut before storing, so stored and
    /// indexed text match. Lexical-only operation counts analyzer terms.
    fn cutoff<'a>(&self, text: &'a str) -> Result<&'a str> {
        match &self.encoder {
            Some(encoder) => Ok(encoder.fit_document(text).map_err(IndexError::from)?),
            None => Ok(crate::analyzer::prefix(text, self.settings.max_tokens)),
        }
    }

    pub fn put(
        &self,
        context: &RequestContext,
        collection_id: &str,
        text: &str,
        metadata: Option<&Metadata>,
        record_id: Option<&str>,
        options: WriteOptions<'_>,
    ) -> Result<Mutation<(String, i64, bool)>> {
        let handle = self.state(context, collection_id, Operation::Contribute)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, indexes } = &mut *state;
        let intent = intent(
            context,
            collection_id,
            "put",
            options.idempotency_key,
            json!([
                record_id,
                text,
                metadata,
                options.publish,
                options.expected_revision
            ]),
            if record_id
                .map(|id| store.record_author(collection_id, id))
                .transpose()?
                .flatten()
                .is_some()
            {
                "update"
            } else {
                "contribute"
            },
        )?;
        if let Some((result, authority)) = store.replay::<(String, i64, bool)>(&intent)? {
            let operation = if authority == "contribute" {
                Operation::Contribute
            } else {
                Operation::Update
            };
            policy::mutation(store, context, collection_id, &result.value.0, operation)?;
            store.get(collection_id, &result.value.0, None)?;
            indexes.get(store, collection_id)?;
            policy::validate(store, context)?;
            return Ok(result);
        }
        authorize_put(store, context, collection_id, record_id, options)?;
        if options.expected_revision.is_some_and(|r| r < 0)
            || (record_id.is_none() && options.expected_revision.is_some_and(|r| r > 0))
        {
            return invalid("invalid revision precondition");
        }
        validate_metadata(metadata, 65_536)?;
        if text.trim().is_empty() {
            return invalid("text must be a non-empty string");
        }
        if record_id.is_some_and(|record_id| !is_record_id(record_id)) {
            return invalid(RECORD_ID_RULE);
        }
        let stored = self.cutoff(text)?;
        let truncated = stored.len() < text.len();
        store.check_put_revision(collection_id, record_id, options.expected_revision)?;
        let index = indexes.get(store, collection_id);
        authorize_put(store, context, collection_id, record_id, options)?;
        let index = index?;
        let encoded = index.encode(stored);
        authorize_put(store, context, collection_id, record_id, options)?;
        let encoded = encoded?;
        let created = store.commit_put(
            &intent,
            stored,
            truncated,
            metadata,
            record_id,
            options.expected_revision,
        )?;
        crate::fault::check("after_record_commit").map_err(IndexError::from)?;
        let result = index.upsert(store, &created.value.0, created.value.1, stored, encoded);
        policy::validate(store, context)?;
        result?;
        crate::fault::check("after_mutation_complete").map_err(IndexError::from)?;
        Ok(created)
    }

    pub fn get(
        &self,
        context: &RequestContext,
        collection_id: &str,
        record_id: &str,
        revision: Option<i64>,
    ) -> Result<Revision> {
        let handle = self.state(context, collection_id, Operation::Read)?;
        let state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
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
        options: DeleteOptions<'_>,
    ) -> Result<Mutation<()>> {
        let handle = self.state(context, collection_id, Operation::Delete)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, indexes } = &mut *state;
        policy::mutation(store, context, collection_id, record_id, Operation::Delete)?;
        let intent = intent(
            context,
            collection_id,
            "delete",
            options.idempotency_key,
            json!([record_id, options.expected_revision]),
            "delete",
        )?;
        if let Some((result, _)) = store.replay::<()>(&intent)? {
            indexes.get(store, collection_id)?;
            policy::validate(store, context)?;
            return Ok(result);
        }
        if options.expected_revision.is_some_and(|r| r <= 0) {
            return invalid("invalid revision precondition");
        }
        store.check_delete_revision(collection_id, record_id, options.expected_revision)?;
        policy::mutation(store, context, collection_id, record_id, Operation::Delete)?;
        let committed = store.commit_delete(&intent, record_id, options.expected_revision)?;
        crate::fault::check("after_record_commit").map_err(IndexError::from)?;
        indexes.get(store, collection_id)?;
        policy::validate(store, context)?;
        store.complete_mutations(collection_id)?;
        crate::fault::check("after_mutation_complete").map_err(IndexError::from)?;
        Ok(committed)
    }

    pub fn delete_collection(&self, context: &RequestContext, collection: &str) -> Result<()> {
        let handle = self.state(context, collection, Operation::Admin)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, indexes } = &mut *state;
        policy::collection(store, context, collection, Operation::Admin)?;
        store.purge_collection(collection)?;
        crate::fault::check("after_collection_purge_commit").map_err(IndexError::from)?;
        indexes.finish_purge(store, collection)?;
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
        let handle = self.state(context, collection_id, Operation::Read)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, indexes } = &mut *state;
        policy::collection(store, context, collection_id, Operation::Read)?;
        if text.trim().is_empty() {
            return invalid("query text must be a non-empty string");
        }
        if !(1..=MAX_LIMIT).contains(&limit) {
            return invalid(format!("limit must be between 1 and {MAX_LIMIT}"));
        }
        if text.len() > 16_384 {
            return invalid("query is too large");
        }
        validate_metadata(filters, 16_384)?;
        if filters.is_some_and(|f| f.len() > 64) {
            return invalid("too many filters");
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
        policy::collection(store, context, collection_id, Operation::Read)?;
        Ok(SearchOutcome {
            hits,
            timings,
            gatherer,
        })
    }

    pub fn collection(
        &self,
        context: &RequestContext,
        id: &str,
    ) -> Result<crate::store::Collection> {
        let store = self.connect()?;
        let collection = inspect_collection(&store, context, id)?;
        policy::validate(&store, context)?;
        Ok(collection)
    }

    pub fn collections(
        &self,
        context: &RequestContext,
        after: &str,
        limit: i64,
    ) -> Result<Vec<crate::store::Collection>> {
        let store = self.connect()?;
        policy::validate(&store, context)?;
        if !(1..=100).contains(&limit) || (!after.is_empty() && !is_record_id(after)) {
            return invalid("invalid collection pagination");
        }
        let result = store
            .visible_collections(context, after, limit)?
            .iter()
            .map(|id| inspect_collection(&store, context, id))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        policy::validate(&store, context)?;
        Ok(result)
    }

    /// Live records of a readable collection in ID order. `mine` keeps only the
    /// caller principal's records, so it needs an identity; keys are irrelevant.
    pub fn records(
        &self,
        context: &RequestContext,
        collection_id: &str,
        mine: bool,
        after: &str,
        limit: i64,
        include_text: bool,
    ) -> Result<Vec<RecordSummary>> {
        let store = self.connect()?;
        policy::collection(&store, context, collection_id, Operation::Read)?;
        if !(1..=MAX_LIMIT).contains(&limit) || (!after.is_empty() && !is_record_id(after)) {
            return invalid("invalid record pagination");
        }
        let author = match (mine, context.principal_id()) {
            (false, _) => None,
            (true, Some(principal)) => Some(principal),
            (true, None) => return invalid("author=me requires a credential"),
        };
        let result = store.records(collection_id, author, after, limit, include_text)?;
        policy::collection(&store, context, collection_id, Operation::Read)?;
        Ok(result)
    }

    /// Scoped diagnostics require admin, including before index loading/recovery.
    pub fn health(&self, context: &RequestContext, collection_id: &str) -> Result<Health> {
        let handle = self.state(context, collection_id, Operation::Admin)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
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
    options: WriteOptions<'_>,
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
    if existing.is_some() && options.expected_revision != Some(0) {
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
    if collection.visibility == Visibility::Public && !options.publish {
        return invalid("public writes require explicit publication intent");
    }
    Ok(())
}

fn validate_metadata(metadata: Option<&Metadata>, max_bytes: usize) -> Result<()> {
    fn visit(value: &Value, depth: usize, nodes: &mut usize) -> bool {
        *nodes += 1;
        if depth > 8 || *nodes > 1024 {
            return false;
        }
        match value {
            Value::Array(values) => values.iter().all(|v| visit(v, depth + 1, nodes)),
            Value::Object(values) => values.values().all(|v| visit(v, depth + 1, nodes)),
            _ => true,
        }
    }
    if let Some(metadata) = metadata {
        if serde_json::to_vec(metadata).map_or(true, |v| v.len() > max_bytes)
            || !visit(&Value::Object(metadata.clone()), 0, &mut 0)
        {
            return invalid("metadata exceeds resource limits");
        }
    }
    Ok(())
}

fn inspect_collection(
    store: &Store,
    context: &RequestContext,
    id: &str,
) -> policy::Result<crate::store::Collection> {
    match policy::collection(store, context, id, Operation::Read) {
        Err(PolicyError::Unavailable) => policy::collection(store, context, id, Operation::Admin),
        result => result,
    }
}

fn intent<'a>(
    context: &'a RequestContext,
    collection: &'a str,
    operation: &'a str,
    key: Option<&'a str>,
    payload: Value,
    authority: &'a str,
) -> Result<Intent<'a>> {
    if key.is_some_and(|key| {
        key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    }) {
        return invalid("invalid idempotency key");
    }
    let payload = Sha256::digest(serde_json::to_vec(&payload).expect("JSON value is serializable"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Intent {
        collection,
        principal: context.principal_id().ok_or(PolicyError::Unavailable)?,
        operation,
        key,
        payload,
        authority,
    })
}

#[cfg(test)]
mod recovery_tests;
