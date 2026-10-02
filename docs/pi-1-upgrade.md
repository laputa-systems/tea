# Pi 1.0 upgrade record

A selective port of upstream Pi 1.0 behavior into tea, keeping tea's
single-session, embeddable, executor-agnostic shape.

- **Upstream reference:** Pi `a13d35a742c6ef8462812a28fbe1d8c8b7431c32`
  ("Release v1.0.0", 2026-10-01), read-only checkout. Tea tests do not
  require it to be present.
- **Tea base:** `5baee99` ("pi 1"). Work is on branch `pi-1-upgrade`.
- **No new external dependency.** `rustix` and `smol` were already used by the
  terminal; tests gained only path dev-dependencies on workspace crates.

## What changed, by area

| Area | Summary | Detail |
| --- | --- | --- |
| Typed requests and configuration | `ModelRequest` carries a typed `Transcript` (purpose, physical and selected model, output cap). Prompt sections and declared tools are transcript-ordered `System` configuration updates, durable as `configuration_changed` entries, replayed in place or collapsed per provider capability. | [architecture](architecture.md), [semantics](semantics.md) |
| Native Anthropic | `provider-anthropic`: API-key Messages adapter ported from Pi with request/SSE fixture tests. | [anthropic-provider.md](anthropic-provider.md) |
| Thinking | Provider-exposed thinking streams and persists separately from answer text across Anthropic, OpenRouter, local, and Codex; signatures stay private; cross-model replay drops it. | [anthropic-provider.md](anthropic-provider.md), [tui.md](tui.md) |
| Cache warming | Active-work prompt-cache maintenance ported from Pi's `cache-warmer.ts` (streaming mode). | [cache-warming.md](cache-warming.md) |
| Discovery and codemode | Authorized / discoverable / declared tools; BM25 `tool_search`; core composition facility; Luau `codemode`. | [discovery-and-codemode.md](discovery-and-codemode.md) |
| MCP | Host-side local stdio servers as deferred tools; fake server and example. | [mcp.md](mcp.md), [examples/mcp](../examples/mcp/README.md) |
| Virtual models | Extension routers over host-approved physical models; plan/build example. | [virtual-models.md](virtual-models.md) |
| Crash isolation | Headful sessions run as a session-owned runtime behind a disposable relay; `tea --attach SESSION_ID`. | [crash-isolation.md](crash-isolation.md) |

## Provenance

| Pi source | Tea counterpart |
| --- | --- |
| `packages/ai/src/api/anthropic-messages.ts`, `providers/anthropic*.ts` | `crates/tea-providers/src/anthropic/` |
| `packages/ai/test/anthropic-*.test.ts`, `system-message-replay.test.ts`, `transform-messages-*.test.ts` | `crates/tea-providers/src/anthropic/tests/`, `crates/tea-core/src/transcript/tests.rs` (mapping in [anthropic-provider.md](anthropic-provider.md)) |
| `packages/ai/src/api/openai-completions.ts` reasoning fields | `crates/tea-providers/src/openai.rs::chat_reasoning_text` |
| `packages/coding-agent/src/core/cache-warmer.ts`, `test/cache-warmer.test.ts` | `crates/tea-core/src/cache_warming.rs`, `crates/tea-core/tests/cache_warming.rs` |
| `packages/coding-agent/src/extensions/tool-search/tool.ts`, `test/tool-search.test.ts` | `crates/tea-core/src/tool_search.rs` and tests |
| `packages/coding-agent/src/extensions/codemode/`, `docs/codemode.md` (concept only) | `crates/tea-core/src/run/nested.rs`, `crates/tea-luau/src/codemode.rs` |
| `packages/coding-agent/src/core/virtual-models.ts`, `examples/extensions/jev-router.ts` (concept) | `crates/tea-core/src/routing.rs`, `crates/tea-luau/examples/plan_build_router/` |

## Decisions that matter

1. **Configuration is transcript data.** Prompt/tool changes are typed
   `System` messages in conversation order, committed before the request that
   first uses them, so resume, fork, and compaction reconstruct exposure
   exactly. Hooks may annotate context but cannot change configuration;
   requests that declare unauthorized tools fail.
2. **Capabilities are declared, never inferred.** `ModelCapabilities`
   (in-place updates, exposed thinking, prompt-cache lifetime and replay
   safety, prices, context window) come from the adapter for the physical
   model.
3. **Maintenance lives inside the run.** The warmer is polled in the run's
   own future with a host clock — no spawned task, daemon, or scheduler — and
   its usage is a separate durable record.
4. **Composition goes through the run.** Nested calls share preparation,
   hooks, effect gate, and cancellation with model calls and leave durable
   facts; nothing calls a capability behind the runtime.
5. **MCP and routing stay in hosts and extensions.** Core sees only trusted
   tools (`DynamicToolSource`) and `ModelRouter`s constrained to approved
   targets.
6. **Crash isolation reuses the application.** The runtime is the unchanged
   terminal app rendering into a virtual terminal; the relay carries bytes.
   No second agent protocol, broker, or session enumeration.

## Deliberate deviations and exclusions

- **Anthropic:** no images, OAuth/subscription, federation, Claude Code
  impersonation, strict schemas, `tool_choice`, server fallbacks,
  session-affinity headers, or `onPayload`; unsigned thinking is dropped from
  replay instead of flattened to text; `Off` maps to the lowest effort on
  managed-effort models; cost is an estimate, never `Usage.cost`.
