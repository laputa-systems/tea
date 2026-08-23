Yes. The shootout should execute **three conditions** from fresh copies of the same pinned repository state:

1. `pi-static`
2. `tea-static`
3. `tea-jit`

It should then emit two first-class reports:

* `static.md`: Pi static versus Tea static.
* `evolution.md`: Tea static versus Tea JIT, with Pi static versus Tea JIT as an external reference.

Tea already has an appropriate default task: `express-3936-medium`, pinned to exact `pi-bench` and Express commits, with a direct fast validator whose baseline is known to fail and whose fix commit is known to pass.   The Pi SDK exposes the exact machinery required for a controlled adapter—isolated services, in-memory settings/session management, event subscription, system-prompt access, tool factories, session statistics, and cleanup—and Pi’s own eval harness demonstrates the intended pattern.   The current published SDK manifest is `0.84.2` and requires Node `>=22.19.0`, so this can be pinned exactly and run without Pi CLI headless mode.

# Coding-agent prompt: scaffold Tea Harness JIT v0 and `make pi-shootout`

Implement the first small, controlled proof of Tea’s harness-JIT thesis.

Assume all previously specified architectural cleanup and `tea-tui` work has been completed. Adapt paths and type names to the final repository, but preserve the design below.

The resulting command must be:

```sh
PI_SHOOTOUT_MODEL=<openrouter-model-id> \
PI_SHOOTOUT_ENV_FILE=.env \
make pi-shootout
```

It must run one pinned `pi-bench` coding task under three conditions:

```text
pi-static
tea-static
tea-jit
```

and emit two separate reports:

```text
static.md
evolution.md
```

This is a proof-of-concept evaluation and observability scaffold, not a claim of broad benchmark superiority.

---

## 1. Central experiment

The experiment tests:

> Given the same task, base repository, model, provider, reasoning level, output limit, shell authority, timeout, and validator, how do Pi static, Tea static, and Tea with one bounded task-local harness adaptation compare?

The three conditions are:

### `pi-static`

* Pinned Pi SDK.
* Pi default coding harness.
* Active coding tools:

  * `read`
  * `bash`
  * `edit`
  * `write`
* No discovered extensions.
* No discovered skills.
* No discovered global prompts.
* No subagents.
* No dedicated web-search or browser tool.
* Shell commands may invoke `curl`.
* In-memory session/settings where practical.
* Empty isolated Pi agent directory.

### `tea-static`

* Tea’s pinned default coding profile.
* Tea’s normal durable runtime path.
* Self-extension disabled.
* Same model and task authority as Pi.
* Active coding tools:

  * `read`
  * `bash`
  * `edit`
  * `write`
* No task-local plugins.
* No subagents.
* No dedicated web-search or browser tool.
* Shell commands may invoke `curl`.

### `tea-jit`

* Exactly the same Tea base profile and durable runtime as `tea-static`.
* Task-local harness authoring enabled.
* The same model performs both coding and harness adaptation.
* No separate optimizer model.
* No web-research tool.
* No subagents.
* Shell commands may invoke `curl`.
* At most one candidate may be staged.
* At most one candidate may be activated.
* At most one epoch rollover may occur.
* No new ambient capability may be introduced.

The JIT is allowed to choose `NoChange`.

Do not force candidate creation merely to demonstrate the mechanism.

---

# 2. Scope

Implement only enough infrastructure to:

1. run the three conditions fairly;
2. normalize their results;
3. capture curated observability;
4. validate one task;
5. generate the two reports;
6. prove the Tea JIT lifecycle with deterministic tests.

Do not implement:

* an autonomous multi-iteration research loop;
* global harness promotion;
* Rust self-modification;
* automatic Rust patches;
* Terminal-Bench;
* SWE-bench;
* a broad benchmark downloader;
* a web-search API;
* browser automation;
* subagents;
* multiple models;
* provider comparison;
* automatic model selection;
* long-term cross-task memory;
* dynamic native plugins;
* arbitrary capability expansion.

This work should make a future outer evolution loop possible without implementing that loop now.

---

# 3. Read only relevant repository areas

Before changing code, inspect:

```text
AGENTS.md
Makefile
Cargo.toml

evals/README.md
evals/controller.py
evals/test_controller.py

evals/quality/README.md
evals/quality/__main__.py
evals/quality/coding_cases.py
evals/quality/coding_runner.py
evals/quality/test_coding_cases.py

evals/quality/cases/coding/express-3936-medium/manifest.json
evals/quality/cases/coding/express-3936-medium/fast-validator.js
```

Inspect the final post-refactor locations of:

```text
Tea default coding profile
Tea runtime/session construction
Tea provider evaluation binary
Tea harness snapshots and candidate lifecycle
Tea self-extension mode
Tea runtime events
Tea model accounting
Tea trace/evidence artifacts
```

For Pi SDK behavior, use the pinned npm package API. The implementation pattern should follow these upstream concepts:

