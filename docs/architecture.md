# Runtime architecture

Tea is an in-process extensible agent runtime. The session is authoritative;
model context and terminal presentation are projections of it.

```text
Rust embedding / one-shot CLI / TUI
          |
    direct runtime operations
          |
 SessionSupervisor ---------------- host-owned TaskRuntime
   | accepted inputs, control order,    | polls and joins owned children
   | recovery, pinned configuration     |
   +----------------+-------------------+
                    |
             one Agent FSM
    request -> stream -> tools -> continuation -> settlement
                    |
       EffectGate / semantic commits
                    |
     SessionWriter -> JSONL + immutable artifacts
                    |
        atomic snapshot / bounded typed updates
```

## Execution and ownership

`Agent` owns the fixed execution algorithm: request preparation, provider
streaming, source-ordered tool preparation, permitted parallel execution,
continuation, transactional compaction and settlement. It permits one active
run and owns its cancellation scope. It never chooses an executor or spawns
detached work.

`SessionSupervisor` owns accepted inputs, durable operation identities, one
live drive per lane, immutable harness selection, recovery gates and serialized
session mutation. It constructs the same Agent for roots and children.
A supervisor operation may cross source-pinned epochs at a safe activation
boundary; that does not introduce another tool/provider execution algorithm.

`RuntimeServices` contains explicitly supplied provider, tools, hooks,
compactor and policy identities. The host owns models, credentials, transports,
workspace/process/filesystem/network authority, clocks and execution.
`HostedEpoch` uses the same resolved harness and Agent with the caller's
`EffectGate`; it creates no durable session and rejects session-dependent
policies that have no explicit host implementation. Memory-backed sessions are
useful embeddings/tests, not crash-durable storage.

## Persistence

`SessionWriter::commit` validates a nonempty ordered `SessionCommit` and
publishes it as one transition. Entries, operation records, lane topology and
facts may share that unit. The memory and JSONL implementations use the same
reducer contract. A JSONL commit is one integrity-sealed envelope; incomplete
final bytes require explicit torn-tail repair, while malformed complete records
are corruption. Ambiguous writes poison the writer until reopen.

Provider admission commits an intent and exact post-policy request material
before dispatch. Tool admission commits its source assistant decision and
pinned intent before capability invocation. Completed outcomes commit before
semantic completion is exposed. Immutable artifacts publish before references.
No token or progress tick requires a log append.

`tea-session` owns records, reduction, JSONL, artifact integrity and writer
ownership. It does not execute a provider or extension. Disposable indexes and
HEAD hints are never authority. Read-only inspect/export/verify operations do
not configure providers. See [session format](session-format-v1.md) and
[recovery](harness-recovery.md).

## History and context

The append-only session preserves original messages, results and artifacts.
`runtime::context` derives provider-valid context through committed compaction,
selection and policy projections. Compaction replaces effective context only
after validation and durable acceptance; it does not delete raw history.
The compiler preserves call/result pairing in source order even when parallel
effects complete in another order.

Immutable harness snapshots and exact request material identify effective
instructions, ordered tool declarations, model selection and request-time
policy changes. Prompt-layout ledgers are per-lane disposable observations,
not durable authority or proof of cache billing. Unknown usage/cache/cost
fields remain unknown; supplied monetary values remain exact.

## Extension lifetime

`HarnessResolver` resolves immutable source trees, snapshots, candidates,
revisions, provenance and capability bindings. Authoring stages closed source
within the host ceiling, retains validation/evaluation evidence, and chooses
activation or NoChange. Activation and rollback select immutable lineage at a
safe epoch boundary. They do not undo external filesystem effects.

ABI-v3 extensions provide ordinary tools, prompts, hooks, commands and bounded
continuation policy. Each extension has one private bounded whole JSON state
value with an explicit state version. Rust owns queues, outcomes and authority.
A closed invocation generation cannot write. See [extension ABI](luau-abi-v3.md)
and [immutable evolution](harness-self-extension.md).

## Children and observation

Optional children are bounded root-to-child runs using the same Agent and one
shared session writer. Each has isolated workspace, context, provider/harness
selection, compaction and cancellation ownership. Reports enter parent context
only through explicit retrieval. Workspace application is explicit and
transactional. Reopen never resurrects children; see [subagents](subagents.md).

`TeaEventSubscription` combines one atomic `TeaObservationSnapshot` with
bounded typed updates. Semantic updates and coalescible previews are distinct.
Lag requires a fresh snapshot. Reliable input completion does not depend on
observation, global idleness, or terminal callbacks. Consumers run outside
writer/state locks. Diagnostic trace remains observational.

The zero-dependency `tea-tui` crate owns presentation primitives. Terminal OS
integration stays in `tea-agent`. Stable entry identities and a local frontier
emit settled rows once to native scrollback; the bounded live tail remains
mutable and only modals borrow the alternate screen.

There is no daemon, RPC/control protocol, arbitrary durable-task registry,
workflow language, replicated document store, service hot reload, or hidden
second engine.
