# Runtime semantics

This is the contract pinned by deterministic in-process fixtures. A fixture is
required for every edge where a provider, observer, or callback can affect
externally visible settlement.

## Vocabulary and identity

An **agent** owns durable transcript/configuration state and can have at most one active **run**.
A run is one `prompt` or `continue` invocation together with all turns, tool work, queue drains,
observers, and terminal settlement caused by that invocation. A **turn** starts at
`turn_start`, contains one assistant response and its tool work/results, and ends at `turn_end`.
A **message** is a transcript item; a **tool call** is an assistant content block and has the
provider-supplied `toolCallId`.

Public events do not carry run, turn, or message IDs. V1 therefore makes the
Rust representation explicit: `RunId` is a process-local monotonic counter; `TurnId` begins at
one within a run; `MessageId` is a durable agent-local monotonic counter; and `EventSequence`
begins at one within a run. A cancelled prompt keeps any already-retained message IDs, and later
runs never reuse them. The fixture normalizer only handles these generated IDs.
`toolCallId` is provider data and is never normalized. The invariant is covered by
`tests::generated_run_message_and_event_ids_are_monotonic_after_cancellation`.

## Agent state and snapshots

The durable state is:

```text
system_prompt: string
model: ModelDescriptor
thinking_level: off | minimal | low | medium | high | xhigh | max
tools: ordered list of AgentTool definitions
messages: ordered list of AgentMessage values
```

The runtime-owned snapshot is:

```text
is_streaming: bool
streaming_message: optional partial/final assistant or current message snapshot
pending_tool_calls: set of toolCallId values
error_message: optional string from the most recent failed/aborted assistant turn
active_run: optional RunSnapshot
accounting: per-turn provider-reported usage plus aggregate token/cost fields
```

Model accounting is retained only when the provider reports a usage update. Each record carries
the `run_id`, `turn_id`, and requested `ModelDescriptor`. Input, output, reasoning, cache-read,
and cache-write counts remain independently optional, so an explicit zero is distinct from an
unknown value. Provider-reported cost remains exact decimal text; the core never estimates a
price from token counts. `model_turn_usage` delivers the settled record to observers before the
turn's `turn_end` event.

State inspection returns a snapshot. It never exposes a mutable borrow into the loop. Assigning a
new message/tool list copies the top-level list; message/content values remain explicit owned
protocol data.

## Manual compaction transaction

`Agent::start_compaction` is an idle-only ownership operation, like starting a
normal run but with the separate `compaction_start`, `compaction_result`, and
`compaction_end` event grammar. A caller-supplied `Compactor` receives an
owned `CompactionContext` (version, prompt, model, retained messages, and host
messages) plus the operation cancellation token. It proposes a replacement;
it never receives mutable agent state.

Core validates unique nonzero message IDs and a one-to-one preceding assistant
tool-call/name relationship for every retained tool result before atomically
replacing history. A
failure, invalid replacement, cancellation, active-agent request, or observer
failure leaves the old history intact. The handle then settles to idle and the
agent can run another prompt. Core does not derive a summary, choose a model,
or aggregate the optional compactor usage report into normal model-turn
accounting.

## Opt-in automatic compaction

`AutomaticCompactionPolicy` extends the same caller-supplied `Compactor` port;
it never selects a summary model, prompt, executor, or provider. A host supplies
an explicit `ContextBudgetSource`, reserved compaction tokens, recent-tail
budget, overflow policy, and per-run compaction/retry limits. An enabled policy
without a configured compactor stops at a typed `AutomaticCompactionUnavailable`
boundary instead of inventing a fallback.

After a completed assistant/tool turn and before the next provider request, the
core estimates context from the last nonzero, non-error provider input usage plus
only messages appended after that request. If no valid checkpoint exists it uses
a deterministic canonical-message byte estimate. Zero usage and error responses
do not reset the checkpoint. Crossing the threshold runs a cancellation-aware,
transactional compactor operation. `AutomaticCompactionRequest` gives the
compactor an exact safe retained suffix, summary prefix, and split-turn prefix
when a retained suffix begins mid-turn, plus the requested recent-token
budget, reason, and retry intent; compactors may override
`Compactor::compact_automatic` to use this split while compactors that do not
override it use their `compact` behavior.

```text
completed assistant/tool turn
  -> estimate next context
  -> threshold: compact -> validate pairs -> atomically commit -> request provider
typed overflow on incomplete response
  -> restore pre-request transcript -> compact once -> retry the same turn
cancel/failure/unavailable/limit
  -> emit outcome, leave pre-transaction history unchanged, settle
```

