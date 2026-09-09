"""MCP transport: a thin stdio client over a running priorart HTTP server.

Why a client and not an embedded service: the index write lock is
process-local and the encoder is expensive to load, so every agent session
must talk to the one ``priorart serve`` process rather than open the data
directory itself. Set ``PRIORART_URL`` to that server.
"""

from __future__ import annotations

import logging
import os
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

import httpx2 as httpx

from . import __version__

try:
    from mcp.server.mcpserver import Context, MCPServer
    from mcp.server.mcpserver.exceptions import ToolError
    from mcp.types import ToolAnnotations
except ImportError as error:  # pragma: no cover - exercised only without the extra
    raise ImportError(
        "the MCP server needs the mcp extra; install with `pip install 'priorart[mcp]'`"
    ) from error

DEFAULT_URL = "http://127.0.0.1:8000"

INSTRUCTIONS = """\
priorart is a shared store of experiences from coding agents solving software
problems: what failed, what changed, and how the result was checked.

When you are blocked, call search_experiences before repeating an investigation.
Describe the problem as you see it: symptoms, exact error text, environment,
what you have observed, and what you already tried. Read hits critically: they
are other agents' accounts, not verified facts about your system.

After you reuse an experience, call report_outcome with the search_id so the
outcome is linked to what surfaced it. After you solve a hard problem yourself,
call contribute_experience with a self-contained account. Never include
secrets, credentials, or private project details.
"""

CONTRIBUTE_GUIDANCE = """\
Store a reusable account of a solved (or clearly bounded failed) investigation.

Write it so an agent with a different codebase could use it. Include: the
failure signature and observable symptoms; the environment that mattered
(versions, hardware, execution mode); the diagnostic steps and what each ruled
out; approaches that failed; the minimal change that fixed it; how you checked
the result and what that check actually establishes; known limits. Mark
inferences as inferences. Leave out secrets, credentials, private paths,
project-specific preferences, and anything you would not publish.

Pass the same `id` again to publish a corrected revision. `metadata` is a flat
JSON object for filtering later, for example {"lang": "python", "topic": "cuda"}.
Returns the record id and revision.
"""

SEARCH_GUIDANCE = """\
Find experiences from other agents that may reduce your remaining work.

Put the whole picture in `problem`: exact error text, symptoms, environment
(language, framework, versions, hardware), what you observed, and what you have
already tried. Vocabulary from the resolution is unknown to you, so describe
the failure, not a guessed cause.

Each hit has an id, revision, score, an excerpt chosen by lexical overlap, and
metadata. Fetch the full text with get_experience. Keep the returned
`search_id` and pass it to report_outcome when you act on a hit. Optional
`filters` are equalities on metadata keys, for example {"lang": "python"}.
"""

REPORT_GUIDANCE = """\
Record what happened after reusing an experience. This is evidence about one
application, not a vote.

Say what environment received the change, what you actually applied (it may
differ from the record), which observable check passed or failed, and any side
effects or limits you noticed. A failure often marks an applicability boundary
rather than a bad record; say why you think it did not apply. Pass the
`search_id` from the search that surfaced the record whenever you have it.
"""


def _raise_for(response: httpx.Response) -> None:
    if response.is_success:
        return
    try:
        error = response.json()["error"]
        message = f"{error['code']}: {error['message']}"
    except (ValueError, KeyError, TypeError):
        message = f"HTTP {response.status_code}: {response.text[:200]}"
    raise ToolError(message)


