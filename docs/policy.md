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
lookups, filters, or index loading. A forbidden caller cannot use record
existence or tombstone differences to learn about a restricted collection. An
authorized reader can still distinguish a missing record from a deleted one.

## Current operations

There are three grants, each implying the ones before it: `read`, `write`, `admin`.

| Operation | Required policy |
|---|---|
| Get current or old revision; list records; search with or without filters | Public visibility or `read` |
| Create record | `write`; author is the requesting principal |
| Update or delete record | `write` plus original authorship, or `admin`; tombstones retain authorship |
| Export revisions | `admin`; rechecked for every streamed row |
| Import revisions | `write`; writing to an existing record needs its authorship or `admin` |
| Delete collection | `admin`; deletes data and scoped grants, leaving an ID/time tombstone |
| Scoped health/diagnostics | `admin` on that collection, before counting documents |
| List collections | Public, or any grant; scope applied before pagination |

Which grants a key may hold is decided at issuance ([credentials](credentials.md)):
owners may hold any grant on their collections; anyone may hold `read` and `write`
on a public collection. `admin` therefore belongs to the owner only.

Ownership alone grants nothing; the key's grants do. Changing another author's
record preserves its original authorship. Public visibility grants readability,
not write permission. Visibility is immutable.

Searches persist nothing: no query, filter, result list, or receipt.

Imports ignore source identity and authorship; the importer becomes the author.

## Revocation, indexes, and concurrency

Authorization runs on every request, including when its index is already resident.
The service has no result or authorization cache; loaded collection indexes do not
confer access. Writes check authorization immediately before committing.

Each check observes a SQLite snapshot. A revocation observed by the check denies
the request; revocation after it cannot recall an admitted mutation or a delivered
result. Idempotent retries replay a write only after successful reauthentication
and authorization.

Requests serialize under a per-collection mutex and owned SQLite connection;
different collections can run concurrently within the bounded collection cache.
A server holds exclusive data-directory ownership for its lifetime. Direct
concurrent content writes through the trusted `Store` API remain unsupported;
they bypass both policy and in-memory index coordination.

## Revision preconditions

`Service::put` takes `WriteOptions { expected_revision, idempotency_key }`. Without
a precondition the write is unconditional: it creates the record or appends a
revision. `Some(0)` requires absence, and a positive value must equal the current
revision; otherwise the write returns `RevisionConflict`. `Service::delete` treats
its optional `expected_revision` the same way; repeating a delete is a no-op.
Checks follow policy and run inside the SQLite mutation transaction. Each service
mutation returns `Mutation<T>` containing a durable mutation ID and its operation
result. Replays recheck `write` plus authorship or `admin` and use the same
collection synchronization as new writes.
