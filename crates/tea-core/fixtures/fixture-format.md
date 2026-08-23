# Declarative fixture format

Declarative fixtures describe an agent run without describing how either implementation invokes
it. They are JSON files with `format_version: 1` and `kind: "declarative_parity_fixture"`.
Unknown top-level fields are rejected so a typo cannot silently change a test. JSON object key
order is insignificant; array order is significant.

## Shape

The required shape is:

```json
{
  "format_version": 1,
  "kind": "declarative_parity_fixture",
  "id": "single-turn-text",
  "description": "A short human-readable reason for this case.",
  "setup": {
    "system_prompt": "...",
    "model": { "provider": "fixture", "id": "deterministic-text" },
    "thinking_level": "off",
    "tools": []
  },
  "actions": [
    { "kind": "prompt", "text": "..." }
  ],
  "model_script": [
    {
      "chunks": [
        { "kind": "text_delta", "text": "..." },
        {
          "kind": "done",
          "stop_reason": "stop",
          "usage": {
            "input": 0,
            "output": 0,
            "cache_read": 0,
            "cache_write": 0,
            "total_tokens": 0
          }
        }
      ]
    }
  ],
  "stream_comparison": "semantic",
  "host": { "tools": [] },
  "assertions": {
    "outcome": "completed",
    "event_types": ["agent_start", "agent_settled"]
  }
}
```

`description` is required for review but is not compared. `id` is a stable slash-separated
identifier, unique within the fixture tree, and is also the basename used for expected and
normalized output. It must not contain a path escape, an absolute path, or a provider secret.
`stream_comparison` is optional and defaults to `semantic`; its only other value is `exact`.

### `setup`

`setup` is explicit agent state:

* `system_prompt` is the exact prompt text, including intentional whitespace.
* `model` has the provider and model identifier. For deterministic fixtures the provider should
  be `fixture`; a live provider is not permitted in a declarative fixture.
* `thinking_level` is one of the levels supported by the target contract (`off`, `minimal`,
  `low`, `medium`, or `high`).
* `steering_mode` and `follow_up_mode` are optional `one-at-a-time` (the default) or `all` drain
  policies for their corresponding explicit queues.
* `tools` is an ordered list of tool definitions. Each definition has `name`, `description`, and
  a JSON value `parameters` describing its input shape. `execution_mode` is optional and is
  `parallel` by default; `sequential` makes an entire assistant tool batch sequential, matching
  Pi. Tool names are unique. The schema is data in the fixture; validating it does not require a
  schema package in the harness.

Tool definitions are only capabilities supplied to the agent. They do not grant ambient
filesystem, process, clock, network, or environment access.

`setup.context_hooks` is an optional, closed v1 fixture adapter for the context boundary. It
exists to exercise the Rust context boundary without embedding runner-specific callbacks in a case.
When present it requires:

```json
{
  "host_messages": ["host-only"],
  "transform_append_host_message": "transformed",
  "convert_prefix": "converted:",
  "prepare_next_turn": {
    "host_messages": ["replacement"],
    "model": {"provider": "replacement-provider", "id": "replacement-model"},
    "thinking_level": "high"
  }
}
```

The runners retain these values as explicit host context, append the transform value before each
conversion, and record the resulting request context, model, and thinking level in canonical
`request_trace`. `prepare_next_turn` replaces those three values only for the following request.
This directive is deliberately data-only; arbitrary callback source remains out of the format.

### `actions`

Actions are applied in order. Version 1 defines:

* `{ "kind": "prompt", "text": string }` — add a user message and start inference.
* `{ "kind": "continue" }` — continue from the settled state without adding a user message.

An action consumes one `model_script` turn when inference is started. A fixture must provide
enough turns for its actions. Extra turns are an error, rather than silently ignored input.

The currently checked-in v1 adapters implement a deliberately closed action slice: ordered
`steer`, `follow_up`, `prompt`, and `continue` actions. Deterministic model-stream cancellation
uses the explicit `model_script[*].cancel_after` checkpoint described below; arbitrary timing,
wall-clock cancellation, and ambient provider behavior remain outside this format.

### `model_script`

`model_script` is a deterministic provider-neutral stream script. The Rust adapter translates it
into the provider-stream interface. Each entry is one inference turn;
`chunks` are emitted in order. Version 1 chunk kinds are:

* `text_delta` with a string `text`;
* `tool_call` with a stable fixture-local `id`, `name`, and JSON `arguments`;
* `done` with `stop_reason` (`stop`, `tool_call`, or `length`) and a complete `usage`
  object;
* `error` with `reason` (`error` or `aborted`), stable `message`, and complete `usage`.

Every turn ends in exactly one `done` or `error` chunk. A `tool_call` done turn is followed by
the tool result and, unless the fixture reaches a terminal condition, the next scripted inference turn. Scripted
arguments are data, not executable code.

