# Tea Compaction Observability, Efficiency, and Regression Engineering

Work autonomously through research, design, implementation, tests, documentation, and final evaluation. Do not stop after producing a plan or scaffolding. Adapt names and file placement to the repository’s actual conventions rather than forcing the names suggested below.

The objective is to make tea’s context compaction a rigorously observable, versioned, budgeted state transition, then use that infrastructure to improve compaction layout and algorithm quality without compromising tea’s lean, provider-agnostic architecture.

The resulting system must make it possible to answer, with durable artifacts rather than anecdotes:

* Why did a compaction occur?
* Where in the agent loop did it occur?
* What exact context range did it replace?
* What exact recent suffix was retained?
* Which compaction strategy and schema version ran?
* What request did the compactor actually receive?
* Did the compaction request preserve the previous request’s model-visible prefix?
* Which request-envelope components changed?
* How much context was consumed before and after compaction?
* How much usable headroom did compaction create?
* Which durable facts survived, disappeared, or became contradictory?
* How much work did the agent repeat afterward?
* How many model requests, tool calls, bytes, and tokens elapsed before the next compaction?
* Did repeated compactions progressively destroy long-lived state?
* Did the complete coding task become cheaper, more expensive, or less reliable?
* Did a change improve real provider cache reuse, or merely tea’s deterministic prefix proxy?

The guiding definition is:

> Compaction quality is the ability to resume productive work after reducing context, with minimal semantic loss, minimal uncached inference, minimal rereading, minimal repeated failure, and sufficient headroom to avoid another premature compaction.

A small summary is not inherently good. A summary that is compact but causes the agent to reread the repository, retry rejected approaches, lose exact paths, or compact again immediately is a regression.

## Execution model

The required implementation must be fully buildable and testable without:

* provider credentials,
* network access,
* a running inference server,
* recorded secrets,
* or any real model call.

There is no local-inference lane in this project.

Do not add:

* oMLX integration,
* llama.cpp integration,
* Ollama integration,
* a generic OpenAI-compatible local-server harness,
* local-model setup documentation,
* local-model CI,
* or local-model-specific configuration.

The mandatory default CI lane is entirely provider-free.

An optional hosted-provider canary may use tea’s existing provider adapters when credentials are explicitly supplied. It must not be required for normal builds, tests, or CI. Do not add a new provider solely for this work.

When no hosted credentials are available:

* complete all observability work;
* complete deterministic trace and regression infrastructure;
* establish the current baseline;
* implement candidate strategies behind explicit non-default strategy IDs;
* test request construction, state transitions, budgets, structural invariants, and deterministic continuation behavior;
* produce a complete offline comparison report;
* do not claim that model-generated semantic summary quality was measured;
* do not claim a prompt-cache hit from prefix similarity;
* do not promote a materially different model-facing compaction strategy solely from hand-authored or deterministic summaries.

Mechanical fixes that are provably correct without inference may become the default. Examples include fixing stale accounting, preserving exact suffix boundaries, preventing concurrent-history loss, enforcing retry caps, improving lifecycle events, or rejecting a non-shrinking replacement.

A materially different summary prompt, summary layout, retained-history policy, or model-driven compaction algorithm requires real-provider evidence before becoming the default.

## Architectural constraints

Preserve tea’s design character.

* `tea-core` remains provider-neutral.
* The core must not infer provider limits from model-name heuristics.
* Preserve transactional compaction semantics.
* Preserve cancellation semantics.
* Preserve exact tool-call/tool-result validation.
* Preserve the exact retained suffix selected by the core.
* Preserve bounded overflow recovery.
* Preserve the existing provider projection and context-hook pipeline.
* Preserve existing public behavior unless a change is explicitly justified and covered by regression tests.
* Do not add MCP support.
* Do not add a general plugin framework.
* Do not add a dynamic plugin registry, service locator, capability graph, dependency-injection container, or extension marketplace.
* Do not restructure tea around DeepSeek Harness’s plugin architecture.
* Do not move provider-specific request fields into `tea-core`.
* Do not hard-code provider prices into core code or durable trace schemas.
* Do not add an LLM judge to the default test suite.
* Do not add a database, telemetry service, daemon, or network dependency.
* Avoid new dependencies.
* Follow the repository’s current async/runtime conventions.
* Follow the pinned Rust toolchain and existing CI policy.
* Keep the TUI thin. Do not turn it into a telemetry dashboard.
* Do not execute tools during a compaction-summary request.
* Do not collect metrics by constructing or projecting an effectful request twice.

Any new dependency must remove materially more complexity or risk than it adds and must be justified in the final report.

## Optimization order

Use a lexicographic objective rather than one blended score.

In priority order:

1. Structural correctness.
2. End-to-end task success.
3. Critical durable-state retention.
4. Transaction, cancellation, and concurrency safety.
5. Compaction convergence and sufficient post-compaction headroom.
6. Prompt-layout and cache-domain stability.
7. Reduced uncached model work.
8. Reduced repeated tool work and rereading.
9. Reduced latency and compaction frequency.
10. Smaller summaries.

A candidate that is cheaper but loses critical state fails.

A candidate that preserves state but leaves insufficient headroom fails.

A candidate that produces a tiny checkpoint but causes immediate recompaction fails.

A weighted efficiency score may be useful for ranking successful candidates, but it must never override the preceding gates.

## Research phase: verify current upstream behavior

Before changing tea, inspect the latest available upstream source rather than relying on remembered behavior.

Study at least:

### OpenAI Codex

Inspect:

* `openai/codex`
* `codex-rs/core/src/compact.rs`
* compaction prompt templates
* history replacement and persistence code
* request construction surrounding local and remote compaction
* token accounting after compaction
* retained-recent-history policy
* pre-turn versus mid-turn replacement layout
* retry and overflow handling
* client/session reuse during compaction
* cache-key and request-envelope propagation

Search current code, issues, discussions, and merged or open pull requests for:

* `compact`
* `compaction`
* `prompt cache`
* `cache key`
* `tool specs`
* `reasoning effort`
* `service tier`
* `max output tokens`
* `remote compact`
* `stale usage`
* `immediate recompaction`
* `concurrent history`
* `summary loss`
* `cumulative compaction`
* `image`
* `tool result`
* `headroom`

### Pi

Inspect:

* `badlogic/pi-mono`
* the latest active development branch
* `packages/coding-agent/docs/compaction.md`
* `packages/coding-agent/src/core/compaction/`
* session-manager compaction entries
* branch summarization
* file-operation extraction
* previous-summary propagation
* split-turn handling
* `firstKeptEntry`
* automatic-compaction threshold accounting
* cache-miss diagnostics
* recent compaction issues and fixes

