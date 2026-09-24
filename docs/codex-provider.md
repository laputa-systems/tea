# Codex provider

`codex` is an experimental, opt-in native Tea provider for a ChatGPT Codex
subscription. It reads the installed Codex client's current access token or
uses Tea-owned ChatGPT OAuth, and implements the direct Codex Responses SSE
contract in Tea; it does not use an OpenAI Platform API key, the Codex SDK, or
the installed `codex` executable. The direct third-party backend contract is
not a formally documented public API, so change it only with new evidence.

## Enable and select it

The feature is disabled by default:

```bash
cargo run -p tea-agent --features provider-codex -- auth status codex
```

When running from this checkout, use that same feature-enabled invocation for
login; an installed `tea` executable must likewise have been built with
`provider-codex`. An existing `codex login` is enough to provide credentials.
Select an explicit current model ID. Tea deliberately has no checked-in Codex default
model.

```bash
cargo run -p tea-agent --features provider-codex -- auth login codex
tea auth login codex
tea auth login codex --no-open
tea auth login codex --device
tea auth status codex
tea auth logout codex

tea --provider codex --model gpt-5.6-luna --thinking low
```

`--no-open` prints the browser URL without launching it. The browser flow binds
only the allowlisted loopback callbacks on ports 1455 and 1457; if neither is
available, it accepts a state-validated pasted completion value. The device
flow is suitable for a headless machine.

## Credentials and refresh

The terminal first uses `$CODEX_HOME/auth.json`, or `~/.codex/auth.json` when
`CODEX_HOME` is unset, if that file exists. Tea uses the access token,
account ID, and JWT expiry, and reloads the file for each request. It never
copies the installed client's refresh token, writes to that file, or refreshes
it independently. If its access token is near expiry or rejected, renew it
with `codex login`; Tea then picks up the new token on the next request.

The optional `tea auth login codex` stores one v1 record at `~/.tea/auth/codex.json`.
`--tea-home PATH` changes that root, so the record is `PATH/auth/codex.json`.
Tea uses this record when no installed Codex client auth file exists.
`tea auth logout codex` removes only this Tea-owned record; it does not log out the
installed Codex client.
`tea auth status codex --tea-home PATH` inspects the Tea-owned record at that
explicit root. Without `--tea-home`, status prefers the installed client.
On Unix, Tea creates the `auth` directory with mode `0700` and writes the
record with mode `0600`. Replacement is flushed to a sibling temporary file
and atomically renamed.

Tea never imports the installed client's rotating refresh token, browser
cookies, or ChatGPT local storage. Sharing refresh tokens can invalidate both
programs. The separate Tea-owned record contains only subscription OAuth material; it never stores an
OpenAI Platform API key.

For a Tea-owned record, `CodexAuthManager` refreshes within five minutes of expiry, serializes refresh
within a process and through a Tea-owned file lock across Tea processes,
reloads after taking that lock, and atomically commits a whole replacement
before a request can use it. A response without a replacement refresh token
retains the current rotating token. A permanent refresh failure requires a new
`tea auth login codex`.

## Direct backend boundary

The provider sends only to
`https://chatgpt.com/backend-api/codex/responses`, through `tea-http`. It never
sends a ChatGPT subscription bearer token to `api.openai.com`, and there is no
ordinary endpoint override.

Requests identify Tea honestly:

```text
originator: tea
User-Agent: tea/<tea-version>
```

Tea must not spoof first-party values such as `codex_cli_rs`, `codex-tui`, or
`codex_vscode`. A model that is visible to official Codex can still be
rollout-gated for Tea's honest originator. A 404 reports the exact selected
`codex/<model-id>` and wire compatibility version rather than retrying another
model or impersonating a first-party client.

`CODEX_WIRE_COMPAT_VERSION` in `crates/tea-providers/src/codex/wire.rs` is a
pinned wire-capability declaration, not Tea's product identity. It is sent in
the `version` header; its provenance comment names the upstream Codex commit.

## Context, tools, and continuity

