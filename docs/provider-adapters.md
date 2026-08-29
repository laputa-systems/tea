# Provider adapters

The default `tea-core` build contains only the `ModelProvider` and
`ModelEventStream` ports. It does not choose a provider, issue HTTP requests,
or discover credentials. Optional adapters are an embedding convenience, not
a change to that core boundary.

The finite-response adapters retry replay-safe failures with a bounded
exponential backoff. The standard policy makes the initial attempt plus three
retries at 250 ms, 500 ms, and 1 s, capped at 8 s. Transport failures are
retryable for finite adapters; provider response errors are retried only when
the adapter can classify them as transient (for example, 429 or 5xx). Hosts can
replace the policy with `RetryPolicy` through each finite adapter config's
`with_retry_policy` method. The generic `ModelProvider` port does not retry
opaque caller providers or replay a stream after it has exposed events.

| Feature | Module | Wire protocol | Intended use |
| --- | --- | --- | --- |
| `provider-openrouter` | `tea_providers::openrouter` | OpenRouter Chat Completions SSE plus inline usage/accounting | Opt-in incremental rustls + Graviola HTTPS transport with packet-bound model validation and response-stall timeouts. |
| `provider-commandcode` | `tea_providers::commandcode` | Command Code `/alpha/generate` NDJSON | Opt-in rustls + Graviola HTTPS gateway transport; the evaluation runner selects it with `--provider commandcode`. |
| `provider-local` | `tea_providers::local` | Caller-selected local OpenAI-compatible Chat Completions SSE endpoint | Opt-in incremental HTTP transport for oMLX and similar local servers; no credentials or endpoint discovery. |

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
defaults. A response stall reaches the configured receive timeout and
enters the same bounded retry policy as other transport failures.
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
OpenRouter maps reported cache fields and exact total cost into that update;
Command Code maps the token fields it reports and leaves cache and cost
unknown. The core performs no pricing lookup.

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

Both adapters accept a key directly in their configuration. They never read
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
isolated child's physical session worktree. In particular Command Code
`workingDir` and project metadata must remain stable across two equivalent child
leases; private index paths, worktree paths and lease suffixes never cross the
provider boundary.

Command Code also requires a `CommandCodeHostContext`, which makes the
gateway's `workingDir`, `date`, and `environment` fields an explicit host
decision:

```rust,no_run
use tea_providers::commandcode::{
    CommandCodeConfig, CommandCodeHostContext, CommandCodeProvider,
};
use tea_providers::RetryPolicy;
use std::time::Duration;

let host = CommandCodeHostContext::new("/sandbox/project", "2026-08-14", "linux")?;
let config = CommandCodeConfig::new("caller-supplied-api-key", "deepseek/deepseek-v4-flash", host)?;
let config = config.with_retry_policy(RetryPolicy::new(
    3,
    Duration::from_millis(250),
    Duration::from_secs(8),
));
let provider = CommandCodeProvider::new(config);
# let _ = provider;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`CommandCodeConfig` also provides explicit permission mode, a canonical UUID thread ID, mode,
temperature, output-token limit, and zero-data-retention-header settings. When present, the
thread ID is also sent as the Command Code session ID, matching the current
per-thread request shape without having the library generate or discover an identifier.
The current Command Code client metadata is also preserved: the project slug defaults to the
final component of the already-explicit `workingDir` (and can be overridden), and taste learning
defaults to the upstream client's enabled setting but can be disabled with
`with_taste_learning_enabled(false)`.
The provider accepts only a request whose `ModelDescriptor` is
`command-code` with the configured model, avoiding a silent model mismatch.

The `tea-eval` executable is a caller-owned integration boundary. Its
Command Code mode reads `COMMANDCODE_API_KEY` from its process environment,
not from the library, and requires explicit `--commandcode-date` and
`--commandcode-environment` values plus a caller-owned canonical UUID passed as
`--commandcode-thread-id`, plus `--commandcode-project-slug`. This keeps ambient
secret and host lookup out of `CommandCodeProvider` while making a deliberate
command-line harness practical.

## Context and stream mapping

Hosts using either concrete adapter should install
`tea_providers::openai::OpenAiContextHook` on the agent. It converts
the core transcript to the standard Chat Completions message array consumed by
both adapters. The core default `NoHooks` value is intentionally diagnostic
Rust text and is not a provider wire format.

Before conversion, core applies the configured `ToolResultProjectionPolicy` to
a clone of canonical tool results. Raw content/details stay in the transcript
and lifecycle events; model-facing text is bounded, marks error state and
recovery guidance, and encodes unsupported details in a marked representation.
The OpenAI-compatible array carries `is_error` for in-tree native adaptation;
Command Code maps it to its `isError` field and preserves marked details in the
text output.

The Command Code adapter consumes a caller-converted standard Chat
Completions JSON message array from `ModelRequest.context`. It maps textual
user/assistant messages and function-style assistant tool calls into the
gateway's `text`, `tool-call`, and `tool-result` content blocks. A tool result
must match a preceding assistant tool call, so the adapter can preserve its
tool name instead of guessing it.

The gateway's `text-delta`, `tool-call`, `finish`, usage, error, and abort
events map directly to core model-stream events. HTTP-level JSON error
envelopes without an NDJSON `type` are accepted as terminal gateway errors and
retain their bounded structured diagnostics in `last_error_report()`. Gateway
error payloads stay generic before entering agent state, so a remote service
cannot inject arbitrary error text into a transcript. A trusted host can instead call
`CommandCodeProvider::last_error_report()` for the last failure's source,
message, status, type, code, and retryability classification. The configured
API key is redacted from this host-only report, but its remote message remains
untrusted data and belongs only in private host diagnostics. Command Code
accepts `low`, `medium`, `high`, `xhigh`, and `max` reasoning effort values.
Generic `Off` omits the provider field, and generic `Minimal` maps to `low`,
because the gateway rejects `off` and `minimal`.

Reasoning deltas
are intentionally not retained: the current core model-stream contract has no
separate reasoning content variant, so treating them as assistant text would
corrupt the visible answer. This is a known API limitation rather than a hidden
fallback. The current gateway may emit a `provider-metadata` envelope after
`finish`; it is accepted as non-content metadata rather than misclassified as a
second terminal event.

OpenRouter and Local expose network-time assistant deltas through the core
stream while preserving final usage before their terminal events. Command Code
still collects its timeout-bounded native response before returning a finite
core stream. The generic `ModelProvider` port does not retry or replay a stream
after it has exposed events.

All in-tree adapters own their request boundary. On the run
cancellation token, Local and Command Code check cancellation before and
between body chunks, while OpenRouter's and Local's body workers yield
completed chunks to the caller-polled stream. Command Code also checks after
its timeout-bounded request settles.
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
