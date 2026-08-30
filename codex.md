Implement a complete, production-grade, opt-in Tea provider named **`codex`** that uses a user’s **ChatGPT Codex subscription**, not an OpenAI Platform API key.

This must be a native Tea model provider. Do not shell out to `codex`, do not embed or wrap the Codex agent loop, do not use the Codex SDK, and do not proxy through `api.openai.com`. Implement the ChatGPT OAuth flow and the Codex Responses wire protocol directly, preserving Tea’s provider/core separation and minimal-dependency philosophy.

Do not stop at a proof of concept. Complete authentication, refresh, secure persistence, request construction, streaming, tool calls, opaque reasoning-state preservation, retries, cancellation, terminal integration, tests, and durable documentation.

Do not ask me implementation questions. Make the best repository-consistent decisions described below, run the full verification suite, and leave the checkout complete.

# Research baseline

Use the following upstream snapshots as the protocol research baseline. The local Tea checkout is authoritative for Tea architecture; do not reset or discard newer local work.

## OpenAI Codex

Research snapshot:

```text
repository: ~/d/codex
commit: 63d213884daea50e4f74efc192cdc44f549b67d5
```

Inspect these targeted files rather than broadly mapping the repository:

```text
codex-rs/model-provider-info/src/lib.rs
codex-rs/model-provider/src/provider.rs
codex-rs/model-provider/src/auth.rs
codex-rs/model-provider/src/bearer_auth_provider.rs
codex-rs/login/src/server.rs
codex-rs/login/src/device_code_auth.rs
codex-rs/login/src/auth/manager.rs
codex-rs/login/src/auth/default_client.rs
codex-rs/login/src/auth/storage.rs
codex-rs/protocol/src/auth.rs
codex-rs/core/src/client.rs
codex-rs/codex-api/
codex-rs/http-client/
```

These establish the first-party Codex backend URL, Responses-only wire path, ChatGPT bearer/account headers, OAuth authorization-code flow, PKCE, device flow, refresh rotation, client identity headers, and stream semantics.

## Pi

Research snapshot:

```text
repository: ~/d/pi
commit: 853a80d26c90a14c1886f0ebb8ffaae133ca2185
```

Inspect:

```text
packages/ai/src/api/openai-codex-responses.ts
packages/ai/src/api/openai-responses-shared.ts
packages/ai/src/auth/oauth/openai-codex.ts
packages/ai/src/auth/oauth/device-code.ts
packages/ai/test/openai-codex-oauth.test.ts
packages/ai/test/openai-codex-stream.test.ts
```

Pi is the strongest independent implementation oracle. It demonstrates the direct ChatGPT OAuth flow, Codex Responses request shape, SSE processing, encrypted reasoning continuity, tool-call reconstruction, error classification, optional request compression, and WebSocket continuation behavior.

## Tea

Tea’s existing contract is that provider adapters own provider-specific payloads, headers, status handling, and stream decoding, while all HTTP I/O goes through `tea-http`. Credentials and host paths are host authority, not ambient adapter discovery. Preserve that contract.

When Pi and Codex differ, use this precedence:

1. Current first-party Codex behavior.
2. Pi’s independently validated behavior.
3. The smallest behaviorally correct adaptation that preserves Tea invariants.

Record intentional divergences in the provider documentation.

# Required result

Add an opt-in provider with these public identities:

```text
Cargo feature: provider-codex
Rust module: tea_providers::codex
Provider descriptor ID: codex
Human label: Codex (ChatGPT subscription)
Model spelling: codex/<model-id>
```

The feature must remain disabled by default.

A build that does not enable `provider-codex` must not compile or link its OAuth, JSON, cryptographic, or provider code. Do not increase the default binary’s dependency surface.

The provider must support:

* Browser OAuth login with PKCE.
* Headless/device-code login.
* Secure v1 credential persistence.
* Automatic rotating refresh-token handling.
* ChatGPT account-ID extraction.
* Codex Responses SSE.
* Assistant text streaming.
* Function-tool definitions, calls, and outputs.
* Parallel tool calls.
* Reasoning effort and summaries.
* Preservation and replay of encrypted reasoning state.
* Prompt-cache/session identity.
* Usage accounting with unknown cost.
* Provider diagnostics.
* Cancellation.
* Bounded pre-stream retries.
* Full terminal/provider-factory integration.
* Offline deterministic tests.
* An optional ignored live smoke test.

# Non-negotiable design decisions

## Direct backend, not a subprocess

The inference endpoint is:

```text
https://chatgpt.com/backend-api/codex/responses
```

Represent the canonical API root internally as:

```text
https://chatgpt.com/backend-api/codex
```

and append `/responses` exactly once.

Never send subscription credentials to:

```text
https://api.openai.com
```

Do not invoke the installed `codex` binary. Do not read its stdout protocol. Do not use `codex exec`, app-server, MCP, or the Codex SDK.

## Honest client identity

Tea must not impersonate a first-party client.

Send:

```text
originator: tea
User-Agent: tea/<tea-version>
```

Do not send:

```text
originator: codex_cli_rs
originator: codex-tui
originator: codex_vscode
```

Codex backend rollout behavior has historically depended on the combination of originator and client version. A third-party originator may temporarily lack access to a newly rolled-out model even when official Codex has it. Never “fix” that by spoofing a first-party originator.

Add a source-level constant:

```rust
CODEX_WIRE_COMPAT_VERSION
```

Set it to the Codex package version represented by the pinned upstream contract you actually implement. Send that value in the backend’s `version` header.

This is a wire-capability declaration, while `originator` and `User-Agent` continue to identify Tea honestly. Place a provenance comment next to the constant naming the upstream Codex commit. Update this constant only alongside contract fixtures and tests.

