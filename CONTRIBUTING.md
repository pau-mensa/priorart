# Contributing

## Setup

priorart is a Rust crate; lateweave is a git dependency pinned in `Cargo.toml`.

```bash
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

## Where things live

| Module | Responsibility |
|---|---|
| `store` | SQLite: principals, collections, records, revisions, credentials, mutation journal |
| `store::migrations` | transactional schema versions |
| `index` | a collection's in-memory BM25 index and its lateweave pipeline |
| `gather` | BM25 and its lateweave candidate generator |
| `analyzer`, `excerpt` | terms for BM25 and query-aware excerpts |
| `service` | the protocol operations, transport-independent |
| `api` | axum routes and the error envelope |
| `mcp` | stdio MCP tools forwarding to the HTTP API |
| `main.rs` | the `priorart serve` / `priorart mcp` CLI |

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
