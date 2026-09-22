# Tea: final clean redesign of a small extensible agent runtime

## 1. Mission, authority, and settled decisions

Implement this redesign through working code, updated callers, tests, documentation, and the authorized verification below. Do not stop at a design document, scaffolding, or an experimental second implementation.

Tea remains a **small extensible agent runtime**. Borrow Pi's useful separation of authoritative session state, model context, presentation, effects, and extension lifetime. Do not turn Tea into Pi's general durable runtime or OpenCode's application platform. Architectural similarity is not an objective when it adds machinery without a Tea use case.

These decisions are final:

- In-process Rust APIs only. No daemon, server, sockets, attach/reconnect system, remote clients, general service framework, or new bidirectional stdio/RPC protocol. Preserve useful existing one-shot and read-only export output, but do not create a control protocol.
- A small, fixed agent execution model, with owned concurrent child runs. No public arbitrary durable-task registry, workflow engine, task dependency language, or resumable custom Luau jobs.
- Reopening restores committed state and explains interruptions. It performs no inference or tool execution and does not restart goals or children. Resuming work requires an explicit user or host action.
- Incomplete streaming text and tool-progress previews may be transient and lost after a crash. Completed semantic results must be committed before they are acknowledged as complete.
- User-facing branching is limited to recorded settled user-turn boundaries. No arbitrary mid-exchange rewind or general historical document database.
- Luau extends existing prompts, tools, hooks, bounded state, commands, and continuation policy. It does not define the execution kernel.

This prompt replaces all earlier redesign prompts and conflicting product plans, including requirements for durable documents/tasks/services, fully durable progress, IPC, and daemon-ready abstractions. Do not implement an earlier requirement unless this prompt retains it.

There is **no compatibility requirement** for Rust APIs, CLI/config syntax, extension ABI, internal types, or storage format. Update every in-repository consumer and delete superseded implementations. No compatibility aliases, migrations for retired formats, legacy modes, or parallel old/new engines. Clean break does not mean rewriting correct code merely to rename it.

Never delete or overwrite the user's existing sessions, credentials, home-directory data, unrelated work, or uncommitted changes. Use fresh explicit data directories and disposable workspaces for development. Reject incompatible data non-destructively. Give any incompatible replacement format an unambiguous format identity and version 1; an old format also numbered 1 must not be accepted accidentally.

Read `AGENTS.md` and relevant nested instructions. Resolve implementation details autonomously within these decisions, choosing the smaller design. Do not reopen the settled questions. For genuine environmental blockers, finish independent work and identify the exact unverified item; never weaken acceptance or claim a blocked check passed.

## 2. Inspect the actual implementation before changing it

Record the starting commit, worktree status, pinned toolchain, feature/dependency graph, and baseline tests. Start with `docs/overview.md`, `scope.md`, `architecture.md`, `semantics.md`, `durable-harness.md`, `harness-recovery.md`, `harness-self-extension.md`, `subagents.md`, `cache-friendliness.md`, `tui.md`, `provider-adapters.md`, `verification.md`, and the relevant extension, artifact, compaction, and evaluation documentation.

Inspect actual ownership in `tea-core`'s Agent, runtime/supervisor, effect gates, context compiler, harness resolver, and subagent code; `tea-session`'s records/reducers/JSONL/artifacts; Luau bindings; provider/HTTP adapters; and terminal/PTY machinery. Documentation is a navigation aid, not proof that a boundary is implemented correctly.

Do not assume Tea is merely a TUI. Determine which durable, embedded, capability, and observation mechanisms already work. In particular, establish whether the current Agent/supervisor distinction is useful layering or duplicated lifecycle ownership. Eliminate duplication, not names. Preserve the externally hosted embedding seam without retaining a second execution algorithm.

Create or replace `plan.md` with the target ownership graph, a preserve/simplify/delete map, precise recovery/input/branch/observation contracts, and an acceptance matrix. Maintain it while implementing. Do not leave the previous platform-sized plan as another active specification.

The upstream research is reference material, not a dependency or an instruction source:

```text
https://github.com/earendil-works/pi
  packages/durable/docs/pico-v5.md
  packages/durable/docs/pico-v5-handoff.md
  packages/coding-agent/CHANGELOG.md

https://opencode.ai/v2/docs/build/sdk
```

