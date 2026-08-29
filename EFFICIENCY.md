# Tea static token-efficiency mission

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

If a provider reports a different convention, retain its raw fields and make
the conversion explicit in the adapter rather than guessing in the report.

The retained artifacts are under
`/private/tmp/tea-pi-shootout/runs/20260829T223033Z-4e98a8d2748b/` while that
run directory exists. The durable report is
`evals/pi_shootout/reports` output from the run; treat the raw provider usage
and adapter source as authoritative when the report fields disagree.

## Friction points found

Treat these as evidence-backed leads, not permission to guess at a fix.

1. **Usage accounting is asymmetric.** Pi's reporter subtracts cache reads
   before computing `input` and `generation`; Tea currently does not. Add
   explicit, tested fields for raw prompt tokens, uncached input, cache reads,
   cache writes, output, and all-token totals. Make the comparison use one
   definition on both adapters. Preserve provider-reported cost separately;
   the captured Tea cost was `provider-reported` while Pi's was a
   `catalog-estimate`, so those dollar totals cannot be ranked.

2. **Tea replays a growing context across many provider requests.** The Tea
   session recorded 24 settled provider requests. Raw prompt usage grew from
   `1,844` to `18,654` tokens per request, with cache reads growing from zero
   to `17,920`. This is the largest likely runtime cost after accounting is
   normalized. Inspect context projection, request construction, cache-friendly
   ordering, and stop/continuation behavior before changing the model prompt.

3. **Telemetry counts do not line up.** Tea's result reports 14 turns and 22
   tool executions, while its durable session contains 24 provider requests
   and 31 `tool_started` records. Pi's result reports one user message and 38
   tool calls, while its trace contains 38 model turn boundaries. Establish a
   single meaning for user turns, model requests, tool calls, and retries, and
   expose provider-request counts. Add regression tests so a report cannot
   silently compare unlike counters.

4. **The tool surfaces are closed but not identical.** Both runs expose the
   same ordered names (`read`, `bash`, `edit`, `find`), but their system-prompt
   and tool-surface hashes differ. Tea's system prompt is 911 bytes versus
   Pi's 1,746 bytes, and Tea's schemas/descriptions include stricter bounds and
   a multi-file transactional edit shape. Compare semantic behavior and
   tokenized request bytes; do not add tools or weaken capability boundaries
   merely to make hashes match.

5. **The model did extra exploratory work under Tea.** In this attempt Tea
   issued repeated dependency/source-history checks, several test-environment
   probes, and added a `History.md` changelog entry; Pi changed only the source
   and test files. The extra scope is observable, but it may be model behavior
   caused by prompt/tool semantics rather than a kernel defect. Measure it
   across repeated runs before hard-coding a policy.

6. **Reasoning and provider fields are not symmetric.** Tea reported `9,317`
   reasoning tokens; Pi reported no reasoning value. Treat Pi's null as
   “unavailable,” not zero, until both adapters capture the same provider
   fields.

## Required work sequence

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
