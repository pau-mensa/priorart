import pytest
from conftest import FakeEncoder
from fastapi.testclient import TestClient

from priorart.api import create_app
from priorart.config import Settings
from priorart.service import Service


@pytest.fixture
def client(tmp_path):
    service = Service(Settings(data_dir=tmp_path), FakeEncoder())
    with TestClient(create_app(service)) as client:
        yield client
    service.close()


def test_record_lifecycle(client):
    created = client.post(
        "/v1/records", json={"text": "NCCL barrier timeout fixed", "metadata": {"k": "v"}}
    )
    assert created.status_code == 201
    record_id = created.json()["id"]
    assert created.json()["revision"] == 1

    got = client.get(f"/v1/records/{record_id}")
    assert got.status_code == 200
    assert got.json()["text"] == "NCCL barrier timeout fixed"
    assert got.json()["metadata"] == {"k": "v"}

    updated = client.post("/v1/records", json={"text": "second", "id": record_id})
    assert updated.json() == {"id": record_id, "revision": 2}
    assert client.get(f"/v1/records/{record_id}", params={"revision": 1}).json()["revision"] == 1
    assert client.get(f"/v1/records/{record_id}", params={"revision": 9}).status_code == 404

    assert client.delete(f"/v1/records/{record_id}").status_code == 204
    gone = client.get(f"/v1/records/{record_id}")
    assert gone.status_code == 410
    assert gone.json()["error"]["code"] == "record_deleted"
    assert client.delete(f"/v1/records/{record_id}").status_code == 204
    assert client.delete("/v1/records/missing").status_code == 404


def test_search_and_report(client):
    client.post("/v1/records", json={"text": "CUDA illegal address in bf16 attention", "id": "a"})
    client.post("/v1/records", json={"text": "NCCL worker never reached the barrier", "id": "b"})
    found = client.post("/v1/search", json={"text": "worker barrier", "limit": 2})
    assert found.status_code == 200
    body = found.json()
    assert body["hits"][0]["id"] == "b"
    assert body["gatherer"] == "exhaustive"
    assert set(body["hits"][0]) == {
        "id",
        "revision",
        "score",
        "score_semantics",
        "excerpt",
        "metadata",
    }

    reported = client.post(
        "/v1/reports",
        json={"record_id": "b", "text": "worked", "revision": 1, "search_id": body["search_id"]},
    )
    assert reported.status_code == 201
    listed = client.get("/v1/records/b/reports").json()["reports"]
    assert listed[0]["id"] == reported.json()["id"]
    assert listed[0]["search_id"] == body["search_id"]

    bad = client.post("/v1/reports", json={"record_id": "b", "text": "x", "search_id": "nope"})
    assert bad.status_code == 404
    assert bad.json()["error"]["code"] == "search_not_found"


def test_error_envelopes(client):
    empty = client.post("/v1/records", json={"text": "   "})
    assert empty.status_code == 400
    assert empty.json()["error"]["code"] == "invalid_input"
    malformed = client.post("/v1/records", json={"metadata": {}})
    assert malformed.status_code == 422
    assert malformed.json()["error"]["code"] == "validation_error"
    assert "text" in malformed.json()["error"]["message"]
    assert client.get("/v1/records/nope").json()["error"]["code"] == "record_not_found"
    assert client.get("/v1/records/nope/reports").status_code == 404
    assert client.post("/v1/search", json={"text": "x", "limit": 0}).status_code == 400


def test_healthz(client):
    health = client.get("/healthz").json()
    assert health["status"] == "ok"
    assert health["encoder"] == "fake"
    assert health["document_count"] == 0


def test_cli_version_and_help():
    from priorart.__main__ import main

    with pytest.raises(SystemExit) as exit_info:
        main(["--version"])
    assert exit_info.value.code == 0
