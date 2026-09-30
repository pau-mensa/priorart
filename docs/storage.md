# Storage, recovery, and upgrades

A data directory holds `priorart.sqlite` (with its WAL files) and `writer.lock`.
SQLite is the only persistent state. Each collection's BM25 index is built in
memory from its live records on first search and updated after every committed
write; after a restart or cache eviction it is simply rebuilt from SQLite.

## Schema versions

On startup priorart reads the `schema_version` table and applies pending
migrations before serving. A schema version is the tag of the release that
introduced that schema, so an unsupported version names the release that can open
it. Schema versions are independent of the HTTP protocol version.

| Stored state | Startup behavior |
|---|---|
| Empty database | Create the current schema (`v0.9.0`) transactionally |
| No `schema_version`, any existing schema | Reject; priorart only initializes empty databases |
| `v0.9.0` | Open without rerunning migrations |
| A tag this build does not know | Reject, naming that release as the one to use |
| Nonzero `PRAGMA user_version` | Reject as a Python implementation directory; use `v0.1.0` |

`v0.9.0` is the first schema. It bootstraps principal `local-principal` and the
restricted collection `local` it owns. See [credentials](credentials.md).

## Identities

Principals and collections have server-generated IDs. Collections are `public` or
`restricted` (the default), and visibility is immutable. Record identity is
`(collection_id, record_id)`; revision identity adds `revision`. The same record ID
may exist independently in several collections. Authorship belongs to the record's
principal and is not changed by updates or moderation.

The low-level `Store` API is trusted and does no authorization; untrusted callers
go through `Service` and its [policy](policy.md).

## Ownership and concurrency

`Service` takes an exclusive advisory lock on `writer.lock` for its lifetime, so a
second server on the same directory (including through a symlink) fails at
startup. The in-memory indexes depend on this: a second writer would leave them
stale. Use a local filesystem with reliable advisory locks.

Each cached collection has its own mutex, SQLite connection, and index. Operations
within a collection serialize; different collections run concurrently. A search
holds every selected collection's mutex, taken in collection ID order.
`PRIORART_MAX_LOADED_INDEXES` (default 8) bounds cached collections. Idle ones are
evicted LRU; when every slot is in use, a request for another collection gets
`429 resource_limit`.

## Mutation journal

Every put and delete commits its content change and a mutation receipt in one
`synchronous=FULL` SQLite transaction, with the revision precondition checked
inside it. Receipts hold scope, operation, required authority, timestamps, digests
of the idempotency key and payload, and the minimal result; never text, metadata,
or raw keys. A retry with the same key rechecks current authority and returns the
original result and mutation ID. See [HTTP retries](protocol.md#mutation-retries).

## Deletion

Record deletion removes every revision and leaves a tombstone (IDs, author,
deletion time, last revision) in one transaction; deleted IDs cannot be reused.
Unkeyed receipts for the record are dropped. Keyed receipts become markers without
payload digest or result, so a delayed retry cannot recreate the content.

Collection deletion removes the collection's records, revisions, receipts, and
grants, bumps the affected credentials' grant versions, and leaves an ID/time
tombstone so the ID is never reused. Deleting `local` does not recreate it.

Deletion removes logical rows. It does not erase SQLite free pages, WAL files,
filesystem snapshots, exports, or backups.

## Content transfers

Each collection has a content version, bumped by every revision change. Export
reads one revision at a time by keyset and aborts if the version changes. Import
commits each revision and its receipt in one transaction, under the source record
ID, so a tombstoned ID cannot be recreated. Import retry keys live in the `put`
namespace with an `import:` prefix.

## Backups and upgrades

Stop the server and back up the whole data directory, WAL files included, before
upgrading or restoring. An older backup lacks later deletions; replay them before
serving it. All pending migrations run in one `BEGIN IMMEDIATE` transaction and
roll back together on error. There is no automatic downgrade.

To add a migration, append it to `MIGRATIONS` in `src/store/migrations.rs`, tagged
with the release that ships it. Never modify a released migration. Migrations run
inside the open transaction and must not issue `BEGIN`, `COMMIT`, or `ROLLBACK`.
