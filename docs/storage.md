# Storage versions and upgrades

On startup, priorart checks `PRAGMA user_version` in `priorart.sqlite` and applies
pending migrations before opening the index. Storage versions are independent of
the HTTP protocol and package versions.

| Stored version | Startup behavior |
|---|---|
| `0`, empty database | Create the current schema transactionally |
| `0`, complete legacy v1 layout | Preserve every row and stamp storage version `1` |
| `0`, other layout | Reject without repairing or adopting an unknown database |
| `1` | Open without rerunning migrations |
| Newer than supported | Reject with an upgrade-required error |

Version 1 introduces the version marker, not a new record layout. Records,
revision history, metadata, content hashes, tombstones, reports, search logs, and
index mirror/encoder state retain their existing semantics. This upgrade does not
introduce collections, authentication, encryption, or changes to visibility.

Legacy recognition is intentionally conservative: the full application schema
must match the original table/index definitions, ignoring whitespace and case.
Extra application tables, indexes, or triggers, missing objects, and manually
altered definitions are rejected. SQLite's internal objects are excluded. Do not
set `user_version` manually to bypass this check; investigate or restore instead.

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
