# tea

tea is a minimal, extensible, durable agent harness. The core execution engine
remains provider- and executor-agnostic; the repository-owned terminal enters
through the durable harness for every prompt.

## Start here

- [Quickstart](quickstart.md) explains an application-owned core integration.
- [Scope](scope.md) states the v1 product boundary.
- [Architecture](architecture.md) and [semantics](semantics.md) describe the
  pure execution kernel.
- [Terminal host](tui.md) describes the repository-owned tea application.

## Durable harness

- [Durable harness](durable-harness.md) explains the execution boundary.
- [Session format v1](session-format-v1.md) defines the only session format.
- [Persistence inventory](persistence-inventory.md) classifies every
  repository-owned persistence path by authority and recovery role.
- [Persistence measurements](persistence-benchmarks.md) records reproducible
  generated fixture commands and local comparison evidence.
- [Harness recovery](harness-recovery.md) explains reopening and effect safety.
- [Artifact recovery](artifact-recovery.md) defines immutable evidence roots.
- [Trace](trace.md) defines redacted v1 trace artifacts.

## Harness policy and evolution

- [Harness self-extension](harness-self-extension.md) describes immutable
  candidate staging and safe activation.
- [Luau ABI v1](luau-abi-v1.md) defines the one accepted plugin contract.
- [Model-harness profiles](model-harness-profiles.md) defines serving identity.
- [Harness evolution](harness-evolution.md) defines durable experiments,
  promotion, and rollback.

## Optional runtime layers

- [Default coding profile](default-coding-profile.md)
- [Provider adapters](provider-adapters.md)
- [Compaction](compaction.md)
- [Durable subagents](subagents.md)
- [Verification](verification.md)
- [Prompt cache-friendliness](cache-friendliness.md) describes content-free logical continuity evidence.