## No ambient Codex credential reuse

Do not automatically read:

```text
~/.codex/auth.json
$CODEX_HOME/auth.json
```

Do not import or share Codex’s refresh token. Refresh tokens rotate; two independent programs sharing one refresh token can invalidate each other.

Tea must perform a separate OAuth authorization and own its own credential record.

Do not inspect browser cookies or ChatGPT local storage.

## Preserve Tea’s credential boundary

The provider adapter must not independently inspect environment variables, home directories, the current working directory, or platform keychains.

Split responsibilities as follows:

* The terminal/host resolves Tea’s state root and credential path.
* A Codex auth manager receives that explicit path or an injected credential-store implementation.
* `CodexProvider` receives an explicit shared auth manager/token source.
* Tests use an in-memory credential store and injected clock.
* Provider construction remains lazy, so merely compiling or listing an unused Codex model does not read credentials or perform network I/O.

## No arbitrary production endpoint override

A bearer token for `chatgpt.com` is highly sensitive. Do not expose an ordinary CLI/config option that can redirect it to another host.

Production inference and OAuth origins must be fixed constants.

Tests may inject loopback HTTP endpoints through constructors that are either private, test-only, or explicitly named as test infrastructure. Reject non-loopback plaintext endpoints.

Never follow a redirect while retaining authorization headers across origins.

## SSE is the required transport

Implement a complete SSE transport in this change.

Do not add WebSocket support, a generic WebSocket stack, or an async-Tungstenite dependency in this implementation. WebSocket continuation is an optimization, not a correctness requirement. Tea can send complete canonical context over SSE and retain prompt-cache identity.

Also do not add zstd merely to compress request bodies. The backend accepts uncompressed JSON, and a compression dependency is not justified for the initial Tea provider.

Leave clean internal seams for a later transport optimization, but do not ship speculative abstractions or unused transport enums.

## No remote Codex compaction in this change

Use Tea’s existing provider-agnostic compaction path. A compaction summary is an ordinary tool-free Codex request followed by Tea’s transactional compactor.

Do not send Codex backend-specific compaction-trigger items in this change.

# Feature and crate wiring

Follow the existing provider feature pattern.

At minimum, wire `provider-codex` through every crate that currently forwards provider features, likely including:

```text
tea-providers
tea-core
the terminal/application crate
workspace convenience features, when present
```

Requirements:

* Disabled by default.
* No Tokio dependency.
* No reqwest dependency.
* No OpenAI SDK.
* All HTTP through `tea-http`.
* Reuse the workspace executor bridge.
* Reuse existing JSON infrastructure where practical.
* Reuse existing bounded retry and diagnostic types.
* Preserve `cargo check` with no provider features.
* Preserve builds with every existing provider combination.

Add the module export only under the feature:

```rust
#[cfg(feature = "provider-codex")]
pub mod codex;
```

# Module structure

Use the repository’s existing conventions, but converge on approximately this structure:

```text
crates/tea-providers/src/codex/
    mod.rs
    config.rs
    auth.rs
    oauth.rs
    credentials.rs
    context.rs
    payload.rs
    stream.rs
    error.rs
    wire.rs
    tests.rs
```

Do not force one-file-per-concept when the implementation is small, but keep these boundaries explicit:

* `wire`: constants and backend contract.
* `credentials`: secret-bearing data and persistence abstractions.
* `oauth`: browser/device/token endpoints.
* `auth`: refresh coordination and request snapshots.
* `context`: Tea transcript to Responses input.
* `payload`: deterministic request serialization.
* `stream`: incremental SSE reduction.
* `error`: provider-safe classifications and diagnostics.
* `config`: validated construction and timeouts.

Before duplicating code, inspect `opencode_zen`. Extract genuinely provider-neutral OpenAI Responses machinery into a private shared module when that reduces duplication without weakening type boundaries.

Suitable shared components include:

* Responses input item types.
* Function-tool serialization.
* Incremental SSE record framing.
* Common Responses event decoding.
* Usage field decoding.

Keep these provider-specific:

* Authentication.
* Backend URL.
* Headers.
* Codex request defaults.
* Subscription quota errors.
* Encrypted reasoning continuity.
* Client identity and compatibility version.
* Codex-specific terminal event aliases.

Do not turn the provider implementation into a generalized “support every OpenAI-compatible server” framework.

# Exact OAuth contract

## Constants

Use:

```text
OAuth issuer:
https://auth.openai.com

Public client ID:
app_EMoamEEZ73f0CkXaXp7hrann

Authorize endpoint:
https://auth.openai.com/oauth/authorize

Token endpoint:
https://auth.openai.com/oauth/token

Revocation endpoint:
https://auth.openai.com/oauth/revoke

Primary browser redirect:
http://localhost:1455/auth/callback

Fallback browser redirect:
http://localhost:1457/auth/callback

Device user-code endpoint:
https://auth.openai.com/api/accounts/deviceauth/usercode

Device polling endpoint:
https://auth.openai.com/api/accounts/deviceauth/token

Device verification page:
https://auth.openai.com/codex/device

Device token-exchange redirect:
https://auth.openai.com/deviceauth/callback
```

The client ID is a public native-client identifier, not a secret. Never introduce a client secret.

## Browser authorization

Implement OAuth authorization code + PKCE S256.

Generate with an operating-system CSPRNG:

* A high-entropy PKCE verifier.
* SHA-256 code challenge.
* A separate high-entropy state value.

Reuse cryptographic dependencies already present in the workspace. When none are suitable, add the smallest audited, feature-gated dependencies needed for secure randomness, SHA-256, and base64url. Do not implement a custom PRNG or custom SHA-256.

