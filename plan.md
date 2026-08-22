# Tea Durable Self-Improving Harness

You are implementing a durable, recoverable, self-extensible, empirically self-improving agent harness in:

`https://github.com/laputa-systems/tea`

Work directly in the repository. Inspect the current implementation before changing it. Do not merely produce a design document or partial scaffold: implement the design incrementally, add executable fixtures before each new behavior, run the complete verification suite after every phase, and leave the repository in a finished, internally consistent state.

Assume the previously designed compaction work is already implemented. Preserve its transactional proposal/validation/commit model, strategy identities, metrics, regression gates, and provider-agnostic design. Integrate with it rather than replacing it.

Do not add:

* MCP support.
* Tokio.
* A local inference backend.
* A general package manager.
* An ambient-authority plugin system.
* A file-watcher-driven reload architecture.
* Arbitrary Rust self-modification.
* An ORM or heavyweight persistence framework.
* A second independent agent scheduler beside the existing Tea state machine.
* Fake or placeholder APIs that return plausible empty values.
* “Best effort” recovery that silently discards state while claiming durability.

The final system must support long-running Tea sessions that can survive process termination at every defined effect boundary, retain recoverable raw execution evidence outside model context, evolve a bounded Luau harness safely, activate session-local harness changes automatically, and evaluate proposed harness improvements under reproducible regression gates.

---

# 1. First ground the implementation in the current repository

Before editing, inspect only the current Tea code and documentation needed to
locate the existing ownership boundaries for:

- core runs, tools, hooks, cancellation, and settlement;
- session persistence and restoration;
- traces and events;
- Luau extension loading and host capability bindings;
- compaction and cache metrics;
- fixture and live evaluation infrastructure.

Verify the current APIs rather than assuming file paths or types in this prompt
are exact. Reuse existing mechanisms wherever they already satisfy the required
contract. Do not conduct additional external design research, produce a broad
repository survey, or reopen the architecture established below.

Write a brief implementation map containing only:

1. existing components that will be reused;
2. components that must be extended;
3. new crates or modules that are actually required;
4. any direct conflict between the specification and the current repository.

Then begin implementation.

---

# 2. Target architecture

Implement this hierarchy:

```text
┌───────────────────────────────────────────────────────────────┐
│ Frozen application and experiment control plane              │
│                                                               │
│ evaluator · task manifests · budgets · capability envelopes   │
│ promotion rules · active global profile · rollback            │
└──────────────────────────────┬────────────────────────────────┘
                               │
┌──────────────────────────────▼────────────────────────────────┐
│ Durable Tea harness supervisor                               │
│                                                               │
│ session tree · lane reducer · operation WAL · recovery        │
│ harness snapshots · artifact store · context projection       │
│ core-run rollover · events · usage ledger                     │
└──────────────────────────────┬────────────────────────────────┘
                               │
┌──────────────────────────────▼────────────────────────────────┐
│ tea-core execution mechanism                                 │
│                                                               │
│ model steps · tools · hooks · cancellation · settlement       │
│ transactional compaction · no filesystem/session ownership    │
└──────────────────────────────┬────────────────────────────────┘
                               │
┌──────────────────────────────▼────────────────────────────────┐
│ Hermetic Luau policy plane                                   │
│                                                               │
│ prompt sections · bounded hooks · projections · pure tools    │
│ no ambient authority · no WAL/storage/promotion ownership     │
└───────────────────────────────────────────────────────────────┘
```

The primary rule is:

> **Luau may propose policy; Rust owns mechanism, durable state, authority, validation, activation, and promotion.**

Do not turn Luau into another runtime scheduler. Do not let plugins write session records, advance lane pointers, grant capabilities, switch the active harness pointer, modify evaluator state, or rewrite Rust source.

---

# 3. Non-negotiable invariants

Encode these as tests and documentation.

## 3.1 Durable operation invariants

1. An accepted operation is durable before its caller is told it was accepted.
2. Every external effect has a durable intent record before the effect begins.
3. Every completed effect has a durable result or settlement record before downstream execution observes it.
4. A crash may leave an unfulfilled intent, but never an unexplainable side effect in the durable model.
5. Recovery must classify every unfulfilled intent as:

   * complete,
   * safely retry,
   * synthesize an interrupted result,
   * or fault as corruption.
6. No operation has more than one terminal outcome.
7. At most one operation is open on a lane.
8. A storage failure faults the harness before any further effect starts.
9. Exactly-once external effects are not promised. Replay safety is explicit.
10. Recovery is idempotent. Running recovery twice from a half-completed recovery prefix produces the same final durable state.

## 3.2 Session invariants

1. Semantic conversation state is append-only.
2. Operation records never enter model context.
3. Conversation entries never contain mutable orchestration pointers.
4. All durable mutations share one monotonically increasing session sequence.
5. Storage allocates sequence numbers inside the commit; callers never reserve them independently.
6. Durable writes resolve in commit order.
7. Events are emitted only after their corresponding durable commit.
8. A reconnect obtains one atomic snapshot and then live events; live events are not replayed.
9. The session has one writer. Multiple active writers are unsupported and rejected.
10. The live reduced state must equal a fresh reduction from durable storage after restore, recovery, suspension, activation, and settlement.

## 3.3 Harness invariants

1. Every core run uses exactly one immutable harness snapshot.
2. An active core run never changes configuration beneath itself.
3. Harness edits produce immutable candidates, never in-place active mutations.
4. Candidate validation cannot grant new authority.
5. Activation occurs only at a safe core-run boundary.
6. Session-local activation is distinct from global promotion.
7. Existing sessions remain pinned to their snapshotted global plugin inputs.
8. A later global plugin edit cannot silently alter an existing session.
9. Every run and provider request is attributable to an exact harness snapshot.
10. Rollback is an ordinary immutable revision transition, not destructive file replacement.

## 3.4 Context invariants

1. The full recoverable execution history is not the same object as model context.
2. Large tool output is durably retained before it is projected into bounded model-visible form.
3. Every lossy model projection contains a stable recovery locator.
4. Reading a recovery artifact does not recursively create another inaccessible recovery artifact.
5. Compaction may reduce model context but does not delete semantic history or referenced artifacts.
6. Without compaction, navigation, or a provider-surface harness change, the serialized provider context grows only at its tail.
7. Dynamic revision IDs, timestamps, session paths, and other churn do not enter stable prompt prefixes.
8. Provider-reported cache usage and deterministic prefix similarity are tracked separately.

## 3.5 Evolution invariants

1. The target model, evaluator, task manifests, environment, budgets, and capability envelope are frozen inside an experiment.
2. The model may propose candidates but cannot score or promote them.
3. Rejected candidates remain in lineage.
4. Every candidate names a hypothesis, targeted evidence, expected effect, and regression risk.
5. A candidate is never accepted solely because it compiles.
6. A merged candidate is evaluated as a fresh candidate.
7. Prompt, tool, memory, middleware, and projection changes are treated as interacting components.
8. Final sealed tests are never fed back into the current campaign.
9. Harness evolution is compared against compute-matched retry and refinement baselines.
10. Search cost and serving cost are reported separately.

---

# 4. Separate the six state planes

Implement six explicitly distinct state planes.

## 4.1 Semantic session tree

This contains immutable, parent-linked entries such as:

* User messages.
* Assistant messages.
* Tool results.
* Compaction checkpoints and summaries.
* Branch summaries.
* Model or thinking-level changes.
* Tool-activation changes.
* Harness-revision changes.
* Typed plugin memory entries.
* Typed custom semantic entries registered by trusted host code.

Entries may be non-model-visible while still affecting the derived configuration view. For example, a `HarnessRevisionChanged` entry belongs on the semantic branch because forks and future operations must inherit it, but it must not be serialized into the provider prompt as prose.

The tree never contains:

* Active operation pointers.
* Retry counters.
* Pending queues.
* Tool execution intents.
* Storage recovery markers.
* In-process task handles.
* UI state.
* Candidate worktree paths.
* Evaluation scores.

Every entry has an immutable `parent_id`. Branches share prefixes; do not copy entry content merely to fork.

## 4.2 Lane state

A lane is:

```rust
struct LaneState {
    lane_id: LaneId,
    leaf_id: Option<EntryId>,
    status: LaneStatus,
    active_operation: Option<OperationId>,
    active_harness_revision: HarnessRevisionId,
    // Derived pending state, never separately authoritative.
}
```

Design all persisted types and APIs with `LaneId` now, but expose only the `main` lane in the first complete vertical slice.

Do not implement speculative parallel subagent UI before single-lane durability is proven. The schema must permit future parallel lanes under one session writer without a migration.

A lane owns:

* Its leaf.
* Its operation log.
* At most one open operation.
* Its queues and deferred writes.
* Its effective model/tool/harness configuration view.

## 4.3 Operation WAL

The operation log records execution facts and obligations. It is never projected into model context.

It must represent:

* Accepted operations.
* Core execution epochs.
* Retryable model steps.
* Physical provider requests.
* Tool-effect intents.
* Deferred writes.
* Queue acceptance and cancellation.
* Usage.
* Harness-activation requests.
* Abort requests.
* Terminal operation outcomes.

## 4.4 Artifact store

The artifact store owns exact immutable bytes for:

* Large tool results.
* Model-readable recovery blobs.
* Luau source files.
* Harness snapshot manifests.
* Raw redacted traces.
* Verifier reports.
* Candidate diffs.
* Evaluation outputs.
* Compaction evidence and recovery indexes.

It must be content-addressed and independent of model context.

## 4.5 Harness lineage

Harness lineage stores:

* Immutable source trees.
* Immutable harness snapshots.
* Candidate manifests.
* Parent revision links.
* Activation and rollback transitions.
* Global-profile promotions.
* Rejected candidate evidence.

The active revision is derived from durable semantic/session state. A `HEAD` file may exist as a verified cache but must not be the sole source of truth.

## 4.6 Evolution store

The evolution store contains:

* Experiment locks.
* Task and split manifests.
* Candidate evaluations.
* Raw trace references.
* Failure signatures.
* Statistical comparisons.
* Pareto-frontier state.
* Promotion decisions.
* Search and serving cost records.

It must not share writable authority with the target agent.

---

# 5. Crate and ownership boundaries

Prefer the following decomposition, adapting names only where the current repository makes another name clearly superior.

## 5.1 `tea-core`

Preserve `tea-core` as the sessionless execution mechanism.

It may own:

* Model-step orchestration.
* Tool-batch execution.
* Hook invocation.
* Cancellation.
* Run settlement.
* Compaction proposal and commit contracts.
* Typed prepared effects.
* Recovery execution primitives derived from a host-supplied recovery plan.
* Injected effect gating.

