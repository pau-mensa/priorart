# Service authorization

Every content operation on `Service` takes an explicit `RequestContext` and
collection ID. There is no default collection or omitted-scope search in the Rust
service API. HTTP v1 authenticates header credentials and selects explicit collections.
Its default local mode gives requests without credentials access to `local` only;
authenticated mode uses anonymous public reads as its credential-free default.
MCP currently selects `local`. Hosted operation remains disabled.

The policy follows the [deny-by-default and per-request validation guidance](https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html).
The low-level `Store` and `Index` APIs remain trusted persistence/retrieval primitives;
untrusted operations must use `Service`.

## Identities and collection scope

- `Service::authenticate` mints a context from a bearer credential. Every service
  operation revalidates the context, its versions, expiry, and revocation against
  SQLite. An invalid supplied context never becomes anonymous.
- `RequestContext::anonymous()` can read public collections and cannot mutate or
  inspect diagnostics.
- `RequestContext::local()` explicitly opts trusted local integration code into
  the built-in local principal. It can operate only on `local`, even when another
  collection is public. It must never be constructed from untrusted request data.

Unknown and unauthorized collections both return `PolicyError::Unavailable`, with
`requested resource is unavailable`. The service checks this before record/revision
lookups, filters, or index loading/recovery. A forbidden caller cannot use record
existence or tombstone differences to learn about a restricted collection. An
authorized reader can still distinguish a missing record from a deleted one.

## Current operations

| Operation | Required policy |
|---|---|
| Get current or old revision; search with or without filters | Public visibility or explicit `read` |
| Create record | Authenticated `contribute`; author is the requesting principal |
| Update record | `update` plus original authorship, or `update` plus `moderate` |
| Delete record | `delete` plus original authorship, or `delete` plus `moderate`; tombstones retain authorship |
| Export revisions | `export` and current `read`; rechecked for every streamed row |
| Import revisions | `contribute`, plus `update` for subsequent revisions; explicit destination visibility/publication |
| Delete collection | `admin`; deletes data and scoped grants, leaving an ID/time tombstone |
| Scoped health/diagnostics | `admin` on that collection, before loading its index or counting documents |
| List collections | Public, or `read`/`admin`; scope applied before pagination |

Which grants a key may hold is decided at issuance ([credentials](credentials.md)):
owners may hold any operation on their collections; anyone may hold `read`,
`contribute`, `update`, and `delete` on a public collection. `moderate` therefore
belongs to the owner only.

Ownership and `admin` do not imply other credential operations. Moderation preserves
original record authorship. Public visibility grants readability, not write or
moderation permission. Creating or updating public record text additionally requires
`publish: true`; the flag grants no authority by itself. Visibility is immutable.

Searches persist nothing: no query, filter, result list, or receipt.

Imports treat source identity and authorship as unverified claims; they never
resolve source IDs or impersonate the uploaded author. The importer becomes the
new author. Private provenance is excluded from public responses and exports.

## Revocation, indexes, and concurrency

Authorization runs on every request, including when its index is already resident.
The service has no result or authorization cache; loaded collection indexes do not
confer access. Search rechecks after loading/recovering an index, after retrieval,
and before returning results. Writes recheck after index recovery and encoding,
before committing the record, and after write-through indexing completes.

Each validation observes a SQLite snapshot. A revocation observed by the final
pre-mutation check denies the write; one observed by the final result check denies
the response. Revocation after that check cannot recall an already admitted mutation
or delivered result. If revocation happens during indexing after a record has
committed, the record can remain committed even though the request returns an
authentication error. Journaled idempotent retries recover that write only after
successful reauthentication and authorization.

Requests serialize under a per-collection mutex and owned SQLite connection;
different collections can run concurrently within the bounded collection cache.
A server holds exclusive data-directory ownership for its lifetime. Direct
concurrent content writes through the trusted `Store` API remain unsupported;
they bypass both policy and in-memory index coordination.

## Revision preconditions

`Service::put` takes `WriteOptions { publish, expected_revision, idempotency_key }`. A new record
allows no precondition or `Some(0)` (explicit absence). Existing updates require
`Some(current_revision)`; no precondition returns `RevisionRequired`, and stale
preconditions or explicit create collisions return `RevisionConflict`. An explicit
create requires contribute, while an update requires update and authorship/moderation.
`Service::delete` requires the current revision, including on repeat tombstone deletes.
Checks follow policy, precede index work, and run again inside the SQLite mutation
transaction. Each service mutation returns `Mutation<T>` containing a durable
mutation ID and its operation result. Replays revalidate current original operation
authority and use the same collection synchronization as new writes.