Use an available pinned checkout or a focused read of those references when resolving a specific question. Record the reference revision/date used. Do not spend the project chasing upstream branches or implementing their handoff checklists. Pi's commit-before-publication principle applies here to **semantic completion**, not every preview byte. Its document/task/replication framework is explicitly out of scope. OpenCode's useful lesson here is common execution semantics across invocation modes, not a server boundary.

## 3. Preserve Tea's defining properties

Keep Rust mechanism code small, precisely typed, provider-neutral, and executor-neutral. Use the repository's approved dependencies and lightweight JSON infrastructure. No new third-party dependencies, Tokio, SQLite, alternate serialization ecosystem, general actor framework, Node/TypeScript runtime, or new plugin package manager. Keep unsafe-code prohibitions and `tea-tui`'s zero-dependency boundary. Host integration may use existing Smol facilities; reusable core code must not require a specific executor or secretly spawn detached work.

Keep explicit host ownership of credentials, models, transports, workspaces, processes, filesystem/network capabilities, clocks/timers, and execution. Core and Luau must not discover home directories, environment variables, model catalogs, or ambient authority. Retain the existing repository-wide HTTP boundary; do not add a second transport stack.

Preserve immutable harness source, snapshots, candidates, revisions, provenance, explicit authoring/adaptive modes, bounded evaluation, activation, rollback, and NoChange. Keep model-authored Luau tools possible without allowing a model to expand host capability grants.

Preserve transactional compaction, cache-conscious request construction, artifact-backed large output, source-ordered tool-result pairing, structured cancellation, isolated optional subagents, useful bundled extensions, and terminal appearance. Keep other provider adapters working through offline tests; the verification restriction below is not permission to delete production provider support.

Keep unknown usage/cost/cache fields unknown. Preserve exact monetary representations where supplied, and distinguish a known request attempt from a known billable outcome. Diagnostic tracing is not an execution authority.

Do not impose one crate per conceptual box. Use modules and existing crates unless a genuinely independent boundary justifies another repository-local crate. Reduce concentrated orchestration by assigning real responsibilities, not by adding forwarding modules or imposing arbitrary line-count limits.

## 4. Target ownership and execution model

Implement these responsibilities; exact names may follow the repository:

```text
TUI / one-shot CLI / Rust embedding
                 |
       direct runtime operations
                 |
      fixed agent execution model
      + owned optional child runs
      + optional Luau customization
                 |
        semantic commit boundary
                 |
     JSONL + immutable artifacts
     or explicit host persistence
```

A session owns committed history, configuration/harness selection, accepted inputs, extension state, and effect/outcome records. Each conversation/lane permits at most one active run. Root and child runs may execute concurrently, but session mutations have one serialized commit order.

The agent FSM owns request preparation, streaming, tool preparation/execution, continuation, compaction, and settlement. These may have private exhaustive Rust states. They are **not** public task kinds. All invocation modes and child runs use the same FSM and effect rules. Keep a lower-level Agent and durable coordinator when that cleanly separates mechanism from persistence; do not maintain competing transitions in both.

The host owns polling/spawning and joins through narrow existing ports. An existing `TaskRuntime` abstraction for executing owned futures is acceptable; a workflow scheduler with kind registration, dependencies, persisted coroutine stacks, or arbitrary phase handlers is not.

Provide a coherent Rust surface for create/open, submit, continue, cancel, withdraw queued input, fork at a valid boundary, inspect/snapshot/subscribe, and close. Reuse useful current operations rather than creating a mandatory universal command enum. A Rust method need not be serializable.

Preserve embedding with caller-owned persistence through explicit commit/effect ports and the same execution path. Memory-backed examples/tests are allowed. Do not invent an external-storage capability framework or silently pretend that in-memory execution is crash durable. Unsupported combinations must fail explicitly.

An operation/input result has its own reliable completion mechanism. Print mode and embeddings must not infer their result from a lossy UI subscription or wait for unrelated global idleness.

## 5. One semantic commit boundary, lightweight storage

Retain append-only JSONL and immutable artifacts unless an existing equally small implementation already satisfies the required contracts. Do not build a new database project. Rebuildable indexes/caches remain disposable accelerators, never authority; preserve an existing justified binary cache rather than introducing one speculatively.

