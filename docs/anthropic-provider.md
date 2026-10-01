# Native Anthropic provider

`tea-providers` feature `provider-anthropic` adds a direct Anthropic Messages
API adapter authenticated by an API key. It is a selective port of upstream Pi's
`anthropic-messages` implementation; the provenance record for the whole Pi 1.0
upgrade is in [pi-1-upgrade.md](pi-1-upgrade.md).

Upstream reference: Pi `a13d35a742c6ef8462812a28fbe1d8c8b7431c32`,
`packages/ai/src/api/anthropic-messages.ts`,
`packages/ai/src/providers/anthropic.ts`, and
`packages/ai/src/providers/anthropic.models.ts`.

## Enabling it

Library hosts construct an explicit configuration; nothing reads the process
environment:

```rust,no_run
use tea_providers::anthropic::{AnthropicConfig, CacheRetention};
use tea_providers::{ProviderConfiguration, ProviderRegistry};

let registry = ProviderRegistry::new();
let selection = registry.resolve_model("anthropic", "claude-sonnet-5-5")?;
let config = AnthropicConfig::try_new("caller-owned key", "claude-sonnet-5-5")?
    .with_cache_retention(CacheRetention::Long);
let configured = registry.build(
    selection.into_descriptor(),
    ProviderConfiguration::Anthropic(config),
)?;
# let _ = configured;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The terminal host reads `ANTHROPIC_API_KEY` only when an `anthropic/...` model
is selected and a request is about to start. Optional terminal settings live in
`$TEA_HOME/config.toml`:

```toml
[anthropic]
cache_retention = "short"      # "none", "short" (5 min), or "long" (1 h)
thinking_display = "summarized" # or "omitted"
```

The credential is never accepted from this file.

## Upstream → tea mapping

| Upstream Pi | tea |
| --- | --- |
| `buildParams` | `anthropic/payload.rs` `build_payload` |
| `convertMessages`, `transformMessages` | `Transcript::prepared_for` (provider-neutral replay rules) and `payload.rs` message conversion |
| `convertTools`, deferred tool `defer_loading` | `payload.rs`; deferred declarations use a stable placeholder list so in-place tool additions do not rewrite the prefix |
| `system` replay / `system-message-replay.test.ts` | `tea_core::transcript` (`ConfigurationProjection::{InPlace, Collapsed}`) and `transcript/tests.rs` |
| `mapThinkingLevelToEffort`, `DEFAULT_THINKING_BUDGETS`, `adjustMaxTokensForThinking` | `payload.rs` effort/budget mapping |
| `iterateSseMessages`, `decodeSseLine`, `repairJson` | `anthropic/sse.rs` |
| `stream()` / `iterateAnthropicEvents` / `mapStopReason` | `anthropic/events.rs` |
| `isRetryable`, `retry-after-ms`/`retry-after`, `x-should-retry` | `anthropic/mod.rs` transport loop |
| model catalog generator | `anthropic/catalog.rs` (`AnthropicCompat`, listed pricing, prompt-cache TTL) |
| `calculateCost` incl. 1 h cache-write multiplier | `anthropic::estimate_cost` |
| context-overflow patterns | `anthropic/mod.rs` overflow classifier |

Ported tests (all run offline against request fixtures and a loopback chunked
HTTP fixture server, including one-byte fragmentation):

| Upstream test | tea test |
| --- | --- |
| `anthropic-sse-parsing.test.ts` | `sse.rs` tests; `stream_tests.rs::a_fragmented_text_response_streams_through_the_real_transport`, `malformed_tool_json_is_repaired`, `usage_less_deltas_and_trailing_unknown_events_are_tolerated` |
| `system-message-replay.test.ts` | `tea-core/src/transcript/tests.rs`; `payload_tests.rs::native_updates_send_tool_changes_in_place_with_a_stable_deferred_tool_list`, `redefinitions_and_missing_initial_tools_fall_back_to_the_current_tool_list`, `without_native_support_updates_fold_into_the_leading_prompt`, `native_tool_changes_require_both_capabilities` |
| `anthropic-mid-conversation-effort.test.ts` | `managed_effort_*` payload tests |
| `anthropic-adaptive-thinking-models.test.ts`, `anthropic-force-adaptive-thinking.test.ts` | `adaptive_models_without_managed_effort_use_top_level_effort`, `budget_thinking_adds_a_budget_beneath_the_output_ceiling` |
| `anthropic-thinking-disable.test.ts` | `thinking_off_disables_only_where_the_model_allows_it` |
| `anthropic-temperature-compat.test.ts` | `temperature_is_sent_only_without_thinking_on_supporting_models` |
| `anthropic-empty-thinking-signature-compat.test.ts` | `unsigned_thinking_is_dropped_unless_empty_signatures_are_accepted` |
| `anthropic-eager-tool-input-compat.test.ts` | `tool_input_streaming_is_eager_unless_the_model_needs_the_fine_grained_beta` |
| `anthropic-cache-write-1h-cost.test.ts` | `one_hour_cache_writes_are_priced_at_twice_input`, `usage_maps_to_full_prompt_input_with_cache_subsets_and_estimated_cost` |
| `transform-messages-*` (thinking replay subset) | `same_model_signed_and_redacted_thinking_replay_and_cross_model_thinking_does_not`, `tool_ids_are_normalized_consistently_and_results_are_grouped` |

Run them with:

```sh
cargo test -p tea-providers --features provider-anthropic anthropic
```

## Exclusions

Deliberately not ported:

- images and any multimodal content;
- OAuth, Claude subscription login, and the `anthropic-auth-token` path;
- federation, the Bedrock/Vertex/Copilot Anthropic variants, and Claude Code
  identity impersonation including tool-name normalization
  (`anthropic-tool-name-normalization.test.ts`, `anthropic-oauth.test.ts`,
  `anthropic-federation*.test.ts`);
- strict tool schemas (`anthropic-strict-tool-schema.test.ts`), `tool_choice`,
  server-side model fallback headers, session-affinity headers, and the
  `onPayload` mutation hook;
- live `*-e2e`/`*-smoke` tests, which require real inference.

## Deviations

- Thinking without a signature is dropped from replay rather than converted to
  plain text; plain-text conversion would put model-private reasoning into the
  visible transcript. Models whose compat flag accepts empty signatures still
  replay it.
- For managed-effort models `ThinkingLevel::Off` maps to the lowest effort
  (`"low"`) because those models do not accept a disabled-thinking request.
- `Usage.cost` stays `None`: Anthropic reports token counts, not a charge. The
  catalog's listed pricing is exposed through `ModelCapabilities::pricing` and
  `estimate_cost`, and every consumer labels it an estimate.
- `Usage.input_tokens` is the full prompt (`input + cache_read + cache_write`)
  so it is comparable with other adapters; the cache fields are subsets.
- Rust strings are always valid UTF-8, so Pi's surrogate sanitization has no
  counterpart.
- Retries happen only before any visible output; a server retry delay longer
  than 60 s fails immediately with the typed error instead of waiting.

## Limits of the offline evidence

Fixtures prove request bytes, header selection, SSE decoding, retry
classification, and usage/cost arithmetic. They do not prove that the live API
still accepts every field, that prompt-cache hits occur, or what a real session
costs; those need a funded key and are intentionally not part of verification.
