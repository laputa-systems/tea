# Compaction provider canary — 2026-08-22

This is a sanitized, manually authorized provider observation. It is not a
benchmark and it does not promote a strategy.

Command contract: the credential was injected only with
`vault OPENROUTER_API_KEY -- …`; no key, prompt body, provider body, or raw
checkpoint was written to the repository.

| Model / strategy | Result | Interpretation |
| --- | --- | --- |
| `poolside/laguna-xs-2.1:free` / `cache_replay_summary_v0` | pressured canary committed once with 12 lifecycle records and 2 terminals | baseline reaches the real OpenRouter adapter with its original tool envelope |
| `poolside/laguna-xs-2.1:free` / `tool_free_replay_summary_v1` | pressured canary committed once with 12 lifecycle records and 2 terminals | explicit compatibility candidate reaches the same provider path without silently changing the baseline |
| `poolside/laguna-xs-2.1:free` / later retry with continuation check | rejected before an automatic compaction started (`run_failed=true`, fact survival `false`) | free-provider availability or request acceptance is variable; this is not a strategy-quality comparison |
| `poolside/laguna-s-2.1:free` / historical pre-baseline correction | `committed=true`, 12 content-free lifecycle records, 2 terminal records | successful bounded canary, but it used the then-tool-free request under the old baseline ID and is not baseline evidence |

The successful probes used the repository-owned `tea-compaction-canary` binary
with four queued pressure turns, explicit `--pressure-bytes 5000`,
`--context-window 5000`, and an explicit `--strategy`. They verify the same
provider adapter, host context hook, `ProviderCompactor`, automatic-policy
transaction, lifecycle, and content-free reporting boundary used by the
terminal host. With `--continuation-check`, it also asks for a turn-one durable
fact after pressure/compaction and emits only a boolean survival result. The
canary now emits its strategy ID and records a content-free failure result when
a run ends before compaction or fails that requested continuation check.

The comparison decision remains unchanged:
`cache_replay_summary_v0` stays the default. No alternate candidate strategy
was promoted, because isolated free-model successes and subsequent rejection
cannot establish semantic continuation quality, cache savings, latency
improvement, or reduced-rework evidence. A promotion needs an explicit
candidate, controlled workload comparison, provider-reported cache accounting
where available, and a reviewed replacement of this decision record.