```text
createAgentSessionServices
createAgentSessionFromServices
ModelRuntime
SettingsManager.inMemory
SessionManager.inMemory
createCodingTools
AgentSession.subscribe
AgentSession.getSessionStats
AgentSession.systemPrompt
AgentSession.messages
AgentSession.dispose
```

Do not invoke:

```text
pi CLI
pi --print
pi RPC mode
pi headless CLI
```

The Pi condition must be a direct SDK embedding.

---

# 4. Do not add a new Rust crate

Use the existing final crate architecture.

The provider-integrated Tea evaluation adapter should live as a binary target in the package that already owns concrete providers—presumably `tea-providers` after the planned refactor.

Prefer extending the existing evaluation binary if one remains.

Do not create:

```text
tea-evals
tea-bench
tea-shootout
```

as new Rust library crates.

The Python and TypeScript evaluation code belongs under `evals/`.

---

# 5. Proposed directory layout

Create:

```text
evals/pi_shootout/
├── __init__.py
├── __main__.py
├── contract.py
├── runner.py
├── report.py
├── README.md
├── test_contract.py
├── test_report.py
└── sdk/
    ├── package.json
    ├── package-lock.json
    ├── tsconfig.json
    ├── src/
    │   ├── canonical.ts
    │   ├── reporter.ts
    │   └── pi-adapter.ts
    └── test/
        └── reporter.test.ts
```

Names may vary slightly, but keep one obvious Python orchestrator and one isolated TypeScript Pi SDK adapter.

Do not introduce a JavaScript package at repository root.

Add `node_modules/` under this package to `.gitignore`.

---

# 6. Pin the Pi SDK exactly

In:

```text
evals/pi_shootout/sdk/package.json
```

pin exact versions with no caret or tilde.

Use:

```json
{
  "private": true,
  "type": "module",
  "engines": {
    "node": ">=22.19.0"
  },
  "dependencies": {
    "@earendil-works/pi-ai": "0.84.2",
    "@earendil-works/pi-coding-agent": "0.84.2"
  },
  "devDependencies": {
    "@types/node": "22.19.19",
    "typescript": "5.9.3"
  }
}
```

Check in the generated `package-lock.json`.

Do not use:

```text
latest
^0.84.2
~0.84.2
npx with an unpinned package
a global Pi installation
```

Use Node’s native TypeScript type stripping if the pinned Node floor supports the script syntax cleanly.

Keep the TypeScript to erasable syntax:

* no enums requiring transformation;
* no namespaces;
* no parameter properties;
* no decorators;
* no path aliases;
* no build step required to run.

Use `tsc --noEmit` as a static check.

If Node’s direct `.ts` execution proves incompatible with the exact supported Node floor, add one exact pinned `tsx` dependency. Do not use a floating `npx tsx`.

---

# 7. Canonical normalized adapter result

Replace or extend the existing evaluation result contract with one canonical contract emitted by both the Pi TypeScript adapter and the Tea Rust adapter:

```text
tea-coding-eval-result/v2
```

Update all repository-owned readers and tests together. No backward compatibility is required for the old eval result schema.

The normalized result must have this semantic shape:

```json
{
  "schema_version": "tea-coding-eval-result/v2",
  "attempt_id": "…",
  "baseline_id": "pi-static | tea-static | tea-jit",

  "terminal": {
    "status": "completed | failed | cancelled | aborted",
    "code": null
  },

  "final_text": "…",

  "runtime": {
    "implementation": "pi-sdk | tea",
    "version": "…",
    "revision": "…",
    "dirty": false,
    "dirty_digest": null
  },

  "model": {
    "provider": "openrouter",
    "requested_model": "…",
    "returned_model": null,
    "returned_provider": null,
    "thinking_level": "off",
    "max_output_tokens": 4096,
    "sampling": {
      "temperature": null,
      "seed": null,
      "source": "provider-default"
    }
  },

  "surface": {
    "system_prompt_bytes": 0,
    "system_prompt_sha256": "…",
    "workspace_normalized_system_prompt_sha256": "…",
    "tool_surface_sha256": "…",
    "active_tools": [],
    "research_tools": [],
    "subagents": false,
    "shell_curl_available": true,
    "shell_environment_sha256": "…"
  },

  "timings": {
    "agent_ms": 0,
    "candidate_validation_ms": 0,
    "rollover_ms": 0
  },

  "counts": {
    "turns": 0,
    "provider_requests": null,
    "tool_calls": 0,
    "retries": 0,
    "compactions": 0
  },

  "usage": {
    "input": 0,
    "output": 0,
    "generation": 0,
    "reasoning": null,
    "cache_read": 0,
    "cache_write": 0
  },

  "cost": {
    "kind": "provider-reported | catalog-estimate | unavailable",
    "currency": "USD",
    "total": null
  },

  "harness": {
    "mode": "static | jit",
    "base_snapshot_id": null,
    "initial_snapshot_id": null,
    "final_snapshot_id": null,
    "decision": "not-applicable | no-change | rejected | activated",
    "candidate_count": 0,
    "candidate_id": null,
    "changed_surfaces": [],
    "candidate_source_bytes": 0,
    "hypothesis": null
  },

  "trace": []
}
```

