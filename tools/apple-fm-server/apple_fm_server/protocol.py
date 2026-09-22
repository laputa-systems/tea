"""OpenAI Chat Completions wire shapes, and their translation into one Apple prompt.

This module owns what a client sends and what this server writes back. It never
imports the Apple SDK, so its tests run without a model present.

Apple's on-device model is not a message-list API: a ``LanguageModelSession``
carries instructions plus a running transcript, and this server creates one
fresh session per request. The conversation a client sends is therefore
rendered into a single prompt by :func:`render_conversation`. The rendering is
deliberately plain text — a small model follows an explicit turn transcript
better than it follows JSON — and it round-trips assistant tool calls and tool
results, which is what makes an OpenAI tool loop work at all here.
"""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass, field
from typing import Any, Mapping, Sequence

# Roles accepted from a client. Anything else is a client bug, not a model
# limitation, so it is rejected before any generation starts.
_KNOWN_ROLES = frozenset({"system", "user", "assistant", "tool"})

# The cue that ends every rendered prompt: the model continues from the
# assistant turn rather than answering a bare question.
_ASSISTANT_CUE = "### assistant"


class ProtocolError(Exception):
    """A failure that maps onto an OpenAI error response.

    ``status`` is the HTTP status and ``kind`` the OpenAI ``error.type``. Both
    are chosen where the condition is detected, so the HTTP layer stays free of
    provider conditions.
    """

    def __init__(self, message: str, *, status: int = 400, kind: str = "invalid_request_error") -> None:
        super().__init__(message)
        self.status = status
        self.kind = kind


@dataclass(frozen=True, slots=True)
class ToolSpec:
    """One OpenAI function tool offered to the model."""

    name: str
    description: str
    parameters: Mapping[str, Any]


@dataclass(frozen=True, slots=True)
class ChatRequest:
    """One parsed Chat Completions request, already reduced to what the model needs."""

    model: str
    instructions: str
    prompt: str
    tools: tuple[ToolSpec, ...] = ()
    max_tokens: int | None = None
    temperature: float | None = None
    stream: bool = True
    include_usage: bool = False
    # Request fields this server accepted but does not honour, named for the log.
    ignored_fields: tuple[str, ...] = field(default=())


def parse_chat_request(body: bytes) -> ChatRequest:
    """Parse and validate an OpenAI Chat Completions request body."""
    try:
        payload = json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProtocolError(f"request body was not valid JSON: {error}") from error
    if not isinstance(payload, dict):
        raise ProtocolError("request body must be a JSON object")

    messages = payload.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ProtocolError("'messages' must be a non-empty array")

    instructions, prompt = render_conversation(messages)

    # Accepted-and-ignored request fields. Each one has a plausible-looking
    # Apple counterpart that would mean something else: top_p/min_p would have
    # to become top-k, and chat_template_kwargs is a chat-template switch the
    # on-device model does not expose.
    ignored = tuple(name for name in ("top_p", "min_p", "chat_template_kwargs") if name in payload)

    model = payload.get("model")
    if not isinstance(model, str) or not model.strip():
        raise ProtocolError("'model' must be a non-empty string")

    return ChatRequest(
        model=model,
        instructions=instructions,
        prompt=prompt,
        tools=_parse_tools(payload.get("tools")),
        max_tokens=_parse_max_tokens(payload.get("max_tokens")),
        temperature=_parse_temperature(payload.get("temperature")),
        stream=bool(payload.get("stream", False)),
        include_usage=_parse_include_usage(payload.get("stream_options")),
        ignored_fields=tuple(ignored),
    )


def _parse_tools(raw: Any) -> tuple[ToolSpec, ...]:
    if raw is None:
        return ()
    if not isinstance(raw, list):
        raise ProtocolError("'tools' must be an array")
    specs: list[ToolSpec] = []
    for entry in raw:
        if not isinstance(entry, dict) or entry.get("type") != "function":
            raise ProtocolError("every tool must have type 'function'")
        function = entry.get("function")
        if not isinstance(function, dict):
            raise ProtocolError("every tool must carry a 'function' object")
        name = function.get("name")
        if not isinstance(name, str) or not name.strip():
            raise ProtocolError("every tool function must have a non-empty name")
        parameters = function.get("parameters")
        if parameters is None:
            parameters = {"type": "object", "properties": {}}
        if not isinstance(parameters, dict):
            raise ProtocolError(f"tool '{name}' parameters must be a JSON Schema object")
        description = function.get("description")
        specs.append(
            ToolSpec(
                name=name,
                description=description if isinstance(description, str) else "",
                parameters=parameters,
            )
        )
    return tuple(specs)


def _parse_max_tokens(raw: Any) -> int | None:
    if raw is None:
        return None
    if isinstance(raw, bool) or not isinstance(raw, int) or raw <= 0:
        raise ProtocolError("'max_tokens' must be a positive integer")
    return raw


def _parse_temperature(raw: Any) -> float | None:
    if raw is None:
        return None
    if isinstance(raw, bool) or not isinstance(raw, (int, float)):
        raise ProtocolError("'temperature' must be a number")
    return float(raw)


def _parse_include_usage(raw: Any) -> bool:
    if not isinstance(raw, dict):
        return False
    return bool(raw.get("include_usage"))