Build the authorization URL with these parameters:

```text
response_type=code
client_id=<public-client-id>
redirect_uri=<the-bound-allowlisted-redirect>
scope=openid profile email offline_access
code_challenge=<base64url-no-pad SHA256(verifier)>
code_challenge_method=S256
state=<random-state>
id_token_add_organizations=true
codex_cli_simplified_flow=true
originator=tea
```

Tea does not implement OpenAI connectors, so deliberately omit Codex’s connector-read/invoke scopes. Document that least-privilege divergence.

## Callback server

Implement a small bounded loopback callback server:

* Bind only `127.0.0.1`.
* Try port 1455 first.
* Try port 1457 if 1455 is unavailable.
* Do not choose a random port; these redirects are allowlisted.
* Accept only `/auth/callback`.
* Validate the state before accepting a code.
* Surface OAuth `error` and `error_description` safely.
* Place strict limits on the request line and headers.
* Apply a finite login timeout.
* Return a small static success or failure page.
* Do not reflect unescaped provider strings into HTML.
* Close the connection explicitly.
* Ensure the listener shuts down on success, failure, cancellation, or timeout.

Launch the browser without adding a web-browser dependency:

* macOS: spawn `open` with the URL as one argument.
* Linux: spawn `xdg-open` with the URL as one argument.
* Never invoke a shell.
* Failure to launch a browser is nonfatal; print the URL.

Support `--no-open`, which always prints the authorization URL.

When neither callback port can be bound, provide a manual completion path that accepts the final callback URL, `code#state`, or code plus separately retained state. Validate state wherever supplied.

## Browser token exchange

POST form-encoded data to `/oauth/token`:

```text
grant_type=authorization_code
client_id=<public-client-id>
code=<authorization-code>
code_verifier=<pkce-verifier>
redirect_uri=<exact-redirect-used-for-authorization>
```

Require:

* `access_token`
* `refresh_token`
* `expires_in`

Accept `id_token` when returned, but do not retain it unless the current implementation proves it is necessary. Extract required account metadata, then discard it.

## Device flow

Implement:

```text
tea auth login codex --device
```

Start the device flow with:

```http
POST /api/accounts/deviceauth/usercode
Content-Type: application/json

{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}
```

Parse:

```text
device_auth_id
user_code
interval
```

Display the verification page and user code clearly, then poll the device token endpoint with:

```json
{
  "device_auth_id": "...",
  "user_code": "..."
}
```

Handle these states:

* HTTP 403 or 404 while authorization remains pending.
* `deviceauth_authorization_pending`.
* `slow_down`, increasing the polling interval.
* Cancellation.
* A 15-minute overall deadline.
* Any definitive denial or malformed response as terminal.

On success, parse:

```text
authorization_code
code_verifier
```

Exchange that authorization code at `/oauth/token` using the device redirect URI.

Keep browser and device token-exchange logic shared after authorization-code acquisition.

## Account ID extraction

The request requires a ChatGPT account ID.

Decode JWT payloads as untrusted base64url JSON and look under:

```text
https://api.openai.com/auth
```

for:

```text
chatgpt_account_id
```

Prefer the access token because it is the actual bearer used by Pi’s working implementation. Fall back to the ID token when necessary.

Do not treat local JWT decoding as signature verification or authorization. The token remains opaque server-issued credential material.

Fail login clearly when no account ID can be obtained.

## Refresh

Refresh before expiry, using a five-minute safety window:

```http
POST https://auth.openai.com/oauth/token
Content-Type: application/x-www-form-urlencoded
```

Body:

```text
grant_type=refresh_token
refresh_token=<current-refresh-token>
client_id=<public-client-id>
```

Requirements:

* Use a single-flight refresh within the process.
* Prevent concurrent Tea processes from racing the same rotating refresh token.
* Reload the credential record after acquiring the cross-process lock because another process may already have refreshed it.
* Persist a newly returned refresh token atomically.
* When a conforming response omits `refresh_token`, retain the current refresh token rather than erasing it.
* Commit the entire new credential snapshot in one atomic replacement.
* Do not expose the new access token to request code until persistence succeeds.
* Classify expired, reused, revoked, invalidated, and other permanent refresh failures.
* On permanent refresh failure, retain a clear “login required” state rather than repeatedly retrying.
* Retry transient transport/5xx refresh failures only within a strict bounded policy.

Use the smallest robust cross-process locking mechanism consistent with the repository and supported targets. Do not add a heavyweight locking dependency. Reuse an existing lock primitive when available; otherwise implement a narrowly scoped lockfile/OS lock for macOS and Linux.

## Logout

Implement:

```text
tea auth logout codex
```

Attempt token revocation through `/oauth/revoke`, but remove local credentials even if revocation fails.

Never print the revoked token.

# Credential persistence

Use Tea’s existing resolved state/config root. Do not create a competing home-directory convention.

Store a single current schema:

```text
auth/codex.json
```

Conceptual v1 shape:

```json
{
  "version": 1,
  "provider": "codex",
  "access_token": "...",
  "refresh_token": "...",
  "expires_at_unix_ms": 0,
  "account_id": "...",
  "obtained_at_unix_ms": 0
}
```

Do not add legacy readers, migration layers, or multiple schema versions.

Requirements:

* Parent auth directory mode `0700` where supported.
* Credential file mode `0600`.
* Atomic write via a sibling temporary file and rename.
* Flush file contents before replacement.
* Preserve a valid old record when writing a new record fails.
* Reject unsupported versions and malformed records.
* Reject empty access token, refresh token, or account ID.
* Reject absurd or overflowing timestamps.
* Do not follow a credential-file symlink when the platform APIs permit safe prevention.
* Secret-bearing types must implement redacted `Debug`.
* No token value may appear in `Display`, errors, traces, diagnostics, session records, snapshots, or test failure output.
* Do not persist the OpenAI API key that first-party Codex may optionally obtain. This provider is deliberately subscription-only.

