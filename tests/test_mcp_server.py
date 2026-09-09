import asyncio
import json

import httpx2 as httpx
import pytest
from conftest import FakeEncoder

pytest.importorskip("mcp")
from mcp.client import Client  # noqa: E402

from priorart.api import create_app  # noqa: E402
from priorart.config import Settings  # noqa: E402
from priorart.mcp_server import create_server  # noqa: E402
from priorart.service import Service  # noqa: E402


@pytest.fixture
def server(tmp_path):
    service = Service(Settings(data_dir=tmp_path), FakeEncoder())
    service.put("CUDA illegal address in bf16 attention; fixed by padding head dim.", None, "cuda")
    service.put("NCCL worker never reached the barrier: stray exit() in loader.", None, "nccl")
    app = create_app(service)
    yield create_server("http://testserver", transport=httpx.ASGITransport(app=app))
    service.close()


def call(server, name, arguments):
    async def go():
        async with Client(server) as client:
            return await client.call_tool(name, arguments)

    return asyncio.run(go())


def test_tools_and_annotations(server):
    async def go():
        async with Client(server) as client:
            return await client.list_tools()

    tools = {tool.name: tool for tool in asyncio.run(go()).tools}
    assert set(tools) == {
        "search_experiences",
        "get_experience",
        "contribute_experience",
        "report_outcome",
        "delete_experience",
    }
    assert tools["search_experiences"].annotations.read_only_hint is True
    assert tools["delete_experience"].annotations.destructive_hint is True
    assert "search_id" in tools["report_outcome"].description
    assert "problem" in tools["search_experiences"].input_schema["required"]
    assert "ctx" not in tools["search_experiences"].input_schema["properties"]


def test_search_then_report_loop(server):
    found = call(server, "search_experiences", {"problem": "worker never reached barrier"})
    assert not found.is_error
    body = found.structured_content
    assert body["hits"][0]["id"] == "nccl"
    assert body["search_id"] in body["next_step"]

    full = call(server, "get_experience", {"id": "nccl"}).structured_content
    assert full["text"].startswith("NCCL")

    reported = call(
        server,
        "report_outcome",
        {"record_id": "nccl", "outcome": "same cause, fixed", "search_id": body["search_id"]},
    )
    assert not reported.is_error
    assert reported.structured_content["id"]


def test_contribute_revises_and_delete(server):
    created = call(
        server,
        "contribute_experience",
        {"text": "torch.compile recompiles: mark_dynamic fixed it", "metadata": {"lang": "python"}},
    ).structured_content
    assert created["revision"] == 1
    revised = call(
        server, "contribute_experience", {"text": "corrected account", "id": created["id"]}
    ).structured_content
    assert revised == {"id": created["id"], "revision": 2}
    deleted = call(server, "delete_experience", {"id": created["id"]})
    assert deleted.structured_content == {"id": created["id"], "deleted": True}
    gone = call(server, "get_experience", {"id": created["id"]})
    assert gone.is_error
    assert "record_deleted" in gone.content[0].text


def test_server_errors_surface_to_the_model(server):
    missing = call(server, "get_experience", {"id": "nope"})
    assert missing.is_error
    assert "record_not_found" in missing.content[0].text
    empty = call(server, "contribute_experience", {"text": "   "})
    assert empty.is_error
    assert "invalid_input" in empty.content[0].text
    bad_report = call(
        server, "report_outcome", {"record_id": "cuda", "outcome": "x", "search_id": "zz"}
    )
    assert "search_not_found" in bad_report.content[0].text


def test_unreachable_server_fails_fast():
    def refuse(request):
        raise httpx.ConnectError("connection refused")

    server = create_server("http://nowhere", transport=httpx.MockTransport(refuse))

    async def go():
        async with Client(server) as client:
            await client.list_tools()

    with pytest.raises(Exception, match="cannot reach priorart|connection refused|closed"):
        asyncio.run(go())


def test_instructions_mention_loop(server):
    assert "report_outcome" in server.instructions
    assert json.dumps(server.instructions)  # plain text, serializable