### DeepSeek Harness

Inspect:

* `deepseek-ai/deepseek-harness`
* compaction subsystem documentation
* the basic summarizing implementation
* model-free tool-result pruning
* durable compaction lifecycle events
* surface replacement and shadowed-event representation
* locking, cancellation, timeout, and failure behavior
* request-header inheritance
* prompt-prefix preservation
* recent bugs involving request-envelope differences and hung compactions

### OpenAI prompt-caching guidance

Verify the current guidance concerning:

* append-only model-visible history,
* exact repeated prefixes,
* deterministic tool ordering,
* stable tool definitions,
* and keeping runtime policy out of tool schemas.

### Research output

Create a concise repository document such as:

`docs/compaction-research.md`

For each transferable innovation, record:

* upstream repository and commit SHA inspected;
* exact source path or issue;
* observed behavior;
* failure mode addressed;
* whether tea already implements it;
* whether it fits tea’s architecture;
* whether it is adopted, rejected, or deferred;
* the corresponding tea test or measurement.

Do not copy another harness wholesale. Extract narrow mechanisms that fit tea.

The initial research should explicitly evaluate these hypotheses:

1. Exact message-prefix preservation is insufficient when other cache-relevant request-envelope fields differ.
2. Tool definitions and their order may be part of the effective prompt-cache domain.
3. Changing reasoning effort, output-token limits, service tier, or similar request parameters may split the provider cache domain.
4. Compaction needs an explicit lifecycle identity rather than being inferred from adjacent model requests.
5. Pre-turn and mid-turn compaction may require different replacement layouts.
6. Cumulative structured checkpoints may retain operational state better than free-form recent-work summaries.
7. Harness-derived file and command ledgers can preserve exact state more reliably than asking the model to remember it.
8. Stale token accounting can cause immediate post-compaction retriggering.
9. Clone-then-replace compaction can lose concurrent history unless commit validates the source generation.
10. Large historical tool results may justify a separate model-free pruning stage.
11. A compaction call needs bounded cancellation/deadline behavior because it is issued near maximum context pressure.
12. A compaction that does not create meaningful working headroom is not successful.

## Phase 0: audit tea and freeze the baseline behavior

Read `AGENTS.md` and all repository-local instructions first.

Inspect at least the following paths if they still exist:

* `crates/tea-core/src/compaction.rs`
* `crates/tea-core/src/run/mod.rs`
* `crates/tea-core/src/event.rs`
* `crates/tea-core/src/measurement.rs`
* `crates/tea-core/src/scheduler.rs`
* the `ModelRequest` definition
* provider adapters
* usage normalization
* context hooks
* provider-context conversion
* `crates/tea-agent/src/app/compaction.rs`
* built-in read, write, edit, and shell tools
* `crates/tea-trace`
* `evals/quality`
* existing compaction tests
* automatic-compaction policy tests
* provider-overflow tests
* cache-friendliness tests
* trace compatibility tests
* CI workflows
* `Makefile`
* `rust-toolchain.toml`

Trace these complete paths:

1. Ordinary user turn followed by an ordinary model request.
2. One model turn with several tool-call/result iterations.
3. Automatic threshold compaction before a request.
4. Provider-overflow compaction followed by retry.
5. Manual compaction.
6. The first ordinary request after successful compaction.
7. A second compaction when the history already contains a previous checkpoint.
8. Exact provider-context replay for the summary request.
9. Standalone summary fallback when exact replay would not fit.
10. Cancellation while compaction is in flight.
11. State mutation or user steering while compaction is in flight.
12. Compaction failure after a start event.
13. Application restart or trace replay after a committed compaction.

Produce a short design note that records:

* canonical versus provider-visible history;
* ownership of the compaction snapshot;
* source and retained-tail selection;
* validation before commit;
* current retry bounds;
* current cancellation behavior;
* current context estimation;
* current provider-usage checkpoints;
* current summary-request construction;
* current summary prompt;
* current trace representation;
* current cacheability measurement;
* current blind spots;
* where measurements can be captured without running hooks twice.

Before changing model-facing behavior, add or preserve a named baseline strategy such as:

`cache_replay_summary_v0`

The name is illustrative. Use a repository-appropriate stable identifier.

Capture the current summary prompt and request layout as a versioned baseline. Do not silently mutate an existing strategy ID.

## Phase 1: introduce a first-class compaction lifecycle

### Compaction identity

Give every attempted compaction a stable ID.

Represent at least:

* operation ID;
* run ID;
* triggering request or turn ID;
* source-history generation or revision;
* trigger:

  * manual,
  * automatic;
* reason:

  * user request,
  * threshold,
  * provider overflow;
* phase:

  * before a model request,
  * between model calls in the same agentic turn,
  * standalone/manual;
* implementation:

  * deterministic test compactor,
  * provider summarization,
  * provider-native compaction if tea already supports one;
* strategy ID;
* strategy schema version;
* attempt number;
* automatic-compaction ordinal within the run;
* overflow-retry ordinal;
* whether a successful commit will retry an interrupted request;
* start time or monotonic duration data where appropriate;
* terminal outcome:

  * committed,
  * rejected,
  * failed,
  * cancelled,
  * timed out,
  * unavailable.

Do not invent runtime variants for behavior tea does not support. Trace schemas may use forward-compatible strings where appropriate, while runtime enums should describe real behavior.

The following must be joinable by compaction ID:

* start;
* selected source range;
* prepared compactor request;
* provider usage;
* proposed replacement;
* validation result;
* commit;
* terminal result;
* first post-compaction model request;
* next compaction;
* terminal run outcome.

### Transaction and concurrency correctness

Audit whether an owned snapshot can be committed after canonical history has changed.

A compaction commit must verify that it still applies to the intended history generation and source range.

Required behavior:

* no concurrent history entry may be silently dropped;
* cancellation before commit must leave canonical history unchanged;
* an obsolete proposal must be rejected rather than overwriting newer state;
* a failed proposal must leave canonical state unchanged;
* all rejection reasons must be typed and observable;
* retry behavior must remain bounded;
* no compaction loop may continue indefinitely.

Use the narrowest mechanism that fits the existing design:

* revision counter;
* source generation;
* compare-and-swap-style validation;
* exact source-ID validation;
* or an existing transaction guard.

Do not introduce a general locking framework.

### Deadline and cancellation audit

Determine whether a compaction request can hang indefinitely despite normal run cancellation.

If the existing provider call already inherits a bounded deadline, prove it with tests and record it in the design note.

If it does not, add a host-owned bounded compaction deadline that:

* integrates with existing cancellation;
* produces a typed terminal outcome;
* does not mutate canonical state on timeout;
* emits a terminal trace event;
* is configurable without provider-specific core fields;
* has deterministic tests using a fake clock or controlled future.

