# Tea runtime redesign: implementation and acceptance ledger

This is the current implementation contract, replacing the original redesign
prompt. Pending and blocked checks are never passes.

## Baseline

- Starting commit: `6e157cd181646e4a1adfa771c72687235a112955`; clean worktree.
- Pinned toolchain: the single `rust-toolchain.toml` channel, rustc
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
- No user sessions, unrelated work, formatters, linters,
  hooks, commits, pushes, or uncontrolled inference may be touched.
- The user explicitly supplied `~/.codex/auth.json` for this lane and clarified
  that Tea's Codex provider must use the existing Codex login. Tea reads its
  access token without copying or rotating the client refresh token. Live
  reports, ledgers, and synthetic workspaces remain outside the repository.
- Upstream Pi references are advisory; no upstream implementation imported or
  relied upon. The live lane now uses the user-selected native Codex provider,
  `gpt-5.6-luna`, and low reasoning effort after the backend rejected the
  original `gpt-6-luna` choice for this ChatGPT account. Its exact model record and request
  ledger are tracked in verification evidence; account access is established
  only by a real request.

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
| Execution | One run/lane, same engine across modes/children, source ordering, schema/grants, cancellation boundaries, no late updates, joins | PASS: workspace suite; cooperative-cancellation fork close/join (`closing_the_supervisor_cancels_and_joins_a_live_fork_lane`) |
| Inputs/goals | Durable queue/membership/withdrawal, own completion, headless pause/continue, terminal errors | PASS: queue/control/completion tests; TUI user rows projected on dispatch, idle re-drive holds no task, live-turn queueing not gated as recovery |
| Storage | Shared memory/file atomic batches, missing/corrupt objects, every partial write/publication, poisoned writer, repair, second writer | PASS: `tea-session --lib` 79 (74 + 5 ignored resource fixtures run separately); host header check uses `SESSION_HEADER_KIND` |
| Restart | Subprocess kill/reopen, zero calls on open/idle, explicit safe continuation, no repeated committed effects, unsafe gate, no child restart | PASS: SIGKILL tests; explicit continuation restores committed spawn/apply results without host effects (`child_outcome_recovery_tests`) |
| Context | Raw preservation, compaction success/failure, interruption pairing, reproducible requests, stable prefixes | PASS: compaction encoder matches durable reconstruction (host `failure`, unreported usage); ungated provider strategy fails closed |
| Fork/state/evolution | Valid/invalid anchors, historical selection/state, no inherited execution, bounded namespace/state compatibility, author/evaluate/activate/rollback/NoChange, stale generation, helper | PASS: generation guard (revision-only writer removed); ABI v3-only bundles; committed state equals reopen decode; authoring ceiling admits seeded grants; scripted evolution activate/use/rollback/reopen |
| Observation | Snapshot race, preview coalescing/loss/fences, committed completion, slow/reentrant/throwing/dropped observers, explicit lag/resnapshot, bounded memory | PASS: non-vetoing observers; fenced tool previews keep a content-free activity remnant |
| Terminal | Existing PTY oracle, queue/recovery/fork, stable scrollback, modal/resize, disabled identity, control safety, busy switch | PASS: PTY 11/11 (run 3x); shared PTY lock no longer poisons later scenarios |
| Children | Deterministic 16 concurrency, ceilings/provenance, worktrees, explicit report/apply, conflict/rollback/indeterminate, no restart | PASS: 16-child test; per-lease Git serialization (`concurrent_finalizations_of_one_lease_serialize_to_one_delta`); one-shot cleanup 12/12 (was flaky) |
| Workspace | `cargo test --workspace --locked` | PASS: 731 passed, 0 failed, 7 ignored |
| PTY | `cargo test -p tea-agent --features pty-harness --test pty_streaming --locked` | PASS: 11/11 |
| Fixtures | `./crates/tea-core/fixtures/run.sh` | PASS: 29/29 (`awaited-agent-end-observer` fixture deleted with its superseded API) |
| Boundaries | `python3 scripts/check-crate-graph.py`, optional features, prohibited deps, `git diff --check` | PASS: crate graph; prior all-target feature checks; focused `tea-core` lib 214/214, `tea-providers` Codex lib 67 passed/1 ignored, current `tea-agent` live-verification lib 205 passed/1 ignored, example 3/3, Python audit 5/5, entrypoint audit, diff check; no new dependencies. The child HTTP fixture deadline was widened from 5 to 15 seconds after it failed only under broad test load |
| Platform/resources | Native macOS and Linux AArch64 Docker; identical binary/startup/idle/replay/large-history fixtures before/after | PASS: `make test-linux` exit 0 (workspace + PTY 11/11). Release baseline -> current: binary 8,356,288 -> 8,885,648 B (+6.3%); idle RSS 8,432 -> 8,480 KiB; `--version` 8.5 -> 7.8 ms. Long-history fixtures: JSONL +5-7%, replay 114 -> 129 ms (10k), 393 -> 446 ms (27k), single runs |
| Live guard | Exact `codex/gpt-5.6-luna` and low reasoning, explicit installed Codex or Tea-owned credential path, injected restricted factory, offline no-network rejects, request ledger | PASS: client access token loaded read-only; v2 ledger retained all 144 attempts, including ten rejected `gpt-6-luna` attempts. Final report: `/tmp/tea-codex-live.nLMg68/report-recheck.json`; ledger: `/tmp/tea-codex-live.nLMg68/ledger.json` |
| Live coding | Synthetic read/edit/test actual files/exit oracle and deterministic counterpart | PASSED live and offline in final six-case run |
| Live invocation | Headless Rust plus separate one-shot CLI, same engine and own completion, deterministic counterparts | PASSED live and offline in final six-case run |
| Live recovery | Controlled interruption, passive reopen, explicit safe continuation, committed-state oracle, deterministic counterpart | PASSED live and offline in final six-case run. A typed cancellation is the expected interruption settlement; passive reopen and exact continuation passed |
| Live compaction | Forced bounded synthetic history, executable critical-fact oracle and deterministic counterpart | PASSED live and offline in final six-case run. The threshold admits the synthetic history and leaves room after checkpoint for a follow-up tool turn |
| Live evolution | Synthetic Luau validate/activate/use/rollback with real source/state checks and deterministic counterpart | PASSED live and offline in final six-case run. Exact nested `tea_harness` schema and explicit retry of a rejected todo row preserved source, state, rollback, and reopen oracles |
| Live children | One/two isolated assignments, explicit reports/parent apply and deterministic counterpart | PASSED live and offline in final six-case run |

