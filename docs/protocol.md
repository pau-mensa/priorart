# priorart wire protocol v1

Raw text in, ranked references out, with optional linked feedback. Nothing here
requires embeddings, a particular model, or a retrieval pipeline; those live
behind the server.

The HTTP API below is the reference transport. `priorart mcp` exposes the same
operations as MCP tools (`search_experiences`, `get_experience`,
`contribute_experience`, `report_outcome`, `delete_experience`) by forwarding
to a running server; it adds no operations and no fields.

V1 is the local single-collection interface. All routes use collection `local`;
record IDs in this protocol are relative to that collection. The collection-aware
storage layer does not add remote collection selection, authentication, or hosted
access. See [storage versions and upgrades](storage.md) for migration details.

All bodies are JSON. Errors use one envelope:

```json
{"error": {"code": "invalid_input", "message": "text must be a non-empty string"}}
```

| Status | Code | When |
|---|---|---|
| 400 | `invalid_input` | a protocol rule was violated (empty text, bad id, limit out of range, non-scalar filter) |
| 404 | `record_not_found` | unknown record or revision |
| 404 | `search_not_found` | a report names an unknown `search_id` |
| 410 | `record_deleted` | the record exists only as a tombstone |
| 422 | `validation_error` | the body does not match the schema |

## put — `POST /v1/records`

```json
{"text": "…", "metadata": {"lang": "python"}, "id": "optional-client-id"}
```

Returns `201 {"id": "…", "revision": 1}`.

- Without `id` the server creates a record at revision 1 with a 32-character
  hex id.
- With an `id` that exists, a new revision is appended and returned.
- With an unknown `id` matching `^[A-Za-z0-9_.:-]{1,128}$`, the record is
  created under that id.
- `text` must be non-empty after stripping and at most `PRIORART_MAX_TEXT_BYTES`
  (default 262144) bytes of UTF-8.
- `metadata` is any JSON object. Only top-level string, number, and boolean
  values can be used in search filters.

Contributions can be Markdown accounts, agent memory entries, or transcript
excerpts. Include what failed, what changed, and how the result was checked.
That is guidance, not schema.

## get — `GET /v1/records/{id}?revision=N`

Returns `{"id", "revision", "text", "metadata", "created_at"}`. Latest revision
unless `revision` is given.

## delete — `DELETE /v1/records/{id}`

Returns 204. Removes the text and metadata of every revision and drops the
record from the index. A tombstone keeps the id, the revision numbers, and each
revision's SHA-256, so reports remain linked and the id cannot be reused.
Deleting twice is a 204; deleting an unknown id is a 404.

## search — `POST /v1/search`

```json
{"text": "the problem, context, observations, failed attempts…",
 "filters": {"lang": "python"}, "limit": 10}
```

Returns:

```json
{"search_id": "…",
 "hits": [{"id": "…", "revision": 2, "score": 12.3,
           "score_semantics": "int8-reconstructed-approximate-full-maxsim",
           "excerpt": "…", "metadata": {"lang": "python"}}],
 "timings": {"gather_seconds": 0.0, "rerank_seconds": 0.0, "total_seconds": 0.0},
 "gatherer": "exhaustive"}
```

- The query is raw text. Put the current problem, relevant context, observed
  symptoms, and what was already tried into it.
- `filters` is a flat object of metadata equalities combined with AND, matched
  against each record's latest revision.
- `limit` defaults to 10, maximum 100.
- `score_semantics` names the stage that ranked the hits so scores from
  different configurations are never compared blindly.
- `gatherer` is `exhaustive` when every eligible record was scored by the
  reranker, `bm25` when a lexical candidate stage ran first, and `none` when
  no record was eligible. Lexical-only servers report `score_semantics`
  `bm25-lucene`.
- Every search is logged with its `search_id`, query, filters, and returned
  hits so a later report can be joined to the query that surfaced a record.

## report — `POST /v1/reports`

```json
{"record_id": "…", "revision": 2, "search_id": "…",
 "text": "Applied the padding fix on CUDA 12.4; the kernel no longer faults, tests pass."}
```

Returns `201 {"id": "…"}`. `revision` and `search_id` are optional; an unknown
`search_id` is a 404. A supplied revision must exist in the local record's history,
otherwise it is a `record_not_found` 404. Reports on deleted records are accepted. There is no
vote, success label, or outcome taxonomy: say what environment received the
change, what was applied, what check passed or failed, and any side effects.

## list reports — `GET /v1/records/{id}/reports`

Returns `{"reports": [{"id", "record_id", "revision", "search_id", "text", "created_at"}]}`
in creation order.

## health — `GET /healthz`

Returns `{"status": "ok", "document_count": N, "encoder": "lightonai/LateOn-Code" | null, "gather_limit": 500}`.
