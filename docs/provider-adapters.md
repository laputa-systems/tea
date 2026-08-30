# Provider adapters

The default `tea-core` build contains only the `ModelProvider` and
`ModelEventStream` ports. It does not choose a provider, issue HTTP requests,
or discover credentials. Optional adapters are an embedding convenience, not
a change to that core boundary.

The finite-response adapters and incremental OpenRouter/Codex streams retry
replay-safe failures with a bounded exponential backoff. The standard policy
makes the initial attempt plus three
retries at 250 ms, 500 ms, and 1 s, capped at 8 s. Transport failures are
retryable before output for adapters that can safely replay them; provider
response errors are retried only when the adapter can classify them as
transient (for example, 429 or 5xx). Hosts can
replace the policy with `RetryPolicy` through each finite adapter config's
`with_retry_policy` method. The generic `ModelProvider` port does not retry
opaque caller providers or replay a stream after it has exposed events.

| Feature | Module | Wire protocol | Intended use |
| --- | --- | --- | --- |
| `provider-openrouter` | `tea_providers::openrouter` | OpenRouter Chat Completions SSE plus inline usage/accounting | Opt-in incremental rustls + Graviola HTTPS transport with packet-bound model validation and response-stall timeouts. |
| `provider-local` | `tea_providers::local` | Caller-selected local OpenAI-compatible Chat Completions SSE endpoint | Opt-in incremental HTTP transport for oMLX and similar local servers; no credentials or endpoint discovery. |
| `provider-opencode-zen` | `tea_providers::opencode_zen` | OpenCode Zen Responses API SSE (`https://opencode.ai/zen/v1/responses`) via `input` array | Opt-in incremental rustls + Graviola HTTPS transport for `opencode-zen`/`muse-spark-1.2-contributor-free` (free). Mirrors the real `opencode` TUI provider (`opencode` → `https://opencode.ai/zen/v1`, `OPENCODE_API_KEY`, `openai-compatible` for most models, `responses` for `muse-spark`). Uses `Authorization: Bearer` + `Accept: text/event-stream`, `User-Agent: tea/1.0 opencode-zen`, `x-opencode-client: tea`. |
| `provider-codex` | `tea_providers::codex` | Direct ChatGPT-subscription Codex Responses SSE | Opt-in ChatGPT OAuth, Tea-owned rotating credentials, honest `originator: tea`, encrypted reasoning continuity, and no OpenAI Platform API key. See [Codex provider](codex-provider.md). |

All repository HTTP I/O goes through `tea-http`. Provider adapters share one
pooled `tea_http::TransportClient` through their `transport_runtime` executor
bridge; the adapter itself owns only its payload, headers, status
classification, and SSE/NDJSON parsing. `tea-http` owns the asynchronous
h12tiny client, direct-origin DNS/TCP/TLS, HTTP/1.1 and HTTP/2 negotiation,
connection pooling, cancellation, request deadlines, response-stall limits,
and byte-stream delivery. It has no provider protocol or credential knowledge.

Enable only the provider an application owns:

```toml
[dependencies]
tea-core = { path = "../tea/crates/tea-core", features = ["provider-local"] }
```

Provider features are opt-in and none is enabled by default. The adapters use
the caller's selected executor and add no Tokio dependency.

## Factory-grade OpenRouter contract

`OpenRouterProvider` takes an explicit `OpenRouterConfig`; it does not read a
credential file, environment variable, working directory, or model catalog.
Use `OpenRouterConfig::try_new` (or call `validate` before admission) so an
empty key/model, zero output cap, or unsafe key spelling fails before transport.
Every request must carry a `ModelDescriptor` whose provider is exactly
`openrouter` and whose model is exactly the configured model. A missing or
mismatched descriptor is a terminal adapter error and never results in a
network request.

