# Trace v1

tea-trace defines a compact, append-only v1 episode format. A trace has an
episode header, zero or more model-turn, tool, and compaction records, and one
terminal episode-end record.

Every JSON Lines and CBOR record carries schema_version 1 and a type
discriminator. The trace crate is a sink boundary; it does not own a provider,
filesystem location, session store, executor, or clock.

DurableHarness wraps its trace capture in a redactor that replaces model input
and output, tool input and output, and terminal diagnostics before persistence.
It retains chronology, event kinds, cache evidence, compaction lifecycle, and
durable provenance without retaining prompt or tool content.

At an epoch boundary the supervisor writes the complete redacted JSON Lines
artifact to the session object store and appends SessionFact::TraceArtifact.
The fact includes exact byte length plus operation, epoch, core run, revision,
snapshot, and model-harness profile. Session verification and artifact
collection treat that artifact as a reachable root.

Failure signatures in tea-evolve cite positive byte spans inside those artifacts.
EvolutionStore reloads, rehashes, and bounds-checks each citation before it
persists or reopens an experiment.
