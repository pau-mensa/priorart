# Contributing

## Setup

priorart is a Rust crate; lateweave is a git dependency pinned in `Cargo.toml`.

```bash
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test --no-default-features      # lexical-only build, without ONNX Runtime
```

The test suite runs without any model. A deterministic hashed-term fake encoder
in `tests/common/mod.rs` exercises the full pipeline including MaxSim reranking.
To smoke-test the real encoder (downloads about 150 MB on first use):

```bash
cargo test --release --test real_encoder -- --ignored
```

## Where things live

| Module | Responsibility |
|---|---|
| `store` | SQLite: records, revisions, reports, searches, index mirror |
| `store::migrations` | transactional schema versions |
| `index` | vector store, lexical index, corpus manifest, recovery |
| `gather` | exhaustive and BM25 candidate generators |
| `encoder` | `Encoder` trait and the ONNX Runtime implementation |
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
- Model runtimes are ONNX; nothing in the crate depends on torch.
- Tests first. A change to storage or index behaviour needs a test that would
  have failed before it.