The exact JSON formatting may differ, but do not omit the semantic fields.

## Token semantics

The primary token count is:

```text
generation = input + output
```

Do not define the primary total as:

```text
input + output + cache_read + cache_write
```

Report these independently:

```text
input
output
generation
reasoning
cache_read
cache_write
```

Unknown fields must remain `null` where distinguishable from zero.

Do not manufacture zero when the adapter truly cannot determine a value.

## Cost semantics

The Pi SDK may expose catalog-derived estimated cost while Tea/OpenRouter may expose provider-reported cost.

Record the source honestly:

```text
catalog-estimate
provider-reported
unavailable
```

Do not compute a direct cost ratio unless both compared values have equivalent `kind` semantics.

---

# 8. System-prompt and tool-surface comparison

The first static report must answer more than “did the task pass?”

It must show whether Tea is currently presenting the same model-facing harness as Pi.

For every condition, capture:

```text
raw system prompt byte length
raw system prompt SHA-256
workspace-normalized system prompt SHA-256
active tool order
canonical tool descriptions
canonical tool parameter schemas
tool execution modes where exposed
canonical tool-surface SHA-256
```

For fingerprinting only, replace the exact attempt workspace path with:

```text
{WORKSPACE}
```

Do not alter the prompt actually sent to the model.

Save exact private evidence under each attempt directory:

```text
surface/system-prompt.txt
surface/tool-surface.json
```

The Markdown report should show hashes and equality/difference status, not dump the whole prompt.

If Pi and Tea surfaces differ, generate:

```text
surface/system-prompt.diff
surface/tool-surface.diff
```

Do not fail the shootout merely because the surfaces differ. The point of the first report is to establish where Tea currently stands.

---

# 9. Pi SDK adapter

Implement:

```text
evals/pi_shootout/sdk/src/pi-adapter.ts
```

It must:

1. parse explicit argv;
2. validate the task and capability manifests;
3. create an isolated Pi runtime;
4. select the exact requested model;
5. construct the exact coding tools;
6. subscribe to events before prompting;
7. run the task;
8. collect usage and timings;
9. export the model-facing surfaces;
10. write one valid normalized result even when the model run fails;
11. dispose all Pi state.

## Required arguments

Support approximately:

```text
--task-json
--workspace
--capabilities-json
--result-json
--evidence-dir
--attempt-id
--baseline-id
--provider
--model
--thinking-level
--max-output-tokens
```

Reject:

* missing required arguments;
* duplicate arguments;
* unknown arguments;
* mismatched task/capability manifests;
* unsupported providers;
* unsupported active tool lists.

## Isolation

Use:

```text
empty attempt-local agentDir
SettingsManager.inMemory()
SessionManager.inMemory(workspace)
ModelRuntime with network catalog refresh disabled
explicit model
explicit thinking level
```

The provider inference call may use the network.

The model catalog itself should not be refreshed from the network during the attempt.

Assert:

```text
no discovered extensions
no discovered skills
no discovered prompt templates
no discovered custom providers
no task-provided subagent tools
active model-facing tools are exactly read/bash/edit/write
```

If a repository resource is discovered unexpectedly, fail the adapter as an infrastructure/configuration error rather than silently including it.

## Events

Normalize event receipt into a bounded trace including at least:

```text
agent_start
turn_start
message_start
message_update
message_end
tool_execution_start
tool_execution_update
tool_execution_end
compaction_start
compaction_end
retry start/end where available
agent_end
```

Do not retain chain-of-thought or reasoning text.

Do not retain provider credentials.

Do not retain arbitrary environment values.

For potentially large tool updates, retain:

```text
event type
sequence
tool call ID
tool name
success/error state
bounded byte count
content digest
optional bounded preview
```

Do not copy multi-megabyte command output into the normalized result.

## Usage

Use Pi’s session statistics and message usage to populate:

```text
input
output
cache_read
cache_write
cost estimate
tool calls
turns
```

Capture the returned model/provider metadata from assistant messages where available.

The Pi SDK adapter’s `cost.kind` should normally be:

```text
catalog-estimate
```

unless the SDK provides direct, clearly provider-reported cost evidence.

## Reporter module

Implement the normalization logic in:

```text
reporter.ts
```

rather than mixing it throughout the SDK bootstrapping code.

The reporter should expose focused operations such as:

```ts
reporter.observe(event)
reporter.captureSurface(session, workspace)
reporter.finish(session, terminal)
reporter.write(path)
```

The exact API is flexible.

Add synthetic TypeScript tests for:

* successful run;
* failed run;
* cancelled run;
* one tool call;
* tool error;
* multiple turns;
* cache usage;
* workspace prompt normalization;
* tool-surface canonicalization;
* large update truncation;
* no secret/environment leakage.

---

# 10. Equal shell authority

Both Pi and Tea must receive the same shell authority.

The agent must be able to run:

```sh
curl ...
```

through the existing `bash` tool.

Do not add a first-class `curl` tool.

Do not add a `web_search` tool.

Do not add a browser.

Do not block shell network egress inside the evaluation harness.

## Sanitized shell environment

Do not expose provider credentials to child shell commands.

Construct an explicit attempt-local environment for both Pi and Tea:

```text
PATH
HOME=<attempt-local empty home>
TMPDIR=<attempt-local temp>
LANG=C
LC_ALL=C
npm_config_cache=<explicit cache when available>
NPM_CONFIG_AUDIT=false
NPM_CONFIG_FUND=false
```

Optionally pass non-secret system certificate-path variables if necessary for `curl`.

Do not pass:

```text
OPENROUTER_API_KEY
COMMANDCODE_API_KEY
AWS_*
GITHUB_TOKEN
PI_* session metadata
parent HOME
parent credential paths
proxy URLs containing credentials
```

For Pi, use the exported coding-tool factories and `BashToolOptions.spawnHook` or the narrowest supported SDK customization to supply this environment while preserving Pi’s normal model-facing tool definition.

Set:

```text
exposeSessionEnvironment = false
```

For Tea, configure the standard coding operations with the same explicit environment.

Before running the model, verify from the exact child environment that:

```sh
command -v curl
```

succeeds.

Record:

```text
shell_curl_available
shell_environment_sha256
```

The environment hash must be computed from variable names and non-secret values after replacing attempt paths with placeholders.

---

# 11. Fair model configuration

The Make target must support exactly one v0 provider:

```text
openrouter
```

Do not implement a provider matrix.

Require an explicit model ID.

Use the same:

```text
provider
model ID
thinking level
maximum output tokens
attempt timeout
```

for all three conditions.

Default:

```text
thinking level = off
maximum output tokens = 4096
```

Do not claim temperature or seed equality unless both adapters actually set and verify those fields.

If neither adapter sets sampling fields and both rely on provider defaults, record:

```json
{
  "temperature": null,
  "seed": null,
  "source": "provider-default"
}
```

Do not write a fictitious deterministic sampling configuration into the report.

If the returned model or provider revision differs between conditions, mark the report with a comparability warning.

---

# 12. Tea adapter

Extend the existing provider-backed Tea evaluation binary.

Do not create two separate binaries for static and JIT.

Add:

```text
--harness-mode static
--harness-mode jit
```

The adapter must emit the same `tea-coding-eval-result/v2` contract as the Pi adapter.

## Use the durable runtime for both Tea conditions

Do not compare:

```text
tea-static using sessionless Agent
tea-jit using durable SessionRuntime
```

Both must use the same durable runtime path.

The only intentional differences should be:

```text
tea-static:
    self-extension off
    no JIT addendum
    no harness-authoring tool

tea-jit:
    self-extension authoring enabled
    bounded JIT addendum
    harness-authoring tool exposed
    one candidate/rollover budget
```

This isolates the effect of enabling harness adaptation.

Use an attempt-local durable session and artifact root under the evidence directory.

Do not write to the user’s normal Tea home.

## Tea static

Configure:

```text
pinned default coding profile
read/bash/edit/write
OpenRouter provider
same model
same thinking level
same output ceiling
self-extension off
no preloaded plugins
```

## Tea JIT

Configure:

```text
same pinned default coding profile
same coding tools
same provider/model
same runtime
self-extension author mode
maximum candidates = 1
maximum activations = 1
maximum rollovers = 1
maximum candidate source = 16 KiB
no capability expansion
```

Expose only the existing stable harness-authoring and artifact-reading primitives required by Tea’s current self-extension design.

Do not invent a second plugin API for this benchmark.

---

# 13. Tea JIT instructions

Append a small, explicit JIT policy section to the Tea JIT system prompt.

Use language equivalent to:

```text
Task-local harness adaptation is available but optional.

First inspect the task and repository using the normal coding tools. Use
NoChange unless you have concrete evidence that one bounded harness change is
likely to improve this task.

You may stage at most one task-local harness candidate. It may alter only
currently supported prompt, tool-presentation, hook, context, memory-selection,
failure-policy, or compaction-policy surfaces. It cannot grant new authority,
change the provider or model, access hidden validators, use subagents, or add a
web-research tool.

A candidate must include:
- observed task or failure evidence;
- a root-cause hypothesis;
- the expected effect;
- regression risk;
- the harness surfaces changed.

If the candidate activates, continue solving the same task under the new
immutable harness revision. All adaptation time and model usage count toward
the task result.
```

Do not mention the known fix commit.

Do not expose the hidden validator.

