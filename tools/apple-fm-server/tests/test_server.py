"""HTTP behavior of the OpenAI-compatible surface, against a scripted backend."""

from __future__ import annotations

import http.client
import json
import threading
from dataclasses import dataclass
from typing import AsyncIterator, Sequence

import pytest

from apple_fm_server.backend import Event, Finish, TextDelta, ToolCalls, Usage
from apple_fm_server.protocol import ChatRequest, ProtocolError
from apple_fm_server.server import ServerConfig, create_server
from apple_fm_server.tools import RecordedCall


@dataclass
class FakeBackend:
    """A Backend that replays scripted events, or fails at a scripted point."""

    events: Sequence[Event | ProtocolError]
    requests: list[ChatRequest] | None = None

    def __post_init__(self) -> None:
        self.requests = []

    async def chat(self, request: ChatRequest) -> AsyncIterator[Event]:
        self.requests.append(request)
        for event in self.events:
            if isinstance(event, ProtocolError):
                raise event
            yield event


@pytest.fixture
def server_factory():
    servers = []

    def start(backend) -> tuple[str, int]:
        config = ServerConfig(host="127.0.0.1", port=0, model_id="test-model", context_size=4096)
        server = create_server(backend, config)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        servers.append(server)
        return config.host, server.bound_port

    yield start
    for server in servers:
        server.shutdown()
        server.server_close()


def post(host: str, port: int, path: str, payload) -> http.client.HTTPResponse:
    connection = http.client.HTTPConnection(host, port, timeout=10)
    body = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
    connection.request("POST", path, body=body, headers={"Content-Type": "application/json"})
    return connection.getresponse()


def get(host: str, port: int, path: str) -> http.client.HTTPResponse:
    connection = http.client.HTTPConnection(host, port, timeout=10)
    connection.request("GET", path)
    return connection.getresponse()


def sse_records(response: http.client.HTTPResponse) -> list[dict | str]:
    """Every SSE record in body order, with ``[DONE]`` kept as a marker."""
    records: list[dict | str] = []
    while True:
        line = response.readline()
        if not line:
            return records
        text = line.decode("utf-8").strip()
        if not text.startswith("data:"):
            continue
        payload = text[len("data:") :].strip()
        records.append(payload if payload == "[DONE]" else json.loads(payload))


def chat_request(**overrides) -> dict:
    payload = {
        "model": "test-model",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": True,
    }
    payload.update(overrides)
    return payload


def test_models_lists_the_configured_model(server_factory):
    host, port = server_factory(FakeBackend([]))
    response = get(host, port, "/v1/models")

    assert response.status == 200
    body = json.loads(response.read())
    assert body["object"] == "list"
    assert body["data"][0]["id"] == "test-model"
    assert body["data"][0]["context_length"] == 4096


def test_an_unknown_route_is_a_404_in_the_openai_error_shape(server_factory):
    host, port = server_factory(FakeBackend([]))
    response = get(host, port, "/v1/nope")

    assert response.status == 404
    assert json.loads(response.read())["error"]["type"] == "invalid_request_error"


