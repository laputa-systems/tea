# tea - token elicitation arts

minimal extensible agent harness

Start with [docs/overview.md](docs/overview.md). The main routes are:

- [Quickstart](docs/quickstart.md) for an application integration.
- [Scope](docs/scope.md), [architecture](docs/architecture.md), and
  [semantics](docs/semantics.md) for the durable core contract.
- [Glossary](docs/glossary.md) for the durable names and boundaries used across
  the repository.
- [Default coding profile](docs/default-coding-profile.md) and
  [provider adapters](docs/provider-adapters.md) for optional runtime layers.
- [Tracing](docs/trace.md) and [Luau ABI v1](docs/luau-abi-v1.md) for
  optional observability and policy layers.
- [Terminal host](docs/tui.md) for the repository-owned `tea` TUI.
- [Durable subagents](docs/subagents.md) for the optional asynchronous
  multi-lane execution and isolated-workspace contract.
- [Quality evaluation](evals/README.md) and [verification](docs/verification.md)
  for contract and quality evidence.
- [fixture format](crates/tea-core/fixtures/fixture-format.md) and
  [fixture guide](crates/tea-core/fixtures/README.md)
  for fixture-based contract work.
- [Luau ABI v1](docs/luau-abi-v1.md) for the optional capability-scoped
  policy plane.

## Working contract

- Establish behavior, boundaries, callers, tests, and documentation before
  changing a contract. Make the smallest reversible assumption.
- Use precise types and explicit capability boundaries. Do not add a dependency
  without user approval or hide a fallback that changes semantics.
- For bugs, add the smallest isolated failing regression test first, then fix
  the root cause and retain the test.
- Start with focused evidence, then broaden checks.
- Keep core executor- and provider-agnostic.
