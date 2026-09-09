"""Opt-in smoke test against the official LateOn-Code ONNX artifact.

Run with ``PRIORART_TEST_REAL_ENCODER=1 uv run --extra encoder pytest tests/test_real_encoder.py``.
The first run downloads about 150 MB.
"""

import os

import numpy as np
import pytest

pytest.importorskip("onnxruntime")
if os.environ.get("PRIORART_TEST_REAL_ENCODER") != "1":
    pytest.skip("set PRIORART_TEST_REAL_ENCODER=1 to run", allow_module_level=True)

from priorart.config import Settings  # noqa: E402
from priorart.encoder import OnnxEncoder  # noqa: E402
from priorart.service import Service  # noqa: E402

MODEL = "lightonai/LateOn-Code"


@pytest.fixture(scope="module")
def encoder():
    return OnnxEncoder(MODEL)


def test_representation_and_vectors(encoder):
    assert encoder.representation.dimension == 128
    assert encoder.representation.normalized
    assert encoder.representation.query_template == "[Q] "
    (vectors,) = encoder.encode_documents(["def f(): return 1"])
    assert vectors.dtype == np.float32
    assert np.allclose(np.linalg.norm(vectors, axis=1), 1.0, atol=1e-3)
    # Bare punctuation tokens are on the skiplist and dropped from documents only.
    (document,) = encoder.encode_documents(["foo(bar); x = y[0]"])
    (query,) = encoder.encode_queries(["foo(bar); x = y[0]"])
    assert len(query) - len(document) == 3
    (stripped,) = encoder.encode_documents(["  padded  "])
    (plain,) = encoder.encode_documents(["padded"])
    assert np.allclose(stripped, plain)


def test_end_to_end(tmp_path, encoder):
    service = Service(Settings(data_dir=tmp_path, encoder=MODEL), encoder)
    service.put(
        "torch.compile recompiles every step because a Python int changes; mark it dynamic.",
        None,
        "compile",
    )
    service.put(
        "NCCL watchdog timeout: rank 3 exited early from a stray sys.exit in the data loader.",
        None,
        "nccl",
    )
    outcome = service.search("distributed training hangs at the end of the first epoch")
    assert outcome.hits[0].id == "nccl"
    service.close()
