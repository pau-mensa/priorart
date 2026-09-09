"""Encoder boundary: raw text to per-token unit vectors.

The protocol is the only thing the rest of the server knows about a model.
``OnnxEncoder`` is the implementation: a pylate-onnx-export artifact (the
official ``lightonai/LateOn-Code`` repository ships one) run through ONNX
Runtime on CPU, with the pylate conventions reproduced outside the graph:
prefix token inserted after ``[CLS]``, truncation to the configured lengths,
padding masked out, and skiplist tokens dropped from documents only.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol, runtime_checkable

import numpy as np
from lateweave import Representation

if TYPE_CHECKING:
    from .config import Settings

INSTALL_HINT = "install with `pip install 'priorart[encoder]'`"
CONFIG_FILE = "onnx_config.json"
TOKENIZER_FILE = "tokenizer.json"
DEFAULT_ONNX_FILE = "model_int8.onnx"


class EncoderUnavailable(RuntimeError):
    """The configured encoder cannot be loaded in this environment."""


@runtime_checkable
class Encoder(Protocol):
    representation: Representation

    def encode_queries(self, texts: Sequence[str]) -> list[np.ndarray]: ...

    def encode_documents(self, texts: Sequence[str]) -> list[np.ndarray]: ...


def pack(arrays: Sequence[np.ndarray]) -> tuple[np.ndarray, np.ndarray]:
    """Concatenate ``[tokens, dimension]`` matrices into lateweave's packed form."""
    if not arrays:
        raise ValueError("nothing to pack")
    lengths = np.asarray([len(array) for array in arrays], dtype=np.int64)
    if np.any(lengths <= 0):
        raise ValueError("every document must have at least one token vector")
    return np.ascontiguousarray(np.concatenate(arrays), dtype=np.float32), lengths


def build_batch(
    rows: Sequence[tuple[Sequence[int], Sequence[int]]],
    *,
    prefix_id: int | None,
    pad_id: int,
    skiplist: frozenset[int],
) -> tuple[dict[str, np.ndarray], list[np.ndarray]]:
    """Turn tokenizer rows ``(ids, attention_mask)`` into model inputs and keep-masks.

    Mirrors pylate: the prefix id goes right after ``[CLS]``; the keep-mask is
    the attention mask AND ``id not in skiplist``. Pass an empty skiplist for
    queries.
    """
    ids: list[list[int]] = []
    attention: list[list[int]] = []
    for row_ids, row_mask in rows:
        row_ids, row_mask = list(row_ids), list(row_mask)
        if prefix_id is not None:
            row_ids.insert(1, int(prefix_id))
            row_mask.insert(1, 1)
        ids.append(row_ids)
        attention.append(row_mask)
    width = max(len(row) for row in ids)
    keep: list[np.ndarray] = []
    for row_ids, row_mask in zip(ids, attention, strict=True):
        padding = width - len(row_ids)
        row_ids.extend([pad_id] * padding)
        row_mask.extend([0] * padding)
        keep.append(
            np.asarray(
                [bool(m) and t not in skiplist for t, m in zip(row_ids, row_mask, strict=True)]
            )
        )
    inputs = {
        "input_ids": np.asarray(ids, dtype=np.int64),
        "attention_mask": np.asarray(attention, dtype=np.int64),
    }
    return inputs, keep


