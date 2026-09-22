# Tea runtime redesign: implementation and acceptance ledger

This is the current implementation contract, replacing the original redesign
prompt. Pending and blocked checks are never passes.

## Baseline

- Starting commit: `6e157cd181646e4a1adfa771c72687235a112955`; clean worktree.
- Pinned toolchain: `nightly-2026-09-15`, rustc
  `1.100.0-nightly (574ff7d98 2026-09-14)`.
- Native macOS AArch64; Docker reports Linux AArch64 available.
- Baseline `cargo test --workspace --locked`: PASS (2026-09-21);
  full log: `/tmp/tea-redesign-baseline-tests.log`.
- Baseline crate graph: PASS. Nine existing crates, no new dependencies;
  `tea-tui` remains zero-dependency. Smol stays at host/test boundaries.
- Baseline PTY: FAIL (3 passed, 7 failed); the first enabled-subagent fixture
  received no expected provider request, then six tests hit its poisoned shared
  mutex. Log: `/tmp/tea-redesign-baseline-pty.log`.
- Baseline declarative fixtures: 29 passed, 1 failed. The stale
  `length-stop-no-tools` script expected settlement despite the core's tested
  continuation rule. Its script/golden now exercise the intended second turn.
- No user sessions, credentials, home data, unrelated work, formatters, linters,
  hooks, commits, pushes, or uncontrolled inference may be touched.
- Upstream Pi references are advisory; no upstream implementation imported or
  relied upon. Official Zen source/date/free-route evidence is tracked in the
  verification report; uncertain free status blocks live inference.

## Responsibility graph

```text
TUI / one-shot CLI / Rust embedding
    | direct create/open/submit/continue/cancel/withdraw/fork/inspect/close
    v
SessionSupervisor: accepted input, serialized commits, immutable selection,
                   explicit recovery, owned optional root-to-child runs
    |                         |
    |                         +-- host TaskRuntime owns polling and joins
    v
Agent: one fixed request/stream/tool/compaction/continuation/settlement FSM
    | immutable resolved harness + host effect/commit ports
    v
tea-session: atomic semantic commit -> JSONL + immutable artifacts
    |
    +-- derived model context (not mutable history)
    +-- snapshot + bounded typed updates -> disposable UI projection
```

`HostedEpoch` preserves caller-owned persistence through the same Agent and
explicit effect ports; unsupported policy/durability combinations fail.
Supervisor owns persistence/operation coordination, not a second agent FSM.
No daemon, server, RPC, workflow registry, document database, migration system,
detached work, per-token journal, native hot reload or second HTTP stack.

## Preserve / simplify / delete

| Component | Decision |
| --- | --- |
| Agent and executor-neutral effect/provider/tool ports | Preserve one fixed execution algorithm |
| Supervisor / HostedEpoch | Preserve distinct persistence/embedding ownership; assign real module responsibilities |
| JSONL / artifacts / repair / writer ownership | Preserve; add mixed atomic semantic batches and reject incompatible old format |
| Harness source / snapshots / candidates / revisions / provenance | Preserve immutable authoring, bounded evaluation, activation, rollback, NoChange |
| Luau and capability ceilings | Preserve; simplify each extension to one bounded private whole value |
| Raw history / large-output artifacts | Preserve; append-only context edits/compaction derive effective context |
| Bounded root-to-child runs / isolated worktrees / explicit apply | Preserve one shared writer and same engine |
| Child resurrection | Delete; retain interruption/report/workspace evidence; further work requires a new assignment |
| TUI accepted queue / goal scheduler | Replace with durable runtime input membership and headless control precedence |
| Unbounded lossless observation / silent semantic loss | Delete; bounded coalescible previews and explicit lag/resnapshot |
| Arbitrary user-facing entry forks | Replace with recorded settled-turn checkpoint anchors |
| Terminal appearance / scrollback / modal / history | Preserve; stable committed identities drive projection frontier |
| All provider adapters / exact optional accounting | Preserve; other routes tested offline only |
| Compatibility aliases / parallel formats / superseded plans | Delete; reject incompatible data non-destructively |

## Exact contracts

### Commits and effects

A single writer validates the entire prospective batch before append, then
adopts memory and publishes semantic completion only after storage settles.
Input/placement, provider outcome/answer, effect result, operation/membership
settlement, and activation/state are inseparable where applicable. Required
immutable objects publish before references. Missing/hash-invalid objects fail
closed. Ambiguous append/flush poisons the writer until explicit reopen.
Observer failure cannot reverse a successful commit. Callbacks and external
async work execute outside writer/state locks.

