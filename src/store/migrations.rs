//! Transactional SQLite schema versions, independent of record operations.
//!
//! Append future migrations; never edit a released one. Migrations run inside
//! the caller's write transaction and must not commit or touch external files.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::{StoreError, LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID};

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
    version: "v0.9.0",
    apply: create_schema,
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

fn create_schema(transaction: &Transaction<'_>) -> Result<(), StoreError> {
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
        "INSERT INTO collections (id, owner_principal_id, visibility, created_at) \
         VALUES (?1, ?2, 'restricted', ?3)",
        (LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID, &now),
    )?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE principals (id TEXT PRIMARY KEY, created_at TEXT NOT NULL);
CREATE TABLE collections (
    id TEXT PRIMARY KEY,
    owner_principal_id TEXT NOT NULL REFERENCES principals(id),
    visibility TEXT NOT NULL DEFAULT 'restricted'
        CHECK (visibility IN ('public', 'restricted')),
    created_at TEXT NOT NULL,
    content_version INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER collection_visibility_immutable
    BEFORE UPDATE OF visibility ON collections
    WHEN NEW.visibility != OLD.visibility
    BEGIN SELECT RAISE(ABORT, 'collection visibility is immutable'); END;
CREATE TABLE collection_tombstones (id TEXT PRIMARY KEY, deleted_at TEXT NOT NULL);
CREATE TRIGGER prevent_collection_resurrection BEFORE INSERT ON collections
    WHEN EXISTS(SELECT 1 FROM collection_tombstones WHERE id = NEW.id)
    BEGIN SELECT RAISE(ABORT, 'collection was deleted'); END;
CREATE TABLE records (
    collection_id TEXT NOT NULL REFERENCES collections(id),
    id TEXT NOT NULL,
    author_principal_id TEXT REFERENCES principals(id),
    created_at TEXT NOT NULL,
    deleted_at TEXT,
    deleted_revision INTEGER,
    PRIMARY KEY (collection_id, id)
);
CREATE INDEX records_author ON records(collection_id, author_principal_id, id);
CREATE TABLE revisions (
    collection_id TEXT NOT NULL,
    record_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    text TEXT,
    metadata TEXT,
    text_sha256 TEXT NOT NULL,
    truncated INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
    created_at TEXT NOT NULL,
    PRIMARY KEY (collection_id, record_id, revision),
    FOREIGN KEY (collection_id, record_id) REFERENCES records(collection_id, id)
);
CREATE TRIGGER revision_insert_version AFTER INSERT ON revisions
    BEGIN UPDATE collections SET content_version = content_version + 1 WHERE id = NEW.collection_id; END;
CREATE TRIGGER revision_delete_version AFTER DELETE ON revisions
    BEGIN UPDATE collections SET content_version = content_version + 1 WHERE id = OLD.collection_id; END;
CREATE TRIGGER revision_update_version AFTER UPDATE ON revisions
    BEGIN UPDATE collections SET content_version = content_version + 1 WHERE id = NEW.collection_id; END;
CREATE TABLE principal_auth_state (
    principal_id TEXT PRIMARY KEY REFERENCES principals(id),
    version INTEGER NOT NULL CHECK (version > 0)
);
CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals(id),
    verifier BLOB NOT NULL CHECK (length(verifier) = 32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    revoked_at INTEGER,
    grant_version INTEGER NOT NULL DEFAULT 1 CHECK (grant_version > 0),
    CHECK (expires_at IS NULL OR expires_at > created_at)
);
CREATE INDEX credentials_principal ON credentials(principal_id);
CREATE TABLE credential_grants (
    credential_id TEXT NOT NULL REFERENCES credentials(id),
    collection_id TEXT NOT NULL REFERENCES collections(id),
    operation TEXT NOT NULL CHECK (operation IN ('read', 'write', 'admin')),
    PRIMARY KEY (credential_id, collection_id, operation)
);
CREATE TABLE mutations (
    id TEXT PRIMARY KEY,
    collection_id TEXT NOT NULL REFERENCES collections(id),
    principal_id TEXT NOT NULL REFERENCES principals(id),
    operation TEXT NOT NULL CHECK (operation IN ('put', 'delete')),
    idempotency_digest TEXT,
    payload_digest TEXT NOT NULL,
    result TEXT NOT NULL,
    created_at TEXT NOT NULL,
    target_record_id TEXT,
    UNIQUE (collection_id, principal_id, operation, idempotency_digest)
);
CREATE INDEX mutations_target ON mutations(collection_id, target_record_id);
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn failed_migration_rolls_back_and_a_later_open_retries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("priorart.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        fn fail(transaction: &Transaction<'_>) -> Result<(), StoreError> {
            create_schema(transaction)?;
            transaction.execute_batch("SELECT missing_function()")?;
            Ok(())
        }
        let broken = [Migration {
            version: SCHEMA_VERSION,
            apply: fail,
        }];
        assert!(run(&connection, &broken).is_err());
        assert_eq!(stored_version(&connection).unwrap(), None);
        let tables: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tables, 0);
        drop(connection);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            stored_version(&store.connection).unwrap().as_deref(),
            Some(SCHEMA_VERSION)
        );
        assert!(store.get_collection(LOCAL_COLLECTION_ID).is_ok());
    }

    #[test]
    fn unknown_schema_versions_are_rejected() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_version (version TEXT NOT NULL);
                 INSERT INTO schema_version VALUES ('v0.8.0');",
            )
            .unwrap();
        assert!(matches!(
            migrate(&connection),
            Err(StoreError::Schema(SchemaError::UnsupportedVersion(_)))
        ));
    }
}
