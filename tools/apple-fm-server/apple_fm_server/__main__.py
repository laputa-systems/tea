"""Run the Apple Foundation Models OpenAI-compatible server.

    uv run python -m apple_fm_server

The default port matches tea's local provider default base URL
(``http://127.0.0.1:8000/v1``), so the server needs no client-side flag.
"""

from __future__ import annotations

import argparse
import sys

from .backend import ProtocolError, create_backend
from .server import ServerConfig, create_server

# tea's `local` provider defaults to this port, so the two agree without flags.
DEFAULT_PORT = 8000
DEFAULT_MODEL_ID = "apple-foundation-models"
# tea's `--local-context-window` is its compaction capacity, not a claim about
# this model. Passing the real 4096 makes tea reserve a quarter of it and compact
# above 3072 tokens — below the 3258-token cost of tea's own harness prompt — so
# it compacts on the first turn and fails. The hint therefore suggests a
# capacity that keeps compaction out of the way; this server enforces the real
# window regardless.
SUGGESTED_COMPACTION_WINDOW = 16_384


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="apple-fm-server",
        description=(
            "Serve Apple's on-device Foundation Model over an OpenAI-compatible API "
            "(GET /v1/models, POST /v1/chat/completions with SSE streaming)."
        ),
    )
    parser.add_argument("--host", default="127.0.0.1", help="interface to bind (default: %(default)s)")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT, help="port to bind (default: %(default)s)")
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL_ID,
        help="model id advertised to clients (default: %(default)s)",
    )
    parser.add_argument("--verbose", action="store_true", help="log every HTTP request")
    parser.add_argument(
        "--dump-requests",
        metavar="DIR",
        help="write every request body to DIR/request-NNNN.json for inspection",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        backend = create_backend()
    except ProtocolError as error:
        print(f"apple-fm-server: {error}", file=sys.stderr)
        return 2
    config = ServerConfig(
        host=args.host,
        port=args.port,
        model_id=args.model,
        context_size=backend.context_size,
    )
    server = create_server(backend, config, verbose=args.verbose, dump_dir=args.dump_requests)
    print(
        f"apple-fm-server: '{config.model_id}' listening on http://{args.host}:{server.bound_port}/v1 "
        f"(model context window {config.context_size} tokens)",
        file=sys.stderr,
        flush=True,
    )
    print(
        "apple-fm-server: point tea at it with --provider local "
        f"--model {config.model_id} --local-base-url http://{args.host}:{server.bound_port}/v1 "
        f"--local-context-window {SUGGESTED_COMPACTION_WINDOW}",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\napple-fm-server: interrupted", file=sys.stderr)
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