The configured `max_tokens` is sent as the OpenRouter `max_tokens` output cap.
The adapter defaults each HTTP request to a 300-second timeout and
detects a response that has started but then produced no non-whitespace bytes
for 60 seconds. A streaming request may legitimately have no body bytes while
the provider is generating it, so the request timeout bounds
that pre-response period; callers can replace both with
`OpenRouterConfig::with_request_timeout` and `with_stall_timeout` to keep
retries inside their own session wall budget. The factory host derives both
timeouts from the admitted assignment wall limit rather than using the adapter
defaults. Retryable setup and response-status failures are retried with the
configured bounded backoff before any model-visible event escapes; the timer
wait is interrupted by cancellation. Once output, a tool call, or usage has
escaped, a later stream failure is terminal to the adapter rather than replaying
an ambiguous request for the caller.
Request-scoped `ThinkingLevel` values are mapped to OpenRouter's native
`reasoning: { "effort": ... }` object (`off` maps to `none`); the default level
omits the field. This keeps provider-specific wire details in the adapter while
leaving policy and model selection with the host.

The provider sends the API key as an in-memory Authorization header. It never
puts the key in argv, a child environment, or a temporary file. The shared
transport has no ambient proxy or credential-file discovery. It checks the
run's `CancellationToken` before, between received body chunks, and after the
synchronous, timeout-bounded response body. A provider-owned HTTP worker
performs those blocking reads while the caller-polled `ModelEventStream`
reduces each complete SSE record in order. Cancellation settles as
`StopReason::Cancelled` at the next bounded body-read boundary; the core and
host are not blocked from processing already-delivered deltas while the
provider continues generating.

Provider accounting is available through `usage_snapshot` and `cost_report`.
Token counters retain unknown-vs-zero semantics. Each cost turn and the
aggregate report expose exact non-negative decimal strings in
`total_usd_exact`, `upstream_inference_usd_exact`,
`reported_total_usd_exact`, and
`reported_upstream_inference_usd_exact`. The parallel `f64` fields exist only
as convenience projections and must not be used for budget decisions.

When an adapter drives `Agent`, its normalized `Usage` update is retained in
`AgentSnapshot.accounting` and emitted as `AgentEventKind::ModelTurnUsage`.
Adapters map reported cache fields and total cost into that update when they
provide them; unknown values remain unknown. The core performs no pricing lookup.

`OpenRouterProvider::last_error_report` exposes the most recent trusted-host
diagnostic. It retains the failure boundary, captured HTTP status, retry
classification, attempt number, request/response byte counts, and a bounded
redacted response prefix. The generic model stream still receives only the
stable adapter error as its terminal message; a separate typed diagnostic
event carries the bounded report, so provider text cannot enter agent state
unboundedly.
When the terminal host receives that stream, it also writes the same bounded
report to `ProviderRequestSettledRecord.provider_error` in the durable session.
The report is therefore available after reopening or with `tea session dump`,
while credentials and an unbounded raw HTTP body are never persisted.

## Credentials and host authority

Finite adapters accept keys directly in their configuration where applicable. They never read
environment variables, a home-directory auth file, the current working
directory, or the system clock. Applications may obtain credentials and host
facts using their own secret/capability boundary, then pass those values in.

The terminal's optional subagent composition uses a lazy provider factory keyed
by the exact `ModelDescriptor`. It resolves a closed checked-in catalog before
persisting the session policy, constructs an adapter only when that descriptor
is actually selected, and keeps one immutable compactor per descriptor/provider
pair. The child tool call cannot supply a provider, endpoint, credential or
model outside that catalog, and unused catalog entries do not trigger credential
lookup.

Workspace-bearing adapters receive the stable logical repository label, not an
isolated child's physical session worktree. Stable logical repository labeling and
project metadata should remain stable across equivalent child leases; private index
paths, worktree paths, and lease suffixes should not cross provider boundaries.

## Codex subscription contract

The optional `codex` adapter is deliberately distinct from API-key adapters.
`FileCredentialStore` receives the terminal-owned explicit
`auth/codex.json` path, while `CodexAuthManager` receives that store and
`CodexProvider` receives the shared manager. None discovers a home directory,
an environment variable, or another client credential. The terminal wires
`tea auth login|status|logout codex` and requires an explicit
`codex/<model-id>` descriptor.

`CodexProvider` uses the fixed ChatGPT backend origin, not an OpenAI Platform
origin. It sends `originator: tea` and never substitutes a first-party Codex
identity. It is intentionally SSE-only, without WebSocket continuation or
request compression. The detailed OAuth, persistence, context-continuity, and
upstream-contract maintenance rules are in [Codex provider](codex-provider.md).

