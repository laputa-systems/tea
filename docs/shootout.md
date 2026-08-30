# Pi/Tea shootout

This is the durable contract for the repository's pinned coding-harness
shootout. It combines the historical implementation brief in
`shootout.md` with the efficiency mission in `EFFICIENCY.md`. Those root files
are retained as legacy context; this document is the maintained entry point.

The experiment is deliberately narrow: one checked-in coding task, one model,
one provider, and two static harnesses. It is evidence for this task and this
configuration, not a broad benchmark or a claim of general model or product
superiority.

## Mission and scope

The primary comparison is:

```text
tea-static  versus  pi-static
```

Both conditions must use the same task, baseline repository, validator,
provider, requested model, reasoning level, output ceiling, shell authority,
and timeout. The target metric is **uncached generation** (uncached input plus
output). The stretch goal is a strictly lower value for Tea; a tie satisfies
the minimum efficiency target. Correctness and the closed capability contract
are gates, never tradeable costs.

`tea-jit` is a separate evolution condition. It uses the same durable Tea
runtime but enables one bounded task-local harness candidate. It is not part of
the static efficiency claim, and static commands never instantiate or run it.

The v0 experiment intentionally does not implement a provider matrix, model
selection, web-search or browser APIs, autonomous research loops, subagents,
global harness promotion, native self-modification, or long-term benchmark
memory.

## Fixed experiment contract

The current pinned case is `express-3936-medium` and its checked-in fast
validator. The fixed live configuration is:

```text
provider       = openrouter
model          = deepseek/deepseek-v4-flash-0731
thinking       = high
output limit   = unlimited
timeout        = 900 seconds
tools          = read, bash, edit, find (this exact order)
```

The model-facing coding surface is closed. Neither static condition has a
dedicated web tool, browser, subagent, or ambient extension. `curl` remains
available only through the ordinary `bash` capability so network authority is
identical. Tea's static evaluator marks durable artifact recovery as
host-only at the provider boundary: the OpenRouter adapter applies a
four-name model-tool allowlist while the durable supervisor retains its
recovery tools for execution and retention. The default durable runtime and
the JIT condition retain their normal model-readable recovery policy.

Every attempt gets a fresh detached baseline checkout, empty HOME, private
TMPDIR, private dependency tree, and explicit shell allowlist. Repeats are
counterbalanced: each lane runs its selected Pi/Tea order sequentially, while
independent lanes may run in parallel. Setup caches may be shared, but no
working tree or `node_modules` directory is shared between attempts.

The provider credential crosses only the final live adapter boundary:

```sh
vault OPENROUTER_API_KEY -- <adapter>
```

The orchestrator, evidence files, and coding-tool child processes never read
or serialize the key. Pi and Tea both receive the same non-secret shell
allowlist (`PATH`, attempt-local `HOME`/`TMPDIR`, locale, and the explicit npm
cache settings). Pi's bash factory disables session-environment exposure;
Tea's process capability uses the same environment policy.

## Normalized result contract

Both adapters emit `tea-coding-eval-result/v3`. The result keeps the following
semantic groups even when an adapter cannot observe a value (unknown is `null`,
not a manufactured zero):

```text
terminal, final_text, runtime, model, surface, timings, counts, usage,
cost, harness, trace
```

