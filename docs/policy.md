# Service authorization

Every content operation on `Service` now takes an explicit `RequestContext` and
collection ID. There is no default collection or omitted-scope search in the Rust
service API. HTTP v1 authenticates header credentials and selects explicit collections.
Its default local mode gives requests without credentials access to `local` only;
authenticated mode uses anonymous public reads as its credential-free default.
MCP currently selects `local`. Hosted operation remains disabled pending privacy,
resource accounting, and deletion work.

The policy follows the [deny-by-default and per-request validation guidance](https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html).
The low-level `Store` and `Index` APIs remain trusted persistence/retrieval primitives;
untrusted operations must use `Service`.

## Identities and collection scope

- `Service::authenticate` mints a context from a bearer credential. Every service
  operation revalidates the context, its versions, expiry, revocation, and issuer
  chain against SQLite. An invalid supplied context never becomes anonymous.
- `RequestContext::anonymous()` can read public collections and cannot mutate,
  read private feedback, or inspect diagnostics.
- `RequestContext::local()` explicitly opts trusted local integration code into
  the built-in local principal. It can operate only on `local`, even when another
  collection is public. It must never be constructed from untrusted request data.

Unknown and unauthorized collections both return `PolicyError::Unavailable`, with
`requested resource is unavailable`. The service checks this before record/revision
lookups, filters, receipts, or index loading/recovery. A forbidden caller cannot use
record existence or tombstone differences to learn about a restricted collection.
An authorized reader can still distinguish a missing record from a deleted one.
These error rules do not promise identical timing or hide operational side channels.

## Current operations

| Operation | Required policy |
|---|---|
| Get current or old revision; search with or without filters | Public visibility or explicit `read`; context must still be valid |
| Create record | Authenticated `contribute`; author is the requesting principal |
| Update record | `update` plus original authorship, or `update` plus `moderate` |
| Delete record | `delete` plus original authorship, or `delete` plus `moderate`; tombstones retain authorship |
| Create report | `report` plus target read access; attached receipt must belong to requester and contain the exact target revision |
| List reports | `feedback_read` plus target read access; only the requester's rows are loaded |
| Read receipt | Owning principal, `feedback_read`, and current scope read access |
| Publish report | Author, `report_publish`, readable live public target, and selected text |
| Delete private report/receipt | `feedback_delete` plus requester ownership; no read permission required |
| Delete published report | `feedback_delete` plus publisher ownership, or `feedback_delete` plus `moderate` |
| Create/inspect/run retention job | Current `admin` on its explicit collection; no private feedback text returned |
| Read published reports | Target read access; anonymous readers allowed for public targets |
| Export revisions | `export` and current `read`; rechecked for every streamed row |
| Import revisions | `contribute`, plus `update` for subsequent revisions; explicit destination visibility/publication |
| Delete collection | `admin`; deletes data and scoped grants, leaving an ID/time tombstone |
| Scoped health/diagnostics | `admin` on that collection, before loading its index or counting documents |

Ownership and `admin` do not imply other credential operations. Moderation preserves
original record authorship. Public visibility grants readability, not write or
moderation permission. Creating or updating public record text additionally requires
`publish: true`; the flag grants no authority by itself. Restricted visibility remains
immutable, and no operation automatically republishes restricted content.

Reports remain private even on public records. An owner/moderator cannot read another
requester's reports. Deleted targets do not expose reports through the service.
HTTP and direct service calls require an exact positive report revision. Attached
receipts must contain that exact result. Unknown and someone else's receipt IDs
produce the same missing-receipt error. Publication creates a separate copy with
selected text and the public target reference, without private source links.

Anonymous searches return `search_id: None` and persist no query or receipt.
Authenticated/local searches persist minimal requester-private receipts without
queries, filters, scores, or timings. Retention and standalone feedback removal
remain later lifecycle work. Record/collection deletion purges
linked feedback and index generations.
Collection discovery requires read or admin access and applies its scope before
pagination. Provisioning remains local administration; no remote creation or background-job API is available.

Imports treat source identity and authorship as unverified claims; they never
resolve source IDs or impersonate the uploaded author. The importer becomes the
new author. Private provenance is excluded from public responses and exports.

## Delegation across principals

`Store::delegate_credential_to` can issue a key to another existing principal. Every
grant must be in the issuer's scope, accompanied by `delegate` on that collection;
expiry and depth limits are unchanged. The original issuer chain remains attached
and is checked on every authentication/revalidation. Revoking or replacing grants on
an ancestor revokes descendants across principal boundaries and increments affected
principal versions. Independent sibling bearer keys remain usable after reauthentication.
The same-principal `delegate_credential` method remains useful for additional agents.
Root issuance still requires ownership. No remote grant-management endpoint exists.

## Revocation, indexes, and concurrency

Authorization runs on every request, including when its index is already resident.
The service has no result or authorization cache; loaded collection indexes do not
confer access. Search rechecks after loading/recovering an index, after expensive
retrieval, and before returning results. Writes recheck after index recovery and
encoding, before committing the record. Diagnostics recheck after index recovery.
The service also checks context validity after write-through indexing completes.

Each validation observes a SQLite snapshot. A revocation observed by the final
pre-mutation check denies the write; one observed by the final result check denies
the response. Revocation after that check cannot recall an already admitted mutation
or delivered result. The final validation and record commit are not one cross-process
transaction. Process ownership prevents competing service content writers, while
local credential administration can still change grants concurrently.
If revocation happens during indexing after a record has committed, the record can
remain committed even though the request returns an authentication error. Journaled
idempotent retries recover that write only after successful reauthentication and
authorization; a key does not grant access to its saved result.

Requests serialize under a per-collection mutex and owned SQLite connection;
different collections can run concurrently within the bounded collection cache.
A server holds exclusive data-directory ownership for its lifetime. External credential
administration can revoke while encoding/retrieval runs, and regression tests exercise
that boundary. Direct concurrent content writes through the trusted `Store` API remain
unsupported; they bypass both policy and in-memory index coordination.

## Revision preconditions

`Service::put` takes `WriteOptions { publish, expected_revision, idempotency_key }`. A new record
allows no precondition or `Some(0)` (explicit absence). Existing updates require
`Some(current_revision)`; no precondition returns `RevisionRequired`, and stale
preconditions or explicit create collisions return `RevisionConflict`. An explicit
create requires contribute, while an update requires update and authorship/moderation.
`Service::delete` requires the current revision, including on repeat tombstone deletes.
Checks follow policy, precede index work, and run again inside the SQLite mutation
transaction. `DeleteOptions` and `ReportOptions` carry their preconditions/targets
and optional retry key. Each service mutation returns `Mutation<T>` containing a
durable mutation ID and its operation result. Replays revalidate current original
operation authority and use the same collection synchronization as new writes.

Retention administration authorizes scoped age-based removal of private feedback
without granting read access to that feedback. No policy runs automatically:
operators must choose and communicate retention cutoffs before scheduling jobs.
Each execution revalidates the current credential; jobs store no bearer secrets
or reusable authority. They cannot select another collection's data. Retention and
feedback deletion have no credit/balance precondition.
