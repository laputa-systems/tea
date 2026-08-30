Your job is to harden the existing Pi-vs-Tea shootout into a benchmark whose conclusions can actually be trusted.

This is **not** a request to optimize Tea against the current result yet. Do not attempt to reduce Tea's token count merely because the latest run shows Tea behind Pi. First eliminate remaining harness, provider-routing, request-shape, observability, and experimental-design confounders.

The benchmark already exists. Preserve its small scope and architecture. Do not build a new eval framework or new Rust crate.

The central static experiment remains:

```text
pi-static
vs
tea-static
```

using the same pinned coding task and model authority.

`tea-jit` remains the separate evolution condition.

The native static comparison is intentionally:

```text
Pi's native static coding harness
vs
Tea's native static coding harness
```

Therefore differences in their native system prompts, tool descriptions, schemas, prompt guidelines, and other harness-policy choices are **measurements / independent variables**, not automatically experiment-invalidating defects.

The controlled variables are the task, initial repository, validator, model, actual inference-provider route, reasoning configuration, sampling configuration, output ceiling, external authority, environment, timeout, and other non-harness execution conditions.

Read the current implementation before editing:

```text
shootout.md
EFFICIENCY.md
Makefile

evals/pi_shootout/
evals/quality/cases/coding/express-3936-medium/

crates/tea-providers/src/bin/tea-eval.rs
crates/tea-providers/src/openai.rs
crates/tea-providers/src/openrouter/
```

Also clone or update:

```text
https://github.com/earendil-works/pi
```

and inspect the current upstream implementation, especially:

```text
packages/coding-agent/src/core/sdk.ts
packages/coding-agent/src/core/agent-session.ts
packages/coding-agent/src/core/system-prompt.ts
packages/coding-agent/src/core/tools/
packages/coding-agent/src/index.ts

packages/agent/src/agent.ts
packages/agent/src/agent-loop.ts

packages/ai/src/api/openai-completions.ts
packages/ai/src/api/transform-messages.ts
```

The executable benchmark must continue using its exact pinned published Pi SDK dependency. Current Tea pins `@earendil-works/pi-ai` and `@earendil-works/pi-coding-agent` to `0.84.4`; do not silently switch the benchmark to an arbitrary Pi HEAD. Inspect HEAD to understand current upstream design and fixes, while treating the exact package lock used by the benchmark as the executable source of truth.

The following recently discovered fixes are already intentional and must not regress:

```text
- Pi uses the public *ToolDefinition factories rather than metadata-less wrappers.
- The model-facing Pi prompt must contain its native tool prompt snippets and must never say "Available tools: (none)" while read/bash/edit/find are active.
- Pi remains isolated while its custom bash definition receives the shootout's sanitized environment.
- Pi SDK is exactly pinned.
- Tea does not send an explicit temperature merely because its internal default used to be zero.
- Tea omits empty assistant tool_calls arrays where Pi/OpenAI projection omits them.
- DeepSeek assistant history is replayed with the required reasoning_content shape.
- Normalized usage accounting and the clarified turns/model_turns/tool_calls/retry/compaction meanings remain intact.
```

Implement this work in the following order.

1. Make the actual provider request the primary fairness oracle.

Add a sanitized, canonical **wire-request evidence layer** for both adapters.

Capture the request at the last meaningful boundary before the OpenRouter HTTP request is sent—not a high-level approximation reconstructed later.

For Pi, use the public/request-interception machinery exposed by the pinned SDK/provider stack where possible. Pi's agent/provider implementation already has payload interception support; do not fork Pi or monkey-patch private internals merely to observe it.

For Tea, instrument the existing OpenRouter request construction/send boundary cleanly.

For each model request, retain enough evidence to answer exactly what the model/provider received while never persisting credentials.

At minimum derive and record:

```text
request ordinal
canonical request SHA-256
model
message count
message roles in order
canonical per-message structural/content digests
system-prompt digest
tool count
tool names in exact request order
canonical tool-schema digest
reasoning configuration
temperature presence/value
seed presence/value
max_tokens / max_completion_tokens presence/value
tool_choice presence/value
parallel-tool-calls or analogous setting if present
stream setting
stream-options setting
OpenRouter provider-routing object
other model-affecting top-level request fields
```

Persist a sanitized canonical representation under the attempt evidence directory. It is acceptable to retain complete task/model-facing text inside the private attempt evidence if the existing shootout already treats system prompts as private evidence, but the shareable comparison report should primarily expose hashes and structural summaries.

Never persist:

```text
OPENROUTER_API_KEY
Authorization headers
vault credentials
unredacted unrelated environment variables
```

Normalize attempt-local workspace/HOME/TMPDIR/cache paths only for fingerprinting. Do not mutate what is actually sent to the model.