It must not own:

* Session paths.
* JSONL.
* Artifact directories.
* Global profile selection.
* TUI state.
* Candidate worktrees.
* Evaluation task manifests.
* Promotion policy.
* Ambient executor/runtime ownership.

Do not duplicate the core run loop in `tea-agent` or a new harness crate. Refactor the current state machine into reusable fresh-run and recovery entry paths if necessary, but keep one implementation of tool execution, hooks, cancellation, and settlement.

## 5.2 New `tea-session`

Create a small, executor-agnostic session crate containing:

* Opaque ID newtypes.
* Semantic entry types.
* Lane record types.
* Pure lane reduction.
* Storage traits.
* In-memory reference backend.
* JSONL v4 backend.
* v3 import.
* Corruption validation.
* Session snapshots.
* Artifact-store interfaces and filesystem implementation.
* Recovery-plan derivation.

Do not put model/provider logic in this crate.

## 5.3 New `tea-harness`

Create a durable supervisor crate containing:

* Harness snapshots and revisions.
* Candidate staging.
* Session-operation orchestration.
* Core-run epoch rollover.
* Activation and rollback.
* Context derivation from a session branch.
* Recovery coordination.
* Model-harness profiles.
* Capability-envelope enforcement.
* Host-level events and snapshots.
* Integration between `tea-session`, `tea-core`, `tea-luau`, and `tea-trace`.

Keep it executor-agnostic where practical. Inject sleep/spawn behavior rather than introducing Tokio. The `tea-agent` application may drive it using the existing Smol/futures-lite environment.

## 5.4 `tea-luau`

Keep `tea-luau` narrowly responsible for:

* Parsing closed bundles.
* Compiling and executing bounded Luau code.
* Converting typed Luau returns into host policy values.
* Enforcing source, memory, and instruction limits.
* Resolving declared bundle-local modules.
* Adapting explicitly granted host capabilities.

It must not know:

* Session paths.
* Active harness pointers.
* Candidate promotion.
* Evaluator scores.
* Provider credentials.
* Operation WAL internals.

## 5.5 `tea-trace`

Keep `tea-trace` as an immutable episode/core-run trace format.

Extend provenance to include:

* `session_id`
* `lane_id`
* `operation_id`
* `epoch_id`
* `core_run_id`
* `harness_snapshot_id`
* `harness_revision_id`
* `model_harness_profile_id`
* `experiment_id`, when applicable

Do not turn `tea-trace` into the authoritative session store.

## 5.6 `tea-agent`

The application owns:

* Session discovery and selection.
* Filesystem locations.
* TUI rendering.
* Self-extension mode selection.
* Host capability bindings.
* Provider configuration.
* Application commands.
* Concrete artifact/session stores.
* Automatic production driving.
* Live event fan-out.

## 5.7 Evaluation implementation

Preserve the existing fixture-first Rust tests and the existing stdlib-oriented `evals/quality` orchestration.

Add a Rust `tea-evolve` binary or narrowly scoped crate only when the underlying session and harness APIs are stable. Do not create an independent provider client: invoke Tea through its existing provider abstraction and CLI.

---

# 6. Durable IDs and canonical identity

Introduce opaque types, reusing the repository’s existing ID generator where appropriate:

```rust
pub struct SessionId(...);
pub struct LaneId(...);
pub struct Sequence(u64);
pub struct EntryId(...);
pub struct RecordId(...);
pub struct OperationId(...);
pub struct EpochId(...);
pub struct StepId(...);
pub struct ProviderRequestId(...);
pub struct ToolInvocationId(...);
pub struct ArtifactId(...);
pub struct HarnessTreeId(...);
pub struct HarnessSnapshotId(...);
pub struct HarnessRevisionId(...);
pub struct HarnessCandidateId(...);
pub struct ModelHarnessProfileId(...);
pub struct ExperimentId(...);
```

Do not add a UUID dependency merely for aesthetics if Tea already has a sufficient ID generator.

Use BLAKE3 for content identity:

```text
ArtifactId         = BLAKE3(exact artifact bytes)
HarnessTreeId      = BLAKE3(canonical ordered registry + normalized paths + blob IDs)
HarnessSnapshotId  = BLAKE3(canonical complete transitive harness configuration)
HarnessRevisionId  = BLAKE3(parent revisions + snapshot ID + transition metadata)
```

Do not hash ordinary JSON whose map ordering is unspecified.

Implement a tiny canonical hash writer with:

* Domain-separation prefixes.
* Fixed field order.
* Explicit discriminants.
* Length-prefixed byte strings.
* Sorted maps and normalized paths.
* Explicit schema and ABI version fields.

Example domain prefixes:

```text
tea-artifact-v1
tea-harness-tree-v1
tea-harness-snapshot-v1
tea-harness-revision-v1
tea-model-harness-profile-v1
```

Source bytes remain exact; do not normalize Luau whitespace or Unicode before hashing.

---

# 7. Session JSONL v4

## 7.1 On-disk layout

Use a per-session directory:

```text
<tea-home>/sessions/<workspace-key>/<session-stem>.tea/
    session.jsonl
    HEAD
    objects/
        blake3/
            ab/
                <full-digest>
    harness/
        trees/
            <tree-id>.json
        snapshots/
            <snapshot-id>.json
        revisions/
            <revision-id>.json
        candidates/
            <candidate-id>.json
    worktrees/
        main/
            registry.json
            plugins/
                <plugin-id>/
                    manifest.json
                    main.luau
    traces/
        <trace-id>.jsonl
    evals/
        <eval-id>.json
```

The content-addressed objects and immutable manifests are authoritative.

`worktrees/` are human-readable projections. They are reconstructible and must never be trusted as the sole source of active state.

`HEAD` is a cache. Reconstruct active state from the lane branch and durable revision entries when it is absent or inconsistent.

## 7.2 Format header

Define a v4 header containing at least:

```json
{
  "kind": "session",
  "version": 4,
  "session_id": "...",
  "created_at_ms": 0,
  "workspace": "...",
  "metadata": {},
  "initial_lane": "main"
}
```

## 7.3 Mutation lines

Use one complete JSON object plus newline per committed mutation.

Keep the fundamental line classes narrow:

```text
entry
record
lane
fact
```

Harness changes that affect branch semantics are entries. Harness candidate and activation obligations are records. Usage is a record. Do not invent a new top-level line kind for every feature.

Every line contains:

```rust
struct LogEnvelope<T> {
    seq: Sequence,
    timestamp_ms: u64,
    payload: T,
}
```

Storage assigns `seq` and timestamp inside the write commit.

## 7.4 Durability

A write is durable only after:

1. The complete line and newline are written.
2. The buffered writer is flushed.
3. The file data is synchronized according to the configured durability mode.
4. The append future resolves.

Use strict durability by default for accepted operations, effect intents, effect results, harness activations, and terminal operation outcomes.

An explicitly selected development mode may weaken `fsync`, but it must be named and visible. It must not silently become the production default.

Artifact writes use:

1. Temporary file in the destination filesystem.
2. Complete write.
3. File synchronization.
4. Atomic rename to content-addressed destination.
5. Parent-directory synchronization where supported and necessary.
6. Idempotent success when an identical object already exists.

## 7.5 Torn writes and corruption

On open:

* A missing final newline is an uncommitted torn tail and is truncated.
* A malformed final line is truncated to the preceding complete line.
* A malformed interior line is corruption.
* A duplicate sequence number is corruption.
* A non-increasing sequence is corruption.
* A referenced missing entry or operation is corruption unless the schema explicitly defines it as a provisioned future result.
* Two open operations on one lane are corruption.
* A provisioned ID that materializes with different content is corruption.
* A missing referenced content-addressed harness source or active artifact is corruption.
* An orphan object is not corruption; it is eligible for later garbage collection.

## 7.6 v3 compatibility

Preserve current Pi-compatible v3 session loading.

Required behavior:

1. Existing v3 JSONL sessions open without modification.
2. They normalize into one idle `main` lane.
3. Their existing linear messages become one parent-linked branch.
4. Existing model/thinking metadata is preserved.
5. Preserve model revision metadata when present; do not continue discarding it.
6. Opening a v3 session is read-only with respect to that original file.
7. On the first new mutation, atomically create a sibling v4 session directory, import the normalized branch, record the legacy source path and import provenance, then continue in v4.
8. Never rewrite or partially append v4 records into the old v3 file.
9. Add fixtures for labels, compaction entries, unknown v3 entry types, malformed tails, and large sessions.

---

# 8. Semantic entries

Define a versioned entry enum. Adapt names to current Tea protocol types, but preserve these semantics:

```rust
enum SessionEntry {
    UserMessage(UserMessageEntry),
    AssistantMessage(AssistantMessageEntry),
    ToolResult(ToolResultEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    ModelChanged(ModelChangedEntry),
    ThinkingChanged(ThinkingChangedEntry),
    ToolActivationChanged(ToolActivationChangedEntry),
    HarnessRevisionChanged(HarnessRevisionChangedEntry),
    PluginMemory(PluginMemoryEntry),
    Custom(CustomEntry),
}
```

Every entry contains:

```rust
struct EntryHeader {
    id: EntryId,
    parent_id: Option<EntryId>,
    seq: Sequence,
    timestamp_ms: u64,
}
```

The caller provisions `id`. Storage assigns `parent_id`, `seq`, and timestamp based on the lane’s current leaf during append.

Callers never submit an arbitrary parent when appending to a lane. This prevents stale-parent races.

Each entry type declares:

* Whether it is model-visible.
* How it contributes to effective configuration.
* How it contributes to branch/fork queries.
* Which artifact references it pins.
* Which consistency checks apply.

A tool call and its tool result must remain relationally paired in every derived model context.

---

# 9. Durable operation model

Distinguish the long-lived durable session operation from an immutable Tea core run.

## 9.1 Operation

A durable `Run` operation begins when a user prompt is accepted and ends only when:

* The model has stopped.
* No tool continuation is pending.
* No steering/follow-up input is pending.
* No harness activation is pending.
* No automatic core-run rollover is pending.
* No required compaction continuation is pending.

One durable operation may span multiple Tea core runs, called **epochs**.

This is essential for safe self-reload:

```text
durable operation R
    core epoch E0 under harness H0
    harness edit requests activation H1
    E0 settles
    H1 activates durably
    core epoch E1 continues under H1
    operation R eventually finishes
```

Do not represent this as two unrelated user operations. The user asked one task; harness rollover is an internal continuation.

## 9.2 Record catalog

Implement versioned records equivalent to:

