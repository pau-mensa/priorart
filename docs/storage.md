# Storage and upgrades

A data directory holds `priorart.sqlite` (with its WAL files) and `writer.lock`.
SQLite is the only persistent state. Each collection's BM25 index is built in
memory from its live records on first use, updated after every write, and
rebuilt from SQLite after a restart or cache eviction.

## Schema versions

On startup priorart applies pending migrations before serving. A schema version is
the tag of the release that introduced it, independent of the HTTP protocol
version.

| Stored state | Startup behavior |
|---|---|
| Empty database | Create the current schema (`v0.1.0`) |
| Existing tables without `schema_version` | Reject |
| `v0.1.0` | Open |
| A tag this build does not know | Reject, naming that release |

## Records and concurrency

Record identity is `(collection_id, record_id)`; the same record ID can exist in
several collections. Revisions are kept; deletion removes every revision and
leaves a tombstone, so a deleted ID is never reused.

One server owns a data directory: it holds an exclusive lock on `writer.lock`, and
a second server on the same directory fails at startup. Use a local filesystem
with reliable advisory locks. Within a collection, operations run one at a time;
different collections run concurrently. `PRIORART_MAX_LOADED_INDEXES` (default 8)
bounds cached collections; idle ones are evicted, and when all are busy a request
for another collection gets `429 resource_limit`.

Every put and delete commits its content change and a mutation receipt in one
transaction. Receipts hold digests of the idempotency key and payload plus the
result, never text or metadata; they back [retries](protocol.md#mutation-retries).

## Deletion

Deleting a record or collection removes its rows. It does not erase SQLite free
pages, WAL files, filesystem snapshots, exports, or backups.

## Backups and upgrades

Stop the server and back up the whole data directory, WAL files included, before
upgrading or restoring. A restored backup lacks later deletions; replay them
before serving it. Migrations run in one transaction and roll back together on
error. There is no downgrade.

To add a migration, append it to `MIGRATIONS` in `src/store/migrations.rs`, tagged
with the release that ships it. Never modify a released migration, and never issue
`BEGIN`, `COMMIT`, or `ROLLBACK` inside one.
