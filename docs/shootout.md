# Pi/Tea shootout

This is the durable contract for the repository's pinned coding-harness
shootout. It combines the historical implementation brief in
`shootout.md` with the efficiency mission in `EFFICIENCY.md`. Those root files
are retained as legacy context; this document is the maintained entry point.

The experiment is deliberately narrow: one selected checked-in coding task,
one model, one provider, and two static harnesses. The repository currently
pins a medium case for routine comparisons and a harder case for timeout
calibration and future repeated comparisons. Evidence is specific to the
selected task and configuration, not a broad benchmark or a claim of general
model or product superiority.

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

The default pinned case is `express-3936-medium` and its checked-in fast
validator. The repository also carries `express-4205-hard` for longer-task
diagnostics and future repeated comparisons. The fixed live configuration is:

```text
provider       = openrouter
model          = deepseek/deepseek-v4-flash-0731
thinking       = high
output limit   = unlimited
sampling       = temperature 0; seed 20260829
timeout        = 900 seconds (medium); 1800 seconds (hard)
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

The runner supports controller-recognized early stopping only for Tea-only
diagnostic attempts. An operator writes an atomic, attempt-keyed stop request
through `python3 -m evals.pi_shootout stop`; the controller polls it, uses the
ordinary process-group cleanup, and records an `exclusion.json` witness. An
excluded lane remains outside `attempts`, reports, pairs, and efficiency gates.
Raw signals and malformed requests are ordinary infrastructure failures, and
paired runs cannot accept the stop protocol.

For safe Tea-only prompt diagnostics on macOS, the runner optionally selects
`macos-seatbelt-v1` or `macos-seatbelt-v2`. Both wrap only each Tea `bash`
child and its descendants in a profile that permits workspace/private attempt
paths and fixed toolchain read roots while blocking other data paths and
outbound network access. V2 additionally blocks reads and writes beneath the
workspace `.git` directory, making history commands fail without restricting
ordinary workspace listing or source work. The networked provider adapter
stays outside that profile. This is not a complete attempt sandbox and it narrows Tea’s shell
authority, so the runner rejects it for a paired Pi/Tea comparison unless an
identical Pi policy is implemented and recorded.

For a separate Tea-only invalid-edit diagnostic, the runner can select
`--edit-recovery-projection canonical-v1`. After the core has rejected the
known top-level `path`/`edits` edit envelope, Tea retains the raw call and
schema error durably and projects one canonical-envelope reminder onto only
the latest matching tool result in the cloned next provider context. It never
accepts, normalizes, or rewrites the rejected arguments. Because this changes
model-visible continuation context and token use, it is prohibited in a paired
comparison until Pi has an identical policy. Tea records the mode, correction
hash, hook identity, and distinct host profile for forensic attribution.

Static prompt composition is likewise explicit. `--static-prompt-profile`
selects `builtin-v1` (the paired default) or one of the Tea-only diagnostic
profiles `no-history-v1`, `prefix-guard-v1`, and `prefix-guard-focused-v1`.
The no-history profile replaces only the generic Bash section’s Git/history
invitation with workspace-local build and validation guidance. The prefix-guard profile retains that
replacement and appends an explicit RegExp mount-prefix semantic guard derived
from the observed task failure. Its focused variant additionally requires an
`index.js`-only guard at the existing trim boundary and forbids repro-file and
matching-internal edits; both are task-specific diagnostics that cannot enter
a paired comparison. These profiles keep the resolved `read`, `bash`, `edit`,
`find` tool definitions, order, and authority intact. Tea retains the profile,
projected Bash-section hash, full system-prompt hash, and resulting durable
host profile with the attempt.

The optional fresh-static `--pre-edit-tool-gate direct-edit-v1` condition makes
the initial workflow state explicit after prompt-only screens choose extended
exploration. Until a prior successful `edit` result is present, both adapters
block `bash` and `find` as the same model-visible policy error while leaving
`read` and `edit` available; then both reopen the blocked tools for focused
validation. It never rewrites model arguments, and a same-batch edit does not
unlock a sibling shell call. Pi and Tea retain the same mode, blocked-tool
order, unlock rule, same-batch rule, and block-reason hash in their result
surfaces. It is static-only (never JIT); comparison rejects a disagreement
between adapters or with the recorded run mode.

The stricter `--pre-edit-tool-gate source-local-v1` condition is available
only to a fresh paired static run (`pi-static` and `tea-static` together); it
never enters `tea-jit` or a Tea-only diagnostic. Before a successful
target-local `edit`, it permits only `read` and `edit` calls whose declared
paths are listed in the task's checked-in
`source_local_v1` metadata (`tea-coding-eval-source-local/v1`). Each target
must occur literally in the task prompt and be a regular file in the runner's
clean baseline worktree. It blocks `bash`, `find`, and non-target `read` or
`edit` calls with the same generic model-visible reason in both adapters.
A successful unlock must join the durable/public tool-result call ID to an
admitted target-local edit call; a same-batch result cannot unlock siblings.
The ordered target list is retained in each adapter policy object and run
metadata, and comparison treats any disagreement as non-comparable.

`--post-edit-validation-gate unmasked-evidence-v1` is a separate shared
fresh-static workflow condition. It is valid only for the paired Pi/Tea static
run with `--pre-edit-tool-gate source-local-v1`; it is rejected for Tea-only
and JIT configurations. After every successful declared-target native `edit`,
only a later direct foreground `bash` child with the content-free
`"exited-zero"` process witness qualifies as validation evidence. Generic
tool success is neither recorded as nor sufficient for that witness. Pipeline
and status-suppression wrappers do not qualify, same-batch calls are too early,
and any later successful native `edit` result (including a non-target edit
after the source-local prerequisite) resets the requirement. Bash filesystem
effects do not reset it. A terminal attempt with missing evidence receives at
most one completion reminder. The adapters retain one identical policy object in both
result policy surfaces and a content-free result-root `validation_evidence`
witness. This does not identify, invoke, or expose the host validator, and it
does not establish that the selected workspace-local check was the right test.
The comparison only controls the shared workflow policy and run metadata; it
does not treat outcome evidence as native-harness parity.

The provider credential crosses only the final live adapter boundary:

```sh
vault OPENROUTER_API_KEY -- <adapter>
```

The orchestrator, evidence files, and coding-tool child processes never read
or serialize the key. Pi and Tea both receive the same non-secret shell
allowlist (`PATH`, attempt-local `HOME`/`TMPDIR`, locale, and the explicit npm
cache settings). Pi's bash factory disables session-environment exposure;
Tea's process capability uses the same environment policy.

Timeout policy is task-specific in the Makefile and CLI. Set
`PI_SHOOTOUT_TIMEOUT_SECONDS=0` or `--timeout-seconds 0` only for an explicit
uncapped diagnostic: the runner then waits without imposing an outer wall
clock, while individual model-generated shell commands may still run without
end. Both static adapters derive their request and stream-idle timeouts from
this budget and use the 24-hour transport guard for an uncapped diagnostic.
The runner then allows both static adapters a 15-second finalization
grace for a terminal adapter result and direct-request witness before forced
cleanup. The zero-budget diagnostic uses a 24-hour transport guard because the
HTTP clients require a finite deadline, so neither evaluator imposes a shorter
body-idle cutoff while the provider is still delivering a response. Paired
static attempts do not replay retryable pre-output transport/status failures:
both Pi and Tea use zero provider retries. Output-bearing failures remain
terminal because replay would make delivery ambiguous. Such a diagnostic is
useful for calibrating the finite hard-case ceiling but is not a scored
efficiency result.

## Normalized result contract

Fresh adapters emit `tea-coding-eval-result/v4`. The result keeps the following
semantic groups even when an adapter cannot observe a value (unknown is `null`,
not a manufactured zero):

```text
terminal, final_text, runtime, model, surface, timings, counts, usage,
cost, effective_policy, harness, validation_evidence, trace
```

The comparison reader has a narrow, read-only v3 compatibility path. A v3 run
with complete post-edit policy and validation-evidence roots is an enriched-v3
artifact and retains its normal comparison scope. A v3 run that lacks the run
post-edit mode and lacks all three result roots is a genuine legacy artifact;
comparison projects only the disabled policy into its in-memory view and marks
the unavailable witness as unknown, so it cannot support a strict efficiency
conclusion. It never rewrites persisted evidence. A partial v3 shape, a mixed
v3/v4 pair, or a fresh v4 result missing any required root is rejected rather
than defaulted.

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
Each adapter also retains only the case-insensitive
`x-openrouter-provider` and `x-openrouter-model` response values, with fixed
`OpenRouter response header` provenance; it never persists arbitrary response
headers. If either adapter does not receive those headers, the route remains
unobserved and blocks a strict conclusion.

Reports distinguish:

- controlled model, routing, sampling, output-limit, timeout, tool-order, and
  wire-shape mismatches, which block a strict conclusion;
- native prompt, tool-schema, and execution differences, which are measured
  harness results rather than silently treated as parity; and
- unobserved fields, such as missing route headers or an adapter timeout
  policy that cannot be observed, which remain unknown rather than being
  guessed into equivalence.

The live static adapters set the same explicit zero temperature, seed, and
`store: false` retention policy; provider-default sampling is not part of the
static condition.

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
The observer also applies the static condition's shared temperature `0` and
seed `20260829` at the final request boundary. Its in-memory settings pin
both provider-request and HTTP-idle timeouts to the task budget (or the
documented diagnostic guard), so those controls are reported rather than
inferred.

For the optional fresh-static `direct-edit-v1` condition, that same hidden
observer blocks only `bash` and `find` until it observes a prior successful
`edit` result. It leaves the four public tool definitions and static authority
unchanged, applies the same-batch rule before any tool executes, and records
the exact policy object in both result surfaces.

For the paired-only fresh-static `source-local-v1` condition, the hidden
observer additionally admits pre-edit `read` and `edit` only when their exact
paths are in task-owned `source_local_v1` metadata. It copies rather than
rewrites public tool arguments, and unlocks only when the result carries the
same ID as an admitted target-local edit call.

### Tea

`crates/tea-providers/src/bin/tea-eval.rs` is the provider-backed Tea adapter.
Both Tea modes use the durable `SessionSupervisor` path, an attempt-local
JSONL session, the pinned Luau coding builtins, and the same OpenRouter
configuration. Static-only guidance is evaluation-scoped and is not added to
JIT. Static mode also selects a non-model-readable artifact policy so durable
recovery readers stay host-only and cannot expand the four-tool wire surface.
Static Tea sends the same explicit temperature, seed, and `store: false`
retention policy through its OpenRouter configuration; JIT leaves provider
sampling at its normal default.

`OpenRouterRequestCapture` records the same two whitelisted returned-route
headers as Pi when OpenRouter supplies them, without retaining raw HTTP
headers. The report preserves a missing route as unknown rather than deriving
one from the requested model or routing policy.

For reasoning/tool sequences, `crates/tea-providers/src/openrouter/response.rs`
retains OpenRouter `reasoning_details` as provider-private continuation state,
and `crates/tea-providers/src/openai.rs` replays that state on the next Chat
Completions request. The normal payload path still supplies the empty
`reasoning_content` marker required by DeepSeek compatibility. Neither field
is exposed to transcript rendering or coding tools. A single retained
continuation item is bounded at 1 MiB and the corresponding JSONL entry at
2 MiB: this accommodates the unlimited high-reasoning evaluation condition
while retaining a finite durable-state boundary.

Tea preserves provider-neutral core behavior: tool execution, durable session
reopen, effect attribution, and provider request capture remain in the core
runtime and adapter layers rather than in the report script.

For the optional fresh-static `direct-edit-v1` condition,
`PreEditToolGateHook` derives its state from the durable context and blocks
only `bash` and `find` until a prior successful `edit` result. The paired Pi
observer uses the same model-visible block reason, blocked-tool order, unlock
condition, and same-batch rule. The condition is static-only and comparison
requires its full policy evidence to agree across adapters and run metadata;
it does not imply resumable-session equivalence.

For paired-only fresh-static `source-local-v1`, `PreEditToolGateHook` derives
the same state from durable assistant/tool-result pairs: it parses the earlier
edit arguments, admits only declared target paths, and requires the successful
result's exact tool-call ID. This keeps a rehydrated Tea context from treating
unrelated edit-result text as a source-local unlock.

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

For a Tea-only hard-case baseline, use `make pi-shootout-tea-static` with
`PI_SHOOTOUT_TASK=express-4205-hard` (and usually one serialized repeat). The
run retains Tea's normal attempt and provider evidence and writes
`reports/tea-static.md`; because Pi is intentionally absent, no paired
comparison report is produced.

After a run, render the provider-free comparison:

```sh
python3 -m evals.pi_shootout compare \
  --run-dir /tmp/tea-pi-shootout/runs/<run-id>
```

The analyzer writes `reports/comparison.json` (`tea-pi-shootout-analysis/v2`)
and `reports/comparison.md`. It validates shared identity, consumes normalized
fields, reports paired Tea-minus-Pi deltas with median/min/max and descriptive
bootstrap intervals, and includes durable Tea turn categories where
available. It never reconstructs token totals or turns from an unlike trace.
Its scoped `efficiency_gate` can pass only with at least three complete,
strictly comparable paired repeats, every validator passing, and both the
median and worst Tea-minus-Pi normalized uncached-generation deltas at or
below zero. The gate describes that one run; it is not a cross-task benchmark
claim and does not remove the hard-case requirement.

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
6. Read the analyzer's `efficiency_gate` separately from comparability: it
   rejects one- or two-pair runs and any run whose median or worst normalized
   uncached-generation delta is positive.

Never declare a regression or win from one attempt. For repeated
static runs, report the paired outcome classes (`both pass`, one-sided pass,
or both fail), median and worst-case normalized totals, and the evidence path.
If cost kinds differ, say so explicitly. If Pi cannot expose reasoning,
provider-request, or route fields, retain `null` and name the
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
uncached-generation tokens versus Pi's 12,836. That historical pair used
provider-default sampling,
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
