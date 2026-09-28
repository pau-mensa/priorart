# Storage versions, recovery, and upgrades

A data directory holds `priorart.sqlite` (with its WAL files) and an `indexes/`
directory. Each collection's storage directory is the lowercase SHA-256 hex digest
of its ID, computed by the server; IDs are never interpreted as paths. Its `CURRENT`
file atomically selects a UUID-named generation containing `manifest.json` and,
when nonempty and encoded, a `vectors/` store. It also identifies the previous
generation retained for recovery. `writer.lock` holds exclusive server ownership.
On startup,
priorart reads the schema version from the `schema_version` table, applies pending
migrations, and only then opens the index. A schema version is the tag of the
release that introduced that schema, so an unsupported version names the release
that can open it. Schema versions change only when a release changes storage; they
are independent of the HTTP protocol version.

| Stored state | Startup behavior |
|---|---|
| Empty database | Create the current schema transactionally |
| No `schema_version`, any existing schema | Reject; priorart only initializes empty databases |
| `v0.2.0` | Apply credential, mutation, feedback, and lifecycle migrations |
| `v0.3.0` | Apply mutation, feedback, and lifecycle migrations |
| `v0.4.0` | Apply feedback and lifecycle migrations |
| `v0.5.0` | Purge existing tombstoned content and add resumable index cleanup |
| `v0.6.0` | Open without rerunning migrations |
| A tag this build does not know | Reject, naming that release as the one to use |
| Nonzero `PRAGMA user_version` | Reject as a Python implementation directory; use `v0.1.0` |

`v0.2.0` is the collection-scoped layout. It bootstraps principal
`local-principal`, account `local-account` owned by it, and collection `local`
owned by that account with visibility `restricted`. Data directories written by
the Python implementation (release `v0.1.0`, which versioned its schema with
`PRAGMA user_version`) are not adopted.

Schema `v0.3.0` adds credential verifiers, scoped grants, and authentication versions.
Schema `v0.4.0` adds durable, scoped mutation receipts without rewriting content.
Schema `v0.5.0` drops search text, filters, timings, and hit scores while preserving
receipt IDs, owners, timestamps, and returned revision links. It adds a separate
published-report table without private source or receipt links. Dropping columns
is logical removal; old pages, journals, and backups may retain previous values.
See [local credential administration](credentials.md) for lifecycle and context semantics.

HTTP v1 accepts header credentials and explicit collection scopes under
[context and collection policy](policy.md). Local and authenticated modes are
loopback-only; hosted operation remains disabled. Encryption at rest is outside
the implementation scope. Collection provisioning requires local administration;
MCP currently targets `local`. Keep the server behind a trusted local boundary.

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
changed by an update. Writes/deletes compare an expected revision inside their
immediate SQLite transaction; stale updates and explicit create collisions fail
without appending a revision. This does not make independent index writers safe.

Composite primary and foreign keys scope revisions, reports, search hits, and index
mirror entries to their collection and exact target revision. Report revisions remain
nullable in storage for existing rows; new service reports require an exact revision. Index state is keyed by collection and key; internal vector IDs are unique
within a collection only. The trusted index API accepts an explicit existing
collection; it does not provide authorization. Service policy authorizes before
index loading or recovery. HTTP selects explicit collections; the bundled MCP client selects `local`.

## Ownership and concurrency

`Service` acquires an exclusive OS advisory lock on `writer.lock` before opening
SQLite or loading the encoder, and holds it for its lifetime. A second server,
including one using a symlink alias of the same directory, fails immediately.
Process exit releases the lock; never delete or replace its inode while a server
runs. Use a local filesystem with reliable advisory locks, atomic rename, and
file/directory sync semantics. This is not a distributed/network-filesystem lock.

Each admitted collection owns a mutex, SQLite connection, and lazily loaded index.
The connection never participates in another collection's request/transaction.
Authentication and discovery use short-lived separate connections. All operations
within a collection serialize, including record reads, so search results cannot
mix an index revision with a concurrent update or delete. Different collections
can encode and search concurrently. SQLite still serializes its short write
transactions; no write transaction is held while encoding.

`PRIORART_MAX_LOADED_INDEXES` (default 8) bounds cached collection states and acts
as a process-wide bound on concurrently active collections. Idle states use LRU
eviction; pinned states cannot be evicted or duplicated. If admitting a different
collection would exceed capacity and every state is pinned, it gets `429
resource_limit`. The bound counts collections, not bytes, token matrices, or queued
requests. Each encoder call can itself use `PRIORART_ENCODER_THREADS` CPU threads.

Local credential administration can still revoke credentials during retrieval.
The low-level `Store` and `Index` APIs are trusted building blocks, not independent
server writers: direct concurrent content/index writes bypass ownership and policy
and are unsupported. Stop the server before schema upgrades or direct maintenance.

## Journal and idempotency

Every service put/delete/report has a durable mutation ID. The same immediate
SQLite transaction applies revision preconditions, commits the content change,
and inserts the mutation receipt. SQLite connections explicitly use `synchronous=FULL`.
Receipts contain scope, operation, required authority, creation time, digests of
optional idempotency keys and payloads, and the minimal result (IDs/revision).
They do not duplicate record/report text, metadata, credentials, or raw keys.

