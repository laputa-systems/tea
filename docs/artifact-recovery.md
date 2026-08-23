# Artifact recovery and collection

Immutable artifacts are content-addressed BLAKE3 objects held by an
ArtifactStore. A durable record or fact must identify exact bytes before it
claims the object is retained.

`FileArtifactStore` streams and hashes bytes into a private temporary file,
synchronizes that file, then publishes only the completed digest path without
overwriting an existing object. A stream failure removes the temporary and
cannot leave a resolvable partial object; a published but unreferenced object
is an ordinary GC candidate, never session history.

If a process dies before it can remove a private `.artifact.tmp` file, object
inventory and collection ignore that exact temporary-name namespace. It is not
a digest bucket and cannot make a valid session or a finalized object appear
corrupt. Other unexpected names, symlinks, and non-regular object paths still
fail closed.

The deterministic publication matrix interrupts before temporary creation,
before and after file synchronization, before and after no-replace
publication, and before and after directory synchronization. Before-publication
failures leave no object; later ambiguous failures can leave only the exact,
independently verifiable immutable object as an orphan. A separate commit test
shows that publication before a failed log reference has the same harmless
orphan outcome, while a committed reference verifies its object. `tea session
verify` reports finalized orphan identities and lengths separately from required
object verification; it never treats an orphan as evidence that the committed
prefix is corrupt. Before doing so, the terminal host validates every retained
harness catalog's canonical manifest and source-tree objects, so an intact
catalog blob cannot hide a missing or altered transitive source object.

The root set is explicit:

- payload and semantic references in the v1 session;
- harness catalog and immutable source-tree objects;
- redacted trace artifacts recorded by SessionFact::TraceArtifact;
- evolution failure-signature trace citations returned by
  EvolutionStore::artifact_roots.

plan_artifact_gc computes a reviewed collection plan from an artifact inventory
and a declared quota. apply_artifact_gc removes only objects in that plan after
rechecking their identity. JsonlSession::collect_unreferenced_artifacts runs
that plan/application pair while the session writer lock is held. Stores
without inventory/removal support reject collection rather than pretending it
completed.

JsonlSession::export_to creates a new destination directory atomically. It
copies the verified JSONL prefix and only the supplied reachable artifact
roots, then writes `export.json` naming the exported prefix digest plus every
included object's content identity and exact byte length.
The terminal export command first derives and verifies roots from every
retained harness catalog; restore rejects an export manifest that omits any of
those required source roots. It never copies a mutable worktree, process-local
cache, or unreferenced object.

Hosts should redact content before persisting any model-readable artifact. When
an immutable harness policy sets `redact_before_persist`, Tea retains the
post-policy tool result as the durable inline payload or artifact; raw tool
content is not retained through that path. Schema-deviation evidence fails
closed when that policy lacks a host redactor. The durable trace sink is
deliberately content-redacting even when ordinary artifact policy allows
model-readable payloads.
