# Conversation compaction

Compaction is a transactional replacement of canonical conversation history.
It is not a provider feature, a session-store rewrite, or a claim that a
provider prompt cache was hit. The core owns the history and performs the only
commit; a caller-owned `Compactor` sees an owned `CompactionContext` and can
only return a proposal.

`cache_replay_summary_v0` is the current `tea-agent` default. It preserves the
existing provider-visible source conversation, ordered tool definitions, and
system instructions, then appends exactly one update instruction when that
source is an exact active-context prefix and fits the configured reserve. Tool
execution is prohibited by the compactor stream even though the model-visible
tool envelope is retained. Otherwise it uses the existing standalone summary
layout. The fallback is observable; it is not silently described as
cache-friendly.

## Lifecycle and safety contract

Every manual or automatic attempt receives one `CompactionId` and emits the
content-free `CompactionLifecycleRecord` sequence:

```text
Started → SourceSelected → RequestPrepared → ProviderUsageObserved?
        → ReplacementProposed? → Terminal
```

`Terminal` is exactly one of committed, rejected with a typed reason, failed,
cancelled, timed out, or unavailable. The record includes the trigger,
threshold/overflow reason, agent-loop phase, strategy ID/schema/prompt
fingerprint, source history revision, ordinal counters, source/replacement
sizes, tool-result size, request-layout facts, adapter request bytes, and
provider usage when supplied. It does not include checkpoints, prompts,
arguments, tool results, or serialized request bodies.

The commit is guarded by `AgentState.history_revision`. A compactor snapshot
can never replace history that was mutated after source selection. Manual
compaction reserves an idle agent; automatic compaction commits only in the
owning run and only if cancellation has not won.

Automatic proposals must validate message/tool-pair structure, retain the
selected recent suffix byte-for-byte as canonical messages, have a nonempty
non-tool checkpoint, strictly reduce canonical bytes, and leave the explicit
`minimum_headroom_tokens`. Rejection or failure leaves the pre-transaction
history intact.

The OpenRouter adapter has explicit request and stall bounds; provider-backed
compaction uses that same bounded `ModelProvider` boundary. A host that can
classify its own deadline returns `CompactionError::timed_out`, which becomes a
typed terminal timeout rather than a successful or ambiguous failure.

## Strategies

The strategy descriptor is versioned and is emitted on every attempt.

| ID | Layout | Status |
| --- | --- | --- |
| `cache_replay_summary_v0` | exact replay when safe; standalone fallback otherwise | default baseline |
| `tool_free_replay_summary_v1` | exact replay with tools omitted | provider-compatibility candidate only |
| `structured_checkpoint_v1` | standalone marker-versioned structured checkpoint plus host ledger | candidate only |
| `incremental_checkpoint_update_v1` | prior marker checkpoint plus discarded delta and ledger delta | candidate only |

Candidate strategies are deliberately not the default. Offline evidence can
prove safety and determinism but cannot establish provider cache behavior or
semantic recovery quality. Promotion requires an explicit provider canary and
a committed comparison artifact; rollback is selecting the prior strategy ID.

The host binds every listed candidate to the existing `ModelProvider` through
`ProviderCompactor`. `structured_checkpoint_v1` owns a readable v1 marker and
injects a bounded, host-derived operation ledger after the model's semantic
text. `incremental_checkpoint_update_v1` recognizes that exact marker, sends
only the previous checkpoint and newly discarded provider history, and uses a
visible standalone first-generation fallback. `tool_free_replay_summary_v1`
changes only the prompt-facing tool envelope, so it can isolate provider
compatibility from checkpoint-prompt changes. All remain non-default.

## Trace and privacy

With `tea-core/trace`, `TraceObserver` maps lifecycle records to additive
`tea-trace::Compaction` V1 records. Existing header/turn/tool/end records
remain schema V0. The terminal committed record is joined to the first normal
post-compaction adapter request, including exact serialized request bytes and
an adapter cache-domain fingerprint when safely available.

Compaction trace records are content-free by construction. Ordinary turn/tool
trace records can contain redacted user and tool values, so production sinks
still need `tea_trace::RedactingSink`. Never put raw checkpoints, prompt
payloads, provider response bodies, credentials, or workspace content in an
artifact.

## Verification and operations

The normal gate is offline and does not make a provider request:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction --out /tmp/tea-compaction-quality
```

It writes 70 named Rust-contract coverage reports, five independently executed
continuation episodes, and a summary that distinguishes provider cache
accounting from a prefix proxy. The continuation fixtures assert facts,
latest-wins removal, operation-ledger/rework metrics, and five successive
generations; they do not grade a provider-generated checkpoint. To replace its
checked-in contract baseline,
an explicit reason is required:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction \
  --out /tmp/tea-compaction-quality --update-baseline \
  --reason "describe the reviewed contract change"
```

Run the focused Rust checks with the pinned toolchain:

```sh
CARGO_TARGET_DIR=/tmp/tea-compaction-target rustup run nightly-2026-07-24 cargo test \
  -p tea-core --test compaction --test automatic_policy --test cache_friendliness
CARGO_TARGET_DIR=/tmp/tea-compaction-target rustup run nightly-2026-07-24 cargo test \
  -p tea-core --features trace --lib trace::tests
```

Provider canaries are manual, credential-scoped, and must use a free model.
Use the `vault OPENROUTER_API_KEY -- …` boundary for the final provider command
and write only sanitized outcomes, model ID, strategy ID, timing, and
provider-reported usage to an external artifact directory. Do not treat an
unavailable or rate-limited free model as a semantic comparison result.

The repository-owned probe drives four pressured turns through the same host
assembly and requires one committed compaction:

```sh
vault OPENROUTER_API_KEY -- env CARGO_TARGET_DIR=/tmp/tea-compaction-target \
  rustup run nightly-2026-07-24 cargo run --quiet -p tea-agent \
  --bin tea-compaction-canary -- \
  --model poolside/laguna-xs-2.1:free --strategy tool_free_replay_summary_v1 \
  --pressure-bytes 5000 --context-window 5000 --continuation-check
```

It emits only content-free JSON counts, a stable strategy ID, and (when
requested) a boolean fact-survival continuation check; it never emits the
fact itself. The ordinary `tea` host accepts the same deliberate selection through
`--compaction-strategy <id>`; omitted means the baseline. Try
`poolside/laguna-s-2.1:free` only when the preferred free model is unavailable.
The latest sanitized result and non-promotion decision are recorded in
[`compaction-canary-2026-08-22.md`](compaction-canary-2026-08-22.md).
The checked-in initial strategy comparison is
[`compaction-comparison.md`](compaction-comparison.md).
