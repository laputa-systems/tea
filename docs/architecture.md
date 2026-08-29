# Core architecture

The implementation is an executor-owned Rust library with an explicit
capability boundary. Rust owns mechanism; callers own transports and
capabilities; the optional Luau policy plane is downstream and cannot alter
the core state machine.

## Crate boundaries

```text
tea-protocol  -> serializable model/message/tool/event/error values
        ^
        |
tea-session   -> durable session format, reducers, and immutable artifact store
        |
tea-core      -> Agent, durable runtime, harness lineage, coding authority, evolution
        |
        +--> tea-trace   (optional immutable event consumer)
        ^                         ^
        |                         |
tea-providers -> concrete provider adapters   tea-luau -> optional policy adapter
                                           tea-http -> generic route-scoped HTTP
        \                         |                         /
         +-------------------- tea-agent --------------------+
                         explicit terminal/application composition root
```

The workspace crates are:

| Crate | Owns | Must not own |
| --- | --- | --- |
| `tea-protocol` | Shared `JsonValue`, canonical JSON encoding, and JSON conversion seams | Runtime IDs/messages/events, scheduler state, provider SDKs, Luau types, filesystem APIs |
| `tea-session` | Versioned session format, reducers, immutable artifact storage, export/reopen verification | Agent/provider execution, provider SDKs, policy VMs, UI state |
| `tea-core` | Agent FSM, durable `runtime::SessionSupervisor`, immutable `harness` lineage, coding host authority, evolution control, context conversion, tool scheduling, hooks/queues, cancellation and settlement | HTTP/provider implementations, cwd/home/config discovery, TUI, Luau VM/runtime, Tokio executor |
| `tea-trace` | Immutable event-to-linear-episode recorder, redaction and caller-selected JSONL/CBOR sinks | Agent state, session tree, replay mutations, sink-driven behavior |
| `tea-providers` | Concrete provider wire adapters and evaluation runner | Core state, durable session writes, UI ownership, direct HTTP clients |
| `tea-luau` (optional policy) | Hermetic VM, capability manifest/modules, policy hooks/tools, script error/limit translation | Core lifecycle/state/scheduling, ambient OS authority, event-loop ownership |
| `tea-http` | Repository-wide pooled HTTP: generic byte streams plus route-scoped JSON, host-only fixed headers, retries, rate/cooldown policy, cancellation, and ordered concurrent batches | Provider request/response models, search policy, arbitrary network authority, an async runtime |
| `tea-agent` | Application composition of provider adapter, Luau extension engine, core runtime, and terminal host | Reusable core/domain contracts |

`tea-core` owns the capability-scoped coding host and never imports Luau. The
checked-in Pi capture remains historical evidence only. `tea-luau` owns the
first-party coding-tool declarations, while application hosts select explicit
workspace/process capability implementations and grants.

`tea-http` is the repository-wide HTTP boundary. It depends on the
asynchronous pooled `h12tiny-client` transport and the narrow core
cancellation/capability contracts. `tea-agent` creates its route-scoped JSON
client for the bundled Luau web policy; its host binding fixes Firecrawl and
optional TinyFish routes and keeps the TinyFish API key in a redacted fixed
header. Provider adapters share its generic byte-stream client through a
caller-owned executor. `tea-http` has no Firecrawl, TinyFish, or provider
models.

## Ports and adapters

```text
             caller-owned Smol executor
                       |
                       v
  +------------------------------------------------+
  | Agent                                         |
  |  durable state + Run ownership + event reduce |
  +--------------------+---------------------------+
                       |
                 Agent loop FSM
              /        |          \
             /         |           \
    ModelStream      ToolScheduler   Hook/queue ports
        |                 |                 |
  caller provider   AgentTool + policy   caller callbacks
        |                 |                 |
  no HTTP in core    explicit capabilities  no hidden mailbox
```

The core consumes a `ModelProvider` port with a request containing only model descriptor, system
prompt, converted messages, ordered tool definitions, thinking level, and child cancellation. Its
`stream` future resolves once a response source exists; the returned `ModelEventStream` is then
polled one event at a time. The reducer applies each delta before requesting the next one, so a
partial assistant message is observable while the provider source remains open. A provider adapter
may use HTTP, a native model, a world runtime, or a deterministic fixture; none of those mechanisms
appears in core state.

