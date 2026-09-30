# Authorization

Access is deny-by-default and checked on every request against current SQLite
state; there is no authorization cache.

## Identities

- A **key** authenticates as its principal and carries per-collection grants.
- **Anonymous** requests (authenticated mode, no key) can read public collections.
- The **local** identity (local mode, no key) is the built-in `local-principal`,
  confined to collection `local`.

Unknown and forbidden collections return the same `404`, so a caller cannot learn
whether a collection it may not read exists. A reader can tell a missing record
(`404`) from a deleted one (`410`).

## Grants

There are three grants, each implying the ones before it: `read`, `write`, `admin`.

| Operation | Requires |
|---|---|
| Get, list, search | Public visibility or `read` |
| Create a record | `write` |
| Update or delete a record | `write` and authorship, or `admin` |
| Import | `write`; changing an existing record needs authorship or `admin` |
| Export, diagnostics, delete collection | `admin` |
| List collections | Public, or any grant |

A key may hold any grant on collections its principal owns, and `read` or `write`
on any public collection, so `admin` belongs to the owner only. Ownership and
metadata grant nothing by themselves; the key's grants do. Changing another
author's record keeps its original authorship. Visibility is immutable.
