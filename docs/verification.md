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

## Guarded Codex Luna live lane

The normal Make targets and `python3 -m evals.quality` commands are
provider-free. Confirm that boundary after changing an evaluation entry point:

~~~sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/check-verification-entrypoints.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest evals.test_live_verification
~~~

The legacy `evals/run-rust-live.sh` command always exits before Cargo or
transport. The generic `evals/controller.py` contract is not an approved live
verification entry point; its checked-in test adapters are provider-free.

The feature-gated `tea_agent::verification::RestrictedCodexFactory` accepts
only `codex/gpt-5.6-luna` with low reasoning effort. It constructs the native
Tea Codex subscription adapter from an explicit installed Codex client
`auth.json` or Tea-owned `auth/codex.json`
path and creates guarded root, child, compaction, candidate-evaluation, and
comparison consumers. Every model request must match the exact model and
effort before transport. The shared ledger records each request before
transport and retains previous attempts across continued runs. It does not
impose a request-count, wall-time, or concurrent-stream limit. The Codex
subscription wire does not expose a reliable request output token cap, so this
lane makes no reserved-output-token claim.
The v2 ledger records the exact model for every attempt. The ten earlier
`gpt-6-luna` attempts remain recorded after the user-approved switch to
`gpt-5.6-luna`; the backend rejected the former with HTTP 400 for this
ChatGPT account.

The checked record at `evals/live/codex-luna-model-evidence.json` was reviewed
against [official OpenAI Codex model documentation](https://learn.chatgpt.com/docs/models)
on 2026-09-24 UTC. It pins the model and low effort. OpenAI says availability
varies by account, rollout, and client; an actual Tea request must establish
access. The direct subscription endpoint and credential boundary are
defined in [Codex provider](codex-provider.md). Refresh the record on the UTC
date of a live run. Keep fixture input disposable or deliberately public.

Run the provider-free report with caller-owned paths outside the repository:

~~~sh
cargo run -p tea-agent --example codex-luna-verification --features live-verification -- \
  --model-evidence evals/live/codex-luna-model-evidence.json \
  --ledger /tmp/tea-live-ledger.json --out /tmp/tea-live-report.json \
  --run-counterparts
~~~

`--run-counterparts` executes six checked-in, provider-free oracles with
ambient API credentials removed from their process environment. Without
`--live`, the command writes a sanitized six-case `BLOCKED` report and reads
no Codex credential. Each case passes only when its live scenario and offline
oracle pass; the suite passes only when all six cases pass. A failed case marks
the report `FAILED`, and missing evidence leaves it `BLOCKED`.

A live exercise additionally requires `--live --synthetic-or-public-fixtures`,
an explicit `--credential-path` naming the installed Codex client's
`auth.json` or Tea-owned `auth/codex.json`, and fresh caller-created
`--tea-home` and `--workspace` directories. The client path is read-only;
Tea reloads its access token but never imports its refresh token. The example injects the guarded provider
into the feature-only terminal harness and runs synthetic coding, headless
one-shot, interrupted reopen, forced compaction, Luau activation/rollback,
and isolated child scenarios. Each scenario has a durable oracle and an
offline counterpart; scripted providers never count as a live pass. A missing
credential or setup rejection remains `BLOCKED` and sends no inference.