Tools expose name, description, raw JSON Schema, execution mode, and an async execute operation.
Preparation/validation and scheduling are generic. The core's Miniserde-native validator accepts
the structural and combinator keywords used by the profile (`type`, `properties`, `required`,
`additionalProperties`, `items`, `enum`, `const`, `allOf`, `anyOf`, `oneOf`, `not`, size bounds,
and numeric bounds); unsupported draft-specific keywords are rejected as invalid tool schemas
rather than ignored. A tool receives a call ID, validated JSON, cancellation, and an update sink.
First-party coding tools are resolved from immutable Luau harness source;
`CodingHost` supplies their trusted operation ports without owning their
provider-visible semantics.

The optional `tea_providers` crate is a separate adapter layer behind explicit
Cargo features. `provider-openrouter`, `provider-local`, and `provider-opencode-zen`
are opt-in blocking HTTP/1.1 transports backed by Rustls/Graviola, with caller-supplied
keys and no ambient configuration discovery; the provider-owned worker thread keeps that
blocking I/O outside the core executor. The evaluation runner selects one only through its
explicit provider argument. They do not change the default build or the `ModelProvider` contract. See
[provider adapters](provider-adapters.md) for their wire and context boundaries.

## Durable harness composition

`tea_core::harness` owns immutable source trees, snapshots, revisions, candidates,
profiles, capability bindings, and the `HarnessResolver`. Resolving a revision produces
a provider-independent `ResolvedHarness`: prompt, extension tools, hooks, and pinned
policy values. `HarnessSeedBuilder` gives composition roots one explicit, provider-free
way to create the initial source tree, snapshot, revision, and model-harness profile;
it performs no discovery and creates no session.

`tea_core::runtime::SessionSupervisor` owns session writes, recovery, activation,
effects, artifacts, and event publication. It combines a `ResolvedHarness` with
host-owned `RuntimeServices` (provider transport, trusted base tools, model
selection, and compactor) only to construct an epoch `Agent`.

An embedding that already owns outer durability can instead call
`RuntimeServices::prepare_hosted_epoch`. `HostedEpoch` uses the same agent-construction
path and immutable harness fingerprints, but creates no Tea session, file, provider,
capability binding, artifact tool, harness-authoring tool, task, or executor. The caller
supplies its effect gate, external provenance, and any explicit epoch-local tools. Tea
populates or validates snapshot, revision, profile, and provider-surface provenance.
The stateless hosted path rejects resolved context or lifecycle policies because it has
no durable port on which to execute them.

The `tea-agent` crate is the explicit composition root: it selects `tea-providers`
and `tea-luau::LuauExtensionEngine` and passes both through the narrow core ports.
Consequently `tea-core` has no dependency on either concrete provider code or Luau.

## Ownership and state transitions

`SessionSupervisor` owns one durable session and a map of lane runtimes. The
main lane is selected only by root-facing entry points; the lower operation
machinery receives a lane explicitly. Each lane owns its provider services,
thinking level, active agent and operation claim, compactor, semantic branch,
and prompt-layout ledger. The single session writer serializes mutations from
all concurrently driven lanes into one global sequence.

```text
                         one SessionSupervisor
                                  |
                  +---------------+---------------+
                  |               |               |
              main lane       child lane A    child lane B
              Agent/Run        Agent/Run        Agent/Run
                  |               |               |
                  +---------------+---------------+
                                  |
                       serialized SessionWriter
```

Optional subagents are supplied through explicit provider-neutral host and task
ports. `SubagentHost` prepares and finalizes isolated workspaces and returns
lane-specific `RuntimeServices`; `TaskRuntime` owns every asynchronous child
handle through cancellation and join. `tea-core` neither invokes Git nor owns
an executor. The terminal alone may turn its resolved `config.toml` into a
`SubagentServices` value; library callers receive no ambient configuration
semantics and must construct that value themselves. With `subagents: None`, no
coordinator, child provider factory, or collaboration tools exist. See
[durable subagents](subagents.md).

One owned `Agent` has exactly zero or one active `Run`. The application owns the `Agent` and drives
its futures; the core does not create an executor, spawn detached work, or maintain a background
thread pool. A run owns a child cancellation scope and all in-flight provider/tool/hook work.