Shared counters have one meaning: `turns` is durable user messages,
`model_turns` is model-loop turns, and `tool_calls` is assistant-emitted tool
call blocks. `provider_requests` is exact when the adapter exposes it (Tea
does; Pi's SDK currently reports `null`). `retries` counts durable attempts
with an explicit retry reason, and `compactions` counts completed compaction
lifecycles.

### Token accounting

OpenRouter's `prompt_tokens` already includes cached prompt tokens. The
adapter, not the report renderer, owns this normalization:

```text
prompt_total        = prompt_tokens
uncached_input      = max(prompt_total - cache_read - cache_write, 0)
uncached_generation = uncached_input + output
all_tokens          = prompt_total + output
```

The result publishes `input` (uncached input), `prompt_total`, `output`,
`generation` (uncached generation), `all_tokens`, `reasoning`, `cache_read`,
and `cache_write` independently. Cache components are never added a second
time, and the comparison consumes these fields directly.

Cost is separate from token efficiency. Preserve `cost.kind` as
`provider-reported`, `catalog-estimate`, or `unavailable`; do not rank dollar
totals whose kinds differ.

### Surface and wire evidence

Each attempt retains:

```text
surface/system-prompt.txt
surface/tool-surface.json
surface/wire-requests.json
```

The final direct provider payload is the wire ground truth. It is captured
before credentials are attached, with credential-like fields redacted and
attempt paths normalized. Numeric model controls such as
`max_tokens`/`max_completion_tokens` remain visible so an unlimited run cannot
hide an adapter-imposed ceiling. The result's wire hashes are derived from
this retained witness, not from a reconstructed prompt or a summary.

Reports distinguish:

- controlled model, routing, sampling, output-limit, timeout, tool-order, and
  wire-shape mismatches, which block a strict conclusion;
- native prompt, tool-schema, and execution differences, which are measured
  harness results rather than silently treated as parity; and
- unobserved fields, such as missing route headers or Pi timeout visibility,
  which remain unknown rather than being guessed into equivalence.

## Adapter boundaries

### Pi

`evals/pi_shootout/sdk/src/pi-adapter.ts` embeds the exact pinned Pi SDK
directly (`@earendil-works/pi-ai` and
`@earendil-works/pi-coding-agent`, version `0.84.4`). It uses isolated in-memory
settings/session services, disables model-catalog refresh, registers the
public Pi `read`, `bash`, `edit`, and `find` tool definitions, subscribes to
events before prompting, and disposes the session on every path. The observer
captures the public provider payload and removes any converter default output
ceiling because the shootout contract is unlimited.

### Tea

`crates/tea-providers/src/bin/tea-eval.rs` is the provider-backed Tea adapter.
Both Tea modes use the durable `SessionSupervisor` path, an attempt-local
JSONL session, the pinned Luau coding builtins, and the same OpenRouter
configuration. Static-only guidance is evaluation-scoped and is not added to
JIT. Static mode also selects a non-model-readable artifact policy so durable
recovery readers stay host-only and cannot expand the four-tool wire surface.

Tea preserves provider-neutral core behavior: tool execution, durable session
reopen, effect attribution, and provider request capture remain in the core
runtime and adapter layers rather than in the report script.

## JIT evolution condition

When explicitly requested by `make pi-shootout` (not a static run), Tea JIT
enables the existing self-extension contract:

```text
maximum candidates       = 1
maximum activations      = 1
maximum epoch rollovers  = 1
maximum candidate source = 16 KiB
capability expansion     = forbidden
```

The model may choose `NoChange`. A candidate must retain observed evidence,
root-cause hypothesis, expected effect, regression risk, and changed surfaces.
Adaptation time and tokens count toward the task. The evidence records base,
initial, and final snapshots, candidate lineage, validation and rollover
timings, epoch usage, and the decision (`no-change`, `rejected`, or
`activated`). Provider-free deterministic tests cover activation, rejection,
budget limits, and safe epoch rollover; a live task is not required to stage a
candidate.

## Isolation, validator, and oracle rules

The external validator is the only correctness authority. A model terminal
status of `completed` is not a passing task unless the same validator passes.
Validator source and the known-correct fix commit remain outside the model
workspace. Attempt materialization contains only the baseline commit; the
remote is removed and `git cat-file -e <known-fix-commit>` must fail before an
adapter starts. This prevents the local Git object database from becoming an
oracle while preserving symmetric public `curl` access.

Infrastructure failures (missing result, malformed contract, wrong identity,
timeout cleanup failure, or validator execution failure) fail the command.
Benchmark failures (validator failure, model error, timeout with a valid
terminal result, or a rejected JIT candidate) remain in the evidence and do
not suppress reports.

## Running the experiment

The provider-free preflight is:

```sh
make pi-shootout-plan
make pi-shootout-check
```

Three repeats are a smoke/diagnostic run:

```sh
vault OPENROUTER_API_KEY -- make pi-shootout-static
```

The named serious static workflow uses seven counterbalanced repeats:

```sh
vault OPENROUTER_API_KEY -- make pi-shootout-static-serious
```

Use `make pi-shootout` or `make pi-shootout-serious` only when the JIT
evolution condition is intentionally in scope. To serialize repeat lanes,
set `PI_SHOOTOUT_PARALLEL_REPEATS=1`; otherwise bound parallelism with a value
between one and the repeat count.

After a run, render the provider-free comparison:

```sh
python3 -m evals.pi_shootout compare \
  --run-dir /tmp/tea-pi-shootout/runs/<run-id>
```

The analyzer writes `reports/comparison.json` and
`reports/comparison.md`. It validates shared identity, consumes normalized
fields, reports paired Tea-minus-Pi deltas with median/min/max and descriptive
bootstrap intervals, and includes durable Tea turn categories where
available. It never reconstructs token totals or turns from an unlike trace.

## Reading a result

Use this order:

1. Both attempts must pass the same validator and reach a valid terminal
   outcome.
2. Required controls and direct wire evidence must agree; unknown required
   observations prevent a strict claim.
3. Compare uncached generation, then prompt total and all-token total as
   separate context/billing views.
4. Compare wall time and comparable provider cost only when their definitions
   match.
5. Inspect native surface differences and durable turn categories as causal
   uncertainties, not as proof that the runtime caused an observed behavior.

Never declare a regression or win from one unseeded attempt. For repeated
static runs, report the paired outcome classes (`both pass`, one-sided pass,
or both fail), median and worst-case normalized totals, and the evidence path.
If cost kinds differ, say so explicitly. If Pi cannot expose reasoning,
provider-request, route, or timeout fields, retain `null` and name the
resulting uncertainty.

## Current evidence and known bottleneck

The largest measured Tea cost in the latest passing historical pair was
context replay: durable requests grew from roughly 1.8k to 14k raw prompt
tokens while cache reads grew from zero to roughly 13.6k. Tea also performed
more model turns and tool calls in that pair. These are evidence-backed leads,
not proof that the durable runtime caused the model's exploratory choices.

The first historical report overstated Tea's generation by treating cached
tokens as additional input. The v3 accounting above repairs that semantic
error. Historical numbers and any run with a failed validator, a malformed
wire surface, or an unknown required control are diagnostic only and must not
be pooled into a strict efficiency conclusion.

The latest corrected smoke evidence (three repeats in
`/private/tmp/tea-pi-shootout/runs/20260830T040602Z-5e580c5239dd`) had all Tea
validators pass and all Pi validators fail; it is therefore not a paired
efficiency result. A subsequent comparable single pair
(`20260830T042924Z-fdfbb25ce3fc`) passed both validators: Tea used 23,235
uncached-generation tokens versus Pi's 12,836. With provider-default sampling,
native prompt/tool differences, and missing route/timeout observations, the
analyzer correctly withheld a strict conclusion. These paths are ephemeral
diagnostic artifacts, not checked-in fixtures.

## Verification contract

The focused repository gate is:

```sh
make pi-shootout-check
```

It runs the Python result/report/comparison tests, pinned TypeScript install,
typecheck and SDK tests, Tea provider and durable-session tests, core runtime
tests, and offline quality cases. The live shootout is opt-in and must not be
added to normal `make test`, CI, formatters, linters, or pre-commit hooks.

Before handoff, record:

- the exact run directory and generated report paths;
- Pi SDK versions and lock digest;
- task, baseline, validator, model, provider, and thinking identity;
- validator outcomes and paired normalized usage;
- active tool order and wire-surface evidence;
- observed bottlenecks and every remaining unknown;
- the focused commands that passed.

That record is the durable experiment result. It is not a promise that either
harness will generalize beyond the pinned task and controls.
