//! The protocol operations, independent of transport.
//!
//! Every content operation requires an explicit request context and collection.
//! Revision preconditions are checked again inside the storage transaction.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use crate::store::mutations::{Intent, Mutation};
use lateweave::{Query, SearchRequest};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::analyzer::tokens;
use crate::auth::{AuthError, Operation, RequestContext};
use crate::config::Settings;
use crate::excerpt::{excerpt, DEFAULT_WIDTH};
use crate::gather::Statistics;
use crate::index::{Index, IndexError};
use crate::policy::{self, PolicyError};
use crate::store::{Metadata, RecordSummary, Revision, Store, StoreError};

mod transfer;

pub const MAX_LIMIT: i64 = 100;
/// A search spans at most this many collections, and never more than
/// `PRIORART_MAX_LOADED_INDEXES`.
pub const MAX_SEARCH_COLLECTIONS: usize = 16;
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
    Store(#[from] StoreError),
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
    pub excerpt: String,
    pub metadata: Option<Metadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub document_count: usize,
}

/// A cached collection: its own connection, and its index once a search needs it.
struct State {
    store: Store,
    index: Option<Index>,
}

impl State {
    fn index(&mut self, collection: &str) -> Result<&mut Index> {
        if self.index.is_none() {
            self.index = Some(Index::load(&self.store, collection)?);
        }
        Ok(self.index.as_mut().expect("loaded above"))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WriteOptions<'a> {
    pub idempotency_key: Option<&'a str>,
    /// None writes unconditionally; 0 requires absence; positive values compare revisions.
    pub expected_revision: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeleteOptions<'a> {
    pub expected_revision: Option<i64>,
    pub idempotency_key: Option<&'a str>,
}

pub struct Service {
    settings: Settings,
    states: Mutex<States>,
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
        Store::open(settings.data_dir.join(DATABASE_FILE))?;
        Ok(Self {
            settings,
            states: Mutex::default(),
            _owner: owner,
        })
    }

    pub(crate) fn connect(&self) -> Result<Store, StoreError> {
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

    /// Authorization precedes admission.
    fn state(
        &self,
        context: &RequestContext,
        collection: &str,
        operation: Operation,
    ) -> Result<Arc<Mutex<State>>> {
        let store = self.connect()?;
        policy::collection(&store, context, collection, operation)?;
        self.admit(collection, store)
    }

    /// Each cached collection owns its connection and mutex. Pinned entries
    /// cannot be evicted or replaced while a request uses them.
    fn admit(&self, collection: &str, store: Store) -> Result<Arc<Mutex<State>>> {
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
            states.loaded.insert(
                collection.into(),
                Arc::new(Mutex::new(State { store, index: None })),
            );
        }
        states.lru.retain(|id| id != collection);
        states.lru.push_back(collection.into());
        Ok(states.loaded[collection].clone())
    }

