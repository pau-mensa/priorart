//! Collection-scoped SQLite persistence.
//!
//! Every content operation takes an explicit collection. This is a persistence
//! boundary, not authorization: trusted local callers supply principal IDs
//! until service policy exists.

pub mod migrations;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

pub use migrations::{SchemaError, SCHEMA_VERSION};

pub const LOCAL_COLLECTION_ID: &str = "local";
pub const LOCAL_ACCOUNT_ID: &str = "local-account";
pub const LOCAL_PRINCIPAL_ID: &str = "local-principal";

pub type Metadata = Map<String, Value>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no record {record_id}{} in collection {collection_id}", revision.map(|r| format!(" revision {r}")).unwrap_or_default())]
    RecordNotFound {
        collection_id: String,
        record_id: String,
        revision: Option<i64>,
    },
    #[error("record {record_id} in collection {collection_id} was deleted")]
    RecordDeleted {
        collection_id: String,
        record_id: String,
    },
    #[error("no search {search_id} in collection {collection_id}")]
    SearchNotFound {
        collection_id: String,
        search_id: String,
    },
    #[error("no collection {0}")]
    CollectionNotFound(String),
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Restricted,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Restricted => "restricted",
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Visibility {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "public" => Ok(Self::Public),
            "restricted" => Ok(Self::Restricted),
            other => Err(format!("unknown visibility {other:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub id: String,
    pub owner_account_id: String,
    pub visibility: Visibility,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordRef {
    pub collection_id: String,
    pub record_id: String,
    pub revision: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Revision {
    pub collection_id: String,
    pub record_id: String,
    pub revision: i64,
    pub text: Option<String>,
    pub metadata: Option<Metadata>,
    pub text_sha256: String,
    pub created_at: String,
    pub author_principal_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub collection_id: String,
    pub id: String,
    pub record_id: String,
    pub revision: Option<i64>,
    pub search_id: Option<String>,
    pub text: String,
    pub created_at: String,
    pub reporter_principal_id: Option<String>,
}

/// One logged hit; its collection is the logged search's collection.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub record_id: String,
    pub revision: i64,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub internal_id: u64,
    pub record_id: String,
    pub revision: i64,
}

const TIMESTAMP: &[FormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z");

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(TIMESTAMP)
        .expect("UTC timestamps always format")
}

fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn encode(value: &Metadata) -> String {
    serde_json::to_string(value).expect("JSON objects always serialize")
}

fn decode(encoded: Option<String>) -> rusqlite::Result<Option<Metadata>> {
    encoded
        .map(|text| {
            serde_json::from_str(&text).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })
        })
        .transpose()
}

/// JSON equality with integers and floats compared numerically.
fn scalar_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
            (Some(left), Some(right)) => left == right,
            _ => left.as_f64() == right.as_f64(),
        },
        _ => left == right,
    }
}

const REVISION_COLUMNS: &str = "r.collection_id, r.record_id, r.revision, r.text, r.metadata, \
     r.text_sha256, r.created_at, rec.author_principal_id FROM revisions r JOIN records rec \
     ON rec.collection_id = r.collection_id AND rec.id = r.record_id";

fn revision_row(row: &Row<'_>) -> rusqlite::Result<Revision> {
    Ok(Revision {
        collection_id: row.get(0)?,
        record_id: row.get(1)?,
        revision: row.get(2)?,
        text: row.get(3)?,
        metadata: decode(row.get(4)?)?,
        text_sha256: row.get(5)?,
        created_at: row.get(6)?,
        author_principal_id: row.get(7)?,
    })
}

pub struct Store {
    path: PathBuf,
    connection: Connection,
}

