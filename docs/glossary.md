# Glossary

This glossary names the durable v1 boundaries. These terms describe observable
ownership and recovery behavior, not implementation convenience.

## Durable runtime

**Session** — The append-only v1 durable history. A session contains lane
entries, records, facts, and header metadata.

**Operation** — One caller-visible durable request. An operation starts before
any provider or tool effect and has exactly one terminal outcome.

**Epoch** — One bounded core execution interval inside an operation. Each epoch
is pinned to one harness revision, snapshot, and model-harness profile.

**Harness revision** — An immutable branch node that selects a harness snapshot.
The active revision is committed in a HarnessRevisionChanged entry.

**Harness snapshot** — The content-addressed executable declaration for a
revision: prompts, plugin source trees, capability bindings, tool presentation,
and policy fingerprints.

**Harness manager** — The session-local resolver that validates immutable
lineage, resolves a revision into a provider-independent `ResolvedHarness`, and persists the
catalog needed for reopen.

**Durable harness** — `tea_core::runtime::SessionRuntime`. The only host execution
surface for durable prompts, recovery, artifact verification, and collection.

**Hosted epoch** — `tea_core::runtime::HostedEpoch`. One resolved immutable Tea
agent epoch executed under a caller-owned durable authority. It shares standard
harness construction and provenance, but owns no Tea session or recovery state.

**Artifact** — Immutable content-addressed bytes stored outside the session
stream and referenced by an artifact ID plus declared length.

**Effect intent** — A durable provider, tool, or policy obligation committed
before the corresponding external effect begins.

**Recovery plan** — The reducer-derived next durable action after reopening an
unfinished operation. Recovery never guesses whether an unrecorded external
effect occurred.

**Trace artifact** — A redacted v1 event stream whose digest, byte length, and
operation/epoch provenance are committed as a session fact.

## Core epoch

**Agent** — `tea_core::Agent`, the in-memory executor for one resolved epoch.
It owns transient core state; it is not the durable session authority.

**Agent message** — `AgentMessage`, the canonical core transcript value for a
user, assistant, or tool-result message.

**Tool call** — `AgentToolCall`, the provider-issued identifier, name, and
arguments for one requested capability execution.

**Tool result** — `AgentToolResult`, the finalized correlated output returned
to the core loop after a tool settles.

**Model provider** — A caller-owned `ModelProvider` implementation. It receives
an explicit request and cancellation token; it never comes from ambient
configuration.

**Stop reason** — The terminal outcome of one model turn. `StopReason::Stop`
is the normal completion value.

## Policy and evolution

**Capability binding** — A host-reviewed connection from a plugin declaration
to a concrete executable authority. A plugin cannot obtain an undeclared
binding.

**Candidate** — A staged immutable harness proposal. A candidate is not active
until its validated revision transition is committed.

**Experiment lock** — The frozen profile, task set, evaluation method, policy,
and evidence contract used by one evolution campaign.

**Champion** — An operator-selected evaluated candidate within a campaign.
Champion selection alone does not make it globally active.

**Global profile pointer** — The durable operator-controlled mapping from one
model-harness profile to a promoted candidate snapshot. Promotion and rollback
both retain transition lineage.

See `docs/harness-self-extension.md` and `docs/harness-evolution.md` for the
full policy and operator boundaries.
