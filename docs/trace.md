# Core trace adapter

The optional `tea-core/trace` feature connects the core's awaited
`EventObserver` boundary to the dependency-free `tea-trace` contract:

```text
AgentEvent reducer → TraceObserver → RedactingSink → caller TraceSink
```

Construct `tea_core::trace::TraceObserver` with a host-owned episode ID
and a `tea-trace::TraceSink`, then register it on the agent builder with
`.observer(Arc::new(observer))`. The adapter emits an
`EpisodeHeader` at `AgentStart`, a compact `Turn` at each `TurnEnd`, a `Tool`
at each settled tool execution, and an `EpisodeEnd` at `AgentEnd`.

The compact V0 trace records the exact serialized arguments carried by each
pre-dispatch `ToolExecutionStart` event in `Tool.input`, plus settled tool
output. It deliberately does not duplicate streaming tool updates; core
observers still receive `ToolExecutionUpdate` events. The start event is
emitted before schema validation, hooks, and capability dispatch, so a host
observer can apply its redaction policy while the arguments are still an
explicit boundary value.

`Tool.input` is not itself redacted. Wrap the sink in
`tea_trace::RedactingSink` (or perform equivalent policy in a host
observer) before persisting or forwarding it. The core never guesses which
argument fields are sensitive and never stores a second hidden copy.

Tracing is best effort. `TraceObserver` wraps the sink in
`tea_trace::IsolatedSink`; `failed_events()` reports dropped records while
the agent run continues with the same state and result. To redact prompts or
tool content, wrap the sink in `tea_trace::RedactingSink` before passing it
to the observer.

The adapter is intentionally synchronous at the observer boundary. Sink work
must remain bounded and must not call back into the agent. It creates no task,
thread, executor, clock, session tree, or persistence policy.

`tea-trace` supplies explicit writer adapters for the two V0 encodings:
`JsonLinesSink<W>` writes one stable, escaped JSON record per line, and
`CborSink<W>` writes a concatenated sequence of definite-length CBOR maps.
Both accept an already-open caller-owned `Write`; neither opens a path or
chooses a destination. The JSON and CBOR record maps carry the same
`schema_version` and `type` fields, so an archive format change is a deliberate
trace-contract change rather than an implicit sink behavior change.

Compaction adds an additive V1 `compaction` record. Existing
`episode_header`, `turn`, `tool`, and `episode_end` records remain V0, so an
archive reader that already understands V0 retains its prior behavior and can
ignore the new type discriminator. A compaction record contains only lifecycle
IDs, stages, strategy metadata, sizes, fingerprints, usage, and classified
outcomes. It deliberately cannot carry a checkpoint, prompt, raw provider
request, tool arguments, or tool result. See [`docs/compaction.md`](compaction.md)
for lifecycle and evidence levels.