Do not add a global timeout subsystem solely for compaction.

## Phase 2: measure the exact request and layout

### One request preparation path

Metrics must describe the exact request that is actually sent.

Do not:

* rerun a context hook for measurement;
* rerun provider projection;
* rebuild tool definitions separately;
* serialize a second approximation after sending;
* or invoke any effectful preparation step twice.

If necessary, refactor request construction into:

1. a pure or single-execution preparation stage that produces a prepared request and its measurement inputs;
2. one effectful provider send.

The provider receives the same prepared object that the measurement describes.

Add tests proving that measurement collection does not invoke hooks or projection twice.

### Request-layout observation

Generalize the current cacheability measurement so it can compare:

* ordinary request → next ordinary request;
* last ordinary request → compaction request;
* compaction request → first post-compaction ordinary request;
* last pre-compaction ordinary request → first post-compaction ordinary request;
* successive compaction requests;
* equivalent requests with only one envelope field changed.

Measure at least:

* model identity;
* system-prompt length and fingerprint;
* ordered tool-definition length and fingerprint;
* tool count;
* individual tool-order fingerprints where useful;
* thinking/reasoning configuration;
* output-token limit;
* service tier or routing class when exposed through a provider-neutral observation;
* provider-visible context length and fingerprint;
* approximate complete request bytes;
* adapter-serialized request bytes when safely available;
* exact common provider-context prefix bytes;
* common-prefix ratio;
* whether the candidate request is an exact extension of the previous context;
* whether the compaction instruction is a pure append;
* whether system instructions changed;
* whether tool definitions changed;
* whether tool order changed;
* whether the model changed;
* whether thinking configuration changed;
* whether any adapter-reported cache-relevant request-envelope component changed;
* normalized cache-domain fingerprint;
* component-level cache-domain differences.

Do not expose only one opaque `cache_domain_changed` boolean. A report must identify the changing component.

### Provider-neutral adapter observation

`ModelRequest` may not contain every transport field relevant to a provider cache key.

Add a narrow optional observation seam at the adapter boundary. It may expose normalized, content-safe information such as:

* deterministic cache-domain fingerprint;
* serialized request byte count;
* cache-relevant option-name fingerprints;
* provider-reported input tokens;
* provider-reported output tokens;
* provider-reported cache-read tokens;
* provider-reported cache-write tokens;
* provider-reported reasoning tokens;
* provider request ID;
* whether cache accounting was unavailable.

Do not expose provider-specific structs through `tea-core`.

Do not place fields named after a specific provider into the core schema.

Prefer a small normalized struct or opaque component map over a plugin API.

### Cache terminology

Use these distinctions everywhere:

* `provider_reported`: the provider explicitly reported cache usage;
* `prefix_proxy`: tea measured exact request-prefix compatibility;
* `unavailable`: neither actual cache accounting nor a meaningful proxy exists.

Never call a matching prefix a cache hit.

Never estimate cache-read tokens from prefix bytes.

Never merge actual cache tokens and prefix-proxy measurements into the same field.

### Compaction source observation

For each attempt, record content-free metadata for:

* canonical message count;
* canonical message bytes;
* provider-visible context bytes;
* estimated input tokens;
* latest provider-reported input tokens, if any;
* latest provider-reported cache tokens, if any;
* tool-result bytes;
* opaque or image payload counts and estimated sizes where applicable;
* exact source message IDs;
* source range boundaries;
* exact retained message IDs;
* exact split-turn-prefix message IDs;
* prior checkpoint ID and generation;
* current compaction depth;
* whether the source contains a prior tea checkpoint;
* whether the source is an exact prefix of the active provider context.

### Compactor-request observation

Record:

* strategy ID and version;
* exact-replay versus standalone mode;
* model identity;
* request-envelope fingerprint;
* relation to the previous ordinary request;
* exact common prefix;
* source estimated tokens;
* request estimated tokens;
* provider-reported usage when available;
* output byte count;
* retry count;
* deadline/cancellation outcome;
* whether tools were defined;
* whether tool execution remained prohibited;
* whether the summary instruction was appended exactly once.

### Proposed-replacement observation

Record:

* replacement message count;
* replacement bytes;
* checkpoint bytes;
* deterministic ledger bytes;
* exact retained-suffix bytes;
* estimated next-request tokens;
* configured context capacity;
* target utilization;
* minimum headroom requirement;
* resulting headroom;
* reduction bytes;
* reduction ratio;
* whether source size strictly decreased;
* whether structural validation passed;
* whether all source and suffix IDs matched;
* typed rejection reason if not committed.

### Continuation observation

After a compaction commits, fill continuation metrics incrementally:

* first post-compaction request ID;
* requests until first productive tool action;
* requests until next compaction;
* tokens or bytes appended until next compaction;
* tool calls until next compaction;
* productive tool calls until next compaction;
* immediate-recompaction flag;
* next compaction ID;
* repeated reads;
* repeated searches;
* repeated commands;
* repeated failed approaches;
* edit/revert churn;
* terminal task outcome.

Do not retain an unbounded in-memory telemetry graph. Keep the implementation compact and stream observations into the existing trace/eval machinery.

## Phase 3: durable trace representation

Extend `tea-trace` and quality traces so compaction is explicit rather than inferred.

Use an append-only lifecycle representation such as:

* compaction started;
* source selected;
* compaction request prepared;
* compaction provider usage observed;
* replacement proposed;
* replacement rejected or committed;
* compaction failed, cancelled, or timed out;
* post-compaction continuation checkpoint;
* compaction lineage closed by next compaction or run termination.

A started compaction with no terminal event must be recognizable as interrupted or crashed.

The durable trace must retain the original history records. Compaction may define a derived model-visible replacement or shadowed range, but must not erase the underlying audit trail.

Record exact source and retained message IDs, not merely array indexes that become ambiguous after replay.

### Trace privacy

Default traces must not contain:

* raw source code;
* raw prompts;
* full tool output;
* model-generated summaries;
* secrets;
* environment values;
* shell output;
* file contents.

Default traces may contain:

* fingerprints;
* byte counts;
* token counts;
* message IDs;
* tool names;
* normalized operation signatures;
* paths when allowed by existing trace policy;
* exit status;
* short pre-existing redacted diagnostics;
* strategy and schema IDs.

If the existing architecture cleanly supports explicit debug payload capture, make it opt-in and clearly marked unsafe or sensitive. Do not add raw payload capture merely because it is convenient for development.

Maintain backward compatibility with existing trace formats.

Add fixtures proving old traces remain readable.

## Phase 4: human-readable compaction diagnostics