Do not include task-specific hints beyond the actual user prompt.

---

# 14. JIT lifecycle measurement

Subscribe to Tea runtime events and collect:

```text
base snapshot ID
initial JIT-enabled snapshot ID
candidate staged
candidate rejected
candidate activated
candidate ID
changed paths
changed surfaces
candidate source bytes
validation duration
rollover started
rollover completed
final snapshot ID
epoch count
usage by epoch
```

Classify the final JIT decision as exactly one of:

```text
no-change
rejected
activated
```

`no-change` means no candidate was staged.

A rejected candidate must remain visible as retained lineage but must not prevent the model from continuing with the base harness unless the existing contract requires termination.

Group provider usage by epoch where the existing provenance allows it.

Report:

```text
pre-activation input/output/cache usage
post-activation input/output/cache usage
total usage
```

Do not label all pre-activation tokens as pure “adaptation tokens.” They also contain ordinary task analysis.

Call them:

```text
pre_activation_generation_tokens
post_activation_generation_tokens
```

---

# 15. Deterministic JIT proof

The live `pi-bench` task may legitimately choose `NoChange`.

Therefore add a provider-free deterministic test that proves the mechanism itself.

Use a scripted/fake provider that:

1. inspects the initial harness;
2. invokes the harness-authoring tool;
3. stages one valid bounded Luau candidate;
4. causes activation;
5. crosses one safe epoch boundary;
6. observes the changed prompt or hook in the next epoch;
7. finishes the task.

Assert:

```text
one candidate
one activation
one rollover
base snapshot differs from final snapshot
active epoch never mutates in place
candidate evidence retained
usage attributed to both epochs
operation settles normally
```

Also test:

```text
NoChange
invalid candidate
capability expansion rejection
second candidate rejection
second rollover rejection
oversized source rejection
```

These tests must use no real provider.

---

# 16. Oracle isolation

The model must not be able to inspect the known-correct fix commit from the local Git object database.

The current cache contains both baseline and known-correct commits for validator auditing. Do not clone every cache ref into the agent’s attempt repository.

Change the attempt materialization path so each model workspace contains only the baseline commit and its required tree/history.

A safe approach is:

```text
1. create a fresh empty repository;
2. fetch only the exact baseline commit from the local bare cache with depth 1;
3. detach at FETCH_HEAD;
4. remove the remote;
5. expire reflogs if needed;
6. verify the fix commit is absent.
```

Before launching an adapter, assert inside the attempt workspace:

```sh
git rev-parse HEAD == baseline commit
git status --porcelain is empty
git remote is empty
git cat-file -e <known-fix-commit> fails
```

Add a regression test for this.

The agent may still use public network access through `curl`. That is intentional and symmetric across conditions.

Do not provide a dedicated search tool or known issue URL.

---

# 17. Shootout orchestrator

Implement the main orchestration in Python, using only the standard library and existing Tea eval helpers.

The orchestrator should reuse:

```text
case loading
cache preparation
safe worktree creation
validator execution
process-group timeout handling
patch collection
```

Do not duplicate the entire existing coding-case implementation.

Run conditions:

```text
pi-static
tea-static
tea-jit
```

Each condition receives:

* a fresh isolated baseline workspace;
* a fresh empty HOME;
* a fresh temp directory;
* the same task prompt;
* the same provider/model settings;
* the same timeout;
* the same validator;
* the same shell environment policy.

Run them sequentially with concurrency `1`.

Randomize condition order from an explicit seed to reduce systematic provider-time ordering effects.

Record the actual order.

Default:

```text
repeats = 1
seed = 20260823
```

Support larger repeat counts without changing code.

For multiple repeats, randomize the order independently and deterministically from the configured seed.

---

# 18. Process and secret boundary

The Python orchestrator must never parse or retain the provider API key.

Continue using the existing pattern where an explicit env file is sourced only in the final child-process boundary.

The visible command record should show something like:

```text
source explicit env file (redacted)
```

Never serialize:

```text
env-file contents
provider key
authorization header
full child environment
```

Both adapter processes should receive the provider credential.

Their child coding-tool shell processes must not.

Kill the complete adapter process group on timeout.

Do not leave provider clients running after a timed-out attempt.

---

# 19. Evidence-pack layout

Each run should create a unique run directory, such as:

```text
<out>/runs/20260823T123456Z-<short-digest>/
```

Write:

```text
run.json
summary.json

attempts/
├── pi-static/
│   ├── adapter-result.json
│   ├── record.json
│   ├── patch.diff
│   ├── validator.json
│   ├── trace.jsonl
│   ├── stdout.log
│   ├── stderr.log
│   └── surface/
│       ├── system-prompt.txt
│       └── tool-surface.json
├── tea-static/
│   └── ...
└── tea-jit/
    ├── ...
    └── harness/
        ├── candidate.json
        ├── candidate-source/
        ├── validation.json
        └── lineage.json

reports/
├── static.md
├── evolution.md
└── surface-diff.md
```