Add a provider-free fixture/test path that can construct/capture the first Pi and Tea requests without spending inference tokens. The point is to catch request-shape regressions before a live shootout.

A regression equivalent to:

```text
session says four tools are active
but provider payload has zero tools
```

must become impossible to miss.

2. Fix the remaining OpenRouter routing mismatch.

Tea currently adds:

```json
{
  "provider": {
    "require_parameters": true
  }
}
```

when tools are present.

The Pi path does not appear to apply the same routing condition.

This is a benchmark confound. OpenRouter can serve this model through many underlying providers, and `require_parameters` changes provider eligibility.

Do not merely document the mismatch.

Make provider routing an explicit controlled variable of the shootout and send the exact same routing policy from Pi and Tea.

Prefer a reproducible shootout design that pins one suitable underlying OpenRouter provider endpoint for the whole run, after verifying that endpoint supports the exact model parameters used by this experiment, including tool calling and the chosen reasoning configuration.

If pinning one endpoint is impractical for a defensible reason, use one explicit identical provider-routing object for both implementations and preserve enough response/request evidence to determine the actual endpoint chosen. Do not leave one side with `require_parameters=true` and the other with OpenRouter defaults.

Do not change Tea's general production OpenRouter semantics gratuitously. If `require_parameters=true` is desirable for normal Tea operation, make the shootout's controlled routing policy an explicit configuration rather than globally weakening production behavior.

Add the routing policy and, where actually observable, the concrete routed provider endpoint to run/attempt identity.

A pair that used different underlying inference providers should be visibly marked as route-mismatched and should not support a strict efficiency conclusion.

3. Repair the definition of "comparable."

There is currently a contradiction:

`shootout.md` says differing Pi/Tea model-facing harness surfaces should be measured and should not by themselves fail the shootout.

`compare.py` currently makes differing normalized system-prompt hashes and tool-surface hashes reasons the pair is non-comparable.

Resolve this in favor of the experiment's actual purpose.

For the native static shootout, comparability must require equality of controlled variables such as:

```text
task identity
initial repository commit/state
validator
requested model
actual provider-routing policy
actual underlying provider when strict route identity is available
thinking/reasoning configuration
sampling controls
output ceiling
timeout
closed capability names/authority
shell authority
secret boundary
initial workspace state
```

But it must **not** require Pi's and Tea's native system prompts or native tool schemas/descriptions to be byte-identical.

Instead report native harness differences prominently as measured surface differences.

Keep the closed authority invariant:

```text
read
bash
edit
find
```

Anything outside that authority boundary remains an invalid comparison.

Distinguish at least:

```text
controlled-condition mismatch
native-harness surface difference
wire-shape bug/mismatch
observability unknown
```

Do not collapse all four into `comparable=false`.

If useful, represent comparability as structured checks rather than one opaque list of strings.

Do not add a whole second benchmark mode unless it substantially simplifies the design. A future "same exact prompt/tools, compare runtimes only" experiment may be useful, but it is not the primary shootout being implemented here.

4. Fix Pi adapter/reporting correctness.

The current Pi reporter's `final_text` extraction assumes:

```text
assistant.content is a string
```

Real Pi assistant messages use structured content blocks.

Extract visible final assistant text correctly from Pi's real message representation. Prefer Pi's public content helpers if suitable. Do not include hidden reasoning or serialize tool calls into `final_text`.

Update tests to use realistic Pi assistant message content rather than the current string-only fake.

Also fix `returned_model` and `returned_provider`.

Do not report:

```text
session.model.id
session.model.provider
```

as provider-returned values. Those describe the selected model configuration / OpenRouter transport, not necessarily what the response said.

Leave these fields null unless they were actually observed in a provider response. If the wire response exposes a concrete returned model or underlying provider, capture it at the response boundary and record its provenance explicitly.

Tea currently being honest with null values is preferable to manufactured certainty.

Do the same audit for every result field whose name implies provider-observed information.

5. Make the Pi tool-surface regression test real.

The current adapter test stops before constructing/running the live Pi session because no credential exists. It therefore would not have caught the original `(none)` bug.

Create a provider-free test seam allowing a real pinned Pi session to be constructed with the isolated shootout definitions without making a network inference call.

Assert all of the following from the actual session/request construction:

```text
getActiveToolNames() is exactly read,bash,edit,find in expected order

the system prompt contains Pi's native prompt snippets for all four tools

the system prompt does not contain:
Available tools:
(none)

the actual model request has exactly four tool definitions

the request has no duplicate tool names

the actual schemas/descriptions are the native *ToolDefinition-derived Pi definitions

the isolated bash execution environment remains the shootout environment

no PI_* session authority is accidentally leaked into bash when exposeSessionEnvironment=false

no discovered extension/skill/prompt/AGENTS resource enters the Pi session
```

