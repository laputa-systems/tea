# Coding-agent prompt: bring tea up to speed with Pi 1.0

A local Pi checkout is available at **`~/d/pi`** as a read-only implementation and test reference.

Implement a selective architectural upgrade based on Pi 1.0: transcript-aware prompt and tool changes, explicit provider capabilities, native Anthropic support, cache warming, deferred tool discovery, Luau codemode, optional local MCP integration, virtual models, and provider-exposed thinking. Also implement **TUI crash isolation for headful sessions**.

The goal is **a better small, extensible agent runtime—not feature parity with Pi, and not a port of Pi Durable**. Pi’s announcement distinguishes its coding-harness improvements from the separate Durable framework; follow the former selectively. [Earendil](https://earendil.com/posts/pi-1-0/)

**Implement the work, do not stop at a design document.** You own the detailed design, module boundaries, sequencing, and necessary refactoring. The requirements below describe direction and observable outcomes rather than prescribing a new class hierarchy or exact API.

## Working approach and architectural boundaries

Read tea’s `AGENTS.md`, architecture, semantics, provider, extension, persistence, TUI, and verification documentation before changing contracts. Inspect implementations and tests rather than assuming documentation is perfectly current.

Inspect the relevant Pi implementation, helpers, and tests in `~/d/pi`. Record both repositories’ starting revisions. Prefer adapting proven behavior over independently inventing approximations, especially at provider boundaries. Preserve appropriate license notices and attribution for ported code and tests.

Build on tea’s existing agent algorithm, durable session authority, effect admission and settlement, immutable harness revisions, host-owned execution, and observation model. Do not introduce a parallel engine for the new features. Tea’s existing single-session contract explicitly permits a session-owned presentation/execution process boundary while prohibiting a shared resident service.

The following remain hard boundaries:

- **One attached live root session per host.** No daemon, broker, live-root registry, session tabs, background root-session scheduler, or global control plane. Saved sessions remain inert until explicitly opened. Existing child lanes remain subordinate to their root.
- **Small, embeddable Rust runtime.** Keep the core provider- and executor-agnostic. Reuse existing dependencies, Luau, and `tea-http`; do not introduce Tokio, a JavaScript runtime, SQLite, or a new application framework. Follow the repository’s dependency-approval rule.
- **Preserve tea’s terminal design.** Keep native scrollback, the bounded mutable presentation tail, and the zero-dependency `tea-tui` boundary. Do not adopt Pi’s fullscreen default.
- **No multimodal expansion.** No image input, image generation, audio, video, classifier integration, or dormant scaffolding for those features. Provider-exposed thinking is explicitly in scope.

Prefer one clean contract over parallel legacy APIs. Update in-tree consumers, documentation, and fixtures together. Preserve durability and recovery guarantees; make any unavoidable storage incompatibility explicit rather than silently reinterpreting saved sessions. Do not turn this into an unrelated persistence rewrite.

## 1. Typed requests and transcript-aware configuration

Strengthen tea’s request and conversation representation so that provider behavior is not built around opaque serialized-context manipulation.

Pi’s shared message contract records an initial system/tool configuration and subsequent ordered updates. Its transcript helpers are a useful reference, but tea should express this through its own authoritative session and projection model rather than maintaining a second configuration history.

The resulting design should distinguish:

**Conversation and configuration history.** Prompt-section changes and changes to model-visible tools must have a clear place in conversation order, survive reopen and branching, and reproduce the intended effective configuration.

**Provider projection.** Adapters should derive their supported wire representation from typed material. Where a provider supports genuine mid-conversation configuration updates, preserve that behavior. Where it does not, use a deliberate, tested projection and report any resulting cache-layout discontinuity honestly.

**Authority and exposure.** Making an already-authorized tool visible is not the same as granting a capability or activating different extension code. Preserve immutable extension activation and host capability ceilings.

**Request purpose and provenance.** Ordinary turns, compaction, and cache maintenance should be distinguishable without inspecting prompt text for magic strings. Preserve selected versus dispatched model identity where routing is involved.

Compaction must preserve the effective configuration needed to continue correctly without deleting original history. Reopen, forks, model changes, and extension activation must not accidentally apply future configuration to earlier history.

The deliverable is a coherent contract used by the actual runtime and all affected adapters—not new types sitting beside the existing string-based path.

## 2. Native Anthropic provider: port Pi’s behavior and tests

Add an **optional native Anthropic API-key adapter**, integrated with tea’s existing provider selection, configuration, transport, accounting, and error-reporting boundaries.

Use Pi’s implementation as the primary reference, particularly:

```text
~/d/pi/packages/ai/src/api/anthropic-messages.ts
~/d/pi/packages/ai/src/providers/anthropic.ts
```

Follow their dependencies into transcript conversion, reasoning options, message transformation, retry handling, schema compatibility, and related tests. Pi’s adapter relies on the Anthropic SDK, so translating the visible adapter alone is not necessarily enough to reproduce its wire behavior. Account for relevant SDK-provided behavior that tea’s direct transport must handle itself.

**Port and adapt the applicable implementation and test cases into Rust.** Preserve the behavioral edge cases, not the TypeScript packaging or SDK architecture. Reuse `tea-http` rather than adding another HTTP stack.

Cover the supported text, thinking, tool-use, transcript-update, cache-control, usage, cancellation, and error paths. Integrate the relevant model capabilities and reasoning configuration without building an exhaustive new provider catalog.

Port only the agreed scope. Do not import image handling, subscription OAuth, cloud federation, unrelated provider variants, legacy compatibility shims, or first-party-client impersonation behavior. Keep tea’s identity honest and credential resolution host-owned.

Maintain a compact mapping from relevant upstream behavior/tests to the corresponding tea implementation/tests. Identify deliberate exclusions, especially image and authentication features, so “ported from Pi” has a concrete meaning.

**No live Anthropic verification is required or authorized.** A successful deliverable is an offline-verified implementation, not proof of current account availability or actual provider cache savings.

## 3. Provider-exposed thinking, without image support

Support thinking or reasoning summaries that providers intentionally expose, separately from answer text.

Preserve meaningful content-block order and distinguish visible thinking from opaque signatures, encrypted continuation material, and ordinary assistant content. Provider-private replay material must remain opaque and must not be rendered or exposed to tools.

Implement this through the full path: provider parsing, runtime events and snapshots, durable settlement and reopen, context projection, and TUI presentation. Wire existing adapters where they expose supported reasoning output; do not implement this only for Anthropic.

Cross-provider and cross-model replay must respect compatibility. A model switch must not blindly forward another provider’s private signatures or flatten thinking into ordinary assistant prose. Keep any required transformation explicit and tested.

The terminal should display provider-exposed thinking usefully without changing the native-scrollback architecture. Preserve distinctions during partial streaming, cancellation, failure, and reattachment.

Do not add image variants, attachment plumbing, image placeholders, or base64 handling merely because Pi’s content representation includes them.

## 4. Active-work cache warming

Implement cache warming for eligible requests, beginning with native Anthropic support.

Use Pi’s cache warmer and its associated tests as the behavioral reference:

```text
~/d/pi/packages/coding-agent/src/core/cache-warmer.ts
```

Pi’s implementation includes request-replay eligibility, declared lifetimes, economic decisions, deadline checks, bounded warming horizons, cancellation, and usage records outside model context. Carry over the applicable behavior rather than reducing the feature to a periodic keep-alive timer.

**Scope is active work only.** Cover eligible long-running generation and tool waits. Do not warm between completed user turns or keep an otherwise idle session alive for predicted future activity.

Use explicit provider/model capabilities and the actual admitted request’s cache-relevant configuration. A maintenance request must not rerun routing or request-mutating extension hooks, accidentally target a different physical model, or execute returned tool calls.

Keep maintenance subordinate to the owning operation and session. It must be cancellable, must not delay a ready real continuation unnecessarily, and must not become a separate agent run. Do not allow refreshes to extend their own lifetime indefinitely.

Preserve Pi’s relevant replay guards and timing behavior. Handle expired or late deadlines, laptop suspension, request supersession, configuration changes, and incompatible reasoning settings. Do not assume that reducing the output limit is cache-equivalent for every model or request.

Treat warming as an attributed provider operation rather than a fake assistant message. Account for available usage and cost without polluting model context or disturbing ordinary request-continuity measurements. Keep estimated savings separate from reported usage and charges; unknown values remain unknown. A late result must not mutate a superseded conversation, but any available billing evidence should not simply disappear.

Failures should be best-effort maintenance failures, not failures of otherwise healthy agent work. Keep the policy and diagnostics understandable without creating a large tuning surface.

## 5. Deferred discovery and optional Luau codemode

Separate **authorized tools**, **discoverable tools**, and **tools currently declared to the model**.

Allow larger optional tool sets to be discovered and exposed on demand without placing every schema in every prompt. Preserve stable ordering, identities, and cache-friendly descriptions where possible. Discovery must never grant new authority, and resumed or branched sessions must reconstruct the intended exposure.

Keep tea’s ordinary four-tool coding experience intact. Deferred discovery and codemode are optional additions, not a requirement to replace every direct call.

Implement **Luau codemode** using tea’s existing interpreter infrastructure. Pi’s codemode demonstrates the desired benefit: a model-authored script can compose tool calls, process intermediate results, and return only useful output to the model. Adapt that concept, not its QuickJS implementation or non-chat model APIs.

Model-authored scripts must receive a deliberately restricted tool-calling surface, not extension-host privileges. Calls must retain the normal validation, capability checks, hooks, effect attribution, cancellation, and settlement semantics. Do not create an alternate path that invokes capabilities directly behind the runtime’s back.

Support useful sequential composition and permitted concurrency. Intermediate results may stay out of the next model prompt, but their actual effects must not disappear from durable evidence.

Provide bounded execution and output, clear recovery-oriented errors, and honest partial-failure behavior. A script failure does not undo tool effects that already occurred. Outstanding calls must settle according to tea’s existing ownership rules rather than becoming detached work.

Keep this a small composition facility. Do not introduce a durable workflow language, general task scheduler, recursive agent invocation API, or a separate persistent script-state framework.

## 6. Optional local MCP integration

Add **host-side MCP support for explicitly configured local stdio servers**.

Keep protocol handling, server-process ownership, configuration, and any credential handling outside `tea-core`. Integrate MCP tools through the same tool registry, discovery, exposure, and codemode facilities as ordinary tea tools.

Do not make deferred discovery an MCP-only mechanism. Conversely, do not eagerly load every MCP tool into the prompt or block unrelated startup work on unused optional servers.

Preserve usable structured results, errors, tool identities, and cancellation behavior. Unsupported non-text content must be identified clearly rather than silently dropped or converted into a huge base64 transcript.

The MCP server lifecycle belongs to the session/runtime, not the renderer. Handle connection failure, server exit, and shutdown through explicit ownership without creating another resident supervisor.

Out of scope: remote HTTP MCP, OAuth flows, provider-token sharing, server marketplaces, ambient configuration discovery, and a large MCP-management UI. Include a small local example and a self-contained fake server for verification.

## 7. Small, extension-defined virtual models

Implement virtual model selection constrained to a **host-approved physical model set**.

Pi’s virtual-model contract distinguishes user selection from physical dispatch and supports request-aware routing with branch-local state. Use it as a reference while retaining tea’s stricter authority and immutable-extension boundaries.

Start with deterministic extension-defined routing, not classifier calls, speculative execution, or an autonomous optimization system. Keep tool continuations and retries on their selected physical model by default unless an explicit authorized policy changes it.

Persist the identities needed to explain what was selected and what actually ran. Context limits, reasoning support, provider-private continuation data, cache eligibility, and accounting must follow the physical dispatch.

Any routing state must use tea’s existing bounded extension-state model and preserve branch/reopen semantics. A missing router or unavailable target should fail clearly rather than silently choosing a different provider.

Include a small working example, such as an explicit planning/implementation router. Cache warming must replay the prior eligible physical request without invoking the router again.

## 8. Actual TUI crash isolation for headful sessions

Implement crash isolation for **normal headful sessions**, not merely an abstraction that could support it later.

The TUI becomes disposable presentation for one session-owned runtime. Killing or crashing only the TUI must not kill admitted generation, tool work, subordinate children, or durable settlement.

Keep the boundary strictly one-to-one and session-local. Reattachment must identify the exact session directly. There must be no broker, global listener, live-session enumeration service, or runner that can be repurposed to host other root sessions.

Use the smallest suitable local process/IPC design. Reuse tea’s existing runtime and snapshot/update contract; do not invent a second agent protocol or transfer semantic authority to the TUI.

Headless invocations and Rust embeddings should remain straightforward in-process users of the same runtime. They should not need a helper process merely because headful sessions use one.

Define and implement clear behavior for normal quit, unexpected TUI loss, reattachment, `/new`, `/resume`, runtime failure, and detached-idle cleanup. A renderer disconnect is not automatically an agent cancellation. Conversely, switching sessions must not quietly accumulate abandoned root runtimes. Do not leave an indefinite hidden service after detached work has settled.

Preserve one authoritative writer and prevent duplicate execution across reattachment. Rebuild the view from authoritative state without replaying completed effects or duplicating accepted inputs. Runtime failure must still use tea’s existing honest recovery semantics for ambiguous effects.

Pay particular attention to terminal and process-group ownership: the runtime must survive TUI loss without weakening bounded `bash` cleanup, subagent cancellation, or explicit shutdown.

## Offline verification is a deliverable

**All acceptance verification must be offline and require no inference spend.** Do not invoke paid providers, free providers, live local models, real authentication flows, or real external MCP services. Do not read the user’s actual provider credentials during tests.

Local fixture HTTP servers, subprocesses, PTYs, and fake stdio MCP servers are allowed. Once normal build prerequisites are available, acceptance tests must run without external network access.

Do not indiscriminately run upstream Pi tests: distinguish offline tests from live integration tests and port the relevant cases. The final tea tests must be self-contained; they must not require `~/d/pi` to remain present.

### Build a strong shared mock-provider facility

Extend or replace weak existing mocks with a reusable, deterministic fixture provider that can drive the actual runtime and headful host.

It should support realistic incremental output, provider-exposed thinking, tool calls, usage, failures, cancellation, and controlled pauses. Tests should be able to coordinate request and effect boundaries, inspect outgoing requests, and exercise continuation behavior without relying on fragile sleeps.

Use Pi’s mock/test infrastructure where useful, but do not assume its mocks are sufficient unchanged. Keep this test support focused rather than turning it into a second provider framework.

**A scripted model mock is not enough to verify an adapter.** Also test the real Anthropic serializer and parser against request fixtures and raw response/SSE fixtures, including fragmented delivery. Preserve upstream expected behavior independently of tea’s implementation so tests cannot pass merely because a fake and the implementation share the same mistake.

Use controllable clocks for cache timing. Keep process-crash tests real, but coordinate them with explicit boundaries rather than long wall-clock waits.

### Required acceptance evidence

Choose the exact tests and organization. At minimum, demonstrate these outcomes:

| Boundary | Observable acceptance |
|---|---|
| **Anthropic port** | Applicable upstream cases pass through tea’s real request/response implementation, covering text, thinking, tool calls, usage, cache controls, errors, cancellation, and relevant retry behavior. Deliberate omissions are documented. |
| **Transcript/configuration** | Prompt and tool-exposure changes reproduce correctly through continuation, compaction, fork, reopen, and provider projection without widening authority. |
| **Thinking** | Streamed and settled thinking remains distinct from answer text; opaque replay material stays private; cancellation and model switching preserve a valid transcript. |
| **Cache warming** | Virtual-time tests cover eligible refreshes, ineligible requests, late/expired deadlines, supersession, cancellation, accounting, and best-effort failure. Warming neither executes tools nor enters model context, and stops after active work. |
| **Codemode** | Scripts compose real registered tools under existing authority, retain completed effects on partial failure, bound output/work, and settle outstanding calls. Recovery errors are useful. |
| **MCP and discovery** | A fake stdio server proves tool discovery, exposure, structured results, failures, cancellation, and process cleanup without contacting external services. |
| **Virtual models** | Selection and dispatch are distinguishable; routing respects the approved set, physical capabilities, continuation behavior, and branch-local state. |
| **TUI crash isolation** | A real PTY/process test kills the TUI during admitted work, proves that work settles durably, reattaches to the exact session, and verifies no duplicate request, input, or effect. |
| **Lifecycle and recovery** | Normal quit, session replacement, stale attachment metadata, competing ownership, runner failure, and detached-idle cleanup behave as documented. Existing ambiguous-effect protections remain intact. |
| **Existing behavior** | Ordinary direct-tool use, headless operation, embeddings, child lanes, native scrollback, and existing provider behavior retain appropriate regression coverage. |

Include combined scenarios, not just isolated feature tests. In particular, exercise discovery or codemode during an active operation while cache maintenance is eligible, and exercise TUI loss while runtime-owned work is in progress. These interactions are where ownership mistakes are most likely.

Use the repository’s existing integration, fixture, and PTY suites as the baseline. Verify macOS ARM64 behavior and use the repository’s Linux/Docker path for relevant cross-platform process behavior. Follow current repository verification conventions, including its restrictions on formatters and linters.

Measure the effect on release size, startup, and idle memory where the existing tooling permits. Include the **combined TUI-plus-runtime footprint** for headful sessions; do not compare only one process against the previous whole application. Investigate meaningful regressions without inventing arbitrary performance targets.

## Completion and handoff

Ship the integrated implementation, runnable examples, self-contained offline tests, and updated documentation. Optional features should be usable and verifiable, not disconnected scaffolding or placeholder interfaces.

Keep a compact record of the Pi revision and relevant source/test provenance, the architectural decisions that matter, deliberate deviations, and test commands/results. Document how to enable the new features and how to reattach a headful session without introducing a session manager.

The final report must distinguish **offline protocol/behavior verification** from claims that require real providers. No live calls are needed, and unmeasured real-world cache savings are not a blocker to completing this work. Report them as unmeasured—not as demonstrated by a mock.

Make reasonable implementation decisions and proceed. Favor the smallest coherent design that delivers these capabilities while keeping tea recognizably tea.
