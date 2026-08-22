# Conversation compaction

Compaction is a transactional replacement of canonical conversation history.
It is not provider-native history management, session persistence, or evidence
of a provider cache hit. `tea-core` owns the history and its only commit; a
caller-owned `Compactor` receives an owned `CompactionContext` and can return
only a proposed replacement. The host owns the provider, prompt, model,
context capacity, and compactor algorithm.

`tea-agent` has one provider-backed strategy:
`cache_replay_summary_v0`. It preserves a usable provider-visible source
conversation, ordered tool definitions, and system instructions, then appends
one compaction instruction. The compactor stream rejects tool calls, so tools
can remain in the prompt-facing envelope without gaining execution authority.
When exact replay is unavailable, it sends the existing standalone summary
request instead. The selected layout is observable; standalone fallback is
never described as cache-friendly.

The former tool-free, structured-checkpoint, and incremental-checkpoint
experiments were removed. Their provider-free results and one-off canaries did
not meet the promotion bar for semantic continuation quality or provider cache
reuse. A future strategy must arrive as a new, reviewed implementation and ID;
it must not repurpose this baseline.

## Lifecycle and safety

Every manual or automatic attempt has a `CompactionId` and emits a
content-free lifecycle stream:

```text
Started → SourceSelected → RequestPrepared? → ProviderUsageObserved?
        → ReplacementProposed? → Terminal
```

`RequestPrepared`, provider usage, and a proposal are conditional. For
example, an automatic attempt without a configured compactor reaches
`Terminal(Unavailable)` after source selection; preparation can also fail
before a request is ready. Terminal outcome is exactly one of committed,
typed rejection, failed, cancelled, timed out, or unavailable.

The operation records its manual/automatic trigger, threshold/overflow/user
reason, loop phase, strategy and schema, source-history revision, and retry
counters. Source and proposal records carry only IDs, counts, byte sizes,
request-layout facts, usage, and classified outcomes; they never contain
summary text, prompts, arguments, tool results, or serialized request bodies.

The commit is guarded by `AgentState.history_revision`. A snapshot may not
replace history changed after source selection. Manual compaction reserves an
idle agent. Automatic compaction remains in its owning run and checks
cancellation before commit. Failed, cancelled, stale, and rejected proposals
leave canonical history unchanged.

Automatic proposals must have valid message/tool-call structure, preserve the
core-selected recent suffix byte-for-byte as canonical messages, contain a
nonempty non-tool checkpoint, strictly reduce canonical bytes, and satisfy the
configured `minimum_headroom_tokens`. Rejections are typed, observable, and do
not loop indefinitely.

## Automatic policy and request layout

`AutomaticCompactionPolicy` is opt-in. A host supplies a context capacity,
reserved compaction tokens, recent-tail budget, minimum headroom, overflow
recovery policy, and per-run compaction/retry limits. The core estimates the
next request from valid provider usage plus new messages, or a deterministic
canonical-byte estimate when no valid checkpoint exists. It does not infer
capacity from a model name.

After a completed assistant/tool turn, the policy can compact before the next
provider request. A typed `ModelStreamEvent::ContextOverflow` can restore the
pre-request transcript, compact once, and retry that incomplete continuation.
Each continuation retries at most once; the run policy bounds total recovery.
Only the typed event authorizes overflow recovery—the core does not parse
provider error text.

Exact replay is used only when the selected provider context is an exact
message prefix of the active context (when active context is supplied) and
when the system prompt, context, and ordered tools fit within the declared
context budget after adding both the configured reserve and the fixed
4,096-token safety margin. Otherwise the host uses the explicit standalone
fallback.

The compactor-request measurement describes the exact request sent: it does
not run hooks, provider projection, or serialization a second time. The
adapter may provide serialized request bytes and a normalized cache-domain
fingerprint without exposing provider-specific request types through core.

## Evidence and metrics

Every value has an evidence level. Unknown remains unknown; zero means the
source actually reported zero.