`CodexContextHook` in `crates/tea-providers/src/codex/context.rs` converts Tea
transcript state to Responses `input` items. Effective system instructions go
in top-level `instructions`, not a Chat Completions `messages` array. Function
definitions, calls, and outputs use native Responses shapes. Every request
uses `tool_choice: "auto"`; each native function definition carries
`strict: null`, matching the pinned non-strict Codex wire shape. Tool-free
compaction calls omit tool definitions and use Tea's normal transactional
compactor.

Codex may return encrypted reasoning state. Tea preserves only a typed,
provider-scoped minimal continuation record (`type`, optional `id`, `summary`,
and encrypted content) alongside the settled assistant turn; it is not
rendered, placed in ordinary transcript output, exposed to tools or Lua, parsed
as reasoning text, or reused by another provider. The next compatible Codex
request emits it once before the associated visible assistant content. It
survives session reopen and disappears with replaced turns during committed
compaction. Selecting a different provider or model starts a fresh durable
session in the terminal, so this provider-private state cannot cross a
model-selection boundary. The normal `tea session dump` view redacts its
payload while preserving the provider/kind/item identity needed for diagnosis.

The initial transport is Responses SSE. It supports incremental text, parallel
function calls, usage, cancellation, bounded pre-stream retry, and one forced
refresh/replay after a pre-output 401. Once a visible event is emitted, Tea does
not replay the request. Retry delay honors `retry-after-ms`, numeric
`Retry-After`, and HTTP-date `Retry-After` headers within Tea's configured
bound. Subscription cost remains unknown rather than being estimated as
Platform API spend.

Tea intentionally excludes WebSockets, zstd request compression, remote Codex
compaction trigger items, and connector scopes. These are optimizations or
extra privilege that are not needed for Tea's correctness boundary.

## Contract provenance and update checklist

The current provider contract was researched from OpenAI Codex commit
`63d213884daea50e4f74efc192cdc44f549b67d5` (`codex-rs/model-provider*`,
`codex-rs/login/*`, `codex-rs/protocol/src/auth.rs`,
`codex-rs/core/src/client.rs`, `codex-rs/codex-api`, and
`codex-rs/http-client`) and Pi commit
`853a80d26c90a14c1886f0ebb8ffaae133ca2185`
(`packages/ai/src/api/openai-codex-responses.ts`, shared Responses helpers,
OAuth/device-code code, and Codex OAuth/stream tests).

When upstream Codex changes:

1. Inspect the pinned upstream paths and an independent implementation for the
   changed OAuth, headers, payload, or event semantics.
2. Update `codex/wire.rs`, including `CODEX_WIRE_COMPAT_VERSION` and its
   provenance comment.
3. Update sanitized OAuth, payload, header, and SSE fixtures/tests; retain the
   opaque-reasoning and pre-stream replay invariants.
4. Re-run offline feature and default coverage, then perform the opt-in live
   smoke with an explicit installed Codex or Tea-owned credential path.
5. Record intentional least-privilege or transport divergences in this file.

## Explicit live smoke gate

The repository includes one ignored direct-backend test named
`live_chatgpt_subscription_smoke`. It is never part of normal CI. Run it only
with all three explicit values:

```bash
TEA_CODEX_LIVE_SMOKE=1 \
TEA_CODEX_CREDENTIAL_PATH=/absolute/path/to/.codex/auth.json \
TEA_CODEX_LIVE_MODEL=<current-model-id> \
cargo test -p tea-providers --features provider-codex \
  live_chatgpt_subscription_smoke -- --ignored
```

It sends one tool-free prompt to the fixed ChatGPT Codex endpoint and requires
at least one assistant delta plus a terminal event. It prints neither
credentials nor their contents. With a Tea-owned login, use the explicit
`~/.tea/auth/codex.json` path or `PATH/auth/codex.json` for `--tea-home PATH`.

## Troubleshooting

- **Expired, revoked, or rejected login:** renew the selected credential with
  `codex login` or `tea auth login codex`. Tea-owned records allow one forced
  refresh after a pre-output 401; client-owned records are never refreshed by Tea.
- **Quota exhaustion:** subscription limits are terminal, not ordinary retries.
- **Model not found:** confirm the exact model spelling and account rollout.
  Tea will not spoof a first-party originator to bypass rollout gating.
- **No browser available:** use `tea auth login codex --device`, or `--no-open`
  and open the printed URL elsewhere.
