# Session recovery

Opening a session restores committed history and an interruption report. It does
not invoke a model, tool, goal, child, workspace application, or idle callback.
Working provider credentials are unnecessary for restoration and inspection.
The terminal's `/resume` picker opens a saved conversation; `/continue` is a
separate explicit execution action.

## Read before executing

`JsonlSession::inspect` validates committed JSONL without acquiring execution
authority. `JsonlSession::open` requires a complete valid prefix and the one
writer lock. An unterminated final envelope requires explicit
`JsonlSession::repair_torn_tail`; corruption in a complete record is not a
torn tail and is never silently erased. A failed creation, rejected writable
open, and completed or failed repair release their locks before returning,
even when a child process briefly inherited a duplicate file descriptor.

`SessionSupervisor::reopen` restores immutable catalog/revision/configuration.
`inspect_recovery(&snapshot)` and `recovery_report()` expose interrupted
lanes and effect identities. Immutable artifacts must be reachable and hash
correctly. The replacement format is unambiguously identified by header
`kind: "tea-session"`, `format: "tea-session-jsonl"`, `version: 1`;
the retired `kind: "session"` format is rejected, never migrated or overwritten.

Accepted queued inputs remain inspectable and withdrawable after reopening.
Goal objective/state survives without execution authorization. Incomplete
assistant/tool previews do not become restored semantic history.

## Explicit continuation

`SessionSupervisor::resume` is caller-polled and runs the same Agent FSM.
It checks unresolved effects before admitting new work. A new prompt follows
the same gate; it cannot bypass indeterminate effects.

| Committed evidence | Explicit continuation |
| --- | --- |
| Complete provider/tool result | Reuse it; never repeat merely because delivery was lost |
| Valid assistant decision without tool admission | Ordinary schema/grant/preparation rules still apply |
| Provider intent without request-material admission | Conclusively uninvoked; retain that distinction and make a fresh attempt |
| Admitted provider request without outcome | Record interruption with unknown usage/billing; make a distinct new attempt |
| Replay-safe tool intent without outcome | Compare current pinned declaration/authority before any permitted repeat |
| Non-idempotent tool intent without outcome | Block with the exact reconciliation requirement |
| Unfinished child | Reconcile its effects/workspace and retain truthful interruption; never restart it |

Cancellation or a killed process does not prove that a mutation did nothing.
Shell commands, edits and workspace applications with unknown outcome cannot be
blindly retried. `reconcile_tool_result(lane_id, result_entry_id, result, evidence)`
lets an explicitly trusted host record the exact observed outcome and bounded
evidence. It rejects cross-lane/call identities and already committed outcomes.
This operation is not exposed as a model tool.

A child with unresolved workspace/effect evidence retains it until reconciled.
An embedding can register fresh explicit lane services when reconciliation
requires a child lane's pinned policy. Provider credentials remain unnecessary
for read-only reports. A new child assignment is required for further execution.
Existing committed reports/deltas remain available; open never applies them.

## Commit guarantees

A semantic batch is one validated, integrity-sealed JSONL envelope. Storage
adopts the new state only after a successful append under its configured sync
policy. An ambiguous failure faults the writer; reopen verifies actual bytes.
Objects publish before references, so interrupted publication may leave an
unreferenced immutable object but never a successful reference to partial bytes.

Accepted append, process-crash recovery and power-loss durability are different
guarantees. Strict synchronization is documented in [session format](session-format-v1.md);
subprocess kill tests do not establish power-loss behavior.

Forking at a recorded settled-turn checkpoint copies conversation state through
that boundary. It does not roll back workspace files or inherit execution
authorization. See [semantics](semantics.md).
