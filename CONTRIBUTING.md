# Contributing

## Setup

priorart is a Rust crate built on [lateweave](https://crates.io/crates/lateweave).

```bash
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

## Where things live

| Module | Responsibility |
|---|---|
| `store` | SQLite: principals, collections, records, revisions, credentials, mutation journal |
| `store::migrations` | transactional schema versions |
| `recipe` | the retrieval seam: how collections are indexed and ranked ([recipes](docs/recipes.md)) |
| `index` | a collection's loaded recipe index and the revision of each record in it |
| `gather` | the built-in BM25 recipe and its lateweave candidate generator |
| `analyzer`, `excerpt` | terms for BM25 and query-aware excerpts |
| `service` | the protocol operations, transport-independent |
| `api` | axum routes, peer admission, and the error envelope; `api::admin` for the operator endpoints, `api::limit` for per-key limits |
| `mcp` | stdio MCP tools forwarding to the HTTP API |
| `cli` | the `priorart serve` / `admin` / `mcp` command line; `main.rs` runs it with BM25 |

The wire protocol is `docs/protocol.md`. Schema changes follow
[the storage migration guidance](docs/storage.md).

## Ground rules

- The public protocol is raw text in, ranked references out. Do not add
  required structured fields to it; derived structure belongs behind the
  server boundary.
- Anything derived from a contribution (chunks, summaries, generated
  symptoms) must reference the record and revision it came from and must
  never be returned as if it were the contributor's own text.
- Tests first. A change to storage or index behaviour needs a test that would
  have failed before it.