```rust
enum LaneRecord {
    OperationStarted(OperationStartedRecord),
    OperationFinished(OperationFinishedRecord),
    AbortRequested(AbortRequestedRecord),

    EpochStarted(EpochStartedRecord),
    EpochFinished(EpochFinishedRecord),

    StepAttempted(StepAttemptedRecord),
    ProviderRequestStarted(ProviderRequestStartedRecord),
    ProviderRequestSettled(ProviderRequestSettledRecord),

    ToolStarted(ToolStartedRecord),

    QueueEnqueued(QueueEnqueuedRecord),
    QueueCancelled(QueueCancelledRecord),
    WriteDeferred(WriteDeferredRecord),

    HarnessActivationRequested(HarnessActivationRequestedRecord),

    Usage(UsageRecord),
}
```

## 9.3 Operation start

`OperationStartedRecord` includes:

```rust
struct OperationStartedRecord {
    id: OperationId,
    lane_id: LaneId,
    source_leaf_id: Option<EntryId>,
    kind: OperationKind,
    original_input: Vec<ProvisionedEntry>,
    initial_harness_revision: HarnessRevisionId,
    model_harness_profile: ModelHarnessProfileId,
    operation_resume_data: BTreeMap<StableHookId, JsonValue>,
}
```

The operation is accepted only after this record commits.

Initial user entries are provisioned in the operation record and then appended. A crash between acceptance and append is recoverable because the exact target entries and IDs are durable.

## 9.4 Epoch start

`EpochStartedRecord` includes:

```rust
struct EpochStartedRecord {
    id: EpochId,
    operation_id: OperationId,
    epoch_index: u32,
    source_leaf_id: Option<EntryId>,
    harness_revision_id: HarnessRevisionId,
    harness_snapshot_id: HarnessSnapshotId,
    model_harness_profile: ModelHarnessProfileId,
    core_run_id: CoreRunId,
    epoch_resume_data: BTreeMap<StableHookId, JsonValue>,
}
```

Persist it before starting a Tea core run.

## 9.5 Step attempts

A retryable assistant or compaction step has a durable attempt record:

```rust
struct StepAttemptedRecord {
    id: StepId,
    operation_id: OperationId,
    epoch_id: EpochId,
    kind: StepKind,
    attempt: u32,
    result_entry_id: EntryId,
    reason: Option<StepReason>,
}
```

Attempt counts survive process restarts. A crash/restart loop cannot reset a retry budget.

## 9.6 Provider requests and cost

Record each physical provider request separately:

```rust
struct ProviderRequestStartedRecord {
    request_id: ProviderRequestId,
    operation_id: OperationId,
    epoch_id: EpochId,
    step_id: StepId,
    physical_attempt: u32,
    model_harness_profile: ModelHarnessProfileId,
    request_surface_digest: Digest,
    idempotency_key: Option<String>,
}

struct ProviderRequestSettledRecord {
    request_id: ProviderRequestId,
    outcome: ProviderOutcome,
    usage: Option<Usage>,
    response_artifact: Option<ArtifactId>,
    classification: ProviderSettlementClassification,
}
```

The provider-start record commits before network dispatch.

The settlement record commits before:

* Retry classification.
* Overflow handling.
* Response discard.
* Assistant-entry append.
* A subsequent provider request.

Record usage even when:

* The provider response is rejected.
* A retry follows.
* An overflow result is discarded.
* A compaction request fails.
* The final assistant entry never materializes.

Do not make cost accounting dependent on semantic result durability.

Where a provider supports idempotency keys, derive one from the durable physical request identity. Do not pretend all providers support exactly-once request semantics.

## 9.7 Tool-effect intent

After tool-call schema validation and `before_tool` argument transformation, but before the tool’s effect begins, persist:

```rust
enum ToolReplayPolicy {
    Never,
    Safe,
}

struct ToolStartedRecord {
    record_id: RecordId,
    operation_id: OperationId,
    epoch_id: EpochId,
    assistant_entry_id: EntryId,
    tool_index: u32,
    tool_call_id: String,
    tool_name: String,
    effective_args: JsonValue,
    result_entry_id: EntryId,
    replay_policy_at_start: ToolReplayPolicy,
    tool_definition_digest: Digest,
    harness_revision_id: HarnessRevisionId,
    idempotency_key: String,
}
```

The durable invocation identity is:

```text
assistant_entry_id + tool_index
```

Validate that this position still names the stored tool call ID and tool name during recovery.

Blocked or schema-invalid tool calls do not write `ToolStarted`. They begin no effect. Their error result is an ordinary semantic tool-result entry.

## 9.8 Tool result

After the tool returns:

1. Run the bounded `after_tool` policy.
2. Redact according to the model-readable artifact policy.
3. Persist the complete canonical result inline or in the artifact store.
4. Construct the bounded model-facing projection.
5. Append the provisioned `ToolResultEntry`.
6. Only then emit the committed tool event or continue model execution.

The tool-result entry persists:

* Invocation identity.
* Full-result artifact or inline payload reference.
* Model-facing projection.
* Error status.
* `terminate`.
* Usage reported by nested model work.
* Projection strategy ID.
* Redaction policy ID.

A separate terminal record for the tool is unnecessary if the result entry is the complete durable outcome.

## 9.9 Tool recovery matrix

Implement exactly:

### State X0: no assistant entry

There is no durable tool call. Resume the assistant step.

### State X1: assistant entry exists, no `ToolStarted`, no result

Run the complete normal path again:

* Validate schema.
* Invoke `before_tool`.
* Possibly block.
* Persist `ToolStarted` if the effect will execute.
* Execute.

### State X2: `ToolStarted` exists, no result

Replay only when both are true:

* The persisted declaration says `Safe`.
* The current resolved tool declaration says `Safe`.

Otherwise append a synthetic interrupted result under the provisioned result ID.

The synthetic result must say that execution was interrupted and the harness cannot prove whether the external effect occurred. It must not claim the effect definitely failed or definitely succeeded.

### State X3: result entry exists

Do not execute the tool again.

### State X4: parallel batch prefix committed

Preserve original tool ordinals. Resolve only missing calls. Finalize model-visible results in original source order even if effects executed concurrently.

Run all recovery states twice in fixtures to prove idempotence.

## 9.10 Replay safety

Default effectful tools to `Never`.

A tool may be `Safe` only when its contract is explicit and tested, such as:

* Pure calculation.
* Immutable read.
* Content-addressed idempotent write.
* Request with a supported idempotency key.
* `tea_harness.apply`, where the candidate identity is deterministic and staging is idempotent.

A plugin cannot unilaterally mark a tool safe. The host computes the maximum permitted replay policy from the tool implementation and granted capability composition.

---

# 10. Pure reduction and recovery plans

Implement a pure reducer:

```rust
fn reduce_lane(input: LaneReductionInput) -> Result<LaneReduction, Corruption>;
```

It derives:

```rust
struct LaneReduction {
    lane_state: ReducedLaneState,
    effective_configuration: EffectiveLaneConfiguration,
    recovery_plan: Option<RecoveryPlan>,
    pending_queues: PendingQueues,
    pending_writes: Vec<PendingWrite>,
    pending_harness_activation: Option<PendingHarnessActivation>,
    usage_totals: Usage,
}
```

It performs:

* No writes.
* No provider calls.
* No tool calls.
* No hook execution.
* No clock reads.
* No filesystem mutation.

It validates:

* One open operation maximum.
* Consecutive attempt numbers.
* Valid operation references.
* No records after operation finish.
* Unique invocation identities.
* Correct tool ordinal/name/call-ID correspondence.
* Provisioned result identity.
* Queue cancellation validity.
* Harness activation parent identity.
* Active snapshot existence.
* Valid epoch sequence.
* Terminal outcome uniqueness.

Recovery consumes one reducer result. Do not rederive independent interpretations in multiple modules.

After:

* opening a session,
* completing recovery,
* suspending,
* activating a harness,
* finishing an epoch,
* or finishing an operation,

recompute the reduction from storage and compare it to live state. A mismatch faults the harness immediately.

---

# 11. Injected effect boundary and manual drive

Every effect must cross one injected boundary:

```rust
enum EffectKind {
    DurableWrite,
    ProviderRequest,
    ToolExecution,
    HookInvocation,
    Timer,
    ArtifactWrite,
    HarnessActivation,
}
```

Provide two drive modes:

```rust
enum DriveMode {
    Automatic,
    Manual,
}
```

Production uses automatic mode.

Tests use manual mode with a stable interface conceptually equivalent to:

```rust
fn peek_action(&self) -> Option<PendingAction>;
async fn execute_action(&self, expected: ActionId) -> Result<ActionOutcome>;
```

Required semantics:

1. `peek_action` has no side effect.
2. It returns the same action until that action is released or the harness closes.
3. `execute_action` releases exactly the identified action.
4. While parked, no storage write, provider request, tool call, hook, timer, or activation occurs.
5. Stopping before an action leaves exactly the preceding durable prefix.
6. Production and tests run the same procedures. Manual mode changes only when effects are released.

Use this to mechanically test process death before and after every durable/effect boundary.

Do not implement a shadow “test scheduler.” Wrap the actual production effect calls.

---

# 12. Recoverable tool-result projection

Implement projection at initial result commit. Do not append the entire result to model context and retroactively rewrite history later.

## 12.1 Full result first

Before constructing a lossy model projection, retain the complete redacted result as either:

```rust
enum PayloadRef {
    Inline(JsonValue),
    Artifact {
        artifact_id: ArtifactId,
        byte_len: u64,
        media_type: String,
    },
}
```

Small results may remain inline. Large results go to the artifact store.

The semantic entry always knows where the complete model-readable result resides.

## 12.2 Model projection

Use a deterministic policy with configurable, benchmarked thresholds.

A starting policy may resemble:

```text
small:
    complete inline result

medium:
    prefix marker
    bounded head
    omission marker
    bounded tail

very large:
    prefix marker
    smaller preview
    artifact only for the complete result
```

Do not blindly freeze Pi or DeepSeek threshold values. Begin with their 8,192 / 4,096 / 1,024 and 50,000-character reference points only as an experiment seed, then calibrate against Tea’s existing compaction metrics and Laguna S 2.1.

The first bytes of every reduced projection must contain a stable locator:

```text
[full tool result: tea-artifact://blake3/<digest>;
 preview omits bytes <start>..<end>;
 use tea_artifact_search or tea_artifact_read]
```

Put the locator before the retained head so another head-preserving serializer cannot erase it.

Use byte offsets in the durable contract. Ensure preview cut points fall on UTF-8 boundaries. Tools may additionally report line ranges for convenience.

## 12.3 Artifact tools

Add stable Rust-owned read-only tools:

```text
tea_artifact_read
tea_artifact_search
tea_history_search
```

`tea_artifact_read` accepts:

* Artifact ID.
* Byte offset or line offset.
* Bounded maximum length.

`tea_artifact_search` accepts:

* Artifact ID.
* Literal query and, only if already supported cheaply, regex query.
* Bounded result count.
* Context width.

`tea_history_search` searches compacted semantic history and referenced artifacts by:

* Text.
* Tool name.
* Operation ID.
* Entry type.
* Time/sequence range.
* Harness revision.
* Error status.

Artifact reads must return a bounded requested page directly. Do not run that page through the ordinary spill projector again. Otherwise the model can enter a recovery loop in which reading the spill creates another spill locator.

## 12.4 Storage failure

Do not copy Pi’s fail-open “prune without spill” behavior into Tea’s recoverable mode.

When full-result artifact persistence fails:

1. Attempt to commit the complete result inline if it satisfies the hard session-record limit.
2. If complete durable retention still fails, fault the harness.
3. Do not continue the model with a projection that falsely claims the result is recoverable.
4. Keep the in-memory result available for a persistence retry while the process lives.
5. If the process dies after the external effect but before result durability, ordinary `ToolStarted` recovery semantics apply.

This cannot eliminate the fundamental ambiguity of a crash after a non-idempotent effect. It must eliminate silent information loss during normal operation.

## 12.5 Redaction

Define explicit artifact policies:

```rust
struct ArtifactPolicy {
    policy_id: ArtifactPolicyId,
    model_readable: bool,
    redact_before_persist: bool,
    maximum_inline_bytes: usize,
    maximum_page_bytes: usize,
}
```

The recoverable artifact is complete relative to the model-visible, redacted result. Do not persist credentials merely to claim byte-for-byte raw auditability.

Record raw byte length and an optional one-way digest before redaction only when this can be done without retaining the secret.

## 12.6 Compaction integration

The assumed compaction implementation must:

* Preserve artifact locators in retained tool-result projections.
* Preserve the original user task and protected root instructions.
* Keep tool-call/result relational validity.
* Never delete artifacts still referenced by semantic entries or compaction checkpoints.
* Add a model-visible recovery marker to each compaction summary:

```text
[Earlier session history remains searchable with tea_history_search;
 checkpoint=<entry-id>]
```

A compaction entry should retain:

* Covered entry range.
* Retained-tail boundary.
* Summary.
* Compaction strategy ID.
* Recovery-index artifact ID.
* Relevant metric evidence.
* Harness revision used to generate it.

The recovery index is metadata, not a substitute for raw history. It may list commands, files, errors, artifact IDs, and entry spans to accelerate search.

---

# 13. Harness source trees and snapshots

## 13.1 Harness layers

Compose the effective harness in this order:

```text
trusted Rust-owned base profile
stable optional self-extension addendum
operator-approved global Luau plugins
session/lane-local Luau plugins
```

There is also a separate candidate tree that is never active merely because it exists.

Ownership:

```text
base profile:
    Rust-owned; not editable by the target agent

global plugins:
    operator-approved; snapshotted into a session at creation

session plugins:
    editable by the target agent within the session capability ceiling

candidate tree:
    staged and validated; inactive until a durable activation transition
```

## 13.2 Session pinning

At session creation:

1. Resolve the selected base profile.
2. Resolve the selected model-harness profile.
3. Resolve the ordered global plugin registry.
4. Copy all transitive source blobs into immutable content-addressed storage.
5. Build the initial harness tree and snapshot.
6. Append or seed the initial harness revision on the `main` branch.

Later edits to global plugin files do not change the session.

Provide an explicit rebase operation for importing a newer approved global state. Treat rebase as another candidate and activation transition.

## 13.3 Plugin source layout

Use:

```text
plugins/<plugin-id>/
    manifest.json
    main.luau
    <declared-module>.luau
```

Restrict plugin IDs and paths to a portable conservative alphabet.

Reject:

* Absolute paths.
* `..`.
* NUL.
* Empty components.
* Symlinks.
* Undeclared modules.
* Duplicate canonical paths.
* Case-insensitive path collisions.
* Imports outside the plugin bundle.
* Reserved tool names.
* Duplicate tool names.
* Excessive file count or source size.

## 13.4 Manifest

A v2 manifest should conceptually contain:

```json
{
  "schema_version": 1,
  "abi_version": 2,
  "id": "session.verify",
  "entrypoint": "main.luau",
  "modules": [
    "main.luau"
  ],
  "requested_capabilities": [],
  "resource_limits": {
    "source_bytes": 65536,
    "memory_bytes": 1048576,
    "instruction_checks": 10000
  }
}
```

Reuse and extend Tea’s existing bundle schema rather than creating a competing loader.

## 13.5 Snapshot type

Implement a canonical structure equivalent to:

```rust
struct HarnessSnapshotV1 {
    schema_version: u16,
    luau_abi_version: u16,

    base_profile_digest: Digest,
    model_harness_profile: ModelHarnessProfileId,

    self_extension_prompt_version: Option<String>,

    ordered_global_plugins: Vec<PluginBundleRef>,
    ordered_session_plugins: Vec<PluginBundleRef>,

    prompt_sections: Vec<PromptSectionDescriptor>,
    tool_presentations: Vec<ToolPresentationDescriptor>,
    hooks: HookBundleDescriptor,
    capability_bindings: Vec<CapabilityBindingRef>,
    resource_limits: HarnessResourceLimits,

    compaction_strategy: CompactionStrategyDescriptor,
    tool_projection_strategy: ToolProjectionStrategyDescriptor,
    failure_policy: FailurePolicyDescriptor,
}
```

Compute separate fingerprints:

```rust
struct HarnessSurfaceFingerprints {
    system_prompt_digest: Digest,
    ordered_tool_definitions_digest: Digest,
    hook_bundle_digest: Digest,
    capability_bindings_digest: Digest,
    compaction_policy_digest: Digest,
    provider_surface_digest: Digest,
}
```

`provider_surface_digest` covers only exact model-visible system prompt and ordered tool definitions.

A hook-only edit may change the complete snapshot ID without changing the provider surface digest.

## 13.6 Revision type

```rust
struct HarnessRevisionV1 {
    revision_id: HarnessRevisionId,
    snapshot_id: HarnessSnapshotId,
    parent_revision_ids: Vec<HarnessRevisionId>,
    actor: HarnessActor,
    reason: HarnessRevisionReason,
    candidate_id: Option<HarnessCandidateId>,
    created_at_ms: u64,
}
```

The timestamp is revision metadata, not part of provider-visible content.

---

# 14. Candidate staging

Implement candidate/active separation rigorously.

A candidate manifest contains:

```rust
struct HarnessCandidateV1 {
    candidate_id: HarnessCandidateId,
    parent_revision_id: HarnessRevisionId,
    proposed_snapshot_id: HarnessSnapshotId,

    actor: HarnessActor,
    operation_id: Option<OperationId>,
    tool_invocation_id: Option<ToolInvocationId>,

    hypothesis: CandidateHypothesis,
    changed_paths: Vec<NormalizedPath>,
    registry_operations: Vec<RegistryOperation>,
    changed_surfaces: BTreeSet<HarnessSurface>,

    targeted_failures: Vec<FailureSignatureId>,
    evidence: Vec<EvidenceRef>,
    expected_effects: Vec<String>,
    regression_risks: Vec<String>,

    capability_diff: CapabilityDiff,
    complexity_delta: HarnessComplexityDelta,
}
```

Candidates are immutable after staging.

Candidate validation must include:

1. Canonical path validation.
2. File and source-size limits.
3. Manifest schema.
4. Closed import graph.
5. Luau compilation.
6. Luau execution under resource limits.
7. Prompt-section composition.
8. Tool-schema validity.
9. Reserved-name and collision checks.
10. Hook return-shape validation.
11. Capability-envelope subset check.
12. Provider-surface token/byte limits.
13. Compaction/projection descriptor validity.
14. Deterministic snapshot recomputation.
15. No-op detection.

A candidate producing the same snapshot ID as its parent is a no-op. Do not activate it or emit a misleading reload event.

---

# 15. Automatic safe self-reload

User-visible “hot reload” must be implemented as an automatic durable core-run rollover, never an in-place active-run mutation.

## 15.1 Stable model-facing tool

Replace the current draft/global-file authoring path with one stable host-owned tool named `tea_harness`.

It supports:

```text
status
help
list
read
diff
apply
rollback
```

There is no `reload` operation.

A successful mutating operation automatically requests activation.

The internal API must remain independent of its model-facing schema so model profiles can later test a split-tool presentation without rewriting the harness manager.

## 15.2 `apply`

`apply` accepts an atomic patch:

```json
{
  "operation": "apply",
  "base_revision": "<revision-id>",
  "hypothesis": {
    "failure_signature": "repeated completion without targeted verification",
    "expected_effect": "run the narrowest relevant validator before finalization",
    "regression_risk": "unnecessary validation on trivial tasks"
  },
  "files": [
    {
      "operation": "upsert",
      "path": "plugins/session.verify/manifest.json",
      "content": "{...}"
    },
    {
      "operation": "upsert",
      "path": "plugins/session.verify/main.luau",
      "content": "..."
    }
  ],
  "registry_operations": [
    {
      "operation": "add",
      "plugin_id": "session.verify"
    }
  ]
}
```

Also support deletion with an expected current blob digest to prevent stale destructive edits.

The model cannot write the registry file directly. Registry changes are structured operations.

The model cannot:

* Modify the trusted base.
* Modify snapshotted global plugin source in place.
* Add a capability grant.
* Reorder global plugins.
* Change the evaluator.
* Change the model profile.
* Change resource ceilings.
* Change promotion policy.

A mutating `tea_harness` invocation must be the only tool call in its assistant tool batch. If the model emits other calls in the same batch, block the mutation with an actionable result asking it to retry the harness mutation alone. Do not execute a partially ordered mixture of harness activation and unrelated effects.

## 15.3 Transaction

The exact successful flow is:

```text
1. Assistant requests tea_harness.apply as the sole tool call.
2. Core validates call shape and runs before_tool.
3. Persist ToolStarted for tea_harness.apply.
4. Build candidate objects under the invocation’s idempotency key.
5. Validate the complete proposed snapshot.
6. Persist HarnessActivationRequested.
7. Append the tea_harness tool result with terminate=true.
8. Current core epoch settles under the old snapshot.
9. Durable supervisor verifies the pending request and candidate again.
10. Resolve every source and capability binding.
11. Append HarnessRevisionChanged with the provisioned entry ID.
12. Update the in-memory configuration derived from that committed entry.
13. Emit HarnessSnapshotActivated.
14. Start a new core epoch under the new immutable snapshot.
15. Continue the same durable user operation without another user message.
```

