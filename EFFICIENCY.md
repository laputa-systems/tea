# Tea static token-efficiency mission

> Maintained contract: [`docs/shootout.md`](docs/shootout.md). This root file
> is retained as historical mission context.

## Measurement hardening notice

The historical numbers and conclusions below predate the current shootout
hardening and are not a basis for a strict Tea-versus-Pi efficiency claim.
They remain useful as leads about accounting and runtime behavior only.

Current measurements use `tea-coding-eval-result/v3`, retain the sanitized
final provider payload in each attempt's `surface/wire-requests.json`, and
derive normalized wire facts from that artifact. The retained request witness,
not a reconstructed prompt/tool surface or summary alone, establishes the
model-facing request. Credentials are removed before it is persisted.

The comparison distinguishes three kinds of facts:

- Controlled model fields, wire shape, and observed provider routes must agree;
  a mismatch blocks a strict efficiency conclusion.
- Native prompt/tool-schema/execution differences are reported as harness
  results. They are not silently hidden or treated as parity gates.
- Unobserved fields remain unknown. In particular, missing route headers or
  adapter timeout visibility prevent a strict conclusion rather than being
  guessed into equivalence.

Run `make pi-shootout-static` for the three-repeat smoke workflow, or
`make pi-shootout-static-serious` for seven counterbalanced repeats. The
provider-free `python3 -m evals.pi_shootout compare --run-dir <run>` output
contains paired deltas and deterministic descriptive bootstrap intervals. The
Tea-only target `make pi-shootout-tea-static` accepts
`PI_SHOOTOUT_TASK=express-4205-hard` for a single-baseline hard-case run; it
writes Tea evidence and `reports/tea-static.md` but intentionally cannot emit a
paired comparison. The
live static adapters use temperature `0` and seed `20260829`; the analyzer
still reports unknown route and timeout fields instead of inferring them.

Repeats are isolated parallel lanes by default: two requested repeats run in
parallel, but the Pi/Tea condition order remains sequential within each lane.
Each lane has its own worktree, evidence, dependencies, tool npm cache, HOME,
and TMPDIR; only synchronized setup-cache consumption is shared. Set
`PI_SHOOTOUT_PARALLEL_REPEATS=1` when a serialized lane schedule is required.

The medium Express case uses a checked-in production-only dependency lock;
the harder `express-4205-hard` diagnostic case has the same pinned-validator
arrangement. The medium case defaults to a 900-second attempt wall clock and
the hard case to 1,800 seconds. For calibration only, set
`PI_SHOOTOUT_TIMEOUT_SECONDS=0` to remove the runner's outer wall clock; this
is not a scored result because a model-generated shell command can wait
indefinitely. Tea's shootout OpenRouter request timeout follows the finite
budget, while the runner reserves a 15-second finalization grace for
`tea-static` to retain a terminal result and request witness before forced
cleanup. Zero-budget
diagnostics use a 24-hour transport guard. Cache
preparation is explicit, while attempts use a per-attempt
`npm ci --offline` tree outside the Git workspace. This removes ambient npm
resolution from both the model condition and the fast validator.

---

You are a coding agent working on `tea`. Your single objective is to make the
static Tea coding harness (`tea-static`) match or beat the Pi static harness on
token efficiency while retaining correctness and the closed coding-tool
contract.

`tea-jit` is out of scope. Do not modify it, optimize it, run it, or use it as
a comparison condition. The target is `tea-static` versus `pi-static` only.

## Non-negotiable goal

For the same task, model, provider, thinking level, workspace, and tool
capabilities:

- `tea-static` must pass the same external validator as `pi-static`.
- The primary normalized metric, uncached generation (`uncached input +
  output`), must be less than or equal to `pi-static`; the stretch goal is
  strictly lower. Prompt total and provider cost are tracked separately so a
  lower number of requests cannot hide a regression in billable generation.
- A result is not a win if it changes the task, validator, model, provider,
  thinking level, output ceiling, tool permissions, or workspace baseline.
- Do not trade away correctness, patch quality, deterministic boundaries, or
  the `read`, `bash`, `edit`, `find` capability surface to save tokens.

