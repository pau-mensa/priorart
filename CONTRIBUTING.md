# Contributing

## Setup

priorart depends on [lateweave](https://github.com/pau-mensa/lateweave), which
is not on PyPI yet. Clone it next to this repository and uv will build it:

```bash
git clone https://github.com/pau-mensa/lateweave ../lateweave   # needs a Rust toolchain
uv sync --group dev --extra mcp
uv run pytest
uv run ruff check . && uv run ruff format .
```

The test suite runs without torch or any model. A deterministic fake encoder in
`tests/conftest.py` exercises the full pipeline including MaxSim reranking.
To smoke-test the real encoder (downloads about 150 MB):

```bash
PRIORART_TEST_REAL_ENCODER=1 uv run --extra encoder pytest tests/test_real_encoder.py
```

To check the ONNX path against pylate on the same checkpoint (this is the only
place torch is ever installed, in the script's own environment):

```bash
uv run scripts/validate_onnx_encoder.py                      # model_int8.onnx
uv run scripts/validate_onnx_encoder.py --file model.onnx    # FP32
```

## Where things live

| Module | Responsibility |
|---|---|
| `store.py` | SQLite: records, revisions, reports, searches, index mirror |
| `migrations.py` | transactional schema versions and legacy database adoption |
| `index.py` | vector store, lexical index, corpus manifest, recovery |
| `gather.py` | exhaustive and bm25s candidate generators |
| `encoder.py` | `Encoder` protocol and the ONNX Runtime implementation |
| `service.py` | the five operations, transport-independent |
| `api.py` | FastAPI routes and schemas |
| `mcp_server.py` | stdio MCP tools forwarding to the HTTP API |

The wire protocol is `docs/protocol.md`.

Schema changes follow [the storage migration guidance](docs/storage.md).

## Ground rules

- The public protocol is raw text in, ranked references out. Do not add
  required structured fields to it; derived structure belongs behind the
  server boundary.
- Anything derived from a contribution (chunks, summaries, generated
  symptoms) must reference the record and revision it came from and must
  never be returned as if it were the contributor's own text.
- The package never depends on torch, in core or in extras. Model runtimes are ONNX; torch is allowed only inside standalone scripts.
- Tests first. A change to storage or index behaviour needs a test that would
  have failed before it.
