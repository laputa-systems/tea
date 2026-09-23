# Verification

Start with the nearest relevant hard judge and broaden only when its evidence
supports the change.

For the durable v1 surface, the focused checks are (`rust-toolchain.toml` is
the single source of truth for the pinned nightly; plain `cargo` resolves it
through the rustup shim):

~~~sh
cargo test -p tea-session --lib --locked
cargo test -p tea-providers --locked
cargo test -p tea-luau --locked
cargo test -p tea-core --locked
cargo test -p tea-agent --lib --locked
cargo test -p tea-agent --features pty-harness --test pty_streaming --locked
cargo test -p tea-agent --features live-verification --lib --locked
cargo test -p tea-providers --all-features --locked
./crates/tea-core/fixtures/run.sh
python3 scripts/check-crate-graph.py
scripts/check-toolchain-pin.sh
git diff --check
~~~

For the default coding builtins, begin with the narrow checks:

~~~sh
cargo test -p tea-core --test coding_capabilities --locked
cargo test -p tea-luau builtins::tests::coding_builtins_are_closed_single_tool_extensions_with_fixed_grants --lib --locked
~~~

They prove the four independent Luau builtins and the trusted workspace,
transaction, search, and process capability boundary before broader harness or
terminal checks.

The fixture command compares every provider-free declarative core case with
its checked-in canonical result. Optional quality-evaluation tooling lives
under `evals/`; live-provider evaluation requires explicit caller
authorization and must write its reports outside the source tree.

Durability work must verify the actual recovery boundary: a session opens from
its v1 log, reachable immutable artifacts rehash correctly, the harness catalog
reconstructs the committed revision, and the terminal can reopen without an
in-memory transcript. Trace and evolution work must additionally verify their
artifact roots and exact evidence spans.

Subagent verification is entirely offline. `tea-session` tests policy and graph
facts, cross-reference corruption, JSONL fixed points, artifact reachability and
unresolved-lease export rules. `tea-core` uses scripted providers plus fake
`SubagentHost` and `TaskRuntime` implementations for concurrent lanes, exact
source-leaf provenance, spawn replay and capacity, event-driven wait ordering,
structured cancellation, report retention, prompt layout and recovery prefixes.
`tea-agent` uses temporary Git repositories for snapshot isolation, binary
deltas, private-index preflight, and `Applied`/`Conflict`/`RolledBack`/
`Indeterminate` classification, and the PTY suite for
feature-disabled visual identity and feature-enabled root-only presentation. No
credential or live inference is part of these checks.

Do not run formatters, linters, pre-commit hooks, or push as part of normal
verification in this repository.

## Guarded free-only live lane

The normal Make targets and `python3 -m evals.quality` commands are
provider-free. Confirm that boundary after changing an evaluation entry point:

~~~sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/check-verification-entrypoints.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest evals.test_live_verification
~~~

The legacy `evals/run-rust-live.sh` command always exits before Cargo or
transport. The generic `evals/controller.py` contract is not an approved live
verification entry point; its checked-in test adapters are provider-free.

The sole proposed live boundary is the feature-gated
`tea_agent::verification::RestrictedZenFactory`. It accepts only
`opencode-zen`, `muse-spark-1.3-contributor-free`, and
`https://opencode.ai/zen/v1/responses`; it creates guarded root, child,
compaction, candidate-evaluation, and comparison consumers from one factory.
It refuses a model/provider mismatch before transport and reserves a
content-free ledger entry before every allowed request. The aggregate limits
are 40 attempts, 100,000 requested output tokens, 30 minutes, and two active
streams. Evidence must be refreshed on the current UTC date before the
factory can construct a provider; active streams also stop at the task wall
deadline. The Zen adapter also rejects cross-origin redirects.

The checked record at
`evals/live/zen-free-catalog-evidence.json` was reviewed against the official
[OpenCode Zen catalog](https://opencode.ai/docs/zen) on 2026-09-22. It records
Free input, output, and cached-read charges per one million tokens;
cached-write has no listed price. The catalog says the selected Contributor
Free route permits prompts and completions to train future Meta models.
Consequently, this lane may send only disposable synthetic or deliberately
public fixtures. Refresh this record from the official page before any future
live attempt; if the exact identity, endpoint, charge, or data-use term cannot
be confirmed, record `BLOCKED` and do not substitute a model.

Run the fail-closed report command with caller-owned paths outside the
repository:

~~~sh
cargo run -p tea-agent --example zen-free-verification --features live-verification -- \
  --catalog-evidence evals/live/zen-free-catalog-evidence.json \
  --ledger /tmp/tea-live-ledger.json --out /tmp/tea-live-report.json \
  --run-counterparts
~~~

`--run-counterparts` executes all six checked-in, provider-free oracles with
credentials removed from their process environment. Without `--live`, the
command creates a sanitized six-case `BLOCKED` report and reads no credential.
A real headless exercise additionally requires
`--live --synthetic-or-public-fixtures`, an explicit `OPENCODE_API_KEY`, and
fresh caller-created `--tea-home` and `--workspace` directories. It injects
the one restricted factory's providers into the feature-only terminal harness
and sends only public synthetic prompts. It performs a workspace read/edit and
host-side exit oracle, a distinct terminal one-shot completion with an exact
synthetic response oracle, and controlled cancellation after durable provider
admission followed by passive reopen and a fresh exact continuation. Forced
compaction, Luau activation/rollback, and child orchestration each run through
a dedicated scenario adapter in `tea_agent::verification`
(`run_live_compaction_scenario`, `run_live_evolution_scenario`,
`run_live_child_scenario`) with its own durable oracle; none substitutes a root
transport prompt. Each adapter also has a feature-gated scripted-provider test
under `--features live-verification`. A missing credential or any setup
rejection remains `BLOCKED` and sends no inference. The required cases and executable
provider-free counterparts are
synthetic coding (`cargo test -p tea-core --test coding_capabilities --locked`),
headless/one-shot and interrupted reopen (the `tea-fixtures` executable run
against checked-in `single-turn-text` and `model-stream-cancellation-reuse`
oracles), forced compaction (`cargo test -p tea-core --lib --locked
compaction`), Luau activation/rollback (`cargo test -p tea-luau --locked`),
and isolated children (`cargo test -p tea-core --lib --locked subagent`).
The `--run-counterparts` report records the exact command for every case.
Scripted providers only prove their deterministic counterparts and never
constitute a live pass.
