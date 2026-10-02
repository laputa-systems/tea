# tea - token elicitation arts

minimal extensible agent harness

Tea is deliberately single-session. A running host attaches to at most one live
root session; it never becomes a multi-session manager or resident service.

Start with [docs/overview.md](docs/overview.md). The main routes are:

- [Quickstart](docs/quickstart.md) for an application integration.
- [Scope](docs/scope.md), [architecture](docs/architecture.md), and
  [semantics](docs/semantics.md) for the durable core contract.
- [Glossary](docs/glossary.md) for the durable names and boundaries used across
  the repository.
- [Default coding profile](docs/default-coding-profile.md) and
  [provider adapters](docs/provider-adapters.md) for optional runtime layers.
- [Tracing](docs/trace.md) and [Luau ABI v3](docs/luau-abi-v3.md) for
  optional observability and policy layers.
- [Terminal host](docs/tui.md) for the repository-owned `tea` TUI, and
  [headful crash isolation](docs/crash-isolation.md) for its session runtime
  and terminal relay.
- [Pi 1.0 upgrade record](docs/pi-1-upgrade.md) for the Anthropic adapter,
  thinking, cache warming, discovery/codemode, MCP, and virtual models.
- [Durable subagents](docs/subagents.md) for the optional asynchronous
  multi-lane execution and isolated-workspace contract.
- [Quality evaluation](evals/README.md) and [verification](docs/verification.md)
  for contract and quality evidence.
- [fixture format](crates/tea-core/fixtures/fixture-format.md) and
  [fixture guide](crates/tea-core/fixtures/README.md)
  for fixture-based contract work.
- [Luau ABI v3](docs/luau-abi-v3.md) for the optional capability-scoped
  policy plane.

## Architectural invariants

- A tea host owns or attaches to at most one live root session at a time.
  Switching with `/new` or `/resume` replaces that attachment; it never
  multiplexes root sessions.
- Saved session directories are inert durable state until explicitly opened.
  Listing, inspecting, exporting, verifying, or presenting them must not keep
  them alive or advance work.
- Do not add a live-session registry, session tabs/workspaces, a background
  session scheduler, a daemon, broker, resident server, global control socket,
  or any service whose job is to supervise or route among root sessions.
- Fork lanes and optional subagents belong to the one attached root session.
  They must not become independently managed top-level sessions.
- Presentation may be crash-isolated from execution only through a one-to-one,
  session-owned boundary. A session-scoped runtime process may outlive a failed
  TUI and use minimal session-local IPC, but it may serve only that session,
  cannot discover/create/switch other root sessions, and cannot become a
  reusable global service. Reattachment must identify the exact session
  directly rather than asking a broker to enumerate live sessions.
- Independent tea invocations may each own one session. The prohibition is on
  multiplexing or supervising multiple root sessions inside one harness or
  shared resident service, not on a machine-global singleton.

## Working contract

- Establish behavior, boundaries, callers, tests, and documentation before
  changing a contract. Make the smallest reversible assumption.
- Use precise types and explicit capability boundaries. Do not add a dependency
  without user approval or hide a fallback that changes semantics.
- For bugs, add the smallest isolated failing regression test first, then fix
  the root cause and retain the test.
- Start with focused evidence, then broaden checks.
- Keep core executor- and provider-agnostic.