If the counters are not semantically comparable, repair the accounting and
tests first. Never claim an efficiency improvement from an accounting
artifact.

## Evidence to start from

The pinned run below is the baseline for this investigation:

- Run: `20260829T223033Z-4e98a8d2748b`
- Task: `express-3936-medium`
- Model/provider: `deepseek/deepseek-v4-flash-0731` / OpenRouter
- Thinking: high; output ceiling: unlimited
- Both validators passed; wall time was nearly equal (Pi `115322 ms`, Tea
  `118195 ms`).

The first report appeared to show a dramatic Pi advantage:

| Metric | Pi static | Tea static |
| --- | ---: | ---: |
| Reported input | 19,194 | 282,674 |
| Output | 11,585 | 15,142 |
| Reported generation (`input + output`) | 30,779 | 297,816 |
| Cache read | 512,000 | 260,352 |
| Reported turns | 1 | 14 |
| Reported tool calls | 38 | 22 |

Those generation figures are not apples-to-apples. Pi's OpenAI-compatible
usage parser computes uncached input as:

```text
input = prompt_tokens - cached_tokens - cache_write_tokens
```

Tea's OpenRouter parser currently stores `prompt_tokens` as `input_tokens` and
stores `cached_tokens` separately. Thus Tea's reported generation includes
cache-hit tokens that Pi removes from its generation count.

Using the captured usage with one consistent interpretation:

| Normalized quantity | Pi static | Tea static |
| --- | ---: | ---: |
| Uncached input | 19,194 | 22,322 (`282,674 - 260,352`) |
| Uncached generation | 30,779 | 37,464 |
| Prompt total (cached tokens included once) | 531,194 | 282,674 |
| All prompt and output tokens (cached tokens included once) | 542,779 | 297,816 |

The real baseline depends on which efficiency quantity is intended. Tea is
about 21.7% above Pi on uncached generation, while its raw prompt-plus-output
total is lower, consistent with its fewer provider requests. These are different
questions from provider billing: cached tokens are a subset of `prompt_tokens`,
not an amount to add a second time. The 9.7x generation comparison in the
first report was therefore invalid, and any future report must show both raw
prompt totals and uncached/billable input explicitly.

Use these names and formulas in the revised result schema (for providers whose
`prompt_tokens` includes cached tokens, as OpenRouter does):

```text
prompt_total       = prompt_tokens
uncached_input     = max(prompt_total - cache_read - cache_write, 0)
uncached_generation = uncached_input + output
all_tokens         = prompt_total + output
```

The shootout result now records these normalized usage fields directly:
`input` is uncached input, `prompt_total` is the raw prompt total,
`generation` is uncached input plus output, and `all_tokens` is prompt total
plus output. Shared counters follow Pi's meanings: `turns` is user messages
and `tool_calls` is assistant-emitted tool-call blocks. Tea additionally emits
`model_turns` and durable `provider_requests`; `retries` is the number of
durable step attempts carrying an explicit retry reason. A null provider
counter remains an honest "not exposed" value for Pi.
The adapter and contract own this normalization; an efficiency agent should
consume these fields directly and must not recompute generation from raw cache
components or mix trace-event counts with durable counters.

If a provider reports a different convention, retain its raw fields and make
the conversion explicit in the adapter rather than guessing in the report.

The retained successful static-only artifacts are under
`/private/tmp/tea-pi-shootout/runs/20260829T232452Z-d6b21971c820/` while that
run directory exists. A later final-label rerun had a model-side validator
failure and is retained as a non-comparable attempt. The durable report is
`evals/pi_shootout/reports` output from the run; treat the raw provider usage
and adapter source as authoritative when the report fields disagree.

The latest passing pair reports Tea at `35,154` uncached generation tokens
versus Pi at `32,376` (`+2,778`, or `+8.6%`). Tea used 28 model turns and 34
tool calls versus Pi's 24 and 32. The comparison command records the observed
Tea work categories and marks the pair non-comparable until the differing
model-facing prompt/tool surfaces are deliberately controlled.

## Apples-to-apples audit (2026-08-30)

