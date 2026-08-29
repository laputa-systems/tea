# Rust contract fixtures

This directory is the deterministic, provider-free contract corpus for the Rust agent kernel.
Each declarative fixture is run by the checked-in Rust adapter and compared with its checked-in
canonical result. There is no upstream checkout, differential runner, or live provider in this
verification path.

## Layout

    crates/tea-core/fixtures/
    ├── declarative/       # deterministic, provider-free test inputs
    ├── expected/          # checked-in canonical Rust results
    ├── fixture-format.md  # declarative input contract
    ├── normalization.md   # canonicalization and redaction rules
    ├── runners.md         # Rust runner I/O and scope boundaries
    └── run.sh             # full Rust fixture check

Provider response captures and replay artifacts are outside this core fixture corpus.

The corpus covers text turns, cancellation and reuse, tool success/error, parallel completion
ordering, partial updates, hooks, queues, and continuation. The default coding
bundle has separate Luau and host-capability contract tests.

## Compaction reference behavior

Automatic compaction is intentionally not represented as a Pi session-storage
fixture: this crate owns a canonical in-memory transcript while Pi owns session
entries, summaries, and storage boundaries. The shared behavioral surface is
tested in Rust by `crates/tea-core/tests/automatic_policy.rs`: valid cut
boundaries, split-turn prefix exposure, retained tool-call/result pairing,
last-valid usage fallback, threshold ordering, and one overflow retry.

When updating that surface, run Pi's independent reference cases beside the
Rust test, from explicit local checkouts:

```bash
(cd ~/d/pi/packages/coding-agent && npx vitest --run test/compaction.test.ts)
cargo test -p tea-core --test automatic_policy
```

Compare the decision mechanics (`findCutPoint`/`prepareCompaction` and the
Rust `AutomaticCompactionRequest`), not Pi session entry IDs, persisted summary
prompts, or application queue behavior. Neither command is part of the
provider-free checked-in fixture runner.

## Workflow

1. Add a provider-free JSON fixture under `crates/tea-core/fixtures/declarative/`.
2. Add its canonical result under `crates/tea-core/fixtures/expected/`.
3. Run `crates/tea-core/fixtures/run.sh`.
4. Treat a mismatch as a contract or fixture change; do not weaken normalization to hide it.

The runner uses the pinned nightly toolchain and jq for canonical JSON comparison. It never
starts a Pi CLI, installs packages, contacts a provider, reads ambient configuration, or mutates
the checked-in fixture tree. Fixture outcomes that represent model/tool errors or cancellation
are valid data; malformed fixtures and runner failures are verification failures.

The historical Pi capture under `crates/tea-core/profile/` is not a runtime
profile. See [`../profile/README.md`](../profile/README.md) and
[`../../tea-luau/builtins/coding/`](../../tea-luau/builtins/coding/) for the
production coding boundary.