Make time and credential storage injectable so tests do not depend on wall-clock time or the real filesystem.

# Auth manager contract

Create a shared auth manager that can supply a request-safe snapshot:

```rust
struct CodexAuthSnapshot {
    access_token: SecretString,
    account_id: String,
}
```

The exact public API should follow Tea conventions, but it must support:

* Loading the current credential.
* Refreshing before expiry.
* Forced refresh after an HTTP 401.
* Single-flight process-local refresh.
* Cross-process refresh serialization.
* Atomic persistence.
* An injected clock.
* An in-memory test store.
* Cancellation and finite network deadlines.

The provider must ask the auth manager for a fresh snapshot immediately before each attempt. It must not cache an access token independently.

On HTTP 401:

1. If no model-stream event has been exposed, force one refresh.
2. Rebuild headers from the refreshed snapshot.
3. Retry the logical request once.
4. Never enter an unlimited 401-refresh loop.
5. Never replay after any visible stream event.

# Exact inference request

## Endpoint and method

```http
POST https://chatgpt.com/backend-api/codex/responses
```

## Required headers

For the SSE path, send:

```text
Authorization: Bearer <access-token>
ChatGPT-Account-ID: <account-id>
originator: tea
version: <CODEX_WIRE_COMPAT_VERSION>
User-Agent: tea/<tea-version>
OpenAI-Beta: responses=experimental
Accept: text/event-stream
Content-Type: application/json
session-id: <stable-tea-session-id>
x-client-request-id: <stable-logical-request-id>
```

Header behavior:

* The Tea session ID is stable across turns and retries of a resumed Tea session.
* Generate one client request ID per logical model turn.
* Preserve that request ID across retries of the same logical turn.
* Never derive either ID from a token, account ID, repository path, or user prompt.
* Validate all header values before network I/O.
* Do not log authorization or account-ID headers.
* Keep header names centralized in `wire.rs`.
* Contract tests must pin the complete nonsecret header set.

Use the existing Tea user-agent helper when one exists, provided it still identifies Tea honestly.

Treat `OpenAI-Beta: responses=experimental` as a pinned protocol constant. Verify it against the current upstream contract during implementation. When current Codex has definitively superseded it, update the constant and fixture together rather than accepting arbitrary user configuration.

## Request body

Produce deterministic JSON with this conceptual shape:

```json
{
  "model": "<model-id>",
  "store": false,
  "stream": true,
  "instructions": "<effective-system-prompt>",
  "input": [],
  "tools": [],
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "reasoning": {
    "effort": "high",
    "summary": "auto"
  },
  "text": {
    "verbosity": "low"
  },
  "include": [
    "reasoning.encrypted_content"
  ],
  "prompt_cache_key": "<stable-tea-session-id>"
}
```

Omit optional fields rather than serializing nulls unless the backend specifically requires null.

Always send:

```text
store=false
stream=true
parallel_tool_calls=true
include=["reasoning.encrypted_content"]
```

Send a stable `prompt_cache_key` derived from the Tea session ID.

Do not send:

```text
max_tokens
messages
prompt_cache_retention
previous_response_id
service_tier
```

unless inspection of the current Codex source establishes that a field is required for the SSE contract. In particular, do not copy Chat Completions field names into a Responses request.

Omit `temperature` by default. Do not add an unconditional Tea temperature override. Codex models frequently own their sampling defaults and may reject or mishandle caller-set temperature.

When Tea exposes an explicit request-level temperature, include it only when the selected model contract confirms support. Otherwise return a typed unsupported-option error rather than silently changing behavior.

For Tea’s output cap, inspect the current Codex request contract. Use `max_output_tokens` only when upstream confirms support. Never translate it to Chat Completions `max_tokens`. When the subscription endpoint does not support a reliable wire cap, preserve Tea’s local wall/cancellation budget without inventing a request field.

## Reasoning

Map Tea reasoning levels to Codex Responses reasoning effort:

```text
off      -> none or omit, according to current model contract
minimal  -> minimal
low      -> low
medium   -> medium
high     -> high
xhigh    -> xhigh
```

Use Tea’s existing enum and model-specific clamping behavior. Do not let arbitrary strings reach the request.

Default reasoning summary:

```text
auto
```

Do not request or expose raw chain-of-thought.

## Text verbosity

Default:

```json
{"verbosity":"low"}
```

Use a typed enum when Tea already exposes a verbosity setting. Do not pass arbitrary strings.

# Responses context conversion

Do not use Chat Completions `messages` as the Codex wire shape.

Implement a typed Responses input representation.

## System instructions

Place the effective Tea system prompt in top-level:

```text
instructions
```

Do not duplicate it as an input message.

## User messages

Represent text using a Responses message/input-text item, matching the exact shape accepted by Codex at the pinned upstream contract.

## Assistant messages

Replay prior assistant-visible output using the accepted Responses assistant message/output-text shape.

Do not serialize diagnostic text, UI status, provider errors, hidden planning state, or raw reasoning as assistant content.

## Function calls

Replay an assistant function call as a Responses function-call item containing at least:

```text
type
call_id
name
arguments
```

Preserve the original `call_id` exactly.

Do not generate a new call ID during replay.

## Function results

Replay a tool result as:

```text
type=function_call_output
call_id=<matching-call-id>
output=<projected-tool-result>
```