Intent precedes invocation and pins owning run, revision, tool policy and
arguments. Validated assistant tool decisions commit before tools execute;
partial streamed arguments are previews. Concurrent result completion preserves
source-order pairing. A committed decision not yet admitted is conclusively
uninvoked; missing outcome after admission is indeterminate. No exactly-once
claim, invented receiver deduplication, or assumption that cancellation prevented
side effects. Completed outcomes do not replay after lost notification.

Acknowledgments distinguish complete append, process-crash recovery and sync
policy; SIGKILL does not prove power-loss durability. Canonical data alone
reconstructs state after cache removal. A second writer is rejected. Existing
inspect/dump/verify/export/artifact operations remain useful and non-executing.

### Recovery and cancellation

Open reconstructs committed state and a report, with zero inference, tools,
goal continuation, child start or apply, and without working credentials.
Accepted inputs and goal objectives remain visible but do not authorize work.
Incomplete previews are discarded. Corruption blocks execution while honest
read-only diagnostics remain available.

Explicit Continue checks committed outcomes/current grants, marks interrupted
attempts honestly, and makes provider-valid context with fresh attempt IDs.
Replay-safe tools repeat only with current authority and explicit continuation.
Every ambiguous non-idempotent shell/edit/apply effect blocks until explicit
host reconciliation; a new prompt cannot bypass it. Partial answers never
concatenate with new attempts. Children are interrupted, never resurrected;
retained results/deltas stay inspectable, old spawn identity cannot duplicate a
child, and further child work requires a new assignment.

Cancellation closes admission, propagates to owned work, joins and settles once.
Stale callbacks cannot revive a finished operation. A started non-cancellable
storage/workspace transaction is classified before cancellation is acknowledged.
Closing the application joins jobs. Ordinary bounded retry/goal behavior remains
allowed within explicitly authorized live execution; errors do not self-resume.

### Inputs, goals and forks

The composer owns only unsubmitted text. Accepted input has stable durable ID,
payload, queue placement and reliable awaitable/queryable completion independent
of subscriptions/global idleness. At most one run per lane; lanes may overlap
through serialized commits. Combined dispatch records ordered input membership
and settles it together. Withdrawal atomically removes only undispatched inputs
before returning their text. Error/cancel paths neither erase nor strand inputs.

Pending pause/clear controls precede queued user input, which precedes automatic
goal continuation. Goals request ordinary runs only after settlement under
current process-local explicit authorization and budgets. No TUI callback,
wall-clock scheduler or persisted active flag grants execution permission.

User forks select recorded checkpoints after whole logical operation settlement
and joins, with no unresolved exchange. Reject invalid anchors before creation.
Capture exact historical config/revision/private state; inherit history only
through that anchor. Exclude queue, effects, live handles, child ownership and
goal execution authority. Fresh identities diverge independently. Forks never
roll back workspace files. Exact internal child-source inheritance is distinct.

### Context, extensions and observation

Immutable raw history survives context omissions/replacements/summary records.
Failed/invalid/stale/cancelled compaction leaves effective context unchanged.
Large output remains inspectable in artifacts. Exact rendered prompt/tool
material or immutable source is reconstructible at preparation. Apply changes at
safe boundaries. Stable prefixes exclude timestamps/incidental IDs/worktree
paths; lane cache state remains separate. Filter private state, model-less UI
notices, interrupted previews and incompatible provider-specific continuation.

Each extension owns one explicitly bounded JSON value, private unless projected,
with no cross-namespace/storage/queue/authority access. Retain validated owned
values, never escaping VM references. Activation pins executable revision and
rejects incompatible state or atomically replaces it explicitly. Closed
generations cannot write. Preserve bundled coding/goal/todo/web and demonstrate
a public sandboxed helper with bounded state and command/hook. No custom jobs.

Snapshot capture plus subscription registration is atomic. Ordered semantic
batches supersede bounded coalescible identity-fenced previews. No per-token
transcript serialization/copy or persistence. Semantic queue overflow explicitly
requires resnapshot or closes with lag; reliable completion is separate.
Slow/reentrant/throwing/dropped observers cannot alter commits or deadlock.
Terminal stable IDs/frontier prevent duplicate scrollback after resnapshot.
Busy conversation switching rejects or cancels/joins. Model/tool text has no
terminal control authority. Preserve main-screen prefix/live-tail and modal-only
alternate screen, geometry, history/search/pickers/tool detail/status.

## Implementation sequence

1. Baseline, ownership/map/contracts, guarded verification, failing regressions.
2. Atomic commits, effect ownership/recovery reports, explicit continuation.
3. Runtime input/result ownership, headless goals, scripted end-to-end run.
4. Checkpoints, bounded extension state/evolution, preserved subagents.
5. Bounded observation, host wiring, visual regression evidence.
6. Remove superseded paths/docs; offline/platform/resource checks and guarded live
   suite. Dependency-driven overlap is permitted, never waived acceptance.

## Acceptance matrix

