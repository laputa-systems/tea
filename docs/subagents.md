# Durable subagents

Tea subagents are an optional host capability for running bounded child agents
as concurrent lanes in one durable session. The root controls children through
ordinary tools; child output enters the root context only through an explicit
`wait_agent` result, and child repository changes enter the root workspace only
through an explicit `apply_agent_changes` transaction.

This contract keeps the execution kernel provider- and executor-neutral. Core
receives explicit `SubagentServices`; only the `tea-agent` terminal host reads
global configuration, constructs providers and task handles, or invokes Git.

## Scope and non-goals

V1 supports a single durable graph depth:

```text
root lane -> child lane
```

Children are asynchronous, independently compacted lanes with isolated writable
workspaces. A root operation may spawn several children, list them, wait for
their durable results, interrupt them, and selectively apply one returned delta
at a time.

V1 has no grandchildren, roles, named profiles, teams, mailbox, unsolicited
completion injection, automatic child reuse, detached tasks, shared writable
worktree, or remote model discovery. Children do not receive collaboration
tools. One session writer assigns the global sequence order for every lane.

## Terminal configuration

The terminal feature defaults to disabled and is enabled only by the resolved
Tea home's `config.toml`:

```toml
[features]
subagents = true

[subagents]
provider = "openrouter"
models = ["openai/gpt-5.6-luna"]
max_concurrent = 4
max_total_per_operation = 16
timeout_seconds = 900
```

`--tea-home` redirects both the session store and this file. The terminal loads
the file once during application assembly, after resolving Tea home, and never
for `tea session ...` inspection, verification, export, restore, repair, or
garbage-collection commands.

Missing `[features]`, a missing `subagents` key, or a missing file means false.
When false, the terminal constructs no coordinator or child provider factory,
adds no tool or prompt instruction, and renders no new footer field. Existing
prompt bytes, tool definitions, one-shot behavior, and PTY presentation remain
unchanged.

The parser rejects symlinks, files larger than 256 KiB, unknown or duplicate
keys, wrong types, duplicate or empty model IDs, an enabled empty model list,
and invalid limits. Limits are `max_concurrent` 1..=16,
`max_total_per_operation` `max_concurrent`..=64, and `timeout_seconds`
30..=7200.

The effective provider is the configured provider or the root provider. An
explicit model list is preserved exactly after registry validation. Without
one, the checked-in provider catalog is used in registry order, with a valid
same-provider custom root model appended when absent. No remote catalog is
queried. A reopened session uses its persisted catalog; current configuration
may reject or restrict execution but cannot expand or rewrite it.

This file is terminal configuration only. `tea-core`, `tea-session`,
`tea-providers`, and SDK users never inspect `~/.tea/config.toml`.

## Library boundary

Library users enable the capability explicitly when creating a
`SessionSupervisor`. Every supervisor input remains explicit; `Some` installs
the child capability and `None` installs nothing:

```rust,ignore
let enabled_supervisor = SessionSupervisor::create(SessionSupervisorInput {
    session,
    resolver,
    root_identity,
    root_services,
    artifacts,
    rollover_budget: 0,
    subagents: Some(SubagentServices {
        policy,
        host: Arc::new(my_host),
        tasks: Arc::new(my_task_runtime),
    }),
})?;
```

The disabled form supplies the same durable inputs but creates no coordinator,
child provider factory, or collaboration tools:

```rust,ignore
let disabled_supervisor = SessionSupervisor::create(SessionSupervisorInput {
    session: disabled_session,
    resolver: disabled_resolver,
    root_identity: disabled_root_identity,
    root_services: disabled_root_services,
    artifacts: disabled_artifacts,
    rollover_budget: 0,
    subagents: None,
})?;
```

`SubagentHost` owns workspace preparation, reopen, finalization, application,
cleanup, and lane-specific runtime services. `TaskRuntime` owns asynchronous
task spawning, sleeping, cancellation, and joining. Core has no Git, directory,
credential, Smol, Tokio, or global-configuration authority, and no default
subagent service exists.

## Durable lanes and identity

The main lane is the root. Every child is a separate lane in the same session,
with its own active operation, model, thinking level, harness identity,
provider and tools, compactor, prompt-layout ledger, semantic branch, and
compaction history. A lane admits at most one operation; different lanes may
run concurrently while the session writer remains serialized.

`AgentId`, child `LaneId`, `WorkspaceLeaseId`, child `OperationId`, and
`WorkspaceDeltaId` are deterministic hashes of their durable parents and
content. The spawn tool's durable idempotency key participates in `AgentId`, so
replaying one intent resolves to the same child instead of creating another.
Timestamps, process counters, nicknames, and temporary paths never define
correctness-critical identity.