Preserve Pi's native prompt/tool metadata. Do not "solve" parity by rewriting Pi's definitions to resemble Tea.

6. Improve surface evidence fidelity.

Do not hardcode:

```text
execution_mode = "parallel"
```

in the Pi reporter merely because that is currently expected.

Serialize the actual effective tool execution metadata wherever the public Pi definition/runtime exposes it. If some property genuinely cannot be observed, record it as unknown/null instead of manufacturing a value.

Ensure `tool_surface_sha256` is computed from exactly the tools exposed to the model in their effective order, not from an unrelated registry snapshot.

Retain a separate registered-tool view only if diagnostically useful.

For both Pi and Tea, distinguish:

```text
authority: what operations the harness permits
prompt surface: how tools are described to the model
wire surface: what JSON tool definitions are sent to the provider
execution semantics: how accepted calls actually run
```

These are different concepts and should not share one misleading hash.

7. Make Pi turn-level observability substantially symmetric with Tea.

Pi's public events already expose more than the adapter currently retains.

Without persisting raw sensitive commands in the shareable report, capture for Pi:

```text
turn ordinal
assistant stop reason
visible assistant text byte count
tool call names
tool call IDs
SHA-256 of canonical tool arguments
tool execution success/error
per-assistant-message/model-turn token usage where exposed
cache read/write usage where exposed
retry/compaction lifecycle
```

The actual Pi event emits tool arguments at tool-execution start, so hash them there rather than claiming they are unavailable.

This should allow the same work-category analysis currently performed from Tea's durable session to operate meaningfully on Pi:

```text
inspection
edit
validation
lint
repository_state
upstream_or_dependency
shell
other
```

Do not put raw shell commands into the shareable comparison report.

If private attempt evidence retains them, clearly separate private evidence from shareable report data.

Preserve null for genuinely unavailable Pi data such as exact provider-request count if the SDK cannot observe it reliably. Never infer provider requests from model turns.

8. Stop positionally pairing unrelated Tea request/turn evidence.

Audit the Tea durable-session analysis in `compare.py`.

It currently associates provider request information with assistant turns largely by ordinal position.

That is only safe while:

```text
one model turn == one settled provider request
no retry/failure perturbs ordering
no background model operation exists
```

Strengthen the durable records or analysis so model turns/provider requests carry an explicit correlation/request ID and are joined by identity.

If changing the durable Tea schema is disproportionate, at minimum detect ambiguity and mark the turn-level request association incomplete instead of silently pairing the wrong records.

Never manufacture per-turn attribution merely because list lengths happen to line up.

9. Record runtime policies before the run, not only outcomes after it.

The result currently tells us that there were N retries or N compactions. That does not fully prove both harnesses were configured with equivalent policies.

Add explicit effective-policy evidence for the static conditions covering at least:

```text
automatic compaction enabled/disabled
compaction thresholds relevant to the task
provider retry count/policy
request timeout/idle timeout where applicable
outer attempt timeout
tool execution mode/scheduling
model reasoning setting
output-token ceiling
provider-routing policy
sampling overrides
```

Classify which of these are controlled conditions and which are intentionally native harness behavior.

Fail closed only for variables the experiment claims to control.

10. Counterbalance run order.

Replace the current independent per-repeat random permutation.

For two-condition static comparisons, generate a balanced sequence of:

```text
AB
BA
```

with any odd leftover randomized fairly.

For the three-condition full shootout, use a simple balanced Latin-square/Williams-style schedule so each condition appears in each ordinal position approximately equally and follows the others approximately equally.

The seed should determine reproducible assignment/order, but balance should be an invariant rather than an accident.

Keep attempts sequential unless there is a compelling reason to change that; simultaneous provider calls would introduce a different class of confound.

Add deterministic tests showing order balance for representative repeat counts and seeds.

11. Distinguish smoke repeats from an efficiency conclusion.

Keep repeats configurable.

Three repeats may remain useful as the inexpensive developer workflow, but label that explicitly as a smoke/diagnostic comparison.

For a serious static comparison, use a somewhat stronger default or named target—preferably at least 5, and 7 if cost remains reasonable.

In addition to median/min/max, report paired per-repeat observations and a simple uncertainty measure appropriate to the tiny sample.

Do not pretend an asymptotic p-value is meaningful at n=3.

A bootstrap confidence interval for the paired delta is acceptable as descriptive evidence, provided the report states the sample size prominently.

Never declare "Tea is X% more efficient than Pi" from one stochastic run.

12. Fingerprint the executable environment.

The current sanitized child environment preserves the caller's PATH. This is good for secret isolation but is not enough to establish reproducibility across machines/runs.

Do not necessarily containerize the benchmark.

