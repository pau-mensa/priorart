"""Settings, read once from ``PRIORART_*`` environment variables."""

from __future__ import annotations

import os
from collections.abc import Mapping
from dataclasses import dataclass, fields
from pathlib import Path


@dataclass(frozen=True)
class Settings:
    data_dir: Path = Path("data")
    encoder: str = "none"
    encoder_file: str = "model_int8.onnx"
    encoder_revision: str = "main"
    encoder_threads: int | None = None
    gather_limit: int = 500
    max_text_bytes: int = 262_144
    host: str = "127.0.0.1"
    port: int = 8000

    def __post_init__(self) -> None:
        if self.gather_limit <= 0:
            raise ValueError("gather_limit must be positive")
        if self.max_text_bytes <= 0:
            raise ValueError("max_text_bytes must be positive")
        if self.encoder_threads is not None and self.encoder_threads <= 0:
            raise ValueError("encoder_threads must be positive")
        if not 0 < self.port < 65536:
            raise ValueError("port must be in 1..65535")

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> Settings:
        env = os.environ if env is None else env
        values: dict[str, object] = {}
        for field in fields(cls):
            raw = env.get(f"PRIORART_{field.name.upper()}")
            if raw is None:
                continue
            if field.name == "data_dir":
                values[field.name] = Path(raw)
            elif field.name in ("gather_limit", "max_text_bytes", "port"):
                values[field.name] = int(raw)
            elif field.name == "encoder_threads":
                values[field.name] = int(raw) if raw else None
            else:
                values[field.name] = raw
        return cls(**values)  # type: ignore[arg-type]
