# Active-work cache warming

Long generations and long tool waits can outlast a provider's prompt-cache
lifetime. When that happens the next real request pays a full cache write
instead of a cheap cache read. Tea's warmer replays the most recent admitted
request with a one-token output cap shortly before its cache entry expires.

The behavior is a port of upstream Pi's
`packages/coding-agent/src/core/cache-warmer.ts` (Pi revision
`a13d35a742c6`) in its default `streaming` mode. The code lives in
`tea_core::cache_warming`.

## Scope

- **Active work only.** Warming runs only while an agent run is working. It
  stops when the run settles, so it never warms between completed user turns
  and never keeps an idle session alive. Pi's `idle` mode is not ported.
- **Subordinate to its run.** The warmer is polled inside the run's own
  `RunHandle::drive` future. It is not spawned, cannot outlive the run, and is
  not a separate agent run, service, or scheduler.
- **The exact admitted request.** A maintenance request is the admitted
  `ModelRequest` with `purpose = CacheMaintenance` and `max_output_tokens =
  Some(1)`. It does not rerun routing, hooks, or context transforms, so it
  cannot target a different physical model. Returned text, thinking, and tool
  calls are discarded; tool calls never execute.
- **Explicit capabilities.** A model is warmed only when its provider declares
  `ModelCapabilities::prompt_cache` (lifetime and `MinimalOutputReplay`) and
  `ModelCapabilities::pricing`. Today only the native Anthropic adapter does.
  `MinimalOutputReplay::SafeWithoutThinking` covers Anthropic budget-based
  thinking, whose budget is derived from the output cap: a one-token replay
  with thinking on would change the cache key, so such requests are not warmed.

## Decisions

| Rule | Value (as in Pi) |
| --- | --- |
| Refresh delay | `min(90% of TTL, TTL − 10 s)`; no warming for TTL ≤ 10 s |
| Late timer | A refresh later than `next + (TTL − delay) / 2` is skipped and warming stops ("cache refresh deadline missed"). The host clock is wall time, so suspension counts. |
| Horizon | No refresh is scheduled more than one hour after the real request that started warming; refreshes never move that start. |
| Economics | `miss = price(cache write or input) − price(cache read)` for the last provider-reported prompt size, `warm = price(cache read) + one output token`; warm only when `miss − warm ≥ $0.05`. Unknown prompt size or prices stop warming ("cache economics unavailable"). |
| Currency | Warming stops if the run is cancelled, the model or reasoning level changes, the transcript no longer extends the request (for example after compaction), or a collapsed-projection provider sees a configuration change. |
| Supersession | Each new real request replaces the warmer and cancels a refresh in flight, so a ready continuation never waits on maintenance. |
| Retries | Maintenance requests are never retried (Pi's `maxRetries: 0`). |

Pi's extension override hook (`cache_warming_decision`) and the idle
continuation probability are deliberately omitted: the policy has no tuning
surface beyond on/off.

## Attribution

Maintenance is an attributed provider operation, not an assistant message:

- the run emits `AgentEventKind::CacheMaintenance { record }` before its
  terminal `AgentEnd`;
- durable sessions persist a `cache_maintenance` lane record (owning operation,
  physical and selected model, outcome, provider-reported usage, and a
  listed-price `estimated_cost`). It is never a session entry, never model
  context, not a provider request record, and not counted in model-turn or
  lane usage totals;
- the prompt-layout ledger and trace cache evidence ignore maintenance, so
  ordinary request-continuity measurements are unchanged;
- a refresh cancelled by supersession or settlement still records the usage it
  had already reported, so billing evidence does not disappear;
- `Usage.cost` stays whatever the provider reported (Anthropic reports none);
  `estimated_cost` is always labelled an estimate, and unknown stays unknown.

Economics are seeded from the lane's last provider-reported prompt size, so the
first long generation of a new operation can be priced.

## Enabling

Library hosts opt in explicitly:

```rust,ignore
use tea_core::cache_warming::CacheWarmingPolicy;

let services = RuntimeServices::new(provider, tools)
    .cache_warming(CacheWarmingPolicy::new(Arc::new(my_clock)));
```

The clock implements `MaintenanceClock` (`now` must advance across
suspension; `sleep_until` may wake early or late). The `tea` terminal enables
warming by default with a wall-clock timer. Disable it in
`$TEA_HOME/config.toml`:

```toml
[cache_warming]
enabled = false
```

The footer shows `warm ×N ~$X` for maintenance in the attached session.

## Evidence

Offline tests on a virtual clock (`tea_core::testing::VirtualClock`) and the
scripted provider:

```sh
cargo test -p tea-core --all-features cache_warming
cargo test -p tea-providers --all-features cache_maintenance
```

They cover exact replay, repeated refreshes, late deadlines, the one-hour
horizon, economics, unsafe replay, missing lifetimes, stale conversations,
supersession during a tool wait with partial billing evidence, durable
records across reopen, and no maintenance after settlement.

Not measured: real cache-hit rates and actual savings. Those require live
provider traffic and are outside offline verification.