Bound retained stdout/stderr.

Preserve complete patches.

Do not delete evidence just because an attempt passed.

Delete transient worktrees after collecting:

* patch;
* changed-file list;
* patch hash;
* validator output;
* normalized result;
* surface artifacts.

Allow an explicit debug option to keep worktrees.

---

# 20. External timing

The orchestrator must measure separately:

```text
adapter process wall time
validator wall time
total attempt wall time
```

The adapter must measure:

```text
agent wall time
candidate validation time
rollover time
```

Definitions:

### Agent wall time

From immediately before accepted prompt/operation execution until terminal agent settlement.

For Tea JIT, this includes:

* task analysis;
* candidate authoring;
* candidate validation;
* rollover;
* post-rollover work;
* retries;
* compaction.

### Adapter process wall time

From process spawn through result-file publication and cleanup.

### Validator wall time

The hidden validator is separate and must not be counted as agent time.

### Total attempt wall time

```text
adapter process + validator
```

Do not include one-time `npm ci`, Cargo compilation, repository cache population, or report generation in task latency.

---

# 21. Validator

Use the existing:

```text
express-3936-medium
```

case and its `fast-validator.js`.

Do not duplicate the task prompt or validator into a new source of truth.

The validator must remain outside the agent workspace.

Run the exact same validator for all conditions.

The report must record:

```text
task manifest SHA-256
validator script SHA-256
baseline commit
known-correct fix commit
baseline-fails evidence
known-correct-passes evidence
```

The agent must not receive the fix commit or validator contents.

A model terminal status of `completed` is not task success.

Only the external validator determines correctness.

---

# 22. Attempt outcome semantics

Infrastructure failure and benchmark failure are different.

## Infrastructure failure

Examples:

* adapter failed to start;
* result JSON missing;
* result contract invalid;
* wrong task identity;
* wrong model;
* wrong capabilities;
* timeout cleanup failed;
* validator could not execute;
* Pi SDK package mismatch.

These should make `make pi-shootout` fail nonzero.

## Benchmark failure

Examples:

* agent completed but validator failed;
* model stopped with error;
* model made no patch;
* model timed out but adapter produced a valid terminal result;
* JIT candidate was rejected;
* JIT made the result worse.

These are evaluation data.

The command should still generate both reports.

A benchmark failure should not suppress the report.

---

# 23. `static.md`

Generate a report titled approximately:

```text
Pi vs Tea Static — express-3936-medium
```

Include:

## Reproducibility

```text
run ID
task ID
task manifest digest
baseline commit
validator digest
model/provider
thinking level
maximum output tokens
condition order
Pi SDK version and lock digest
Tea source revision and dirty digest
```

## Fairness matrix

Show for both conditions:

```text
same task
same baseline
same validator
same provider
same requested model
returned model/provider
same thinking
same output ceiling
same active coding tools
same shell environment policy
curl available
no web-search tool
no subagents
```

## Harness-surface parity

Show:

```text
system prompt normalized equal?
system prompt bytes
system prompt hashes
active tool order equal?
tool descriptions/schemas equal?
tool-surface hashes
```

Link to `surface-diff.md` when they differ.

## Results table

At minimum:

| Metric                  | Pi static | Tea static |
| ----------------------- | --------: | ---------: |
| Validator pass          |           |            |
| Terminal status         |           |            |
| Agent wall time         |           |            |
| Total attempt wall time |           |            |
| Input tokens            |           |            |
| Output tokens           |           |            |
| Generation tokens       |           |            |
| Reasoning tokens        |           |            |
| Cache read tokens       |           |            |
| Cache write tokens      |           |            |
| Turns                   |           |            |
| Tool calls              |           |            |
| Retries                 |           |            |
| Compactions             |           |            |
| Peak RSS                |           |            |
| Cost                    |           |            |
| Cost kind               |           |            |
| Patch SHA-256           |           |            |

## Observed comparison

Use lexicographic reasoning:

1. correctness;
2. generation tokens;
3. agent wall time;
4. comparable cost;
5. resource footprint.

Do not compute one opaque weighted score.

Do not claim broad superiority from one task or one repeat.

Use phrasing such as:

```text
On this pinned task and attempt…
```

For multiple repeats, report paired outcomes:

```text
Tea pass / Pi fail
Pi pass / Tea fail
both pass
both fail
```

For `both pass`, report token and time ratios.

---

# 24. `evolution.md`

Generate a report titled approximately:

```text
Tea Harness JIT v0 — express-3936-medium
```

Its primary comparison is:

```text
tea-static versus tea-jit
```

It must also show:

```text
pi-static versus tea-jit
```

as the current external product reference.

Include:

## JIT configuration

```text
candidate budget
activation budget
rollover budget
source-byte limit
capability ceiling
no web-research tool
curl available through bash
no subagents
same model performs adaptation
```

