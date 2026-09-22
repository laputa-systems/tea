"""OpenAI wire translation, without a model present."""

from __future__ import annotations

import json

import pytest

from apple_fm_server import protocol
from apple_fm_server.protocol import ProtocolError


def parse(**payload):
    return protocol.parse_chat_request(json.dumps(payload).encode())


def test_system_messages_become_instructions_and_the_prompt_ends_with_an_assistant_cue():
    request = parse(
        model="m",
        messages=[
            {"role": "system", "content": "You are terse."},
            {"role": "user", "content": "hello"},
        ],
    )

    assert request.instructions == "You are terse."
    assert request.prompt == "### user\nhello\n\n### assistant\n"


def test_a_tool_loop_round_trips_through_the_prompt():
    request = parse(
        model="m",
        messages=[
            {"role": "user", "content": "list files"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "call_1", "name": "bash", "arguments": {"command": "ls"}},
                ],
            },
            {
                "role": "tool",
                "content": "README.md",
                "tool_call_id": "call_1",
                "tool_name": "bash",
            },
            {"role": "user", "content": "now what?"},
        ],
    )

    assert '[tool call call_1] bash {"command":"ls"}' in request.prompt
    assert "### tool result (bash, call_1)\nREADME.md" in request.prompt
    assert request.prompt.endswith("### user\nnow what?\n\n### assistant\n")


def test_openai_wire_form_tool_calls_are_accepted_too():
    request = parse(
        model="m",
        messages=[
            {"role": "user", "content": "go"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "read", "arguments": '{"path":"a"}'},
                    }
                ],
            },
        ],
    )

    assert '[tool call call_9] read {"path":"a"}' in request.prompt


def test_tool_errors_are_marked_in_the_prompt():
    request = parse(
        model="m",
        messages=[
            {"role": "user", "content": "go"},
            {"role": "tool", "content": "denied", "tool_call_id": "c", "tool_name": "bash", "is_error": True},
        ],
    )

    assert "[error] denied" in request.prompt


def test_multimodal_content_parts_are_flattened_to_text():
    request = parse(
        model="m",
        messages=[{"role": "user", "content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]}],
    )

    assert "one\ntwo" in request.prompt


def test_tools_are_parsed_with_their_schema():
    request = parse(
        model="m",
        messages=[{"role": "user", "content": "go"}],
        tools=[
            {
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Run a command.",
                    "parameters": {"type": "object", "properties": {"command": {"type": "string"}}},
                },
            }
        ],
    )

    assert [spec.name for spec in request.tools] == ["bash"]
    assert request.tools[0].parameters["properties"]["command"]["type"] == "string"


def test_a_tool_without_parameters_gets_an_empty_object_schema():
    request = parse(
        model="m",
        messages=[{"role": "user", "content": "go"}],
        tools=[{"type": "function", "function": {"name": "now"}}],
    )

    assert request.tools[0].parameters == {"type": "object", "properties": {}}


def test_stream_options_request_usage_records():
    request = parse(
        model="m",
        messages=[{"role": "user", "content": "hi"}],
        stream=True,
        stream_options={"include_usage": True},
    )

    assert request.stream is True
    assert request.include_usage is True


def test_accepted_but_unhonoured_fields_are_named_for_the_log():
    request = parse(
        model="m",
        messages=[{"role": "user", "content": "hi"}],
        top_p=1.0,
        min_p=0.0,
        chat_template_kwargs={"enable_thinking": True},
    )

    assert set(request.ignored_fields) == {"top_p", "min_p", "chat_template_kwargs"}


@pytest.mark.parametrize(
    "body",
    [
        b"not json",
        b"[]",
        json.dumps({"model": "m"}).encode(),
        json.dumps({"model": "m", "messages": []}).encode(),
        json.dumps({"model": "m", "messages": [{"role": "wizard", "content": "hi"}]}).encode(),
        json.dumps({"messages": [{"role": "user", "content": "hi"}]}).encode(),
        json.dumps(
            {"model": "m", "messages": [{"role": "user", "content": "hi"}], "tools": [{"type": "web_search"}]}
        ).encode(),
        json.dumps(
            {"model": "m", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 0}
        ).encode(),
    ],
)
def test_malformed_requests_are_rejected_before_generation(body):
    with pytest.raises(ProtocolError):
        protocol.parse_chat_request(body)


def test_only_a_system_message_is_not_answerable():
    """A request with no user, assistant, or tool content has nothing to answer."""
    with pytest.raises(ProtocolError, match="at least one non-system message"):
        protocol.parse_chat_request(
            json.dumps({"model": "m", "messages": [{"role": "system", "content": "s"}]}).encode()
        )


def test_streaming_records_carry_the_openai_chunk_shape():
    chunk = protocol.chunk_payload("chatcmpl-1", "m", 5, delta={"content": "hi"})
    assert chunk["object"] == "chat.completion.chunk"
    assert chunk["choices"] == [{"index": 0, "delta": {"content": "hi"}, "finish_reason": None}]

    final = protocol.chunk_payload("chatcmpl-1", "m", 5, delta={}, finish_reason="stop")
    assert final["choices"][0]["finish_reason"] == "stop"


def test_usage_record_has_no_choices_so_include_usage_clients_can_find_it():
    record = protocol.usage_payload("chatcmpl-1", "m", 5, {"prompt_tokens": 3, "completion_tokens": 4})
    assert record["choices"] == []
    assert record["usage"] == {"prompt_tokens": 3, "completion_tokens": 4}


def test_tool_call_delta_is_emitted_whole():
    delta = protocol.tool_call_delta(0, "call_1", "bash", '{"command":"ls"}')
    assert delta == {
        "index": 0,
        "id": "call_1",
        "type": "function",
        "function": {"name": "bash", "arguments": '{"command":"ls"}'},
    }


def test_sse_framing_terminates_records_with_a_blank_line():
    assert protocol.sse_data({"a": 1}) == b'data: {"a": 1}\n\n'
    assert protocol.SSE_DONE == b"data: [DONE]\n\n"


def test_errors_carry_the_openai_envelope():
    payload = protocol.error_payload(ProtocolError("nope", status=503, kind="server_error"))
    assert payload == {"error": {"message": "nope", "type": "server_error", "param": None, "code": None}}
