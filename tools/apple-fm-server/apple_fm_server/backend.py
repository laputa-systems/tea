"""The Apple Foundation Models boundary.

Everything that touches ``apple_fm_sdk`` lives here, behind :class:`Backend`.
The server layer consumes :class:`Event` values and never sees a session, a
schema, or a system-language-model handle.

Two facts about the on-device model shape this module:

* ``stream_response`` yields *cumulative* snapshots, not deltas, so deltas are
  recovered by difference.
* The model executes tools inside its own session, so a tool call is observed
  through :class:`~apple_fm_server.tools.ToolCallRecorder` rather than returned.
  Text produced after a call is generated against the placeholder result and is
  therefore never forwarded; generation is abandoned at the first recorded call.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
from dataclasses import dataclass
from typing import Any, AsyncIterator, Protocol, Union

from .protocol import ChatRequest, ProtocolError
from .tools import RecordedCall, ToolCallRecorder, convert_tools

# Reasons the model may make a tool call rather than text. Only the two Apple
# reports through options survive to the client: the on-device model has no
# length-stop signal beyond its own token cap.
STOP = "stop"
TOOL_CALLS = "tool_calls"

# How long the text pump waits before re-checking whether a tool call landed.
# Bounds how long generation continues after a call, and costs at most this much
# latency on reporting one.
_CALL_POLL_SECONDS = 0.1


@dataclass(frozen=True, slots=True)
class TextDelta:
    """A genuine increment of assistant text."""

    text: str


@dataclass(frozen=True, slots=True)
class ToolCalls:
    """Calls the client must execute before continuing."""

    calls: tuple[RecordedCall, ...]


@dataclass(frozen=True, slots=True)
class Usage:
    """Token counts measured with the model's own tokenizer."""

    prompt_tokens: int
    completion_tokens: int


@dataclass(frozen=True, slots=True)
class Finish:
    """The terminal event, carrying an OpenAI finish reason."""

    reason: str


Event = Union[TextDelta, ToolCalls, Usage, Finish]


class Backend(Protocol):
    """The narrow boundary the HTTP layer depends on."""

    def chat(self, request: ChatRequest) -> AsyncIterator[Event]:
        """Stream one response for *request*."""
        ...


def import_fm() -> Any:
    """Import ``apple_fm_sdk``, or raise a user-facing error naming the fix."""
    try:
        import apple_fm_sdk as fm
    except ImportError as error:
        raise ProtocolError(
            "Apple Foundation Models SDK is unavailable. This server needs macOS 26+ on Apple "
            "Silicon with Xcode 26+ installed (and its license agreed to) plus Apple Intelligence "
            "enabled. Install the SDK with `uv sync` in tools/apple-fm-server.",
            status=503,
            kind="server_error",
        ) from error
    return fm


def create_backend() -> "AppleBackend":
    """Build the live backend, naming the specific failure when the model is unavailable."""
    fm = import_fm()
    try:
        model = fm.SystemLanguageModel()
    except ImportError as error:
        raise ProtocolError(
            "Apple Foundation Models C bindings are unavailable. Install Xcode 26+, open it once "
            "to agree to the license, then reinstall apple-fm-sdk.",
            status=503,
            kind="server_error",
        ) from error
    except Exception as error:
        raise ProtocolError(
            f"Could not initialize the Apple Foundation Models model: {error}",
            status=503,
            kind="server_error",
        ) from error
    available, reason = model.is_available()
    if not available:
        raise ProtocolError(_unavailable_message(reason), status=503, kind="server_error")
    return AppleBackend(fm, model)


def _unavailable_message(reason: Any) -> str:
    name = getattr(reason, "name", None)
    messages = {
        "APPLE_INTELLIGENCE_NOT_ENABLED": (
            "Apple Intelligence is not enabled. Enable it in System Settings to use the on-device model."
        ),
        "DEVICE_NOT_ELIGIBLE": (
            "This device does not support Apple Foundation Models; Apple Silicon with Apple "
            "Intelligence is required."
        ),
        "MODEL_NOT_READY": (
            "The on-device Foundation Models model is not ready yet (still downloading or "
            "preparing). Try again later."
        ),
    }
    if name in messages:
        return messages[name]
    if reason is not None:
        return f"Apple Foundation Models model is unavailable ({reason})."
    return "Apple Foundation Models model is unavailable for an unknown reason."


