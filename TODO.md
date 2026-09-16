# TODO — durability, compaction, and host polish

Standalone task list. Each item is completable from this repo's contracts alone
(`docs/architecture.md`, `docs/scope.md`, `docs/compaction.md`,
`docs/persistence-inventory.md`, `docs/provider-adapters.md`,
`docs/subagents.md`, terminal host docs). Ordered by value.

## 1. Compaction decision ring and trace report

Record a bounded in-memory ring of compaction events (decision, trigger,
token estimates, accepted/retained counts, outcome, failure reason).

- Keep the ring small and fixed (e.g. 64 entries); overwrite oldest.
- Record routine no-ops separately from failures or keep them out of the
  failure view so real problems stay visible.
- Mask or truncate provider/model detail before storing display strings.
- Surface `last / last-failed / overwritten-before` in the redacted trace
  report (`tea-trace`), content-free only: counts, stages, reasons, never
  prompts or handoffs.

Done when: cancelled, failed, and overwritten compaction attempts are
inspectable after the fact without reading session payloads, and routine
decisions do not drown failures.

## 2. Harden compaction handoff validation

Validate every proposed compaction replacement before and after the summary
call:

- Reject empty, non-UTF-8, or over-capacity handoffs (`estimated > accepted`).
- Re-check capacity against current fixed overhead plus accepted tokens
  immediately before commit (`candidate over capacity` is a distinct error).
- On capacity failure caused by user-history pressure, retry once with a
  smaller retention target derived from the handoff size
  (`summary_reserve`-style retry), then fail cleanly.
- Persist the compaction source checkpoint before running the summary, so a
  crash during summarization can resume from a known boundary.
- A commit that already succeeded wins over a concurrent cancellation;
  cancellation before commit leaves prior context untouched and is retried
  by the next prompt.

Done when: fixtures cover empty/invalid/over-capacity handoffs, retry on
user-capacity pressure, checkpoint-before-summarize, and
commit-wins-over-cancel.

## 3. Content-preserving compaction receipts

Define how compacted-away tool exchanges stay referenceable without staying
in context:

- Tool arguments move to the immutable artifact store; the compacted
  transcript keeps a handle plus a short excerpt and content hash.
- Tool results become receipts: handle, byte counts
  (`output_bytes / stored_bytes / truncated`), and bounded head/tail
  preview (e.g. 1 KiB each side).
- Cap the number of model-visible archive references (e.g. 64); overflow
  goes behind a source index, not inline.
- The summary header carries `{handle, bytes, sha256}`, removed-turn count,
  compaction count, preserved user messages, and archive handles in a
  documented layout version.

Done when: a compacted session can cite every removed tool exchange by
handle, previews are bounded, and resume after compaction is immutable.

## 4. Non-fatal recovery-checkpoint overflow

Recovery checkpoints must never fail a turn just because tool output is large:

- Define `max_recovery_checkpoint_bytes` as an emergency ceiling.
- When projecting a checkpoint, spill large tool results to the artifact
  store (`tool-results/` style) when they fit `stored_text_max_bytes`;
  keep a preview inline and clear the full output from the checkpoint.
- If a result cannot spill, keep it inline and record a spill-failed event.
- If the encoded checkpoint still exceeds the ceiling, keep the previous
  checkpoint file, emit a `recovery_checkpoint_oversized` event with the
  cap, let in-memory execution advance, and continue the turn.
- Loading validates generation/sequence match and rejects mismatches as
  `InvalidRecoveryCheckpoint`.

Done when: a turn with multi-MiB tool output stays persistable, oversize
checkpoints degrade to keep-previous plus event, and fixtures cover
spill / spill-failure / oversize-keeps-previous.

## 5. Incremental usage ledger

Stop re-parsing and rewriting the whole usage record file on every append:

- Parse the durable usage file once and keep an index across appends:
  record count, file boundary offset, per-record dedupe keys, and a small
  tail sample (e.g. last 32 records).
- On append, if the boundary advanced contiguously and the tail sample
  matches, absorb only the new byte range; require a trailing newline.
- Fall back to a full re-parse on allocator change, boundary regression,
  tail mismatch, or compaction/rewrite.
- Enforce caps (`max_records`, `max_record_bytes`, `max_file_bytes`,
  `max_file_incidents`); dedupe exact duplicates, record a conflict once
  for a second variant, ignore further variants without extra writes.

Done when: steady-state appends read only the tail, and a benchmark shows
append cost independent of total file size.

## 6. Asynchronous usage publication

Move durable usage publication off the turn critical path:

- Snapshot pending publications under a mutex, publish
  `pending → incidents → facts` through a sink, then apply generations and
  checkpoint best-effort.
- Drain on a background thread with epoch bump plus cancel-and-join;
  spawn failure or constrained platforms fall back to synchronous flush.
- Deferred observations schedule a drain; stop/reset/restore paths
  cancel and join the drain thread.
- Conflicting publications surface as typed errors, never silent drops.

Done when: turn latency no longer includes ledger fsync/reconciliation,
and shutdown still guarantees publication settlement or a typed error.

## 7. Structured safety review tolerates commentary

When a permission/safety reviewer returns a valid structured decision plus
extra prose, use the decision instead of failing the review:

- Parse the completion for a decision; commentary around it is not malformed.
- Retry at most once, within the original deadline and payload, when the
  response has no valid decision and is structurally malformed.
- Never retry a caution to chase approval; unreviewable actions stay
  unexecuted and the agent may continue with other tools.
- Release every completion payload on all paths including the retry.

Done when: fixtures cover decision-with-commentary accepted, malformed
retried exactly once on the same deadline, caution never retried, and no
completion leaks.

## 8. Provider connection binding identity