Add one concise report surface, preferably in the trace/eval tooling rather than the primary TUI.

For one compaction, show:

* ID;
* trigger, reason, and phase;
* strategy and version;
* source and retained boundaries;
* prior checkpoint generation;
* pre/post size;
* created headroom;
* exact-prefix measurements;
* changed cache-domain components;
* provider cache accounting if reported;
* structural validation;
* outcome;
* requests until productive continuation;
* distance to next compaction;
* repeated-work metrics;
* warnings.

For a run, show:

* total compactions;
* compactions per request;
* compaction generations;
* immediate recompactions;
* average and worst headroom;
* cumulative summary/checkpoint growth;
* cumulative repeated work;
* cache-proxy stability;
* actual provider cache data where available;
* failures, cancellations, and timeouts.

Do not expose one blended “compaction score” as the primary interface.

## Phase 5: provider-free deterministic compaction suite

Add a dedicated compaction lane under the existing `evals/quality` organization.

Do not build a second eval framework.

The normal command should resemble:

`python3 -m evals.quality compaction --out /tmp/tea-compaction`

Adjust this to existing conventions.

The lane must run:

* offline;
* deterministically;
* without credentials;
* without a model;
* without network access;
* quickly enough for normal CI.

### Deterministic test components

Implement narrowly scoped test doubles using existing fixture infrastructure where possible:

* deterministic compactor;
* scripted provider;
* scripted provider-overflow responses;
* scripted usage reports;
* controlled hanging provider future;
* controlled cancellation;
* deterministic clock if required;
* deterministic message-ID generation where required;
* trace replay;
* deterministic tool result corpus.

The deterministic compactor returns known checkpoints so the suite can test the complete transaction and continuation machinery independently of model quality.

This proves the harness, not the production summary prompt. State that explicitly in reports.

### Fixture schema

Each compaction episode should define:

* fixture schema version;
* initial canonical conversation;
* provider-visible projection where needed;
* scripted provider events;
* policy and context capacity;
* strategy ID/version;
* expected source boundary;
* expected retained boundary;
* expected split-turn prefix;
* expected structural invariants;
* durable facts that must survive;
* obsolete facts that must not survive;
* expected file/command ledger;
* expected continuation actions;
* resource budgets;
* expected lifecycle events;
* expected terminal outcome;
* provenance for any recorded material.

### Durable fact representation

Represent facts with stable IDs and classes:

* active goal;
* hard constraint;
* user preference;
* accepted decision;
* rejected alternative;
* superseded decision;
* completed work;
* work in progress;
* blocker;
* failed attempt;
* failure reason;
* exact file path;
* exact symbol;
* exact error;
* command;
* test;
* result status;
* next concrete action.

Facts may be:

* critical;
* required;
* optional;
* obsolete;
* forbidden after compaction.

Missing or contradicting a critical fact is always a hard failure.

### Offline semantic checks

For deterministic or recorded checkpoints, use exact and normalized checks:

* required fact IDs;
* required exact identifiers;
* required paths;
* required symbols;
* required command and test statuses;
* forbidden obsolete facts;
* latest-wins constraints;
* deterministic ledger equality;
* duplicate normalized lines;
* required section order;
* checkpoint byte budget;
* contradiction detection where expressible deterministically.

Do not use an LLM judge.

Do not claim these checks validate the production model’s ability to generate the checkpoint.

### Required deterministic scenarios

Cover at least:

1. Ordinary append-only requests with no compaction.
2. Basic threshold-triggered compaction.
3. Manual compaction.
4. Provider overflow followed by compact-and-retry.
5. Overflow recovery disabled.
6. Maximum compactions per run reached.
7. Maximum overflow retries reached.
8. One enormous tool result.
9. Several large tool results.
10. A retained boundary adjacent to a tool-call/result pair.
11. Multiple tool calls in one assistant message.
12. Parallel tool calls if tea supports them.
13. A split turn where the retained suffix begins inside an agentic turn.
14. Empty compacted prefix with an intact retained suffix.
15. Compactor error.
16. Compactor cancellation.
17. Compactor timeout.
18. Empty checkpoint.
19. Whitespace-only checkpoint.
20. Compactor attempts to return a tool call.
21. Orphaned tool result in a proposed replacement.
22. Unresolved retained tool call.
23. Proposed replacement that does not shrink the source.
24. Proposed replacement that shrinks bytes but fails minimum headroom.
25. Summary request itself would exceed context.
26. Exact-replay summary path.
27. Standalone fallback path.
28. Two consecutive compactions.
29. Three or more cumulative compactions.
30. Previous checkpoint retained and updated.
31. Previous checkpoint accidentally duplicated.
32. A new constraint superseding an old constraint.
33. Accepted decision and rejected alternative.
34. Completed work that must not be resurrected.
35. Failed approach that must retain its failure reason.
36. Files read and modified across multiple compactions.
37. Tests that passed, failed, and were never run.
38. Model identity change between requests.
39. Thinking-level change between requests.
40. Output-token-limit change between requests.
41. Adapter routing or service-tier change.
42. Deterministic tool reordering.
43. Tool-definition content change.
44. Runtime approval-policy change that must not alter tool definitions.
45. Estimator drift versus provider-reported usage.
46. Large tool result appended immediately before the next request.
47. Immediate post-compaction threshold check with stale usage.
48. First post-compaction prompt must not retrigger without genuine pressure.
49. Concurrent canonical-history mutation while compaction is in flight.
50. Cancellation after proposal but before commit.
51. State-generation mismatch on commit.
52. Trace with a started but unterminated compaction.
53. Trace replay after committed compaction.
54. Backward decoding of old trace files.
55. Image or opaque-content cases supported by tea’s message model.
56. Missing or malformed content variants supported by provider adapters.
57. Provider reports cache-read usage.
58. Provider does not report cache usage.
59. Prefix proxy matches while provider cache usage is unavailable.
60. Adapter reports a cache-domain change outside core-visible fields.
61. Repeated unchanged file read after compaction.
62. Legitimate reread after the file changed.
63. Repeated equivalent search.
64. Repeated failed command without a changed precondition.
65. Retried command after a relevant edit.
66. Edit followed by revert.
67. First productive action after compaction.
68. Run terminates successfully before another compaction.
69. Run fails after compaction.
70. Compaction deadline fires and leaves canonical state unchanged.

Use generated cases for tool-call/result boundary safety if this can be done with existing dependencies. Do not add a heavy property-testing framework solely for this work.

### Metamorphic tests

Add provider-free metamorphic tests where useful:

* appending irrelevant old noise should not alter the exact retained suffix;
* changing only a superseded fact should not resurrect it;
* reordering independent old reads should not alter the deterministic ledger;
* changing a file content hash should convert a reread from redundant to legitimate;
* changing one request-envelope field should identify exactly that cache-domain component;
* increasing source history while retaining the same tail should not decrease required headroom unexpectedly;
* repeated compaction of an already normalized ledger should be idempotent;
* serializing and replaying a trace should preserve compaction identities and boundaries.

## Phase 6: operational-rework measurement

Measure work repeated after each committed compaction.

Normalize tool operations using repository-appropriate signatures.

### File reads

Track:

* path;
* range or query;
* content fingerprint when available;
* repository or file generation;
* bytes returned.

Classify:

* first read;
* repeated unchanged read;
* legitimate refresh after modification;
* overlapping read;
* exact duplicate;
* broad reread after previously targeted access.

Report:

* unique read bytes;
* repeated unchanged-read bytes;
* reread amplification;
* unique files read;
* repeated files;
* redundant range overlap.

### Searches

Normalize:

* tool;
* query;
* path scope;
* flags;
* repository generation.

Classify equivalent searches conservatively.

### Commands and tests

Normalize:

* executable;
* arguments;
* working directory;
* relevant environment fingerprint;
* repository generation;
* prior exit status.

Track:

* duplicate command;
* repeated failure without changed precondition;
* legitimate retry after edit or dependency change;
* test rerun after relevant modification;
* repeated repository inspection.

### Edits

Track:

* file;
* before/after fingerprint;
* edit generation;
* later revert;
* repeated edit to the same region where representable.

### Productive continuation

Define productive actions explicitly.

Examples:

* first novel file read that advances beyond the retained ledger;
* first edit;
* first new test;
* first successful command after a changed precondition;
* terminal answer supported by task validation.

Do not mark a duplicate repository scan as productive.

Report:

* requests to first productive action;
* tool calls to first productive action;
* duplicate operations before first productive action;
* repeated failed approaches;
* edit/revert churn;
* distance to next compaction.

Keep this logic in `tea-agent`, trace analysis, or eval code—not `tea-core`.

## Phase 7: metric glossary and baseline manifest

Create a metric glossary with exact names, units, and interpretation.

At minimum define:

### Size and budget

* canonical message count;
* canonical bytes;
* provider-context bytes;
* estimated input tokens;
* provider-reported input tokens;
* checkpoint bytes;
* retained-suffix bytes;
* deterministic-ledger bytes;
* reduction bytes;
* reduction ratio;
* context capacity;
* target utilization;
* headroom tokens;
* headroom ratio.

### Request layout

* common-prefix bytes;
* common-prefix ratio;
* system fingerprint match;
* tool fingerprint match;
* tool-order match;
* model match;
* thinking configuration match;
* output-limit match;
* adapter cache-domain match;
* exact-extension flag;
* append-only instruction flag.

### Actual provider usage

* cache-read tokens;
* cache-write tokens;
* input tokens;
* output tokens;
* reasoning tokens;
* cache status:

  * provider reported,
  * proxy only,
  * unavailable.

### Continuation

* requests until productive action;
* tool calls until productive action;
* requests until next compaction;
* tokens until next compaction;
* tool calls until next compaction;
* immediate recompaction;
* compaction generation;
* cumulative checkpoint growth.

### Rework

* repeated unchanged-read bytes;
* reread amplification;
* duplicate tool signatures;
* duplicate failed approaches;
* edit/revert count;
* repeated repository-inspection count.

### Semantic fixture quality

* critical facts retained;
* required facts retained;
* obsolete facts present;
* contradictions;
* exact identifiers retained;
* ledger discrepancies.

### Reliability

* compaction success;
* rejection;
* failure;
* cancellation;
* timeout;
* stale-generation rejection;
* retry count;
* run terminal outcome.

Do not ambiguously label byte estimates as tokens.

Do not ambiguously label local estimates as provider-reported usage.

### Baseline manifest

Check in a deterministic baseline for the current strategy before changing its model-facing behavior.

Record:

* fixture schema version;
* tea commit;
* strategy ID;
* strategy version;
* summary-prompt fingerprint;
* compaction policy;
* estimator version;
* trace schema version;
* fixture corpus version;
* metric values;
* expected exceptions;
* baseline-update reason.

Normal tests must never silently update baselines.

Provide an explicit update command resembling:

`python3 -m evals.quality compaction --update-baseline --reason "..."`

A non-empty reason is required and stored in the baseline artifact.

Generated baseline files must be deterministic.

## Phase 8: hard invariants and regression budgets

### Hard structural invariants

These have zero tolerance:

* Every tool result has a valid matching tool call.
* Every retained unresolved tool call remains resolvable.
* The retained suffix is byte-for-byte or structurally exactly equal to the selected canonical suffix.
* The retained suffix remains in the same order.
* No committed replacement contains an unexpected tool call.
* No replacement commits after cancellation.
* No obsolete proposal commits to a changed history generation.
* Compaction failure does not mutate canonical history.
* Retry and compaction policy caps are never exceeded.
* A compaction summary operation never executes a tool.
* Default traces contain no prohibited raw content.
* Old traces remain readable.
* Exact-replay fixtures preserve the intended prefix and append the compaction instruction exactly once.
* No unexpected request-domain changes occur in append-only no-compaction fixtures.
* Every committed automatic compaction strictly reduces its selected source.
* Every committed automatic compaction meets the configured minimum headroom.
* No immediate compaction loop occurs without newly accumulated genuine pressure.
* All critical deterministic fixture facts survive.
* All forbidden obsolete facts remain absent.
* A compaction start always reaches a terminal state unless a crash fixture intentionally models interruption.
* A timeout or cancellation leaves canonical history unchanged.
* Measurement collection never invokes effectful request preparation twice.

### Resource budgets

Keep raw metrics authoritative.

For deterministic fixtures, fail on any unapproved regression in:

* model-request count;
* compaction-call count;
* retry count;
* estimated uncached input;
* checkpoint bytes;
* repeated unchanged-read bytes;
* duplicate tool signatures;
* duplicate failed approaches;
* requests to productive continuation;
* requests to next compaction;
* post-compaction headroom;
* cumulative checkpoint growth.

Use exact equality where the fixture is fully deterministic.

Use a small explicit relative allowance, such as 5%, only for aggregate metrics that genuinely vary because of estimator changes or corpus aggregation. Never use tolerance to hide a structural mismatch.

### Secondary efficiency score

A secondary ranking score may resemble:

`ordinary_uncached_input`
`+ cached_input_weight × ordinary_cached_input`
`+ compactor_uncached_input`
`+ compactor_cached_weight × compactor_cached_input`
`+ reread_weight × repeated_unchanged_read_bytes`
`+ duplicate_tool_weight × duplicate_tool_calls`

