//! The opt-in search log (`PRIORART_SEARCH_LOG_DAYS`): each search with the
//! hits it returned, and the searcher's explicit ratings of those hits. It has
//! its own SQLite file, stores record IDs and revisions rather than text, and
//! identifies principals only by a salted hash.
//!
//! Requests never wait on it: they hand messages to one writer thread through a
//! bounded queue, and whatever cannot be queued or written is dropped and
//! counted. One writer applying messages in order means a rating always finds
//! its search.
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::service::Hit;
use crate::store::{timestamp, Metadata, SchemaError, StoreError};

pub const FILE: &str = "searchlog.sqlite";

const SCHEMA: &str = "
CREATE TABLE log (salt TEXT NOT NULL);
CREATE TABLE searches (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    at TEXT NOT NULL,
    principal TEXT,
    collections TEXT NOT NULL,
    query TEXT NOT NULL,
    filters TEXT,
    result_limit INTEGER NOT NULL
);
CREATE INDEX searches_at ON searches (at);
CREATE TABLE hits (
    search_seq INTEGER NOT NULL REFERENCES searches (seq) ON DELETE CASCADE,
    rank INTEGER NOT NULL,
    collection_id TEXT NOT NULL,
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    score REAL NOT NULL,
    PRIMARY KEY (search_seq, rank)
);
CREATE TABLE feedback (
    search_seq INTEGER NOT NULL REFERENCES searches (seq) ON DELETE CASCADE,
    collection_id TEXT NOT NULL,
    record_id TEXT NOT NULL,
    useful INTEGER NOT NULL,
    at TEXT NOT NULL,
    PRIMARY KEY (search_seq, collection_id, record_id)
);
PRAGMA user_version = 1;
";

const QUEUE: usize = 1024;
const BATCH: usize = 256;
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rating {
    pub collection_id: String,
    pub id: String,
    pub useful: bool,
}

struct Search {
    id: String,
    at: String,
    principal: Option<String>,
    collections: Vec<String>,
    query: String,
    filters: Option<Metadata>,
    limit: i64,
    hits: Vec<(String, String, i64, f64)>,
}

enum Message {
    Search(Search),
    Feedback {
        principal: String,
        search_id: String,
        ratings: Vec<Rating>,
        at: String,
    },
    Forget(String),
}

impl Message {
    /// How many entries are lost if this message is.
    fn entries(&self) -> u64 {
        match self {
            Self::Search(_) | Self::Forget(_) => 1,
            Self::Feedback { ratings, .. } => ratings.len() as u64,
        }
    }
}

#[derive(Default)]
struct Dropped {
    queue_full: AtomicU64,
    write_failed: AtomicU64,
    rejected_feedback: AtomicU64,
}

pub struct SearchLog {
    queue: Option<SyncSender<Message>>,
    writer: Option<JoinHandle<()>>,
    reader: Mutex<Connection>,
    dropped: Arc<Dropped>,
}