Apply Tea’s existing `ToolResultProjectionPolicy` before provider serialization. Canonical raw results remain in Tea state; only the bounded model-facing projection is sent.

Preserve error marking and recovery guidance according to the existing projection policy.

Do not mislabel a tool result as a user message.

## Tool definitions

Serialize Tea function tools into Responses function definitions with:

```text
type=function
name
description
parameters
```

Match the current backend’s accepted `strict` behavior. Pi intentionally uses a null/non-strict representation for some Codex tools; do not blindly force `strict=true`.

Requirements:

* Preserve complete JSON Schema metadata.
* Preserve descriptions.
* Reject invalid or duplicate tool names before network I/O.
* Deterministically order tools according to Tea’s canonical ordering.
* Omit `tools` completely for a tool-free compaction call.
* Set `tool_choice` only to supported typed values: `auto`, `none`, or `required`.
* Support multiple concurrent tool calls in one response.

# Encrypted reasoning continuity

This is mandatory.

Codex Responses may return reasoning output items containing opaque `encrypted_content`. That data is not visible reasoning text, but it must be replayed in later requests so the model can retain the intended reasoning context.

Do not:

* Drop it indiscriminately.
* Render it in the terminal.
* Treat it as assistant text.
* Log it.
* Parse or reinterpret it.
* Expose it to Lua tools.
* Include it in user-visible session dumps by default.

Use Tea’s existing provider-metadata or opaque-continuation facility when one already exists.

When no suitable facility exists, add the smallest generic core capability necessary, conceptually:

```rust
struct OpaqueProviderContextItem {
    provider: ProviderId,
    kind: BoundedString,
    payload: BoundedBytes,
}
```

The actual representation must follow Tea’s strongly typed persistence model.

Requirements:

* Provider-scoped.
* Bounded.
* Durable across session save/resume.
* Ordered relative to the associated model output items.
* Invisible to ordinary transcript rendering.
* Available only to the matching provider adapter.
* Never reused after switching provider or model incompatibly.
* Removed when its associated turns are removed by committed compaction.
* Not copied into a child session unless canonical transcript semantics require it.
* Included in the next Codex Responses `input` using the exact upstream reasoning-item shape.

Prefer storing a typed minimal representation of the Codex reasoning item over unbounded arbitrary JSON. Preserve only fields needed for exact replay, such as the item ID/type and encrypted payload.

Add a two-turn test proving that encrypted reasoning returned on turn one appears exactly once, in the correct order, in turn two’s request.

# Streaming SSE implementation

Implement an incremental parser over the byte stream delivered by `tea-http`.

Do not buffer the full response.

The parser must handle:

* Arbitrary transport chunk boundaries.
* A JSON event split at every possible byte.
* Multiple SSE records in one body chunk.
* `\n\n` and `\r\n\r\n`.
* Multiple `data:` lines in one event.
* Optional `event:` lines.
* Comment/keepalive lines.
* UTF-8 split across body chunks.
* A final record without a trailing blank line.
* `[DONE]`.
* Unknown forward-compatible event types.
* Strict bounded buffering.
* Malformed UTF-8.
* Malformed JSON.
* Premature EOF.

Reduce by the JSON payload’s `type` field rather than relying only on the SSE `event:` line.

Support at least the current equivalents of:

```text
response.created
response.in_progress
response.output_item.added
response.content_part.added
response.output_text.delta
response.output_text.done
response.reasoning_summary_text.delta
response.reasoning_summary_text.done
response.function_call_arguments.delta
response.function_call_arguments.done
response.output_item.done
response.completed
response.done
response.incomplete
response.failed
error
```

Inspect current Codex and Pi fixtures for exact names and payload nesting.

## Text

Map `response.output_text.delta` to Tea assistant text deltas immediately.

Do not wait for the final response object before exposing text.

Prevent duplicate final text when both deltas and final output items contain the same text.

## Reasoning summaries

When Tea has a distinct surfaced reasoning-summary/progress event, map summary deltas there.

Otherwise, retain or ignore them as bounded provider metadata. Never concatenate reasoning-summary text into the final assistant answer.

Never expose raw reasoning-text events as chain-of-thought.

## Function calls

Responses may interleave multiple function calls and argument fragments.

Maintain accumulator state keyed by stable item identity/output index, not a single global buffer.

Track:

* Output index.
* Item ID.
* Call ID.
* Function name.
* Argument bytes/string.
* Completion state.

Requirements:

* Interleaved calls remain distinct.
* Fragment order is preserved.
* A call emits exactly once.
* A duplicate `.done` does not duplicate the call.
* A final output item can complete a call when the dedicated done event is absent.
* Arguments remain the exact provider-produced JSON string.
* Validate syntactic JSON before delivering a completed native function call when Tea’s current contract requires valid JSON.
* Malformed arguments become a typed provider protocol failure, not an invented empty object.
* Do not emit a partial tool call into canonical transcript state.

## Terminal status

Normalize current Codex terminal variants:

```text
response.completed
response.done
response.incomplete
response.failed
error
```

Map them into Tea’s stable stop reasons.

At minimum distinguish:

* Normal stop.
* Tool use.
* Output-length/incomplete.
* Cancellation.
* Provider failure.
* Protocol failure.

Require exactly one terminal model event.

EOF before a terminal event is a stream-disconnected error.

## Usage

Parse final Responses usage:

```text
input_tokens
input_tokens_details.cached_tokens
output_tokens
output_tokens_details.reasoning_tokens
total_tokens
```

Map:

* Input tokens.
* Output tokens.
* Cache-read tokens.
* Total tokens.
* Reasoning tokens when Tea has a field for them.

Preserve unknown-versus-zero semantics.