```text
Idle
  | prompt/continue (reserve run, clear transient state)
  v
Active: Starting -> Streaming -> PreparingTools -> ExecutingTools
                  ^                         |              |
                  |                         +--------------+
                  |                                turn end
                  +------ queue/next-turn -----------+
                                   |
                              AgentEnd emitted
                                   v
                     Awaiting terminal observers
                                   |
                                   v
                                  Idle
```

Cancellation or failure can occur in every active state. All terminal paths pass through one
settlement routine that clears `is_streaming`, partial message, pending tool IDs and active-run
ownership before allowing reuse. `agent_end` is emitted once as the final event; awaited terminal
observers may delay the transition to idle. A Rust `Run` drop policy must be chosen and fixture
tested before exposing the final ergonomic API.

## Event reduction

The event reducer updates the state snapshot before invoking observers:

| Event | State reduction |
| --- | --- |
| `message_start` | Set current streaming/message snapshot |
| `message_update` | Replace current partial assistant snapshot |
| `message_end` | Clear current snapshot and append message to transcript |
| `tool_execution_start` | Add call ID to pending set |
| `tool_execution_update` | Observer/trace data only; pending set unchanged |
| `tool_execution_end` | Remove call ID from pending set |
| `model_turn_usage` | Retain one provider-reported model-turn record and update aggregate accounting |
| `compaction_result` | The validated replacement was already atomically committed before observers see it |
| `automatic_compaction_*` | Run-local policy transaction; canonical history changes only on validated success |
| `context_estimate`, `tool_failure_observed`, `provider_request_skipped` | Structured observability; no message content is added to metrics |
| `turn_end` | Record assistant error text when present |
| `agent_end` | Clear streaming snapshot; settlement still awaits terminal observers |

The reducer never invokes a tool, provider, filesystem operation, or policy decision. It is the
single place where runtime-owned state is made observable, so event/state fixtures can compare both
the event stream and snapshots.

## Model request and message boundary

```text
AgentMessage[] (host transcript)
       -> transform_context (optional host-message operation)
       -> convert_to_llm (explicit filtering/conversion)
       -> ModelRequest
       -> AssistantStream events
       -> AssistantMessage + AgentEvent reduction
```

The persisted host envelope is versioned if applications need custom messages. The core does not
invent UI concepts or provider-specific fields. `convert_to_llm` is called only at the model
boundary; a transform failure, conversion failure, provider protocol violation, or provider
transport failure has a typed terminal path and cannot bypass cleanup.

An application-owned session store may pass a validated linear message vector back through
`Agent::restore_messages` while the agent is idle. That API clears transient execution state,
queues, and retained provider accounting; the core still performs no file, home-directory, or
session-format discovery.

## Tool scheduling and ordering

For one assistant message:

```text
source calls A, B, C
      |
      +--> prepare/validate A -> B -> C (always source order)
      |
      +--> execute allowed calls concurrently (parallel mode)
      |       completion: C -> A -> B
      |
      +--> tool_execution_end: C -> A -> B
      |
      +--> tool-result messages/context: A -> B -> C
```

Sequential mode performs the entire prepare/execute/finalize/result cycle in source order. The
selected contract serializes the entire batch when any call has a sequential
override; the mixed-mode fixture in `docs/semantics.md` decides whether v1 preserves that exact
rule. Partial updates are awaited before a tool end event and ignored after settlement. Tool
results are inserted in source order even when completion events are not.

## Hooks and policy boundary

Rust hooks are explicit ports:

```text
before_tool_call -> allow | block(reason, terminate?)
tool execute     -> result/update/error
after_tool_call  -> field replacements (no deep merge)
turn_end         -> prepare_next_turn and/or should_stop_after_turn
```

Hook callbacks receive the active cancellation scope and typed contexts. They cannot mutate core
state directly, bypass event reduction, or reorder tool-result insertion. Hook errors are typed and
follow the fixture-defined abort/block/structured-result rule.

Luau adapters may call those ports, register ordinary Rust tools, request stop, or annotate a
trace. They cannot own the loop, mutate transcript storage, schedule tools, define queue semantics,
hold resource ownership, or emit post-settlement events.

## Cancellation and resource ownership