`cancel_after` is an optional adapter-owned deterministic cancellation checkpoint. v1 supports
`"cancel_after": "text_delta"`: after the first scripted text delta, both adapters truncate the
response, request host cancellation, and settle an `aborted` assistant turn with the stable
diagnostic `Operation aborted`. This is a fixture scheduling directive, not a wall-clock delay or
provider behavior. A later action may start another prompt to verify reuse after cancellation.

### `host`

`host.tools` gives deterministic responses for tools used by the model script. A tool response
has the shape:

```json
{
  "name": "echo",
  "calls": [
    {
      "arguments": { "text": "hello" },
      "result": {
        "is_error": false,
        "content": [{ "type": "text", "text": "hello" }]
      }
    }
  ]
}
```

The first exact `arguments` match is selected. A missing match is a fixture error; a runner may
not invent a tool result. `result.content` uses the same content-part vocabulary as canonical
messages (`text`, `thinking`, `tool_call`, and `json`). A host response may set `is_error: true`
to exercise tool failure while still settling the agent loop. `yield_once: true` is an optional,
runner-owned deterministic scheduling directive: it causes that call to yield one poll/microtask
turn before returning, allowing a fixture to assert completion ordering without a clock.
`updates` is an optional ordered array of text partial results emitted before the final result. It
exists to exercise `tool_execution_update` ordering and is runner data rather than an ambient
progress channel.

`enqueue_during_execution` is an optional closed queue-arrival directive with
`{ "kind": "steer" | "follow_up", "text": string }`. It requires `yield_once: true`, so the
message is enqueued after one deterministic poll/microtask while the tool and its owning run are
still active. `steer` is drained before the immediate tool-continuation request; `follow_up`
waits until that continuation would otherwise leave the run idle. It is a fixture host callback,
not an ambient concurrent user-input source.

`result.terminate` is an optional boolean batch hint. A completed tool batch suppresses its next
model request only when every finalized result has `terminate: true`; omitted is false. The value
is scheduler metadata, not an extra transcript field.

`cancel_after_update: true` is an optional deterministic cancellation checkpoint on a host tool
call. It is valid only when that call supplies at least one `updates` value: both adapters emit the
first update, request cancellation through the active run scope, and preserve the pinned lifecycle
that follows. It is not a timeout and does not use a wall clock.

For the current hook slice, `host.before_tool_call` may be `{ "tool_name": string, "reason":
string, "terminate"?: boolean, "yield_once"?: boolean, "cancel_after_yield"?: boolean }`. It
blocks exactly that named call after schema validation and creates the stated error tool result;
`terminate: true` replaces its batch hint. `yield_once` makes the before-hook await one
deterministic executor turn. `cancel_after_yield: true` requires `yield_once: true`, requests
cancellation from the active run after that await, and then returns `Allow`; the subsequent
tool preparation records `Operation aborted` and the next model request observes the cancelled
scope. `host.after_tool_call` may be `{ "tool_name": string, "content": string,
"is_error": boolean, "terminate"?: boolean }`; it replaces those terminal result fields after
execution. When supplied, `terminate` replaces the finalized batch hint. Other hook behavior is
added only with a dedicated deterministic fixture.

`host.should_stop_after_turn` may be a boolean. When true, it stops the run immediately after
the current `turn_end`, before queue polling or another model request.

`host.observer` may be `{ "hold_agent_end": true }`. The runners register an awaited listener
that pauses exactly at `agent_end`; after observing that the agent is still active, the fixture
releases it and verifies idle settlement. Canonical output gains `observer_settlement` with the
three booleans `agent_end_observed`, `active_before_release`, and `idle_after_release`. This
closed directive tests listener settlement, not the intentionally separate lossy subscription
channel.

### `assertions`

Assertions are intentionally a small projection of the canonical result. They may require
`outcome`, `event_types`, `assistant_text`, `messages`, `usage`, `tool_results`, or `error`.
`event_types` is a convenience projection: a role-bearing event is rendered as
`<type>:<role>` (for example, `message_start:user`), while other events use their `type` alone.
Missing assertion fields mean “do not assert this field”; they do not mean “ignore a required
canonical result field.” For a complete golden result, put the full canonical result in
`expected/<id>.json`.

Canonical output contains `request_trace` only for fixtures using `setup.context_hooks`. Each
entry contains the converted context plus the request model and thinking level, so a later-turn
replacement cannot be masked by matching terminal text alone.

## Fixture classes

Declarative fixtures are provider-free inputs and are safe to execute repeatedly. Expected
fixtures are checked-in canonical outputs for a declarative fixture. External provider captures
and replay artifacts are outside this format and must not be edited to fit it.