Use a small, explicit atomic commit unit for logically inseparable facts. Examples include accepted input plus queue state; a settled tool result plus effect outcome; final answer plus input/run settlement; and harness activation plus selected revision/state. Validate a prospective transition before committing it. Adopt committed state and publish semantic success only after storage succeeds.

One writer serializes mutation and storage settlement. Provider calls, tools, Luau callbacks, and observers execute outside its mutation critical section. Prevent stale invocations from committing after cancellation or settlement. Do not hold ordinary mutex guards across user-controlled asynchronous work.

If an append/flush fails ambiguously, fault that writer and require reopening/verification. Never continue from speculative memory as though persistence succeeded. A consumer callback failure after a successful commit must not make the caller believe that commit failed and should be retried.

Write required immutable objects before publishing references to them. Recovery checks hashes/reachability and distinguishes a torn final write from corruption in a complete committed record. Keep strict repair behavior explicit; do not silently erase malformed history. Prove logical transaction recovery from every relevant partial-write boundary.

Preserve an explicit single-writer ownership rule, including rejection of a second writer to the same durable session. A local file-ownership safeguard is not distributed coordination. Read-only inspection must not activate execution or acquire provider credentials.

Document acknowledgment guarantees for the actual durability mode: an accepted complete append, process-crash recovery, and power-loss durability are not interchangeable. Keep the supported sync policy honest; do not add per-token persistence or claim power-loss guarantees from SIGKILL tests.

Canonical data is sufficient to reconstruct the session after deleting disposable indexes. Retain useful inspect/dump/verify/export/artifact behavior and update it to the new contracts. No document sidecar/incarnation protocol, general schema-migration framework, or extra event journal is required.

## 6. Effects, interruption, and explicit continuation

Persist an effect's intent before invoking it, with stable identity, owning run, pinned harness/tool policy, and the arguments or artifact references required for honest reconciliation. Commit its outcome before exposing successful completion. Reuse suitable existing effect gates and identities.

A complete validated assistant tool-call decision must be committed before its tools execute. Incomplete streamed tool arguments are previews, not executable calls. Results may finish in any permitted execution order, but model context pairs them with the originating calls in source order.

Model the recovery distinctions explicitly:

- An intent conclusively not invoked is different from an invocation with an unknown outcome.
- A committed result is never executed again merely because a reply or observer notification was lost.
- A replay-safe operation may be repeated only after execution is explicitly resumed and current authority permits it.
- A genuinely receiver-idempotent effect may reuse a persisted key only where deduplication or result lookup is actually supported.
- An ambiguous shell command, file mutation, workspace apply, or other non-idempotent effect is interrupted/indeterminate until reconciled. Missing output does not prove it did nothing.

Do not claim exactly-once external execution. Preserve stronger existing transaction/reconciliation capabilities where they exist. Cancelling a future or killing a process does not establish that no side effect occurred.

Opening a session reconstructs committed state and a recovery report. It must not poll a model, invoke a tool, continue a goal, restart a child, apply changes, or infer permission to resume from a stored `active` flag. Restoration itself should not require working provider credentials. Unsafe corruption blocks executable continuation, not honest read-only diagnostics.

Expose one explicit continuation path for the user/host. It consults committed outcomes, normalizes interrupted exchanges into provider-valid context, and either proceeds safely or reports the specific unresolved effect requiring reconciliation. A generic Continue action is not permission to rerun an indeterminate effect. New attempts get distinct identities; partial output from an interrupted attempt must not be concatenated with a new answer. A new submitted prompt may authorize continuation, but it must pass the same recovery gate rather than bypassing unresolved effects.

Already accepted queued inputs remain visible after reopen but are not dispatched automatically. Goals retain their objective/state without acquiring execution permission. Unfinished children remain inspectable; no automatic child restart. Existing retained child results and deltas remain available without rerunning the child. Do not add checkpoint-resurrection machinery for unfinished children: reconcile their effects/workspaces, retain a truthful interrupted outcome, and require an explicit new child assignment for further work. Never turn a replayed old spawn intent into a duplicate child.

Keep normal bounded retry/goal behavior within an explicitly started live run where current policy permits it; explicit restart does not prohibit ordinary in-process operation. Failures and cancellations must not trigger unsolicited continuation.