A ChatGPT subscription provider does not expose a trustworthy per-request dollar cost. Record monetary cost as unknown, not `$0.00`.

Do not estimate API pricing for subscription traffic.

# Retry, timeout, and cancellation behavior

Reuse Tea’s existing bounded retry machinery.

## Default limits

Use repository-consistent configurable defaults, with approximately:

```text
whole request/header deadline: 300 seconds
started-response stall timeout: 60 seconds
OAuth finite request timeout: substantially shorter and bounded
```

The terminal host may derive tighter limits from an admitted assignment wall budget.

## Safe retries

Before any stream event has been exposed, retry only:

* Transport setup failures.
* Connection closure before headers.
* Retryable 429s that are not terminal subscription-quota exhaustion.
* HTTP 500, 502, 503, and 504.
* One forced-refresh replay after HTTP 401.

Honor valid:

```text
retry-after-ms
retry-after
```

Bound server-requested delays by Tea’s maximum retry delay.

Once text, a reasoning summary, a tool call, usage, or another externally visible model event has been emitted, do not replay the request.

## Terminal quota failures

Do not retry subscription exhaustion indefinitely.

Recognize current errors equivalent to:

```text
usage_limit_reached
usage_not_included
rate_limit_exceeded
GoUsageLimitError
FreeUsageLimitError
Monthly usage limit reached
insufficient_quota
available balance exhausted
out of budget
```

Parse reset metadata when present and preserve it in a bounded typed diagnostic.

Do not allow arbitrary backend prose to become agent instructions.

## Model rollout errors

For a 404/model-not-found response:

* Return the actual selected public model ID.
* Identify the provider as `codex`.
* Explain in the diagnostic that the model may be unavailable to Tea’s honest `originator`.
* Include the wire-compatibility version.
* Do not automatically retry another model.
* Do not spoof a first-party originator.
* Do not expose internal deployment names as canonical model IDs.

## Cancellation

Check Tea’s cancellation token:

* Before credential refresh.
* Before each request attempt.
* Before sending the body.
* Between received body chunks.
* Before emitting parsed events.
* Before retry delays.
* After stream completion.

Cancellation is terminal and non-retryable.

Preserve already emitted deltas, then emit the single appropriate cancellation terminal event.

# Diagnostics and redaction

Use or extend Tea’s existing bounded provider diagnostic type.

Retain useful trusted-host fields:

* Failure boundary.
* HTTP status.
* Provider error code.
* Retry classification.
* Attempt number.
* Request byte count.
* Response byte count.
* Bounded redacted response prefix.
* Logical request ID.
* Whether any stream event had been exposed.
* Whether auth refresh was attempted.

Never retain:

* Access tokens.
* Refresh tokens.
* ID tokens.
* Authorization header values.
* Full account IDs in ordinary logs.
* OAuth codes.
* PKCE verifiers.
* Full unbounded backend bodies.
* Encrypted reasoning payloads.

Redact JWT-like strings and known token JSON keys defensively.

Provider response text must not enter the model transcript unboundedly.

Persist the final bounded provider report through the existing `ProviderRequestSettledRecord` path.

# Configuration

Add a validated `CodexConfig` consistent with other Tea providers.

It should contain only provider-owned runtime choices, approximately:

```text
model
shared auth manager/token source
request timeout
response stall timeout
retry policy
reasoning defaults
text verbosity
```

It must not contain an ordinary arbitrary production endpoint.

Validation must reject:

* Empty model IDs.
* A model descriptor whose provider is not exactly `codex`.
* Zero/invalid timeouts.
* Unsupported reasoning values.
* Unsupported tool-choice values.
* Missing credential source.
* Unsafe test endpoints outside loopback.
* Header injection characters.

The provider must validate before making any network request.

# Terminal and host integration

Integrate with Tea’s current terminal/factory architecture rather than creating a parallel runner.

Required commands, adapted to the repository’s established CLI grammar:

```text
tea auth login codex
tea auth login codex --device
tea auth login codex --no-open
tea auth status codex
tea auth logout codex
```

Use the repository’s existing argument parser. Do not add Clap.

`auth status` may display:

* Logged in or logged out.
* Account identifier in safely abbreviated form.
* Credential expiry or refresh-required state.
* Credential path.

It must never display token material.

Provider selection must work through the existing model descriptor/factory path:

```text
provider: codex
model: <explicit model-id>
```

Do not silently make Codex the default provider.

Do not hard-code a default model that can become stale. Require the current host/model-selection machinery to provide an explicit model unless Tea already has a checked-in default-model policy.

Keep provider construction lazy:

* Selecting another provider must not load Codex credentials.
* Listing static help must not refresh credentials.
* Merely opening Tea must not start OAuth or contact OpenAI.
* The first actual Codex request may refresh an expired credential.

When the binary lacks `provider-codex`, selecting it must produce the same clear “provider not compiled” behavior used by existing optional providers.

# Model discovery

Do not make dynamic remote model discovery a prerequisite for inference.

When Tea already has a generic provider model-list capability, implement an authenticated Codex model-list operation against the current Codex models route and parse only the fields Tea needs, including where available:

```text
model slug/id
display name
visibility
supported_in_api
minimal_client_version
```

Filter or annotate models whose minimum client version exceeds `CODEX_WIRE_COMPAT_VERSION`.

When Tea does not already have such a capability, do not create a broad model-registry redesign in this change. Explicit model selection is sufficient.

# Compaction integration

Ensure Tea’s existing compactor can use the Codex provider.

A tool-free compaction request must:

* Omit tool definitions.
* Omit or set tool choice to `none` according to the exact backend contract.
* Use the same auth manager.
* Use the same stable session/prompt-cache identity.
* Stream or collect the summary through the normal model-event contract.
* Preserve usage.
* Commit through Tea’s transactional compactor only after a complete successful response.
* Leave canonical context untouched after cancellation or provider failure.

After successful compaction:

* Remove opaque reasoning items belonging only to replaced turns.
* Retain opaque provider context attached to retained canonical turns.
* Never leave orphan encrypted reasoning state.

Do not implement backend-specific remote compaction.

# Dependency discipline

Before adding any dependency, search the workspace for an existing equivalent.

Permitted dependency additions must be:

* Feature-gated under `provider-codex`.
* Small.
* Audited.
* Directly justified by OAuth security or protocol correctness.

Likely needs are secure randomness, SHA-256, and base64url if the workspace lacks them.

Do not add:

* Tokio.
* reqwest.
* hyper.
* an OpenAI SDK.
* a browser automation library.
* a WebSocket stack.
* an OAuth framework.
* a keyring framework.
* a general URL framework merely for a few fixed endpoints, unless one is already present.
* zstd solely for request compression.

Use `tea-http` for OAuth and inference HTTP.

# Tests

Work test-first. Add deterministic contract fixtures before or alongside implementation.

Normal tests must not access the internet, the real home directory, the real credential file, or an installed Codex client.

## OAuth tests

Cover:

1. PKCE verifier and challenge correctness using a known vector.
2. Secure state generation through an injected deterministic random source.
3. Exact browser authorization parameters.
4. Primary and fallback redirect selection.
5. State mismatch rejection.
6. Missing code rejection.
7. OAuth error propagation.
8. HTML escaping.
9. Exact token-exchange form body.
10. Device user-code request.
11. Device pending polling.
12. `slow_down`.
13. Device timeout.
14. Device cancellation.
15. Device authorization-code exchange.
16. Refresh request.
17. Refresh-token rotation.
18. Refresh response that omits a replacement refresh token.
19. Permanent refresh failure.
20. Transient refresh retry.
21. JWT base64url decoding with and without padding.
22. Account-ID extraction from access token.
23. ID-token fallback.
24. Missing account-ID failure.
25. Secret redaction in every error/debug surface.

## Credential tests

Cover:

1. v1 round trip.
2. Unsupported schema rejection.
3. Empty-secret rejection.
4. Atomic replacement.
5. Failed write preserves old credential.
6. Correct Unix permissions.
7. Symlink safety where supported.
8. In-process single-flight refresh.
9. Two simulated processes coordinating refresh.
10. Reload-after-lock behavior.
11. Concurrent requests observe one committed replacement token.
12. No token content in diagnostic formatting.

## Request golden tests

Pin exact deterministic JSON for:

1. Plain user request.
2. System instructions.
3. One previous assistant turn.
4. One function definition.
5. One function call and result.
6. Two parallel function calls with interleaved results.
7. Tool-free compaction request.
8. Every reasoning effort.
9. Default omitted temperature.
10. Prompt-cache/session ID.
11. Encrypted reasoning item replay.
12. Optional output cap only when supported.
13. Unicode.
14. Empty optional collections omitted appropriately.

Pin exact nonsecret headers:

```text
ChatGPT-Account-ID
originator
version
User-Agent
OpenAI-Beta
Accept
Content-Type
session-id
x-client-request-id
```

Assert that `Authorization` is present but redact its value in snapshots.

Assert that Tea never emits:

```text
api.openai.com
codex_cli_rs
prompt_cache_retention
max_tokens
messages
```

for this provider.

## SSE parser tests

Build sanitized fixtures from current Codex/Pi event shapes.

For every fixture, run the parser with:

* The whole body in one chunk.
* One byte per chunk.
* Every possible single split point.
* Several deterministic irregular chunk patterns.
* CRLF framing.
* Multi-line data records.
* Keepalive comments.
* Final record without trailing blank line.

Cover:

1. Text-only response.
2. Multiple text deltas.
3. Reasoning-summary deltas separated from final text.
4. Opaque encrypted reasoning capture.
5. One function call.
6. Interleaved parallel function calls.
7. Argument fragments split inside UTF-8 and JSON escapes.
8. Usage.
9. Cached-token usage.
10. `response.completed`.
11. `response.done`.
12. `response.incomplete`.
13. `response.failed`.
14. Top-level `error`.
15. Unknown event.
16. Duplicate done event.
17. Premature EOF.
18. Invalid UTF-8.
19. Invalid JSON.
20. Oversized SSE record.
21. `[DONE]`.

Assert exactly one terminal event.

## Provider integration tests

Use a local injected mock origin and real `tea-http` transport behavior.

Cover:

1. Successful streamed text.
2. Tool call followed by function output and final text.
3. Two-turn encrypted-reasoning replay.
4. Automatic pre-request refresh.
5. HTTP 401, forced refresh, successful replay.
6. Repeated HTTP 401 stops after one forced refresh.
7. Retryable 500 before stream.
8. Retryable 429 with bounded Retry-After.
9. Terminal subscription-quota 429 is not retried.
10. Disconnect before any event is retried.
11. Disconnect after text is not retried.
12. Cancellation before request.
13. Cancellation during refresh.
14. Cancellation during stream.
15. Stall timeout.
16. Model-not-found diagnostic without originator spoofing.
17. Usage reaches Tea accounting with cost unknown.
18. Bounded/redacted provider error persistence.
19. Compaction success.
20. Compaction failure leaves canonical context unchanged.

## CLI tests

Use existing CLI/PTY test patterns.

Cover:

1. `auth status` while logged out.
2. Browser login URL rendering with deterministic values.
3. `--no-open`.
4. Device login instructions.
5. Login completion writes the expected v1 record.
6. Status after login.
7. Logout.
8. No tokens appear in terminal captures.
9. Selecting `codex/<model>` reaches the provider factory.
10. Feature-disabled behavior.

## Live smoke test

Add one ignored, explicitly opted-in live test or small test binary.

It must:

* Require an explicit test flag/environment opt-in.
* Require an explicit Tea-owned test credential path.
* Never read `~/.codex/auth.json`.
* Send a trivial tool-free prompt.
* Verify at least one assistant delta and one terminal event.
* Never print tokens.
* Never run in ordinary CI.

A missing live credential is not a test failure in the normal suite.

# Documentation

Update:

```text
docs/provider-adapters.md
```

Add a focused durable document such as:

```text
docs/codex-provider.md
```

Document:

* This is the `codex` provider.
* It uses a ChatGPT subscription and ChatGPT OAuth.
* It does not use an OpenAI Platform API key.
* It is opt-in and experimental because the direct third-party backend contract is not formally documented.
* Exact auth commands.
* Exact model-selection shape.
* Credential location and permissions.
* Automatic refresh behavior.
* Honest `originator: tea`.
* The possibility of rollout-gated models.
* Why first-party originator spoofing is prohibited.
* Why Tea does not reuse `~/.codex/auth.json`.
* Why SSE is used initially.
* Why WebSockets, zstd, remote compaction, and connector scopes are excluded.
* How `CODEX_WIRE_COMPAT_VERSION` is maintained.
* Which upstream commits and files define the current contract.
* A concise upstream-update checklist.
* Troubleshooting for expired/revoked login, 401, quota exhaustion, and model-not-found.

Do not create a temporary `plan.md`.

Route new durable documentation through `AGENTS.md` only when the current repository already uses that thin-router convention.

# Targeted implementation sequence

Execute in this order:

1. Record the exact local starting commit and preserve any existing user changes.
2. Inspect only the targeted Tea files and locate feature/factory/CLI wiring with the supplied `rg` query.
3. Inspect the pinned Codex and Pi contract files.
4. Write sanitized request, OAuth, and SSE golden fixtures.
5. Factor the minimum provider-neutral Responses code from `opencode_zen`, with all existing tests still passing.
6. Add Codex wire constants and validated configuration.
7. Implement typed Responses context and payload construction.
8. Implement the incremental stream reducer.
9. Add opaque encrypted-reasoning continuity.
10. Implement credential types, v1 persistence, and locking.
11. Implement OAuth browser and device flows.
12. Implement refresh and 401 recovery.
13. Implement `CodexProvider`.
14. Wire provider features, registry, lazy factory, accounting, and diagnostics.
15. Add terminal auth commands.
16. Prove ordinary agent, tool, resume, and compaction flows in integration tests.
17. Update durable docs.
18. Run formatting, linting, all feature combinations, and tests.
19. Review the final diff for leaked secrets, accidental Tokio/reqwest dependencies, duplicated Responses machinery, first-party spoofing, and default-feature growth.

Do not leave placeholder TODOs for required behavior.

# Acceptance criteria

The work is complete only when all of the following are true:

* `provider-codex` is disabled by default.
* Feature-disabled builds remain clean.
* No Tokio, reqwest, OpenAI SDK, WebSocket, or zstd dependency was added.
* All provider HTTP uses `tea-http`.
* Tea has its own browser and device OAuth flow.
* Tea does not read or mutate Codex’s credential file.
* Refresh-token rotation is atomic and concurrency-safe.
* Credential material is never logged or persisted outside the credential record.
* The backend request uses `/backend-api/codex/responses`.
* `originator` is exactly `tea`.
* The provider never impersonates `codex_cli_rs`.
* The version header declares the pinned Codex wire-compatibility level.
* Requests use Responses `input`, not Chat Completions `messages`.
* Temperature is omitted by default.
* Function tools and parallel calls work end to end.
* Assistant text streams incrementally.
* Encrypted reasoning content survives a session save/resume and is replayed correctly.
* Raw reasoning is never exposed as assistant text.
* Usage preserves unknown-versus-zero semantics.
* Subscription cost remains unknown rather than fabricated.
* Retry never replays a partially exposed stream.
* 401 recovery performs at most one forced refresh replay.
* Quota exhaustion is terminal and clear.
* Provider diagnostics are bounded and redacted.
* Existing providers and tests remain unchanged in behavior.
* Normal tests are fully offline and deterministic.
* Documentation captures the exact contract and maintenance procedure.
* There are no required TODOs, ignored compiler warnings, dead compatibility code, or uncommitted generated artifacts.

# Verification

Use the repository’s documented check commands when they are stricter than the generic commands below. At minimum run the applicable equivalents of:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test --workspace --all-features
```

Also explicitly test representative feature combinations:

```text
no provider features
provider-codex only
provider-opencode-zen only
provider-codex + provider-opencode-zen
all provider features
```

Inspect the resolved dependency tree for `provider-codex` and for the default build. Confirm that the default build did not gain Codex-only dependencies.

Run the repository’s PTY, persistence, compaction, provider, and session-resume test suites where they are separate.

Do not claim live backend verification unless the ignored live smoke test was actually run with a separate Tea-owned OAuth credential.

# Final report

At completion, report:

1. The exact implementation summary.
2. The final file/module structure.
3. The OAuth and credential-storage contract.
4. The request/stream contract implemented.
5. Intentional differences from first-party Codex and Pi.
6. Dependency changes and why each was necessary.
7. Every verification command run and its result.
8. Whether the ignored live smoke test was run.
9. Any remaining upstream-contract risk.

Do not describe unfinished required work as a future follow-up.
