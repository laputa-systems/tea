# Compaction metric glossary

Every metric names its evidence level. Unknown stays unknown; zero means a
provider or deterministic measurement actually reported zero.

| Metric | Meaning | Evidence level |
| --- | --- | --- |
| `compaction_id` | Stable run-plus-ordinal join key | lifecycle fact |
| `source_history_revision` | Canonical generation summarized | lifecycle fact |
| `canonical_message_bytes` | Approximate canonical source size | deterministic proxy |
| `tool_result_bytes` | Raw canonical tool-result contribution | deterministic proxy |
| `replacement_bytes` | Proposed canonical replacement size | deterministic proxy |
| `estimated_context_tokens_after` | Core byte-derived estimate after proposal | estimate, not tokenizer truth |
| `headroom_tokens` | Explicit budget minus estimate | policy estimate |
| `serialized_request_bytes` | Exact adapter request envelope size | adapter fact |
| `cache_domain_fingerprint` | Adapter-defined normalized cache-domain fingerprint | adapter diagnostic |
| `common_context_prefix_bytes` | Adjacent core-context common prefix | cacheability proxy |
| `cache_read_tokens` / `cache_write_tokens` | Provider-reported prompt-cache accounting | provider fact |
| `provider_input_tokens` / `provider_output_tokens` | Compactor provider usage | provider fact |
| `terminal_outcome` | Commit/rejection/failure/cancel/timeout/unavailable | lifecycle fact |
| `scenario_rows` | Named Rust-contract coverage rows in the offline report | report fact, not attempt count |
| `fixture_compaction_episodes` | Independently executed deterministic continuation episodes | deterministic fixture result |
| `critical_facts_total` / `critical_facts_survived` | Stable-ID fact retention in a fixture checkpoint | deterministic fixture result |
| `required_facts_missing` | Required stable fact IDs absent after merge | deterministic fixture result |
| `obsolete_facts_present` / `contradictions` | Forbidden or superseded facts that reappeared | deterministic fixture result |
| `checkpoint_generation` | Marker-versioned cumulative checkpoint generation | deterministic fixture result |
| `checkpoint_bytes` / `ledger_entries` | UTF-8 checkpoint size and host-derived ledger count | deterministic fixture result |
| `duplicate_tool_calls` | Equivalent tool signatures before a relevant state change | deterministic fixture result |
| `repeated_unchanged_read_bytes` | Byte count reread without a file-generation change | deterministic fixture result |
| `repeated_failed_approaches` | Failed command repeated without a workspace-generation change | deterministic fixture result |
| `tool_calls_until_productive_action` | Scripted calls before first novel read/edit/validated command | deterministic fixture result |
| `requests_until_next_compaction` | Controlled continuation distance to the following compaction | deterministic fixture result |
| `immediate_recompaction` | Next compaction occurred without genuine intervening pressure | deterministic fixture result |

`common_context_prefix_bytes`, exact extension, append-only context, and cache
domain stability help diagnose whether a request layout is cache-friendly.
They never prove a cache hit. Only provider-reported `cache_read_tokens` or
`cache_write_tokens` supports cache-accounting claims.

The offline quality report also marks model-free tool-result pruning as
unpromoted unless its baseline shows that old tool-result bytes dominate
pressure and its before/after trace preserves the needed continuation facts.

The fixture-only continuation metrics are never reclassified as provider facts.
The optional canary can report strategy ID, lifecycle/terminal counts,
adapter-request observation count, and whether the provider supplied usage or
cache fields. A provider failure before compaction is recorded as an excluded
availability/acceptance outcome, not as a zero-cost run.