Cancellation prevents further admission for the cancelled operation, propagates to owned provider/tool/child work, joins it, and settles exactly once. Stale updates cannot resurrect a finished run. Once a non-cancellable storage or workspace-apply boundary starts, finish/classify that boundary before reporting cancellation. Exiting the application joins its owned work; it never detaches agent jobs to survive invisibly.

## 7. Immutable history, model context, and settled-turn forks

Separate immutable recorded history, derived model context, and disposable presentation. Never use TUI AppState or a mutable message vector as the canonical session.

Use append-only records for context omissions/replacements, summary boundaries, and configuration/harness changes where needed. Preserve raw messages/results and artifact references. A failed, invalid, stale, or cancelled compaction leaves prior effective context intact. Large tool output is bounded for the model without destroying its original inspectable artifact.

Make effective system instructions, ordered sections, tool declarations, and selected model/harness state reconstructible at request preparation. Record exact rendered prompt/tool material or immutable references sufficient to reproduce it. Apply changes at defined safe boundaries, not halfway through a request. Do not add an elaborate system-message protocol when existing immutable revision records and a single compiler express the same semantics.

Filter model-less notices, private extension/child state, interrupted previews, and incompatible provider-specific continuation data explicitly. Preserve tool-call/result pairing and stable ordering. Keep cache-stable prefixes free from timestamps, temporary worktree names, UI state, and incidental IDs. Retain per-lane cache/projection state where justified.

Define a user-facing fork anchor as a recorded checkpoint after a complete logical user turn/run settles, not merely after one provider response in a tool loop. The anchor must have no unresolved provider/tool exchange or unjoined work owned by that operation. Reject invalid anchors before creating a fork. Do not support arbitrary entry-ID or mid-tool forks through another public API.

A branch inherits history through that anchor and the effective configuration, harness revision, and extension-state values captured at the same boundary. Later parent changes must not leak into it. Use existing history plus a small checkpoint or bounded snapshots; no configurable per-document history/fork policy system.

Do not inherit queued input, executable handles, live child ownership, outstanding effects, or permission for autonomous goal execution. Create fresh execution identities. Branches diverge independently. Branching conversation history does **not** roll back workspace files; state this in API/CLI documentation and do not build filesystem time travel.

Preserve subagent context inheritance as a distinct internal contract: a child may inherit the exact parent request source already supported by Tea, without exposing that source as a new user-facing rewind feature.

## 8. Small extension state and immutable Luau evolution

Keep native runtime state in typed Rust records. Provide each extension one bounded conversation-local namespaced state value, committed through the semantic boundary. It may contain an ordinary map when needed. Do not add Session/conversation/task document kinds, families, general historical queries, watcher APIs per value, or JSON structural-patch machinery.

Persist state changes as ordinary facts/whole bounded values and include the appropriate value at settled-turn fork checkpoints. Give state and output explicit existing or documented limits; large content belongs in artifacts. Do not serialize all extension/session state on every progress tick.

Extension state is private unless explicitly projected into a prompt or presentation. An extension cannot read or mutate another namespace, runtime queues, effect outcomes, selected authority, or raw storage. Validate Luau-supplied values and retain owned data rather than VM-backed references escaping their lifetime.

Use stable extension identity plus pinned executable revision. Incompatible state changes must fail candidate validation or require an explicit bounded state replacement in the activation transaction. Do not silently reset state, reinterpret it, or implement a migration framework.

Preserve the complete immutable authoring path: stage source, validate a closed bundle and capability ceiling, retain candidate/evidence, evaluate within explicit limits, choose activation or NoChange, activate only at a safe boundary, and retain inspectable rollback lineage. Rollback selects known source/revision/state explicitly; it does not undo external filesystem effects.

An active invocation stays pinned to its prompt, tool schema, execution policy, and grants. Old callbacks cannot write after their generation closes. Activating Luau source is not general host-code hot reload. No arbitrary Rust service replacement, dynamic native loading, persisted VM stacks, or concurrent old/new ownership of one run.

Port bundled coding tools and useful goal/todo/web extensions through this surface. Keep any network authority no broader than before. Demonstrate a small sandboxed helper tool plus bounded state and a command/hook using public extension APIs, without modifying the central runtime for that extension. It must not be a custom resumable job.

## 9. Runtime-owned inputs, goals, and existing subagents