    /// Text beyond `PRIORART_MAX_TOKENS` analyzer terms is cut before storing,
    /// so stored and indexed text match.
    fn cutoff<'a>(&self, text: &'a str) -> &'a str {
        crate::analyzer::prefix(text, self.settings.max_tokens)
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
        let handle = self.state(context, collection_id, Operation::Write)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, index } = &mut *state;
        let intent = intent(
            context,
            collection_id,
            "put",
            options.idempotency_key,
            json!([record_id, text, metadata, options.expected_revision]),
        )?;
        if let Some(result) = store.replay::<(String, i64, bool)>(&intent)? {
            policy::mutation(store, context, collection_id, &result.value.0)?;
            store.get(collection_id, &result.value.0, None)?;
            return Ok(result);
        }
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
        if let Some(record_id) = record_id {
            if options.expected_revision != Some(0)
                && store.record_author(collection_id, record_id)?.is_some()
            {
                policy::mutation(store, context, collection_id, record_id)?;
            }
        }
        let stored = self.cutoff(text);
        let truncated = stored.len() < text.len();
        let created = store.commit_put(
            &intent,
            stored,
            truncated,
            metadata,
            record_id,
            options.expected_revision,
        )?;
        if let Some(index) = index {
            index.upsert(&created.value.0, created.value.1, stored);
        }
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
        Ok(state.store.get(collection_id, record_id, revision)?)
    }

    pub fn delete(
        &self,
        context: &RequestContext,
        collection_id: &str,
        record_id: &str,
        options: DeleteOptions<'_>,
    ) -> Result<Mutation<()>> {
        let handle = self.state(context, collection_id, Operation::Write)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, index } = &mut *state;
        policy::mutation(store, context, collection_id, record_id)?;
        let intent = intent(
            context,
            collection_id,
            "delete",
            options.idempotency_key,
            json!([record_id, options.expected_revision]),
        )?;
        if let Some(result) = store.replay::<()>(&intent)? {
            return Ok(result);
        }
        if options.expected_revision.is_some_and(|r| r <= 0) {
            return invalid("invalid revision precondition");
        }
        let committed = store.commit_delete(&intent, record_id, options.expected_revision)?;
        if let Some(index) = index {
            index.remove(record_id);
        }
        Ok(committed)
    }

    pub fn delete_collection(&self, context: &RequestContext, collection: &str) -> Result<()> {
        let handle = self.state(context, collection, Operation::Admin)?;
        let state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        state.store.purge_collection(collection)?;
        let mut states = self.states.lock().map_err(|_| ServiceError::Poisoned)?;
        states.loaded.remove(collection);
        states.lru.retain(|id| id != collection);
        Ok(())
    }

    /// BM25 over every selected collection with statistics summed across
    /// them, so scores compare; ties go to collection ID, then record ID.
    pub fn search<C: AsRef<str>>(
        &self,
        context: &RequestContext,
        collection_ids: &[C],
        text: &str,
        filters: Option<&Metadata>,
        limit: i64,
    ) -> Result<Vec<Hit>> {
        let mut collection_ids: Vec<&str> = collection_ids.iter().map(AsRef::as_ref).collect();
        let fan_out = MAX_SEARCH_COLLECTIONS.min(self.settings.max_loaded_indexes);
        if !(1..=fan_out).contains(&collection_ids.len()) {
            return invalid(format!("collections must name 1 to {fan_out} collections"));
        }
        collection_ids.sort_unstable();
        if collection_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return invalid("collections must not repeat");
        }
        let authority = self.connect()?;
        let authorize = || {
            collection_ids.iter().try_for_each(|collection| {
                policy::collection(&authority, context, collection, Operation::Read).map(drop)
            })
        };
        authorize()?;
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
        let handles = collection_ids
            .iter()
            .map(|collection| self.admit(collection, self.connect()?))
            .collect::<Result<Vec<_>>>()?;
        // Locked in collection ID order; writers only ever hold one.
        let mut states = handles
            .iter()
            .map(|handle| handle.lock().map_err(|_| ServiceError::Poisoned))
            .collect::<Result<Vec<_>>>()?;
        let query_tokens = tokens(text);
        let mut statistics = Statistics::default();
        for (collection, state) in collection_ids.iter().zip(&mut states) {
            statistics.add(state.index(collection)?.lexical(), &query_tokens);
        }
        let statistics = Arc::new(statistics);
        let query = Query::new(text);
        let mut ranked: Vec<(f32, usize, String, i64)> = Vec::new();
        for (position, (collection, state)) in collection_ids.iter().zip(&mut states).enumerate() {
            let subset = match filters {
                Some(filters) => {
                    let matching: HashSet<String> =
                        state.store.matching_record_ids(collection, filters)?;
                    Some(state.index(collection)?.internal_ids(&matching))
                }
                None => None,
            };
            let index = state.index(collection)?;
            if subset.as_ref().map_or(index.document_count(), Vec::len) == 0 {
                continue;
            }
            let mut request = SearchRequest::new(limit, limit);
            if let Some(subset) = &subset {
                request = request.with_subset(subset);
            }
            let result = index
                .pipeline(statistics.clone())?
                .search(&query, &request)
                .map_err(IndexError::from)?;
            ranked.extend(result.documents.iter().map(|ranked| {
                let (id, revision) = index.document(ranked.document_id);
                (ranked.score, position, id.to_owned(), revision)
            }));
        }
        ranked.sort_unstable_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then(left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        ranked.truncate(limit);
        let hits = ranked
            .into_iter()
            .map(|(score, position, id, revision)| {
                let revision =
                    states[position]
                        .store
                        .get(collection_ids[position], &id, Some(revision))?;
                Ok(Hit {
                    excerpt: excerpt(
                        revision.text.as_deref().unwrap_or_default(),
                        &query_tokens,
                        DEFAULT_WIDTH,
                    ),
                    collection_id: revision.collection_id,
                    id: revision.record_id,
                    revision: revision.revision,
                    score: f64::from(score),
                    metadata: revision.metadata,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        authorize()?;
        Ok(hits)
    }

    pub fn collection(
        &self,
        context: &RequestContext,
        id: &str,
    ) -> Result<crate::store::Collection> {
        let store = self.connect()?;
        let collection = policy::collection(&store, context, id, Operation::Read)?;
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
            .map(|id| policy::collection(&store, context, id, Operation::Read))
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

    /// Scoped diagnostics require admin.
    pub fn health(&self, context: &RequestContext, collection_id: &str) -> Result<Health> {
        let store = self.connect()?;
        policy::collection(&store, context, collection_id, Operation::Admin)?;
        Ok(Health {
            status: "ok",
            document_count: store.live_record_count(collection_id)?,
        })
    }
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

fn intent<'a>(
    context: &'a RequestContext,
    collection: &'a str,
    operation: &'a str,
    key: Option<&'a str>,
    payload: Value,
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
    })
}
