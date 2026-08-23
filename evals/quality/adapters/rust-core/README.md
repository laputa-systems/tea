# `rust-core` quality adapter

This executable is a thin JSON process boundary around the existing
[`tea-fixtures`](../../../../crates/tea-core/src/bin/tea-fixtures.rs)
binary. It uses the repository's pinned `nightly-2026-07-24` toolchain and
does not instantiate a TUI, provider, policy runtime, or ambient workspace
capability.

`adapter.py` reads one JSON request from stdin:

```json
{"protocol":"tea-quality-adapter/v1","operation":"run","fixture":"crates/tea-core/fixtures/declarative/single-turn-text.json"}
```

`fixture` is required and is resolved relative to the repository root when it
is not absolute. The quality suite lowers its case manifests to the closed
fixture vocabulary before invoking this process. Barrier/scheduler controls
are rejected rather than silently ignored because the fixture runner has no
barrier scheduler.

The response is one JSON object with `metadata` describing the Rust runner and
`result` containing the unchanged canonical fixture result emitted by the
binary. Runner status `0` is success; status `1` is a valid error/cancellation
result; status `2` means the request or runner contract was invalid.

Example:

```sh
printf '%s\n' '{"fixture":"crates/tea-core/fixtures/declarative/single-turn-text.json"}' |
  evals/quality/adapters/rust-core/adapter.py
```