def render_conversation(messages: Sequence[Any]) -> tuple[str, str]:
    """Render OpenAI *messages* as (instructions, prompt).

    System messages become the session instructions because that is the only
    durable channel Apple's session gives them. Everything else is appended to
    one prompt transcript in source order.
    """
    instructions: list[str] = []
    blocks: list[str] = []
    for message in messages:
        if not isinstance(message, dict):
            raise ProtocolError("every message must be a JSON object")
        role = message.get("role")
        if role not in _KNOWN_ROLES:
            raise ProtocolError(f"unsupported message role {role!r}")
        content = _message_text(message)
        if role == "system":
            if content:
                instructions.append(content)
            continue
        if role == "user":
            blocks.append(f"### user\n{content}")
        elif role == "assistant":
            blocks.append(_render_assistant(content, message.get("tool_calls")))
        else:
            blocks.append(_render_tool_result(content, message))
    if not blocks:
        # Instructions alone are not a request: the model would be answering
        # nothing, and a silent empty answer would hide the client's bug.
        raise ProtocolError("'messages' must contain at least one non-system message")
    blocks.append(_ASSISTANT_CUE)
    return "\n\n".join(instructions), "\n\n".join(blocks) + "\n"


def _message_text(message: Mapping[str, Any]) -> str:
    """Return a message's text, accepting the multimodal content-parts form."""
    content = message.get("content")
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for part in content:
            if isinstance(part, dict) and isinstance(part.get("text"), str):
                parts.append(part["text"])
        return "\n".join(parts)
    raise ProtocolError(f"message content must be a string or an array of text parts, got {type(content).__name__}")


def _render_assistant(content: str, tool_calls: Any) -> str:
    lines = [_ASSISTANT_CUE]
    if content:
        lines.append(content)
    if isinstance(tool_calls, list):
        for call in tool_calls:
            if not isinstance(call, dict):
                continue
            name, call_id, arguments = _tool_call_parts(call)
            lines.append(f"[tool call {call_id}] {name} {arguments}")
    return "\n".join(lines)


def _tool_call_parts(call: Mapping[str, Any]) -> tuple[str, str, str]:
    """Return (name, id, arguments-json) for one assistant tool call.

    tea projects ``arguments`` as an already-parsed JSON value, while the
    OpenAI wire form is a JSON *string*; both are accepted.
    """
    name = call.get("name")
    function = call.get("function")
    if not isinstance(name, str) and isinstance(function, dict):
        name = function.get("name")
    arguments = call.get("arguments")
    if arguments is None and isinstance(function, dict):
        arguments = function.get("arguments")
    call_id = call.get("id")
    return (
        name if isinstance(name, str) and name else "unknown",
        call_id if isinstance(call_id, str) and call_id else "unknown",
        _arguments_text(arguments),
    )


def _arguments_text(arguments: Any) -> str:
    if arguments is None:
        return "{}"
    if isinstance(arguments, str):
        return arguments
    return json.dumps(arguments, ensure_ascii=False, separators=(",", ":"))


def _render_tool_result(content: str, message: Mapping[str, Any]) -> str:
    name = message.get("tool_name")
    call_id = message.get("tool_call_id")
    name_text = name if isinstance(name, str) else "unknown"
    id_text = call_id if isinstance(call_id, str) else "unknown"
    label = f"### tool result ({name_text}, {id_text})"
    prefix = "[error] " if message.get("is_error") else ""
    return f"{label}\n{prefix}{content}"


def completion_id() -> str:
    """A fresh OpenAI-shaped completion identifier."""
    return f"chatcmpl-{uuid.uuid4().hex}"


def sse_data(payload: Mapping[str, Any]) -> bytes:
    """Encode one Server-Sent Events ``data:`` record."""
    return b"data: " + json.dumps(payload, ensure_ascii=False).encode("utf-8") + b"\n\n"


SSE_DONE = b"data: [DONE]\n\n"
SSE_CONTENT_TYPE = "text/event-stream"


def chunk_payload(
    completion: str,
    model: str,
    created: int,
    *,
    delta: Mapping[str, Any],
    finish_reason: str | None = None,
) -> dict[str, Any]:
    """One streaming ``chat.completion.chunk`` carrying *delta*."""
    return {
        "id": completion,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": dict(delta), "finish_reason": finish_reason}],
    }


def usage_payload(completion: str, model: str, created: int, usage: Mapping[str, int]) -> dict[str, Any]:
    """The final ``chat.completion.chunk`` that carries token counts.

    OpenAI emits this record with an empty ``choices`` array, which is what
    clients that asked for ``stream_options.include_usage`` expect.
    """
    return {
        "id": completion,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [],
        "usage": dict(usage),
    }


def completion_payload(
    completion: str,
    model: str,
    created: int,
    *,
    content: str,
    tool_calls: Sequence[Mapping[str, Any]],
    finish_reason: str,
    usage: Mapping[str, int],
) -> dict[str, Any]:
    """A non-streaming ``chat.completion`` response."""
    message: dict[str, Any] = {"role": "assistant", "content": content or None}
    if tool_calls:
        message["tool_calls"] = [dict(call) for call in tool_calls]
    return {
        "id": completion,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": dict(usage),
    }


def tool_call_delta(index: int, call_id: str, name: str, arguments: str) -> dict[str, Any]:
    """One tool call in streaming delta form.

    The call is emitted whole in a single record: the model already produced
    it, and splitting a complete call across records would only invite clients
    to reassemble something that never streamed.
    """
    return {
        "index": index,
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }


def error_payload(error: ProtocolError) -> dict[str, Any]:
    """The OpenAI error envelope for *error*."""
    return {"error": {"message": str(error), "type": error.kind, "param": None, "code": None}}