The session persists the enabled policy, ordered full `ModelDescriptor`
identities (provider, model, and optional revision), display metadata, limits,
timeout, and exact collaboration-tool surface digest before a root harness
refers to them. The model IDs exposed in the closed spawn enum are unique even
when revisions differ, and every catalog descriptor belongs to its one
persisted provider family. On reopen, a configured provider must match that
family and a configured model list must contain every persisted model ID; it is
an authorization ceiling, never a way to expand or rewrite the immutable
catalog. The session also persists first-class spawn linkage, workspace delta,
child terminal result, and applied-delta facts. The pure `reduce_agent_graph`
projection validates all graph, policy, harness, operation, lease, delta, and
normalized-path relationships.
`AgentSpawned` and `WorkspaceDeltaApplied` facts are also bound to the exact
explicit fields in the persisted `ToolStarted.effective_args` that authorized
them. An omitted `spawn_agent.thinking` remains an inheritance request rather
than a default-expanded argument: `AgentSpawned.thinking` is the first durable
resolved value and must match the child lane's `ThinkingChanged` entry. The
child operation's assignment must equal the spawn intent's task, so a valid
call ID cannot be reused to bless different durable semantics.

## Root tool contract

An enabled root exposes exactly these tools in order:

```text
spawn_agent
wait_agent
list_agents
interrupt_agent
apply_agent_changes
```

`spawn_agent` is sequential, awaits cancellation settlement, and is not
exclusive-batch. It accepts a unique `^[a-z][a-z0-9_]{0,63}$` task name, a
trimmed assignment of at most 64 KiB, one model from the persisted enum, an
optional thinking level, and `task` or `parent` context. The default context is
`task`; omitted thinking inherits the parent lane's current runtime value. A
host default need not already exist as a parent `ThinkingChanged` entry, so the
spawn fact and child configuration durably record the resolved inherited
value.

`wait_agent` is sequential and cancellable. It resolves 1..=16 unique targets
owned by the current root operation before waiting, uses an event-driven
notifier, and returns entries in requested order. `return_when` defaults to
`all`; `timeout_ms` defaults to 300000 and is bounded to 100..=600000. A timeout
has no durable side effect.

`list_agents` is parallel-safe and returns the current operation's children in
`task_name`, then `agent_id`, order. It returns identities, configuration,
state, usage, and delta metadata, but not reports, reasoning, intermediate
messages, tool output, patch bytes, or physical paths.

`interrupt_agent` is sequential and settles cancellation. It is idempotent,
rejects the root and foreign-operation children, aborts and joins live work,
finalizes salvageable changes, retains the terminal result, and releases
capacity.

`apply_agent_changes` is root-only, sequential, exclusive-batch, and accepts one
durable delta ID. It preflights before mutation, preserves the parent Git index,
ignores cancellation after application starts until the outcome is classified,
and persists an applied fact only after proving the committed state.

## Context and prompt layout

`task` children start without inherited semantic history. Their stable system
prompt and tools are followed by a stable logical workspace descriptor and the
explicit assignment as the final variable context item.

`parent` children fork the exact semantic leaf used as input to the parent
assistant step that emitted the spawn call. Typed tool provenance carries that
source leaf; the coordinator never infers it from a later mutable lane leaf.
Sibling spawn calls from one assistant response therefore share the same
semantic source, subject only to source-ordered parent workspace mutations from
earlier sequential tools.

Every lane owns a distinct `PromptLayoutLedger`. Child requests order their
stable system prompt, ordered tools, child suffix, optional inherited semantic
context, logical workspace descriptor, and task-last assignment. IDs, task
runtime state, physical worktree paths, timestamps, and random suffixes are
excluded. Sibling requests are never treated as adjacent requests in one cache
domain, and compaction rebases only its lane.

## Workspace isolation

The terminal host requires Git and a containing worktree and rejects submodules
in V1. It snapshots the exact spawn-time parent state with a private index,
including dirty tracked files, deletions, and nonignored untracked files. It
writes a synthetic commit and hidden session/agent base ref without modifying
the user's index, branch, or checked-out refs, then creates a detached child
worktree below the durable session directory.

Finalization uses another private index to commit the child's complete result,
retains a hidden result ref, generates a binary full-index patch and sorted
NUL-safe changed paths, stores the patch as an immutable session artifact, and
commits the workspace delta fact before operational cleanup. Coding tools use
the physical child worktree, while provider prompts and provider-specific host
metadata receive only the stable logical parent workspace label.