The model must not poll, sleep, or issue a reload command.

The `tea_harness.apply` result tells it:

* Candidate ID.
* Snapshot ID.
* Validation status.
* Whether activation is scheduled.
* That Tea will continue automatically.

Do not inject a second redundant model-visible “reload happened” message when the model initiated the edit. The committed tool result already provides that information.

For external/operator-triggered activation, inject at most one compact host annotation when necessary.

## 15.4 Activation crash matrix

Test every state:

### A. Objects written, no activation request

The objects are orphan candidates. They do not activate. GC may later remove them.

### B. Activation request committed, tool result absent

Recovery sees the unfinished replay-safe `tea_harness.apply`, reruns staging idempotently, and appends the provisioned result.

### C. Tool result committed, epoch not settled

Resume the epoch’s termination path. Do not activate while the old core run is still active.

### D. Epoch settled, activation entry absent

Validate candidate and append the provisioned `HarnessRevisionChanged` entry.

### E. Activation entry committed, in-memory configuration absent

Restore derives the revision from the branch and loads the exact snapshot. No second activation entry is written.

### F. Activation committed, continuation epoch absent

Start the next epoch under the committed revision.

### G. Continuation epoch accepted, no effect begun

Resume that epoch.

### H. New epoch running

Normal operation recovery applies.

### I. Process dies during rollback

Rollback uses the same activation protocol and is equally recoverable.

## 15.5 Rollover budget

Add a configurable operation-local rollover budget.

Default adaptive behavior should permit one successful self-extension rollover per user operation. A profile may permit two for repair of an initially invalid plugin, but the default must prevent unbounded edit–reload loops.

When exhausted:

* Reject further automatic activation for that operation.
* Preserve the candidate for inspection.
* Continue or terminate according to explicit policy.
* Emit a bounded diagnostic.

---

# 16. Host-level events and snapshots

Keep existing core `AgentEvent` or equivalent run-scoped events.

Add an application-level envelope:

```rust
enum TeaEvent {
    Agent(AgentEvent),
    Session(SessionEvent),
    Harness(HarnessEvent),
    Artifact(ArtifactEvent),
}
```

Minimum harness events:

```rust
enum HarnessEvent {
    CandidateStaged {
        lane_id: LaneId,
        candidate_id: HarnessCandidateId,
        parent_revision_id: HarnessRevisionId,
        snapshot_id: HarnessSnapshotId,
        changed_paths: Vec<NormalizedPath>,
    },

    CandidateRejected {
        lane_id: LaneId,
        candidate_id: Option<HarnessCandidateId>,
        active_revision_id: HarnessRevisionId,
        stage: ValidationStage,
        code: DiagnosticCode,
        diagnostic: String,
    },

    ActivationScheduled {
        lane_id: LaneId,
        operation_id: OperationId,
        candidate_id: HarnessCandidateId,
        target_revision_id: HarnessRevisionId,
    },

    SnapshotActivated {
        lane_id: LaneId,
        operation_id: OperationId,
        previous_revision_id: HarnessRevisionId,
        revision_id: HarnessRevisionId,
        snapshot_id: HarnessSnapshotId,
        provider_surface_changed: bool,
        changed_surfaces: BTreeSet<HarnessSurface>,
    },

    RolloverStarted {
        lane_id: LaneId,
        operation_id: OperationId,
        from_epoch: EpochId,
        to_revision_id: HarnessRevisionId,
    },

    RolloverCompleted {
        lane_id: LaneId,
        operation_id: OperationId,
        epoch_id: EpochId,
        revision_id: HarnessRevisionId,
    },

    RolledBack {
        lane_id: LaneId,
        from_revision_id: HarnessRevisionId,
        to_revision_id: HarnessRevisionId,
    },
}
```

Artifact events contain IDs, sizes, and policy identifiers, never content.

Default telemetry must not contain:

* Prompts.
* Completions.
* Tool arguments.
* Tool output.
* Source files.
* Provider payloads.
* Headers.
* Credentials.

Events observe committed state. Hooks may affect execution. Telemetry is passive process-local diagnostics. Keep these three concepts separate.

A UI connection receives:

1. One atomic `HarnessSnapshotView`.
2. Then live events.
3. On reconnect, a new snapshot rather than event replay.

---

# 17. Luau ABI v2

Preserve v1 compatibility. Add an explicitly versioned v2 rather than changing existing return semantics invisibly.

## 17.1 Editable surfaces

ABI v2 may control only typed bounded surfaces:

* Namespaced prompt sections.
* Tool presentation overlays.
* Pre-tool policy.
* Post-tool model projection.
* Context projection patches.
* Stop/continuation policy.
* Explicit operation/epoch resume data.
* Typed plugin memory proposals.
* Compaction strategy selection or parameters.
* Tool-result projection selection or parameters.
* Pure or explicitly capability-bound plugin tools.

## 17.2 Prompt sections

Replace append-only growth with named sections.

Conceptual return:

```lua
return {
    prompt_sections = {
        {
            id = "verification",
            content = "Before finishing, run the narrowest relevant validator.",
        },
    },
}
```

Rules:

* Base sections are protected.
* Plugin sections are namespaced by plugin ID.
* A plugin may replace or delete its own sections in a later source revision.
* Composition order is deterministic.
* Duplicate section IDs within one effective registry reject.
* Prompt byte and token estimates are recorded.
* No timestamp or revision ID is inserted into the prompt.

## 17.3 `before_tool`

Allow:

* Permit.
* Block with a reason.
* Terminate.
* Schema-valid argument normalization.

Argument normalization must be validated against the canonical tool schema after transformation.

Unknown behavior-changing arguments must not be silently accepted.

## 17.4 `after_tool`

Allow changes only to the model-visible result and bounded control data:

* Add a recovery hint.
* Select a projection strategy.
* Add structured annotations.
* Mark error classification.
* Request `terminate`.
* Propose a typed memory entry.

It must not:

* Change the already completed external effect.
* Delete the raw artifact.
* Alter the tool identity.
* Falsify usage.
* Grant authority.
* Write operation records directly.

## 17.5 Context transformation

Do not expose arbitrary mutable access to raw semantic history.

Return a typed patch such as:

```rust
struct ContextProjectionPatch {
    retain_entries: Vec<EntryId>,
    omit_eligible_entries: Vec<EntryId>,
    annotations: Vec<ContextAnnotation>,
    selected_memory: Vec<EntryId>,
    requested_compaction_strategy: Option<CompactionStrategyId>,
}
```

The Rust core validates protected invariants:

* Root system contract remains.
* Original user task remains.
* Required safety/capability statements remain.
* Tool-call/result pairs remain valid.
* Artifact recovery locators remain available.
* The context fits provider constraints.
* No semantic entry is deleted from storage.

## 17.6 Operation and epoch resume data

A plugin may return bounded JSON state from stable hooks:

```text
before_operation
before_epoch
before_resume
```

Use stable registration IDs.

Persist each hook’s state under its own registration ID. On resume, a handler receives only its own value.

`before_resume` must be idempotent. It rebuilds process-local state but does not own durable state.

A crash before the durable consumer of a hook result commits may run that hook again. Document this explicitly.

Initially, deny effectful capabilities from resume hooks. Later capability calls must be idempotent and keyed by a durable hook invocation identity.

## 17.7 Typed plugin memory

Do not introduce a hidden mutable Lua global memory store.

A plugin may propose:

```rust
struct PluginMemoryProposal {
    plugin_id: PluginId,
    kind: String,
    content: PayloadRef,
    provenance: Vec<EvidenceRef>,
    visibility: MemoryVisibility,
    retention: MemoryRetention,
}
```

Rust validates and appends a semantic `PluginMemoryEntry`.

Memory selection is a context-policy decision. Full memory remains external and queryable.

## 17.8 Compaction and projection policy

Plugins may choose or parameterize registered, versioned strategies.

They do not commit context replacement directly.

The existing Rust compaction transaction remains:

```text
policy proposes
Rust validates
Rust measures
Rust commits or rejects
```

## 17.9 Plugin-defined tools

Wire the existing `handler_source`/Luau handler machinery into `tea-agent`, but begin with a closed capability catalog.

Initial permitted capabilities should be:

* Pure JSON/text transformation.
* Read-only session metadata.
* Read-only artifact search/read.
* Read-only redacted trace search.
* Deterministic computation.

Do not initially expose:

* Arbitrary shell.
* Arbitrary filesystem.
* Network.
* Provider credentials.
* Session WAL mutation.
* Nested unrestricted tool dispatch.
* Evaluator control.
* Global profile mutation.

Capability grants are host objects bound to:

* Plugin ID.
* Exact harness snapshot digest.
* Capability version.
* Resource limits.

A plugin name or path is not a trust identity.

## 17.10 Authority ceiling

Each session has a frozen capability ceiling:

```text
requested capabilities ⊆ session capability ceiling
```

An automatic session-local candidate must have an empty capability-expansion diff.

A candidate requesting more authority is retained but routed to explicit manual review. It cannot self-approve.

---

# 18. Self-extension modes

Implement:

```rust
enum SelfExtensionMode {
    Off,
    Author,
    Adaptive,
}
```

## `Off`

* No self-extension system addendum.
* No `tea_harness` model-facing tool.
* Session still records the exact harness snapshot for reproducibility.

## `Author`

* The user may ask the agent to create or edit a plugin.
* The agent should not proactively mutate the harness.
* Valid edits activate safely at an epoch boundary.
* Continuation may be automatic after a user-requested edit.

## `Adaptive`

* The agent may propose a session-local plugin when it observes a concrete reusable harness failure.
* Valid edits activate automatically through rollover.
* Mutation is limited by the operation rollover budget.
* Global promotion remains external.

Store the selected mode in session metadata and operation provenance. Do not place the mode’s dynamic identity in prompt prose beyond the presence or absence of the stable addendum.

---

# 19. Stable self-extension prompt

Make the optional addendum a versioned exact artifact.

Use the following as the initial concise candidate, named something like `self-extension-v1-concise`:

```text
Session harness self-extension

You may improve Tea's session-local harness only when repeated evidence or a clearly reusable failure indicates a harness problem. Do not create a plugin for one-off task facts or ordinary implementation work. Prefer the smallest change and preserve unrelated behavior.

Use `tea_harness` to inspect or atomically edit Luau plugins. A plugin is a closed directory containing `manifest.json` and its declared `.luau` modules. The manifest names the entrypoint and every module. The entrypoint returns named prompt sections and may declare bounded hooks or capability-neutral tools. Imports must be relative and declared. Plugins have no ambient filesystem, process, network, environment, session-storage, evaluator, or capability-grant access. Use `tea_harness` with `operation: "help"` for the complete ABI.

After an edit, Tea validates and snapshots the complete harness automatically. A valid snapshot activates only at a safe run boundary; Tea then continues the task under the new snapshot. Failure leaves the previous snapshot active. Never issue or wait for a reload command.
```

