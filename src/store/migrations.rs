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

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "v0.2.0",
        apply: create_collection_schema,
    },
    Migration {
        version: "v0.3.0",
        apply: create_credential_schema,
    },
    Migration {
        version: "v0.4.0",
        apply: create_mutation_schema,
    },
    Migration {
        version: "v0.5.0",
        apply: create_private_feedback_schema,
    },
];

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

fn create_credential_schema(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute_batch(
        "
        CREATE TABLE principal_auth_state (
            principal_id TEXT PRIMARY KEY REFERENCES principals(id),
            version INTEGER NOT NULL CHECK (version > 0)
        );
        CREATE TABLE credentials (
            id TEXT PRIMARY KEY,
            principal_id TEXT NOT NULL REFERENCES principals(id),
            parent_id TEXT REFERENCES credentials(id),
            verifier BLOB NOT NULL CHECK (length(verifier) = 32),
            created_at INTEGER NOT NULL,
            expires_at INTEGER,
            revoked_at INTEGER,
            grant_version INTEGER NOT NULL DEFAULT 1 CHECK (grant_version > 0),
            CHECK (expires_at IS NULL OR expires_at > created_at),
            CHECK (parent_id IS NULL OR parent_id != id)
        );
        CREATE INDEX credentials_principal ON credentials(principal_id);
        CREATE INDEX credentials_parent ON credentials(parent_id);
        CREATE TABLE credential_grants (
            credential_id TEXT NOT NULL REFERENCES credentials(id),
            collection_id TEXT NOT NULL REFERENCES collections(id),
            operation TEXT NOT NULL CHECK (operation IN (
                'read', 'contribute', 'update', 'delete', 'report', 'feedback_read',
                'feedback_delete', 'report_publish', 'moderate', 'export', 'admin', 'delegate'
            )),
            PRIMARY KEY (credential_id, collection_id, operation)
        );
    ",
    )?;
    Ok(())
}

fn create_mutation_schema(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute_batch(
        "CREATE TABLE mutations (
        id TEXT PRIMARY KEY,
        collection_id TEXT NOT NULL REFERENCES collections(id),
        principal_id TEXT NOT NULL REFERENCES principals(id),
        operation TEXT NOT NULL CHECK(operation IN ('put', 'delete', 'report')),
        idempotency_digest TEXT,
        payload_digest TEXT NOT NULL,
        authority TEXT NOT NULL,
        result TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('committed', 'applied')),
        created_at TEXT NOT NULL,
        UNIQUE(collection_id, principal_id, operation, idempotency_digest)
    ); CREATE INDEX pending_mutations ON mutations(collection_id, state);",
    )?;
    Ok(())
}