The Pi reference was checked against `/Users/josh/d/pi` and the public npm
registry. Both `packages/ai/package.json` and `packages/coding-agent/package.json`
in that checkout are `0.84.4`, and `npm view` reports `0.84.4` for both packages.
The shootout SDK is now pinned to those exact versions in
`evals/pi_shootout/sdk/package.json` and its lockfile; adapter telemetry records
`npm:@earendil-works/pi-coding-agent@0.84.4`.

The adapter previously created a second Pi session from plain `AgentTool`
wrappers. Those wrappers dropped Pi's prompt metadata, so the captured Pi
system prompt said `Available tools: (none)` even though `read`, `bash`, `edit`,
and `find` were active. It now registers the SDK's public
`create*ToolDefinition` values as custom tools, retaining Pi's native schemas,
descriptions, prompt snippets, and execution behavior while keeping the
shootout's isolated bash environment. This is the first valid Pi surface to
use for new measurements; older artifacts with the `(none)` prompt are
historical and must not be pooled with it.

The OpenRouter wire audit also found a real context mismatch in Tea. The live
static condition now sets `temperature: 0` and seed `20260829` on both adapters,
while reasoning runs now replay the empty
`reasoning_content` field required by Pi's DeepSeek/OpenRouter compatibility
profile. Empty assistant `tool_calls` arrays are omitted to match Pi's message
converter. These changes are covered by provider tests.

The surfaces are still intentionally not byte-identical: Tea's safety-oriented
tool schemas and concise builtin prompt differ from Pi's builtins. The analyzer
reports those native differences as measured harness results rather than hiding
them or treating them as parity gates. The shared contract is the exact ordered
capability set (`read`, `bash`, `edit`, `find`), task/workspace/validator,
model/provider, reasoning level, output ceiling, shell authority, and timeout.
A future efficiency result must report those presentation differences as a
causal uncertainty, while direct wire-policy, route, or request-shape failures
block a strict conclusion.

## Reusable comparison procedure

The comparison work is checked in as a provider-free command. After any
static-only run, invoke:

```sh
python3 -m evals.pi_shootout compare \
  --run-dir /tmp/tea-pi-shootout/runs/<run-id>
```

This writes `reports/comparison.json` (machine-readable) and
`reports/comparison.md` (reviewable). It validates the shared task, baseline,
validator, model, provider, thinking level, output ceiling, timeout, and active
tool identity before comparing attempts. It consumes adapter-normalized `usage`
and `counts` directly and reports Tea-minus-Pi deltas plus median/min/max
deltas across repeated pairs. Provider-request `null` remains unknown; it is
never inferred from model turns.

For turn attribution, the analyzer reads Tea's retained durable session when
available. Each turn includes stop reason, provider usage, tool names, tool
result errors, and a category (`inspection`, `edit`, `validation`,
`upstream_or_dependency`, `repository_state`, or `shell`) with argument
digests rather than raw commands. Pi's trace is marked partial if it lacks
those fields. The report has separate **Evidence**, **Hypotheses**, and
**Unknowns** sections, so observed extra work is not mistaken for a causal
runtime effect. Do not hand-reconstruct token totals or declare a regression
from one attempt.

## Friction points found

Treat these as evidence-backed leads, not permission to guess at a fix.

1. **Resolved: usage accounting is now symmetric.** Both reporters publish
   raw prompt total, uncached input, output, uncached generation, all-token
   total, cache reads, and cache writes under the formulas above. Preserve
   provider-reported cost separately; the captured Tea cost was
   `provider-reported` while Pi's was a `catalog-estimate`, so those dollar
   totals cannot be ranked.

2. **Tea replays a growing context across many provider requests.** In the
   latest passing static pair, the Tea session recorded 28 settled requests;
   raw prompt usage grew from `1,844` to `14,367` tokens per request, with
   cache reads growing from zero to `13,568`. This is the largest likely
   runtime cost after accounting is normalized. Inspect context projection,
   request construction, cache-friendly ordering, and stop/continuation
   behavior before changing the model prompt.