Unsubmitted composer text is local. Once an input is accepted, the runtime owns its stable identity, committed payload, queue placement, and eventual result. Admission and completion are distinct. Return an awaitable/queryable result handle without introducing a transport request/replay protocol.

Move accepted next-message queues and goal continuation decisions out of terminal code. Preserve ordinary Enter-as-follow-up behavior and any already supported steering semantics at valid boundaries. Do not add Pi's entire queue-mode surface. Document ordering among user input, pause/clear controls, and goal continuation; explicit user control must take precedence over automatic continuation.

The existing combined next-message UI may remain a projection. If multiple accepted inputs are combined for one dispatch, record their membership and settle them consistently. Editing/withdrawing that slot must atomically withdraw still-unplaced input before returning text to the local composer. It must not erase accepted history or withdraw work already dispatched. No queued input may be stranded by terminal/error paths.

A goal may request another ordinary run only after the previous run settles and current explicit execution authorization/budgets allow it. Apply pending pause/clear controls before this decision. No TUI callback, wall-clock scheduler, or persisted `active` value may be necessary to make the behavior correct. Prove pause and continuation from a headless embedding.

Preserve the existing optional, bounded root-to-child subagent model. No grandchildren, teams, mailbox, unsolicited child injection, detached jobs, generic dependency graph, or broader model authority. Disabled subagents add no collaboration tools, provider lookup, prompt changes, or UI noise.

Children use the same engine with isolated workspaces, independent context/compaction, pinned provider/harness choices, and owned cancellation/join. Preserve exact source provenance, durable spawn identities, capacity bounds, reports, and explicit result retrieval. Parent context receives child content only through the established explicit result boundary.

Applying a child delta remains explicit and transactional with truthful Applied/Conflict/RolledBack/Indeterminate classification. Preserve the parent's index and unrelated files. Reopen must not auto-apply, duplicate a spawn, or discard unresolved workspace evidence. Retain one shared writer, not a separate child persistence system.

Prove the existing maximum supported concurrency deterministically, including 16 children when enabled by policy. Keep live-model concurrency much lower as specified below.

## 10. Transient previews, observation, and terminal preservation

Provide a consistent snapshot plus ordered typed in-process updates. Atomically capture the snapshot and register the observer so completion cannot fall between them. A snapshot may combine committed state with explicitly labelled bounded live previews; preview state is not restored as semantic truth after a crash.

Distinguish committed semantic batches from coalescible preview updates. Assistant deltas, tool-progress previews, and animations do not require JSONL writes. Final semantic results supersede their previews atomically at publication. Fence previews by run/attempt/tool identity so delayed output cannot revive completed or replaced work.

Do not clone/serialize a growing transcript for every token. Use bounded preview buffers and incremental typed updates, not a general JSON patch algebra or immutable replicated-document subsystem.

Bound subscriber queues. Coalesce intermediate previews. If a consumer falls behind on semantic updates, signal lag explicitly and require a fresh snapshot or close that subscription; never silently present an incomplete stream as complete. Commit sequence filtering can create legitimate gaps. Reliable operation completion remains independent of this channel.

Run observers outside state/writer locks. Slow, throwing, reentrant, or dropped consumers must not deadlock execution or change an already committed outcome. No unbounded supposedly lossless queue, replay journal, or subscription-acknowledgment framework. Retain diagnostic tracing under an explicit failure policy rather than making the TUI the trace owner.

Keep `tea-tui` zero-dependency and terminal OS integration in the application host. Preserve native main-screen scrollback, a once-emitted stable prefix, bounded mutable live tail, modal-only alternate screen, styling, geometry, input/history/search, pickers, tool detail, and status behavior.

Use stable entry identities and a projection-local frontier to prevent duplicate scrollback after resnapshot. Branching/resuming may start an explicit fresh projection; neither rewrites already emitted terminal history. Switching the active conversation must reject busy state or cancel/join owned work before switching; it must not hide a still-running operation behind another screen. Treat untrusted model/tool/extension text as content, not terminal control authority.

Use existing PTY tests as the visual oracle. Add narrow tests for changed queue/recovery/branch semantics and byte-level scrollback/modal/resize behavior. Do not regenerate goldens merely to accept regressions. No new full-screen layout or broad UI redesign.

