# Durable runtime

`SessionSupervisor` is the common durable execution boundary for Rust hosts,
one-shot CLI and terminal prompts. It combines one serialized `SessionWriter`,
immutable artifacts and harness lineage with the executor-neutral Agent.

```text
host operations -> SessionSupervisor -> same Agent FSM
                         |
                 atomic SessionCommit
                         |
              JSONL / immutable artifacts
```

## Direct operations

`submit_input` records accepted text and returns an `AcceptedInput`; its
`completion()` is independently reliable. `drive_next_input` consumes accepted
input under explicit host authorization, processing pending controls before
user input and user input before any authorized goal continuation. The host
owns polling; acceptance never secretly spawns work. `withdraw_inputs`
atomically withdraws only inputs that have not been dispatched.

```rust
let accepted = supervisor.submit_input("review the change")?;
let drive = supervisor
    .drive_next_input(IdleAuthorization::UserInputOnly)
    .await?;
let completion = accepted.completion().wait().await;

// A terminal can restore a combined undispatched slot atomically.
let queued = supervisor.submit_input("add this to the same draft")?;
let restored = supervisor.withdraw_inputs(&[queued.id().clone()])?;
```

`QueuedInput` projection comes from `queued_inputs`; no host retains an
authoritative duplicate queue. `dispatch_extension_command` reports either an
immediate `ExtensionCommandAdmission::Applied` or a durable `Queued` control.
Hosts use the same `drive_next_input` call to apply queued controls and make an
explicit headless goal-continuation decision.

`run_root_prompt` is the ordinary convenience path. `resume` explicitly
continues an interrupted operation after the recovery gate. `abort_root`
requests cancellation; `cancel_and_join` and `close` await owned settlement
while the host keeps polling its drive. Snapshot/subscription is observation,
not the operation result channel.

`fork_settled_turn` accepts a recorded checkpoint ID, a fresh lane ID and fresh
host services. It restores the historical configuration/harness/private state
at that boundary without inheriting queued input, children or execution
permission. Conversation branching does not revert files.

## Source-pinned execution

Every epoch resolves its immutable revision through `HarnessResolver`.
Prompts, tools, hooks, grants, artifact policy and model-harness profile remain
pinned throughout that invocation. A candidate may become active only through
a validated durable activation at a safe boundary. Runtime policy identities
are checked against immutable snapshot metadata before Agent construction.

The resolver owns provider-independent source/revision state. Each lane gets
explicit `RuntimeServices`, including its own provider, compactor, tools and
prompt-layout ledger. Tool declaration bytes and host execution/cancellation
policy have separate digests. An extension cannot expand host grants.

When a lane's durable ancestry selects a model through `ModelChanged`, its
`RuntimeServices` descriptor must match the exact provider, model and revision
before create, reopen, lane registration or execution. Historical epochs use
the selection at their recorded source leaf rather than a later lane change.

ABI-v3 extensions contribute bounded tools/prompts/hooks/commands/idle policy
and one private whole state value per namespace. State versions prevent
incompatible source from silently reinterpreting persisted state. Queued goal
pause/clear controls are runtime-owned; headless hosts get the same precedence
as the terminal.

## Effects and observation

The effect gate commits request/tool intent before dispatch and outcome before
successful completion. Complete assistant tool decisions are durable before
their tools run. Required artifact bytes publish before references. Model
context pairs concurrent results in source order.

`TeaEventSubscription` atomically captures committed state plus labelled
bounded live previews and registers for subsequent updates. Preview loss is
allowed; semantic overflow explicitly requires resnapshot. Reliable input
completion remains independent. Trace records diagnostics under an explicit
failure policy and never grants execution authority.

Reopening reconstructs committed state and reports interruptions. It does not
execute or require working provider credentials. See [recovery](harness-recovery.md),
[architecture](architecture.md), [subagents](subagents.md), and
[verification](verification.md).

For caller-owned external persistence, `RuntimeServices::prepare_hosted_epoch`
builds the same Agent with the caller's explicit `EffectGate`. Unsupported
session-dependent policies are rejected. This seam does not pretend an
in-memory run is crash durable.