class OnnxEncoder:
    """A pylate-onnx-export ColBERT artifact on ONNX Runtime (CPU)."""

    def __init__(
        self,
        model_id: str,
        *,
        filename: str = DEFAULT_ONNX_FILE,
        revision: str = "main",
        threads: int | None = None,
        batch_size: int = 8,
    ) -> None:
        try:
            import onnxruntime as ort
            from tokenizers import Tokenizer
        except ImportError as error:
            raise EncoderUnavailable(
                f"encoder {model_id!r} needs onnxruntime; {INSTALL_HINT}"
            ) from error
        model_dir = _resolve(model_id, filename, revision)
        config = json.loads((model_dir / CONFIG_FILE).read_text(encoding="utf-8"))
        if config.get("model_type") != "ColBERT":
            raise EncoderUnavailable(f"{model_id!r} is not an exported ColBERT model")
        if config.get("do_query_expansion", False):
            raise EncoderUnavailable("query expansion is not supported by this encoder")
        if config.get("uses_token_type_ids", False):
            raise EncoderUnavailable("token_type_ids inputs are not supported by this encoder")

        self.model_id = model_id
        self.batch_size = batch_size
        self.query_length = int(config["query_length"])
        self.document_length = int(config["document_length"])
        self.query_prefix_id = _optional_int(config.get("query_prefix_id"))
        self.document_prefix_id = _optional_int(config.get("document_prefix_id"))
        self.pad_id = int(config.get("pad_token_id") or config.get("mask_token_id") or 0)
        self.tokenizer = Tokenizer.from_file(str(model_dir / TOKENIZER_FILE))
        # pylate: convert_tokens_to_ids, which maps unknown words to [UNK].
        unknown = self.tokenizer.token_to_id("[UNK]")
        skiplist: set[int] = set()
        for word in config.get("skiplist_words", []):
            token = self.tokenizer.token_to_id(word)
            if token is None:
                token = unknown
            if token is not None:
                skiplist.add(int(token))
        self.skiplist = frozenset(skiplist)
        options = ort.SessionOptions()
        if threads is not None:
            options.intra_op_num_threads = threads
        options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.session = ort.InferenceSession(
            str(model_dir / filename), sess_options=options, providers=["CPUExecutionProvider"]
        )
        self.output_name = self.session.get_outputs()[0].name
        self.representation = Representation(
            encoder=model_id,
            encoder_revision=revision,
            dimension=int(config["embedding_dim"]),
            normalized=True,
            query_template=str(config.get("query_prefix", "")),
            document_template=str(config.get("document_prefix", "")),
        )

    def _encode_batch(self, texts: Sequence[str], *, is_query: bool) -> list[np.ndarray]:
        prefix_id = self.query_prefix_id if is_query else self.document_prefix_id
        length = self.query_length if is_query else self.document_length
        self.tokenizer.enable_truncation(max_length=length - (1 if prefix_id is not None else 0))
        # pylate strips the text before tokenizing; mirror it so token counts match.
        encoded = self.tokenizer.encode_batch([t.strip() for t in texts], add_special_tokens=True)
        inputs, keep = build_batch(
            [(item.ids, item.attention_mask) for item in encoded],
            prefix_id=prefix_id,
            pad_id=self.pad_id,
            skiplist=frozenset() if is_query else self.skiplist,
        )
        output = self.session.run([self.output_name], inputs)[0]
        vectors: list[np.ndarray] = []
        for row, mask in zip(output, keep, strict=True):
            kept = np.asarray(row[mask], dtype=np.float32)
            if len(kept) == 0:
                # Everything but structure tokens was skipped; keep them so the
                # document still has at least one vector.
                kept = np.asarray(row[np.asarray(inputs["attention_mask"][len(vectors)], bool)])
            norms = np.linalg.norm(kept, axis=1, keepdims=True)
            vectors.append(np.ascontiguousarray(kept / np.maximum(norms, 1e-12), dtype=np.float32))
        return vectors

    def _encode(self, texts: Sequence[str], *, is_query: bool) -> list[np.ndarray]:
        output: list[np.ndarray] = []
        for start in range(0, len(texts), self.batch_size):
            output.extend(
                self._encode_batch(texts[start : start + self.batch_size], is_query=is_query)
            )
        return output

    def encode_queries(self, texts: Sequence[str]) -> list[np.ndarray]:
        return self._encode(texts, is_query=True)

    def encode_documents(self, texts: Sequence[str]) -> list[np.ndarray]:
        return self._encode(texts, is_query=False)


def _optional_int(value: Any) -> int | None:
    return None if value is None else int(value)


def _resolve(model_id: str, filename: str, revision: str) -> Path:
    local = Path(model_id).expanduser()
    if local.is_dir():
        return local
    try:
        from huggingface_hub import snapshot_download
    except ImportError as error:
        raise EncoderUnavailable(
            f"downloading {model_id!r} needs huggingface-hub; {INSTALL_HINT}"
        ) from error
    return Path(
        snapshot_download(
            repo_id=model_id,
            revision=revision,
            allow_patterns=[filename, TOKENIZER_FILE, CONFIG_FILE],
        )
    )


def load_encoder(settings: Settings) -> Encoder | None:
    if settings.encoder.lower() == "none":
        return None
    return OnnxEncoder(
        settings.encoder,
        filename=settings.encoder_file,
        revision=settings.encoder_revision,
        threads=settings.encoder_threads,
    )