## 11. Live verification: OpenCode Zen free models only

All actual inference initiated for verification must use **OpenCode Zen models currently verified to be free**. This covers root runs, children, compaction, summaries, candidate evaluations, optional comparison agents, and any model-based judge. No OpenRouter, direct Meta, local inference, paid Zen variants, ChatGPT/Codex subscription calls, or hidden provider fallback.

Prefer **Muse Spark 1.3 Free**. Treat `muse-spark-1.3-contributor-free` as a candidate catalog ID to verify, not as a guaranteed current identifier or permanent price claim. Check the official current Zen catalog/documentation for the exact ID, supported wire API, availability, and applicable input/output/cache charges before any live request. Missing pricing is not proof of zero. Do not substitute an ordinary similarly named paid model.

Record the checked source/date and selected provider/model/endpoint in verification evidence. A different explicitly verified-free Zen model requires a deliberate recorded test configuration, not automatic substitution. Uncertain status or an unavailable compatible free model blocks live tests.

Implement the restriction in **verification infrastructure**, not as a new production pricing subsystem. Construct a restricted provider/factory once and inject it into every test-run model consumer. Validate exact identity and approved endpoint before inference transport; reject cross-origin redirects and alternate-provider selection. Ensure child/compactor/evaluation factories cannot bypass it. A `free=true` flag or model-name substring check is insufficient. Offline tests must prove the restriction rejects paid/unknown/mismatched routes without sending inference.

Audit evaluation scripts and Make targets before invoking them. Their existing defaults are not authorized. Do not run a paid shootout, start a local model server, download model weights, or invoke upstream CLIs with uncontrolled auxiliary inference. Other provider adapters get deterministic offline protocol tests only.

Use only explicitly supplied credentials through the existing host secret mechanism. Never inspect another application's credential store, print keys, put them in argv, expose them to fixture subprocesses/Luau, or persist them in traces. Do not modify accounts, billing, or subscriptions.

Use disposable synthetic or deliberately public fixtures and synthetic harness source. Do not send the user's private checkout, actual session logs, home files, notes, or secrets to free-model verification. Check the selected route's data-use terms; free-tier availability is not permission to disclose private data. Keep raw reports/workspaces outside the source tree and retain only intentional sanitized evidence.

Use one aggregate live-verification budget across the entire task: at most 40 attempted inference requests, 100,000 requested output tokens, 30 minutes of live-suite wall time, and two concurrent requests. These are maxima, not targets. Start serially. Count retries and all auxiliary calls; reserve each request's output allowance before sending. Existing stricter configured limits win. Budget exhaustion is a reported blocker, not permission to reset the counter.

Deterministic scripted providers, fake clocks, and loopback protocol fixtures are required and are not real inference. Missing credentials, rate limits, service failure, or uncertain free status must produce honest BLOCKED live checks while independent offline work continues. Do not substitute a mock and label it live success.

## 12. Required verification and completion evidence

Use the pinned repository toolchain and existing approved test facilities. Add focused failing regressions before changing behavior. Prefer deterministic schedules, injected faults, counters, and fake clocks over sleep-heavy races. Keep a runnable end-to-end slice throughout the redesign.

The acceptance matrix must cover:

**Execution and ownership.** One run per lane; consistent embedded/TUI/one-shot behavior; source-ordered tool results; offered-schema/capability checks; cancellation at preparation/stream/tool/commit boundaries; no late updates; joined child/process cleanup; reliable own-input completion; goal control without the TUI; no duplicate execution path.

**Persistence and restart.** Shared memory/file contract tests; atomic mixed semantic commits; missing/corrupt objects; every relevant partial append/object-publication boundary; poisoned writes; explicit torn-tail repair; second-writer rejection; repeated replay after deleting indexes; and actual subprocess kill/reopen tests. On open and after an idle delay, assert zero provider/tool/child/apply calls. Then explicitly continue safe work and prove committed effects are not repeated and ambiguous unsafe effects are not blindly replayed.

**Context, forks, and extensions.** Raw-history preservation; context edit/compaction success and failure; provider-valid tool pairing after interruption; cache-stable prefixes; valid and invalid turn anchors; exact historical harness/config/state at a fork; no inherited execution/queue authority; distinct child-inheritance semantics; namespace isolation; incompatible candidate state; bounded authoring/evaluation; activation/rollback/NoChange; stale-generation rejection; and a useful Luau helper through public APIs.