All verification inference (children, compaction, evaluations and retries too)
must use the user-selected `codex/gpt-5.6-luna` route at low reasoning effort.
Unavailable model access or a missing explicitly supplied Codex credential
is BLOCKED, never a model fallback. Use only the explicit credential path and
synthetic/public fixtures; raw reports stay outside the source tree. Aggregate
ledger: reserve before sending and retain every attempt, including failures.
The former 40-request, 30-minute, and two-stream task limits were removed by
user direction; the existing ledger remained the aggregate audit record for
continued runs. The Codex subscription wire has no reliable output-token
request cap, so no such cap is claimed. The final 2026-09-24 live report is
`PASSED`: all six live cases and all six deterministic counterparts passed
using the user-selected model and installed Codex credentials. The aggregate
ledger records 144 attempts across failed and successful runs, with no reset.

## Final deliverables

Updated working code/callers/docs (architecture, semantics, recovery, extensions,
subagents, TUI, quickstart, fixtures/evals, AGENTS routes); exact format/ABI
identity; direct Rust/CLI examples; completed matrix with actual commands/results;
platform/resource/dependency comparisons; sanitized model/budget/attempt
evidence; precise remaining environmental blockers. Mandatory implementation
cannot be relabelled future work. A live pass demonstrates integration only.
