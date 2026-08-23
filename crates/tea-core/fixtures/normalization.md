# Canonical result and normalization policy

The Rust runner emits one JSON object with `kind: "canonical_parity_result"`. The fixture check compares
these objects, never raw provider or Rust event JSON. This keeps provider wire format and runtime
bookkeeping outside the fixture contract while retaining observable agent behavior.

## Canonical result shape

```json
{
  "format_version": 1,
  "kind": "canonical_parity_result",
  "fixture_id": "single-turn-text",
  "outcome": "completed",
  "settled": true,
  "state": {
    "system_prompt": "...",
    "model": { "provider": "fixture", "id": "deterministic-text" },
    "thinking_level": "off",
    "tool_names": [],
    "pending_tool_calls": []
  },
  "events": [
    { "seq": 0, "type": "agent_start", "data": {} }
  ],
  "messages": [
    {
      "role": "user",
      "content": [{ "type": "text", "text": "..." }]
    },
    {
      "role": "assistant",
      "content": [{ "type": "text", "text": "..." }]
    }
  ],
  "last_response": { "api": "fixture", "stop_reason": "stop" },
  "usage": {
    "input": 0,
    "output": 0,
    "cache_read": 0,
    "cache_write": 0,
    "total_tokens": 0
  },
  "error": null
}
```

All fields shown are required. `error` is `null` for `completed` and `cancelled` outcomes. An
`error` object is required for `error` outcomes and has `kind`, `message`, and `retryable`; a
runner may add a stable `code`, but must not add raw provider envelopes. `last_response` is
`null` when no model response was received. `settled` must be `true` for every emitted result.

`state.pending_tool_calls` must be empty after settlement. `state.tool_names` preserves the
configured tool order. A message always has `role` and an ordered `content` array. Message content
parts use one of these forms:

```json
{ "type": "text", "text": "..." }
{ "type": "thinking", "text": "..." }
{ "type": "tool_call", "id": "tool_call_1", "name": "echo", "arguments": {} }
{ "type": "tool_result", "tool_call_id": "tool_call_1", "is_error": false, "content": [] }
{ "type": "json", "value": {} }
```

The canonical event vocabulary is lifecycle-oriented: `agent_start`, `turn_start`,
`message_start`, `message_update`, `message_end`, `tool_execution_start`,
`tool_execution_update`, `tool_execution_end`, `turn_end`, `agent_end`, and `agent_settled`.
Role, turn, tool name, and other stable details live under `data`; event order is the `seq` order.
`seq` is reassigned from zero after normalization.

## Normalization rules

Apply these rules in this order:

1. Validate the result and reject malformed or unknown required values. A failed normalization is
   a harness error, not a passing comparison.
2. Convert CRLF and CR to LF in text fields. Preserve every other Unicode code point, including
   leading/trailing whitespace, case, and intentional blank lines. Do not trim or collapse text.
3. Normalize JSON objects by key for serialization and represent absent optional values according
   to the canonical shape (`null` where the field is required to be nullable). Array order,
   message order, content order, event order, and tool order remain significant.
4. Replace runtime-generated identifiers by encounter order, independently by category:
   `run_1`, `message_1`, and `tool_call_1`. Apply the same mapping to tool-result references.
   Fixture-local tool-call IDs are normalized too, so an SDK/provider ID format cannot affect a
   comparison. IDs that are part of user-visible text are not rewritten.
5. Remove fields that are runtime metadata rather than behavior: timestamps, elapsed durations,
   process IDs, session IDs, request IDs, provider trace IDs, raw headers, and event object keys
   that are not in the canonical shape. Unknown semantic fields must cause a reviewable schema
   update instead of being silently dropped.
6. In paths and error text only, replace the explicitly configured workspace root with
   `<workspace>`, temporary roots with `<temp>`, and the home directory with `<home>`; normalize
   path separators to `/`. Do not redact arbitrary text that merely resembles a path.
7. Keep usage values exactly as reported. A reported zero is `0`; an unavailable counter is
   `null` only where the provider genuinely did not supply it. Never infer token counts from a
   tokenizer in the fixture adapter.
8. By default, join adjacent `message_update` text deltas for the same message before assigning
   `seq`. This makes provider chunk boundaries non-semantic. A fixture may opt into exact stream
   boundaries with `stream_comparison: "exact"`; then each declared update remains observable.
   Tool execution updates and lifecycle order are never reordered or discarded.
9. Omit `ProviderRequestObserved` from the lifecycle event array. It is runtime telemetry, while
   a fixture that opts into context hooks records the normalized request at the provider boundary
   in `request_trace`; the two representations must not duplicate one semantic request.

Normalization is not a license to hide behavior. Provider error codes, stop reasons, tool names,
tool arguments, tool-result error flags, message content, usage, cancellation outcome, and event
ordering remain observable. If a value is nondeterministic but semantically important, the fixture
must provide a deterministic substitute or the contract must explicitly classify it.