- **Cache warming:** no idle mode, no extension decision hook, no tuning
  surface beyond on/off; budget-thinking replays are excluded via
  `MinimalOutputReplay::SafeWithoutThinking`.
- **Codemode:** Luau instead of QuickJS; no persistent `store`/`load`, no
  `models` (classifiers/images), no budgeted catalog in the description.
- **Discovery:** composition-only tools are never loadable for direct calls;
  no namespaces.
- **MCP:** local stdio only; no HTTP transport, OAuth, marketplace, ambient
  discovery, or automatic restart; schemas are reduced to tea's validated
  vocabulary.
- **Virtual models:** deterministic routers only (no classifier calls or
  speculative dispatch); continuations are sticky unless a model declares
  `routed`; the terminal ships one example router and offers it only when
  `[routing] approved_models` is configured.
- **Crash isolation:** rendering runs in the runtime, so a renderer bug
  (rather than terminal loss) still ends the runtime; the relay restores the
  terminal and durable recovery applies on reopen.
- **No multimodal content** anywhere.

## Compatibility

- The durable session format gained `configuration_changed` entries (replacing
  `tool_activation_changed`), assistant `content_blocks`/`origin`,
  `cache_maintenance` lane records, and `tea.nested-tool-effect.v1` facts.
  Older readers reject sessions that use them.
- Sessions created before the upgrade remain inspectable (`tea session
  inspect`, `dump`, `verify`), but reopening one for execution fails with an
  explicit hook-bundle identity mismatch: the terminal's hook bundle changed
  when provider-specific request hooks were removed in favor of typed
  transcripts. A pre-upgrade session containing `tool_activation_changed`
  cannot be opened. There is no silent migration.

## Enabling the features

```toml
# $TEA_HOME/config.toml
[anthropic]
cache_retention = "short"            # or "long" / "none"

[cache_warming]
enabled = true                       # default; false to disable

[features]
codemode = true

[mcp.servers.tracker]
command = "/path/to/server"

[routing]
approved_models = ["anthropic/claude-opus-5-5", "anthropic/claude-haiku-4-5"]
```

Run `tea --provider anthropic --model claude-sonnet-5-5` with
`ANTHROPIC_API_KEY` set, or `tea --provider virtual --model plan-build` with
routing configured. Reattach a headful session with
`tea --attach SESSION_ID` (the id is in the footer and in `/resume`); it
connects to the runtime still serving that session or reopens it. Use
`TEA_IN_PROCESS=1` to keep a terminal session in one process.

## Verification

All verification is offline: loopback HTTP fixtures, scripted providers,
virtual clocks, a fake MCP stdio server, subprocesses, and real PTYs. No
provider, local model, authentication flow, or external MCP service is
contacted, and no real credential is read.

| Command | Result (macOS AArch64) |
| --- | --- |
| `cargo test --workspace --locked` | 865 passed, 0 failed |
| `cargo test -p tea-core --all-features --locked` | 320 passed |
| `cargo test -p tea-providers --all-features --locked` | 164 passed, 1 ignored (incl. 42 Anthropic) |
| `cargo test -p tea-agent --features mcp-fixture --locked` | 214 passed, 1 ignored (incl. real-process MCP and durable discovery) |
| `cargo test -p tea-agent --features pty-harness --test pty_streaming --test pty_isolation --locked` | 12 + 6 passed |
| `cargo test -p tea-luau --locked` | 110 passed (codemode, plan/build router, combined active-work scenario) |
| `cargo check --workspace --all-targets --all-features --locked` | passed |
| `./crates/tea-core/fixtures/run.sh` | 29 passed |
| `python3 scripts/check-crate-graph.py`, `scripts/check-toolchain-pin.sh`, `scripts/check-verification-entrypoints.py`, `git diff --check` | passed |
| `make test-linux` (Linux AArch64 Docker, `make test` inside) | passed at `b53d12c`, including both PTY suites |

The Linux run first exposed a pre-existing timing race in
`cancellation_settles_the_owned_process_scope_promptly` (a fixed 50 ms delay
before cancelling a `sleep`); the test now waits for the command's first
output, and the rerun passed.

### Measurements (macOS AArch64, release, local comparison)

| Metric | Base `5baee99` | Upgrade |
| --- | --- | --- |
| Release `tea` size | 8,902,128 B | 9,448,800 B (+6.1%) |
| `tea --version` (median of 30) | ~9 ms | ~9 ms |
| First frame, headful mock (median of 30) | ~22–26 ms | ~31 ms split; ~21 ms `TEA_IN_PROCESS=1` |
| Idle RSS, headful (relay + runtime) | 8.9 MiB (one process) | 12.2–12.4 MiB (3.2 relay + ~9.1 runtime) |
| Idle RSS, `TEA_IN_PROCESS=1` | — | 8.9–9.2 MiB |

The headful increase is the relay process and the second exec; the runtime
itself is within 3% of the old single process. Measured with
`scripts/measure-idle-rss.py` (now summing the started process and its
descendants) and a PTY first-frame probe. These are local comparisons, not
limits.

### Not established offline

- Live acceptance of every Anthropic request field, real cache-hit rates, and
  the real savings of cache warming are **unmeasured**; mocks prove request
  bytes, timing, and accounting arithmetic only.
- Real MCP servers beyond the fake one, and real routed-model quality, are not
  exercised.