Custom model connections must be bound to the endpoint they were defined
against:

- Connection identity is a hash of `id + protocol + base_url + auth
  type + credential env name` — never the secret itself.
- Saved sessions retain the connection name plus this non-secret
  fingerprint; changing or removing the connection blocks implicit resume
  against a different destination. History stays readable.
- Strict connection validation: identifier charset, reserved names,
  URL shape (no user/query/fragment, HTTPS except loopback, strip
  trailing slash).
- Invalid connection configuration fails model startup; never fall back
  to another credential or endpoint.
- Optional per-connection reviewer-model override stays on the same
  connection; reviewer failure leaves the action unapproved.

Done when: swapping a base URL or auth mapping invalidates resume binding,
and misconfiguration is a startup error, not a silent reroute.

## 9. Process-scoped provider/model overrides

Support ephemeral provider/model/effort/speed selection that does not
rewrite stored settings:

- Leading interactive-launch flags and per-run one-shot flags select
  provider, model, reasoning effort, and speed tier for that invocation.
- Per-run overrides win over startup/resumed values and are never persisted.
- Explicit model selection drops compiled-default speed behavior unless
  explicitly re-requested.
- Empty values are ignored (same as unset); conflicting toggles
  (`--fast` + `--no-fast`) are a parse error.
- Record provenance (`compiled_default | stored | process_override`) for
  the effective provider and model source.

Done when: an ephemeral override affects exactly one invocation, leaves
settings files untouched, and provenance is inspectable.

## 10. Opaque reasoning continuity scoped to same authority

Preserve provider reasoning blocks across tool calls and resume without
interpreting them:

- Forward opaque reasoning material only when provider authority
  (binding identity) and model both match; otherwise drop it and keep text.
- For custom connections, re-inject reasoning only when the exact tool-call
  sequence it belongs to is still present; otherwise omit the whole block.
- Strip signed/encrypted reasoning details on connection or model change.
- One-shot summary/compaction calls disable tools and keep the ambient
  model, deadline, and output limits.

Done when: same-connection continuations preserve reasoning, changed
endpoints strip it, and no test depends on interpreting opaque bytes.

## 11. Persisted session catalog for instant resume

Open the resume picker instantly from a persisted catalog, then revalidate
in the background:

- Maintain a versioned, checksummed catalog file (magic header + payload
  hash, size/record caps) listing actionable sessions with summaries.
- Fingerprint each row against session sources
  (`session.json`, event log, authority/pending/display/subagent markers)
  plus file stat (inode, size, mode, mtime/ctime); absence of a file is
  part of the proof.
- Paint the stale snapshot immediately; admission of the selected session
  always re-checks the live directory. Corrupt/oversize/version-mismatch
  catalogs degrade to a miss, never block the picker.
- Isolate picker vs. ranking writes so they do not clobber each other;
  gate freshness (e.g. 5 s) and supersede/abandon background loads by
  generation.

Done when: cold resume-picker open paints without a full session scan,
selection still validates live state, and corrupt catalogs are invisible
to the user.

## 12. Background session titles

Generate a short title for new conversations without blocking turns:

- Gate on: setting enabled, provider supports titles, session untitled,
  not a recovery replay, no conflicting task running.
- Excerpt the first prompt (trim, reject empty/command-only, UTF-8 safe,
  e.g. 2 KiB cap); system instruction asks for a short title
  (e.g. ≤8 words) and marks the excerpt untrusted.
- Cap output (e.g. 60 bytes), first line only, strip quotes/controls.
- Run on a background task with a timeout and small output cap; exclude
  the call from billing/usage ledgers; record outcome
  (`generated | unavailable + reason`) for trace.
- Install only if the persisted title still equals the history-derived
  default; never overwrite an explicit rename.

Done when: new sessions get titles asynchronously, failures are silent
except in trace, and renames are never clobbered.

## 13. Shell failure recovery guidance

When a shell/process tool call fails validation or execution, tell the
model how to recover:

- Match error prefixes by `argv0` basename so renamed binaries still match.
- Explain the argument problem and suggest a correction only when the
  repair is unambiguous; otherwise report the failure without guessing.
- Keep raw command output escaped before it reaches the model or terminal.

Done when: common shell parse/usage errors produce actionable guidance
and ambiguous cases do not hallucinate fixes.

## 14. Transcript anchor preservation across retention rewrites

Retention, truncation, and resume-reclip must not shuffle or lose visible
rows:

- Distinguish strict rewrites from same-epoch preservations; retention
  prefix trims ride along as shadow state so they do not invalidate the
  anchor.
- Reclip resumed rows to the live terminal width; repaint after a
  retention rebase marks the transcript unpainted.
- Resolved Q&A and command status lines re-render cleanly across widths.
- Cover with tests: slow-path anchor preserved on retention rewrite,
  same-epoch rewrite kept, strict/geometry-blocked falls back.

Done when: long-session resume, resize, and retention no longer shuffle
rows or drop the last character on full-width lines.

## 15. Long-turn memory budget and benchmark

Bound temporary memory during long turns and enforce it in CI:

- Scope per-attempt scratch (network/parser/recovery-checkpoint) to an
  arena that is released every attempt; only a small owned result escapes.
- Add a bounded long-turn benchmark: fixed steps against a local fixture
  that discards bodies, samples RSS periodically, and writes
  `measurements.json` (completed flag, peak RSS, retained history).
- Gate in CI (e.g. N steps under a MiB ceiling); `OutOfMemory` is not a pass.
- Document the reproduction command and what counts as retained vs. scratch.

Done when: a 500+ step fixture passes under the RSS ceiling in CI and the
bench distinguishes leaks (retained growth) from per-step scratch.
