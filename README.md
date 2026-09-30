# priorart

A self-hostable text store for experiences written by coding agents: what
failed, what changed, how the result was checked. An agent stores raw text.
Another agent, stuck on a problem, searches with raw text describing what it
sees and what it has tried.

The bet is that retrieving a relevant experience early reduces the tokens,
latency, and failed attempts an agent spends per completed task. The public
protocol is deliberately generic: put, get, search, delete. Everything
retrieval-specific stays behind the server and is replaceable without a
protocol change.

## What it does

- **Raw text in.** Markdown accounts, agent memory entries, transcript
  excerpts. Optional free-form JSON metadata for filtering. No mandatory
  schema.
- **Raw text queries.** Describe the current problem, context, observations,
  and failed attempts; get ranked references with query-aware excerpts.
- **BM25 retrieval** (Lucene variant) through a
  [lateweave](https://github.com/pau-mensa/lateweave) search pipeline. Each
  collection's index lives in memory and is rebuilt from SQLite when needed.
- **One file of state.** A single SQLite database. Revisions are kept; deletes
  remove text and keep a tombstone.
- **One binary.** Written in Rust; the HTTP server and the MCP client ship in
  the same `priorart` executable.

## Install and run

priorart needs a Rust toolchain (1.88 or newer). lateweave is fetched as a git
dependency, so no sibling checkout is required.

```bash
cargo install --path .        # or: cargo build --release
priorart serve                # http://127.0.0.1:8000
```

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
# {"collections":["local"],"hits":[{"id":"3f9c…","revision":1,"score":…,"excerpt":"…"}],…}

```

The full wire specification is in [docs/protocol.md](docs/protocol.md).

## Using it from a coding agent (MCP)

`priorart mcp` is a stdio MCP server that forwards to a running `priorart
serve`. It exposes `search_experiences`, `get_experience`,
`contribute_experience`, and `delete_experience`, with tool descriptions that
tell the agent what to put in a query and what a useful contribution contains.

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

The MCP process is deliberately a client, not an embedded store: one server owns
the data directory and its in-memory indexes, so every agent session must go
through it.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `PRIORART_MODE` | `local` | `local` identity or `authenticated` header access; both loopback-only; `hosted` is disabled |
| `PRIORART_DATA_DIR` | `./data` | SQLite database location |
| `PRIORART_MAX_LOADED_INDEXES` | `8` | cached collections and their in-memory indexes; idle LRU eviction, busy admission returns 429 |
| `PRIORART_MAX_TOKENS` | `8192` | document cutoff in analyzer terms; longer text is truncated before storing |
| `PRIORART_HOST` / `PRIORART_PORT` | `127.0.0.1` / `8000` | loopback bind address |

## Local credentials

`priorart admin issue --grant local:read` issues an opaque agent credential for
the local principal and prints its secret once. Local commands also create
principals and collections, and list, rotate, revoke, and replace grants. See [credential administration](docs/credentials.md).
The Rust service enforces [collection and object policy](docs/policy.md) on explicit
request contexts. Start with `PRIORART_MODE=authenticated priorart serve` to require
header credentials for restricted content and mutations. Public reads can be anonymous.
Send credentials only in `Authorization: Bearer …`; invalid supplied keys always fail.
The default local mode grants requests without credentials access to `local` only.
`priorart admin create-collection` provisions a restricted collection; add
`--visibility public` for an explicitly public collection. The bundled MCP client
currently targets local mode.

## Limitations of this version

For database upgrades and backups, see [storage](docs/storage.md).

- Hosted mode is disabled. Local and authenticated HTTP modes are loopback-only;
  do not expose them through a proxy. Per-request limits exist, but rate limiting,
  hosted admission, and full deletion guarantees remain future work.
- HTTP requests select explicit collections; search spans up to 16 with shared
  BM25 statistics. Storage is access-controlled by the trusted service and remains
  unencrypted.
- One indexed view per record. Text beyond `PRIORART_MAX_TOKENS` is truncated
  before storing, so stored and indexed text match; there is no chunking.
  The cutoff counts the analyzer's terms (casefolded `\w+` runs).
- One server owns each data directory. Writes update the in-memory index under a
  per-collection lock; different collections can run concurrently within capacity.
- Record mutations are journaled in SQLite; HTTP retry keys avoid duplicate
  mutations after lost responses. Backups and physical storage erasure are separate.
- Searches persist nothing: no query, filter, or result list.

Collection revisions can be streamed through the [export/import API](docs/protocol.md#streaming-export-and-import).
Imports require explicit destination visibility and assign new authorship; retries
resume within a batch without duplicating revisions.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. lateweave is Apache-2.0.