**Observation and presentation.** Snapshot/subscription races; preview coalescing/loss; committed completion after previews; slow/throwing/reentrant observers; explicit lag and resnapshot; no duplicated settled scrollback; existing PTY appearances; modal restoration; resize; feature-disabled identity; and control-sequence safety.

**Subagents and resources.** Deterministic supported concurrency; strict model/capability ceilings; isolated worktrees; exact spawn provenance; explicit reports/apply; conflict/rollback/indeterminate recovery; no automatic restart; bounded buffers; binary/dependency deltas; idle/startup resources; replay scaling; and large-history streaming without per-token whole-session copies or persistence.

Run the applicable existing offline gates, adapting names only when cleanly replacing their implementations:

```sh
cargo test --workspace --locked
cargo test -p tea-agent --features pty-harness --test pty_streaming --locked
./crates/tea-core/fixtures/run.sh
python3 scripts/check-crate-graph.py
git diff --check
```

Audit commands for hidden inference before running them. Exercise relevant optional-feature builds, including Zen, and verify prohibited dependencies have not appeared. Respect repository instructions against routine formatters, linters, pre-commit hooks, and pushing. Do not run `make lint` or copy upstream npm checks.

Verify natively on the available macOS host and through the repository's Linux AArch64 Docker path when available. Distinguish executed tests from cross-compilation and unavailable platforms. Compare resource measurements using identical toolchain/features/fixtures; report regressions rather than claiming minimalism from line counts alone.

The minimum guarded real-model suite is:

1. A synthetic coding task through Tea: read, edit, execute fixture tests, and check actual files/exit status.
2. A direct headless Rust invocation and a separate one-shot CLI invocation using the same engine and reliable own-input completion. No IPC component.
3. A controlled interrupted run, reopen with zero automatic inference, explicit safe continuation, and committed-state verification. Use deterministic fault tests, not risky live effects, to cover indeterminate execution.
4. Forced compaction of bounded synthetic history followed by a task with an executable oracle for retained critical facts.
5. A small synthetic Luau candidate that is validated, activated at the correct boundary, used, and rolled back, with actual state/source checks.
6. One or two small isolated child assignments with explicit report retrieval and parent apply.

A deterministic counterpart of each case is mandatory. Retain every attempted live result, including model mistakes and service errors. Do not retry until success and discard failures. A live pass demonstrates integration, not exhaustive reliability or superiority over another harness. Mark unachieved live gates separately from implemented offline behavior.

## 13. Implementation sequence and final deliverables

Implement in this order unless an actual dependency requires a documented adjustment:

1. Baseline, preservation/deletion map, exact contracts, free-only verification factory, and regression tests.
2. Semantic commits, effect ownership, recovery reports, and explicit-continuation behavior.
3. Consolidated fixed run lifecycle, durable input ownership, headless goal control, and a working scripted end-to-end run.
4. Context/fork checkpoints, bounded extension state, immutable Luau evolution, and preserved subagents.
5. Snapshot/typed updates, transient previews, embedding/CLI/TUI wiring, and visual tests.
6. Removal of superseded code/plans, documentation/caller updates, full offline/platform/resource checks, and the guarded live suite.

Do not leave TODO implementations, hidden second engines, compatibility shims, or mandatory acceptance items labelled future work. Conversely, do not add deferred platform features as empty traits, unused schema fields, reserved protocol enums, or daemon-ready adapters. Out of scope means absent.

Update architecture, semantics, recovery, extension, subagent, TUI, quickstart, verification, fixture/evaluation entry points, and relevant AGENTS routing so there is one current contract. Preserve useful regression evidence even when its old API disappears.

Deliver working code and a final report with: the responsibility graph; retained/simplified/deleted components; any new format/ABI identity; direct Rust and CLI usage examples; the acceptance matrix with actual commands/results; platform/resource/dependency comparisons; sanitized free-model identity/budget/attempt evidence; and every remaining blocker stated precisely.

Success is not adding Session/Task/Document/Service types. Success is **less overlapping machinery, one clear execution and commit story, explicit safe recovery, useful constrained extensibility, preserved Tea behavior, and no platform infrastructure that Tea does not need**.