Only `ModelStreamEvent::ContextOverflow` authorizes overflow recovery; the core
does not inspect provider error text. A successful response is never retried.
Each incomplete continuation is retried at most once; the configured per-run
retry limit bounds recovery across distinct later continuations.
If a retained context still exceeds the threshold, the core emits
`StillAboveThreshold` and blocks an immediate identical compaction loop; normal
per-run limits remain the final guard. Automatic lifecycle events include the
reason, before/after estimates where known, count, retry intent, and bounded
failure detail.

The terminal invariant is true after every successful, failed, or cancelled run:

```text
is_streaming == false
streaming_message is absent
pending_tool_calls is empty
active_run is absent
```

The transcript and `error_message` follow the normalized terminal event result. A later prompt may
reuse the same agent without reset; `reset` is only legal while idle and clears transcript, queues,
runtime state, and error.

## Active-run contract

The active-run contract is intentionally explicit because it is the boundary most likely to be
blurred by an async Rust API.

| Operation/state | Required behavior | Fixture assertion |
| --- | --- | --- |
| Start | Reserve the run before emitting `agent_start`; set streaming true and clear streaming/error transient state | Observer sees active run during first event |
| Direct `prompt` while active | Reject with a typed busy error; do not append input or emit events | Transcript/event stream unchanged |
| Direct `continue` while active | Reject with typed busy error | Same |
| `steer` while active or idle | Append to steering queue; never start a run implicitly | Message is injected at the documented drain point of an explicit run |
| `follow_up` while active or idle | Append to follow-up queue; never start a run implicitly | Message waits until the run would otherwise stop |
| `abort` idle | No-op | No new run/events |
| `abort` active | Idempotently signal the child cancellation scope | One terminal outcome and no duplicate settlement |
| `wait_for_idle` idle | Resolve immediately | No active run |
| `wait_for_idle` active | Resolve only after terminal event observers settle | Delayed `agent_end` observer holds logical busy state |
| Drop unfinished run | `RunHandle::drop` requests cancel-and-settle; an un-driven handle settles immediately, while a driven handle settles at its cancellation-aware boundary | No orphaned active ownership |
| Finish | Clear transient state before making the agent idle; resolve run exactly once | Next prompt can run normally |

The agent rejects direct `prompt`/`continue` while `activeRun` exists, exposes
`abort`, and awaits `subscribe` listeners in registration order. Rust exposes that awaited path
as `Agent::subscribe`, whose RAII `ObserverSubscription` can safely be dropped from a callback:
changes apply to the next event. An observer error returns a typed run error but still produces one
terminal `agent_end` and releases active ownership. Rust also offers the explicitly distinct
`Agent::subscribe_nonblocking`: it uses a caller-selected bounded queue, `try_send`, and a dropped
event counter, so it never participates in settlement or creates a task. The corresponding core
tests pin error, reentrancy, unsubscribe, overflow, and drop behavior.

### Exact active-run fixture template

```json
{
  "scenario": "active-run/<case>",
  "initial_state": {"messages": [], "tools": []},
  "actions": [
    {"at": "before_first_event", "call": "prompt", "input": "hello"},
    {"at": "during_model_stream", "call": "prompt", "input": "illegal-direct-prompt"},
    {"at": "during_model_stream", "call": "continue"},
    {"at": "during_model_stream", "call": "steer", "message": "steer-me"},
    {"at": "during_model_stream", "call": "follow_up", "message": "follow-me"},
    {"at": "after_agent_end_before_observer_settlement", "call": "wait_for_idle"},
    {"at": "observer_settled", "call": "prompt", "input": "reuse"}
  ],
  "expected": {
    "busy_errors": [{"operation": "prompt", "kind": "<typed-kind>"}, {"operation": "continue", "kind": "<typed-kind>"}],
    "event_order": ["<fill from event grammar>"],
    "queue_drain_order": ["<fill>"],
    "idle_observed_before_reuse": true,
    "terminal_state": {"is_streaming": false, "streaming_message": null, "pending_tool_calls": []}
  }
}
```

## Event contract

Events are emitted in this order and awaited by the active run's event observer:

```text
agent_start
turn_start
message_start*
message_update*
message_end
tool_execution_start*
tool_execution_update*
tool_execution_end*
message_start*
message_end*
turn_end
... (more turns)
agent_end
```

The stars are constrained, not arbitrary: prompt messages have start/end with no assistant update;
assistant streaming has one start, zero or more updates, and one end; tool-result messages have
start/end and no assistant update; every tool start has at most one matching end. A run has exactly
one `agent_start` and exactly one terminal `agent_end`, including normal, error, and cancellation
outcomes. `agent_end` is the final emitted event, although its awaited observers may still keep the
run logically busy.

### Plain generation grammar