def test_streaming_deltas_arrive_as_separate_records_and_end_with_done(server_factory):
    backend = FakeBackend(
        [
            TextDelta("Hel"),
            TextDelta("lo"),
            Usage(prompt_tokens=7, completion_tokens=2),
            Finish("stop"),
        ]
    )
    host, port = server_factory(backend)
    response = post(host, port, "/v1/chat/completions", chat_request(stream_options={"include_usage": True}))

    assert response.status == 200
    assert response.getheader("Content-Type") == "text/event-stream"
    records = sse_records(response)

    deltas = [
        record["choices"][0]["delta"].get("content")
        for record in records
        if record != "[DONE]" and record["choices"]
    ]
    assert "".join(text for text in deltas if text) == "Hello"
    assert records[-1] == "[DONE]"
    assert records[-2]["usage"] == {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
    assert records[-3]["choices"][0]["finish_reason"] == "stop"
    assert backend.requests[0].prompt == "### user\nhello\n\n### assistant\n"


def test_the_chunked_body_terminates_cleanly(server_factory):
    """A client parsing chunked framing must not see a truncated response."""
    backend = FakeBackend([TextDelta("hi"), Usage(prompt_tokens=1, completion_tokens=1), Finish("stop")])
    host, port = server_factory(backend)
    response = post(host, port, "/v1/chat/completions", chat_request())

    body = response.read()  # raises IncompleteRead if the zero-length chunk is missing

    assert body.rstrip().endswith(b"data: [DONE]")


def test_usage_is_omitted_unless_the_client_asked_for_it(server_factory):
    backend = FakeBackend([TextDelta("hi"), Usage(prompt_tokens=1, completion_tokens=1), Finish("stop")])
    host, port = server_factory(backend)
    records = sse_records(post(host, port, "/v1/chat/completions", chat_request()))

    assert all("usage" not in record for record in records if record != "[DONE]")


def test_a_tool_call_reaches_the_client_with_a_finish_reason_of_tool_calls(server_factory):
    backend = FakeBackend(
        [
            ToolCalls((RecordedCall(name="bash", call_id="call_1", arguments='{"command":"ls"}'),)),
            Usage(prompt_tokens=3, completion_tokens=5),
            Finish("tool_calls"),
        ]
    )
    host, port = server_factory(backend)
    records = sse_records(post(host, port, "/v1/chat/completions", chat_request()))

    call = records[0]["choices"][0]["delta"]["tool_calls"][0]
    assert call["id"] == "call_1"
    assert call["function"] == {"name": "bash", "arguments": '{"command":"ls"}'}
    assert records[1]["choices"][0]["finish_reason"] == "tool_calls"
    assert records[-1] == "[DONE]"


def test_a_failure_before_any_output_is_an_http_error(server_factory):
    backend = FakeBackend([ProtocolError("request needs 5000 tokens but the model holds 4096")])
    host, port = server_factory(backend)
    response = post(host, port, "/v1/chat/completions", chat_request())

    assert response.status == 400
    assert "5000 tokens" in json.loads(response.read())["error"]["message"]


def test_a_failure_after_output_travels_as_an_error_record_then_done(server_factory):
    backend = FakeBackend([TextDelta("partial"), ProtocolError("the model declined to answer")])
    host, port = server_factory(backend)
    records = sse_records(post(host, port, "/v1/chat/completions", chat_request()))

    assert records[0]["choices"][0]["delta"]["content"] == "partial"
    assert "declined" in records[1]["error"]["message"]
    assert records[2] == "[DONE]"


def test_a_non_streaming_request_returns_one_completion(server_factory):
    backend = FakeBackend(
        [
            TextDelta("Hel"),
            TextDelta("lo"),
            Usage(prompt_tokens=2, completion_tokens=2),
            Finish("stop"),
        ]
    )
    host, port = server_factory(backend)
    response = post(host, port, "/v1/chat/completions", chat_request(stream=False))

    body = json.loads(response.read())
    assert body["object"] == "chat.completion"
    assert body["choices"][0]["message"] == {"role": "assistant", "content": "Hello"}
    assert body["choices"][0]["finish_reason"] == "stop"
    assert body["usage"]["total_tokens"] == 4


def test_a_non_streaming_tool_call_carries_the_call_in_the_message(server_factory):
    backend = FakeBackend(
        [
            ToolCalls((RecordedCall(name="read", call_id="call_7", arguments='{"path":"a"}'),)),
            Finish("tool_calls"),
        ]
    )
    host, port = server_factory(backend)
    body = json.loads(
        post(host, port, "/v1/chat/completions", chat_request(stream=False)).read()
    )

    message = body["choices"][0]["message"]
    assert message["content"] is None
    assert message["tool_calls"][0]["function"]["name"] == "read"
    assert body["choices"][0]["finish_reason"] == "tool_calls"


def test_a_malformed_body_is_rejected_without_reaching_the_model(server_factory):
    backend = FakeBackend([])
    host, port = server_factory(backend)
    response = post(host, port, "/v1/chat/completions", b"{not json")

    assert response.status == 400
    assert backend.requests == []
