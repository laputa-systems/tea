# Artifact recovery and collection

Immutable artifacts are content-addressed BLAKE3 objects held by an
ArtifactStore. A durable record or fact must identify exact bytes before it
claims the object is retained.

The root set is explicit:

- payload and semantic references in the v1 session;
- harness catalog and immutable source-tree objects;
- redacted trace artifacts recorded by SessionFact::TraceArtifact;
- evolution failure-signature trace citations returned by
  EvolutionStore::artifact_roots.

plan_artifact_gc computes a reviewed collection plan from an artifact inventory
and a declared quota. apply_artifact_gc removes only objects in that plan after
rechecking their identity. Stores without inventory/removal support reject
collection rather than pretending it completed.

JsonlSession::export creates a new destination directory atomically. It copies
the verified JSONL prefix and only the supplied reachable artifact roots. It
never copies a mutable worktree, process-local cache, or unreferenced object.

Hosts should redact content before persisting any model-readable artifact. The
durable trace sink is deliberately content-redacting even when ordinary
artifact policy allows model-readable payloads.
