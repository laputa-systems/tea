# tea

`tea` is a small, headless Rust implementation of a pinned,
useful subset of Pi's agent runtime. It is an execution microkernel, not a
port of Pi's interactive application.

The core reduces this explicit loop:

```text
model stream -> assistant response -> tool execution -> tool results -> next turn
```

It is designed for disposable agents in CI sandboxes, VM worlds, RL
environments, and swarms. The selected compatibility behavior is captured by
an in-process, provider-free Rust fixture harness; this project never launches
the Pi CLI or depends on an external runtime for verification.

## Design commitments

- Rust owns state transitions, scheduling, cancellation, event settlement,
  tracing boundaries, and resource ownership.
- The embedding owns the executor, model transport, workspace, tools, policy,
  credentials, and side effects.
- The core has no ambient configuration, session storage, `$HOME` discovery,
  package/plugin discovery, provider implementation, or background runtime.
- The checked-in nightly in `rust-toolchain.toml` is authoritative. Tokio is
  prohibited; applications commonly drive the core with Smol.
- `tea-protocol` uses Miniserde at the JSON boundary. Serde values are not
  part of the public workspace contract.
- The pinned Pi default coding profile is batteries-included but fully
  replaceable. Its workspace and all filesystem/process authority are explicit.
- `tea-luau` is optional. A pure Rust agent neither links nor constructs a
  scripting VM.

## Crate direction

```text
tea-protocol <- tea-core <- tea-luau
                   <- tea-trace
```

Arrows point from a dependency toward its dependent. The protocol provides
stable data and event shapes; the core owns the loop; trace and Luau are
downstream optional layers.

## Documentation map

- [Quickstart](quickstart.md) — build and run a first caller-owned agent.
- [Scope and compatibility boundary](scope.md) — selected Pi subset and hard
  exclusions.
- [Architecture](architecture.md) — ownership, ports, state machine, and
  scheduling.
- [Core terminology](core-terminology.md) — upstream Pi vocabulary, Rust
  names, aliases, and intentional differences for resync work.
- [Glossary](glossary.md) — repository-wide vocabulary for state, boundaries,
  lifecycle, tools, policy, and verification layers.
- [Runtime semantics](semantics.md) — observable lifecycle and cancellation
  contracts.
- [Fixture format](../crates/tea-core/fixtures/fixture-format.md) and
  [Rust fixture guide](../crates/tea-core/fixtures/README.md) — exact
  behavioral fixtures and verification evidence owned by `tea-core`.
- [Default coding profile](default-coding-profile.md) — captured prompt, tools,
  operation adapters, and update procedure.
- [HTTP/2 boundary](../HTTP2.md) — transport-version evidence and the future
  H2 adapter contract.
- [Verification](verification.md) — required checks and completed V0 evidence.
- [Prompt cache-friendliness](cache-friendliness.md) — deterministic prefix measurements and
  compaction cache discipline.
- [Conversation compaction](compaction.md) — transactional lifecycle, policy,
  request-layout evidence, trace/privacy boundary, historical rationale, and
  provider-free verification.
- [Quality evaluation](../evals/README.md) — deterministic core checks,
  resource diagnostics, and the explicit provider-opt-in coding tier.
- [Tracing](trace.md) — optional trajectory observer boundary.
- [Terminal host](tui.md) — `tea` ownership boundaries, interaction
  contract, and post-V0 direction.
- [`fx` UI compatibility oracle](fx-ui-compatibility.md) — frozen terminal sizes,
  normalized-cell snapshots, and the reduced tea parity map.
- [Writing Luau extensions](luau-extensions.md) — closed bundles, capability
  bindings, coroutine-backed tools, limits, and review rules.

The core-owned fixture harness has its own
[guide](../crates/tea-core/fixtures/README.md).
The end-to-end coding evaluation controller is documented in
[`evals/README.md`](../evals/README.md).
