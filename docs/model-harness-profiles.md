# Model-harness profiles

A ModelHarnessProfileId names the exact serving profile for a harness snapshot.
It is separate from a provider model name: the same model may use different
prompt, tool, policy, or capability configurations, and a profile can be
evaluated independently for each target model.

HarnessSnapshotV1 binds the profile to:

- trusted base prompt and optional self-extension addendum;
- ordered closed global and session-local bundles;
- named prompt sections and tool presentations;
- hook, compaction, projection, and failure-policy identities;
- capability bindings and resource limits;
- provider-visible surface fingerprints.

The resolved profile is recorded with every epoch and trace artifact. Recovery
derives it from the committed harness revision; hosts do not silently replace
it from current preferences or mutable source files.

Global serving selection lives in EvolutionStore as a GlobalProfilePointerV1.
It can only point to an operator-selected champion whose proposal targets that
profile and whose immutable snapshot is retained by the frozen experiment.