Instead add a lightweight toolchain manifest recording the resolved path and version/digest where practical for the executables the coding task can materially depend on, at least:

```text
bash
git
curl
node
npm
```

and any additional directly required runtime for the pinned Express case.

Normalize attempt-local paths but retain meaningful executable provenance.

Hash the manifest into run identity/evidence.

Also record/assert the clean initial workspace state before the agent begins. The pinned git commit remains authoritative, but an explicit clean-state/tree fingerprint makes accidental generated/untracked state obvious.

Do not leak arbitrary environment variables while doing this.

13. Strengthen provider-free regression coverage.

`make pi-shootout-check` should catch as much experiment-integrity drift as possible without an API key.

Add focused tests/golden structural assertions for:

```text
Pi native tool prompt visibility
Pi actual four-tool request surface
Tea four-tool request surface
DeepSeek reasoning_content replay
empty tool_calls omission
temperature omission/equality
reasoning effort equality
max-output field equality/omission
OpenRouter routing-policy equality
wire request structural comparison
final_text extraction
returned-model/provider provenance
surface-difference comparability semantics
controlled-condition mismatch rejection
argument-digest privacy
turn/request correlation behavior
counterbalanced scheduling
toolchain manifest determinism
no credential leakage
```

The request fixtures must be semantic/canonical rather than hypersensitive snapshots of irrelevant JSON formatting.

When a meaningful model-facing field changes, a test should fail with a useful structural diff.

14. Update the documentation to match reality.

Clean up contradictions across:

```text
shootout.md
EFFICIENCY.md
evals/pi_shootout/README.md
```

Make the benchmark philosophy explicit:

```text
The static shootout compares two native harnesses under the same controlled inference/task environment.

Native harness differences are results, not automatic invalidators.

Provider/wire/control-condition mismatches invalidate strict comparison.

Raw provider requests are the ground truth for what the model actually saw.

Normalized adapter contracts are summaries of that evidence, not substitutes for it.
```

Document the two run classes:

```text
cheap smoke/diagnostic run
serious repeated comparison
```

Do not leave stale numerical claims prominently documented after changing a condition that invalidates those historical numbers. Historical results may remain clearly labeled as pre-hardening/non-comparable evidence.

### TODO: make validator dependencies deterministic

The pinned Express `express-3936-medium` checkout declares `body-parser` in
`package.json` but does not include `node_modules` (or a lockfile that the
shootout cache can key). The fast validator invokes Node directly, so a clean
attempt fails with `Cannot find module 'body-parser'` unless the model happens
to run `npm install` first. This happened for Tea in medium repeat 1; Pi and
Tea repeat 2 installed dependencies during their sessions and therefore did
not exercise the same setup.

Before the next live shootout, choose and implement one deterministic,
network-disabled dependency strategy (for example, a pinned lockfile plus
runner-provisioned cache, or an identical pre-validator install for every
attempt). Dependency availability must not depend on whether a model remembers
to run `npm install`, and setup failures must be distinguished from model
validator failures. Add a provider-free regression check and record the
resolved dependency/toolchain fingerprint in the attempt evidence.

15. Verification and completion criteria.

Run all provider-free checks, including:

```sh
make pi-shootout-check
```

plus relevant Rust tests/format/lint commands used by this repository.

Exercise the plan command and inspect generated counterbalanced schedules.

Construct provider-free Pi and Tea payload fixtures and prove the structural comparator catches intentionally introduced mismatches.

Specifically inject test mutations equivalent to:

```text
Pi tools = []
Tea temperature = 0
Tea provider.require_parameters differs from Pi
reasoning_content missing
different routed-provider policy
extra capability/tool
```

and prove the appropriate check fails.

Also prove that:

```text
Pi native system prompt != Tea native system prompt
```

does **not** by itself invalidate the native static comparison.

If `OPENROUTER_API_KEY` is available through the repository's existing `vault` workflow, finish with at least one live static smoke pair and inspect the captured wire evidence.

Do not require a paid live run for ordinary unit-test success.

If a live run is available, do not optimize Tea based on it in this task. The purpose of the run is to verify that the experiment is now instrumented and controlled correctly.

At completion, report:

```text
what concrete fairness bugs were fixed
what request fields are now directly observed
the exact provider-routing policy
what conditions now invalidate comparison
what native harness differences remain intentionally visible
which Pi fields remain genuinely unknowable
provider-free verification commands and results
live smoke result if one was actually run
remaining limitations
```

Do not declare the benchmark "fair" merely because hashes happen to match. Demonstrate that the invariants are checked at the model/provider boundary.

Keep the implementation narrow. No new Rust crate, no generic benchmark platform, no SWE-bench/Terminal-Bench work, no subagents, no new model family, and no Tea performance tuning yet.

The immediate target is a shootout where the next efficiency result is worth believing.
