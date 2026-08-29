# Scope

Tea v1 is a provider- and executor-agnostic Rust agent core with an optional
durable harness and a repository-owned terminal host.

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

Luau remains optional and capability-scoped. Its closed v1 bundles can
contribute bounded policy but cannot redefine core state transitions, session
storage, run settlement, or host authority.

Features outside these explicit boundaries require a new contract, focused
tests, and documentation. No ambient discovery, silent provider selection,
format interpretation, or fallback for a retired contract belongs in the v1
surface. The terminal's documented TinyFish web fallback is an explicit
host-owned exception: it uses only a caller-provisioned `TINYFISH_API_KEY`,
never exposes that key to policy or durable state, and leaves Firecrawl Keyless
as the default backend.
