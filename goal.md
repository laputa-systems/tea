## Objective

Add durable, asynchronous subagents to Tea, closely following the useful parts of Codex’s multi-agent v2 design:

* The controlling agent spawns a child through an ordinary tool call.
* The tool returns a durable child handle immediately.
* Children run asynchronously.
* The parent may spawn multiple children, inspect their state, wait for results, interrupt them, and selectively apply their changes.
* Child reports do not appear in the parent context until the parent explicitly asks for them.
* Child repository edits are isolated from the parent and returned as durable deltas.

Simplify the design toward Tea’s existing principles:

* Explicit configuration.
* Deterministic ordering.
* Minimal hidden inheritance.
* One durable session.
* Pure reduction from an append-only log.
* Provider-neutral core.
* Executor-neutral core.
* No detached work.
* Stable, cache-friendly prompt layouts.
* No shared writable worktree between agents.

The feature must be completely optional.

APIs may be broken freely. Do not retain compatibility facades, aliases, deprecated constructors, migration shims, or decoders for obsolete intermediate formats.


---

# Non-negotiable decisions

## 1. Subagents are opt-in

For the terminal application, subagents are controlled by:

```toml
# ~/.tea/config.toml

[features]
subagents = true
```

The default is `false` when:

* `config.toml` does not exist.
* `[features]` does not exist.
* `features.subagents` is omitted.

When disabled:

* No subagent tools are registered.
* No subagent instructions are appended to the root prompt.
* No subagent coordinator is created.
* No child provider factory is initialized.
* No new footer text is rendered.
* Existing root-agent behavior must remain unchanged.
* Existing feature-disabled PTY output must remain visually identical.

This global configuration applies only to the `tea-agent` terminal application, including its interactive and one-shot prompt modes.

It must not affect:

* `tea-core`
* `tea-session`
* `tea-providers`
* `tea-protocol`
* library or SDK users
* session inspection, verification, export, restore, repair, or garbage-collection commands

Library users enable subagents manually by supplying explicit subagent services to the core runtime. No library crate may inspect `$HOME`, `~/.tea`, environment-specific global config, or `config.toml`.

## 2. No compatibility runtime facade

Replace the current single-lane `SessionRuntime` design with the final multi-lane runtime directly.

Do not retain:

* `SessionRuntime`
* an alias named `SessionRuntime`
* a wrapper that forwards to the new runtime
* deprecated constructors
* old session-runtime terminology in current documentation

Use a clean final name such as:

```rust
SessionSupervisor
```

The TUI may define a private concrete application alias such as:

```rust
type HostSession = SessionSupervisor<JsonlSession>;
```

That is an application convenience, not a public compatibility layer.

## 3. One session, many agent lanes

Use one durable Tea session and one serialized session writer.

Represent:

* the root agent as the main lane
* each child agent as another lane in the same session

Each lane has:

* its own active operation
* its own model and thinking configuration
* its own resolved harness identity
* its own prompt-layout ledger
* its own agent instance while active
* its own transcript branch
* its own compaction history

Different lanes may run concurrently.

A single lane may have at most one active operation.

The session writer remains serialized and assigns one global sequence order to all durable mutations.

Do not create a separate session directory per child.

## 4. The controlling model chooses the child model

Do not create named agent profiles such as `explorer`, `worker`, or `reviewer`.

Do not require `~/.tea/agents.json`.

The `spawn_agent` tool exposes a closed model enum. The controlling model selects the model it considers appropriate for the assignment.

The host determines the permitted catalog.

A tool call may never:

* name an unapproved model
* name an arbitrary provider
* provide credentials
* override provider endpoints
* expand its own model catalog
* manufacture capabilities

## 5. One provider family per TUI subagent catalog

The TUI configuration may optionally override the provider and constrain its models:

```toml
[features]
subagents = true

[subagents]
provider = "openrouter"

models = [
    "openai/gpt-5.6-luna",
    "inclusionai/ling-3.0-tiny:free",
]

max_concurrent = 4
max_total_per_operation = 16
timeout_seconds = 900
```

Semantics:

### Effective provider

```text
configured subagents.provider
    or
root session provider
```

All models exposed to `spawn_agent` belong to that effective provider.

### Effective models

When `subagents.models` is present:

* Preserve its declared order.
* Resolve every model through the provider registry.
* Reject duplicates.
* Reject empty model identifiers.
* Reject models unsupported by that provider.
* Expose exactly those models.

When `subagents.models` is absent:

* Expose the checked-in registry catalog for the effective provider in registry order.
* When the root model uses the same provider and is a valid custom model not already in the catalog, append that exact root model.

Do not silently query a remote provider for an unbounded live model list.

### Limits

```text
max_concurrent             default 4, range 1..=16
max_total_per_operation    default 16, range max_concurrent..=64
timeout_seconds            default 900, range 30..=7200
```

`max_concurrent` counts active child operations, excluding the root.

`max_total_per_operation` counts every child spawned by one root user operation, including completed, failed, and interrupted children.

When capacity is exhausted, `spawn_agent` returns a structured recoverable error. Do not silently queue the spawn.

## 6. Persist the effective policy

When a new subagent-enabled session is created, persist:

* enabled state
* effective provider
* ordered allowed model catalog
* model display names
* known context windows
* concurrency limit
* total-spawn limit
* timeout
* exact subagent tool-surface digest

The persisted policy defines that session’s immutable root tool schema and child-model domain.

On reopen:

* A session created without subagents remains without subagents. Enabling global config later does not retrofit tools into it.
* A session created with subagents requires `features.subagents = true` before the TUI may execute it.
* The persisted catalog remains the session’s exact schema.
* The current global config acts as an authorization ceiling:

  * when `subagents.provider` is set, it must match the persisted provider
  * when `subagents.models` is set, every persisted model must still be included
* Current configuration may restrict or reject an old session, but it may never silently expand or rewrite the persisted catalog.
* Read-only session commands remain available even when execution is refused.

## 7. Children cannot spawn children in v1

V1 supports only:

```text
root → child
```

Do not add:

* recursive subagents
* grandchildren
* child-owned collaboration tools
* teams
* role inheritance
* automatic planner hierarchies

Structure the durable graph so depth can be added later without changing the identity model, but reject child spawn attempts in v1.

## 8. No mailbox magic in v1

V1 does not include:

* `send_message`
* `followup_task`
* unsolicited child messages
* automatic child-completion injection
* background messages inserted into parent context
* automatic reuse of an idle child
* nicknames

The parent receives child output only through an explicit `wait_agent` call.

## 9. Children use isolated writable workspaces

Every child receives an isolated snapshot of the parent repository.

Children may use normal Tea coding tools against that isolated workspace.

A child may not directly mutate:

* the parent worktree
* the parent Git index
* the parent checked-out branch
* another child’s worktree

The parent explicitly chooses whether to apply a child’s delta through `apply_agent_changes`.

## 10. No real provider is required for verification

All core, persistence, orchestration, recovery, cache-layout, workspace, and TUI tests must run with:

* scripted providers
* mock providers
* temporary Git repositories
* deterministic clocks where needed
* fault injection

Do not make completion dependent on real credentials or live inference.

---

# Initial repository review

Before changing code, inspect these current areas and their immediate dependencies:

```text
docs/architecture.md
docs/durable-harness.md
docs/cache-friendliness.md
docs/semantics.md
docs/tui.md
docs/default-coding-profile.md

crates/tea-core/src/runtime/
crates/tea-core/src/agent/
crates/tea-core/src/run/tool_execution.rs
crates/tea-core/src/tool.rs
crates/tea-core/src/effect.rs
crates/tea-core/src/measurement.rs
crates/tea-core/src/coding/

crates/tea-session/src/model.rs
crates/tea-session/src/reduction.rs
crates/tea-session/src/store.rs
crates/tea-session/src/jsonl.rs
crates/tea-session/src/verification.rs
crates/tea-session/src/ids.rs

crates/tea-providers/src/registry/
crates/tea-agent/src/app/
crates/tea-agent/tests/pty_streaming.rs
```

Do not perform a broad speculative repository rewrite.

Refactor adjacent code only where required to establish clean multi-lane, provider-factory, workspace, or configuration boundaries.

---

# TUI configuration contract

Add:

```text
<resolved tea home>/config.toml
```

With the normal Tea home this is:

```text
~/.tea/config.toml
```

An explicit `--tea-home` redirects both:

* session storage
* TUI global configuration

This makes tests hermetic.

Use a real TOML parser. Do not hand-roll TOML syntax.

Prefer a small parser that does not require introducing Serde across the workspace. Keep the dependency private to `tea-agent`. Before finalizing, inspect `cargo tree` and ensure no TOML parser reaches the library crates.

Configuration types should resemble:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
struct TuiConfig {
    features: FeatureConfig,
    subagents: SubagentTuiConfig,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FeatureConfig {
    subagents: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SubagentTuiConfig {
    provider: Option<String>,
    models: Option<Vec<String>>,
    max_concurrent: NonZeroU32,
    max_total_per_operation: NonZeroU32,
    timeout: Duration,
}
```

Load with:

```rust
fn load_tui_config(tea_home: &Path) -> Result<TuiConfig, ConfigError>;
```

Requirements:

* Missing file returns defaults.
* Empty file returns defaults.
* Reject symlinks.
* Bound the file to 256 KiB.
* Reject unknown root tables and keys.
* Reject unknown `[features]` keys.
* Reject unknown `[subagents]` keys.
* Reject duplicate keys.
* Reject wrong value types.
* Reject duplicate models.
* Reject empty model strings.
* Reject an explicitly empty model array while subagents are enabled.
* Reject invalid ranges.
* Report the config path and, when supported by the parser, source location.
* Do not create or rewrite the file.
* Do not load it for `tea session ...` commands.

Load the config exactly once during TUI application assembly after Tea home resolution.

---

# Library-level API

Subagent support must be an explicit optional capability of the core runtime.

A suitable public shape is:

```rust
pub struct SessionSupervisorInput<S> {
    pub session: S,
    pub resolver: Arc<HarnessResolver>,
    pub root_identity: HarnessIdentity,
    pub root_services: RuntimeServices,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub subagents: Option<SubagentServices>,
}
```

```rust
pub struct SessionSupervisor<S> {
    // private
}
```

```rust
impl<S> SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    pub fn create(
        input: SessionSupervisorInput<S>,
    ) -> Result<Arc<Self>, HarnessError>;

    pub async fn run_root_prompt(
        &self,
        input: impl Into<String>,
    ) -> Result<DurableOperation, HarnessError>;

    pub async fn resume(
        &self,
    ) -> Result<DurableOperation, HarnessError>;

    pub fn abort_root(
        &self,
    ) -> Result<bool, HarnessError>;

    pub fn snapshot(
        &self,
    ) -> Result<SessionSnapshot, HarnessError>;

    pub fn subscribe_events(
        &self,
    ) -> Result<TeaEventSubscription, HarnessError>;
}
```

The exact naming may be adjusted to fit the current code, but preserve these principles:

* The supervisor owns the session.
* The root lane is not special inside lower-level operation machinery.
* Subagent services are optional.
* No default subagent services exist.
* `None` means the feature is completely absent.

## Subagent model and policy

Add provider-neutral types resembling:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentModel {
    pub descriptor: ModelDescriptor,
    pub display_name: String,
    pub context_window: Option<NonZeroU64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentPolicy {
    pub models: Vec<SubagentModel>,
    pub max_concurrent: NonZeroU32,
    pub max_total_per_operation: NonZeroU32,
    pub timeout: Duration,
}
```

Validate:

* nonempty catalog
* unique provider/model identities
* one provider family across the catalog
* stable declared order
* valid limits
* bounded display names
* bounded identifiers

## Explicit host services

Core must not know how to:

* read global config
* construct provider credentials
* call Smol
* invoke Git
* create directories
* clone worktrees
* apply patches

Use explicit host ports.

A suitable shape is:

```rust
pub struct SubagentServices {
    pub policy: SubagentPolicy,
    pub host: Arc<dyn SubagentHost>,
    pub tasks: Arc<dyn TaskRuntime>,
}
```

```rust
pub trait SubagentHost: Send + Sync {
    fn prepare<'a>(
        &'a self,
        request: PrepareSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent>;

    fn reopen<'a>(
        &'a self,
        request: ReopenSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent>;

    fn finalize<'a>(
        &'a self,
        request: FinalizeSubagentRequest,
    ) -> SubagentHostFuture<'a, WorkspaceDelta>;

    fn apply<'a>(
        &'a self,
        request: ApplyWorkspaceDeltaRequest,
    ) -> SubagentHostFuture<'a, WorkspaceApplyOutcome>;

    fn cleanup<'a>(
        &'a self,
        lease: WorkspaceLease,
    ) -> SubagentHostFuture<'a, ()>;
}
```

```rust
pub struct PreparedSubagent {
    pub workspace: WorkspaceLease,
    pub runtime_services: RuntimeServices,
}
```

Core task ownership should use an executor-neutral port:

```rust
pub trait TaskRuntime: Send + Sync {
    fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<Arc<dyn TaskHandle>, SubagentTaskError>;

    fn sleep(
        &self,
        duration: Duration,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}
```

```rust
pub trait TaskHandle: Send + Sync {
    fn cancel(&self);

    fn join<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}
```

The TUI provides Smol-backed implementations.

No task may be detached from supervisor ownership.

---

# Refactor `HarnessResolver`

The current resolver/runtime relationship was designed around one runtime-service template.

That is insufficient when children may use:

* another model
* another provider adapter
* another compactor
* another workspace-bound coding-tool registry
* another prompt-layout ledger

Refactor so that:

* `HarnessResolver` resolves provider-independent immutable harness state.
* `HarnessResolver` does not own one session-global `RuntimeServices`.
* The supervisor supplies lane-specific `RuntimeServices` when constructing an epoch agent.
* The same immutable harness repository and catalog may serve root and child lanes.
* Host executable authority remains outside persisted harness source.

Do not work around this by hiding a mutable global provider/template inside the resolver.

---

# Root and child harnesses

For a new subagent-enabled session:

1. Resolve the effective model catalog.
2. Build the normal root coding harness.
3. Add the five subagent tools to the root harness.
4. Append the stable root subagent instruction suffix.
5. Build one child harness identity per permitted model.
6. Child harnesses use:

   * Tea v2 coding tools
   * stable artifact/history recovery tools
   * the child instruction suffix
   * no collaboration tools
7. Persist the complete immutable harness catalog before any operation begins.
8. Persist the subagent policy before the root harness revision that refers to its dynamic tool schema becomes active.

Do not synthesize child prompt/tool schemas ad hoc after a spawn call.

The child executable coding tools are instantiated for its physical workspace when the child runs, but their model-facing definitions and harness identities are already immutable.

---

# Model-facing tool surface

The root exposes exactly these five tools, in this order:

```text
spawn_agent
wait_agent
list_agents
interrupt_agent
apply_agent_changes
```

No child exposes these tools in v1.

## Tool scheduling

Use Tea’s existing tool scheduling semantics deliberately.

### `spawn_agent`

* Execution mode: sequential.
* Cancellation settlement: await completion.
* Not exclusive-batch.

This permits multiple `spawn_agent` calls in one assistant batch while preserving source-order snapshot semantics.

For example:

```text
spawn A
spawn B
edit parent
```

creates A and B from the parent state before the edit.

```text
edit parent
spawn A
```

creates A after the edit.

The children themselves run asynchronously after their spawn calls settle.

### `wait_agent`

* Execution mode: sequential.
* Cancellation may terminate the wait.
* No durable side effect merely from timing out.

### `list_agents`

* Read-only.
* Parallel-safe.

### `interrupt_agent`

* Execution mode: sequential.
* Await cancellation settlement.

### `apply_agent_changes`

* Sequential.
* Exclusive-batch.
* Await settlement after application begins.

No sibling tool may run beside a repository application transaction.

---

# `spawn_agent` contract

Schema:

```json
{
  "type": "object",
  "properties": {
    "task_name": {
      "type": "string",
      "description": "Stable unique name for this child within the current root operation."
    },
    "task": {
      "type": "string",
      "description": "Complete, self-contained assignment for the child."
    },
    "model": {
      "type": "string",
      "enum": ["<host-authorized model IDs in stable order>"],
      "description": "Model selected for this child."
    },
    "thinking": {
      "type": "string",
      "enum": [
        "off",
        "minimal",
        "low",
        "medium",
        "high",
        "xhigh",
        "max"
      ],
      "description": "Optional child reasoning level."
    },
    "context": {
      "type": "string",
      "enum": ["task", "parent"],
      "description": "task uses only the assignment; parent forks the exact parent source context."
    }
  },
  "required": ["task_name", "task", "model"],
  "additionalProperties": false
}
```

Additional validation:

```text
task_name:
    ^[a-z][a-z0-9_]{0,63}$

task:
    trimmed nonempty
    maximum 64 KiB UTF-8

thinking:
    omitted means inherit the parent lane's current thinking level

context:
    omitted means "task"
```

Immediate result:

```json
{
  "agent_id": "agent-...",
  "task_id": "operation-...",
  "task_name": "audit_recovery",
  "state": "running"
}
```

The call returns only after:

* the workspace lease exists
* the child lane is durable
* child configuration is durable
* graph linkage is durable
* the child operation is accepted
* the host task runtime has accepted responsibility for driving it

It does not wait for child completion.

---

# Context modes

Support exactly two modes in v1.

## `task`

The child lane begins without inherited semantic history.

It receives:

* its stable child system prompt
* its coding tools
* its logical workspace descriptor
* the explicit assignment

The assignment must be appended as the final variable context item.

This is the default.

## `parent`

Fork the child lane from the exact semantic leaf used as input to the parent assistant step that emitted `spawn_agent`.

Do not fork from:

* the assistant entry containing the spawn call
* the parent’s later lane leaf
* the state at the moment the child future happens to begin
* a dynamically summarized approximation

All spawn calls emitted by one parent assistant response therefore see the same parent context unless earlier sequential tools in that same batch deliberately change the parent repository snapshot.

Add the parent epoch source leaf to typed tool provenance so `spawn_agent` does not infer it from mutable state.

---

# Tool provenance and event attribution

Extend run/tool provenance so every tool can receive:

* session ID
* lane ID
* operation ID
* epoch ID
* source leaf ID
* core run ID
* harness revision ID
* harness snapshot ID
* model-harness profile ID
* provider-surface digest

Replace the current minimal tool context with a typed structure containing this provenance.

Also make agent events lane-aware.

Instead of an unqualified event:

```rust
TeaEvent::Agent(event)
```

use a lane-attributed form such as:

```rust
TeaEvent::Agent {
    lane_id: LaneId,
    event: AgentEvent,
}
```

The TUI must project only main-lane agent events into the root transcript.

Child events may update child state and aggregate accounting, but must never appear as root conversation rows automatically.

---

# `wait_agent` contract

Schema:

```json
{
  "type": "object",
  "properties": {
    "targets": {
      "type": "array",
      "items": {
        "type": "string"
      },
      "minItems": 1,
      "maxItems": 16,
      "description": "Agent IDs or task names returned by spawn_agent."
    },
    "return_when": {
      "type": "string",
      "enum": ["any", "all"]
    },
    "timeout_ms": {
      "type": "integer",
      "minimum": 100,
      "maximum": 600000
    }
  },
  "required": ["targets"],
  "additionalProperties": false
}
```

Defaults:

```text
return_when = "all"
timeout_ms = 300000
```

Rules:

* Resolve all targets before waiting.
* Reject targets outside the current root operation.
* Reject duplicate targets.
* Return immediately when the requested condition is already true.
* Wait using an event-driven notifier, not polling.
* Wake on:

  * child terminal state
  * child interruption
  * parent cancellation
  * timeout
* Return entries in the exact order requested, never completion order.
* Do not mutate the parent context until the tool result is committed.

Example:

```json
{
  "completed": [
    {
      "agent_id": "agent-...",
      "task_id": "operation-...",
      "task_name": "audit_recovery",
      "state": "completed",
      "model": "openai/gpt-5.6-luna",
      "thinking": "high",
      "report": {
        "preview": "Reviewed recovery...",
        "artifact": null
      },
      "changes": {
        "delta_id": "delta-...",
        "changed_paths": [
          "crates/tea-core/src/runtime/supervisor.rs"
        ],
        "patch_artifact": "tea-artifact:..."
      },
      "usage": {
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0
      }
    }
  ],
  "pending": [],
  "timed_out": false
}
```

---

# `list_agents` contract

`list_agents` accepts no required arguments.

Return every child owned by the current root operation, sorted by:

```text
task_name
then agent_id
```

Include:

* agent ID
* task ID
* task name
* model
* thinking
* state
* context mode
* usage totals
* optional workspace delta ID
* changed path count

Do not include:

* full report text
* reasoning
* intermediate messages
* tool outputs
* patch bytes
* temporary paths

---

# `interrupt_agent` contract

Input:

```json
{
  "target": "agent ID or task name"
}
```

Behavior:

* Reject the root.
* Reject a child owned by another root operation.
* Be idempotent.
* Cancel its current operation.
* Abort its live agent instance when present.
* Join its task handle.
* Finalize salvageable workspace changes.
* Preserve the child lane and terminal result.
* Release active concurrency capacity.
* Return the previous and resulting states.

An interrupted child may still produce a durable workspace delta.

---

# `apply_agent_changes` contract

Input:

```json
{
  "delta_id": "delta-..."
}
```

Rules:

* Root-only in v1.
* Apply exactly one durable child delta.
* Exclusive tool batch.
* Do not touch the parent Git index.
* Preflight before mutation.
* Return conflicts without mutating the parent.
* Once application begins, ignore cancellation until the result can be classified honestly.
* Persist a durable applied fact only after a proven committed result.
* Repeating an already committed application returns the existing committed result.
* Never silently reapply an ambiguous crash prefix.

Success:

```json
{
  "delta_id": "delta-...",
  "state": "applied",
  "changed_paths": [
    "crates/tea-core/src/runtime/supervisor.rs"
  ]
}
```

Conflict:

```json
{
  "delta_id": "delta-...",
  "state": "conflict",
  "conflicting_paths": [
    "crates/tea-core/src/runtime/supervisor.rs"
  ],
  "patch_artifact": "tea-artifact:..."
}
```

Indeterminate:

```json
{
  "delta_id": "delta-...",
  "state": "indeterminate",
  "diagnostic": "Repository state requires explicit inspection before retry."
}
```

An indeterminate result is terminal for that application attempt and must not be guessed away.

---

# Durable identity

Use validated opaque IDs following the existing Tea ID pattern.

Add:

```rust
AgentId
WorkspaceLeaseId
WorkspaceDeltaId
```

Derive identities deterministically.

A suitable scheme is:

```text
AgentId =
    hash(
        "tea-agent-id-v1",
        session_id,
        parent_lane_id,
        parent_operation_id,
        spawn tool durable idempotency key
    )

child LaneId =
    "agent-" + AgentId digest

WorkspaceLeaseId =
    hash(
        "tea-workspace-lease-v1",
        AgentId
    )

child OperationId =
    hash(
        "tea-subagent-operation-v1",
        AgentId,
        task digest
    )

WorkspaceDeltaId =
    hash(
        "tea-workspace-delta-v1",
        WorkspaceLeaseId,
        base commit,
        result commit
    )
```

Do not derive correctness-critical identity from timestamps, process-local counters, or random nicknames.

Replaying the same durable spawn tool intent must resolve to the same child.

---

# Durable session model

Update the existing session format in place as v1.

Do not:

* increment to v2
* add an old-v1 migration
* retain a decoder for the intermediate pre-subagent shape
* add compatibility wrappers around obsolete records

Update fixtures and tests to the final v1 contract.

Add first-class correctness-critical facts rather than using generic custom facts.

## Persisted subagent policy

```rust
pub struct SubagentPolicyFact {
    pub schema_version: u16,
    pub models: Vec<SubagentModelRecord>,
    pub max_concurrent: u32,
    pub max_total_per_operation: u32,
    pub timeout_ms: u64,
    pub tool_surface_digest: Digest,
}
```

## Agent spawn linkage

```rust
pub struct AgentSpawnedFact {
    pub agent_id: AgentId,

    pub parent_lane_id: LaneId,
    pub parent_operation_id: OperationId,

    pub lane_id: LaneId,
    pub task_name: String,

    pub model: SubagentModelRecord,
    pub thinking: String,
    pub context_mode: AgentContextMode,
    pub base_leaf_id: Option<EntryId>,

    pub workspace_lease_id: WorkspaceLeaseId,

    pub harness_revision_id: HarnessRevisionId,
    pub harness_snapshot_id: HarnessSnapshotId,
    pub model_harness_profile_id: ModelHarnessProfileId,

    pub spawn_tool_call_id: String
}
```

## Workspace delta

```rust
pub struct WorkspaceDeltaFact {
    pub delta_id: WorkspaceDeltaId,
    pub agent_id: AgentId,
    pub workspace_lease_id: WorkspaceLeaseId,

    pub base_commit: String,
    pub result_commit: String,

    pub changed_paths: Vec<String>,
    pub patch: PayloadRef,
}
```

## Child terminal result

```rust
pub struct AgentTaskFinishedFact {
    pub agent_id: AgentId,
    pub operation_id: OperationId,
    pub outcome: OperationOutcome,

    pub final_entry_id: Option<EntryId>,
    pub report: PayloadRef,
    pub workspace_delta_id: Option<WorkspaceDeltaId>,
}
```

## Applied delta

```rust
pub struct WorkspaceDeltaAppliedFact {
    pub delta_id: WorkspaceDeltaId,
    pub target_lane_id: LaneId,
    pub tool_call_id: String,
    pub changed_paths: Vec<String>,
}
```

Add:

```rust
OperationKind::Subagent {
    agent_id: AgentId,
    parent_operation_id: OperationId,
}
```

The existing child `OperationStarted`, `EpochStarted`, provider, tool, usage, and `OperationFinished` records remain authoritative for execution.

Do not duplicate every derived agent state as another durable transition record.

Derive:

* running from an open child operation
* completed from operation outcome plus terminal result fact
* interrupted from operation outcome
* delta-ready from workspace delta fact
* applied from applied fact

Implement:

```rust
pub fn reduce_agent_graph(
    snapshot: &SessionSnapshot,
) -> Result<AgentGraphReduction, Corruption>;
```

Validate:

* one agent ID maps to one lane
* one lane maps to one agent
* task names are unique within a root operation
* parent operation exists
* child model belongs to persisted policy
* child harness/profile identity is known
* child operation belongs to the bound lane
* terminal result follows operation completion
* workspace delta belongs to the same agent and lease
* applied fact references a known delta
* paths are normalized repository-relative paths
* paths contain no NUL
* paths contain no `..`
* paths are unique and deterministically ordered

Update:

* JSONL encoding and decoding
* pure reducers
* verification
* artifact reachability
* garbage-collection roots
* export and restore
* tests

---

# Session supervisor architecture

Delete the monolithic single-lane runtime file and split responsibilities.

A suitable layout is:

```text
crates/tea-core/src/runtime/supervisor/mod.rs
crates/tea-core/src/runtime/supervisor/lane.rs
crates/tea-core/src/runtime/supervisor/operation.rs
crates/tea-core/src/runtime/supervisor/recovery.rs

crates/tea-core/src/runtime/subagents/mod.rs
crates/tea-core/src/runtime/subagents/types.rs
crates/tea-core/src/runtime/subagents/host.rs
crates/tea-core/src/runtime/subagents/coordinator.rs
crates/tea-core/src/runtime/subagents/tools.rs
```

## Session-wide state

`SessionSupervisor` owns:

* the serialized session writer
* artifact store
* harness repository/resolver
* lane map
* durable agent graph projection
* optional subagent coordinator
* process-local event fanout
* session publication lock
* root lane identity

## Lane-local state

Each `LaneRuntime` owns:

```rust
struct LaneRuntime {
    lane_id: LaneId,
    active: AtomicBool,
    active_agent: Mutex<Option<Agent>>,
    thinking_level: Mutex<ThinkingLevel>,
    runtime_services: RuntimeServices,
    prompt_layout_ledger: Arc<PromptLayoutLedger>,
}
```

The exact field split may vary, but these must not remain session-global:

* active operation claim
* active agent
* model provider
* model descriptor
* thinking level
* compactor
* automatic compaction policy
* workspace-bound coding tools
* prompt-layout predecessor

## Generic lane execution

Refactor current root-only operation machinery so lower-level methods take a lane explicitly.

Eliminate literals such as:

```rust
LaneId::main()
```

below the public root entry points.

The root methods select the main lane.

Child methods select their own lane.

Different lanes may drive concurrently while all durable appends serialize through the one session writer.

---

# Subagent coordinator

Use process-local coordination derived from durable state.

A suitable internal shape is:

```rust
struct SubagentCoordinator {
    policy: SubagentPolicy,
    host: Arc<dyn SubagentHost>,
    tasks: Arc<dyn TaskRuntime>,

    state: Mutex<CoordinatorState>,
    activity: ActivityNotifier,
}
```

```rust
struct CoordinatorState {
    by_id: BTreeMap<AgentId, AgentRuntimeState>,

    by_task_name:
        BTreeMap<(OperationId, String), AgentId>,

    handles:
        BTreeMap<AgentId, Arc<dyn TaskHandle>>,

    active_count: u32,

    total_by_root_operation:
        BTreeMap<OperationId, u32>,
}
```

The in-memory map is not authoritative.

It must be rebuildable from:

* the session snapshot
* the agent graph reducer
* durable workspace metadata
* the host’s reopen operation

Use an event-driven activity notifier with:

* a monotonic generation counter
* retained wakers
* cancellation checks
* no polling loop
* no Tokio type

---

# Spawn transaction

The durable spawn sequence must be explicit.

```text
parent ToolStarted(spawn_agent)
    ↓
deterministic agent and lease IDs derived
    ↓
host prepares/reuses isolated workspace lease
    ↓
child lane created
    ↓
child ModelChanged appended
    ↓
child ThinkingChanged appended
    ↓
child HarnessRevisionChanged appended
    ↓
AgentSpawnedFact appended
    ↓
child OperationStarted appended
    ↓
child assignment UserMessage appended
    ↓
host TaskRuntime accepts the child drive
    ↓
parent spawn_agent tool result settles
```

Crash recovery must be deterministic at every prefix.

Make `spawn_agent` durably replay-safe.

If replay sees:

* an existing lease, reuse it
* an existing lane, validate it
* an existing agent fact, validate it
* an existing child operation, return its existing handle
* an already terminal child, return its terminal handle state

Never create a second child for the same durable spawn intent.

---

# Child completion transaction

The terminal child sequence must be:

```text
child final assistant entry
    ↓
child OperationFinished
    ↓
workspace finalized
    ↓
patch artifact committed
    ↓
WorkspaceDeltaFact committed when changes exist
    ↓
report retained inline or as artifact
    ↓
AgentTaskFinishedFact committed
    ↓
worktree cleanup
    ↓
wait_agent may expose the result
```

Do not expose a child as completed to the parent before its report and optional delta are durable.

Concurrency capacity is released when the terminal result is durable, not when the parent waits for it.

---

# Root structured concurrency

One root user operation owns every child it spawns.

Before the root operation can finish normally, fail, or abort:

1. Reject further spawns for that operation.
2. Identify all active children owned by it.
3. Request child cancellation.
4. Abort each live child agent.
5. Join every child task handle.
6. Finalize salvageable child workspaces.
7. Commit each child terminal result.
8. Clean operational worktrees.
9. Only then commit root `OperationFinished`.

No child task or worktree may outlive its owning root operation unnoticed.

A completed child does not consume an active concurrency slot.

A durable child lane remains inspectable after its task finishes.

---

# Child report retention

The settled final assistant response is the child report.

Always create `AgentTaskFinishedFact`.

Retention rules:

* Inline reports up to 32 KiB UTF-8.
* Larger reports become immutable artifacts.
* Parent-facing preview is at most 16 KiB using Tea’s deterministic middle truncation.
* Patch bytes are always artifacts.
* `wait_agent` never includes:

  * child reasoning
  * intermediate assistant messages
  * child tool outputs
  * child trace contents
  * complete oversized reports
  * raw patch bytes

Include aggregate child usage across:

* provider turns
* retries
* compaction
* discarded responses where already accounted by Tea

Interrupted or failed children may still return a report and workspace delta.

---

# Git workspace isolation

Implement the concrete local workspace host in `tea-agent`.

V1 requirements:

* The workspace must be within a Git worktree.
* Git must be available.
* Submodules are rejected in v1.
* Git operations run off the async executor’s main polling path.
* The user’s Git index must not be modified.
* The user’s checked-out branch and refs must not be moved.
* Ignored files are excluded.
* Dirty tracked files are included.
* Tracked deletions are included.
* Untracked nonignored files are included.
* Binary files are supported.
* Renames are represented through the resulting tree/patch.

Store operational child workspaces beneath the durable session directory rather than in a global temporary directory.

## Snapshot construction

Use a private temporary Git index.

Conceptually:

```bash
repo_root=$(git rev-parse --show-toplevel)
head=$(git rev-parse --verify HEAD)
```

For an unborn repository, begin with an empty index.

Using `GIT_INDEX_FILE=<private-index>`:

```bash
git read-tree "$head"
git add -A -- .
base_tree=$(git write-tree)
base_commit=$(git commit-tree "$base_tree" -p "$head")
```

Create a hidden durable ref:

```text
refs/tea/sessions/<session-id>/agents/<agent-id>/base
```

Then create a detached child worktree at that synthetic commit.

The synthetic commit must represent the exact spawn-time parent workspace without modifying the real index.

## Child finalization

Using another private index in the child worktree:

```bash
git read-tree "$base_commit"
git add -A -- .
result_tree=$(git write-tree)
result_commit=$(git commit-tree "$result_tree" -p "$base_commit")
```

Retain:

```text
refs/tea/sessions/<session-id>/agents/<agent-id>/result
```

Generate:

```bash
git diff --binary --full-index --no-ext-diff \
  "$base_commit" "$result_commit"
```

Generate deterministic changed paths from Git’s NUL-delimited output.

Store the binary patch in the session artifact store.

Commit the durable workspace delta before deleting the operational worktree.

## Logical versus physical workspace

Coding tools operate on the child’s physical worktree.

Model instructions receive a stable logical workspace label, such as the original repository root.

Never include in a provider request:

* session-directory worktree paths
* temporary index paths
* lease-directory paths
* random suffixes

Refactor `TeaDefaultCodingProfileV2` so prompt composition accepts a stable logical workspace label separately from the physical tool authority.

Do the same for provider-specific host context such as Command Code workspace metadata.

---

# Applying a child delta

Application must preserve the user’s index.

A suitable Git strategy is:

1. Resolve and verify the durable patch artifact.
2. Capture pre-apply bytes/digests for every changed path.
3. Preflight:

```bash
git apply --3way --check --whitespace=nowarn <patch>
```

4. On preflight conflict:

   * return structured conflicts
   * leave the parent unchanged
5. On successful preflight:

```bash
git apply --3way --whitespace=nowarn <patch>
```

Do not use `--index`.

6. Classify the result:

   * every path matches expected applied state → committed
   * every path matches original state → rolled back
   * mixed or unprovable state → indeterminate
7. Append `WorkspaceDeltaAppliedFact` only after proven commit.

Once the apply process begins, cancellation cannot be treated as permission to forget its outcome.

An ambiguous crash before the applied fact must enter explicit recovery-required state. Do not silently run `git apply` again.

---

# Prompt layout and cache friendliness

## One ledger per lane

Never share one `PromptLayoutLedger` across root and child lanes or across two children.

Each ledger’s predecessor is the preceding request in one logical conversation.

Sharing it across agents would produce false rebases and domain transitions.

## Root cache domain

The root subagent-enabled domain includes:

* root system prompt
* ordered root tools
* dynamic model enum
* selected root model
* thinking
* harness revision
* tool presentation
* projection policy
* compaction policy

Changing the allowed child model catalog is a deliberate root prompt-domain change.

## Child prompt layout

Compose child requests in this order:

```text
stable Tea v2 child system prompt
stable ordered coding tool definitions
stable child instruction suffix
optional inherited parent semantic context
stable logical workspace descriptor
explicit assignment as final variable item
```

Do not include:

* `AgentId`
* task ID
* temporary worktree path
* spawn timestamp
* active-agent counts
* task-runtime state
* arbitrary nickname
* random path suffix

Two children using:

* the same model
* the same child harness
* the same context mode
* the same parent source leaf
* different task text

must have an identical request prefix through the item immediately preceding the assignment.

Continue to distinguish:

* deterministic common-prefix evidence
* actual provider-reported cache read/write usage

Never claim a cache hit from a prefix measurement alone.

## Child compaction

Compaction is lane-local.

It may rebase only that child’s prompt ledger.

It must not alter:

* parent transcript
* sibling transcript
* root ledger
* sibling ledger

---

# Stable prompt text

Append this only to a subagent-enabled root harness:

```text
You may delegate independent work to isolated subagents.

Use spawn_agent with a complete assignment and a model selected from the
host-authorized catalog. Subagents do not share your writable working tree.
Their changes are returned as durable deltas and remain invisible here until
you explicitly call apply_agent_changes.

Use wait_agent to retrieve reports. Child output is never inserted into your
context automatically. Delegate only work that can proceed independently; keep
small or sequential work in the current agent.
```

Append this to every child harness:

```text
You are a Tea subagent executing one bounded assignment.

Work only on the assigned task. Your workspace is an isolated snapshot. Your
edits are not visible to the parent until the parent explicitly applies them.

Inspect and test your work normally. Use your final response as a concise
report covering what you found, what you changed, validation performed, and
remaining risks.

You cannot spawn additional agents.
```

Keep these strings stable and fixture-tested.

Feature-disabled root prompt bytes must remain identical to the current baseline.

---

# TUI provider factory

The current TUI is organized around one configured provider and mutable compactor configuration. Replace that with a reusable factory.

A suitable shape is:

```rust
struct ProviderFactory {
    registry: ProviderRegistry,
    local_base_url: Option<String>,
    local_context_window: Option<NonZeroU64>,
    logical_workspace: String,

    cache:
        Mutex<BTreeMap<ModelDescriptor, Arc<ConfiguredProvider>>>,
}
```

Responsibilities:

```rust
fn resolve_subagent_policy(
    &self,
    root: &ModelDescriptor,
    config: &SubagentTuiConfig,
) -> Result<SubagentPolicy, AppError>;
```

```rust
fn configured(
    &self,
    descriptor: &ModelDescriptor,
) -> Result<Arc<ConfiguredProvider>, AppError>;
```

```rust
fn compactor(
    &self,
    configured: &ConfiguredProvider,
) -> Arc<ProviderCompactor>;
```

Requirements:

* Construct adapters lazily.
* Cache by exact provider/model descriptor.
* Do not fetch credentials for models that are never used.
* Keep credentials host-owned.
* Preserve local endpoint configuration.
* Preserve explicit local context capacity.
* Build one immutable compactor per descriptor/provider pair.
* Delete mutable `ProviderCompactor::configure`.
* Do not let the child model mutate provider configuration.

---

# TUI task runtime

Implement a Smol-backed `TaskRuntime` in `tea-agent`.

Do not call `.detach()` for child tasks.

Store the concrete `smol::Task` behind a handle that supports:

* idempotent cancellation
* idempotent join
* exactly-once terminal observation
* cleanup on supervisor shutdown

The supervisor owns every handle.

The TUI event loop remains responsible for polling its own UI work, but child operation ownership belongs to the supervisor/coordinator.

---

# TUI presentation

Keep the v1 UI minimal.

When disabled:

* no visual change

When enabled:

* append a compact footer field such as:

```text
agents 2/4
```

* show `0/N` while idle
* derive counts from supervisor state
* update it in the mutable live tail
* do not commit status chatter into terminal scrollback
* continue showing normal settled root tool rows for:

  * spawn
  * wait
  * interrupt
  * apply
* do not add a new picker or full-screen agent browser in v1
* do not render child transcripts into the root conversation

Aggregate footer usage and cost across all lanes, while preserving per-agent usage in `list_agents` and `wait_agent`.

Ctrl+C and TUI shutdown must cascade through the supervisor and join all active children.

The one-shot prompt path uses the same configuration and supervisor path as the interactive TUI.

---

# Suggested file organization

Do not add a new crate for v1.

## `tea-session`

Create:

```text
crates/tea-session/src/agents.rs
```

Modify:

```text
crates/tea-session/src/ids.rs
crates/tea-session/src/model.rs
crates/tea-session/src/reduction.rs
crates/tea-session/src/store.rs
crates/tea-session/src/jsonl.rs
crates/tea-session/src/verification.rs
crates/tea-session/src/lib.rs
crates/tea-session/src/tests.rs
```

## `tea-core`

Delete:

```text
crates/tea-core/src/runtime/session.rs
```

Create:

```text
crates/tea-core/src/runtime/supervisor/mod.rs
crates/tea-core/src/runtime/supervisor/lane.rs
crates/tea-core/src/runtime/supervisor/operation.rs
crates/tea-core/src/runtime/supervisor/recovery.rs

crates/tea-core/src/runtime/subagents/mod.rs
crates/tea-core/src/runtime/subagents/types.rs
crates/tea-core/src/runtime/subagents/host.rs
crates/tea-core/src/runtime/subagents/coordinator.rs
crates/tea-core/src/runtime/subagents/tools.rs
crates/tea-core/src/runtime/subagents/tests.rs
```

Modify as needed:

```text
crates/tea-core/src/runtime/mod.rs
crates/tea-core/src/runtime/services.rs
crates/tea-core/src/runtime/events.rs
crates/tea-core/src/runtime/context.rs
crates/tea-core/src/runtime/tests.rs
crates/tea-core/src/effect.rs
crates/tea-core/src/tool.rs
crates/tea-core/src/run/tool_execution.rs
crates/tea-core/src/coding/profile.rs
crates/tea-core/src/measurement.rs
crates/tea-core/src/lib.rs
```

## `tea-agent`

Create:

```text
crates/tea-agent/src/app/config.rs
crates/tea-agent/src/app/provider_factory.rs

crates/tea-agent/src/app/subagents/mod.rs
crates/tea-agent/src/app/subagents/host.rs
crates/tea-agent/src/app/subagents/task_runtime.rs
crates/tea-agent/src/app/subagents/workspace.rs
crates/tea-agent/src/app/subagents/git.rs
```

Modify:

```text
crates/tea-agent/Cargo.toml
crates/tea-agent/src/app.rs
crates/tea-agent/src/app/runtime.rs
crates/tea-agent/src/app/durable.rs
crates/tea-agent/src/app/host.rs
crates/tea-agent/src/app/picker.rs
crates/tea-agent/src/app/compaction.rs
crates/tea-agent/src/app/error.rs
crates/tea-agent/src/app/state.rs
crates/tea-agent/src/app/tests.rs
crates/tea-agent/src/render.rs
crates/tea-agent/tests/pty_streaming.rs
```

## Documentation

Create:

```text
docs/subagents.md
```

Update:

```text
docs/architecture.md
docs/core-terminology.md
docs/durable-harness.md
docs/cache-friendliness.md
docs/quickstart.md
docs/semantics.md
docs/tui.md
docs/verification.md
docs/glossary.md
AGENTS.md
```

---

# Implementation sequence

Follow this sequence. Do not stop after scaffolding.

## Phase 1: Capture the baseline and write the contract

Before changing behavior:

* Run existing workspace tests.
* Run current PTY tests.
* Record current feature-disabled prompt/tool fixture hashes.
* Record current TUI screenshots or PTY transcripts used as visual oracles.
* Record the exact existing public `SessionRuntime` references.

Write `docs/subagents.md` first with:

* scope
* non-goals
* configuration
* library boundary
* tool contracts
* lane model
* workspace isolation
* durability ordering
* recovery
* cancellation
* prompt layout
* test requirements

Update architecture and semantics diagrams to the target design.

Acceptance:

* No implementation ambiguity remains around spawn ordering, child completion, apply classification, or feature-disabled behavior.

## Phase 2: Add strict TUI-only configuration

Implement `config.toml` parsing and validation.

Tests must cover:

* missing file
* empty file
* enabled example
* disabled example
* unknown root key
* unknown feature key
* unknown subagent key
* wrong types
* duplicate model
* empty model
* empty model array
* invalid limits
* symlink rejection
* file-size limit
* `--tea-home` redirection
* session commands not loading config

Acceptance:

* `tea-core`, `tea-session`, and `tea-providers` dependency trees contain no TOML parser.
* Feature-disabled TUI behavior is unchanged.

## Phase 3: Add durable policy and agent graph records

Implement IDs, facts, JSONL encoding, reduction, verification, artifact reachability, and GC roots.

Add corruption tests for every invalid cross-reference.

Acceptance:

* New records round-trip through JSONL v1.
* Reopening yields the same agent graph.
* No compatibility decoder exists for obsolete intermediate shapes.
* Verification finds malformed graph, policy, delta, and apply relationships.

## Phase 4: Replace `SessionRuntime`

Perform the multi-lane refactor before adding live subagent tools.

Migrate the root path onto `SessionSupervisor`.

Acceptance:

* Existing root behavior passes unchanged.
* Two test lanes can run concurrent scripted operations.
* Session writes remain serialized.
* Per-lane state and prompt ledgers are independent.
* `SessionRuntime` no longer exists in Rust code.
* No compatibility alias is introduced.

## Phase 5: Decouple harness resolution from runtime services

Remove one-template runtime ownership from `HarnessResolver`.

Make lane-specific services explicit.

Acceptance:

* Root epochs still resolve identically.
* Two child lanes can resolve immutable harnesses while using different providers and workspace tool registries.
* Resolver state remains provider-independent.

## Phase 6: Add host ports and typed provenance

Implement:

* `SubagentPolicy`
* `SubagentServices`
* `SubagentHost`
* `TaskRuntime`
* typed source-leaf provenance
* lane-attributed events

Use fake host and task-runtime implementations in tests.

Acceptance:

* `subagents: None` creates no coordinator and no tools.
* A test tool receives exact lane, operation, epoch, and source-leaf provenance.
* Child events cannot become root transcript events.

## Phase 7: Add stable root and child tool surfaces

Implement and fixture-test:

* dynamic model enum
* five tool schemas
* root prompt suffix
* child prompt suffix
* tool ordering
* execution modes
* cancellation settlement modes

Acceptance:

* Feature-disabled prompt/tool bytes remain unchanged.
* Feature-enabled tool schema is deterministic for an ordered catalog.
* Changing model allowlist produces a deliberate root prompt-domain change.
* Children have no collaboration tools.

## Phase 8: Implement the durable coordinator and spawn

Implement deterministic identity, capacity limits, lane creation, child operation acceptance, and asynchronous drive.

Tests:

* task-context spawn
* parent-context spawn
* invalid task name
* duplicate task name
* disallowed model
* thinking inheritance
* explicit thinking
* active-capacity exhaustion
* total-budget exhaustion
* repeated identical durable spawn
* multiple spawns in one assistant batch
* source-order repository snapshots
* same provider, different models
* alternate configured provider

Acceptance:

* Spawn replay cannot create a duplicate.
* Parent receives a handle immediately.
* Child drive is supervisor-owned.
* Child runs independently after spawn.

## Phase 9: Add wait, listing, interruption, and root cleanup

Implement event-driven waiting and structured cancellation.

Tests:

* wait-any
* wait-all
* already-complete target
* timeout
* wait cancellation
* inverse completion order
* deterministic requested order
* list sorting
* interrupt idempotency
* interrupted child frees capacity
* root abort cascades
* root normal completion cancels unobserved children
* no child task survives root settlement

Acceptance:

* No busy polling.
* No unsolicited parent context changes.
* No detached task remains.

## Phase 10: Refactor TUI provider construction

Add the provider factory and immutable per-model compactors.

Tests:

* provider override
* inherited root provider
* explicit constrained models
* default registry catalog
* custom root model append
* invalid cross-provider model
* lazy credential loading
* provider adapter cache
* local endpoint and context-window preservation

Acceptance:

* Child model choice is exactly bounded by host policy.
* Provider adapters remain host-owned.
* No mutable global compactor is shared across models.

## Phase 11: Implement Git workspace leases

Build the isolated snapshot/finalization path.

Tests must include:

* clean repository
* dirty tracked file
* tracked deletion
* untracked nonignored file
* ignored file exclusion
* binary file
* rename
* two isolated children
* parent byte-identical before apply
* child physical path absent from prompt
* submodule rejection
* unborn repository
* finalization replay
* cleanup replay

Acceptance:

* Parent worktree and index remain unchanged.
* Delta and patch are durable before cleanup.
* Hidden refs retain required objects.

## Phase 12: Implement concrete TUI host and Smol task runtime

Wire:

* model factory
* workspace lease
* child coding tools
* child runtime services
* child compaction
* task ownership
* reopen and cleanup

Add a scripted end-to-end test:

```text
root provider emits spawn_agent
child provider reads and edits isolated workspace
child provider returns final report
root provider emits wait_agent
parent receives report and delta
parent workspace remains unchanged
```

Acceptance:

* No real provider.
* No detached child task.
* Complete result survives session reopen.

## Phase 13: Implement report retention and delta application

Test:

* inline report
* oversized report artifact
* no-change child
* changed child
* interrupted child with changes
* failed child with changes
* clean apply
* successful three-way apply
* text conflict
* binary conflict
* parent index unchanged
* already-applied delta
* indeterminate classification
* cancellation after apply begins

Acceptance:

* Conflict leaves parent unchanged.
* Proven application appends durable applied fact.
* Ambiguous application is never silently retried.

## Phase 14: Implement complete recovery

Fault-inject after every durable spawn and completion boundary.

Required recovery prefixes:

| Prefix                                 | Required action                |
| -------------------------------------- | ------------------------------ |
| Parent spawn tool intent only          | Resume deterministic spawn     |
| Workspace lease exists, no lane        | Reuse lease and create lane    |
| Lane exists, no spawn fact             | Complete graph binding         |
| Spawn fact exists, no operation        | Accept original child task     |
| Child operation open                   | Reattach workspace and resume  |
| Child operation finished, no delta     | Finalize workspace             |
| Delta exists, no terminal fact         | Retain report and finish       |
| Terminal fact exists, worktree remains | Cleanup only                   |
| Apply outcome ambiguous                | Return recovery-required state |

Additional requirements:

* Restore children before a resumed root can wait on them.
* Feature-disabled TUI refuses execution of a subagent-enabled session.
* Read-only session commands still work.
* Missing required live workspace state produces a typed recovery error.
* Export refuses unresolved active workspace leases.
* JSONL reopen is a fixed point.

## Phase 15: TUI and PTY integration

Use the existing PTY suite as the visual oracle.

Tests:

* missing config matches current output
* explicit disabled config matches current output
* enabled idle footer shows `agents 0/N`
* spawn updates live count
* child streaming text never enters scrollback
* child final report appears only in `wait_agent`
* Ctrl+C cancels and joins children
* no worktree remains after shutdown
* resize behavior remains unchanged
* native scrollback behavior remains unchanged
* one-shot prompt uses the same feature path

Acceptance:

* Disabled visuals do not regress.
* Enabled UI remains minimal.

## Phase 16: Cache, trace, and documentation completion

Add tests proving:

* one ledger per lane
* sibling requests are never compared as adjacent
* same child model/context has stable task-last prefix
* worktree path does not affect prompt fingerprints
* model change yields domain change
* child compaction is lane-local
* traces identify lane and agent without leaking task/report/patch content

Update all durable documentation and examples.

Add a library example showing explicit manual enablement:

```rust
let supervisor = SessionSupervisor::create(
    SessionSupervisorInput {
        session,
        resolver,
        root_identity,
        root_services,
        artifacts,
        subagents: Some(SubagentServices {
            policy,
            host: Arc::new(my_host),
            tasks: Arc::new(my_task_runtime),
        }),
    },
)?;
```

Also show disabled use:

```rust
subagents: None
```

Explicitly document that `~/.tea/config.toml` is not an SDK configuration source.

---

# Testing requirements

Use test-first changes for every new contract or bug discovered during implementation.

At minimum, maintain focused suites for:

```text
tea-session:
    policy records
    agent graph
    JSONL round trips
    corruption
    verification
    artifact reachability
    reopen fixed points

tea-core:
    multi-lane execution
    spawn replay
    capacity
    wait ordering
    cancellation
    structured cleanup
    prompt layout
    child result retention
    recovery

tea-agent:
    config parsing
    provider policy resolution
    Git workspace isolation
    patch application
    Smol task ownership
    durable reopen
    PTY visual behavior
```

Do not assert only happy-path final output.

Test intermediate durable prefixes and failure classification.

Use deterministic IDs and clocks in fixtures wherever practical.

---

# Commit discipline

Make coherent commits at architectural checkpoints, for example:

```text
docs: define durable subagent contract
feat(tui): load strict global feature config
feat(session): add durable agent graph records
refactor(core): replace session runtime with lane supervisor
refactor(core): separate harness resolution from runtime services
feat(core): add explicit subagent host ports
feat(core): define subagent tool surface
feat(core): spawn durable child lanes
feat(core): wait for and interrupt child agents
refactor(tui): add reusable provider factory
feat(tui): isolate children in git worktrees
feat(tui): compose child runtime services
feat(core): retain durable child results
feat: apply isolated child changes
feat: recover durable child operations
feat(tui): expose optional subagents
test: lock subagent cache and PTY behavior
docs: finalize subagent and SDK contracts
```

Do not make commits that only add empty scaffolding with no tested behavior.

---

# Final verification

Run from a clean working tree:

```bash
cargo fmt --all --check
```

```bash
rustup run nightly-2026-07-24 \
  cargo clippy \
  --workspace \
  --all-targets \
  --all-features \
  --locked \
  -- \
  --deny warnings
```

```bash
rustup run nightly-2026-07-24 \
  cargo test \
  --workspace \
  --locked
```

```bash
rustup run nightly-2026-07-24 \
  cargo test \
  -p tea-agent \
  --features pty-harness \
  --test pty_streaming \
  --locked
```

Run the existing quality checks:

```bash
make quality-fast
```

Run Linux verification when Docker is available:

```bash
make test-linux
```

Check stale compatibility terminology:

```bash
rg -n '\bSessionRuntime\b|agents\.json|agent_type|agent profile' \
  crates docs AGENTS.md
```

There must be no current implementation reference to:

* `SessionRuntime`
* `agents.json`
* named agent profiles
* `agent_type`

Check dependency containment:

```bash
cargo tree -p tea-core
cargo tree -p tea-session
cargo tree -p tea-providers
cargo tree -p tea-agent
```

The TOML parser must appear only beneath `tea-agent`.

---

# Definition of done

The work is complete only when all of these are demonstrated by tests:

1. Subagents are disabled by default in the TUI.
2. Disabled prompt bytes, tool definitions, and PTY presentation remain unchanged.
3. Global configuration is read only by `tea-agent`.
4. Library users enable subagents only through explicit Rust services.
5. No compatibility `SessionRuntime` facade remains.
6. One session supports concurrent root and child lanes under one serialized writer.
7. The controlling model chooses only from the host-authorized model catalog.
8. The TUI may globally override the provider and constrain model choices.
9. The effective policy is durable and verified on reopen.
10. Spawn replay cannot create duplicate children.
11. Parent-context forks use the exact parent source leaf.
12. Task-context children receive no inherited transcript.
13. Child edits never mutate the parent before explicit application.
14. Sibling children cannot observe each other’s edits.
15. Child reports and patches become durable before parent exposure.
16. Parent-visible wait results follow requested order, not completion order.
17. Child output is never injected into parent context automatically.
18. Root settlement leaves no live child task.
19. Root settlement leaves no unaccounted operational worktree.
20. Clean delta application is durable and idempotent.
21. Conflicting application leaves the parent unchanged.
22. Ambiguous application is reported honestly and never guessed away.
23. Every lane has an independent prompt-layout ledger.
24. Temporary worktree paths do not affect prompt-cache layout.
25. Child compaction remains lane-local.
26. All correctness and recovery tests run without real inference.
27. Documentation accurately distinguishes TUI configuration from the library API.
28. The complete workspace, PTY, lint, and formatting suites pass.
