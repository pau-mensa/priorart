import json

import numpy as np
import pytest
from conftest import FakeEncoder

from priorart.index import Index
from priorart.store import LOCAL_COLLECTION_ID, LOCAL_PRINCIPAL_ID, Store


@pytest.fixture
def store(tmp_path):
    store = Store(tmp_path / "priorart.sqlite")
    yield store
    store.close()


def put(store, index, record_id, text):
    ref = store.put(
        text,
        None,
        record_id,
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )
    index.upsert(record_id, ref.revision, text)
    return ref.revision


def search(index, text, gather_limit=500, limit=5):
    pipeline = index.pipeline(index.document_count, gather_limit)
    return pipeline.search(index.query(text), gather_limit=gather_limit, limit=limit)


@pytest.mark.parametrize("encoder", [FakeEncoder(), None], ids=["fake", "lexical"])
def test_upsert_update_remove(tmp_path, store, encoder):
    index = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    assert index.document_count == 0
    assert index.manifest is None
    assert not (tmp_path / "vectors").exists()

    put(store, index, "a", "cuda illegal address")
    assert index.document_count == 1
    assert (tmp_path / "vectors").exists() == (encoder is not None)
    generation = index.manifest.generation

    put(store, index, "a", "cuda illegal address, bf16 attention")
    assert index.document_count == 1
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == [(0, "a", 2)]
    assert index.manifest.generation > generation

    index.remove("a")
    assert index.document_count == 0
    assert index.manifest is None
    assert not (tmp_path / "vectors").exists()
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == []


def test_remove_middle_renumbers(tmp_path, store):
    encoder = FakeEncoder()
    index = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "alpha alpha")
    put(store, index, "b", "beta beta beta")
    put(store, index, "c", "gamma")
    index.remove("b")
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == [(0, "a", 1), (1, "c", 1)]
    assert index.vectors.document_count == 2
    assert index.vectors.document_lengths([0, 1]) == {0: 2, 1: 1}
    assert index.internal_ids({"c", "zzz"}).tolist() == [1]
    assert index.internal_ids({"a", "c"}).dtype == np.int64


def test_search_finds_right_document(tmp_path, store):
    index = Index(tmp_path, store, FakeEncoder(), collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "CUDA illegal address after switching attention to bf16")
    put(store, index, "b", "pytest fixture scope error when using async")
    put(store, index, "c", "NCCL timeout: one worker never reached the barrier")
    result = search(index, "worker never reached barrier")
    assert index.record_ids[result.documents[0].document_id] == "c"
    assert result.diagnostics["score_semantics"] == "int8-reconstructed-approximate-full-maxsim"
    assert result.diagnostics["gatherer"] == "ExhaustiveGatherer"
    result = search(index, "worker never reached barrier", gather_limit=2, limit=2)
    assert result.diagnostics["gatherer"] == "LexicalGatherer"
    assert index.record_ids[result.documents[0].document_id] == "c"


def test_lexical_only_search(tmp_path, store):
    index = Index(tmp_path, store, None, collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "cuda illegal address")
    put(store, index, "b", "nccl barrier timeout")
    result = search(index, "barrier")
    assert index.record_ids[result.documents[0].document_id] == "b"
    assert result.diagnostics["score_semantics"] == "bm25s-lucene"


def test_reopen_consistent_does_not_reencode(tmp_path, store):
    encoder = FakeEncoder()
    index = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "alpha")
    calls = encoder.calls
    reopened = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    assert reopened.document_count == 1
    assert encoder.calls == calls


def test_recovery_from_mirror_mismatch(tmp_path, store):
    encoder = FakeEncoder()
    index = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "alpha")
    put(store, index, "b", "beta")
    # Simulate a crash between the store write and the mirror write.
    store.replace_index_documents([("a", 1)], "fake", collection_id=LOCAL_COLLECTION_ID)
    reopened = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    assert reopened.document_count == 2
    assert sorted(reopened.record_ids) == ["a", "b"]
    assert reopened.vectors.document_count == 2
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == [(0, "a", 1), (1, "b", 1)]


def test_recovery_when_store_missing_or_stale_revision(tmp_path, store):
    encoder = FakeEncoder()
    index = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "alpha")
    store.put(
        "alpha two",
        None,
        "a",
        collection_id=LOCAL_COLLECTION_ID,
        author_principal_id=LOCAL_PRINCIPAL_ID,
    )  # revision 2 never indexed
    reopened = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    assert store.index_documents(collection_id=LOCAL_COLLECTION_ID) == [(0, "a", 2)]
    import shutil

    shutil.rmtree(tmp_path / "vectors")
    reopened = Index(tmp_path, store, encoder, collection_id=LOCAL_COLLECTION_ID)
    assert reopened.vectors.document_count == 1


def test_representation_change_rebuilds(tmp_path, store):
    index = Index(tmp_path, store, FakeEncoder(dimension=16), collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "alpha")
    reopened = Index(
        tmp_path, store, FakeEncoder(dimension=8, name="other"), collection_id=LOCAL_COLLECTION_ID
    )
    assert reopened.vectors.representation.dimension == 8
    metadata = json.loads((tmp_path / "vectors" / "storage.json").read_text())
    assert metadata["representation"]["encoder"] == "other"


def test_switching_encoder_on_builds_store(tmp_path, store):
    index = Index(tmp_path, store, None, collection_id=LOCAL_COLLECTION_ID)
    put(store, index, "a", "alpha")
    assert not (tmp_path / "vectors").exists()
    reopened = Index(tmp_path, store, FakeEncoder(), collection_id=LOCAL_COLLECTION_ID)
    assert reopened.vectors.document_count == 1
    lexical = Index(tmp_path, store, None, collection_id=LOCAL_COLLECTION_ID)
    assert lexical.vectors is None
    assert lexical.document_count == 1
