# Storage versions, recovery, and upgrades

A data directory holds `priorart.sqlite` (with its WAL files) and, when an encoder
is configured and at least one record is live, the `vectors/` store. On startup,
priorart reads the schema version from the `schema_version` table, applies pending
migrations, and only then opens the index. A schema version is the tag of the
release that introduced that schema, so an unsupported version names the release
that can open it. Schema versions change only when a release changes storage; they
are independent of the HTTP protocol version.

| Stored state | Startup behavior |
|---|---|
| Empty database | Create the current schema transactionally |
| No `schema_version`, any existing schema | Reject; priorart only initializes empty databases |
| `v0.2.0` | Open without rerunning migrations |
| A tag this build does not know | Reject, naming that release as the one to use |
| Nonzero `PRAGMA user_version` | Reject as a Python implementation directory; use `v0.1.0` |

`v0.2.0` is the collection-scoped layout. It bootstraps principal
`local-principal`, account `local-account` owned by it, and collection `local`
owned by that account with visibility `restricted`. Data directories written by
the Python implementation (release `v0.1.0`, which versioned its schema with
`PRAGMA user_version`) are not adopted.

This is an unauthenticated local server with plaintext storage. A restricted
visibility label is groundwork for later access policy, not a confidentiality
guarantee. Encryption at rest is outside the current implementation scope. HTTP
and MCP are bound to the local collection; neither exposes collection
provisioning or selection. Keep the server behind a trusted local boundary.

## Collection identities and references

The trusted `Store` API creates principals, accounts with an owner principal, and
collections with an owner account. Their IDs are server-generated. New collections
default to `restricted`; visibility is `public` or `restricted` and is immutable in
SQLite. These low-level methods do not authenticate callers, grant permissions, or
establish billing authority.

Record identity is `(collection_id, record_id)`; revision identity adds `revision`.
The same client-chosen record ID may exist independently in several collections.
Every content and index-mirror method takes the collection explicitly, and logged
search hits carry no collection of their own: they belong to the search's
collection. Record authorship is separate from collection ownership and is not
changed by an update.

Composite primary and foreign keys scope revisions, reports, search hits, and index
mirror entries to their collection and exact target revision. Report revisions are
nullable. Index state is keyed by collection and key; internal vector IDs are unique
within a collection only. The index is local-only and refuses other collections.

## Index recovery

The index mirror (`index_documents`) records which record revision each dense
internal vector ID holds, and `index_state` records the encoder that wrote it.
A write commits the record in SQLite first, then updates the vector store, then
the mirror. These steps are not atomic together, and lateweave publishes each
vector-store mutation as several file renames (each array, the offsets, and
`storage.json`), so a crash can leave a half-published store.

priorart therefore coordinates database and index recovery on every start: the
mirror must list exactly the live records' latest revisions with contiguous IDs,
and, with an encoder, the vector store must open, be INT8, hold the mirrored
document count, carry the configured representation, and have been written by
the same encoder the mirror records. Any disagreement, including an unreadable
store, rebuilds the whole index from SQLite. Vectors left behind by an earlier
encoder run are never trusted after a lexical-only run has written the mirror.
Journaled mutations with atomic generation activation are planned to replace
this rebuild-on-mismatch model.

## Operating an upgrade

Stop the server and take a consistent backup of the complete data directory before
upgrading. Preserve SQLite's WAL state if present; copying only the main database
file while writes are active is not a consistent backup. Keep vectors and database
state together. Run a single server process per directory.

All pending migrations and their version stamps run inside one `BEGIN IMMEDIATE`
transaction. An error rolls back the entire batch. If the process is interrupted
before commit, SQLite recovers the previous state on reopening and the next start
retries. This covers SQLite changes only, not the vector store.

Unsupported versions are rejected before changing the journal mode or opening the
index; the CLI prints the error and exits with status 2. There is no automatic
downgrade: use a compatible binary or restore a pre-upgrade backup.

## Adding migrations

Append a `Migration` to `MIGRATIONS` in `src/store/migrations.rs`, tagged with the
release that will ship it; its position determines the upgrade order. Never modify
a released migration. The
runner acquires the write lock before reading the version and stamps each
migration inside the same transaction, which serializes startup migration
decisions; it does not make concurrent server processes safe.

Migrations receive the open transaction and must not issue `BEGIN`, `COMMIT`, or
`ROLLBACK`, nor write files outside SQLite. Add tests for data preservation,
repeated startup, and rollback when a later migration in the batch fails.
