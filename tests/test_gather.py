import numpy as np
import pytest
from lateweave import CorpusManifest, Query, document_ids_digest

from priorart.gather import ExhaustiveGatherer, LexicalGatherer, choose_gatherer

TEXTS = [
    "CUDA illegal address after switching attention to bf16",
    "pytest fixture scope error when using async",
    "NCCL timeout: one worker never reached the barrier",
]


@pytest.fixture
def corpus():
    ids = [f"r{i}" for i in range(len(TEXTS))]
    return CorpusManifest("t", "v", len(TEXTS), document_ids_digest(ids))


def test_exhaustive_returns_all_in_order(corpus):
    out = ExhaustiveGatherer(corpus).gather(Query("anything"), 10)
    assert [c.document_id for c in out] == [0, 1, 2]
    assert [c.gather_rank for c in out] == [0, 1, 2]
    assert ExhaustiveGatherer(corpus).gather(Query("x"), 2)[-1].document_id == 1


def test_exhaustive_honours_subset(corpus):
    out = ExhaustiveGatherer(corpus).gather(Query("x"), 10, subset=np.array([0, 2]))
    assert [c.document_id for c in out] == [0, 2]


def test_lexical_ranks_match_first_and_drops_zero(corpus):
    gatherer = LexicalGatherer(corpus, TEXTS)
    out = gatherer.gather(Query("worker barrier timeout"), 10)
    assert out[0].document_id == 2
    assert all(c.gather_score > 0 for c in out)
    assert len(out) == 1
    assert gatherer.gather(Query(""), 10) == ()


def test_lexical_honours_subset(corpus):
    gatherer = LexicalGatherer(corpus, TEXTS)
    out = gatherer.gather(Query("cuda attention barrier"), 10, subset=np.array([1, 2]))
    assert [c.document_id for c in out] == [2]
    assert gatherer.gather(Query("cuda"), 10, subset=np.array([], dtype=np.int64)) == ()


def test_choose_gatherer_threshold(corpus):
    assert isinstance(choose_gatherer(corpus, TEXTS, 3, 3), ExhaustiveGatherer)
    assert isinstance(choose_gatherer(corpus, TEXTS, 4, 3), LexicalGatherer)
