# Service authorization

Every content operation on `Service` now takes an explicit `RequestContext` and
collection ID. There is no default collection or omitted-scope search in the Rust
service API. HTTP/MCP still use the explicit local principal and `local` collection;
they do not yet accept credentials or expose collection selection. Hosted operation
remains unavailable pending the later transport, privacy, and deletion steps.

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
| Create report | `report` plus target read access; attached receipt must belong to requester in the same collection |
| List reports | `feedback_read` plus target read access; only the requester's rows are loaded |
| Scoped health/diagnostics | `admin` on that collection, before loading its index or counting documents |

Ownership and `admin` do not imply other credential operations. Moderation preserves
original record authorship. Public visibility grants readability, not write or
moderation permission. Creating or updating public record text additionally requires
`publish: true`; the flag grants no authority by itself. Restricted visibility remains
immutable, and no operation automatically republishes restricted content.

Reports remain private even on public records. An owner/moderator cannot read another
requester's reports. Deleted targets do not expose reports through the service.
An omitted report revision currently resolves to the latest live revision; explicit
revision requirements and verifying receipt hit membership are later protocol/privacy
work. Unknown and someone else's receipt IDs produce the same missing-receipt error.

Anonymous searches return `search_id: None` and persist no query or receipt.
Authenticated/local searches still use the existing raw query log. Minimal receipts,
retention, complete deletion, export, and public report publication are later steps.
No new export, collection-management, or background-job API is introduced here.

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
transaction; durable mutation coordination and process ownership remain step 8.
If revocation happens during indexing after a record has committed, the record can
remain committed even though the request returns an authentication error. Idempotent
retry handling is likewise deferred to step 8.

Requests within one `Service` remain serialized under its mutex. External credential
administration can revoke while encoding/retrieval runs, and regression tests exercise
that boundary. Direct concurrent content writes through the trusted `Store` API remain
unsupported; they bypass both policy and in-memory index coordination.