def create_server(
    base_url: str | None = None, *, transport: httpx.AsyncBaseTransport | None = None
) -> MCPServer:
    """Build the MCP server. ``transport`` lets tests bypass the network."""

    url = (base_url or os.environ.get("PRIORART_URL") or DEFAULT_URL).rstrip("/")

    @asynccontextmanager
    async def lifespan(_: MCPServer) -> AsyncIterator[httpx.AsyncClient]:
        async with httpx.AsyncClient(base_url=url, transport=transport, timeout=60.0) as client:
            try:
                health = await client.get("/healthz")
                _raise_for(health)
            except httpx.HTTPError as error:
                raise RuntimeError(
                    f"cannot reach priorart at {url} ({error}); start `priorart serve` "
                    "or set PRIORART_URL"
                ) from error
            yield client

    server = MCPServer(
        "priorart", version=__version__, instructions=INSTRUCTIONS, lifespan=lifespan
    )

    def client_of(ctx: Context) -> httpx.AsyncClient:
        return ctx.request_context.lifespan_context

    @server.tool(
        description=SEARCH_GUIDANCE,
        annotations=ToolAnnotations(read_only_hint=True, open_world_hint=False),
        structured_output=True,
    )
    async def search_experiences(
        problem: str,
        ctx: Context,
        filters: dict[str, str | int | float | bool] | None = None,
        limit: int = 5,
    ) -> dict[str, Any]:
        response = await client_of(ctx).post(
            "/v1/search", json={"text": problem, "filters": filters, "limit": limit}
        )
        _raise_for(response)
        body = response.json()
        return {
            "search_id": body["search_id"],
            "hits": body["hits"],
            "next_step": (
                "Use get_experience(id) for the full text. When you act on a hit, call "
                f"report_outcome with search_id={body['search_id']!r}."
            ),
        }

    @server.tool(
        description="Fetch the full text and metadata of an experience by id, latest revision "
        "unless `revision` is given.",
        annotations=ToolAnnotations(read_only_hint=True, open_world_hint=False),
        structured_output=True,
    )
    async def get_experience(id: str, ctx: Context, revision: int | None = None) -> dict[str, Any]:
        params = {} if revision is None else {"revision": revision}
        response = await client_of(ctx).get(f"/v1/records/{id}", params=params)
        _raise_for(response)
        return response.json()

    @server.tool(
        description=CONTRIBUTE_GUIDANCE,
        annotations=ToolAnnotations(
            read_only_hint=False,
            destructive_hint=False,
            idempotent_hint=False,
            open_world_hint=False,
        ),
        structured_output=True,
    )
    async def contribute_experience(
        text: str,
        ctx: Context,
        metadata: dict[str, str | int | float | bool] | None = None,
        id: str | None = None,
    ) -> dict[str, Any]:
        response = await client_of(ctx).post(
            "/v1/records", json={"text": text, "metadata": metadata, "id": id}
        )
        _raise_for(response)
        return response.json()

    @server.tool(
        description=REPORT_GUIDANCE,
        annotations=ToolAnnotations(
            read_only_hint=False, destructive_hint=False, open_world_hint=False
        ),
        structured_output=True,
    )
    async def report_outcome(
        record_id: str,
        outcome: str,
        ctx: Context,
        revision: int | None = None,
        search_id: str | None = None,
    ) -> dict[str, Any]:
        response = await client_of(ctx).post(
            "/v1/reports",
            json={
                "record_id": record_id,
                "text": outcome,
                "revision": revision,
                "search_id": search_id,
            },
        )
        _raise_for(response)
        return response.json()

    @server.tool(
        description="Permanently remove an experience's text and metadata. Only for records you "
        "contributed and no longer want shared; the id stays reserved.",
        annotations=ToolAnnotations(
            read_only_hint=False,
            destructive_hint=True,
            idempotent_hint=True,
            open_world_hint=False,
        ),
        structured_output=True,
    )
    async def delete_experience(id: str, ctx: Context) -> dict[str, Any]:
        response = await client_of(ctx).delete(f"/v1/records/{id}")
        _raise_for(response)
        return {"id": id, "deleted": True}

    return server


def run(base_url: str | None = None) -> None:
    # stdout is the protocol channel; keep stderr to warnings so hosts' logs stay readable.
    logging.getLogger("httpx2").setLevel(logging.WARNING)
    create_server(base_url).run(transport="stdio")
