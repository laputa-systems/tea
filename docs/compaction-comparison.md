# Initial compaction strategy comparison

This committed comparison separates proven mechanical work from unresolved
provider-generated-summary quality. It is based on the provider-free 70-row
contract matrix, five deterministic continuation episodes, and the sanitized
canary record dated 2026-08-22.

| Strategy | Version | Changed dimension | Hard gates | Checkpoint / ledger evidence | Cache evidence | Decision |
| --- | --- | --- | --- | --- | --- | --- |
| `cache_replay_summary_v0` | 0 | preserved model-facing baseline, including ordered tool definitions | pass: CAS, exact suffix, valid tool structure, nonempty checkpoint, strict reduction, headroom, cancellation/retry limits | provider-free transaction tests; one Laguna XS canary committed | adapter bytes/domain and prefix proxy only; no reported cache hit | default retained |
| `tool_free_replay_summary_v1` | 1 | tools omitted from otherwise exact replay | same core hard gates; tool-domain change is explicit | one Laguna XS canary committed; a later retry failed before compaction | unavailable; exact context prefix does not imply an envelope cache match | experimental compatibility candidate |
| `structured_checkpoint_v1` | 1 | human-readable sections, exact marker, and host-derived ledger | marker/ledger tests pass; runtime request construction is covered | five deterministic generations; live canary outcome not captured as comparison evidence | unavailable | experimental |
| `incremental_checkpoint_update_v1` | 1 | previous marker checkpoint plus newly discarded span and ledger delta | first-generation fallback and incremental request construction are covered | five deterministic generations; live canary outcome not captured as comparison evidence | unavailable | experimental |
| model-free pruning | n/a | derived tool-result surface only | not introduced | `tool_result_bytes` is observed | unavailable | rejected until a baseline proves dominance |

## Metric vector and evidence classification

- **Structural proof:** stable compaction ID, append-only lifecycle, source
  generation CAS, terminal outcome, retained suffix equality, tool-pair
  validation, strict byte reduction, typed timeout/cancellation/unavailable.
- **Deterministic fixture result:** 70 named Rust-contract coverage rows and
  five independently executed continuation episodes pass; marker/merge/ledger
  and runtime request-construction tests pass.
- **Adapter fact:** exact serialized request bytes and normalized cache-domain
  fingerprints are captured from the stream that sends the request.
- **Prefix proxy:** exact context extension and common-prefix measures only.
  They are not provider cache accounting.
- **Estimate:** canonical byte-derived context tokens and post-replacement
  headroom; the threshold uses the conservative maximum of canonical estimate
  and usable provider checkpoint.
- **Provider observation:** `poolside/laguna-xs-2.1:free` committed one
  pressured canary each for the preserved baseline and the explicit tool-free
  candidate. Later free-provider retries were rejected before compaction, so
  this is compatibility/reachability evidence rather than a stable comparison.
  No run reported cache-read/write usage or a semantic score.
- **Unresolved:** critical-fact survival from model-generated candidate
  summaries, stale-fact resurrection, task continuation success, duplicate
  work, p90 latency, next-compaction distance, and provider cache reuse.

No weighted score selects a strategy. The original model-facing baseline
remains the default because no materially different candidate has the required
provider-backed continuation evidence.