The run's child cancellation scope is passed to provider stream polling, tool preparation/execution,
updates, hooks, and queue wait points. `CancellationToken::cancelled()` is an executor-neutral
future, so an adapter races it with its own I/O rather than polling an atomic or importing a
runtime-specific token. Cancellation must settle pending futures and observers, prevent
post-terminal events, clear pending call IDs, and leave the agent reusable. No operation is detached
from the run. The application may choose how to run parallel futures on Smol, but the core does not
spawn or own an executor.

The core has no unsafe Rust and no Tokio type in public or private APIs. Dependency review must
keep cancellation executor-agnostic and isolate the chosen token implementation behind the core
contract.

Concrete native adapters own their request boundary to settlement. OpenRouter and
other adapter implementations expose caller-polled body streams and also perform
checked cancellation before and between body chunks; finite adapters additionally
settle after request completion. The generic core additionally wakes its sequential
tool poll on cancellation and records a cancellation result rather than leaving an
uncooperative future holding run ownership.

## Default coding builtins

The default coding surface is four revisioned Luau builtins over trusted host
capabilities:

```text
read / bash / edit / find source trees + singleton host grants
                              |
                              v
          immutable harness revision + executable tool registry
```

The host never discovers cwd, `$HOME`, `.pi`, settings, skills, or sessions.
Luau owns prompt bytes, schemas, descriptions, and ordinary behavior; Rust
independently enforces workspace confinement, process authority, and
transaction settlement. The terminal's documented optional TinyFish fallback
reads only `TINYFISH_API_KEY` from its already-provisioned process environment,
then retains it only as a redacted route header. See `docs/default-coding-profile.md`.

## Tracing boundary

`tea-trace` consumes immutable typed events after the core reducer. It records a linear
episode, not a Pi session tree. Redaction is selected by the caller for prompts/tool content;
trace sink failure is reported separately and cannot change the agent result. No trace, JSONL and
CBOR runs must have identical core behavior.

## Core and optional policy boundary

```text
Core: protocol <- core <- caller provider/tools/hooks/profile
                    |
                    +-> optional trace

Policy: protocol <- core <- luau adapter -> mlua/Luau VM
                                    |
                                    +-> host capability manifest/world/task/trace ports
```

`tea-core` must compile and operate without `mlua`, Luau, world APIs, scripting types, Node,
TypeScript, `napi-rs`, `pi-ai`, or a provider implementation. `tea-luau` may depend on core;
core must not depend on it. Luau module resolution is host-controlled and closed, with no ambient
filesystem, process, environment, network, home, cwd, clock, FFI, package registry, native plugin,
or OS-command authority.

## Contract evidence

These contract-bearing choices are settled by fixtures and dependency review:

| Decision | Required evidence |
| --- | --- |
| Stable run/turn/message ID representation and normalization | `tests::generated_run_message_and_event_ids_are_monotonic_after_cancellation` |
| Awaited observer versus bounded/lossless live subscriptions and overflow/drop behavior | `tests::runtime_subscription_is_reentrant_and_drop_unsubscribes_for_future_events`, `tests::nonblocking_subscription_is_ordered_lossy_and_never_delays_settlement`, `tests::lossless_subscription_is_ordered_without_capacity_drops`, `tests::lossless_subscription_retains_all_events_under_volume`, `tests::dropping_lossless_subscription_unsubscribes_cleanly` |
| Drop unfinished run policy | `tests::agent_allows_one_run_and_drop_settles_cancellation` |
| Manual compaction transaction, replacement validation, and cancellation | `tests/compaction.rs` |
| Cancellation token implementation without Tokio | dependency review + `cancel/checkpoints` |
| Mixed per-tool sequential override behavior | `crates/tea-core/fixtures/declarative/mixed-tool-execution.json` |
| Default coding-tool declarations and grants | `crates/tea-luau/src/builtins.rs` coding-bundle test |
| Trusted coding authority and transaction behavior | `crates/tea-core/tests/coding_capabilities.rs` |
| Typed error hierarchy and failure-to-event mapping | `failure/provider-error`, `cancel/failure-shapes` |

No change may introduce an undocumented fallback. A newly unresolved behavior
must be marked investigating in a deterministic fixture and resolved before
it becomes part of the supported contract.
