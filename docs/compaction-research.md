# Compaction research notes

This is a design record, not a compatibility claim. Sources were inspected on
2026-08-22 at the commits below, with local source paths retained so later
updates can re-run the comparison rather than rely on a blog-summary memory.

| Source | Commit inspected | Relevant paths | Observed behavior | Tea decision |
| --- | --- | --- | --- | --- |
| OpenAI Codex | `343074d4207d` | `codex-rs/core/src/compact.rs`, `session/turn.rs`, `context_manager/history.rs` | separates pre-turn, mid-turn, and standalone compaction; tracks attempt phases and reuses a client/session boundary | adopt explicit phase, attempt, and lifecycle identity; keep provider/session ownership outside core |
| Pi | `c49906ec7778` | `packages/coding-agent/docs/compaction.md`, `packages/ai/src/utils/overflow.ts`, `packages/ai/src/api/openai-completions.ts` | rolling summaries retain an exact tail; OpenRouter/Poolside’s “maximum allowed input length” diagnostic is overflow even without the word “context”; OpenRouter reasoning/session behavior stays in the adapter | adopt exact retained suffix/cache-layout measurement and the narrow Poolside overflow classification; do not adopt Pi session storage or runtime coupling |
| DeepSeek Harness | `b150a551b8d4` | compaction subsystem/source tests | append-only start/summary/end lifecycle, original-source linkage, pre-step pressure plus overflow recovery, optional tool pruning | adopt append-only lifecycle, source generation CAS, and bounded overflow retry; defer pruning until measured dominance |
| OpenAI Cookbook | `79791c4e0dc` | prompt-caching examples | stable prefixes and stable request shape matter, but cache accounting is provider-specific | keep prefix measurements explicitly separate from provider-reported cache usage |

## Hypotheses and outcomes

1. **A compacted replacement needs a durable identity.**
   Implemented as `CompactionId { run_id, ordinal }` and
   `CompactionLifecycleRecord` in `crates/tea-core/src/compaction.rs`. The
   trace adapter joins the committed operation to the first post-compaction
   normal request. This makes retries, failures, and later comparison possible
   without storing transcript content.

2. **Snapshots must not overwrite newer history.**
   Implemented as `AgentState.history_revision` and a compare-and-swap check in
   `commit_replacement`. The regression test
   `compaction::tests::stale_generation_cannot_replace_newer_canonical_history`
   mutates the canonical history between source capture and commit.

3. **Retaining an exact tail is safer than rebuilding the whole transcript.**
   Implemented for automatic compaction: selected retained messages must be an
   exact proposal suffix, and tool-call/result pairs cannot be cut apart. This
   is stronger than “the summary seems adequate” and remains provider-agnostic.

4. **A checkpoint must create usable capacity, not merely be valid text.**
   Implemented with strict byte reduction and an explicit
   `minimum_headroom_tokens` policy. The proposed estimate and headroom are
   recorded. They are estimates, not tokenizer or provider guarantees.

5. **Prompt-cache claims require two evidence levels.**
   `measure_request_layout` captures core and adapter domain changes, exact
   extensions, and serialized-envelope bytes from the same stream that sends a
   request. It deliberately does not call hooks or rebuild a second request.
   Only `Usage::cache_read_tokens` and `cache_write_tokens` are provider cache
   evidence.

6. **A provider-free baseline can establish safety but not quality.**
   Implemented as the 70-scenario `evals.quality compaction` matrix. It covers
   pressure, tool boundaries, state transitions, cache churn, cancellation,
   provider failure classification, checkpoint validity, trace compatibility,
   and determinism. It does not claim that a model summary is semantically
   adequate; that remains an explicitly authorized canary question.

7. **Model-free tool-result pruning should be conditional.**
   Rejected for the default at this stage. The retained canonical transcript is
   lossless, and no checked-in baseline demonstrates old tool results as the
   dominant pressure source while preserving a continuation task. The metric
   `tool_result_bytes` and offline artifact make the trigger auditable before a
   future candidate is introduced.

8. **Provider diagnostics can be precise without polluting core policy.**
   Pi’s current `packages/ai/src/utils/overflow.ts` treats OpenRouter/Poolside
   “Input length … exceeds the maximum allowed input length … tokens” as an
   overflow. Tea now recognizes that exact adapter-bound pattern in
   `provider/openrouter/response.rs`; the core still receives only the typed
   `ModelStreamEvent::ContextOverflow`. The regression test is
   `provider::openrouter::tests::classifies_context_capacity_errors_for_automatic_recovery`.

## Known differences kept deliberately

Tea does not import Codex or Pi session persistence, client reuse, UI-specific
summaries, tool pruning, tokenizer assumptions, provider routing, or a
background compaction worker. The core remains executor- and provider-agnostic.
`tea-agent` owns the current provider-backed prompt and its bounded adapter;
the default model-facing baseline remains unchanged while candidates wait for
explicit provider evidence.