## JIT decision

Show:

```text
no-change | rejected | activated
candidate ID
candidate source bytes
changed surfaces
changed paths
hypothesis
expected effect
regression risk
validation time
rollover time
base snapshot
initial JIT snapshot
final snapshot
epoch count
```

## Results table

At minimum:

| Metric                            | Tea static | Tea JIT | Pi static reference |
| --------------------------------- | ---------: | ------: | ------------------: |
| Validator pass                    |            |         |                     |
| Agent wall time                   |            |         |                     |
| Generation tokens                 |            |         |                     |
| Pre-activation generation tokens  |          — |         |                   — |
| Post-activation generation tokens |          — |         |                   — |
| Cache reads/writes                |            |         |                     |
| Turns                             |            |         |                     |
| Tool calls                        |            |         |                     |
| Candidate validation time         |          — |         |                   — |
| Rollover time                     |          — |         |                   — |
| Total attempt wall time           |            |         |                     |

## Classification

Classify the observed Tea delta:

```text
positive flip:
    static failed, JIT passed

regression:
    static passed, JIT failed

efficiency improvement:
    both passed, JIT used fewer generation tokens or less agent wall time

efficiency regression:
    both passed, JIT consumed more

no observed improvement:
    both failed or materially equal

no-change:
    JIT elected not to stage a candidate
```

Do not call a one-attempt flip causal proof.

If a candidate activated, link its hypothesis to the observed outcome and label it:

```text
confirmed
falsified
unresolved
```

using only the task result and explicit evidence.

---

# 25. Make targets

Add:

```text
pi-shootout
pi-shootout-plan
pi-shootout-check
```

Update `.PHONY`.

## Variables

Use approximately:

```make
PI_SHOOTOUT_TASK ?= express-3936-medium
PI_SHOOTOUT_PROVIDER ?= openrouter
PI_SHOOTOUT_MODEL ?=
PI_SHOOTOUT_THINKING ?= off
PI_SHOOTOUT_MAX_OUTPUT_TOKENS ?= 4096
PI_SHOOTOUT_REPEATS ?= 1
PI_SHOOTOUT_SEED ?= 20260823
PI_SHOOTOUT_ENV_FILE ?= .env
PI_SHOOTOUT_CACHE_ROOT ?= /tmp/tea-pi-shootout-cache
PI_SHOOTOUT_WORKSPACE_ROOT ?= /tmp/tea-pi-shootout-workspaces
PI_SHOOTOUT_OUT ?= /tmp/tea-pi-shootout
```

Do not provide a default model.

Fail with a clear message when:

```text
PI_SHOOTOUT_MODEL is empty
env file is missing
node is too old
npm is missing
curl is missing
git is missing
the provider is not openrouter
```

## `pi-shootout-plan`

Must:

* require no provider key;
* validate task and configuration;
* show exact conditions and randomized order;
* show expected paths;
* perform no model requests.

## `pi-shootout-check`

Must run provider-free checks:

```text
Python contract/report tests
TypeScript typecheck
TypeScript reporter tests
Rust adapter unit tests
deterministic Tea JIT lifecycle tests
oracle-isolation tests
```

It may assume `npm ci` has populated the pinned SDK package or perform the pinned install in its own explicit cache.

## `pi-shootout`

Must:

1. validate prerequisites;
2. run `npm ci` in the isolated SDK directory;
3. prepare the exact repository cache;
4. build the Tea adapter once;
5. execute the three conditions;
6. run validators;
7. generate evidence;
8. generate both reports;
9. print both report paths;
10. print concise report tables to stdout.

Do not add the live provider shootout to normal:

```text
make test
CI
pre-commit
```

It remains explicit and provider-opt-in.

---

# 26. Reuse existing evaluation code

Do not replace all of `evals/quality`.

Reuse and tighten:

```text
load_cases
materialize_clean_worktree
remove_worktree
run_validator
process timeout handling
cache population
patch capture
```

Correct the oracle-object leakage in worktree materialization as part of this work.

Do not build a second generic benchmark controller unless the existing code cannot be cleanly reused.

The shootout-specific orchestration may call existing private helpers initially if necessary, but prefer promoting genuinely shared helpers into focused public module functions rather than copying them.

Avoid a giant refactor of unrelated deterministic core and compaction evals.

---

# 27. Curated observability

The shootout evidence should be sufficient to diagnose:

* wrong files inspected;
* repeated search;
* malformed tool calls;
* tool failures;
* retries;
* compaction;
* harness candidate lifecycle;
* surface differences;
* candidate activation;
* final patch;
* validator outcome;
* token and time cost.

It must not retain:

* provider credentials;
* authorization headers;
* complete process environment;
* hidden chain-of-thought;
* unbounded provider response bodies;
* unbounded command output;
* unrelated home-directory content.

Do not change Tea’s compact production trace solely to support this eval.