Before mutation, applying authenticates the retained base and result refs and
recomputes the canonical binary full-index patch and NUL-delimited normalized
path list. It records parent path type, Git mode, and file or symlink-target
content state, then applies the patch in a private-index three-way sandbox
before using a fresh private index for the real worktree application. The user
index, branch, and refs are preserved. `Applied` requires the observed worktree
and private index to match the sandbox's resolved stage-0 state;
matching the child result before the attempt is not enough to prove how those
bytes arrived. A nonmutating failed preflight is `Conflict`, an exact return to
the recorded state is `RolledBack`, and every pre-existing, mixed, crashed, or
otherwise unprovable result is `Indeterminate` and is never silently retried.
Only a prior `WorkspaceDeltaApplied` fact provides the idempotent success path.

## Durable ordering and visibility

A successful spawn commits in this order:

```text
parent ToolStarted
deterministic IDs
workspace lease
child lane configuration and harness identity
AgentSpawned fact
child OperationStarted
child assignment UserMessage
task-runtime acceptance
parent tool result
```

The immediate result is a durable child handle in `running` or its replayed
terminal state. It does not wait for inference completion.

Child completion commits in this order:

```text
final assistant entry
child OperationFinished
workspace finalization
patch artifact and optional WorkspaceDelta fact
inline-or-artifact report
AgentTaskFinished fact
operational cleanup
wait visibility
```

The final assistant response is the report. Reports at most 32 KiB are inline;
larger reports are immutable artifacts. Parent previews are at most 16 KiB via
deterministic middle truncation, patches are always artifacts, and wait results
contain aggregate usage but never reasoning or intermediate child content.
The terminal fact must name the last assistant entry within the exact child
operation interval; an earlier assistant message is not a settled report.
Operational cleanup never precedes retention of the report and any delta it
names, so reopen, verification, collection, and export observe the same terminal
graph after the worktree is gone.

Agent events carry their lane. The terminal projects only main-lane agent
events into the root transcript. Child events may update state and aggregate
accounting but never create root conversation rows automatically.

## Cancellation and structured ownership

One root user operation owns every child it spawns. Before that root operation
can settle, it closes spawning, cancels all live children, aborts active child
agents, joins every task handle, finalizes salvageable work, commits each
terminal result, and cleans operational worktrees. Only then may the root
`OperationFinished` record commit. Completed children remain durable and
inspectable but consume no active slot.

The terminal's Smol adapter retains the concrete root driver and child tasks
behind explicit completion or idempotent cancel-and-join boundaries; it never
detaches them. Ctrl+C, terminal I/O failure, one-shot stdout failure, and normal
shutdown cascade through the supervisor and join all children before returning
success. If a durable effect boundary cannot be safely settled, shutdown
returns a typed recovery error instead of claiming that the root is closed.

## Recovery

Coordinator memory is a cache rebuilt from the session snapshot, the pure
agent-graph reduction, durable workspace metadata, and `SubagentHost::reopen`.
Recovery is deterministic at every spawn and completion prefix:

| Durable prefix | Recovery action |
| --- | --- |
| Spawn intent only | Resume the same deterministic spawn |
| Lease, no lane | Reuse lease and create lane |
| Lane, no spawn fact | Complete graph binding |
| Spawn fact, no child operation | Accept the original child operation |
| Open child operation | Reopen its workspace and resume |
| Finished operation, no delta | Finalize workspace |
| Delta, no terminal fact | Retain report and finish |
| Terminal fact, remaining worktree | Cleanup only |
| Ambiguous apply | Require explicit recovery inspection |

An `apply_agent_changes` `ToolStarted` without its result is an ambiguous apply:
resume returns typed recovery-required without appending a generic tool error or
calling the mutation port again. Children are restored before a resumed root
can wait on them. A terminal with
subagents disabled refuses to execute a subagent-enabled session, while
read-only session commands remain available. Export refuses unresolved active
workspace leases. Missing required live workspace state is a typed recovery
error, not a synthesized clean state.

## Verification requirements

All orchestration tests use scripted providers, fake host/task ports,
deterministic clocks and IDs, fault injection, and temporary Git repositories;
no real inference or credentials are required. Focused evidence covers strict
configuration, policy and graph corruption, JSONL fixed points, concurrent lane
execution, provenance, prompt layout, spawn replay and capacity, wait ordering,
structured interruption and root cleanup, Git isolation and binary deltas,
application classification, recovery prefixes, report retention, feature-off
prompt/tool bytes, and feature-off PTY output.