class AppleBackend:
    """The live :class:`Backend` over the default on-device system model."""

    def __init__(self, fm: Any, model: Any) -> None:
        self._fm = fm
        self._model = model
        # The on-device model serves one generation at a time. Requests are
        # queued here rather than failing, which is what a single local model
        # can honestly offer a multi-request client.
        self._generation_lock = asyncio.Lock()
        # Instructions and tool definitions repeat across every turn of a
        # session, and each measurement is a round trip to the system service,
        # so their counts are cached by exact content.
        self._overhead_cache: dict[tuple[str, tuple[tuple[str, str], ...]], int] = {}

    @property
    def context_size(self) -> int:
        return int(self._model.context_size)

    async def chat(self, request: ChatRequest) -> AsyncIterator[Event]:
        """Stream one response, serialized against other in-flight requests."""
        async with self._generation_lock:
            async for event in self._chat(request):
                yield event

    async def _chat(self, request: ChatRequest) -> AsyncIterator[Event]:
        recorder = ToolCallRecorder()
        tool_objects, warnings = convert_tools(self._fm, request.tools, recorder)
        for warning in warnings:
            print(f"apple-fm-server: tool schema {warning}", flush=True)

        prompt_tokens, overhead = await self._measure(request, tool_objects)
        budget = self.context_size - prompt_tokens - overhead
        if budget <= 0:
            raise ProtocolError(
                f"request needs {prompt_tokens + overhead} tokens but the on-device model holds "
                f"{self.context_size}; shorten the conversation or the tool schemas."
            )

        session = self._fm.LanguageModelSession(
            instructions=request.instructions, tools=tool_objects
        )
        options = self._options(request, budget)
        forwarded: list[str] = []
        try:
            async for delta in self._forward(session, request, options, recorder):
                forwarded.append(delta.text)
                yield delta
        except Exception as error:
            raise _generation_error(self._fm, error) from error

        if recorder.recorded:
            yield ToolCalls(recorder.calls)
            completion_tokens = await self._count_tokens(
                "".join(f"{call.name}{call.arguments}" for call in recorder.calls)
            )
            yield Usage(prompt_tokens + overhead, completion_tokens)
            yield Finish(TOOL_CALLS)
            return
        yield Usage(prompt_tokens + overhead, await self._count_tokens("".join(forwarded)))
        yield Finish(STOP)

    async def _forward(
        self, session: Any, request: ChatRequest, options: Any, recorder: ToolCallRecorder
    ) -> AsyncIterator[TextDelta]:
        """Yield assistant text, stopping at the first tool call.

        A tool call produces no text snapshot, so nothing in the stream itself
        announces one; the recorder is the only signal. Left alone, the model
        keeps calling the same tool against a placeholder result — observed
        live: four identical calls over 26 seconds. Generation is therefore
        pumped by a task this loop watches, and cancelled as soon as the first
        call lands, so the client gets the call instead of a long silence or an
        error from a torn-down stream.
        """
        updates: "asyncio.Queue[tuple[str, Any]]" = asyncio.Queue()

        async def pump() -> None:
            try:
                async for snapshot in session.stream_response(request.prompt, options):
                    updates.put_nowait(("snapshot", snapshot))
            except asyncio.CancelledError:
                raise
            except Exception as error:
                updates.put_nowait(("error", error))
            finally:
                updates.put_nowait(("end", None))

        task = asyncio.create_task(pump())
        emitted = 0
        try:
            while True:
                if recorder.recorded:
                    return
                try:
                    kind, payload = await asyncio.wait_for(updates.get(), timeout=_CALL_POLL_SECONDS)
                except asyncio.TimeoutError:
                    continue
                if kind == "end":
                    return
                if kind == "error":
                    if recorder.recorded:
                        # Stopping generation at a deferred call is reported by
                        # the SDK as a failure of the whole stream (observed as
                        # GenerationError status 255). The call is what the
                        # client needs, and a real problem would resurface on the
                        # next turn, so the recorded call wins — but loud, since
                        # a real failure is being swallowed here.
                        print(
                            f"apple-fm-server: ignoring {type(payload).__name__} raised while "
                            "stopping a deferred tool call",
                            flush=True,
                        )
                        return
                    raise payload
                if len(payload) > emitted:
                    delta = payload[emitted:]
                    emitted = len(payload)
                    yield TextDelta(delta)
        finally:
            task.cancel()
            # The cancelled generation may report a failure on close; it is the
            # consequence of stopping it deliberately, not a client-facing error.
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await task

    async def _count_tokens(self, text: str) -> int:
        """Count emitted assistant content with the model's own tokenizer.

        Apple reports no usage for a generation, so this measures what the
        client actually received rather than inventing a number. An empty
        response costs nothing and skips the round trip.
        """
        return int(await self._model.token_count(text)) if text else 0

    async def _measure(self, request: ChatRequest, tools: Sequence[Any]) -> tuple[int, int]:
        """Return (prompt tokens, cached instructions + tool-schema tokens)."""
        # Keyed on the exact schemas, not the tool names: a client may reuse a
        # name with different parameters, and a stale count would misreport the
        # budget the request has left.
        cache_key = (
            request.instructions,
            tuple(
                (spec.name, json.dumps(spec.parameters, sort_keys=True))
                for spec in request.tools
            ),
        )
        overhead = self._overhead_cache.get(cache_key)
        if overhead is None:
            overhead = 0
            if request.instructions:
                overhead += int(await self._model.token_count(instructions=request.instructions))
            if tools:
                overhead += int(await self._model.token_count(list(tools)))
            self._overhead_cache[cache_key] = overhead
        prompt_tokens = int(await self._model.token_count(request.prompt))
        return prompt_tokens, overhead

    def _options(self, request: ChatRequest, budget: int) -> Any:
        """Decoding options, with the response capped to the remaining context."""
        maximum = budget if request.max_tokens is None else min(request.max_tokens, budget)
        return self._fm.GenerationOptions(
            temperature=request.temperature,
            maximum_response_tokens=max(1, maximum),
        )


def _generation_error(fm: Any, error: Exception) -> ProtocolError:
    """Translate an SDK failure into an OpenAI-shaped error."""
    name = type(error).__name__
    if name == "ExceededContextWindowSizeError":
        return ProtocolError(
            "the conversation exceeds the on-device model's context window; the request was not "
            "generated."
        )
    if name in ("GuardrailViolationError", "RefusalError"):
        return ProtocolError(f"the on-device model declined to answer ({name}).")
    if name in ("AssetsUnavailableError", "ConcurrentRequestsError"):
        return ProtocolError(
            f"the on-device model is unavailable ({name}).", status=503, kind="server_error"
        )
    if name == "UnsupportedLanguageOrLocaleError":
        return ProtocolError(f"the on-device model does not support this language or locale ({error}).")
    return ProtocolError(
        f"Apple Foundation Models generation failed ({name}): {error}", status=500, kind="server_error"
    )
