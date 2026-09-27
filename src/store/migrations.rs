//! Transactional SQLite schema versions, independent of record operations.
//!
//! Append future migrations; never edit a released one. Migrations run inside
//! the caller's write transaction and must not commit or touch external files.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::{StoreError, LOCAL_ACCOUNT_ID, LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID};

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("{0}")]
    UnsupportedVersion(String),
    #[error("{0}")]
    Unrecognized(String),
}

/// A schema change, identified by the release tag that introduced it.
pub(crate) struct Migration {
    pub version: &'static str,
    pub apply: fn(&Transaction<'_>) -> Result<(), StoreError>,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: "v0.2.0",
    apply: create_collection_schema,
}];

pub const SCHEMA_VERSION: &str = MIGRATIONS[MIGRATIONS.len() - 1].version;

/// The last release of the Python implementation, which versioned its schema
/// with `PRAGMA user_version`.
const PYTHON_RELEASE: &str = "v0.1.0";

pub fn migrate(connection: &Connection) -> Result<(), StoreError> {
    run(connection, MIGRATIONS)
}

/// Applies pending migrations and their version stamps as one transaction.
pub(crate) fn run(connection: &Connection, migrations: &[Migration]) -> Result<(), StoreError> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    // Read under the write lock so concurrent openers cannot migrate twice.
    let python_version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if python_version != 0 {
        return Err(SchemaError::UnsupportedVersion(format!(
            "Database was written by the Python implementation of priorart (schema version \
             {python_version}); open it with priorart {PYTHON_RELEASE} or point \
             PRIORART_DATA_DIR elsewhere."
        ))
        .into());
    }
    let pending = match stored_version(&transaction)? {
        None => 0,
        Some(stored) => match migrations.iter().position(|m| m.version == stored) {
            Some(position) => position + 1,
            None => {
                let latest = migrations.last().map_or("none", |m| m.version);
                return Err(SchemaError::UnsupportedVersion(format!(
                    "Database schema {stored} is not known to this build (latest {latest}); \
                     open it with priorart {stored} or later."
                ))
                .into());
            }
        },
    };
    for migration in &migrations[pending..] {
        (migration.apply)(&transaction)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version TEXT NOT NULL); \
             DELETE FROM schema_version;",
        )?;
        transaction.execute(
            "INSERT INTO schema_version VALUES (?1)",
            [migration.version],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn stored_version(connection: &Connection) -> Result<Option<String>, StoreError> {
    let exists: bool = connection.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    Ok(connection
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .optional()?)
}

fn create_collection_schema(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let existing: i64 = transaction.query_row(
        "SELECT count(*) FROM sqlite_master WHERE substr(name, 1, 7) != 'sqlite_'",
        [],
        |row| row.get(0),
    )?;
    if existing > 0 {
        return Err(SchemaError::Unrecognized(format!(
            "Unversioned database already contains a schema; priorart only initializes \
                 empty databases. If the Python implementation wrote it, open it with priorart \
                 {PYTHON_RELEASE}; otherwise inspect the file or point PRIORART_DATA_DIR \
                 elsewhere."
        ))
        .into());
    }
    transaction.execute_batch(SCHEMA)?;
    let now: String =
        transaction.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |row| {
            row.get(0)
        })?;
    transaction.execute(
        "INSERT INTO principals VALUES (?1, ?2)",
        (LOCAL_PRINCIPAL_ID, &now),
    )?;
    transaction.execute(
        "INSERT INTO accounts VALUES (?1, ?2, ?3)",
        (LOCAL_ACCOUNT_ID, LOCAL_PRINCIPAL_ID, &now),
    )?;
    transaction.execute(
        "INSERT INTO collections VALUES (?1, ?2, 'restricted', ?3)",
        (LOCAL_COLLECTION_ID, LOCAL_ACCOUNT_ID, &now),
    )?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE principals (id TEXT PRIMARY KEY, created_at TEXT NOT NULL);
CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    owner_principal_id TEXT NOT NULL REFERENCES principals(id),
    created_at TEXT NOT NULL
);
CREATE TABLE collections (
    id TEXT PRIMARY KEY,
    owner_account_id TEXT NOT NULL REFERENCES accounts(id),
    visibility TEXT NOT NULL DEFAULT 'restricted'
        CHECK (visibility IN ('public', 'restricted')),
    created_at TEXT NOT NULL
);
CREATE TRIGGER collection_visibility_immutable
    BEFORE UPDATE OF visibility ON collections
    WHEN NEW.visibility != OLD.visibility
    BEGIN SELECT RAISE(ABORT, 'collection visibility is immutable'); END;
CREATE TABLE records (
    collection_id TEXT NOT NULL REFERENCES collections(id),
    id TEXT NOT NULL,
    author_principal_id TEXT REFERENCES principals(id),
    created_at TEXT NOT NULL,
    deleted_at TEXT,
    PRIMARY KEY (collection_id, id)
);
CREATE TABLE revisions (
    collection_id TEXT NOT NULL,
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    text TEXT,
    metadata TEXT,
    text_sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (collection_id, record_id, revision),
    FOREIGN KEY (collection_id, record_id) REFERENCES records(collection_id, id)
);
CREATE TABLE searches (
    collection_id TEXT NOT NULL REFERENCES collections(id),
    id TEXT NOT NULL,
    requester_principal_id TEXT REFERENCES principals(id),
    text TEXT NOT NULL,
    filters TEXT,
    timings TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (collection_id, id)
);
CREATE TABLE search_hits (
    collection_id TEXT NOT NULL,
    search_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    score REAL NOT NULL,
    PRIMARY KEY (collection_id, search_id, position),
    FOREIGN KEY (collection_id, search_id) REFERENCES searches(collection_id, id),
    FOREIGN KEY (collection_id, record_id, revision)
        REFERENCES revisions(collection_id, record_id, revision)
);
CREATE TABLE reports (
    collection_id TEXT NOT NULL,
    id TEXT NOT NULL,
    reporter_principal_id TEXT REFERENCES principals(id),
    record_id TEXT NOT NULL,
    revision INTEGER,
    search_id TEXT,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (collection_id, id),
    FOREIGN KEY (collection_id, record_id) REFERENCES records(collection_id, id),
    FOREIGN KEY (collection_id, record_id, revision)
        REFERENCES revisions(collection_id, record_id, revision),
    FOREIGN KEY (collection_id, search_id) REFERENCES searches(collection_id, id)
);
CREATE INDEX reports_record ON reports(collection_id, record_id, created_at);
CREATE TABLE index_documents (
    collection_id TEXT NOT NULL,
    internal_id INTEGER NOT NULL CHECK (internal_id >= 0),
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    PRIMARY KEY (collection_id, internal_id),
    UNIQUE (collection_id, record_id),
    FOREIGN KEY (collection_id, record_id, revision)
        REFERENCES revisions(collection_id, record_id, revision)
);
CREATE TABLE index_state (
    collection_id TEXT NOT NULL REFERENCES collections(id),
    key TEXT NOT NULL,
    value TEXT,
    PRIMARY KEY (collection_id, key)
);
";