Requirements:

* weights live in eval configuration, not core;
* raw components are always reported;
* provider-reported cache data and prefix proxies are not mixed;
* the score cannot override task success or hard invariants;
* the score is not named “quality.”

## Phase 9: introduce a narrow strategy boundary

Preserve the current `Compactor` seam or add only a small host-owned strategy selection mechanism.

Acceptable forms include:

* a small enum;
* a trait already consistent with the current architecture;
* a versioned strategy descriptor;
* a few explicit constructors.

Do not introduce:

* plugin discovery;
* dynamic loading;
* dependency injection;
* runtime package graphs;
* registration macros;
* service definitions for every component.

Every strategy must have:

* stable ID;
* schema version;
* prompt fingerprint where relevant;
* source-selection policy;
* retained-tail policy;
* request-layout mode;
* output acceptance policy;
* trace identity.

Keep the current implementation selectable as the baseline.

## Phase 10: structured cumulative checkpoint candidate

Implement a candidate strategy, but do not make it the default without provider-backed evidence.

Use a stable, model-neutral, human-readable checkpoint layout:

* Goal
* Constraints and Preferences
* Current Checkpoint
* Decisions and Rationale
* Progress

  * Done
  * In Progress
  * Blocked
* Failed Attempts
* Verification
* Workspace Ledger
* Next Concrete Action
* Critical Context

The exact headings may be improved, but the schema must distinguish:

* current facts from superseded facts;
* accepted decisions from rejected alternatives;
* completed work from pending work;
* failed attempts from future steps;
* tests actually run from tests merely proposed;
* verified state from assumptions;
* user constraints from agent-generated plans.

Add a versioned tea checkpoint marker so repeated compaction can identify prior checkpoints without fuzzy text matching.

The format must remain readable as ordinary text. Do not require provider-specific structured-output APIs.

### Cumulative behavior

When compacting a history that already contains a tea checkpoint:

* recognize the previous version;
* merge prior durable state with the newly discarded span;
* avoid duplicating old sections;
* apply latest-wins semantics for superseded facts;
* retain failed approaches and their failure reasons when still relevant;
* carry forward exact operational state;
* record checkpoint generation;
* prevent unbounded checkpoint growth;
* retain the exact recent suffix outside the model-authored checkpoint.

Add deterministic fixtures for at least five successive compactions.

Measure:

* critical-fact survival by generation;
* obsolete-fact resurrection;
* checkpoint growth;
* duplicated lines;
* ledger stability;
* headroom;
* next-compaction distance.

## Phase 11: deterministic workspace ledger

Build a compact ledger from events tea already observes.

This belongs in `tea-agent` or trace/eval code, not in provider-neutral core.

Track at least:

* files read;
* files created;
* files modified;
* files deleted;
* commands run;
* tests and checks run;
* exit status;
* important failed commands;
* exact working directory where relevant;
* file or repository generation;
* short stable diagnostic fingerprints;
* unresolved verification work.

Requirements:

* deterministic ordering;
* cumulative merging;
* exact path preservation;
* deduplication;
* bounded size;
* no complete tool output;
* no complete file contents;
* no invented status;
* distinguish “not run” from “failed”;
* distinguish “read” from “modified”;
* distinguish a command retry after a change from an unchanged duplicate.

The model-authored checkpoint and harness-derived ledger have different responsibilities:

* the model describes semantic intent, decisions, rationale, blockers, and next action;
* the harness preserves exact operations and statuses it directly observed.

Do not ask the model to regenerate data the harness already possesses exactly.

The ledger must be independently testable without inference.

## Phase 12: compaction request-layout candidates

Implement separate candidate modes so each variable can be measured independently.

### Candidate A: exact cache replay

Construct the summary request by preserving, where compatible with tea’s provider abstraction:

* the selected model;
* the exact ordinary system prompt;
* deterministic ordered tool definitions;
* the exact selected provider-visible source context;
* cache-relevant request-envelope configuration;
* one appended compaction instruction.

Tool definitions may remain present to preserve the stable prefix, while tool execution is prohibited independently by the run mode.

The request must not accidentally omit tool definitions if they were present in the preceding ordinary request.

Measure every request-domain component.

Do not assume identical context bytes imply provider cache reuse.

### Candidate B: incremental checkpoint update

Construct the summary request from:

* previous tea checkpoint;
* newly discarded conversation span;
* split-turn prefix;
* deterministic workspace-ledger delta;
* update instruction.

Keep the exact recent suffix outside the generated checkpoint.

This mode may reduce nominal input size but forfeit a warm provider prefix. Do not assume it is cheaper.

### Candidate C: standalone fallback

Preserve a bounded standalone path for cases where exact replay cannot fit or is unavailable.

Measure why fallback occurred.

Do not silently switch modes without a trace field.

### One-variable experiments

Initially compare modes with only one changed variable.

Do not simultaneously change:

* prompt structure;
* retained-tail policy;
* thinking level;
* tool-result pruning;
* output budget;
* and request mode.

That makes results uninterpretable.

## Phase 13: request-envelope experiments

Tea’s current or future cache-domain proxy must account for more than message text.

Add explicit deterministic experiments for:

* inherited thinking configuration;
* compaction-specific thinking configuration;
* inherited output-token limit;
* compaction-specific output-token limit;
* inherited routing/service tier;
* compaction-specific routing/service tier;
* tools present versus omitted;
* stable versus reordered tools;
* stable versus changed system prompt;
* stable versus changed model;
* runtime approval-policy changes outside tool definitions.

The offline suite can prove:

* which components differ;
* whether the request is an exact prefix;
* whether tea’s normalized domain is stable;
* whether adapter serialization changed.

Only a provider that reports cache usage can prove actual hosted cache reuse.

Do not make provider-specific conclusions from offline fingerprints.

If the current compactor forces a different reasoning/thinking mode than ordinary turns, preserve that behavior in the baseline strategy and add a separate candidate rather than silently changing it.

## Phase 14: source selection and retained-tail policy

Treat source selection and retained-tail selection as semantic layout decisions, not only token arithmetic.

Requirements:

* never split a tool-call/result relationship;
* preserve the exact retained suffix selected by policy;
* make split-turn prefixes explicit;
* preserve recent user intent;
* preserve current in-progress operational state;
* use provider-supplied or configured context capacity;
* do not hard-code limits based on model names;
* do not blindly copy another agent’s fixed retained-token constant;
* measure retained bytes and tokens;
* measure whether the first post-compaction request has sufficient headroom.

Represent pre-turn, manual, overflow, and mid-turn behavior separately if tea actually has those phases.

Do not add a phase solely to resemble Codex.

Where layout differs by phase, test the exact order of:

* system instructions;
* environment or initial context;
* prior checkpoint;
* retained user message;
* retained assistant/tool span;
* new user message.

The final layout must make the continuation unambiguous to the model while preserving tea’s existing provider projection semantics.

## Phase 15: stale accounting and headroom correctness

Audit all threshold decisions after:

* a large tool result;
* a committed compaction;
* a provider-overflow retry;
* a cancelled compaction;
* a failed compaction.

Do not rely on stale pre-compaction provider usage when the pending request has changed materially.

Ensure that newly appended tool output is included before the next model call.

Track estimator error between:

* tea’s local request estimate;
* the final prepared `ModelRequest`;
* adapter-serialized bytes;
* provider-reported input tokens where available.

Break error down by region where feasible:

* system;
* tools;
* existing context;
* newly appended messages;
* tool results;
* checkpoint;
* ledger.

Near a hard context limit, use a conservative decision rule rather than the least conservative estimate.

A successful automatic compaction must satisfy an explicit minimum working-headroom policy.

Do not define success merely as “smaller than before.”

## Phase 16: post-compaction acceptance gate

Before committing an automatic model-visible replacement, require:

* source generation still matches;
* source IDs still match;
* retained suffix is exact;
* tool-call/result structure is valid;
* checkpoint is non-empty;
* checkpoint contains no tool calls;
* proposal is strictly smaller than selected source;
* estimated next request is below the configured target;
* minimum post-compaction headroom exists;
* retry and compaction caps remain within policy;
* operation has not been cancelled or timed out.

On rejection:

* do not mutate canonical state;
* emit a typed reason;
* emit terminal lifecycle data;
* retain diagnostic measurements;
* do not retry indefinitely.

Examples of typed rejection reasons:

* stale source generation;
* source boundary changed;
* retained suffix mismatch;
* invalid tool structure;
* empty checkpoint;
* unexpected tool call;
* non-shrinking replacement;
* insufficient headroom;
* policy cap reached;
* cancelled;
* timed out.

## Phase 17: model-free tool-result pruning candidate

Implement this only if baseline measurements demonstrate that old tool-result payloads materially dominate context or compactor requests.

Keep it as an independent stage and strategy dimension.

Requirements:

* preserve full original results in durable history or trace;
* alter only the derived model-visible surface;
* preserve the tool-call/result relationship;
* do not prune unresolved or recent results needed by the retained turn;
* use bounded head plus omission marker plus bounded tail;
* record original and replacement sizes;
* record exact shadowed IDs;
* reject a replacement that is not smaller;
* make thresholds explicit;
* make head/tail limits explicit;
* test UTF-8 and structured content boundaries;
* avoid pretending the pruned representation is the original content.

Do not combine pruning with a new summary prompt in the first comparison.

Do not make pruning the default solely because it saves bytes. It must preserve deterministic continuation invariants and, before broad activation, provider-backed task quality.

## Phase 18: optional hosted-provider canary

This lane is optional and must use existing tea provider adapters.

It runs only when explicitly enabled and credentials are already available.

Do not:

* add a local inference path;
* require credentials in CI;
* require one named provider;
* add a provider merely for compaction evaluation;
* silently spend provider credits;
* run provider canaries during ordinary tests.

Provide an explicit command and opt-in flag.

The hosted lane should support:

### Episode-level compaction tests

Given a curated pre-compaction transcript:

* run the production compactor;
* capture the generated checkpoint;
* run deterministic fact checks;
* measure checkpoint size;
* measure actual provider usage;
* record cache-read/write tokens where reported;
* repeat enough times to expose variance.

### Continuation tests

For a small curated set of coding tasks:

* run to a controlled compaction point;
* compact;
* continue;
* validate task outcome;
* measure rereading;
* measure duplicate commands;
* measure failed-approach repetition;
* measure time or requests to productive continuation;
* measure distance to next compaction.

### Provider-backed result handling

Report:

* model;
* provider;
* adapter version;
* request-envelope fingerprint;
* strategy ID/version;
* number of repetitions;
* median;
* p90;
* raw run artifacts;
* cache status;
* failures and exclusions.

Do not check credentials or provider outputs into the repository.

Sanitized, intentionally reviewed fixtures may be checked in only through an explicit capture workflow.

### Provider-backed promotion gate

A materially different model-facing default may be promoted only when:

1. all provider-free hard gates pass;
2. at least one real hosted inference backend has generated the checkpoint;
3. every critical fixture fact survives;
4. cumulative-compaction episodes do not show unacceptable drift;
5. end-to-end continuation tasks do not regress;
6. repeated work does not regress;
7. post-compaction headroom does not regress;
8. compaction frequency does not regress.

Actual provider cache accounting is additionally required before claiming improved prompt-cache reuse.

A strategy may improve provider-neutral layout and still have cache status `proxy_only`. Describe it accurately.

If no provider run is available, leave materially different model-facing strategies non-default.

## Phase 19: controlled strategy comparison

Compare at least:

* baseline/current strategy;
* structured cumulative checkpoint;
* exact cache replay;
* incremental checkpoint update;
* request-envelope variants;
* tool-result pruning separately if justified.

For every strategy report:

* hard-invariant pass/fail;
* deterministic task outcome;
* critical-fact retention;
* stale facts;
* contradictory facts;
* exact identifiers retained;
* checkpoint bytes;
* ledger bytes;
* retained-suffix bytes;
* pre/post context;
* headroom;
* exact-prefix measurements;
* cache-domain differences;
* provider cache accounting when available;
* estimated compactor input;
* provider-reported compactor usage when available;
* ordinary request usage;
* compaction count;
* immediate recompaction;
* next-compaction distance;
* repeated unchanged-read bytes;
* duplicate tools;
* duplicate failed approaches;
* first productive-action distance;
* timeouts, cancellations, and failures.

For initial experiments, change only one dimension at a time.

Do not select a winner from one weighted aggregate number.

## Phase 20: default selection rules

Mechanical fixes may become default when fully proven by deterministic tests.

Examples:

* lifecycle identity;
* trace correctness;
* exact suffix validation;
* stale-generation rejection;
* retry caps;
* timeout behavior;
* stale-usage fixes;
* deterministic tool ordering;
* accurate request measurement;
* minimum-headroom acceptance;
* typed rejection reasons.

A model-facing candidate becomes the default only when it:

* preserves every hard invariant;
* preserves task success;
* preserves every critical fact;
* does not resurrect obsolete decisions;
* creates at least as much usable headroom;
* does not increase immediate recompaction;
* does not increase overall compaction frequency;
* does not increase repeated work;
* does not increase cumulative drift;
* improves at least one meaningful efficiency metric;
* has provider-backed evidence.