Use an eval-only evidence collector layered over existing typed events and artifacts.

Keep the durable session and artifact store as Tea’s authoritative evidence where applicable.

---

# 28. Future-facing result without future machinery

The generated:

```text
summary.json
```

should be suitable as input to a later evolution campaign.

It should include:

```text
task identity
condition identity
model identity
harness identity
surface fingerprints
success
usage
timing
tool failures
patch identity
validator identity
candidate identity
candidate hypothesis
candidate outcome
```

Do not implement the later analyzer or optimizer now.

Do not add placeholder classes for future agents.

The schema itself is sufficient scaffolding.

---

# 29. Tests

## Python

Test:

* task selection;
* run-plan randomization;
* three-condition requirement;
* repeat handling;
* adapter contract validation;
* infrastructure versus benchmark failures;
* report generation with:

  * all pass;
  * Pi-only pass;
  * Tea-static-only pass;
  * JIT positive flip;
  * JIT regression;
  * JIT no-change;
  * candidate rejection;
* incompatible cost kinds;
* missing returned model;
* prompt/tool-surface mismatch;
* output-path safety;
* bounded logs;
* oracle commit absent from worktree.

## TypeScript

Test reporter normalization with synthetic SDK objects/events.

No model request.

## Rust

Use deterministic fake providers and temporary session roots.

Test:

* static normalized result;
* JIT no-change;
* candidate activation;
* candidate rejection;
* budget enforcement;
* usage by epoch;
* surface export;
* shell environment does not contain provider key;
* exact active tool list;
* valid result publication after model failure.

## Existing gates

Keep existing:

```text
core fixtures
compaction quality
provider adapter tests
session recovery
PTY visual tests
```

green.

---

# 30. Acceptance criteria

The implementation is complete only when:

* `make pi-shootout-plan` works without credentials.
* `make pi-shootout-check` is provider-free.
* `make pi-shootout` runs all three conditions.
* Pi is invoked through the SDK only.
* Pi SDK versions are exact and lockfile-pinned.
* Every condition gets a fresh baseline repository.
* The known fix commit is absent from attempt object databases.
* Every condition uses the same external validator.
* Every condition uses the same requested model and thinking level.
* Both Pi and Tea have `read/bash/edit/write`.
* Neither Pi nor Tea has a web-search tool.
* Both can run `curl` through bash.
* Provider keys are absent from coding-tool child environments.
* Neither has a subagent tool.
* Tea static and JIT use the same durable runtime implementation.
* Tea JIT may stage at most one candidate and roll over at most once.
* Tea JIT may choose `NoChange`.
* All JIT time and usage are included in totals.
* Static and evolution reports are both produced.
* Static report includes harness-surface parity.
* Evolution report includes candidate lineage and Tea-static delta.
* Benchmark failure still produces reports.
* Infrastructure failure exits nonzero.
* No live provider call occurs in normal tests.

---

# 31. Verification

Run at least:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
    evals.pi_shootout.test_contract \
    evals.pi_shootout.test_report

npm --prefix evals/pi_shootout/sdk ci
npm --prefix evals/pi_shootout/sdk run check
npm --prefix evals/pi_shootout/sdk test

cargo +nightly-2026-07-24 test \
    -p tea-providers \
    --bin tea-eval \
    --locked

cargo +nightly-2026-07-24 test -p tea-core --locked
cargo +nightly-2026-07-24 test -p tea-luau --locked
cargo +nightly-2026-07-24 test -p tea-agent --lib --locked

cargo +nightly-2026-07-24 test \
    -p tea-agent \
    --features pty-harness \
    --test pty_streaming \
    --locked

make pi-shootout-plan
make pi-shootout-check
make test

git diff --check
```

Then perform one explicit live run with caller-supplied credentials:

```sh
PI_SHOOTOUT_MODEL=<explicit-model> \
PI_SHOOTOUT_ENV_FILE=<explicit-env-file> \
PI_SHOOTOUT_REPEATS=1 \
make pi-shootout
```

Do not include the live run in automated CI.

---

# 32. Final report from the coding agent

When implementation is complete, report:

1. exact Pi SDK package versions;
2. exact default task;
3. normalized result schema;
4. Tea adapter path;
5. Pi SDK adapter path;
6. how shell environments are equalized;
7. how `curl` access is preserved;
8. how provider keys are excluded from shell tools;
9. how fix-commit leakage is prevented;
10. Tea JIT candidate and rollover budgets;
11. deterministic JIT tests;
12. `make pi-shootout` invocation;
13. generated report paths;
14. static observed result;
15. JIT observed result;
16. all verification commands run;
17. any metric that remained unavailable or non-comparable.

Do not claim Tea is superior based on this single task.

The successful deliverable is:

> A reproducible one-task experiment that precisely shows Tea’s static position against Pi, isolates the marginal effect and cost of Tea’s task-local harness adaptation, and emits enough sourced evidence to support the next iteration.
