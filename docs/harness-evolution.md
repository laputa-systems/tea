# Harness evolution

`tea_core::evolution` is the durable, capability-neutral control plane for evaluating
immutable harness candidates. It cannot run an agent, mutate a session, invoke
a provider, or activate a revision by itself.

An ExperimentLockV1 freezes the target profiles, initial snapshot, task and
split manifests, evaluator, environment, capability envelope, budgets,
promotion policy digest, and trusted build identity. Its ExperimentId is
content-derived and verified when a campaign opens.

EvolutionStore persists one campaign directory atomically as campaign.json. It
retains:

- verified FailureSignatureV1 records with exact redacted trace byte spans;
- staged candidate proposals, including rejected candidates;
- complete gate results and promotion decisions;
- the selected champion;
- global profile promotion and rollback transitions.

Every cited trace artifact is loaded, rehashed, and bounds-checked before a
signature is accepted or a store is reopened. artifact_roots exposes every
trace citation to backup, export, and collection code.

Evaluation does not activate anything. PromotionAuthority::Operator is required
to select a champion, promote it to a global profile pointer, or rollback that
pointer. Rollback appends a transition rather than erasing the promotion it
reverses.