fn create_private_feedback_schema(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute_batch(
        "ALTER TABLE searches DROP COLUMN text;
         ALTER TABLE searches DROP COLUMN filters;
         ALTER TABLE searches DROP COLUMN timings;
         ALTER TABLE search_hits DROP COLUMN score;
         CREATE TABLE published_reports (
             collection_id TEXT NOT NULL,
             id TEXT NOT NULL,
             record_id TEXT NOT NULL,
             revision INTEGER NOT NULL,
             text TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (collection_id, id),
             FOREIGN KEY (collection_id, record_id, revision)
                 REFERENCES revisions(collection_id, record_id, revision)
         );",
    )?;
    Ok(())
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use crate::auth::{Grant, Operation};
    use crate::store::Store;

    #[test]
    fn credential_upgrade_preserves_records_and_retries_after_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("priorart.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        run(&connection, &MIGRATIONS[..1]).unwrap();
        connection.execute_batch("
            INSERT INTO records VALUES ('local', 'record', 'local-principal', 'before', NULL);
            INSERT INTO revisions VALUES ('local', 'record', 1, 'preserved', NULL, 'digest', 'before');
        ").unwrap();
        // Inject a failure after the real migration DDL, before its stamp commits.
        fn fail(transaction: &Transaction<'_>) -> Result<(), StoreError> {
            create_credential_schema(transaction)?;
            transaction.execute_batch("SELECT missing_function()")?;
            Ok(())
        }
        let broken = [
            Migration {
                version: "v0.2.0",
                apply: create_collection_schema,
            },
            Migration {
                version: "v0.3.0",
                apply: fail,
            },
        ];
        assert!(run(&connection, &broken).is_err());
        assert_eq!(
            stored_version(&connection).unwrap().as_deref(),
            Some("v0.2.0")
        );
        let credentials_exist: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'credentials')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!credentials_exist);
        drop(connection);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .get(LOCAL_COLLECTION_ID, "record", None)
                .unwrap()
                .text
                .as_deref(),
            Some("preserved")
        );
        let credential = store
            .issue_local_credential(
                LOCAL_PRINCIPAL_ID,
                &[Grant::new(LOCAL_COLLECTION_ID, Operation::Read)],
                None,
            )
            .unwrap()
            .into_secret();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.authenticate(&credential).is_ok());
        assert_eq!(
            stored_version(&reopened.connection).unwrap().as_deref(),
            Some(SCHEMA_VERSION)
        );
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    #[test]
    fn journal_migration_rolls_back_and_preserves_existing_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db.sqlite");
        let connection = Connection::open(&path).unwrap();
        run(&connection, &MIGRATIONS[..2]).unwrap();
        connection.execute("INSERT INTO records (collection_id,id,author_principal_id,created_at) VALUES ('local','kept','local-principal','now')", []).unwrap();
        fn broken(transaction: &Transaction<'_>) -> Result<(), StoreError> {
            create_mutation_schema(transaction)?;
            transaction.execute_batch("SELECT missing_function()")?;
            Ok(())
        }
        let migrations = [
            Migration {
                version: "v0.2.0",
                apply: create_collection_schema,
            },
            Migration {
                version: "v0.3.0",
                apply: create_credential_schema,
            },
            Migration {
                version: "v0.4.0",
                apply: broken,
            },
        ];
        assert!(run(&connection, &migrations).is_err());
        assert_eq!(
            stored_version(&connection).unwrap().as_deref(),
            Some("v0.3.0")
        );
        assert!(connection.prepare("SELECT * FROM mutations").is_err());
        migrate(&connection).unwrap();
        assert_eq!(
            stored_version(&connection).unwrap().as_deref(),
            Some(SCHEMA_VERSION)
        );
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM records WHERE id = 'kept'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM mutations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod feedback_tests {
    use super::*;

    #[test]
    fn feedback_upgrade_removes_query_data_and_rolls_back_on_failure() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        run(&connection, &MIGRATIONS[..3]).unwrap();
        connection.execute_batch("
            INSERT INTO records VALUES ('local', 'r', 'local-principal', 'now', NULL);
            INSERT INTO revisions VALUES ('local', 'r', 1, 'record', NULL, 'hash', 'now');
            INSERT INTO searches VALUES ('local', 's', 'local-principal', 'secret query', '{\"secret\":true}', '{}', 'now');
            INSERT INTO search_hits VALUES ('local', 's', 0, 'r', 1, 0.5);
            INSERT INTO reports VALUES ('local', 'f', 'local-principal', 'r', 1, 's', 'private', 'now');
        ").unwrap();
        fn fail(transaction: &Transaction<'_>) -> Result<(), StoreError> {
            create_private_feedback_schema(transaction)?;
            transaction.execute_batch("SELECT missing_function()")?;
            Ok(())
        }
        let broken = [
            Migration {
                version: "v0.2.0",
                apply: create_collection_schema,
            },
            Migration {
                version: "v0.3.0",
                apply: create_credential_schema,
            },
            Migration {
                version: "v0.4.0",
                apply: create_mutation_schema,
            },
            Migration {
                version: "v0.5.0",
                apply: fail,
            },
        ];
        assert!(run(&connection, &broken).is_err());
        assert_eq!(
            stored_version(&connection).unwrap().as_deref(),
            Some("v0.4.0")
        );
        assert!(connection.prepare("SELECT text FROM searches").is_ok());
        migrate(&connection).unwrap();
        migrate(&connection).unwrap();
        for column in ["text", "filters", "timings"] {
            assert!(connection
                .prepare(&format!("SELECT {column} FROM searches"))
                .is_err());
        }
        assert!(connection.prepare("SELECT score FROM search_hits").is_err());
        let link: String = connection
            .query_row("SELECT search_id FROM reports", [], |r| r.get(0))
            .unwrap();
        assert_eq!(link, "s");
        let revision: i64 = connection
            .query_row("SELECT revision FROM search_hits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(revision, 1);
        assert!(connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
    }
}
