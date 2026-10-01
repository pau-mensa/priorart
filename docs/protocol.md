# priorart HTTP protocol v1

Raw text in, ranked references out, with explicit collection scope. Bodies are
JSON; unknown body and query fields are rejected. Identifiers match
`^[A-Za-z0-9_.:-]{1,128}$`, excluding `.` and `..`. Collection IDs are generated
by the server; `local` is built in.

## Authentication

`PRIORART_MODE=local` (the default): a request without a key acts as the local
principal, confined to collection `local`. `PRIORART_MODE=authenticated`: a
request without a key is anonymous and can only read public collections. In
either mode a supplied key replaces that default:

```http
Authorization: Bearer pa1_<lookup-id>_<secret>
```

A malformed, expired, revoked, or unknown key returns `401`; it never falls back
to local or anonymous access. Keys are accepted only in this header. What each
key may do is in [authorization](policy.md); how keys are issued is in
[credentials](credentials.md).

## Network hosting

By default the server binds and serves loopback only, and `local` mode never
serves anything else. In `authenticated` mode, a non-loopback `PRIORART_HOST`
requires one of:

- `PRIORART_TRUSTED_PROXIES`: comma-separated IPs or CIDRs of TLS-terminating
  proxies. A request from one of them must carry exactly one
  `X-Forwarded-Proto: https`, or it gets `403 https_required`.
- `PRIORART_ALLOW_INSECURE_HTTP=true`: plain HTTP from any peer. Only for a
  network you trust, since keys then cross it in the clear.

Any other non-loopback peer gets `403 https_required`. `X-Forwarded-Proto` is the
only forwarding header read. Other forwarding headers from trusted proxies are
ignored; from any other peer, any forwarding header returns `400 untrusted_proxy`.
A proxy on the same host connects from loopback, so list `127.0.0.1` (or `::1`);
direct local clients on that address, health checks included, must then send
`X-Forwarded-Proto: https` too. The proxy must overwrite, not append to,
`X-Forwarded-Proto`. Caddy and Traefik do this by default; with nginx, also raise
the body limit to the server's own (`256 * PRIORART_MAX_TOKENS + 65536` bytes,
128 MiB for imports):

```nginx
location / {
    proxy_pass http://127.0.0.1:8000;
    proxy_set_header X-Forwarded-Proto $scheme;
    client_max_body_size 2200k;
}
location ~ ^/v1/collections/[^/]+/import$ {
    proxy_pass http://127.0.0.1:8000;
    proxy_set_header X-Forwarded-Proto $scheme;
    client_max_body_size 130m;
}
```

