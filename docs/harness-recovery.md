# Harness recovery

A Tea terminal session reopens through tea-session and `tea_core::runtime`; it never
reconstructs an in-memory transcript as a source of truth.

1. JsonlSession::inspect can verify the v1 session prefix without mutation.
   JsonlSession::open accepts only a complete validated prefix; an
   unterminated final tail requires the separate explicit
   JsonlSession::repair_torn_tail operation.
2. reduce_lane derives the main lane's semantic state and any required
   recovery plan.
3. HarnessResolver restores its immutable catalog from a
   SessionFact::HarnessCatalog artifact.
4. SessionRuntime::reopen_with_artifact_store resolves the committed
   harness revision, snapshot, model-harness profile, and policy mode from
   durable state.
5. The supervisor completes or safely resumes the next durable operation
   boundary; it does not replay already settled provider or tool effects.

The recovery boundary is deliberately strict. A missing catalog object,
incorrect artifact hash, inconsistent revision, unresolved policy mode, or
unfinished effect that lacks a valid recovery plan is a typed recovery error.
The host must surface that error instead of inventing a fallback configuration.

Candidate identities include every immutable draft field: parent and proposed
snapshot, actor and optional operation/tool identities, hypothesis, changed
paths and surfaces, ordered registry operations, failure/evidence/effect/risk
lists, and capability ceiling. Catalog rehydration recomputes that identity,
so changing any retained candidate evidence is a new immutable candidate
rather than an in-place edit.

The terminal's /session picker lists only durable session directories for the
current workspace. /resume reopens one of those directories. App does not
maintain a second session database or a shadow transcript.