Requirements:

* Exact bytes are golden-tested.
* The full ABI handbook is not embedded in every provider request.
* `tea_harness help` returns the versioned handbook or an artifact locator.
* The addendum appears before all dynamic plugin prompt sections.
* It does not contain the current session ID, revision ID, path, timestamp, or file listing.
* It is evaluated against shorter and longer alternatives rather than assumed optimal.

---

# 20. Prompt and tool composition for cache stability

Compose exact model-visible system content in this order:

```text
1. trusted base system prompt
2. stable optional self-extension addendum
3. approved global plugin prompt sections
4. session-local plugin prompt sections
```

Compose tools in this order:

```text
1. stable base coding tools
2. stable tea_harness and artifact/history tools
3. approved global plugin tools
4. session-local plugin tools
```

Fix the current behavior where stable authoring instructions are appended after dynamic extension text.

Rules:

* Registry order is deterministic.
* Tool schema serialization is deterministic.
* No file-tree dump is injected.
* No candidate lineage is injected.
* No full plugin handbook is injected.
* No active revision ID is injected.
* No reload event prose is injected when the tool result already communicates it.
* No retroactive tool-result rewrites occur.
* Deferred conversational writes append only at checkpoints.
* Compaction is an explicit cache-invalidating operation.
* Harness changes that do not affect the provider surface preserve the same provider-surface digest.

Measure per request:

```rust
struct CacheEvidence {
    deterministic_common_prefix_bytes: Option<u64>,
    deterministic_common_prefix_tokens_estimate: Option<u64>,
    provider_cache_read_tokens: Option<u64>,
    provider_cache_write_tokens: Option<u64>,
    provider_surface_digest: Digest,
}
```

Unknown provider metrics remain unknown, never zero.

---

# 21. Model-harness profiles

Implement a model-specific but provider-agnostic profile layer:

```rust
struct ModelHarnessProfile {
    profile_id: ModelHarnessProfileId,
    provider_family: String,
    requested_model: String,
    returned_model_revision: Option<String>,
    base_prompt_variant: String,
    tool_presentation_variant: String,
    compatibility_normalizers: Vec<NormalizerId>,
    default_compaction_strategy: CompactionStrategyId,
    default_projection_strategy: ToolProjectionStrategyId,
}
```

Profiles are immutable, versioned, and included in experiment locks and harness snapshots.

Use profiles to address model/harness coupling without contaminating core semantics.

## Tool-schema deviation evidence

When a model emits:

* Unknown argument names.
* Arguments associated with another common harness.
* Duplicate semantic fields.
* Type mismatches.
* Unsupported edit-tool options.

Record a structured diagnostic:

```rust
struct ToolSchemaDeviation {
    profile_id: ModelHarnessProfileId,
    tool_name: String,
    unknown_fields: Vec<String>,
    missing_fields: Vec<String>,
    type_mismatches: Vec<FieldMismatch>,
    raw_arguments_artifact: ArtifactId,
}
```

Do not silently accept unknown arguments.

A profile-specific alias normalizer may be introduced only when:

* The mapping is semantically unambiguous.
* It produces canonical schema-valid arguments.
* It is covered by deterministic tests.
* It improves the target profile’s evaluation.
* It causes no held-out regression.
* The normalized form is recorded.

Promotion remains profile-specific until cross-profile tests establish transfer.

---

# 22. Context derivation

Implement a deterministic projection from a lane leaf:

```rust
fn derive_model_context(
    session: &dyn SessionReader,
    lane: LaneId,
    harness: &HarnessSnapshot,
    limits: ProviderLimits,
) -> Result<DerivedContext>;
```

The projection:

1. Walks the immutable parent chain.
2. Resolves model/thinking/tool/harness configuration entries.
3. Applies committed compaction entries.
4. Includes model-visible semantic entries only.
5. Maintains tool-call/result pairing.
6. Uses committed tool-result projections, never raw operation records.
7. Includes selected typed memory.
8. Produces deterministic exact prompt/tool bytes.
9. Records all omitted entry spans and recovery paths.
10. Never mutates the tree.

Add a byte-level test:

> For any two consecutive requests on one lane with no compaction, navigation, provider-surface harness change, or changed protected root, the previous serialized provider input is an exact prefix of the next input up to the provider’s expected request envelope behavior.

When a harness change alters only hooks and leaves provider-visible prompt/tools identical, the request surface must remain cache-compatible.

---

# 23. Durable self-improvement control plane

Session-local adaptation and empirical global evolution are different systems.

## 23.1 Session-local adaptation

A session candidate may activate after structural and authority validation. This only means:

* It is valid.
* It is bounded.
* It is traceable.
* It cannot increase authority.
* It is reversible.

It does not mean the candidate is empirically better.

## 23.2 Global evolution

Implement an external supervisor with this rule:

> **The model proposes. The supervisor evaluates. Rust promotes.**

The target agent cannot write:

* Task manifests.
* Split assignments.
* Verifiers.
* Evaluation results.
* Promotion decisions.
* Global active profile pointers.
* Experiment budgets.
* Sealed test outputs.

## 23.3 Experiment lock

```rust
struct ExperimentLockV1 {
    experiment_id: ExperimentId,

    target_profiles: Vec<ModelHarnessProfileId>,
    evolver_profile: ModelHarnessProfileId,

    initial_harness: HarnessSnapshotId,

    task_manifest_digest: Digest,
    split_manifest_digest: Digest,
    evaluator_digest: Digest,
    environment_digest: Digest,
    capability_envelope_digest: Digest,

    search_budget: SearchBudget,
    serving_budget: ServingBudget,
    promotion_policy_digest: Digest,

    tea_build_identity: BuildIdentity,
}
```

The lock also records:

* Tea Git commit.
* Dirty patch digest.
* Rust version.
* OS and architecture.
* Provider adapter version.
* Requested model identifier.
* Returned model/revision metadata.
* Thinking and decoding configuration.
* Workspace commit and initial dirty patch.
* Timeouts and retries.
* Tool schema profile.
* Compaction strategy.
* Artifact/redaction policy.

## 23.4 Failure signatures

Mine verifier-grounded failures into:

```rust
struct FailureSignatureV1 {
    terminal_cause: VerifierFailureCode,
    causal_status: CausalStatus,
    locus: FailureLocus,
    mechanism: MechanismCode,
    evidence: Vec<TraceSpanRef>,
    confidence: EvidenceConfidence,
    addressability: Addressability,
}
```

Suggested loci:

```text
task_understanding
repository_discovery
context_retrieval
tool_selection
tool_arguments
tool_execution
tool_result_interpretation
implementation
verification
failure_recovery
memory
compaction
termination
harness_runtime
```

Addressability:

```rust
enum Addressability {
    LuauPolicy,
    HostSubstrate,
    RustCoreGap,
    TaskSpecific,
    ModelCapabilityLimit,
    UnstableOrUnknown,
}
```

Do not force every failure into a plugin edit. Only recurrent, evidence-supported, plausibly editable failures are candidate targets.

## 23.5 Evidence pipeline

Use:

1. Deterministic verifier facts.
2. Deterministic trace facts.
3. Optional diagnostic-model causal hypothesis.
4. Required trace-span citations for causal claims.
5. Normalized clustering.
6. Representative failures and passing contrasts.
7. Prior attempted fixes and regressions.

Summaries are indexes, not the source of truth.

Expose bounded read-only evolver tools or CLI commands:

```text
search_evidence
list_failure_clusters
read_trace_span
compare_task_runs
inspect_snapshot
diff_snapshots
inspect_candidate_history
list_pareto_frontier
```

Store raw redacted traces, source, scores, and decisions in a searchable hierarchy. Do not place the complete history into one proposer prompt.

## 23.6 Candidate proposal

Require each proposal to be:

* Minimal.
* Mechanistically distinct from sibling proposals.
* Tied to one primary hypothesis.
* Supported by trace evidence.
* Explicit about changed surfaces.
* Explicit about complexity and cache effects.
* Explicit about regression risks.
* Within the frozen capability envelope.

The proposer may inspect any prior candidate, not only the current parent.

Maintain:

* One active champion.
* A bounded Pareto frontier.
* A bounded archive indexed by failure locus, mechanism, and edited surface.
* Complete lineage for rejected candidates.

## 23.7 Freeze during candidate evaluation

When evaluating one candidate as a fixed harness:

* Disable spontaneous further self-extension.
* Pin the candidate snapshot.
* Run each task from a fresh workspace.
* Reset task-local memory.
* Preserve the exact model-harness profile.
* Preserve evaluator, timeout, retries, and capability envelope.

Evaluate adaptive in-session self-extension separately as its own benchmark condition.

---

# 24. Evaluation partitions

Use at least:

## Deterministic contracts

Visible during development. Covers runtime, storage, ABI, security, recovery, composition, and exact event ordering.

## Diagnostic/search set

Full traces visible to the proposer. Used to mine recurrent failure mechanisms.

## Promotion-validation set

Task-level traces hidden from the proposer. Used repeatedly for candidate acceptance.

Do not call this the final held-out test once repeated selection decisions depend on it.

## Replay/retention set

Includes:

* Previously passing tasks.
* Prior hard failures.
* Tasks that regressed under related candidates.
* ABI and runtime canaries.
* Context-recovery canaries.

## Sealed in-distribution audit

Never run during current-campaign search. Open only after campaign completion.

## Sealed out-of-distribution audit

Different repositories, task families, generators, or model profiles. Never feed results back into the current campaign.

Split by repository, task family, and generator family. Do not randomly separate near-duplicate variants across search and sealed partitions.

---

# 25. Candidate gates

Run candidates through:

## Gate A — static validity

* Snapshot builds.
* Source parses.
* Imports close.
* Resource limits pass.
* Tool schemas compose.
* Capability diff is empty for automatic activation.
* Protected prompt sections remain unchanged.
* Complexity ceilings pass.

## Gate B — deterministic Tea contracts

Run all storage, recovery, Luau, compaction, artifact, event, cancellation, and security fixtures.

Any failure rejects the candidate.

## Gate C — trace replay

Replay recorded hook inputs and context projections to catch:

* Non-determinism.
* Schema breakage.
* Projection violations.
* Invalid stop behavior.
* Missing artifact locators.

Trace replay may reject a candidate. It cannot establish behavioral improvement.

## Gate D — targeted diagnostic tasks

Run tasks corresponding to the candidate’s declared failure clusters.

## Gate E — replay and retention

Run previously passing and previously regressed cases.

