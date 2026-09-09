import pytest
from conftest import FakeEncoder

from priorart.config import Settings
from priorart.service import InvalidInput, Service
from priorart.store import RecordDeleted, RecordNotFound, SearchNotFound

DOCS = {
    "cuda": "CUDA illegal address after switching attention to bf16. Fixed by padding heads.",
    "pytest": "pytest fixture scope error when using async fixtures with session scope.",
    "nccl": "NCCL timeout: one worker never reached the barrier because of a stray exit().",
}


@pytest.fixture
def service(tmp_path):
    service = Service(Settings(data_dir=tmp_path), FakeEncoder())
    for record_id, text in DOCS.items():
        service.put(text, {"kind": "fix", "topic": record_id}, record_id)
    yield service
    service.close()


def test_put_and_search(service):
    outcome = service.search("worker never reached barrier")
    assert outcome.search_id
    assert outcome.hits[0].id == "nccl"
    assert outcome.hits[0].revision == 1
    assert outcome.hits[0].metadata == {"kind": "fix", "topic": "nccl"}
    assert outcome.hits[0].score_semantics == "int8-reconstructed-approximate-full-maxsim"
    assert outcome.gatherer == "exhaustive"
    assert "barrier" in outcome.hits[0].excerpt
    assert set(outcome.timings) >= {"gather_seconds", "rerank_seconds", "total_seconds"}


def test_update_supersedes(service):
    _, revision = service.put("completely different: rust borrow checker lifetime", None, "nccl")
    assert revision == 2
    outcome = service.search("borrow checker lifetime")
    assert (outcome.hits[0].id, outcome.hits[0].revision) == ("nccl", 2)
    assert service.get("nccl").text.startswith("completely")
    assert service.get("nccl", 1).text.startswith("NCCL")


def test_delete_hides(service):
    service.delete("nccl")
    outcome = service.search("worker never reached barrier")
    assert all(hit.id != "nccl" for hit in outcome.hits)
    with pytest.raises(RecordDeleted):
        service.get("nccl")
    service.delete("nccl")  # idempotent
    with pytest.raises(RecordNotFound):
        service.delete("missing")


def test_filters_restrict(service):
    outcome = service.search("error fixed", filters={"topic": "pytest"})
    assert [hit.id for hit in outcome.hits] == ["pytest"]
    assert service.search("error", filters={"topic": "nothing"}).hits == []


def test_limit_and_threshold(tmp_path):
    service = Service(Settings(data_dir=tmp_path, gather_limit=2), FakeEncoder())
    for record_id, text in DOCS.items():
        service.put(text, None, record_id)
    outcome = service.search("worker never reached barrier", limit=1)
    assert outcome.gatherer == "bm25s"
    assert [hit.id for hit in outcome.hits] == ["nccl"]
    outcome = service.search("worker never reached barrier", filters=None, limit=3)
    assert len(outcome.hits) >= 1
    service.close()


def test_lexical_only_mode(tmp_path):
    service = Service(Settings(data_dir=tmp_path), None)
    service.put(DOCS["cuda"], None, "cuda")
    service.put(DOCS["nccl"], None, "nccl")
    outcome = service.search("barrier")
    assert outcome.hits[0].id == "nccl"
    assert outcome.hits[0].score_semantics == "bm25s-lucene"
    assert service.health()["encoder"] is None
    service.close()


def test_empty_corpus_search(tmp_path):
    service = Service(Settings(data_dir=tmp_path), FakeEncoder())
    outcome = service.search("anything")
    assert outcome.hits == []
    assert outcome.search_id
    service.close()


def test_reports(service):
    outcome = service.search("cuda illegal address")
    report_id = service.report("cuda", "applied the padding fix, tests pass", 1, outcome.search_id)
    reports = service.reports("cuda")
    assert [r.id for r in reports] == [report_id]
    assert reports[0].search_id == outcome.search_id
    with pytest.raises(SearchNotFound):
        service.report("cuda", "x", None, "bogus")
    with pytest.raises(RecordNotFound):
        service.report("missing", "x")
    with pytest.raises(InvalidInput):
        service.report("cuda", "   ")


def test_validation(service):
    with pytest.raises(InvalidInput):
        service.put("   ", None, None)
    with pytest.raises(InvalidInput):
        service.put("x" * 300_000, None, None)
    with pytest.raises(InvalidInput):
        service.put("ok", None, "bad id with spaces")
    with pytest.raises(InvalidInput):
        service.put("ok", ["not", "a", "dict"], None)  # type: ignore[arg-type]
    with pytest.raises(InvalidInput):
        service.search("   ")
    with pytest.raises(InvalidInput):
        service.search("ok", limit=0)
    with pytest.raises(InvalidInput):
        service.search("ok", limit=101)
    with pytest.raises(InvalidInput):
        service.search("ok", filters={"nested": {"a": 1}})


def test_health(service):
    health = service.health()
    assert health["status"] == "ok"
    assert health["document_count"] == 3
    assert health["encoder"] == "fake"


def test_reopen_keeps_data(tmp_path):
    encoder = FakeEncoder()
    service = Service(Settings(data_dir=tmp_path), encoder)
    service.put(DOCS["nccl"], None, "nccl")
    service.close()
    reopened = Service(Settings(data_dir=tmp_path), encoder)
    assert reopened.search("barrier").hits[0].id == "nccl"
    reopened.close()
