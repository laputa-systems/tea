# Deferred work

## Durable multifile edit recovery

The Tea v2 `edit` protocol currently provides complete-precondition validation,
one host transaction boundary, best-effort rollback after ordinary publication
failure, and an honest `Indeterminate` outcome. It deliberately does not claim
crash-atomic multi-file visibility, cross-process exclusion, or replay safety.

Before making interrupted edits replayable, design and prove the following as
one durable contract:

- Carry a typed, host-generated edit invocation ID from the durable effect into
  `ToolContext`. Do not use a provider tool-call ID as an idempotency key. The
  same invocation ID and request digest may return a stored receipt; reusing an
  invocation ID with different request bytes must fail closed.
- Journal explicit `Prepared`, `Publishing`, `Committed`, `RollingBack`,
  `RolledBack`, and `Indeterminate` states. Bind the journal to canonical paths,
  complete preimage/replacement digests, staged filenames, and publication
  progress.
- Flush staged files and the journal before the first target mutation. Define
  the required file and parent-directory synchronization at every durable state
  transition, and retain rollback copies until the terminal receipt is durable.
- Recover unfinished journals before admitting another edit in the same
  workspace. Classify every target as matching its preimage, replacement, or
  neither; converge deterministically to `Committed` or `RolledBack` when the
  evidence permits, otherwise retain an actionable `Indeterminate` record.
- Verify every terminal receipt by re-reading all targets and comparing their
  complete digests. Rename success alone is not sufficient evidence of commit
  or rollback.
- Add workspace-scoped cross-process coordination. Specify lock ownership,
  stale-owner recovery, timeout/cancellation behavior, and the interaction with
  non-Tea writers before selecting a dependency or platform-specific primitive.
- Add fault-injection coverage after every journal write, stage, publication,
  rollback, and directory synchronization. Reopen the adapter after each fault
  and require a deterministic, truthfully classified outcome.

Journal recovery improves resilience but cannot make several ordinary paths
simultaneously visible as one atomic update. If that property becomes required,
evaluate a versioned workspace/tree with a single atomic generation-pointer
switch, and require all readers and writers to resolve through that boundary.
