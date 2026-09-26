# Storage versions and upgrades

On startup, priorart checks `PRAGMA user_version` in `priorart.sqlite` and applies
pending migrations before opening the index. Storage versions are independent of
the HTTP protocol and package versions.

| Stored version | Startup behavior |
|---|---|
| `0`, empty database | Create the current schema transactionally |
| `0`, complete legacy v1 layout | Recognize v1, then migrate to storage version `2` |
| `0`, other layout | Reject without repairing or adopting an unknown database |
| `1` | Migrate existing data into the restricted local collection |
| `2` | Open without rerunning migrations |
| Newer than supported | Reject with an upgrade-required error |

Version 1 introduced the version marker without changing the record layout.
Version 2 adds collection-scoped persistence. Existing records, revisions, metadata,
content hashes, tombstones, reports, searches, and index state move into collection
`local`, owned by account `local-account`. That account belongs to principal
`local-principal`. The collection's visibility is `restricted`; migration never
publishes existing content. Unknown legacy authors, reporters, and search requesters
remain NULL rather than being attributed to the owner.

This is still an unauthenticated local server with plaintext storage. A restricted
visibility label is groundwork for later access policy, not a confidentiality
guarantee. Encryption at rest is outside the current implementation scope.
HTTP and MCP remain bound to the local collection; neither exposes
collection provisioning or selection. Keep the server behind a trusted local
boundary. There is no hosted service mode.

Legacy recognition is intentionally conservative: the full application schema
must match the original table/index definitions, ignoring whitespace and case.
Extra application tables, indexes, or triggers, missing objects, and manually
altered definitions are rejected. SQLite's internal objects are excluded. Do not
set `user_version` manually to bypass this check; investigate or restore instead.

## Collection identities and references

The trusted `Store` API creates principals, accounts with an owner principal, and
collections with an owner account. Their IDs are server-generated. New collections
default to `restricted`; visibility accepts only `public` or `restricted` and is
immutable in SQLite. These low-level methods do not authenticate callers, grant
permissions, or establish billing authority. Credential issuance and policy are
not implemented yet.

Record identity is `(collection_id, record_id)`; revision identity adds `revision`.
The same client-chosen record ID may exist independently in several collections.
Every content/index store method requires `collection_id` explicitly. Writes return
`RecordRef`; revisions, reports, and internal search hits carry collection identity.
Record authorship is separate from collection ownership and is not changed by an
update. New local writes identify the local principal.

Composite primary/foreign keys scope revisions, reports, search hits, and index
mirror entries to their collection and exact target revision. Search hits migrate
from the old JSON array into ordered `search_hits` rows with their scores preserved.
Nullable report revisions remain allowed. Index state is keyed by collection and
state key; internal vector IDs are unique within a collection only. The existing
index remains local-only, explicitly rejects other collections, and keeps its
current vector path. Its reads/rebuilds cannot gather another collection's rows.

V1 allowed reports with nonexistent revision numbers and did not constrain index
mirror or search-result references. Such data cannot satisfy the new foreign keys.
Migration stops and rolls back if it finds invalid references, malformed search
hits, or unrecognized extra hit fields. It does not delete feedback, invent missing
revisions, or silently discard fields. Inspect a backup and resolve the specific
legacy inconsistency before retrying. Reports on deleted records remain valid
when the referenced revision/tombstone exists. New reports naming a nonexistent
revision return `record_not_found`.

## Operating an upgrade

Stop the server and take a consistent backup of the complete data directory before
upgrading. Preserve SQLite's WAL state if present; copying only the main database
file while writes are active is not a consistent backup. Keep vectors and database
state together. Continue to run a single server process for this directory.

All pending SQL migrations and their version stamps run inside one `BEGIN
IMMEDIATE` transaction. An exception rolls back the entire batch and closes the
startup connection. If the process is interrupted before commit, SQLite recovers
the previous state on reopening; restarting retries the pending migration. After
commit, startup skips it. This covers SQLite changes, not external index files or
arbitrary filesystem operations. An upgrade is not a repair mechanism for corrupt
databases or underlying storage failures.

Unsupported versions are rejected before changing journal mode or opening/rebuilding
the index. The CLI prints the error and exits without serving. There is no automatic
downgrade: use a compatible application or restore a pre-upgrade backup. Releases
predating version checks cannot enforce this safeguard, so do not run an old,
unversioned binary against an upgraded directory.

## Adding migrations

Append a migration to `src/priorart/migrations.py`; its position determines the next
storage version. Never modify an already released migration. The runner acquires
the write lock before reading the version and stamps each completed migration
inside the same transaction. This serializes startup migration decisions; it does
not make concurrent server processes or shared request transactions safe.

Migrations use `execute`/`executemany`, never `executescript`, explicit commits, or
external file writes. `executescript` can commit a pending transaction and break
rollback. Add an independent old-schema fixture and tests for data preservation,
repeated startup, failure after changes/version stamping, process interruption,
and unsupported newer versions. The frozen `tests/fixtures/v1.sql` includes live
and deleted revisions, nullable metadata, reports, searches, and index state; do
not regenerate it from the evolving current schema.
