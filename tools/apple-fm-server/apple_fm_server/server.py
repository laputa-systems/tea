"""HTTP transport for the OpenAI-compatible surface.

The Apple SDK is asynchronous and expects to be driven from one event loop, so
this module runs that loop on a dedicated thread and bridges events to the
blocking ``http.server`` handler through a queue. A request handler writes each
event as one Server-Sent Events record, which is the wire mode tea's local
provider consumes.

Cancellation is client-driven: when a write fails because the client is gone,
the handler stops the pump and cancels the generation task, which closes the
Apple stream instead of letting it run to completion for nobody.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import itertools
import json
import queue
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Mapping
from urllib.parse import urlsplit

from . import protocol
from .backend import Backend, Finish, TextDelta, ToolCalls, Usage
from .protocol import ChatRequest, ProtocolError

# How long a handler waits between queue reads. Only a liveness bound: the Apple
# model can take tens of seconds to produce its first token, and the wait must
# not be mistaken for a dead request.
_QUEUE_POLL_SECONDS = 0.25


@dataclass(frozen=True, slots=True)
class ServerConfig:
    """Caller-supplied server identity and bind address."""

    host: str
    port: int
    model_id: str
    context_size: int


class _ModelLoop:
    """The event loop that owns every Apple session."""

    def __init__(self) -> None:
        self.loop = asyncio.new_event_loop()
        self._thread = threading.Thread(target=self._run, name="apple-fm-loop", daemon=True)
        self._thread.start()

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.run_forever()

    def close(self) -> None:
        self.loop.call_soon_threadsafe(self.loop.stop)
        self._thread.join(timeout=5)


def _drain(
    backend: Backend,
    request: ChatRequest,
    sink: "queue.Queue[tuple[str, Any]]",
    cancelled: threading.Event,
    loop: asyncio.AbstractEventLoop,
) -> concurrent.futures.Future:
    """Run one backend request on *loop*, publishing each event to *sink*.

    The HTTP handler reads *sink* from its own thread; the returned future
    cancels the generation when the client goes away.
    """

    async def run() -> None:
        try:
            async for event in backend.chat(request):
                if cancelled.is_set():
                    break
                sink.put(("event", event))
        except ProtocolError as error:
            sink.put(("error", error))
        except Exception as error:  # pragma: no cover - defensive boundary
            sink.put(
                (
                    "error",
                    ProtocolError(f"unexpected server failure: {error}", status=500, kind="server_error"),
                )
            )
        finally:
            sink.put(("end", None))

    return asyncio.run_coroutine_threadsafe(run(), loop)


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "apple-fm-server"

    backend: Backend
    config: ServerConfig
    model_loop: _ModelLoop
    verbose: bool
    dump_dir: str | None

    # Shared by every handler instance: one lock, and one counter that must be
    # advanced on the class itself (`self._dump_count += 1` would create an
    # instance attribute and restart the sequence at 1 for every request).
    _dump_lock = threading.Lock()
    _dump_sequence = itertools.count(1)

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002 - stdlib signature
        if self.verbose:
            print(f"apple-fm-server: {format % args}", flush=True)

    def do_GET(self) -> None:  # noqa: N802 - stdlib signature
        path = urlsplit(self.path).path
        if path == "/v1/models":
            self._write_json(
                200,
                {
                    "object": "list",
                    "data": [
                        {
                            "id": self.config.model_id,
                            "object": "model",
                            "created": int(time.time()),
                            "owned_by": "apple",
                            "context_length": self.config.context_size,
                        }
                    ],
                },
            )
            return
        self._write_error(ProtocolError(f"no route for {path}", status=404, kind="invalid_request_error"))

    def do_POST(self) -> None:  # noqa: N802 - stdlib signature
        path = urlsplit(self.path).path
        if path != "/v1/chat/completions":
            self._write_error(ProtocolError(f"no route for {path}", status=404, kind="invalid_request_error"))
            return
        body = self._read_body()
        if body is None:
            return
        if self.dump_dir:
            self._dump(body)
        try:
            request = protocol.parse_chat_request(body)
        except ProtocolError as error:
            self._write_error(error)
            return
        for field in request.ignored_fields:
            self.log_message("ignoring unsupported request field '%s'", field)
        if request.stream:
            self._stream_completion(request)
        else:
            self._buffered_completion(request)

    def _dump(self, body: bytes) -> None:
        """Write one request body to the dump directory, numbered in arrival order."""
        target = Path(self.dump_dir)
        target.mkdir(parents=True, exist_ok=True)
        with self._dump_lock:
            sequence = next(type(self)._dump_sequence)
        (target / f"request-{sequence:04d}.json").write_bytes(body)

    def _read_body(self) -> bytes | None:
        raw_length = self.headers.get("Content-Length")
        try:
            length = int(raw_length) if raw_length is not None else None
        except ValueError:
            length = None
        if length is None or length < 0:
            self._write_error(ProtocolError("a Content-Length header is required", status=411))
            return None
        return self.rfile.read(length)

    def _start(self, request: ChatRequest) -> tuple["queue.Queue[tuple[str, Any]]", threading.Event, Any]:
        sink: "queue.Queue[tuple[str, Any]]" = queue.Queue()
        cancelled = threading.Event()
        future = _drain(self.backend, request, sink, cancelled, self.model_loop.loop)
        return sink, cancelled, future

    def _stream_completion(self, request: ChatRequest) -> None:
        sink, cancelled, future = self._start(request)
        completion = protocol.completion_id()
        created = int(time.time())
        sent_headers = False
        prompt_tokens = 0
        completion_tokens = 0
        try:
            while True:
                kind, payload = self._next(sink)
                if kind == "end":
                    break
                if kind == "error":
                    if sent_headers:
                        # The status line is already on the wire, so the failure
                        # travels as the error record tea's decoder expects.
                        self._write_chunk(protocol.sse_data(protocol.error_payload(payload)))
                        self._write_chunk(protocol.SSE_DONE)
                        self._end_chunked()
                    else:
                        self._write_error(payload)
                        return
                    break
                if isinstance(payload, TextDelta):
                    if not sent_headers:
                        self._begin_stream()
                        sent_headers = True
                    self._write_chunk(
                        protocol.sse_data(
                            protocol.chunk_payload(
                                completion, request.model, created, delta={"content": payload.text}
                            )
                        )
                    )
                elif isinstance(payload, ToolCalls):
                    if not sent_headers:
                        self._begin_stream()
                        sent_headers = True
                    self._write_chunk(
                        protocol.sse_data(
                            protocol.chunk_payload(
                                completion,
                                request.model,
                                created,
                                delta={
                                    "tool_calls": [
                                        protocol.tool_call_delta(index, call.call_id, call.name, call.arguments)
                                        for index, call in enumerate(payload.calls)
                                    ]
                                },
                            )
                        )
                    )
                elif isinstance(payload, Usage):
                    prompt_tokens = payload.prompt_tokens
                    completion_tokens = payload.completion_tokens
                elif isinstance(payload, Finish):
                    if not sent_headers:
                        self._begin_stream()
                        sent_headers = True
                    self._write_chunk(
                        protocol.sse_data(
                            protocol.chunk_payload(
                                completion,
                                request.model,
                                created,
                                delta={},
                                finish_reason=payload.reason,
                            )
                        )
                    )
                    if request.include_usage:
                        self._write_chunk(
                            protocol.sse_data(
                                protocol.usage_payload(
                                    completion,
                                    request.model,
                                    created,
                                    {
                                        "prompt_tokens": prompt_tokens,
                                        "completion_tokens": completion_tokens,
                                        "total_tokens": prompt_tokens + completion_tokens,
                                    },
                                )
                            )
                        )
                    self._write_chunk(protocol.SSE_DONE)
                    self._end_chunked()
                    break
        except (BrokenPipeError, ConnectionResetError):
            self.log_message("client disconnected; cancelling generation")
        finally:
            cancelled.set()
            future.cancel()
            self.close_connection = True

    def _next(self, sink: "queue.Queue[tuple[str, Any]]") -> tuple[str, Any]:
        """Block until the next event, error, or end arrives from the model loop."""
        while True:
            try:
                return sink.get(timeout=_QUEUE_POLL_SECONDS)
            except queue.Empty:
                continue

    def _buffered_completion(self, request: ChatRequest) -> None:
        sink, cancelled, future = self._start(request)
        text: list[str] = []
        tool_calls: list[Mapping[str, Any]] = []
        usage: Mapping[str, int] = {}
        reason = "stop"
        try:
            while True:
                kind, payload = self._next(sink)
                if kind == "end":
                    break
                if kind == "error":
                    self._write_error(payload)
                    return
                if isinstance(payload, TextDelta):
                    text.append(payload.text)
                elif isinstance(payload, ToolCalls):
                    tool_calls = [
                        {
                            "id": call.call_id,
                            "type": "function",
                            "function": {"name": call.name, "arguments": call.arguments},
                        }
                        for call in payload.calls
                    ]
                elif isinstance(payload, Usage):
                    usage = {
                        "prompt_tokens": payload.prompt_tokens,
                        "completion_tokens": payload.completion_tokens,
                        "total_tokens": payload.prompt_tokens + payload.completion_tokens,
                    }
                elif isinstance(payload, Finish):
                    reason = payload.reason
        finally:
            cancelled.set()
            future.cancel()
        self._write_json(
            200,
            protocol.completion_payload(
                protocol.completion_id(),
                request.model,
                int(time.time()),
                content="".join(text),
                tool_calls=tool_calls,
                finish_reason=reason,
                usage=usage,
            ),
        )

    def _begin_stream(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", protocol.SSE_CONTENT_TYPE)
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _write_chunk(self, payload: bytes) -> None:
        """Write one HTTP/1.1 chunk."""
        self.wfile.write(f"{len(payload):X}\r\n".encode("ascii"))
        self.wfile.write(payload)
        self.wfile.write(b"\r\n")
        self.wfile.flush()

    def _end_chunked(self) -> None:
        """Terminate the chunked body with its zero-length chunk.

        Without it the client reads a complete SSE stream and still reports a
        truncated response, because chunked framing has no other end marker.
        """
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _write_json(self, status: int, payload: Mapping[str, Any]) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _write_error(self, error: ProtocolError) -> None:
        self._write_json(error.status, protocol.error_payload(error))


class AppleFmServer(ThreadingHTTPServer):
    """A ``ThreadingHTTPServer`` that owns the model event loop."""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(
        self,
        backend: Backend,
        config: ServerConfig,
        *,
        verbose: bool = False,
        dump_dir: str | None = None,
    ) -> None:
        self.model_loop = _ModelLoop()
        handler = type(
            "BoundHandler",
            (_Handler,),
            {
                "backend": backend,
                "config": config,
                "model_loop": self.model_loop,
                "verbose": verbose,
                "dump_dir": dump_dir,
            },
        )
        super().__init__((config.host, config.port), handler)

    @property
    def bound_port(self) -> int:
        return int(self.server_address[1])

    def server_close(self) -> None:
        super().server_close()
        self.model_loop.close()


def create_server(
    backend: Backend,
    config: ServerConfig,
    *,
    verbose: bool = False,
    dump_dir: str | None = None,
) -> AppleFmServer:
    """Bind the HTTP server and start the loop that owns the Apple model."""
    return AppleFmServer(backend, config, verbose=verbose, dump_dir=dump_dir)