`PRIORART_KEY_REQUESTS_PER_MINUTE` limits each key separately; a key over its
limit gets `429 rate_limited` with `Retry-After`. Anonymous and operator requests
are not limited, and limits reset on restart. Other abuse handling (per-IP
limits, quotas, blocking) belongs in front of priorart. Serving a public
collection to the internet needs at least a per-IP limit, for example nginx's
`limit_req` or a Cloudflare rate-limiting rule. Behind Cloudflare, Browser Integrity
Check refuses some scripted clients (Python's `urllib` gets `403`, error 1010);
disable it for priorart's hostname. [`deploy/`](../deploy) has a Docker setup.

## Collections

`GET /v1/collections?limit=50&after=ID` returns the collections the caller can
see (public ones, plus any it holds a grant on), sorted by ID:

```json
{"collections": [{"id": "…", "visibility": "restricted", "created_at": "…"}]}
```

`limit` is 1–100; pass the last ID as `after` for the next page.
`GET /v1/collections/{collection}` returns one summary.
`GET /v1/collections/{collection}/diagnostics` requires `admin` and returns
`{"status":"ok","document_count":N}`.

`DELETE /v1/collections/{collection}` requires `admin`. It removes the
collection's records, receipts, and grants and returns `204`; afterwards the
collection is `404`. Collection IDs are never reused.

## Create or update a record

`POST /v1/collections/{collection}/records`

```json
{"text": "…", "metadata": {"lang": "python"}, "id": "optional-client-id",
 "expected_revision": 0}
```

Returns `201 {"collection_id":"…","id":"…","revision":1,"truncated":false}`.

- Omit `id` to have one generated.
- `expected_revision` is optional. Omitted, the write creates the record or
  appends a revision (last write wins). `0` requires absence; a positive value
  must equal the current revision. A mismatch returns `409` and writes nothing.
- Creating needs `write`; updating needs `write` and authorship, or `admin`.
- Text must be nonempty. Text beyond `PRIORART_MAX_TOKENS` (default 8192) is cut
  after the last token that fits, and the response reports `"truncated": true`.
- Metadata is a JSON object of at most 65536 bytes, depth 8, and 1024 values.

A successful write is searchable immediately.

## Mutation retries

Puts and deletes accept an optional `Idempotency-Key` header (1–128 characters
from letters, digits, and `._:-`). Use a fresh key per intended change and keep it
across retries. A retry with the same key returns the original result, even if
the first response was lost; reusing a key with different input returns
`409 idempotency_conflict`. Keys are scoped to the principal, collection, and
operation. A retry still needs current authorization. Retrying a key whose record
has since been deleted returns `410 mutation_purged`. Keys do not expire.

Every successful mutation returns a `Mutation-Id` header.

## Read, list, and delete

`GET /v1/collections/{collection}/records/{id}?revision=N` returns
`{"collection_id","id","revision","text","metadata","truncated","created_at"}`,
latest revision unless `revision` is given.

`GET /v1/collections/{collection}/records?author=me&limit=50&after=ID&include=text`
lists live records in ID order, latest revision each. All parameters are
optional:

- `author=me` keeps the caller's own records. It requires a key (or local mode),
  otherwise `400`. Authorship survives key rotation and revocation.
- `limit` is 1–100 (default 50); `after` is the last ID of the previous page.
- `include=text` adds each record's text.

Returns `{"collection_id","records":[{"id","revision","created_at","updated_at","metadata","truncated"}]}`.

`DELETE /v1/collections/{collection}/records/{id}?expected_revision=N` requires
`write` and authorship, or `admin`. `expected_revision` works as for writes.
Returns `204`, also when repeated. Deletion removes every revision and leaves a
tombstone: the ID cannot be reused, and reading it returns `410`. Deleted data may
remain in backups and on disk; see [storage](storage.md#deletion).

## Search

`POST /v1/search`

```json
{"collections": ["local"], "text": "problem, observations, failed attempts…",
 "filters": {"lang": "python"}, "limit": 10}
```

Returns:

```json
{"collections": ["local"],
 "hits": [{"collection_id": "local", "id": "…", "revision": 2, "score": 12.3,
           "excerpt": "…", "metadata": {"lang": "python"}}]}
```

- `collections` names 1–16 distinct collections, and no more than
  `PRIORART_MAX_LOADED_INDEXES`. The caller must be able to read every one; a
  single unknown or forbidden ID rejects the search with `404`.
- `text` is nonempty and at most 16384 bytes. `limit` is 1–100 (default 10).
- `filters` are up to 64 scalar equalities on metadata (strings, numbers,
  booleans), combined with AND, at most 16384 bytes.

With the built-in [recipe](recipes.md), scores are Lucene BM25 with statistics
taken over the whole selection, so scores from different collections compare.
Hits are merged by score; ties go to collection ID, then record ID. Searches store nothing unless the search log is
enabled.

## Search log and feedback

`PRIORART_SEARCH_LOG_DAYS=N` enables the search log, kept in `searchlog.sqlite`
next to the database. Each search is then stored with its query, filters,
collections, limit, time, a salted hash of the caller's principal (none for
anonymous callers), and the hits returned as collection ID, record ID, revision,
and score (never record text). The response carries a `search_id`.

Logging never delays or fails a request. Searches and ratings are queued for a
background writer; when the queue is full or a write fails, entries are dropped
and counted in `priorart_search_log_dropped_total` (see [metrics](#metrics)), and
a search that could not be queued has no `search_id`. Searches older than N days
are deleted with their ratings, at startup and then hourly. Deleting a collection
deletes every search that included it. Unsetting the variable stops logging but
leaves `searchlog.sqlite` in place.

`POST /v1/search/{search_id}/feedback` rates hits of that search:

```json
{"ratings": [{"collection_id": "local", "id": "…", "useful": true}]}
```

It needs a key (or local mode) and returns `202`: the ratings are queued, not
yet stored. 1–100 ratings are required (otherwise `400`), and a disabled log
returns `404`. The writer keeps a rating only if the search exists, was run by
the same principal, and returned that hit; others are dropped and counted as
`rejected_feedback`. Rating a hit again replaces the earlier rating.

`GET /v1/admin/search-log?after=0&limit=1000` takes the
[admin token](credentials.md#remote-administration) and returns
`application/x-ndjson`: what the writer has stored so far, oldest first, then a
footer:

```json
{"type":"search","id":"…","at":"…","principal":"…","collections":["local"],"query":"…","filters":null,"limit":10,"hits":[{"collection_id":"local","id":"r1","revision":2,"score":12.3}],"feedback":[{"collection_id":"local","id":"r1","useful":true,"at":"…"}]}
{"type":"end","count":1,"next_cursor":null}
```

`limit` is 1–10000. When `next_cursor` is not null, pass it as `after` for the
next page.

## Export and import

`GET /v1/collections/{collection}/export?limit=50` requires `admin`. It streams
`application/x-ndjson`: every revision of every live record, ordered by
`(record_id, revision)`, one per line, then a footer:

```json
{"type":"revision","record":{"version":1,"collection_id":"source","visibility":"restricted","record_id":"r1","revision":2,"author_principal_id":"author","created_at":"2026-01-01T00:00:00Z","text":"record text","metadata":null}}
{"type":"end","count":1,"generation":7,"next_cursor":null}
```

`limit` is 1–10000 revisions per page (pages are also capped at 128 MiB). When
`next_cursor` is not null, continue with `after_record`, `after_revision`, and
the footer's `generation`. If the collection changes during an export, the stream
fails, or the next page returns `409 export_changed`; restart the export.

`POST /v1/collections/{collection}/import?overwrite=false` takes that format with
`Content-Type: application/x-ndjson` and a required `Idempotency-Key` naming the
batch. It requires `write` on an existing destination.

- Records keep their source IDs. By default source revision N must land as
  revision N, so an ID that already exists stops the import with
  `409 revision_conflict`.
- With `overwrite=true`, each row is appended as the record's next revision,
  creating it if absent. Changing another author's record still needs `admin`.
- Deleted IDs cannot be recreated in either mode.
- The importer becomes the author, with new timestamps; uploaded author,
  timestamps, and visibility are ignored.

Each row is acknowledged once committed and searchable:

```json
{"type":"imported","source":{"collection_id":"source","record_id":"r1","revision":2},"collection_id":"destination","record_id":"r1","revision":2,"mutation_id":"mutation-id"}
{"type":"end","count":1}
```

An upload holds at most 10000 rows and 128 MiB, and must end with the export's
`end` footer carrying the right count. The server reads the whole upload before
responding, so an invalid row or footer returns an error status and writes nothing;
this also keeps imports working behind proxies that stop forwarding a body once the
response starts. Rows then commit one at a time: after a failure,
retry the same input with the same batch key; committed rows are not duplicated.
Changing a row or the mode under the same key conflicts.

Failures after the response starts arrive as a final
`{"type":"error","error":{"code":"…","message":"…"}}` line. Only an `end` line
means the stream completed.

Exports move content, not databases: they omit tombstones and authorship. For
backups, see [storage](storage.md#backups-and-upgrades).

## Errors and health

Errors have fixed messages that never echo request contents:

```json
{"error":{"code":"not_found","message":"requested resource is unavailable"}}
```

| Status | Code | Meaning |
|---|---|---|
| 400 | `invalid_input` | invalid scope, identifier, revision, content, or limit |
| 400 | `untrusted_proxy` | a forwarding header from a peer not in `PRIORART_TRUSTED_PROXIES` |
| 401 | `unauthenticated` | invalid key, or a mutation without one |
| 403 | `https_required` | plain HTTP from a non-loopback peer, or a trusted proxy that did not report HTTPS |
| 404 | `not_found` | unknown or forbidden resource, or unknown route |
| 405 | `method_not_allowed` | unsupported method |
| 409 | `revision_conflict` | `expected_revision` mismatch, or an import ID collision |
| 409 | `idempotency_conflict` | idempotency key reused with different input |
| 409 | `export_changed` | collection changed between export pages |
| 410 | `record_deleted` | the record was deleted |
| 410 | `mutation_purged` | retry of a key whose record was since deleted |
| 413 | `payload_too_large` | body over `256 * PRIORART_MAX_TOKENS + 65536` bytes |
| 422 | `validation_error` | malformed JSON or query, wrong types, missing or unknown fields |
| 429 | `resource_limit` | every cached collection is busy |
| 429 | `rate_limited` | the key exceeded its per-minute limit; see `Retry-After` |
| 503 | `unavailable` | backend failure |

`GET /healthz` returns `{"status":"ok"}`.

## Metrics

`GET /metrics` serves the Prometheus text format to any caller admitted by the
network rules, with or without a key. It holds only server-wide search figures,
so it reveals nothing about collections:

- `priorart_search_duration_seconds`: histogram of successful search latency,
  for `histogram_quantile` in Prometheus.
- `priorart_search_duration_recent_seconds{quantile="0.5|0.95|0.99"}`: the
  same quantiles over the last five minutes (at most 10000 searches), `NaN`
  when there were none.
- `priorart_searches_total{outcome="ok|client_error|server_error"}`.

`GET /v1/admin/metrics` takes the [admin token](credentials.md#remote-administration)
and adds requests and latency by route, method, and status; ranking time;
searches, writes, and indexed documents per collection; index load time and
failed index updates; cached collections;
evictions; `resource_limit` and `rate_limited` refusals; and, with the
[search log](#search-log-and-feedback) enabled, entries it dropped by reason
(`queue_full`, `write_failed`, `rejected_feedback`). Collection labels
disappear when the collection is deleted. Counters reset on restart.

## MCP client

`priorart mcp` is a stdio MCP server that forwards tool calls to `PRIORART_URL`.

| Variable | Default | Meaning |
|---|---|---|
| `PRIORART_URL` | `http://127.0.0.1:8000` | server; HTTPS unless loopback |
| `PRIORART_KEY` | unset | sent as `Authorization: Bearer` |
| `PRIORART_COLLECTIONS` | `local` | default search scope, comma-separated |
| `PRIORART_WRITE_COLLECTION` | the search scope, if it is one collection | default destination |
| `PRIORART_ALLOW_INSECURE_HTTP` | `false` | allow a non-loopback `http://` URL |

`search_experiences` takes an optional `collections` scope; the other tools take an
optional `collection_id`, the same field hits carry for follow-up reads. Unknown
arguments are rejected. When the server logs searches, `search_experiences` returns
the `search_id` and `rate_hits` sends the agent's ratings. The key never appears in tool schemas, results, or errors.
The URL must not embed credentials, redirects are not followed, and the key is
checked against `/healthz` at startup. Transport failures and `502`–`504` are
retried up to three times; each write keeps one `Idempotency-Key` across its
attempts.
