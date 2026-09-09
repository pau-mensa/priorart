import sys

import numpy as np
import pytest
from conftest import FakeEncoder

from priorart.config import Settings
from priorart.encoder import (
    Encoder,
    EncoderUnavailable,
    OnnxEncoder,
    build_batch,
    load_encoder,
    pack,
)


def test_pack_concatenates_and_reports_lengths():
    a = np.ones((2, 4), dtype=np.float32)
    b = np.zeros((3, 4), dtype=np.float32)
    packed, lengths = pack([a, b])
    assert packed.shape == (5, 4)
    assert packed.dtype == np.float32
    assert lengths.tolist() == [2, 3]
    with pytest.raises(ValueError):
        pack([])


def test_fake_encoder_is_unit_norm_and_deterministic():
    encoder = FakeEncoder()
    assert isinstance(encoder, Encoder)
    (vectors,) = encoder.encode_documents(["hang hang cuda"])
    assert vectors.shape == (3, 16)
    assert np.allclose(np.linalg.norm(vectors, axis=1), 1.0)
    assert np.array_equal(vectors[0], vectors[1])
    (empty,) = encoder.encode_queries([""])
    assert empty.shape == (1, 16)


def test_build_batch_inserts_prefix_pads_and_masks_skiplist():
    rows = [([1, 10, 11, 2], [1, 1, 1, 1]), ([1, 99, 2], [1, 1, 1])]
    inputs, keep = build_batch(rows, prefix_id=7, pad_id=0, skiplist=frozenset({99}))
    assert inputs["input_ids"].tolist() == [[1, 7, 10, 11, 2], [1, 7, 99, 2, 0]]
    assert inputs["attention_mask"].tolist() == [[1, 1, 1, 1, 1], [1, 1, 1, 1, 0]]
    assert inputs["input_ids"].dtype == np.int64
    assert keep[0].tolist() == [True, True, True, True, True]
    assert keep[1].tolist() == [True, True, False, True, False]


def test_build_batch_without_prefix_and_queries_ignore_skiplist():
    inputs, keep = build_batch(
        [([1, 99, 2], [1, 1, 1])], prefix_id=None, pad_id=0, skiplist=frozenset()
    )
    assert inputs["input_ids"].tolist() == [[1, 99, 2]]
    assert keep[0].all()


def test_load_encoder_none():
    assert load_encoder(Settings(encoder="none")) is None
    assert load_encoder(Settings(encoder="NONE")) is None


def test_settings_encoder_fields_from_env():
    settings = Settings.from_env(
        {
            "PRIORART_ENCODER": "lightonai/LateOn-Code",
            "PRIORART_ENCODER_FILE": "model.onnx",
            "PRIORART_ENCODER_THREADS": "4",
        }
    )
    assert (settings.encoder, settings.encoder_file, settings.encoder_threads) == (
        "lightonai/LateOn-Code",
        "model.onnx",
        4,
    )
    assert Settings.from_env({"PRIORART_ENCODER_THREADS": ""}).encoder_threads is None
    with pytest.raises(ValueError):
        Settings(encoder_threads=0)


def test_onnxruntime_missing_gives_install_hint(monkeypatch):
    monkeypatch.setitem(sys.modules, "onnxruntime", None)
    with pytest.raises(EncoderUnavailable, match="priorart\\[encoder\\]"):
        OnnxEncoder("lightonai/LateOn-Code")
