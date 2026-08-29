# Verification

Start with the nearest relevant hard judge and broaden only when its evidence
supports the change.

For the durable v1 surface, the focused checks are:

~~~sh
cargo +nightly-2026-07-24 test -p tea-session --lib --locked
cargo +nightly-2026-07-24 test -p tea-providers --locked
cargo +nightly-2026-07-24 test -p tea-luau --locked
cargo +nightly-2026-07-24 test -p tea-core --locked
cargo +nightly-2026-07-24 test -p tea-agent --lib --locked
cargo +nightly-2026-07-24 test -p tea-agent --features pty-harness --test pty_streaming --locked
./crates/tea-core/fixtures/run.sh
python3 scripts/check-crate-graph.py
git diff --check
~~~

For the default coding bundle, begin with the narrow checks:

~~~sh
cargo +nightly-2026-07-24 test -p tea-core --test coding_capabilities --locked
cargo +nightly-2026-07-24 test -p tea-luau builtins::tests::coding_is_a_closed_four_tool_bundle_with_fixed_grants --lib --locked
~~~

They prove the four-tool Luau surface and the independent trusted workspace,
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