| Metric | Meaning | Evidence |
| --- | --- | --- |
| `compaction_id`, source revision, trigger/reason/phase, strategy, terminal outcome | lifecycle identity and state | lifecycle fact |
| canonical/source/replacement/retained/tool-result bytes | history size and composition | deterministic proxy |
| estimated context tokens and headroom | policy budget after replacement | estimate, not tokenizer truth |
| serialized request bytes and cache-domain fingerprint | exact adapter envelope observation | adapter fact |
| common context prefix, exact extension, append-only context | request-layout diagnostic | prefix proxy |
| provider input/output tokens | compactor usage | provider fact |
| `cache_read_tokens` / `cache_write_tokens` | provider prompt-cache accounting | provider fact |
| critical facts, obsolete facts, repeated reads, duplicate tools, failed repeats, next-compaction distance | provider-free continuation fixture | deterministic fixture result |

Matching context prefixes and stable envelopes help diagnose cache-friendly
layout. They never prove a cache hit and must not be converted into estimated
cache-read tokens. Only provider-reported cache usage supports a cache claim.

With `tea-core/trace`, `TraceObserver` converts lifecycle events to additive,
content-free `tea-trace::Compaction` V1 records. Existing header, turn, tool,
and episode-end records remain V0. A committed operation is joined to the
first normal post-compaction request, including its turn index and adapter
request facts when available. Use `tea_trace::RedactingSink` for ordinary
turn/tool trace content; compaction records themselves intentionally cannot
contain sensitive payloads.

## Provider-free verification

The normal compaction check is manual, offline, deterministic, and has no
provider, credential, network, or ambient-cache dependency:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction \
  --out /tmp/tea-compaction-quality
```

It writes 70 named coverage rows backed by focused Rust tests and five
independent continuation episodes. The latter assert durable-fact retention,
latest-wins obsolete-state removal, retained-suffix behavior, rework
classification, headroom, and distance to the next compaction. They validate
the transaction and evaluator contracts, not a provider-generated summary.

`evals/quality/cases/compaction/baseline.json` pins the reviewed scenario
contract and historical source provenance. Replacing that contract requires an
explicit reason:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m evals.quality compaction \
  --out /tmp/tea-compaction-quality --update-baseline \
  --reason "describe the reviewed contract change"
```

The focused Rust checks use the pinned toolchain:

```sh
CARGO_TARGET_DIR=/tmp/tea-compaction-target rustup run nightly-2026-07-24 \
  cargo test -p tea-core --test compaction --test automatic_policy --test cache_friendliness
CARGO_TARGET_DIR=/tmp/tea-compaction-target rustup run nightly-2026-07-24 \
  cargo test -p tea-core --features trace --lib trace::tests
```

An optional `tea-compaction-canary` binary exercises only the default host
assembly against an explicitly credentialed OpenRouter model. It emits
content-free lifecycle and usage-presence counts; it does not measure semantic
quality, latency, or a cache hit. It is never run implicitly:

```sh
OPENROUTER_API_KEY=… CARGO_TARGET_DIR=/tmp/tea-compaction-target \
  rustup run nightly-2026-07-24 cargo run --quiet -p tea-agent \
  --bin tea-compaction-canary -- --model <provider-model> \
  --pressure-bytes 5000 --context-window 5000 --continuation-check
```

One 2026-08-22 free-model baseline probe committed; later free-provider
attempts ended before compaction. Neither result establishes semantic
continuation quality or provider cache reuse, so the design makes neither
claim.

## Design rationale and exclusions

On 2026-08-22, tea reviewed OpenAI Codex (`343074d4207d`), Pi
(`c49906ec7778`), DeepSeek Harness (`b150a551b8d4`), and the OpenAI Cookbook
prompt-caching examples. The durable decisions were explicit lifecycle IDs,
source-generation compare-and-swap, exact retained suffixes, minimum
headroom, same-path request observation, bounded overflow recovery, and the
strict distinction between prefix proxies and provider cache accounting.

Tea deliberately does not import upstream session persistence, client reuse,
UI-specific summaries, tool-result pruning, model-name token limits, provider
routing, or background compaction workers. Tool-result pruning remains
unimplemented: no retained baseline showed old tool-result payloads dominating
pressure while preserving the needed continuation state. The core stays
executor- and provider-agnostic.