## Context and stream mapping

Hosts using OpenRouter, Local, or other Chat Completions adapters should install
`tea_providers::openai::OpenAiContextHook` on the agent. It converts
the core transcript to the standard Chat Completions message array consumed by
both adapters. The core default `NoHooks` value is intentionally diagnostic
Rust text and is not a provider wire format.

Hosts using `codex` instead install `tea_providers::codex::CodexContextHook`.
It emits native Responses input items, places effective system instructions in
the top-level Codex payload, and replays only `codex`-scoped opaque encrypted
reasoning context alongside the assistant turn that produced it. Other
providers neither render nor reuse that opaque material.

Before conversion, core applies the configured `ToolResultProjectionPolicy` to
a clone of canonical tool results. Raw content/details stay in the transcript
and lifecycle events; model-facing text is bounded, marks error state and
recovery guidance, and encodes unsupported details in a marked representation.
The OpenAI-compatible array carries `is_error` for in-tree native adaptation;
OpenAI-compatible adapters preserve marked details in the text output.

Those adapters consume a caller-converted standard Chat Completions JSON message
array from `ModelRequest.context`. They map textual user/assistant messages and
function-style assistant tool calls into content blocks and keep tool-result names
paired with prior tool calls.

Provider event envelopes map directly to core model-stream events. Error payloads
stay generic before entering agent state, so a remote service cannot inject
arbitrary transcript text. Hosts can read `last_error_report()` from the concrete
adapter for the last failure's source, message, status, and retryability classification.

Reasoning summaries and raw reasoning text are intentionally not assistant
content: the current core model-stream contract has no visible reasoning event,
so treating them as an answer would corrupt the transcript. The Codex adapter
additionally preserves only its opaque encrypted continuation item for a later
compatible request; it never renders or interprets it. This is an explicit
boundary rather than a hidden fallback. The current gateway may emit a
`provider-metadata` envelope after `finish`; it is accepted as non-content
metadata rather than misclassified as a second terminal event.

OpenRouter and Local expose network-time assistant deltas through the core
stream while preserving final usage before their terminal events. The generic
`ModelProvider` port does not retry or replay a stream after it has exposed events.

All in-tree adapters own their request boundary. On the run
cancellation token, finite adapters check cancellation before and between body
chunks while body workers yield completed chunks to the caller-polled stream.
Cancellation does not become a retryable transport error. Immediate mid-read
interruption remains bounded by the receive timeout.

## Local oMLX and Laguna

The local adapter accepts an explicit API root and model. Its convenience
configuration targets the 5-bit `Laguna-XS-2.1-5bit` checkpoint served by oMLX:

```rust,no_run
use tea_providers::local::{LocalConfig, LocalProvider};

let config = LocalConfig::laguna_xs_2_1("http://127.0.0.1:8000/v1");
config.validate()?;
let provider = LocalProvider::new(config);
# let _ = provider;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The request uses `POST /v1/chat/completions` with `stream: true`, OpenAI
function tools, `max_tokens`, and
`chat_template_kwargs: {"enable_thinking": true}` for Laguna. The adapter
decodes SSE `delta.content` records as they arrive, assembles indexed tool-call
fragments into complete calls, and maps `finish_reason` plus
prompt/completion/cache usage fields (requested with
`stream_options.include_usage`) into the core stream contract. Reasoning
text is intentionally ignored because the current core stream model has no
separate reasoning-content event; turning it into assistant text would corrupt
later context.

### Local compaction boundary

oMLX does not define a provider-side compaction operation. A host that wants
compaction sends an ordinary tool-free summary request through
`LocalProvider`, then gives the proposed replacement messages to core's
transactional `Compactor` boundary. This is the same path used for other
OpenAI-compatible providers and works with the local SSE stream, including its
final usage event. Automatic compaction requires an explicit effective context
capacity: the TUI uses the checked-in Laguna capacity or the
`--local-context-window <tokens>` value for a custom local model. It never
infers capacity from an arbitrary model ID.
