# Scope

Tea v1 is a provider- and executor-agnostic Rust agent core with an optional
durable harness and a repository-owned terminal host. It is deliberately a
single-session harness, not a multi-session manager.

The core owns typed agent state, one active run, provider request construction,
tool scheduling, queues, hooks, cancellation, compaction transactions, event
settlement, and structured error boundaries. A host owns the executor, model
transport, credentials, world authority, workspace, and any UI.

The durable layer owns only durable concerns: append-only session facts, effect
intent and outcome, immutable artifacts, resolved harness lineage, recovery,
and redacted trace evidence. It does not grant a model filesystem, network,
process, provider, artifact-store, or promotion authority.

The terminal uses that durable layer for all prompts. It is not a second
execution engine.

A host may own or attach to at most one live root session. Saved sessions are
inert until explicitly opened; switching sessions replaces the host's
attachment rather than multiplexing roots. Fork lanes, goal continuation and
optional subagents remain subordinate to that one session. Tea does not own a
live-session registry, background session scheduler, daemon, broker, resident
server, or global control plane.

Presentation/execution crash isolation is the only process-separation exception.
A session-owned runtime process may survive a TUI failure and expose minimal
session-local IPC for direct reattachment to that exact session. It may not
discover, create, select, route, or supervise other root sessions, and it may
not be reused as a global service.

Luau remains optional and capability-scoped. Its closed v3 bundles can
contribute bounded policy but cannot redefine core state transitions, session
storage, run settlement, or host authority.

Features outside these explicit boundaries require a new contract, focused
tests, and documentation. No ambient discovery, silent provider selection,
format interpretation, or fallback for a retired contract belongs in the v1
surface. The terminal's documented TinyFish web fallback is an explicit
host-owned exception: it uses only a caller-provisioned `TINYFISH_API_KEY`,
never exposes that key to policy or durable state, and leaves Firecrawl Keyless
as the default backend.