## Gate F — paired promotion validation

Run parent and candidate:

* On identical task IDs.
* With identical profiles and budgets.
* In interleaved randomized order.
* With repeated attempts when inference is stochastic.

## Gate G — composite validation

When combining individually accepted candidates, build a new immutable composite snapshot and evaluate it from the beginning.

Never assume independently useful components compose additively.

## Gate H — canary/global promotion

Capability-neutral profile-specific candidates may enter a canary lane after all gates.

Any candidate that:

* Expands authority.
* Alters evaluator behavior.
* Changes model selection.
* Changes global resource ceilings.
* Modifies Rust code.

requires a separate human-reviewed engineering path.

---

# 26. Promotion statistics

Do not use one noisy pass-count flip as sufficient evidence.

For deterministic outcomes, use exact paired task comparisons.

For stochastic outcomes:

* Pair parent and candidate by task and attempt block.
* Interleave execution to reduce provider-time drift.
* Report task-level deltas.
* Use a paired bootstrap or a clearly documented hierarchical comparison.
* Report uncertainty.
* Define an explicit non-inferiority margin.
* Define a maximum acceptable regression probability.

Conceptual predicate:

```text
promotable(candidate) =
    hard_contract_regressions == 0
    and capability_expansion == 0
    and quality_noninferiority_passes
    and at_least_one_material_improvement
    and cost_within_budget
    and latency_within_budget
    and cache_disruption_within_budget
    and complexity_within_budget
```

Use Pareto dominance over:

* Task correctness.
* Cost.
* Latency.
* Model-visible context.
* Cache disruption.
* Plugin source size.
* Hook execution time.
* Memory footprint.

When quality is statistically equivalent, prefer the smaller, cheaper harness.

Add a validation-exposure budget. End the campaign or rotate to a fresh promotion shard after too many candidate comparisons against one validation set.

---

# 27. Prevent harness accretion

Record for every snapshot:

```text
static system-prompt bytes and estimated tokens
tool-description/schema bytes and estimated tokens
Luau source bytes
plugin count
tool count
hook count
memory seed bytes
model-visible overhead per request
hook execution time
compaction overhead
projection overhead
provider-surface digest changes
```

Enforce hard ceilings.

Run periodic distillation candidates whose explicit objective is to:

* Delete redundant instructions.
* Merge overlapping plugins.
* Remove unused hooks.
* Replace repeated prose with executable policy.
* Remove components that fail leave-one-out contribution tests.

After a winning merged snapshot, run leave-one-component-out ablations. Remove components that do not materially contribute.

Append-only prompt growth is not an acceptable evolution strategy.

---

# 28. Compute-matched baselines

Every evolution campaign must compare against:

1. No-op candidate control.
2. Independent best-of-N attempts.
3. Sequential self-refinement.
4. Fixed-harness retries with the same verifier feedback.
5. The initial harness at equivalent total inference budget.

Report:

```text
search cost
candidate evaluation cost
final serving cost per task
final latency per task
quality at fixed serving budget
break-even task count
```

Calculate:

```text
evolved_total(T) =
    search_cost + T * evolved_serving_cost

baseline_total(T) =
    T * baseline_serving_cost
```

Do not claim harness evolution is economically superior until it beats appropriate test-time scaling at a realistic amortization horizon.

---

# 29. Laguna S 2.1 live evaluation

Add a pinned live-evaluation lane for:

```text
poolside/laguna-s-2.1:free
```

Use the existing provider abstraction and OpenRouter integration. Do not add a local model backend.

The endpoint accepts tool definitions and `tool_choice`, but does not enforce `response_format`. Therefore:

* Evaluate real tool calls.
* Inspect actual files.
* Validate actual snapshots.
* Validate event ordering.
* Run executable task verifiers.
* Never accept a model’s textual claim that it created or fixed a plugin.

Separate:

* Provider/rate-limit failure.
* Transport failure.
* Model failure.
* Harness failure.
* Verifier failure.

Use bounded retry with jitter for operational rate limits, but do not turn a model failure into a transport retry.

Because the free endpoint may use inputs and outputs for model improvement, use public, synthetic, or explicitly approved evaluation repositories. Do not upload private code or credentials.

The current Tea live quality documentation uses Laguna XS 2.1. Preserve that lane if useful and add an explicit S 2.1 lane rather than silently changing its meaning.

## 29.1 Prompt experiment arms

Run at least:

```text
A. self-extension off

B. current authoring behavior
   current prompt and current authoring tools

C. concise addendum
   stable concise prompt
   automatic safe rollover
   full handbook on demand

D. inline handbook
   longer system-prompt instructions
   automatic safe rollover
```

Hold constant:

* Model.
* Provider.
* Task order.
* Tool schemas except where the arm explicitly varies them.
* Thinking level.
* Timeouts.
* Retry policy.
* Workspace fixtures.
* Evaluator.
* Compaction policy.
* Capability envelope.

Golden-test the exact prompt bytes used by each arm.

## 29.2 Live authoring tasks

Include:

1. Create a valid prompt-only session plugin from an empty local registry.
2. Create a `before_tool` verification guard.
3. Create a multi-module plugin with a declared relative import.
4. Repair an invalid manifest after rejected activation.
5. Repair a syntax-invalid Luau source after rejected activation.
6. Inspect the detailed handbook only when needed.
7. Roll back a behaviorally harmful plugin.
8. Resume a session and continue using the exact restored plugin tree.

## 29.3 Behavioral-transfer sequences

Create two-stage cases:

```text
task A exposes a reusable failure
agent authors a narrow plugin
task B exercises the same mechanism in another repository or task family
```

Target mechanisms such as:

* Finishing without running the narrowest relevant validator.
* Repeating the same failed command without changing strategy.
* Reading many files indefinitely without transitioning to implementation.
* Editing generated output instead of its source.
* Losing required environment state between shell invocations.
* Misinterpreting a bounded tool-result preview while the full error is recoverable.
* Repeating a non-replay-safe operation after restart.
* Continuing after a tool result explicitly requested termination.

## 29.4 Negative controls

Include:

1. A straightforward task needing no plugin.
2. A one-off obscure build failure.
3. A task where a verification plugin would waste substantial work.
4. A failure caused by model capability rather than policy.
5. A synthetic Rust-core defect that must be classified `RustCoreGap`.
6. A task-specific fact that must not be encoded as a global/session plugin.
7. A candidate that requests additional capability and must not auto-activate.
8. A plugin that compiles but degrades unrelated behavior.

Track unnecessary mutation as a primary negative metric.

## 29.5 Persistence and compaction cases

Include:

* Create plugin, compact, continue, and verify behavior.
* Create plugin, terminate Tea, resume, and verify exact snapshot ID.
* Crash after every harness activation boundary.
* Change global plugin files after session creation and verify session pinning.
* Fork a session branch and verify independent harness revision heads.
* Read a pruned middle error after compaction.
* Search pre-compaction history through `tea_history_search`.
* Restore with a missing source object and fail closed.
* Restore with a corrupted `HEAD` cache and reconstruct from durable state.

## 29.6 Metrics

Record:

```text
task pass rate
valid-plugin rate
activation success rate
unnecessary mutation rate
turns to first valid candidate
tokens to first valid candidate
candidate source bytes
candidate changed lines
targeted-cluster improvement
promotion-validation regression
sealed ID and OOD performance
tool-schema deviation rate
runtime/plugin failure rate
capability violation count
rollover count
resume equivalence
artifact recovery success
history-search success
deterministic common-prefix bytes
provider cache-read tokens
provider cache-write tokens
total input/output tokens
latency
reported cost
search cost
serving cost
```

---

# 30. Security and fault behavior

Implement and test:

* Private session-directory permissions.
* Canonical path validation.
* Symlink refusal.
* Single-writer lock.
* No credentials in prompt, events, telemetry, candidate manifests, or source snapshots.
* Redaction before model-readable artifact persistence.
* Exact capability-envelope checking.
* Grants bound to snapshot digest.
* No active pointer writable by Luau.
* No evaluator files visible to the target agent.
* No sealed test results visible to the proposer.
* No operation record model projection.
* No automatic capability expansion.
* Fail-closed active-snapshot restoration.
* Faulted harness stops all new effects.
* Explicit reopen required after underlying storage repair.
* Orphan object GC that never deletes reachable artifacts.
* Session export that includes every referenced immutable object.
* Session verification that recomputes every digest and reducer fixed point.

Do not give session-local Luau source the same trust as arbitrary shell access. It remains hermetic and capability-bound.

---

# 31. Garbage collection and quotas

Implement reference-aware GC.

Roots include:

* Every active session branch.
* Every session entry’s artifact references.
* Every active or rollback harness revision.
* Every retained candidate.
* Every trace retained by an experiment.
* Every evaluation decision.
* Every global promoted profile.

GC may remove:

* Unreferenced temporary objects.
* Abandoned invalid candidates after configured retention.
* Expired unpinned trace artifacts.
* Reconstructible worktree projections.

GC must never remove an object still reachable from a session export or active experiment.

Add configurable quotas and visible diagnostics. Do not silently delete active-session evidence merely because it is old.

---

# 32. Testing strategy

Use Tea’s existing fixture-first rule: add the deterministic fixture that defines a new contract before implementing the contract.

## Tier A — pure reduction and recovery

Construct durable prefixes directly through public session APIs, reopen, reduce, resume, and assert the final durable state.

Cover:

* Every tool crash state.
* Safe/never/changed replay declaration.
* Every source-order position in a parallel batch.
* Missing initial messages.
* Provider attempt caps across reopen.
* Failed/discarded provider response usage.
* Pending and cancelled queues.
* Deferred writes.
* Abort before and after every durable point.
* Harness activation crash matrix.
* Compaction transaction prefixes.
* Half-completed recovery run through recovery twice.
* Missing artifacts and missing harness blobs.
* Invalid revision parent.
* Multiple open operations.
* Provisioned ID collision.

Run the same backend-neutral cases against:

* In-memory reference backend.
* JSONL v4 backend.

SQLite is not required for the initial completion. The storage trait must not preclude it later.

## Tier B — writer conformance

Run the real harness against an instrumented store and record:

```text
E = semantic entry
R = operation record
L = lane move
F = global fact
H = hook
A = artifact write
P = provider effect
T = tool effect
X = activation
```

Assert exact order for:

* Simple model response.
* One-tool run.
* Tool block.
* Tool schema failure.
* Tool retry-safe recovery.
* Tool non-replay-safe interruption.
* Parallel tool batch.
* Provider retry.
* Provider overflow and compaction.
* Abort during provider.
* Abort during tool.
* Self-extension rollover.
* Rollback.
* Compaction after rollover.
* Final operation settlement.