3. **Resolved: telemetry meanings now line up.** `turns` means durable user
   messages, `model_turns` means model-loop turns, `tool_calls` means assistant
   tool-call blocks, and `retries` means attempts with an explicit retry reason.
   Tea exposes exact durable provider requests; Pi leaves that field null
   because its SDK does not expose wire-request count. The reusable `compare`
   command consumes these fields without inference.

4. **The tool surfaces are closed but not identical.** Both runs expose the
   same ordered names (`read`, `bash`, `edit`, `find`), but their system-prompt
   and tool-surface hashes differ. Tea's system prompt is 911 bytes versus
   Pi's 1,746 bytes, and Tea's schemas/descriptions include stricter bounds and
   a multi-file transactional edit shape. Compare semantic behavior and
   tokenized request bytes; do not add tools or weaken capability boundaries
   merely to make hashes match.

5. **The model did extra exploratory work under Tea.** In the latest passing
   attempt, the durable session records five upstream/dependency probes, six
   validation turns, and twelve other shell turns; Pi's adapter trace does not
   retain equivalent command arguments. The extra scope is observable, but it
   may be model behavior caused by prompt/tool semantics rather than a kernel
   defect. Use `python3 -m evals.pi_shootout compare` across repeated runs
   before hard-coding a policy.

6. **Reasoning and provider fields are not symmetric.** Tea reported `9,317`
   reasoning tokens; Pi reported no reasoning value. Treat Pi's null as
   “unavailable,” not zero, until both adapters capture the same provider
   fields.

## Required work sequence

The accounting and telemetry normalization in steps 1–3 is complete in the
current tree. Verify it with `make pi-shootout-check` and the `compare` command;
do not reopen those fixes unless a regression test fails.

1. Read the existing shootout contract, adapters, OpenRouter usage parser,
   report schema, and focused tests. Identify the exact request boundary and
   all aggregation points before editing.
2. Add the smallest failing tests for usage normalization and provider-request
   / tool-call accounting. Keep raw provider fields available for audit.
3. Fix the accounting contract so Pi and Tea expose the same normalized
   quantities. Update reports and tests together; do not silently reinterpret
   an existing field.
4. Capture per-request prompt totals, cache reads/writes, output, stop reason,
   request count, and context-prefix/cache evidence for `tea-static`. Use that
   evidence to remove avoidable context replay and redundant continuation
   requests while preserving durable session semantics.
5. Make the Tea static interaction protocol and prompt/tool presentation as
   efficient as Pi's without changing the closed tool set or granting ambient
   network, web, or subagent capabilities. Batch independent tool work where
   the contract permits it, avoid repeated probes, and stop promptly once the
   validator-relevant fix is verified.
6. Add or use a static-only shootout command. It must run `tea-static` and
   `pi-static` and must not instantiate or execute `tea-jit`. The repository
   command is `make pi-shootout-static` (or `python3 -m evals.pi_shootout run
   --static-only`).
7. Run provider-free focused checks, then repeated static-only live attempts
   with the pinned model/provider. Use at least three attempts and report
   median and worst-case normalized totals; a single lucky run is not proof.

## Acceptance criteria

The work is complete only when all of the following are true:

- Every repeated `tea-static` attempt passes the shared validator.
- The normalized metric is documented and identical across adapters:
  uncached input, output, uncached generation, prompt total, all-token total,
  cache reads/writes, provider requests, model turns, and tool calls.
- Tea's median and worst-case normalized uncached generation are no greater
  than Pi's under the same run set. Also report prompt total (where cached
  tokens are included exactly once), all prompt-plus-output tokens, and
  provider cost when both cost sources are comparable; none may be silently
  omitted or double-counted.
- No comparison relies on Pi's missing reasoning value or on unlike cost kinds.
- The active tools remain exactly `read`, `bash`, `edit`, `find` in that order;
  `tea-jit` remains untouched and unrun.
- Correctness, session durability, provider-agnostic core behavior, and
  existing focused tests remain intact.
- The final handoff names the measured bottleneck, lists the tests and
  static-only runs performed, links the evidence, and calls out any remaining
  uncertainty instead of declaring victory from a proxy.

Do not run formatters, linters, pre-commit hooks, or destructive cleanup as
part of this mission. Keep changes narrow, reversible, and reviewable.