impl Store {
    /// Opens or creates the database, applying pending migrations first.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        migrations::migrate(&connection)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.connection,
            TransactionBehavior::Immediate,
        )?)
    }

    // Identities: a trusted local persistence API, not remote provisioning.

    pub fn create_principal(&self) -> Result<String> {
        let id = new_id();
        self.connection
            .execute("INSERT INTO principals VALUES (?1, ?2)", (&id, now()))?;
        Ok(id)
    }

    pub fn create_account(&self, owner_principal_id: &str) -> Result<String> {
        let id = new_id();
        self.connection.execute(
            "INSERT INTO accounts VALUES (?1, ?2, ?3)",
            (&id, owner_principal_id, now()),
        )?;
        Ok(id)
    }

    pub fn create_collection(
        &self,
        owner_account_id: &str,
        visibility: Visibility,
    ) -> Result<String> {
        let id = new_id();
        self.connection.execute(
            "INSERT INTO collections VALUES (?1, ?2, ?3, ?4)",
            (&id, owner_account_id, visibility.as_str(), now()),
        )?;
        Ok(id)
    }

    pub fn get_collection(&self, collection_id: &str) -> Result<Collection> {
        self.connection
            .query_row(
                "SELECT id, owner_account_id, visibility, created_at FROM collections WHERE id = ?1",
                [collection_id],
                |row| {
                    let visibility: String = row.get(2)?;
                    Ok(Collection {
                        id: row.get(0)?,
                        owner_account_id: row.get(1)?,
                        visibility: visibility.parse().map_err(|error: String| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                error.into(),
                            )
                        })?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::CollectionNotFound(collection_id.to_owned()))
    }

    // Records.

    pub fn put(
        &self,
        collection_id: &str,
        text: &str,
        metadata: Option<&Metadata>,
        record_id: Option<&str>,
        author_principal_id: &str,
    ) -> Result<RecordRef> {
        let now = now();
        let record_id = record_id.map_or_else(new_id, str::to_owned);
        let transaction = self.write()?;
        let deleted_at: Option<Option<String>> = transaction
            .query_row(
                "SELECT deleted_at FROM records WHERE collection_id = ?1 AND id = ?2",
                (collection_id, &record_id),
                |row| row.get(0),
            )
            .optional()?;
        let revision = match deleted_at {
            None => {
                transaction.execute(
                    "INSERT INTO records (collection_id, id, author_principal_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    (collection_id, &record_id, author_principal_id, &now),
                )?;
                1
            }
            Some(Some(_)) => {
                return Err(StoreError::RecordDeleted {
                    collection_id: collection_id.to_owned(),
                    record_id,
                })
            }
            Some(None) => {
                let latest: i64 = transaction.query_row(
                    "SELECT MAX(revision) FROM revisions WHERE collection_id = ?1 AND record_id = ?2",
                    (collection_id, &record_id),
                    |row| row.get(0),
                )?;
                latest + 1
            }
        };
        transaction.execute(
            "INSERT INTO revisions (collection_id, record_id, revision, text, metadata, \
             text_sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                collection_id,
                &record_id,
                revision,
                text,
                metadata.map(encode),
                digest(text),
                &now,
            ),
        )?;
        transaction.commit()?;
        Ok(RecordRef {
            collection_id: collection_id.to_owned(),
            record_id,
            revision,
        })
    }

    fn record_state(&self, collection_id: &str, record_id: &str) -> Result<()> {
        let deleted_at: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT deleted_at FROM records WHERE collection_id = ?1 AND id = ?2",
                (collection_id, record_id),
                |row| row.get(0),
            )
            .optional()?;
        match deleted_at {
            None => Err(StoreError::RecordNotFound {
                collection_id: collection_id.to_owned(),
                record_id: record_id.to_owned(),
                revision: None,
            }),
            Some(Some(_)) => Err(StoreError::RecordDeleted {
                collection_id: collection_id.to_owned(),
                record_id: record_id.to_owned(),
            }),
            Some(None) => Ok(()),
        }
    }

    /// The latest revision, or `revision` when given.
    pub fn get(
        &self,
        collection_id: &str,
        record_id: &str,
        revision: Option<i64>,
    ) -> Result<Revision> {
        self.record_state(collection_id, record_id)?;
        let sql = format!(
            "SELECT {REVISION_COLUMNS} WHERE r.collection_id = ?1 AND r.record_id = ?2 \
             AND (?3 IS NULL OR r.revision = ?3) ORDER BY r.revision DESC LIMIT 1"
        );
        self.connection
            .query_row(&sql, (collection_id, record_id, revision), revision_row)
            .optional()?
            .ok_or_else(|| StoreError::RecordNotFound {
                collection_id: collection_id.to_owned(),
                record_id: record_id.to_owned(),
                revision,
            })
    }

    /// Nulls every revision's text and metadata and tombstones the record.
    /// Returns `false` when it was already deleted.
    pub fn delete(&self, collection_id: &str, record_id: &str) -> Result<bool> {
        let transaction = self.write()?;
        let deleted_at: Option<Option<String>> = transaction
            .query_row(
                "SELECT deleted_at FROM records WHERE collection_id = ?1 AND id = ?2",
                (collection_id, record_id),
                |row| row.get(0),
            )
            .optional()?;
        match deleted_at {
            None => {
                return Err(StoreError::RecordNotFound {
                    collection_id: collection_id.to_owned(),
                    record_id: record_id.to_owned(),
                    revision: None,
                })
            }
            Some(Some(_)) => return Ok(false),
            Some(None) => {}
        }
        transaction.execute(
            "UPDATE revisions SET text = NULL, metadata = NULL \
             WHERE collection_id = ?1 AND record_id = ?2",
            (collection_id, record_id),
        )?;
        transaction.execute(
            "UPDATE records SET deleted_at = ?1 WHERE collection_id = ?2 AND id = ?3",
            (now(), collection_id, record_id),
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// The latest revision of every live record, ordered by record ID.
    pub fn live_documents(&self, collection_id: &str) -> Result<Vec<Revision>> {
        let sql = format!(
            "SELECT {REVISION_COLUMNS} WHERE r.collection_id = ?1 AND rec.deleted_at IS NULL \
             AND r.revision = (SELECT MAX(latest.revision) FROM revisions latest \
             WHERE latest.collection_id = r.collection_id AND latest.record_id = r.record_id) \
             ORDER BY r.record_id"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map([collection_id], revision_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Live records whose latest metadata has every `filters` key with an equal value.
    pub fn matching_record_ids(
        &self,
        collection_id: &str,
        filters: &Metadata,
    ) -> Result<HashSet<String>> {
        Ok(self
            .live_documents(collection_id)?
            .into_iter()
            .filter(|document| {
                let metadata = document.metadata.as_ref();
                filters.iter().all(|(key, wanted)| {
                    metadata
                        .and_then(|metadata| metadata.get(key))
                        .is_some_and(|value| scalar_eq(value, wanted))
                })
            })
            .map(|document| document.record_id)
            .collect())
    }

    // Reports and searches.

    pub fn add_report(
        &self,
        collection_id: &str,
        record_id: &str,
        revision: Option<i64>,
        search_id: Option<&str>,
        text: &str,
        reporter_principal_id: &str,
    ) -> Result<String> {
        let id = new_id();
        let transaction = self.write()?;
        let not_found = |revision| StoreError::RecordNotFound {
            collection_id: collection_id.to_owned(),
            record_id: record_id.to_owned(),
            revision,
        };
        let exists = |sql: &str, parameters: &[&dyn rusqlite::ToSql]| {
            transaction
                .query_row(sql, parameters, |_| Ok(()))
                .optional()
                .map(|row| row.is_some())
        };
        if !exists(
            "SELECT 1 FROM records WHERE collection_id = ?1 AND id = ?2",
            &[&collection_id, &record_id],
        )? {
            return Err(not_found(None));
        }
        if let Some(revision) = revision {
            if !exists(
                "SELECT 1 FROM revisions WHERE collection_id = ?1 AND record_id = ?2 AND revision = ?3",
                &[&collection_id, &record_id, &revision],
            )? {
                return Err(not_found(Some(revision)));
            }
        }
        if let Some(search_id) = search_id {
            if !exists(
                "SELECT 1 FROM searches WHERE collection_id = ?1 AND id = ?2",
                &[&collection_id, &search_id],
            )? {
                return Err(StoreError::SearchNotFound {
                    collection_id: collection_id.to_owned(),
                    search_id: search_id.to_owned(),
                });
            }
        }
        transaction.execute(
            "INSERT INTO reports (collection_id, id, reporter_principal_id, record_id, revision, \
             search_id, text, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                collection_id,
                &id,
                reporter_principal_id,
                record_id,
                revision,
                search_id,
                text,
                now(),
            ),
        )?;
        transaction.commit()?;
        Ok(id)
    }

    /// Reports on a record in creation order; deleted records keep theirs.
    pub fn reports_for(&self, collection_id: &str, record_id: &str) -> Result<Vec<Report>> {
        let exists = self
            .connection
            .query_row(
                "SELECT 1 FROM records WHERE collection_id = ?1 AND id = ?2",
                (collection_id, record_id),
                |_| Ok(()),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::RecordNotFound {
                collection_id: collection_id.to_owned(),
                record_id: record_id.to_owned(),
                revision: None,
            });
        }
        let mut statement = self.connection.prepare(
            "SELECT collection_id, id, record_id, revision, search_id, text, created_at, \
             reporter_principal_id FROM reports WHERE collection_id = ?1 AND record_id = ?2 \
             ORDER BY created_at, id",
        )?;
        let rows = statement.query_map((collection_id, record_id), |row| {
            Ok(Report {
                collection_id: row.get(0)?,
                id: row.get(1)?,
                record_id: row.get(2)?,
                revision: row.get(3)?,
                search_id: row.get(4)?,
                text: row.get(5)?,
                created_at: row.get(6)?,
                reporter_principal_id: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn log_search(
        &self,
        collection_id: &str,
        text: &str,
        filters: Option<&Metadata>,
        hits: &[SearchHit],
        timings: &Metadata,
        requester_principal_id: &str,
    ) -> Result<String> {
        let id = new_id();
        let transaction = self.write()?;
        transaction.execute(
            "INSERT INTO searches (collection_id, id, requester_principal_id, text, filters, \
             timings, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                collection_id,
                &id,
                requester_principal_id,
                text,
                filters.map(encode),
                encode(timings),
                now(),
            ),
        )?;
        {
            let mut insert =
                transaction.prepare("INSERT INTO search_hits VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for (position, hit) in hits.iter().enumerate() {
                insert.execute((
                    collection_id,
                    &id,
                    position as i64,
                    &hit.record_id,
                    hit.revision,
                    hit.score,
                ))?;
            }
        }
        transaction.commit()?;
        Ok(id)
    }

    pub fn has_search(&self, collection_id: &str, search_id: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM searches WHERE collection_id = ?1 AND id = ?2",
                (collection_id, search_id),
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // Index mirror.

    pub fn index_documents(&self, collection_id: &str) -> Result<Vec<IndexEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT internal_id, record_id, revision FROM index_documents \
             WHERE collection_id = ?1 ORDER BY internal_id",
        )?;
        let rows = statement.query_map([collection_id], |row| {
            Ok(IndexEntry {
                internal_id: row.get::<_, i64>(0)? as u64,
                record_id: row.get(1)?,
                revision: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn index_encoder(&self, collection_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM index_state WHERE collection_id = ?1 AND key = 'encoder'",
                [collection_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Replaces the mirror with `rows` in internal-ID order, atomically.
    pub fn replace_index_documents(
        &self,
        collection_id: &str,
        rows: &[(&str, i64)],
        encoder: Option<&str>,
    ) -> Result<()> {
        let transaction = self.write()?;
        transaction.execute(
            "DELETE FROM index_documents WHERE collection_id = ?1",
            [collection_id],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO index_documents (collection_id, internal_id, record_id, revision) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (position, (record_id, revision)) in rows.iter().enumerate() {
                insert.execute((collection_id, position as i64, record_id, revision))?;
            }
        }
        set_index_encoder(&transaction, collection_id, encoder)?;
        transaction.commit()?;
        Ok(())
    }

    /// Applies one index write to the mirror atomically: drops `removed`,
    /// shifting later internal IDs down by one, then appends `appended`.
    pub fn update_index_documents(
        &self,
        collection_id: &str,
        removed: Option<u64>,
        appended: Option<(&str, i64)>,
        encoder: Option<&str>,
    ) -> Result<()> {
        let transaction = self.write()?;
        if let Some(removed) = removed {
            let removed = removed as i64;
            transaction.execute(
                "DELETE FROM index_documents WHERE collection_id = ?1 AND internal_id = ?2",
                (collection_id, removed),
            )?;
            // SQLite checks uniqueness row by row, so shift past every ID first.
            let offset: i64 = transaction.query_row(
                "SELECT coalesce(MAX(internal_id), 0) + 1 FROM index_documents \
                 WHERE collection_id = ?1",
                [collection_id],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE index_documents SET internal_id = internal_id + ?3 \
                 WHERE collection_id = ?1 AND internal_id > ?2",
                (collection_id, removed, offset),
            )?;
            transaction.execute(
                "UPDATE index_documents SET internal_id = internal_id - ?2 - 1 \
                 WHERE collection_id = ?1 AND internal_id >= ?2",
                (collection_id, offset),
            )?;
        }
        if let Some((record_id, revision)) = appended {
            transaction.execute(
                "INSERT INTO index_documents (collection_id, internal_id, record_id, revision) \
                 SELECT ?1, count(*), ?2, ?3 FROM index_documents WHERE collection_id = ?1",
                (collection_id, record_id, revision),
            )?;
        }
        set_index_encoder(&transaction, collection_id, encoder)?;
        transaction.commit()?;
        Ok(())
    }
}

fn set_index_encoder(
    connection: &Connection,
    collection_id: &str,
    encoder: Option<&str>,
) -> Result<()> {
    connection.execute(
        "INSERT INTO index_state (collection_id, key, value) VALUES (?1, 'encoder', ?2) \
         ON CONFLICT (collection_id, key) DO UPDATE SET value = excluded.value",
        (collection_id, encoder),
    )?;
    Ok(())
}
