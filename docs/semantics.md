# Runtime semantics

The durable session, model context and presentation have distinct ownership.
Committed history is authoritative; an Agent's messages are the effective
execution context and a TUI transcript is a disposable projection.

## Identity and settlement

A session has independently executable lanes and one serialized writer.
Each lane permits at most one active operation. A logical operation owns its
Agent run, source-pinned epochs, provider/tool attempts and optional children.
A tool/provider turn is not a user-turn fork boundary.

All terminal paths clear streaming state and pending tool ownership. Cancellation
closes admission, propagates to owned work and joins it. A storage or workspace
transaction that already crossed its non-cancellable boundary settles/classifies
before cancellation is acknowledged. A dropped caller-polled handle requests
cancellation; the host must continue driving/joining owned work.

Completed semantic results commit before completion publication. Observer errors
cannot undo a durable result. Operation results are independent of event queues.
A storage error can leave a durable interruption requiring reopen; speculative
memory never becomes a successful acknowledgment.

## Inputs and explicit execution

`SessionSupervisor::submit_input` durably accepts a stable input identity and
returns its own completion handle. Acceptance alone never performs inference.
`drive_next_input` is explicitly caller-polled. A combined dispatch records
ordered accepted membership and settles all members atomically.

`withdraw_inputs` rejects already dispatched inputs and commits withdrawal
before returning text for local editing. It preserves accepted history.
Pending controls apply before queued user input, then authorized goal
continuation. Failure/cancellation does not authorize another run.
Goals preserve objective/state on reopen but lose process-local execution
authorization. The same rules work without a terminal.

The lower-level Agent steering/follow-up queues remain explicit in-memory
mechanism ports. They do not replace durable accepted runtime input. Steering
drains at safe boundaries; follow-up drains when a run would otherwise stop.
A synchronous, non-vetoing core observer is distinct from a passive runtime
subscription. Observer callbacks must return promptly; a callback panic cannot
alter the already-recorded run outcome. Slow consumers use bounded
subscriptions and resnapshot after explicit lag.

## Commit and recovery

`SessionCommit` groups inseparable entries, records and facts. Prospective
validation precedes storage; memory adoption and semantic publication follow
successful storage. Intent/outcome, accepted membership/placement and terminal
membership settlement have explicit atomic boundaries. Required immutable
objects exist before references. A failed ambiguous append poisons the writer.

Open is passive: restore committed state and report interrupted work, with no
provider/tool/goal/child/apply calls and no credential requirement.
`inspect_recovery` and `SessionSupervisor::recovery_report` are read-only.
A queued input remains queued after reopening.

Explicit `resume` creates a new provider attempt after recording the prior
request's unknown interrupted outcome; unknown billing/usage remains unknown.
It never concatenates an interrupted preview with the new answer. A committed
tool result is not executed again. A tool decision with no admitted intent is
different from an intent whose external result is unknown. Replay-safe tools
require current declaration/grants before an explicit retry.

An indeterminate non-idempotent effect blocks continuation and fresh prompt
dispatch. `reconcile_tool_result` is an explicit trusted-host operation naming
the exact result identity, observed outcome and evidence; it is not a
model-visible permission to retry. Cancellation alone is not evidence that a
shell, edit or workspace application did nothing. No exactly-once claim is made.

Unfinished children retain truthful interrupted evidence and are not restarted.
Further child work requires an explicit new assignment. Existing results/deltas
remain available through the established explicit boundaries.

## Streaming and tools

Provider streams are caller-polled. Text deltas are transient incremental
previews; complete validated assistant tool calls commit before tool execution.
Incomplete/truncated tool arguments do not execute. Provider errors and rejected
transports have distinct typed terminal outcomes.

Tool preparation and schema checks run in assistant/source order. Execution may
overlap where tool policy permits it. Finalization notifications may arrive in
completion order; model context always pairs results in originating call order.
Recovery can retain any committed subset without re-executing it.

Tool failure metadata distinguishes cancellation, invalid arguments, recoverable,
retryable and fatal failures. The circuit breaker counts consecutive matching
retryable signatures; success or a different signature resets the count.
Fatal/terminal results stop further provider work while preserving valid
call/result relationships.

A length stop without tools continues the unfinished response within the live
operation's normal policy. Truncated tool calls receive refusal results before
a recovery turn. This in-process behavior does not authorize recovery on open.

## Context and compaction

Raw history is immutable. Context projection can omit eligible entries or use
committed replacement material without deleting original messages/artifacts.
Large results retain their full inspectable artifact and a bounded model view.
Private extension/child state and presentation notices stay out of model context
unless explicitly projected.

Manual and automatic compaction are transactions. The compactor receives owned
source context, cancellation and explicit request-effect authority. Rust validates
replacement structure, source generation, call/result pairing and configured
bounds before the durable replacement is committed. Invalid, stale, cancelled,
failed or non-shrinking proposals leave previous effective context intact.

Exact request material identifies rendered system instructions, ordered tools,
selected model/revision/thinking and converted context after hooks. Changes take
effect only at safe preparation boundaries. Cache-stable prefixes exclude
incidental IDs, physical worktree names, timestamps and UI state; prompt-layout
ledgers remain lane-local and disposable.

## Forks and extensions

User forks accept only recorded settled-turn checkpoint identities. They reject
anchors with unresolved exchanges or owned unjoined work before creating a
branch. History, configuration, immutable harness revision and private extension
values come from the same historical boundary. Queues, live handles, outstanding
effects, children and goal execution authorization are not inherited.
Fresh branches diverge independently; workspace files do not roll back.

Internal child context inheritance may use an exact parent request source; it is
not a public mid-exchange rewind API.

Each extension owns one bounded whole JSON value. State is private unless
projected and cannot mutate another namespace or native authority. State schema
versions are source-pinned; incompatible candidates fail validation instead of
silently resetting/reinterpreting data. Author/evaluate/activate/rollback/NoChange
retain immutable lineage. Old invocation generations cannot write.

## Observation and terminal projection

Snapshot capture and subscriber registration are atomic. Semantic batches are
ordered and previews are bounded/coalescible, keyed by lane, operation, epoch,
run and message/tool identity. Terminal semantic results supersede previews;
delayed previews cannot revive settled work. Reopen restores no preview truth.

Subscriber queues are bounded. Semantic overflow reports `Lagged`; consumers
must resnapshot. A subscriber cannot make an already committed operation fail.
Reliable input completion never depends on UI events. Core bounded subscribers
also report loss explicitly; there is no unbounded lossless subscription.
Live-preview snapshots, terminal-preview fences, and completed-run fences each
retain at most 64 identities; source-order validation and the requirement that
a preview's run is currently active reject delayed work after a fence is
evicted.

The terminal writes a stable prefix once to native main-screen scrollback and
redraws only the bounded live tail. Resnapshot preserves the emitted-entry
frontier; an explicit conversation switch starts a fresh projection after busy
work is rejected or cancelled/joined. Modals restore the main screen; resize
reflows only mutable content. Untrusted content cannot issue terminal controls.

## Evidence

Contract tests are in `tea-session`, `tea-core`, `tea-luau`, `tea-agent` and
the declarative fixture runner. PTY tests preserve the terminal oracle.
[Verification](verification.md) records the acceptance commands and evidence,
including the distinct offline, live, and platform checks.
