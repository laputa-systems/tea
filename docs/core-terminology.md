# Core terminology

The v1 core is an executor- and provider-agnostic epoch engine. It is not a
session store: `tea_harness::DurableHarness` owns durable operation recovery and
creates one core `Agent` for each durable epoch.

| Domain concept | Rust target | Contract |
| --- | --- | --- |
| Core epoch engine | `tea_core::Agent` | Executes one caller-driven model/tool loop in memory. |
| Canonical core message | `tea_core::state::AgentMessage` | User, assistant, or tool-result transcript value. |
| Assistant tool request | `tea_core::state::AgentToolCall` | Provider-supplied call identity preserved through execution. |
| Final tool result | `tea_core::tool::AgentToolResult` | Correlated completed result with explicit failure and termination fields. |
| Turn update | `tea_core::hooks::AgentLoopTurnUpdate` | Request-scoped change applied before the next provider request. |
| Normal model stop | `tea_core::state::StopReason::Stop` | The ordinary completed assistant-turn outcome. |
| Reasoning choice | `tea_core::state::ThinkingLevel` | One of Off, Minimal, Low, Medium, High, XHigh, or Max. |
| Steering input | `Agent::steer` | Queues input for the next eligible active-turn boundary. |
| Follow-up input | `Agent::follow_up` | Queues input for the next idle boundary. |

The core event envelope is `tea_core::event::AgentEvent`. Its `RunId` and
`EventSequence` make event ownership and order explicit. `AgentSnapshot`,
`RunHandle`, `RunState`, and `RunSnapshot` are inspection and lifecycle values
for the same in-memory epoch engine.

## Durable terms

The durable runtime adds the terms that the core deliberately does not own:

- `Session` is the append-only v1 record and entry history.
- `Operation` is one caller-visible durable prompt or resume request.
- `Epoch` is one core execution interval within an operation.
- `Harness revision` pins the immutable executable configuration for an epoch.
- `Harness snapshot` describes the prompt, plugins, capability bindings, and
  policies behind a revision.
- `Artifact` is immutable content-addressed evidence referenced from the
  session, trace, harness catalog, or evolution store.

See `docs/durable-harness.md` for the runtime boundary and
`docs/session-format-v1.md` for the persisted contract.
