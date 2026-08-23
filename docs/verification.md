# Verification

Start with the nearest relevant hard judge and broaden only when its evidence
supports the change.

For the durable v1 surface, the focused checks are:

~~~sh
cargo test -p tea-session --lib
cargo test -p tea-luau
cargo test -p tea-harness --lib
cargo test -p tea-evolve --lib
cargo test -p tea-agent --lib
cargo test -p tea-core --features trace trace::tests --lib
cargo test -p tea-core --lib measurement::tests
git diff --check
~~~

The repository also contains deterministic core fixtures and optional
quality-evaluation tooling under evals/. Live-provider evaluation requires
explicit caller authorization and must write its reports outside the source
tree.

Durability work must verify the actual recovery boundary: a session opens from
its v1 log, reachable immutable artifacts rehash correctly, the harness catalog
reconstructs the committed revision, and the terminal can reopen without an
in-memory transcript. Trace and evolution work must additionally verify their
artifact roots and exact evidence spans.

Do not run formatters, linters, pre-commit hooks, or push as part of normal
verification in this repository.