For a prompt run where the provider emits an assistant stream and no tools:

```text
agent_start
turn_start
message_start(user prompt)
message_end(user prompt)
message_start(assistant partial)       # omit if provider has no start event
message_update*                        # assistant stream events only
message_end(assistant final)
turn_end(assistant, [])
agent_end(all new messages)
```

If the provider returns a final assistant message without a `start` event, the loop emits
`message_start(final)` immediately before `message_end(final)`. This distinction must
remain visible in fixture results.

### Tool grammar and ordering

For each assistant message containing tool calls, preparation starts in assistant/source order.
Each call emits `tool_execution_start` before validation/preparation finishes. The event includes the
exact serialized JSON arguments supplied by the model, allowing an observer or trace redactor to
inspect and sanitize them before capability dispatch. Unknown tools, invalid arguments, blocked
calls, and aborted preparation produce immediate error results and still emit
`tool_execution_end` and a tool-result message.

Sequential mode prepares, executes, finalizes, and inserts each result before the next call. In
parallel mode preparation remains source ordered; allowed executions overlap; each
`tool_execution_end` is emitted in actual finalization/completion order, while tool-result message
events and context insertion are assistant/source ordered. The mixed-batch
fixture pins the selected sequentialization rule.

Updates are emitted during execution. Updates queued before the tool promise settles are awaited
before its end event; callbacks after settlement are ignored. An end event contains the finalized
result and error flag. `afterToolCall` replacement is field-by-field (`content`, `details`, `usage`,
`isError`, `failure`, `terminate`) with no deep merge. `terminate` is a boolean replacement and only `true`
contributes to the all-calls termination rule.

### Tool failure circuit breaker and model projection

`ToolFailure` is explicit host metadata, not a core string heuristic. It carries
one of `Cancelled`, `InvalidArguments`, `Recoverable`, `Retryable`, or `Fatal`, an optional
stable `FailureSignature`, and optional recovery guidance. Tools can attach it
directly to `AgentToolResult`, return `ToolError::Classified`, or a host
`after_tool_call` hook can replace it. Invalid arguments and ordinary execution
failures are recoverable by default. A run-local `ToolFailureCircuitBreaker`
counts only consecutive identical retryable signatures; success, an ordinary
failure, or a different signature resets that streak. Fatal results always end
the run after their result is recorded.

In a sequential batch a terminal result prevents later capability execution but
the core records deterministic skipped error results for later assistant calls,
so the canonical transcript remains compactable. Parallel siblings already in
flight settle normally; any fatal/tripped result suppresses the next provider
request. `ToolFailureObserved` and `ProviderRequestSkipped` make the
disposition, signature, count, and terminal decision observable without copying
unbounded error stacks into metrics.

Canonical tool-result `content`, `details`, usage, `terminate`, `is_error`, and
failure metadata are retained unchanged in `AgentMessage::ToolResult` and
`ToolExecutionEnd`. Before a provider request, the core clones and curates this
context using `ToolResultProjectionPolicy`: it deterministically retains
prefix/suffix around `… [truncated] …`, marks error status/disposition/guidance,
encodes unsupported structured details as bounded marked text, and suppresses
later identical error payloads in the same projected context. This projection
never mutates transcript/audit state. Command Code preserves `isError`; the
OpenAI-compatible context carries the bounded marked representation.

### Event observer and subscription contract

The target distinguishes an awaited `EventObserver` from a non-blocking observational subscription.
The public `subscribe` path behaves as an awaited listener: listeners run in
registration order, receive the run signal, and `agent_end` listener settlement precedes idle.
V1 pins the Rust adaptation rather than silently conflating these meanings.

```text
event emitted/reduced into state
        +-> lossless live subscription (unbounded, before observers)
        +-> awaited observer(s), registration order
              -> bounded non-blocking subscription (try_send)
              -> terminal observer settles
              -> run resolves and active state clears
```

Fixture cases:

```text
observer-before-first-event
observer-registration-order
observer-sees-reduced-state
agent-end-observer-delays-idle
observer-cancelled-with-run
observer-error
observer-unsubscribe-during-callback
slow/dropped observational subscriber capacity and overflow
```

`Agent::subscribe_nonblocking` resolves the last two cases: a full queue drops the new event and
increments `dropped_events`; a dropped receiver unregisters itself. Neither case can hold a run
open indefinitely. `Agent::subscribe_lossless` is the separate live path for hosts that require
every event: it uses an unbounded standard-library receiver, publishes in event sequence order
without waiting for a receiver or host future, and has no capacity-based drop path. Unread events
remain caller-owned memory until drained or the `LosslessEventSubscription` is dropped; dropping
it unregisters the receiver and releases its queued events.