impl SearchLog {
    pub fn open(path: &Path, days: u32) -> Result<Self, StoreError> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                let transaction = connection.transaction()?;
                transaction.execute_batch(SCHEMA)?;
                transaction.execute(
                    "INSERT INTO log (salt) VALUES (?1)",
                    [uuid::Uuid::new_v4().simple().to_string()],
                )?;
                transaction.commit()?;
            }
            1 => {}
            _ => {
                return Err(SchemaError::Unrecognized(format!(
                    "{FILE} has search log schema {version}, which this build does not know"
                ))
                .into())
            }
        }
        let retention = time::Duration::days(days.into());
        prune(&connection, retention)?;
        let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.busy_timeout(Duration::from_secs(5))?;
        let writer = Writer {
            salt: connection.query_row("SELECT salt FROM log", [], |r| r.get(0))?,
            connection,
            retention,
            dropped: Arc::default(),
        };
        let dropped = writer.dropped.clone();
        let (queue, receiver) = mpsc::sync_channel(QUEUE);
        let writer = std::thread::Builder::new()
            .name("priorart-search-log".into())
            .spawn(move || writer.run(receiver))?;
        Ok(Self {
            queue: Some(queue),
            writer: Some(writer),
            reader: Mutex::new(reader),
            dropped,
        })
    }

    fn offer(&self, message: Message) -> bool {
        let entries = message.entries();
        let counter = match self.queue.as_ref().map(|queue| queue.try_send(message)) {
            Some(Ok(())) => return true,
            Some(Err(TrySendError::Full(_))) => &self.dropped.queue_full,
            _ => &self.dropped.write_failed,
        };
        counter.fetch_add(entries, Ordering::Relaxed);
        false
    }

    /// Queues a completed search and returns its ID, or None if it was dropped.
    pub fn record(
        &self,
        principal: Option<&str>,
        collections: &[&str],
        query: &str,
        filters: Option<&Metadata>,
        limit: i64,
        hits: &[Hit],
    ) -> Option<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let search = Search {
            id: id.clone(),
            at: timestamp(OffsetDateTime::now_utc()),
            principal: principal.map(str::to_owned),
            collections: collections.iter().map(|c| c.to_string()).collect(),
            query: query.to_owned(),
            filters: filters.cloned(),
            limit,
            hits: hits
                .iter()
                .map(|hit| {
                    (
                        hit.collection_id.clone(),
                        hit.id.clone(),
                        hit.revision,
                        hit.score,
                    )
                })
                .collect(),
        };
        self.offer(Message::Search(search)).then_some(id)
    }

    /// Queues ratings. The writer keeps only those naming a hit of a search by
    /// the same principal; a later rating of a hit replaces the earlier one.
    pub fn rate(&self, principal: &str, search_id: &str, ratings: Vec<Rating>) {
        self.offer(Message::Feedback {
            principal: principal.to_owned(),
            search_id: search_id.to_owned(),
            ratings,
            at: timestamp(OffsetDateTime::now_utc()),
        });
    }

    /// Drops every search that included `collection`, after any still queued.
    /// Waits for room in the queue rather than losing the deletion.
    pub fn forget_collection(&self, collection: &str) {
        let sent = self
            .queue
            .as_ref()
            .is_some_and(|queue| queue.send(Message::Forget(collection.to_owned())).is_ok());
        if !sent {
            self.dropped.write_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dropped(&self) -> [(&'static str, u64); 3] {
        let count = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        [
            ("queue_full", count(&self.dropped.queue_full)),
            ("write_failed", count(&self.dropped.write_failed)),
            ("rejected_feedback", count(&self.dropped.rejected_feedback)),
        ]
    }

    /// Up to `limit` written searches after cursor `after`, oldest first, with
    /// their hits and feedback, and the cursor for the next page if there is one.
    pub fn export(&self, after: i64, limit: i64) -> rusqlite::Result<(Vec<Value>, Option<i64>)> {
        let connection = self.reader.lock().unwrap_or_else(PoisonError::into_inner);
        let mut searches = connection.prepare(
            "SELECT seq, id, at, principal, collections, query, filters, result_limit
             FROM searches WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let mut hits = connection.prepare(
            "SELECT collection_id, record_id, revision, score FROM hits
             WHERE search_seq = ?1 ORDER BY rank",
        )?;
        let mut feedback = connection.prepare(
            "SELECT collection_id, record_id, useful, at FROM feedback
             WHERE search_seq = ?1 ORDER BY collection_id, record_id",
        )?;
        let parse = |text: Option<String>| {
            text.and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or(Value::Null)
        };
        let mut rows = Vec::new();
        let mut query = searches.query(params![after, limit + 1])?;
        while let Some(row) = query.next()? {
            let seq: i64 = row.get(0)?;
            let hits = hits
                .query_map([seq], |r| {
                    Ok(json!({"collection_id": r.get::<_, String>(0)?, "id": r.get::<_, String>(1)?,
                              "revision": r.get::<_, i64>(2)?, "score": r.get::<_, f64>(3)?}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let feedback = feedback
                .query_map([seq], |r| {
                    Ok(json!({"collection_id": r.get::<_, String>(0)?, "id": r.get::<_, String>(1)?,
                              "useful": r.get::<_, bool>(2)?, "at": r.get::<_, String>(3)?}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.push((
                seq,
                json!({
                    "type": "search",
                    "id": row.get::<_, String>(1)?,
                    "at": row.get::<_, String>(2)?,
                    "principal": row.get::<_, Option<String>>(3)?,
                    "collections": parse(row.get(4)?),
                    "query": row.get::<_, String>(5)?,
                    "filters": parse(row.get(6)?),
                    "limit": row.get::<_, i64>(7)?,
                    "hits": hits,
                    "feedback": feedback,
                }),
            ));
        }
        let next = (rows.len() as i64 > limit).then(|| {
            rows.pop();
            rows.last().map_or(after, |(seq, _)| *seq)
        });
        Ok((rows.into_iter().map(|(_, row)| row).collect(), next))
    }
}

/// Closing the queue lets the writer finish what is queued; the drop waits for it.
impl Drop for SearchLog {
    fn drop(&mut self) {
        self.queue.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

struct Writer {
    connection: Connection,
    salt: String,
    retention: time::Duration,
    dropped: Arc<Dropped>,
}

impl Writer {
    fn run(mut self, receiver: Receiver<Message>) {
        let mut next_prune = Instant::now() + PRUNE_EVERY;
        loop {
            let first =
                match receiver.recv_timeout(next_prune.saturating_duration_since(Instant::now())) {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Err(error) = prune(&self.connection, self.retention) {
                            eprintln!("priorart: search log retention failed: {error}");
                        }
                        next_prune = Instant::now() + PRUNE_EVERY;
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                };
            let mut batch = vec![first];
            while batch.len() < BATCH {
                match receiver.try_recv() {
                    Ok(message) => batch.push(message),
                    Err(_) => break,
                }
            }
            if let Err(error) = self.write(&batch) {
                let lost: u64 = batch.iter().map(Message::entries).sum();
                self.dropped.write_failed.fetch_add(lost, Ordering::Relaxed);
                eprintln!("priorart: search log dropped {lost} entries: {error}");
            }
        }
    }

    fn write(&mut self, batch: &[Message]) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        let mut rejected = 0;
        for message in batch {
            match message {
                Message::Search(search) => {
                    transaction.execute(
                        "INSERT INTO searches (id, at, principal, collections, query, filters, result_limit)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            search.id,
                            search.at,
                            search.principal.as_deref().map(|p| pseudonym(&self.salt, p)),
                            json!(search.collections).to_string(),
                            search.query,
                            search.filters.as_ref().map(|f| Value::Object(f.clone()).to_string()),
                            search.limit,
                        ],
                    )?;
                    let seq = transaction.last_insert_rowid();
                    let mut insert = transaction.prepare_cached(
                        "INSERT INTO hits (search_seq, rank, collection_id, record_id, revision, score)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )?;
                    for (rank, (collection, id, revision, score)) in search.hits.iter().enumerate()
                    {
                        insert.execute(params![
                            seq,
                            rank as i64 + 1,
                            collection,
                            id,
                            revision,
                            score
                        ])?;
                    }
                }
                Message::Feedback {
                    principal,
                    search_id,
                    ratings,
                    at,
                } => {
                    let seq: Option<i64> = transaction
                        .query_row(
                            "SELECT seq FROM searches WHERE id = ?1 AND principal = ?2",
                            params![search_id, pseudonym(&self.salt, principal)],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let Some(seq) = seq else {
                        rejected += ratings.len() as u64;
                        continue;
                    };
                    for rating in ratings {
                        let applied = transaction.execute(
                            "INSERT INTO feedback (search_seq, collection_id, record_id, useful, at)
                             SELECT ?1, ?2, ?3, ?4, ?5 WHERE EXISTS (SELECT 1 FROM hits
                                 WHERE search_seq = ?1 AND collection_id = ?2 AND record_id = ?3)
                             ON CONFLICT (search_seq, collection_id, record_id)
                             DO UPDATE SET useful = excluded.useful, at = excluded.at",
                            params![seq, rating.collection_id, rating.id, rating.useful, at],
                        )?;
                        if applied == 0 {
                            rejected += 1;
                        }
                    }
                }
                Message::Forget(collection) => {
                    transaction.execute(
                        "DELETE FROM searches WHERE EXISTS
                         (SELECT 1 FROM json_each(searches.collections) WHERE value = ?1)",
                        [collection],
                    )?;
                }
            }
        }
        transaction.commit()?;
        self.dropped
            .rejected_feedback
            .fetch_add(rejected, Ordering::Relaxed);
        Ok(())
    }
}

/// Deletes searches older than the retention, with their hits and feedback.
fn prune(connection: &Connection, retention: time::Duration) -> rusqlite::Result<()> {
    let cutoff = timestamp(OffsetDateTime::now_utc() - retention);
    connection.execute("DELETE FROM searches WHERE at < ?1", [cutoff])?;
    Ok(())
}

fn pseudonym(salt: &str, principal: &str) -> String {
    Sha256::digest(format!("{salt}:{principal}"))
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}