If no candidate satisfies these conditions:

* retain the existing model-facing default;
* land the observability;
* land the deterministic suite;
* land mechanical correctness fixes;
* land candidate strategies as explicitly experimental;
* document the unresolved evidence gap.

That is a successful result. Do not manufacture a winner.

## Required focused tests for exact-prefix behavior

Add tests proving:

1. Ordinary append-only turns preserve the stable prefix.
2. Exact-replay compaction preserves the selected provider-visible source prefix.
3. The compaction instruction is appended exactly once.
4. System instructions remain identical when expected.
5. Tool definitions remain identical when expected.
6. Tool order remains identical when expected.
7. Omitting tool definitions is detected.
8. Changing thinking configuration is detected.
9. Changing output-token limits is detected.
10. Changing adapter routing/service-tier configuration is detected.
11. Changing only runtime approval policy does not mutate tool definitions.
12. Adapter-level domain changes can be observed without provider-specific core fields.
13. Provider cache-read/write tokens are reported only when supplied.
14. Prefix-proxy measurements are never labeled as cache hits.
15. The first ordinary request after compaction joins to the responsible compaction ID.
16. The pre-compaction ordinary request can be compared directly with the post-compaction request.
17. Measurement does not invoke a context hook twice.
18. Measurement does not serialize a materially different request than the one sent.
19. Tool execution remains disabled during summarization even when tool definitions are retained.
20. A standalone fallback is visibly distinguished from exact replay.

## Required focused tests for continuation safety

Add tests proving:

1. Compaction recomputes or invalidates stale usage.
2. The first post-compaction prompt does not immediately retrigger from stale accounting.
3. A newly appended large tool result is included in the threshold decision.
4. Concurrent writes cannot be lost by snapshot replacement.
5. Cancellation before commit leaves history unchanged.
6. Timeout leaves history unchanged.
7. A non-shrinking proposal is rejected.
8. A shrinking but insufficient-headroom proposal is rejected.
9. Repeated compaction remains bounded.
10. A prior checkpoint is not duplicated accidentally.
11. Exact retained suffix survives multiple generations.
12. Trace replay preserves source and retained IDs.
13. A started but unterminated compaction is detectable.
14. Old traces remain readable.
15. Repeated unchanged reads are distinguished from legitimate refreshes.

## Documentation

Add concise documentation covering:

### Architecture

* canonical history;
* provider-visible history;
* compaction source;
* retained suffix;
* checkpoint;
* workspace ledger;
* strategy;
* request-layout observation;
* provider usage;
* lifecycle;
* transaction;
* continuation observation.

### Metrics

Define every metric, unit, and source of truth.

Clearly distinguish:

* bytes;
* estimated tokens;
* provider-reported tokens;
* provider cache data;
* prefix proxy;
* estimate;
* inference.

### Privacy

Explain default trace contents, omitted contents, debug behavior, and fixture handling.

### Baselines

Explain:

* how to run the suite;
* how to compare strategies;
* how to update a baseline;
* why a reason is required;
* how to inspect one regression.

### Hosted canary

Explain:

* that it is optional;
* that it uses existing providers only;
* that it is never run implicitly;
* which conclusions require it;
* which conclusions remain provider-free.

Do not include local inference setup.

## CI and commands

Use the repository’s pinned toolchain and mirror existing CI.

Run at minimum the repository equivalents of:

* formatting checks;
* lint and clippy checks;
* `cargo test --workspace`;
* focused `tea-core` compaction tests;
* automatic-policy tests;
* provider-overflow tests;
* request-layout/cacheability tests;
* trace compatibility tests;
* the existing quality fast lane;
* the new provider-free compaction lane.

The default CI lane must:

* require no credentials;
* perform no network calls;
* run deterministic fixtures;
* compare checked-in baselines;
* produce useful failure diagnostics;
* never rewrite baselines.

An optional hosted workflow must require explicit manual invocation or an explicit opt-in configuration.

## Commit sequence

Keep the work reviewable.

Use a sequence resembling:

1. Upstream research and tea behavior audit.
2. Metric glossary and compaction identity.
3. Exact request-layout measurement.
4. Transaction, stale-generation, cancellation, and deadline hardening.
5. Durable trace lifecycle and backward compatibility.
6. Deterministic fixtures and current baseline.
7. Rework and continuation metrics.
8. Minimum-headroom and stale-accounting gates.
9. Versioned strategy boundary.
10. Structured checkpoint candidate.
11. Deterministic workspace ledger.
12. Exact-replay and incremental-update candidates.
13. Optional model-free pruning if justified.
14. CI budgets and reports.
15. Optional hosted canary harness.
16. Final comparison and default-selection decision.

Do not mix the first observability commit with the first model-facing strategy change.

Each commit must leave the repository buildable and tested.

## Final deliverables

Produce:

1. Complete implementation.
2. `docs/compaction-research.md`.
3. Compaction architecture documentation.
4. Metric glossary.
5. Trace privacy documentation.
6. Fixture inventory.
7. Deterministic compactor and provider fixtures.
8. Provider-free compaction eval lane.
9. Checked-in baseline for the original strategy.
10. Explicit baseline-update command.
11. Human-readable per-compaction report.
12. Aggregate per-run report.
13. Versioned baseline strategy.
14. Structured checkpoint candidate.
15. Deterministic workspace ledger.
16. Exact-replay and incremental-update candidates.
17. Model-free pruning only if justified by measurements.
18. Optional hosted-provider canary using existing adapters.
19. CI regression budgets.
20. Baseline-versus-candidate report.
21. Final default-selection decision.
22. List of unresolved hypotheses requiring provider-reported cache accounting or provider-generated summaries.

## Final report format

End with a precise report containing:

### Implemented

List completed code, tests, fixtures, reports, and documentation.

### Current baseline

Describe the original compaction strategy and its measured behavior.

### Mechanical fixes

Identify changes proven correct without inference.

### Candidate strategies

For each candidate, give:

* strategy ID/version;
* changed dimension;
* deterministic results;
* known limitations;
* whether it remains experimental.

### Regression results

Include the complete metric vector and hard-gate results.

### Default decision

State exactly what became the default and why.

If the original model-facing strategy remained the default, say so directly.

### Evidence classification

Classify every major conclusion as:

* structural proof;
* deterministic fixture result;
* provider-reported measurement;
* prefix proxy;
* estimate;
* inference;
* unresolved.

### Deferred work

List only work that genuinely requires provider-generated summaries or provider-reported cache accounting.

Do not describe offline deterministic tests as proof of semantic summary quality.

Do not describe an exact prefix as proof of a provider cache hit.

Do not claim Codex-level compaction quality without end-to-end provider-backed continuation evidence.