## Streaming and context boundary

Before every model request, apply `transform_context` (if present) to host `AgentMessage` values,
then `convert_to_llm`. Build the request from the current system prompt, model, thinking level,
converted messages, and ordered tool definitions. A transform can prune/inject host messages;
conversion can filter them or map them to user/assistant/tool-result messages. No UI/session
message type is invented by the core.

The provider stream is a caller-supplied abstraction. `ModelProvider::stream` resolves to a
`ModelEventStream`, then the core awaits exactly one `next_event` call at a time. It reduces each
event before polling again: a `TextDelta` therefore updates `partial_response`, the transcript, and
the observable `message_update` event while the source is still open. `ModelStream` is only the
finite replay/test adapter; production adapters implement `ModelEventStream` directly. An explicit
`ModelStreamEvent::Error` is retained as a finalized assistant message with `StopReason::Error` and
`error_message`; it is distinct from a rejected provider future (`CoreError::ModelProvider`). A
`StopReason::Length` response is a normal terminal turn when it has no tool calls. If it contains
tool calls, every call is refused with an error tool result (the arguments may be truncated), then
the loop may continue with the next model turn. The core owns the partial-message snapshot and
updates transcript state only through event reduction. Provider transport details and `pi-ai` types
do not cross the Rust core boundary.

## Queues and turn transitions

The two explicit queues have independent modes: `all` drains every item at a drain point;
`one-at-a-time` drains only the oldest item. They are not a general mailbox.

| Drain point | Queue | Behavior |
| --- | --- | --- |
| Before first model request (prompt run) | steering | Inject messages unless the caller explicitly consumed the continue/steering special case |
| After assistant tool batch and `turn_end` | steering | Inject before another model request; tool calls from the just-finished message are not skipped |
| When no tool calls and no steering remain | follow-up | Inject and continue another turn |
| `should_stop_after_turn == true` | neither | Emit `agent_end` first; do not poll queues |
| Cancellation | neither | Stop draining; preserve or clear queued messages only as the pinned fixture says |

`continue` requires a non-empty transcript whose last message is not assistant. If the last message
is assistant, queued steering/follow-up handling is pinned separately; otherwise it is an error.
Queue mode, mixed queue ordering, messages arriving during tool work, and cancellation with queued
messages each require a declarative fixture.

## Cancellation and failure

Cancellation is a child scope owned by the run and passed to model streaming, tool preparation,
tool execution, hooks, and queue waits. `CancellationToken::cancelled()` is the executor-neutral
wakeable future that an adapter races with I/O or a host capability future. Required deterministic
checkpoints are:

```text
before first model token
between streamed chunks
before tool preparation
while one tool runs
while parallel tools run
after tools, before next model request
while before/after/next-turn hook is pending
while queue wait is pending
```

Abort is idempotent. There are no events after terminal settlement, no orphaned model/tool/hook
work, no pending tool IDs, and the same agent accepts a subsequent prompt. The terminal outcome
must distinguish transport failure, model error, model abort, tool failure, hook failure, protocol
violation, schema failure, caller cancellation, and internal invariant failure. Expected failures
are typed results, never Rust panics or an unclassified `anyhow::Error`.

The loop synthesizes an assistant failure message and emits
`message_start`, `message_end`, `turn_end`, `agent_end` when the loop itself throws; provider
streams can instead return an assistant `stopReason` of `error` or `aborted`. Fixtures must retain
that distinction and pin error-message placement.

## Exact event fixture template

```json
{
  "scenario": "events/<plain|tool|parallel|cancel|failure>",
  "provider_script": [{"request": "<predicate>", "events": ["<stream events>"]}],
  "tool_script": [{"name": "<tool>", "prepare": "<result>", "delay": "<clock step>", "updates": ["<partial>"]}],
  "observer_script": [{"event": "<type>", "action": "record|await|abort|enqueue|fail"}],
  "external_actions": [{"at": "<checkpoint>", "action": "abort|steer|follow_up"}],
  "expected": {
    "events": ["<canonical event objects in exact order>"],
    "provider_requests": ["<normalized request objects in order>"],
    "tool_invocations": ["<preparation and execution observations>"],
    "messages": ["<source-order context messages>"],
    "terminal_outcome": "success|transport_error|model_error|aborted|tool_error|hook_error|protocol_error|schema_error|cancelled|invariant_error",
    "state": {"is_streaming": false, "pending_tool_calls": [], "streaming_message": null},
    "normalization": ["timestamps only", "generated IDs only", "durations only"]
  }
}
```

No fixture may embed runner-specific callbacks or arbitrary Rust code. The
scenario language is declarative so the Rust runner executes the same schedule
defined by the contract.