The critical assertion is that no provider, tool, artifact-dependent projection, or activation effect begins before its required durable intent.

## Tier C — manual effect gate

Drive the production procedure one effect at a time.

For every effect:

1. Stop before the effect.
2. Verify the durable prefix.
3. Close the process.
4. Reopen.
5. Resume.
6. Verify the result.
7. Repeat with death immediately after the effect but before the next durable write.

Assert:

* Stable `peek_action`.
* Exact `execute_action`.
* No work while parked.
* One terminal operation outcome.
* Fixed-point reducer/live-state equality.
* Faulted append leaves a valid prefix.
* Recovery twice remains idempotent.

## Artifact projection fixtures

Test:

* Error exactly in the omitted middle.
* Recovery by literal search.
* Recovery by byte range.
* UTF-8 boundaries.
* Prefix locator survives another head truncation.
* Artifact read does not spill recursively.
* Redaction.
* Storage failure fallback.
* Storage failure fault.
* Compaction retains history search.
* Artifact GC reachability.
* Duplicate content deduplication.
* Exact projection strategy identity.

## Harness fixtures

Test:

* Valid single-file plugin.
* Valid multi-file plugin.
* Undeclared import.
* Escaping import.
* Symlink.
* Duplicate path.
* Case-fold collision.
* Duplicate tool.
* Reserved tool.
* Resource limit.
* Capability expansion.
* No-op candidate.
* Candidate/active separation.
* Exact session pinning.
* Operator global rebase.
* Rollback.
* Hook-only provider-surface stability.
* Prompt-changing cache invalidation.
* Exact session export and restore.
* Rollover budget.

## Luau hook fixtures

Test:

* Stable registration IDs.
* Duplicate ID rejection.
* Resume-data round trip.
* Idempotent resume.
* Aggregation order.
* Fail-closed `before_tool`.
* Schema-valid normalization.
* Rejection of unknown behavior-changing fields.
* Raw artifact immutability under `after_tool`.
* Context-patch protected invariants.
* Memory proposal validation.
* Capability-bound handler execution.
* Cancellation and resource exhaustion.

## Telemetry fixtures

Assert absence—not merely redaction—of:

* Prompts.
* Model output.
* Tool arguments.
* Tool output.
* Source.
* Headers.
* Credentials.

---

# 33. Performance and cache benchmarks

Add deterministic microbenchmarks for:

* JSONL append and reopen.
* Pure reduction over long sessions.
* Branch context derivation.
* Artifact write/read/search.
* Harness snapshot build.
* Luau compilation and hook execution.
* Context projection.
* Activation rollover.
* Session export/verify.

Add representative long-session fixtures:

* Thousands of semantic entries.
* Thousands of operation records.
* Hundreds of tool artifacts.
* Repeated compaction.
* Multiple harness revisions.
* Large cumulative usage ledger.

Measure:

* Reopen time.
* Reduction time.
* Context-build time.
* Resident memory.
* JSONL size.
* Object-store size.
* Model-visible context reduction.
* Deterministic uncached-prefix proxy.
* Actual provider cache metrics where supplied.

Do not accept an implementation that rescans every artifact or all historical tool bytes for every provider request.

Build indexes during JSONL replay and maintain them incrementally.

---

# 34. Implementation sequence

Complete the work in this order. Keep every phase compiling and passing all prior tests.

## Phase 1 — `tea-session`

Implement:

* IDs.
* Entries.
* Records.
* Lanes.
* Pure reducer.
* Memory backend.
* JSONL v4 backend.
* v3 import.
* Torn-tail recovery.
* Corruption validation.
* Session snapshots.
* Single-writer enforcement.
* Fixed-point checks.

Expose only `main`, but keep all persisted APIs lane-keyed.

Gate:

* Backend conformance passes.
* v3 fixtures pass.
* No provider/tool integration yet.

## Phase 2 — durable core execution

Refactor the existing core minimally to support:

* Operation/epoch provenance.
* Durable prepared-effect boundaries.
* Tool intent before execution.
* Provider physical-attempt accounting.
* Host-derived recovery plans.
* Manual effect gate.
* One shared fresh/recovery scheduler.

Do not duplicate tool execution logic.

Gate:

* Complete tool crash matrix.
* Provider retry accounting.
* Cancellation fixtures.
* Existing tea-core tests unchanged or intentionally updated.

## Phase 3 — artifact store and recoverable context

Implement:

* Content-addressed objects.
* Full-result retention.
* Bounded projection.
* Artifact/history tools.
* Recovery locators.
* Compaction integration.
* Redaction policy.
* GC roots.
* Storage-failure behavior.

Gate:

* Middle-error recovery.
* No recursive spill.
* Compaction-history recovery.
* Existing compaction metrics remain correct.

## Phase 4 — immutable harness manager

Implement:

* Trees.
* Snapshots.
* Revisions.
* Candidates.
* Canonical digests.
* Global/session layering.
* Session pinning.
* Candidate validation.
* Activation.
* Rollback.
* Events.
* Worktree projection.

Gate:

* Exact source restoration.
* No-op detection.
* Capability checks.
* All activation crash states.

## Phase 5 — automatic core-run rollover

Implement:

* Durable user operation spanning epochs.
* Pending activation semantics.
* `tea_harness`.
* Sole-tool mutation rule.
* Automatic continuation.
* Rollover budget.
* UI status.
* Resume after crash at every rollover point.

Gate:

* One user task continues across a harness activation without another user message.
* Old core run never sees the new snapshot.
* New core run always sees the committed snapshot.
* Operation has one final outcome.

## Phase 6 — Luau ABI v2

Implement:

* Versioned manifest.
* Named prompt sections.
* Bounded hook adapters.
* Resume data.
* Post-tool projection.
* Typed context patches.
* Typed memory.
* Capability-bound tool handlers.
* v1 compatibility.
* Full handbook.

Gate:

* Adversarial Luau suite.
* Capability isolation.
* Resource-limit behavior.
* Deterministic composition.

## Phase 7 — modes, prompt, and cache discipline

Implement:

* `Off`, `Author`, `Adaptive`.
* Stable prompt composition.
* Model-harness profiles.
* Schema-deviation telemetry.
* Provider-surface fingerprints.
* Cache evidence.
* Golden prompt/schema fixtures.

Gate:

* `Off` has zero authoring prompt/tool overhead.
* Dynamic plugins appear only after stable prefix material.
* Hook-only changes preserve provider surface.
* Prompt/tool changes report explicit cache-domain change.

## Phase 8 — evaluation and evolution

Implement:

* Experiment lock.
* Candidate manifests.
* Evidence store.
* Failure signatures.
* Search/proposal workflow.
* Staged gates.
* Paired comparison.
* Pareto frontier.
* Rejected lineage.
* Global profile promotion and rollback.
* Compute-matched controls.
* Laguna S 2.1 live lane.

Gate:

* A complete campaign is reproducible from one experiment directory.
* The target agent cannot alter scores or promotion.
* A rejected candidate cannot become active globally.
* A winning merged candidate is independently reevaluated.
* Search and serving costs are both reported.

## Phase 9 — future lanes, only after all single-lane gates

Prepare and document, but do not destabilize the completed single-lane system merely to claim concurrency.

Full multi-lane support may then add:

* Lane creation.
* Shared immutable branch prefixes.
* One operation per lane.
* Parallel core agents.
* One session writer.
* Shared sequence allocation.
* Independent suspension and recovery.
* Per-lane harness revision heads.
* Cross-lane artifact deduplication.

Do not expose multi-lane UI until its crash and concurrency matrix is complete.

---

# 35. Verification commands

Run the repository’s actual current commands after inspection. At minimum, preserve and run the equivalents of:

```sh
make test
git diff --check
python3 -m evals.quality fast
python3 -m evals.quality resources
python3 -m evals.quality compaction
```

Add focused commands for:

```text
durable session fixtures
recovery crash matrix
artifact projection
harness activation
Luau ABI v2
cache invariants
evolution contracts
Laguna S 2.1 live evaluation
```

Run formatting, linting, and workspace tests.

Do not weaken existing tests, increase arbitrary timeouts to hide races, or bless new snapshots without inspecting the semantic difference.

---

# 36. Required documentation

Produce:

```text
docs/durable-harness.md
docs/session-format-v4.md
docs/harness-recovery.md
docs/artifact-recovery.md
docs/harness-self-extension.md
docs/luau-abi-v2.md
docs/harness-evolution.md
docs/model-harness-profiles.md
```

Document:

* State ownership.
* Durability guarantees.
* Non-guarantees.
* Crash matrices.
* Replay safety.
* Artifact retention.
* Exact activation semantics.
* Capability boundaries.
* Session export and verification.
* v3 conversion.
* Evaluation methodology.
* Global promotion process.
* Operational recovery from corruption.
* Why active-run in-place reload is prohibited.

Include a small, correct example plugin and its expected snapshot composition.

---

# 37. Completion report

At completion, provide:

1. Architectural summary.
2. Final crate/module map.
3. Session v4 format summary.
4. Record and event catalog.
5. Exact guarantees and non-guarantees.
6. All commands run.
7. Deterministic test counts.
8. Crash states covered.
9. Performance measurements.
10. Laguna S 2.1 experiment matrix and results.
11. Any live-provider operational failures, separated from model failures.
12. Prompt variant selected and why.
13. Provider-cache evidence.
14. Candidate promotion example.
15. Remaining explicitly deferred work.

Do not claim the live Laguna gate passed unless it actually ran against `poolside/laguna-s-2.1:free` and its executable validators passed.

Do not claim exactly-once tool effects.

Do not claim lossless model recoverability when artifact storage failed.

Do not claim a session is reproducible unless all referenced source, artifact, profile, evaluator, and environment identities are present and verified.

---

# Final engineering standard

The finished system should make this true:

> A Tea process may die after any durable write, provider request, tool execution, hook, artifact write, compaction boundary, or harness activation boundary. On reopen, Tea either reconstructs the exact accepted state and continues from the last safe boundary, or faults with a precise corruption/ambiguity diagnosis. It never silently invents success, loses an accepted operation, activates an unvalidated plugin, or asks the model to remember state that the durable runtime should own.

And this:

> A Tea agent may author a bounded session-local Luau improvement, but the edit is only a candidate until Rust validates it. The current core run settles under its original immutable snapshot, the new snapshot activates durably, and the same user operation continues automatically under the new snapshot. Every byte of source, every activation, every trace, every score, and every promotion decision remains attributable and reproducible.

And this:

> Model context may be compact and aggressively projected, but relevant execution evidence remains searchable and recoverable outside context. Compaction changes what the model sees now; it does not erase what the session knows.
