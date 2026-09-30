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

priorart needs a Rust toolchain (1.88 or newer).

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
# {"collection_id":"local","id":"3f9c…","revision":1,"truncated":false}

curl -s localhost:8000/v1/search -H 'content-type: application/json' -d '{
  "collections": ["local"],
  "text": "multi-GPU training hangs at the end of the first epoch, no error, GPU util drops to zero",
  "limit": 3
}'
# {"collections":["local"],"hits":[{"collection_id":"local","id":"3f9c…","revision":1,"score":…,"excerpt":"…","metadata":{…}}]}
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

Against an authenticated server, give it a key and a scope:

```bash
claude mcp add --transport stdio \
  --env PRIORART_URL=https://priorart.example.com \
  --env PRIORART_KEY=pa1_… \
  --env PRIORART_COLLECTIONS=team-notes,public-fixes \
  --env PRIORART_WRITE_COLLECTION=team-notes \
  priorart -- priorart mcp
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

The MCP process is a client, not an embedded store: one server owns the data
directory and its in-memory indexes, so every agent session goes through it. See
[MCP client](docs/protocol.md#mcp-client) for its settings.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `PRIORART_MODE` | `local` | `local` identity (loopback only) or `authenticated` header access |
| `PRIORART_DATA_DIR` | `./data` | SQLite database location |
| `PRIORART_MAX_LOADED_INDEXES` | `8` | cached collections and their in-memory indexes; idle LRU eviction, busy admission returns 429 |
| `PRIORART_MAX_TOKENS` | `8192` | document cutoff in analyzer terms; longer text is truncated before storing |
| `PRIORART_HOST` / `PRIORART_PORT` | `127.0.0.1` / `8000` | bind address; non-loopback needs one of the next two |
| `PRIORART_TRUSTED_PROXIES` | unset | IPs/CIDRs of TLS-terminating proxies; they must send `X-Forwarded-Proto: https` |
| `PRIORART_ALLOW_INSECURE_HTTP` | `false` | accept plain HTTP from any peer (trusted networks only); lets the MCP client use a non-loopback `http://` URL |
| `PRIORART_KEY_REQUESTS_PER_MINUTE` | unset | per-key request limit; over it returns 429 |
| `PRIORART_ADMIN_TOKEN` | unset | enables the operator endpoints under `/v1/admin`; 32–256 characters |

## Keys and authenticated mode

`PRIORART_MODE=authenticated priorart serve` requires a key for restricted
collections and for every write; public collections stay readable without one.
Keys are issued by the operator:

```bash
priorart admin create-principal
priorart admin create-collection --owner PRINCIPAL_ID
priorart admin issue --principal PRINCIPAL_ID --grant COLLECTION:write
```

With `PRIORART_ADMIN_TOKEN` set, the same provisioning is available over HTTP
under `/v1/admin`. See [credentials](docs/credentials.md) and
[authorization](docs/policy.md).

## Limitations

- priorart does not terminate TLS; serve it beyond loopback behind a proxy (see
  [network hosting](docs/protocol.md#network-hosting)).
- Storage is unencrypted, and deletion does not erase backups or free disk pages.
- Text beyond `PRIORART_MAX_TOKENS` is truncated, not chunked.
- One server owns each data directory; its indexes live in memory.

See [storage](docs/storage.md) for backups and upgrades.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. lateweave is Apache-2.0.
