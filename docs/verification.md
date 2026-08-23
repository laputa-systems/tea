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

The fixture command compares every provider-free declarative core case with
its checked-in canonical result. Optional quality-evaluation tooling lives
under `evals/`; live-provider evaluation requires explicit caller
authorization and must write its reports outside the source tree.

Durability work must verify the actual recovery boundary: a session opens from
its v1 log, reachable immutable artifacts rehash correctly, the harness catalog
reconstructs the committed revision, and the terminal can reopen without an
in-memory transcript. Trace and evolution work must additionally verify their
artifact roots and exact evidence spans.

Do not run formatters, linters, pre-commit hooks, or push as part of normal
verification in this repository.