Record mutations begin in `committed` state; reports are immediately `applied`
because they have no derived retrieval state. Record mutations become `applied`
only after a complete index generation is activated or validated against the
current live records. Recovery reconciles all committed mutations in that
collection to its latest state. It does not replay an old record payload over a
newer update or deletion. Applied receipts remain available for idempotent retries.
Content purge scrubs affected receipts as described below. Time-based receipt
retention remains step 10c.

A matching idempotency retry reauthenticates, checks current original-operation
permission and target availability, recovers the index if needed, and returns the
original result and mutation ID. The scope is principal, collection, and operation;
a different payload under the same scoped key fails. See [HTTP retries](protocol.md#mutation-retries).

## Atomic generations and recovery

A write encodes its new document before committing content. It then copies the
active vector files to a private UUID generation and applies its index change
there. Rebuilds construct a fresh generation from live SQLite revisions. The
activated generation is never modified by normal writes. The full manifest and
all staged files/directories are synced before atomic replacement of `CURRENT`;
the containing directory is synced before marking journal entries applied.

The previous activated generation is retained. Abandoned staging generations and
older generations are reclaimed on subsequent builds, preserving the active and
previous generations. Unrecognized old layouts are never served or adopted; an
absent/invalid current pointer causes a rebuild from SQLite. Existing unrelated
files are left alone. Ordinary writes retain a previous generation. Record or collection purge removes
all generations in the affected collection; see the purge rules below.

The manifest binds collection, index incarnation, generation, encoder representation,
recipe, ordered record/revision mapping, and vector-store generation. Loading checks
these against the SQL index mirror and latest live revisions. Incompatible or
incomplete state rebuilds only the selected authorized collection. The previous
generation is never used to serve stale or deleted revisions just because the new
one failed. If recovery cannot finish, searches and mutations fail safely and retry
recovery on the next request. A consistent reopen does not re-encode content.

A process may exit after committing a record but before index activation or the
response. The durable journal and current records determine recovery; retrying with
the same idempotency key does not append another revision. A successful put means
the revision was searchable before releasing its collection lock. A later update
or delete may supersede it, including before a retry returns its original receipt.

Copying vector files adds disk I/O proportional to the existing index per mutation,
although unchanged records are not re-encoded. Filesystem syncs establish the
publication order; actual power-loss durability still depends on the filesystem
and storage hardware honoring them. Tokenizer fingerprints and chunking recipe
versions remain later retrieval work.

## Purge and restore

Record deletion commits the tombstone, removal of all revisions/checksums,
associated private/published reports, receipt-result links, and a durable purge
job in one SQLite transaction. Other receipt results are preserved. The remaining
record tombstone contains collection/record IDs, author principal, deletion time,
and last revision for authorization and retry preconditions; its creation time is
cleared. Deleted IDs cannot be reused.

Affected mutation receipts without retry keys are removed. Keyed create/update/
report receipts become markers containing no payload digest or result, so delayed
retries cannot recreate auto-ID records or feedback. Delete receipts retain only
non-content retry evidence. No billing evidence exists at this stage.

After commit, the collection's cached index is dropped and its entire index
storage directory is removed and synced. Recovery acknowledges the job only after
file removal succeeds. The index is rebuilt solely from surviving records before
a record deletion returns success. This re-encodes the surviving collection and
has cost proportional to its size; unrelated collection indexes are untouched.
An encoder failure cannot undo a committed deletion. Revocation during a rebuild
blocks the response, but cannot undo the already authorized database commit.

Collection deletion atomically removes all collection data and scoped grants,
bumps affected credentials' grant versions, and leaves only a collection ID/time
tombstone plus pending filesystem cleanup. A transaction interrupted before commit
rolls back; an interruption after commit leaves the scope unavailable. Startup
resumes cleanup before serving, even when no caller retains a grant. Deleting
`local` does not bootstrap a replacement.

Schema `v0.6.0` applies the same purge to records already tombstoned in the database.
An index manifest carries the collection's purge version. An older restored index
cannot match a newer deletion boundary and is removed before rebuilding. Deleted
collection directories are also removed on startup. The authoritative database,
not an index generation, decides which records exist.

A supported restore uses a consistent database backup that includes every deletion
that must remain effective; discard all copied index directories and rebuild.
Never serve an older database snapshot that lacks subsequent tombstones. If only
an older backup is available, replay all intervening deletions in an isolated
instance before allowing access; without that deletion history, a no-resurrection
guarantee is impossible. Do not merge an old index mirror or old records into the
current database. Keep the service stopped while restoring and preserve WAL files
when taking a live SQLite backup.

Purge removes logical rows, caches, and index files from the live service. It does
not promise physical erasure from SQLite free pages, WAL/journals, filesystem
snapshots, storage media, exported files, or backups. Backup rotation and any
storage-level erasure must be handled separately. Standalone feedback deletion,
retention jobs, and export/import remain the next lifecycle changes.

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
decisions. Server ownership is separately enforced by the directory lock.

Migrations receive the open transaction and must not issue `BEGIN`, `COMMIT`, or
`ROLLBACK`, nor write files outside SQLite. Add tests for data preservation,
repeated startup, and rollback when a later migration in the batch fails.