| Area | Required evidence | Current result |
| --- | --- | --- |
| Execution | One run/lane, same engine across modes/children, source ordering, schema/grants, cancellation boundaries, no late updates, joins | Focused recovery, arbitrary committed tool subsets, and scripted Rust example PASS; final integration pending |
| Inputs/goals | Durable queue/membership/withdrawal, own completion, headless pause/continue, terminal errors | Queue/control tests 6/6 and completion tests 3/3 PASS; final integration pending |
| Storage | Shared memory/file atomic batches, missing/corrupt objects, every partial write/publication, poisoned writer, repair, second writer | `tea-session --lib`: 74 PASS, 5 explicitly ignored resource fixtures run separately |
| Restart | Subprocess kill/reopen, zero calls on open/idle, explicit safe continuation, no repeated committed effects, unsafe gate, no child restart | Two genuine SIGKILL tests PASS; focused recovery tests PASS; final integration pending |
| Context | Raw preservation, compaction success/failure, interruption pairing, reproducible requests, stable prefixes | Durable compaction/reopen and source-order context regressions PASS; final integration pending |
| Fork/state/evolution | Valid/invalid anchors, historical selection/state, no inherited execution, bounded namespace/state compatibility, author/evaluate/activate/rollback/NoChange, stale generation, helper | Fork/state tests PASS; exact generation guard and ABI3-only migration in progress |
| Observation | Snapshot race, preview coalescing/loss/fences, committed completion, slow/reentrant/throwing observers, explicit lag/resnapshot, bounded memory | Bounded preview/subscription and stream tests PASS; non-vetoing callback refinement in progress |
| Terminal | Existing PTY oracle, queue/recovery/fork, stable scrollback, modal/resize, disabled identity, control safety, busy switch | Baseline FAIL described above; changed queue/recovery/fork tests in progress |
| Children | Deterministic 16 concurrency, ceilings/provenance, worktrees, explicit report/apply, conflict/rollback/indeterminate, no restart | Focused 16-child, atomic spawn/report, interruption and non-resurrection tests PASS; final integration pending |
| Workspace | `cargo test --workspace --locked` | Baseline PASS; final pending |
| PTY | `cargo test -p tea-agent --features pty-harness --test pty_streaming --locked` | Baseline FAIL; final pending |
| Fixtures | `./crates/tea-core/fixtures/run.sh` | PASS: 30/30; rerun after final integration |
| Boundaries | `python3 scripts/check-crate-graph.py`, optional features including Zen, prohibited deps, `git diff --check` | Graph, provider-free entrypoint audit, diff check PASS; final feature integration pending |
| Platform/resources | Native macOS and Linux AArch64 Docker; identical binary/startup/idle/replay/large-history fixtures before/after | Pending |
| Live guard | Exact official free identity/API/endpoint/input/output/cache charges/terms, injected restricted factory, offline no-network rejects, redirect/fallback refusal | Python audit tests 5/5 and Zen redirect tests PASS; Rust feature integration pending; zero inference attempts |
| Live coding | Synthetic read/edit/test actual files/exit oracle and deterministic counterpart | Pending |
| Live invocation | Headless Rust plus separate one-shot CLI, same engine and own completion, deterministic counterparts | Pending |
| Live recovery | Controlled interruption, passive reopen, explicit safe continuation, committed-state oracle, deterministic counterpart | Pending |
| Live compaction | Forced bounded synthetic history, executable critical-fact oracle, deterministic counterpart | Pending |
| Live evolution | Synthetic Luau validate/activate/use/rollback with real source/state checks, deterministic counterpart | Pending |
| Live children | One/two isolated assignments, explicit reports/parent apply, deterministic counterpart | Pending |

All verification inference (children, compaction, evaluations and retries too)
must use one deliberately selected verified-free OpenCode Zen route. Prefer
verified `muse-spark-1.3-contributor-free`; uncertain pricing, unavailable route
or missing explicitly supplied credentials is BLOCKED, never substitution.
Use only injected host secrets and synthetic/public fixtures; raw reports stay
outside the source tree. Aggregate budget: at most 40 attempts, 100,000 reserved
output tokens, 30 minutes, two concurrent requests, starting serially. Reserve
before sending; retain every failure. No reset, hidden auxiliary inference,
private checkout upload, account change, or other-app credential inspection.

## Final deliverables

Updated working code/callers/docs (architecture, semantics, recovery, extensions,
subagents, TUI, quickstart, fixtures/evals, AGENTS routes); exact format/ABI
identity; direct Rust/CLI examples; completed matrix with actual commands/results;
platform/resource/dependency comparisons; sanitized free-route/budget/attempt
evidence; precise remaining environmental blockers. Mandatory implementation
cannot be relabelled future work. A live pass demonstrates integration only.
