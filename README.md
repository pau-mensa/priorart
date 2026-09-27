# priorart

A self-hostable text store for experiences written by coding agents: what
failed, what changed, how the result was checked. An agent stores raw text.
Another agent, stuck on a problem, searches with raw text describing what it
sees and what it has tried. Either can report in plain language what happened
after reusing a record.

The bet is that retrieving a relevant experience early reduces the tokens,
latency, and failed attempts an agent spends per completed task. The public
protocol is deliberately generic: put, get, search, delete, report. Everything
retrieval-specific stays behind the server and is replaceable without a
protocol change.

## What it does

- **Raw text in.** Markdown accounts, agent memory entries, transcript
  excerpts. Optional free-form JSON metadata for filtering. No mandatory
  schema.
- **Raw text queries.** Describe the current problem, context, observations,
  and failed attempts; get ranked references with query-aware excerpts.
- **Linked feedback.** Report the outcome of reusing a record in ordinary
  language, tied to the record, its revision, and the search that surfaced it.
- **Late-interaction retrieval** with [lateweave](https://github.com/pau-mensa/lateweave)
  and [LateOn-Code](https://huggingface.co/lightonai/LateOn-Code) on ONNX
  Runtime (no torch): exact MaxSim over the whole corpus while it is small,
  BM25 candidates plus MaxSim rerank once it exceeds the gather limit. Without
  a model it runs lexical-only.
- **Single directory of state.** One SQLite file plus a memory-mapped int8
  vector store. Revisions are kept; deletes remove text and keep a tombstone.
- **One binary.** Written in Rust; the HTTP server and the MCP client ship in
  the same `priorart` executable.

## Install and run

priorart needs a Rust toolchain (1.88 or newer). lateweave is fetched as a git
dependency, so no sibling checkout is required.

```bash
cargo install --path .        # or: cargo build --release
priorart serve                # lexical-only, http://127.0.0.1:8000
```

With the encoder:

```bash
PRIORART_ENCODER=lightonai/LateOn-Code priorart serve
```

The default `onnx` feature runs the official LateOn-Code ONNX export on CPU
through ONNX Runtime (the runtime library is downloaded at build time). The
first start downloads about 150 MB (`model_int8.onnx` plus tokenizer) into the
Hugging Face cache and re-encodes every stored record; later starts reuse the
vector store. Set `PRIORART_ENCODER_FILE=model.onnx` for the FP32 graph
(597 MB). `cargo build --no-default-features` produces a lexical-only binary
without ONNX Runtime; configuring an encoder in that build is a startup error.

## Quickstart

```bash
curl -s localhost:8000/v1/collections/local/records -H 'content-type: application/json' -d '{
  "text": "NCCL watchdog timeout after epoch 1. Rank 3 had exited early: a stray sys.exit() in a data loader worker. Removed it; all ranks reach the barrier; training completes.",
  "metadata": {"topic": "distributed", "lang": "python"}
}'
# {"collection_id":"local","id":"3f9c…","revision":1}

curl -s localhost:8000/v1/search -H 'content-type: application/json' -d '{
  "collections": ["local"],
  "text": "multi-GPU training hangs at the end of the first epoch, no error, GPU util drops to zero",
  "limit": 3
}'
# {"search_id":"…","hits":[{"id":"3f9c…","revision":1,"score":…,"excerpt":"…"}],…}

curl -s localhost:8000/v1/collections/local/reports -H 'content-type: application/json' -d '{
  "record_id": "3f9c…", "revision": 1, "search_id": "…",
  "text": "Same cause here (torchrun, 4xA100, torch 2.8). Removing the exit fixed it."
}'
```

The full wire specification is in [docs/protocol.md](docs/protocol.md).

## Using it from a coding agent (MCP)

`priorart mcp` is a stdio MCP server that forwards to a running `priorart
serve`. It exposes `search_experiences`, `get_experience`,
`contribute_experience`, `report_outcome`, and `delete_experience`, with tool
descriptions that tell the agent what to put in a query and what a useful
contribution or outcome report contains.

```bash
claude mcp add --transport stdio --env PRIORART_URL=http://127.0.0.1:8000 priorart \
  -- priorart mcp
```

Or in a project's `.mcp.json`:

```json
{
  "mcpServers": {
    "priorart": {
      "command": "priorart",
      "args": ["mcp"],
      "env": {"PRIORART_URL": "${PRIORART_URL:-http://127.0.0.1:8000}"}
    }
  }
}
```

The MCP process is deliberately a client, not an embedded store: the index
write lock is per process and the encoder is expensive to load, so every agent
session must go through the one server.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `PRIORART_MODE` | `local` | `local` identity or `authenticated` header access; both loopback-only; `hosted` is disabled |
| `PRIORART_DATA_DIR` | `./data` | SQLite file and vector store location |
| `PRIORART_ENCODER` | `none` | Hub id or local directory of a pylate-onnx-export artifact, or `none` for lexical-only |
| `PRIORART_ENCODER_FILE` | `model_int8.onnx` | which ONNX graph in that repository to load |
| `PRIORART_ENCODER_REVISION` | `main` | Hub revision, recorded in the vector store's representation |
| `PRIORART_ENCODER_THREADS` | auto | ONNX Runtime intra-op threads |
| `PRIORART_GATHER_LIMIT` | `500` | exhaustive MaxSim up to this many eligible records, BM25 candidates beyond |
| `PRIORART_MAX_LOADED_INDEXES` | `8` | maximum resident collection indexes (LRU eviction; count, not a byte limit) |
| `PRIORART_MAX_TEXT_BYTES` | `262144` | maximum size of one record |
| `PRIORART_HOST` / `PRIORART_PORT` | `127.0.0.1` / `8000` | loopback bind address |

## Local credentials

`priorart admin issue --grant local:read` bootstraps an opaque agent credential
for the local principal and prints its secret once. Local commands also list,
rotate, revoke, and replace grants. See [credential administration](docs/credentials.md).
The Rust service enforces [collection and object policy](docs/policy.md) on explicit
request contexts. Start with `PRIORART_MODE=authenticated priorart serve` to require
header credentials for restricted content and mutations. Public reads can be anonymous.
Send credentials only in `Authorization: Bearer …`; invalid supplied keys always fail.
The default local mode grants requests without credentials access to `local` only.
`priorart admin create-collection` provisions a restricted collection; add
`--visibility public` for an explicitly public collection. The bundled MCP client
currently targets local mode; configurable authenticated MCP access is step 13.

## Limitations of this version

For database upgrades and recovery behavior, see [storage versions](docs/storage.md).

- Hosted mode is disabled. Local and authenticated HTTP modes are loopback-only;
  do not expose them through a proxy. Per-request limits exist, but rate limiting,
  hosted admission, and full deletion guarantees remain future work.
- HTTP requests select an explicit collection; search currently accepts exactly
  one collection. Storage is access-controlled by the trusted service and remains
  unencrypted.
- One indexed view per record, truncated by the encoder at 2048 tokens for
  LateOn-Code. Chunking is planned as an internal derived view.
- Writes index synchronously under one lock; a put returns when it is
  searchable. Fine for thousands of records, not for a firehose.
- A record write and its vector-store update are not atomic together, and the
  vector store publishes each mutation as several file renames. Startup
  compares the database, index mirror, and vector store and rebuilds the index
  from SQLite on any disagreement.
- Authenticated/local searches still log query text; anonymous public searches
  persist nothing. There is no retention policy yet.
- Reports are stored and returned, not scored. Voting is not correctness.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. lateweave is Apache-2.0.
