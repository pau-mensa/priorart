"""``priorart serve``: build the service from the environment and run uvicorn."""

from __future__ import annotations

import argparse
import sys

from . import __version__
from .config import Settings
from .encoder import EncoderUnavailable
from .migrations import SchemaError


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="priorart", description="priorart text store")
    parser.add_argument("--version", action="version", version=f"priorart {__version__}")
    commands = parser.add_subparsers(dest="command", required=True)
    serve = commands.add_parser("serve", help="run the HTTP server (settings from PRIORART_*)")
    serve.add_argument("--host", help="override PRIORART_HOST")
    serve.add_argument("--port", type=int, help="override PRIORART_PORT")
    mcp = commands.add_parser(
        "mcp", help="run the stdio MCP server against a running priorart (PRIORART_URL)"
    )
    mcp.add_argument("--url", help="override PRIORART_URL (default http://127.0.0.1:8000)")
    args = parser.parse_args(argv)

    if args.command == "mcp":
        try:
            from .mcp_server import run
        except ImportError as error:
            print(f"error: {error}", file=sys.stderr)
            return 2
        run(args.url)
        return 0

    settings = Settings.from_env()
    if args.host is not None or args.port is not None:
        settings = Settings(
            **{
                **settings.__dict__,
                "host": args.host or settings.host,
                "port": args.port or settings.port,
            }
        )
    try:
        import uvicorn

        from .api import create_app
        from .service import Service

        service = Service.open(settings)
    except (EncoderUnavailable, SchemaError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(
        f"priorart {__version__}: data at {settings.data_dir}, "
        f"encoder {settings.encoder}, {service.index.document_count} documents",
        file=sys.stderr,
    )
    uvicorn.run(create_app(service), host=settings.host, port=settings.port, log_level="info")
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
