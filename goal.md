Consolidate the durable harness's (intro'd in 5f186d59527f57136445644287763a9b001d39b8) persistence layer into one minimal, rigorous, clean-slate v1 design.

The repository is intentionally abandoning all previously written session formats and backward-compatibility code. Existing old sessions do not need to remain readable. Git history is the archive. The active codebase must contain one current persisted contract, version 1, with no v3 importer, no v4 nomenclature, no compatibility shims, and no dormant old schemas.

Implement the work, not merely a design document. Proceed incrementally, add deterministic fixtures before changing each contract, and keep all previously completed harness behavior passing throughout.

---

## 1. Primary objective

Make tea’s persistence model explicit, narrow, mechanically testable, and aligned with its minimal architecture:

```text
authoritative ordered state
    append-only per-session JSONL v1 mutation log

authoritative immutable bytes
    content-addressed BLAKE3 object storage
    immutable content-addressed harness manifests

small operator-authored state
    atomically replaced configuration files

derived disposable acceleration
    session metadata cache
    active-head cache
    in-memory replay indexes
    optional rebuildable snapshots

separate analytical state
    evolution/evaluation storage outside the normal tea binary
```

The core `tea` executable must not become a database application.

The intended result is:

> A session can always be understood as one validated committed log prefix plus the immutable objects referenced by that prefix.

SQL, page stores, mutable B-trees, LSM trees, and general key/value databases are unnecessary for this workload. Do not add them.

---

## 2. Hard constraints

Do not add:

* SQLite.
* `redb`.
* `fjall`.
* `sled`.
* LMDB or `heed`.
* `rkyv` as an authoritative format.
* A generic database abstraction.
* A generic key/value persistence API.
* A query language.
* An ORM.
* Tokio.
* Serde or `serde_json` merely for this task.
* Unsafe code.
* A second session reducer.
* A second durable scheduler.
* A second source of truth for lane state.
* Background compaction or maintenance threads.
* Transparent compression of the live session log.
* Silent recovery that discards a complete committed-looking record.
* Migration or import code for discarded session versions.
* Compatibility aliases such as `SessionV3`, `SessionV4`, `LegacySession`, or `ImportedSession`.
* Placeholder implementations, fake empty indexes, or documentation describing work that was not completed.

Use the repository’s existing JSON, BLAKE3, filesystem, ID, clock, and error infrastructure wherever they satisfy the required contract.

A new dependency is acceptable only if all of the following are true:

1. The existing implementation cannot satisfy a correctness requirement.
2. A focused benchmark or executable test demonstrates the need.
3. The dependency is smaller and safer than implementing the required primitive locally.
4. Its incremental contribution to the release `tea` binary is measured.
5. The final report explains why it was unavoidable.

The default expectation is that no new runtime dependency is needed.

---

## 3. First inspect the completed implementation

Before editing, inspect only the persistence-relevant implementation produced by `plan.md`:

* `tea-session`.
* The concrete filesystem session store.
* The in-memory reference store.
* The pure reducer.
* Session discovery and resume.
* Artifact storage.
* Harness trees, snapshots, revisions, and candidates.
* `HEAD` handling.
* Session snapshots.
* Export, restore, verification, and garbage collection.
* Fault injection and manual effect-gate tests.
* Persistence documentation.
* Persistence-related sections of `plan.md`.

Write a brief implementation map containing:

1. Every authoritative persisted file.
2. Every derived or reconstructible file.
3. Every ephemeral file.
4. Every place assigning sequence numbers, timestamps, parents, or active heads.
5. Every persistence format or schema version still present.
6. Every old importer or compatibility branch.
7. Every dependency linked into the release `tea` binary because of persistence.
8. Any current violation of the authority model in this prompt.

Keep this map concise. Then begin implementation immediately.

Do not conduct broad external research. The architecture is fixed below.

---

## 4. Capture a baseline before changing code

Record the current baseline in a temporary working note or benchmark output:

* Release size of the `tea` binary.
* `cargo tree` for the release `tea` binary or its owning package.
* Workspace test status.
* Persistence-focused test status.
* Full replay time for representative sessions.
* Session listing time for a large generated session directory.
* JSONL bytes for representative fixtures.
* Peak memory during long-session replay, using an existing repository mechanism or a small focused measurement tool.
* Snapshot-assisted reopen time, if snapshots currently exist.

Do not commit machine-specific absolute benchmark claims as universal guarantees. Preserve reproducible fixture definitions, commands, record counts, and before/after comparisons.

---

## 5. Perform the clean-slate v1 reset

Delete all persistence compatibility code and reset the active format to version 1.

Required cleanup:

* Delete the Pi-compatible v3 session reader.
* Delete v3 import and conversion code.
* Delete v3 fixtures.
* Delete v4-specific type names.
* Delete v4-specific filenames and documentation names.
* Delete any “open old format then convert on first mutation” path.
* Delete tests whose only purpose is compatibility with an abandoned format.
* Delete format-dispatch code that recognizes discarded versions.
* Delete old aliases and conversion structs.
* Delete dead feature flags related to old formats.
* Remove stale references from `plan.md`, active documentation, examples, tests, and comments.

The sole supported session header must identify format version 1.

A file with any other version must fail immediately with a precise unsupported-format error. Do not inspect or partially decode its records. Do not offer automatic conversion.

The error should state:

* The path.
* The observed version, when safely readable.
* That the current build supports only session format 1.
* That no automatic migration is available.

Do not scatter `V1` suffixes throughout ordinary Rust type names merely because the wire format is version 1. Prefer:

```rust
SessionHeader
StoredMutation
SessionSnapshot
SessionMetadata
```

Use an explicit version only at persisted format roots and integrity domains. Internal names such as `SessionHeaderV1` are justified only inside a narrowly isolated wire-format module when needed to prevent accidental serialization of domain structs.

At the end of the task, persistence-related code and documentation must have no v3/v4 compatibility residue.

---

## 6. Establish a complete persistence inventory

Create or update one authoritative persistence document containing a table with these columns:

```text
path or namespace
owner crate
classification
authoritative?
mutability
commit protocol
recovery behavior
garbage-collection behavior
contains sensitive data?
```

Every file tea writes must belong to exactly one classification:

### 6.1 Authoritative append-only

Examples:

* `session.jsonl`.
* Any global append-only control-plane log, if one genuinely exists.

These files are reduced to reconstruct current durable state.

### 6.2 Authoritative immutable

Examples:

* Content-addressed artifact bytes.
* Content-addressed harness source blobs.
* Immutable harness tree manifests.
* Immutable harness snapshots and revisions.
* Immutable evaluation evidence retained by a session.

Once published at an identity-derived path, their bytes never change.

### 6.3 Authoritative atomically replaceable configuration

This class is restricted to small operator-authored configuration whose latest complete value is inherently the desired state.

Examples may include:

* User configuration.
* An operator-selected global profile.
* Capability policy selected outside a session.

These files use temporary-write, synchronization, and atomic rename. They are not used for session execution history.

Do not put evolving session state into this class merely because replacing a JSON file is easy.

### 6.4 Derived and disposable

Examples:

* Session-list metadata.
* `HEAD`.
* Replay offset indexes.
* Search indexes.
* Materialized worktrees.
* Session snapshots.
* Cached summaries.
* Benchmark caches.

Deleting every derived file must not destroy semantic history or prevent recovery from the authoritative log and immutable objects.

### 6.5 Ephemeral

Examples:

* Lock files or lock handles.
* Temporary object files.
* Temporary metadata files.
* In-progress exports.
* Test failpoint markers.

Ephemeral files never participate in reduction.

### 6.6 Optional telemetry

Traces remain observational evidence, not a substitute for the authoritative session store. Do not recover semantic session state from an optional trace sink.

No file may have ambiguous authority.

---

## 7. Final v1 on-disk layout

Preserve the existing layout where it already satisfies these semantics, but converge on a structure conceptually equivalent to:

```text
<tea-home>/sessions/<workspace-key>/<session-stem>.tea/
    session.jsonl
    meta.json
    HEAD
    lock

    objects/
        blake3/
            ab/
                <full-lowercase-hex-digest>

    snapshots/
        <through-sequence>-<snapshot-digest>.json

    harness/
        trees/
        snapshots/
        revisions/
        candidates/

    worktrees/
        ...

    traces/
        ...

    evals/
        ...
```

Exact subdirectories may follow the completed implementation, but these rules are mandatory:

* `session.jsonl` is the sole ordered session authority.
* Referenced immutable object bytes are authoritative.
* Immutable harness manifests are authoritative.
* `meta.json` is a disposable session-discovery cache.
* `HEAD` is a disposable active-harness cache.
* `snapshots/` contains only disposable replay acceleration.
* `worktrees/` are reconstructible projections.
* `lock` contains no durable semantic state.
* Temporary files use names that session discovery and object traversal ignore.

Use restrictive file and directory permissions on supported Unix platforms:

* Session directories: owner-only where practical.
* Durable files: owner read/write.
* Never deliberately create world-readable session evidence.

Do not claim equivalent permission guarantees on platforms where tea does not enforce them.

---

## 8. Define the JSONL v1 wire contract manually

Do not serialize arbitrary in-memory Rust structs directly as the durable contract.

Create a narrow wire-format boundary with explicit encoding and decoding. Domain types may evolve without accidentally changing disk bytes.

The file consists of:

1. Exactly one header line.
2. Zero or more committed mutation lines.
3. A newline after every complete line.

Conceptual header:

```rust
struct SessionHeader {
    kind: SessionHeaderKind, // exactly "session"
    version: u32,            // exactly 1
    session_id: SessionId,
    created_at_ms: u64,
    workspace: String,
    initial_lane: LaneId,
    metadata: SessionCreationMetadata,
    digest: Digest,
}
```

Conceptual mutation envelope:

```rust
struct StoredMutation {
    seq: Sequence,
    timestamp_ms: u64,
    prev_digest: Digest,
    mutation: SessionMutation,
    digest: Digest,
}
```

Preserve the completed system’s narrow semantic mutation taxonomy. It may distinguish semantic entries, operation records, lane transitions, and global facts, but do not create a top-level wire kind for every feature.

The persisted contract must not duplicate authoritative envelope fields inside payloads:

* `seq` appears once.
* `timestamp_ms` appears once.
* A lane entry’s parent appears once.
* The active lane leaf is derived from mutations rather than redundantly persisted in multiple mutable forms.

The in-memory reduced representation may materialize sequence and timestamp into convenient views, but the file must have one source of truth.

### 8.1 Canonical encoding

All lines written by tea must use canonical, minified JSON:

* UTF-8.
* `\n` line endings.
* Fixed object-field order defined by the encoder.
* Deterministic map ordering.
* Stable enum tags.
* Stable number spelling.
* No insignificant whitespace.
* No omitted required fields.
* Explicit representation of nullable optional values where the format requires them.
* No non-finite numbers.
* No duplicate keys.

On read:

1. Decode the line.
2. Re-encode it canonically.
3. Require byte-for-byte equality with the original line bytes, excluding the terminating newline.

This rejects:

* Duplicate keys hidden by a map parser.
* Alternate field order.
* Ambiguous numeric spelling.
* Unexpected whitespace.
* Unknown extra fields.
* Noncanonical escaped forms.
* Accidental hand edits.

Do not rely solely on a generic `JsonValue` map decoder if it can silently collapse duplicate fields. The canonical-byte check must make that impossible to accept.

### 8.2 Strict schema handling

For format 1:

* Missing required fields are errors.
* Unknown top-level fields are errors.
* Unknown semantic mutation variants are errors.
* Unknown behavior-changing nested fields are errors.
* Invalid enum values are errors.
* Integer overflow is an error.
* Invalid IDs are errors.
* Invalid paths are errors.
* Invalid digest text is an error.
* Invalid UTF-8 is an error.
* Trailing bytes are an error.

Do not silently ignore unknown fields under the banner of forward compatibility. A future incompatible format can become version 2 deliberately.

### 8.3 Bounded parsing

Replay must be streaming.

Do not read the entire session file into memory.

Implement a bounded line reader that:

* Stops allocation at a documented maximum line size.
* Reports an oversized complete line as corruption.
* Treats an oversized partial tail as an incomplete tail.
* Avoids an unbounded `read_line` allocation.
* Includes line number, byte offset, and session path in errors.

Choose and document a maximum line size based on tea’s valid inline semantic records. Large tool output, raw model evidence, and other large payloads must use the artifact store rather than increasing this limit indefinitely.

Do not impose a small fixed maximum total session size. Long sessions must remain streamable.

---

## 9. Add an integrity chain to the session log

Use the BLAKE3 implementation already required by the completed durable harness.

The header digest must be computed over the canonical unsigned header with a domain-separated prefix such as:

```text
tea-session-header-v1
```

Each mutation digest must cover:

```text
domain separator: tea-session-record-v1
session ID
sequence
timestamp
previous digest
canonical mutation payload bytes
```

The first mutation’s `prev_digest` is the header digest.

Each subsequent mutation’s `prev_digest` is the preceding mutation’s digest.

Requirements:

* Sequence begins at 1.
* Sequence values are exactly consecutive.
* Gaps are corruption.
* Duplicate sequence values are corruption.
* A `prev_digest` mismatch is corruption.
* A record digest mismatch is corruption.
* A changed header is detected.
* Interior record deletion is detected.
* Interior insertion is detected.
* Interior reordering is detected.
* Semantically valid JSON bit corruption is detected.
* Timestamp ordering is not required; wall clocks may move.
* Integrity digests are not signatures and must not be described as protection against an attacker able to rewrite the entire file coherently.

Reuse existing canonical hashing machinery where appropriate. Do not create a competing generic hashing framework.

The final record digest becomes the stable identity of the committed prefix and may be reused by:

* `meta.json`.
* Session snapshots.
* Exports.
* Verification reports.
* Benchmark fixtures.

Do not make any derived cache the source of the digest chain.

---

## 10. Make the storage API semantic

The authoritative session API must represent domain transitions rather than generic storage.

The final interface should be conceptually equivalent to:

```rust
trait SessionReader {
    fn header(&self) -> &SessionHeader;

    fn replay(
        &self,
        visitor: &mut dyn FnMut(CommittedMutation) -> Result<(), ReplayError>,
    ) -> Result<ReplaySummary, ReplayError>;
}

trait SessionWriter: SessionReader {
    fn commit(
        &mut self,
        input: MutationInput,
    ) -> Result<CommittedMutation, CommitError>;
}
```

Adapt the exact trait shape to the completed codebase. Preserve executor independence.

`MutationInput` must not allow callers to provide:

* A sequence number.
* A commit timestamp.
* An arbitrary parent for a lane append.
* A new active leaf independent of the semantic mutation.
* A record digest.
* A previous digest.

The store owns those values.

Callers may provision semantic IDs when later durable records must refer to a future result, but the store must validate uniqueness and identity consistency.

A lane append must derive the parent from the lane’s current durable leaf inside the commit. Do not allow stale-parent races.

Avoid a generic mutation batch API. When one transition must be atomic, model it as one typed semantic mutation. Do not implement a transaction mini-language.

Every prefix ending at a complete newline must be reducible or explicitly corrupt. Do not depend on several lines being committed atomically.

---

## 11. Exact commit protocol

For each authoritative mutation:

1. Verify that the writer is not poisoned.
2. Validate the requested transition against the current reduced durable state.
3. Assign the next sequence.
4. Read the injected clock once and assign the timestamp.
5. Derive lane parentage and other store-owned relationships.
6. Construct the canonical mutation.
7. Compute its integrity digest.
8. Encode exactly one canonical JSON object.
9. Append its bytes.
10. Append `\n`.
11. Flush the userspace buffer.
12. Synchronize the file according to the selected durability mode.
13. Only after successful synchronization:

    * Advance the writer’s committed sequence and digest.
    * Apply the mutation to live reduced state.
    * Return success.
    * Emit any corresponding host event.
    * Permit any dependent external effect.

No observer may see a committed event before the durable commit succeeds.

No provider request, tool execution, harness activation, artifact-dependent projection, or operation settlement may begin based on an uncommitted mutation.

Do not rewrite the existing file. Append cost must be proportional to the new record.

### 11.1 Failed and ambiguous writes

A failure after any bytes may have reached the file is not a normal retryable error.

Required behavior:

* Poison the current writer.
* Prevent further commits.
* Prevent subsequent external effects.
* Return an error that distinguishes a pre-write rejection from an indeterminate post-write failure where practical.
* Require close and reopen before continuing.
* Let reopen determine whether the complete mutation exists.

Do not retry an append in place after a partial write or failed synchronization.

A fully present, newline-terminated, valid record discovered after reopen is part of the durable prefix even if the previous process did not observe commit success. This is safe because no dependent external effect was permitted after the failed commit.

---

## 12. Durability modes

Keep the durability policy narrow.

Required modes:

```rust
enum Durability {
    Strict,
    UnsafeBufferedDevelopment,
}
```

Names may vary, but the weakened mode must visibly communicate possible data loss.

### Strict

Strict is the default.

It must use the strongest safe synchronization semantics already available in the repository without adding unsafe code.

At minimum:

* Complete write.
* Userspace flush.
* File synchronization.
* Parent-directory synchronization after publishing new files or rename-created paths where supported.

Document the exact guarantee actually provided on each supported platform. Do not claim that a standard filesystem synchronization call defeats every drive, controller, virtualization, or filesystem failure mode.

If a stronger macOS primitive is already safely available through an existing dependency, use it. Do not add unsafe FFI merely to call it.

### Unsafe buffered development

This mode may reduce synchronization frequency for local testing.

Requirements:

* Never selected implicitly.
* Never the production default.
* Clearly displayed in logs or startup status.
* Never used by crash-consistency tests that claim strict durability.
* Must preserve record framing and semantic validation even though recent records may be lost after process or machine failure.

Do not add several intermediate durability modes without a demonstrated requirement.

---

## 13. Atomic session creation

Session discovery must never observe a half-created session as valid.

Create a new session using a temporary sibling directory:

1. Create an unpublished temporary directory.
2. Create required subdirectories.
3. Write the canonical v1 header.
4. Synchronize the log file.
5. Synchronize required directories.
6. Atomically rename the directory to its final `.tea` name.
7. Synchronize the parent sessions directory where supported.
8. Only then expose the session to the application.

A crash before publication may leave a temporary directory. It is ephemeral and eligible for later cleanup.

A final `.tea` directory without a valid synchronized v1 header is corruption, not an empty session.

---

## 14. Torn-tail recovery and corruption

Use the newline as the record publication delimiter.

On open:

* Bytes after the last newline are an uncommitted torn tail.
* A final JSON object that appears complete but lacks its newline is still uncommitted.
* Read-only open reports the torn tail and does not mutate the file.
* Read-write recovery may truncate exactly those trailing bytes.
* After truncation, synchronize the file before continuing.
* Preserve the complete committed prefix exactly.

A newline-terminated malformed record is corruption, even when it is the last line.

Do not silently truncate:

* A malformed complete line.
* A checksum-invalid complete line.
* A noncanonical complete line.
* A complete line with an unknown kind.
* A complete line with an invalid reference.
* A complete line that causes reducer corruption.

Do not scan forward to find another plausible record. Do not attempt resynchronization after interior corruption.

Corruption errors must include:

* Session path.
* Line number.
* Byte offset.
* Sequence when available.
* Record kind when available.
* The violated invariant.
* Whether any automatic repair is safe.

---

## 15. Pure replay and fixed-point state

Preserve one pure reducer as the only interpretation of the committed mutation stream.

Replay must:

1. Validate the header.
2. Validate canonical line bytes.
3. Validate consecutive sequence.
4. Validate the digest chain.
5. Decode the semantic mutation.
6. Apply it through the pure reducer.
7. Build any in-memory indexes incrementally.
8. Return the reduced state and replay summary.

The replay path performs no:

* Provider calls.
* Tool calls.
* Hook execution.
* Clock reads.
* Network access.
* Harness activation.
* Artifact mutation.
* Cache mutation unless explicitly requested after successful replay.

After every important lifecycle boundary already required by the durable harness, retain the assertion:

```text
live reduced state == fresh reduction from durable storage
```

This includes:

* Session creation.
* Session restore.
* Recovery completion.
* Operation acceptance.
* Effect settlement.
* Suspension.
* Harness activation.
* Core-run rollover.
* Compaction commit.
* Operation termination.

Use one equality or canonical-state comparison path. Do not maintain a weakened test-only reducer.

---

## 16. Content-addressed object storage

The object store contains exact immutable bytes.

For raw artifact bytes:

```text
ArtifactId = BLAKE3(exact stored bytes)
```

Store raw bytes at an identity-derived path such as:

```text
objects/blake3/<first-two-hex>/<full-lowercase-hex>
```

Do not wrap raw artifact bytes in a mutable metadata envelope. Store metadata such as media type and byte length in the durable record that references the object.

### 16.1 Object publication protocol

For a new object:

1. Create a unique temporary file in the destination filesystem.
2. Stream bytes into it while computing BLAKE3.
3. Complete the write.
4. Flush.
5. Synchronize the file.
6. Determine the final digest-derived path.
7. Create required digest-prefix directories.
8. Atomically publish the file at the final path.
9. Synchronize the containing directory where supported.
10. Return the verified object identity.

Only after object publication succeeds may a durable session mutation reference it.

Consequences:

* Crash before object publication: temporary orphan.
* Crash after publication but before log reference: unreferenced immutable orphan.
* Crash after log reference: referenced object must already exist.
* A committed mutation must never reference bytes that were not first durably published.

If an identical object already exists:

* Verify that it is a regular file.
* Verify expected byte length.
* Verify its content digest when necessary to establish correctness.
* Treat it as idempotent success.
* Never overwrite it with different bytes.

Reject:

* Symlinks at object paths.
* Directories at object paths.
* Digest/path disagreement.
* Existing files with the wrong bytes.
* Path traversal.
* Noncanonical digest paths.

### 16.2 Reading and verification

Ordinary bounded artifact reads should not require scanning every object or every historical artifact.

Provide explicit full verification that:

* Recomputes object digests.
* Validates lengths.
* Checks every required reference.
* Reports unreferenced objects separately from corrupt referenced objects.

An orphan object is not session corruption.

A missing or invalid object referenced by active semantic state is corruption.

---

## 17. Immutable harness manifests

Preserve the completed harness identity design:

* Source blobs are immutable.
* Tree manifests are immutable.
* Harness snapshots are immutable.
* Revisions are immutable.
* Candidates are immutable after staging.
* Paths are derived from content identities or immutable IDs.
* Active state is derived from the session log.

Every content identity must be computed from an explicit canonical representation with:

* Domain separation.
* Fixed field order.
* Explicit discriminants.
* Length-prefixed bytes.
* Sorted maps.
* Normalized paths where the contract requires path normalization.
* Exact source bytes where source identity is defined over exact bytes.

Do not hash ordinary JSON map output whose ordering is merely incidental.

Do not mutate an immutable manifest in place. A changed manifest is a new object with a new identity.

---

## 18. `HEAD` is only a cache

`HEAD` may accelerate active harness lookup, but it has no authority.

On open:

* Derive the active harness revision from the committed session branch.
* Compare any `HEAD` value with the derived result.
* If it matches, retain it.
* If it is missing, stale, malformed, or inconsistent, discard and rebuild it.
* Never change semantic state to agree with `HEAD`.

Write `HEAD` only after the authoritative mutation selecting the revision is durable.

Write it using:

1. Temporary file.
2. Complete canonical content.
3. Flush.
4. File synchronization when appropriate.
5. Atomic rename.
6. Parent-directory synchronization where supported.

Failure to update `HEAD` must not roll back an already successful authoritative commit. Surface a diagnostic and allow later reconstruction.

---

## 19. Add a dedicated disposable `meta.json`

Session discovery must not replay every session log.

Create one small derived metadata cache per session containing only bounded listing information, conceptually:

```rust
struct SessionMetadata {
    version: u32,
    session_id: SessionId,
    workspace: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    display_title: Option<String>,
    status: SessionDisplayStatus,
    active_lane: LaneId,
    through_seq: Sequence,
    through_digest: Digest,
    latest_snapshot: Option<SnapshotLocator>,
}
```

This is not authoritative.

Requirements:

* It must be reconstructible from the session log.
* It must identify the committed prefix through which it was derived.
* It must be atomically replaced.
* A stale cache is acceptable.
* A missing or corrupt cache must not prevent opening the session.
* A cache mismatch never overrides the log.
* Session listing reads bounded metadata and directory names, not complete logs.
* Opening a selected session rebuilds stale metadata.
* A maintenance command may rebuild all metadata caches explicitly.

Do not update `meta.json` after every low-level record if doing so doubles synchronous write amplification.

Update it at stable high-level boundaries such as:

* Session creation.
* Operation terminal settlement.
* Harness activation.
* Session title change.
* Clean suspension.
* Clean shutdown.

The exact policy must be deterministic and documented.

Cache-write failure is visible but does not retroactively invalidate the authoritative commit.

---

## 20. Build indexes during replay

Build required session indexes incrementally while replaying:

* Entry ID lookup.
* Parent and child relationships needed by the exposed branch operations.
* Lane leaf.
* Operation lookup.
* Open operation.
* Tool invocation identity.
* Provider request identity.
* Harness revision lookup.
* Artifact references.
* Usage totals.
* Searchable history spans already required by the completed system.

Do not rescan the complete log or every artifact for each provider request.

Prefer in-memory indexes scoped to an open session.

Do not persist an index merely because it is possible.

A persisted offset or search index is acceptable only when benchmarks demonstrate material value. It must then include:

* Session ID.
* Format version.
* Through-sequence.
* Through-digest.
* Index schema identity.

Any mismatch causes deletion or rebuild, never session failure.

Do not introduce SQLite for session listing, search, or offset lookup.

---

## 21. Reevaluate session snapshots empirically

The completed `plan.md` implementation may already contain snapshots. Do not assume they justify their complexity.

First measure:

* Full canonical replay.
* Full reduction.
* Snapshot load.
* Tail replay.
* Snapshot write amplification.
* Snapshot disk size.
* Snapshot code contribution.
* Failure and corruption paths.

Retain snapshots only if they materially improve representative long-session reopen performance.

A reasonable acceptance gate is:

* At least a 2× median reduction in reopen time on the representative long-session fixture.
* At least 50 ms absolute savings on the same fixture.
* No new runtime dependency.
* No loss of corruption detection.
* No more than a small measured release-binary increase.
* No requirement to migrate snapshots across reducer changes.

If the gate is not met, remove or disable snapshot production and rely on full streaming replay. Simpler is preferable.

### 21.1 Snapshot semantics when retained

Snapshots are disposable caches.

A snapshot must contain:

```rust
struct SessionSnapshot {
    version: u32,             // 1
    session_id: SessionId,
    through_seq: Sequence,
    through_digest: Digest,
    reducer_schema: ReducerSchemaId,
    log_byte_offset: u64,
    state: ReducedSessionState,
    payload_digest: Digest,
}
```

Requirements:

* Create snapshots only at stable reducer fixed points.
* Never snapshot an unresolved in-process task handle.
* Never embed large artifact bytes.
* Write using temporary file, synchronization, and atomic rename.
* Validate session ID, sequence, digest, reducer schema, and payload digest before use.
* Any mismatch causes fallback to replay.
* Snapshot corruption is not session corruption.
* Keep a bounded number of snapshots.
* Garbage collection may delete every snapshot.
* No migration code is required for obsolete snapshot schemas.

Do not add `rkyv` in this pass.

If a future experiment evaluates `rkyv`, it may only be used for disposable snapshots and must include an exact schema fingerprint. It must never become the only representation of durable state.

---

## 22. Single-writer enforcement

One process may own the writable session at a time.

Use a real operating-system file lock through an existing safe dependency or existing repository abstraction.

Do not use a `create_new` PID file as the sole lock because crash-stale lock files are difficult to distinguish safely.

Required behavior:

* Writable open obtains an exclusive lock before repair or append.
* A second writer receives a precise `AlreadyOpen` error.
* Lock release occurs automatically when the owning handle/process exits.
* The lock does not contain semantic state.
* Session listing does not require the writer lock.
* Garbage collection, destructive repair, and restore require exclusive ownership.
* Export must either acquire a consistent read lock or coordinate through the active writer.
* Do not claim support for concurrent cross-process live replay unless it is fully tested.

Within one process, keep one serialized commit path. Do not allow several tasks to append independently to the same file handle.

---

## 23. Explicit operational verification and repair

Provide focused operator commands through the existing CLI structure. Adapt exact command names to current conventions.

At minimum support the equivalent of:

```text
tea session verify <session>
tea session repair-tail <session>
tea session rebuild-meta <session>
tea session gc <session> --dry-run
```

### Verify

Read-only and exhaustive:

* Validate header.
* Validate canonical bytes.
* Validate sequence.
* Validate integrity chain.
* Run full reduction.
* Validate required object references.
* Recompute referenced object digests.
* Validate immutable manifests.
* Validate snapshot/cache consistency without trusting them.
* Report orphan objects separately.
* Print final sequence and prefix digest.

### Repair tail

May only:

* Remove bytes after the last complete newline.
* Synchronize the repaired file.
* Rebuild derived caches.

It must not repair a complete malformed line.

Prefer copying the original file or recording exact pre-repair size before destructive truncation when that can be done cheaply and predictably.

### Rebuild metadata

Replays authority and atomically recreates `meta.json` and `HEAD`.

### Garbage collection

Dry-run by default unless current CLI conventions make an explicit destructive subcommand clearer.

Never describe deletion of referenced evidence as repair.

---

## 24. Garbage collection

GC roots must come from authoritative state, never derived caches.

Roots include all objects reachable from:

* Every retained semantic session branch.
* Active and rollback-capable harness revisions.
* Retained harness candidates.
* Compaction recovery indexes.
* Raw evidence pinned by semantic entries.
* Retained exports.
* Active experiments and evaluation decisions where applicable.

GC may remove:

* Unreferenced finalized objects.
* Old temporary files.
* Reconstructible worktrees.
* Invalid abandoned candidates after explicit retention rules.
* Disposable snapshots.
* Disposable metadata and indexes.

GC must run under an ownership protocol that prevents this race:

```text
writer publishes object
GC sees no log reference yet
GC deletes object
writer commits reference
```

The simplest acceptable design is session-scoped GC under the exclusive session lock.

Alternative designs are acceptable only if their race freedom is proven by tests.

GC must:

1. Capture the authoritative root set.
2. Walk only known object namespaces.
3. Reject symlink traversal.
4. Produce a deterministic deletion plan.
5. Support dry-run.
6. Recheck required ownership immediately before deletion.
7. Never silently delete active-session evidence because of age or quota.

An orphan object is safe to delete only after the race with a possible pending commit is excluded.

---

## 25. Export and restore

Preserve or improve exact session export.

An export must contain:

* The complete authoritative log through a selected committed sequence.
* The header digest.
* The selected final sequence and record digest.
* Every immutable object reachable from that prefix.
* Every required immutable harness manifest.
* A canonical export manifest listing all included identities and lengths.
* No lock files.
* No temporary files.
* No derived metadata required for correctness.
* No unreferenced objects unless explicitly requested.

Export must operate over a consistent committed prefix.

Restore must:

1. Verify the export manifest.
2. Verify the complete log.
3. Verify the digest chain.
4. Verify all included object digests.
5. Verify all required references.
6. Restore into an unpublished temporary session directory.
7. Publish atomically.
8. Rebuild all derived metadata locally.

Do not trust exported `meta.json`, `HEAD`, snapshots, or worktrees as authority.

---

## 26. Security and privacy constraints

Persistence contains source code, prompts, model output, tool arguments, tool results, and execution evidence. Treat it as sensitive.

Requirements:

* Apply the completed redaction policy before durable storage where redaction is required.
* Never store provider credentials or authorization headers in session records.
* Never put secrets into filenames.
* Never expose arbitrary host paths through object IDs.
* Reject path traversal and symlink escape.
* Keep object IDs independent of original filenames.
* Bound all reads exposed to the model.
* Preserve the existing rule that reading an artifact cannot recursively spill another inaccessible artifact.
* Ensure errors do not print complete sensitive payloads.
* Verification errors may print IDs, paths, offsets, lengths, and schema names, but not complete message or artifact contents by default.

Do not implement custom encryption. Rely on filesystem permissions and document that the store is not encrypted by tea.

---

## 27. Keep analytical databases out of the core binary

The normal `tea` executable must not link a database engine.

If the completed implementation has a separate `tea-evolve` or offline evaluation binary with genuinely relational workloads, leave that boundary separate.

It is acceptable for a separate analytical binary to use a database in the future, provided:

* It does not share writable authority with the running target agent.
* It consumes immutable exports, traces, or experiment records.
* Its dependencies are not linked into the normal `tea` executable.
* It cannot mutate authoritative session history.
* Its absence does not prevent session recovery.

Do not add such a database as part of this task.

---

## 28. Do not introduce a second production codec

JSONL v1 remains the authoritative production format for this task.

Do not add a production CBOR, bincode, postcard, or `rkyv` session backend merely because it is more compact.

Instead, leave the domain/store boundary clean enough that another backend could be benchmarked later without changing harness semantics.

Document the evidence required before replacing JSONL:

* Material release-binary impact.
* Material log-size reduction.
* Material replay-time reduction.
* Material peak-memory reduction.
* Equal or better corruption diagnosis.
* Equal or simpler schema handling.
* No weakening of crash semantics.
* No tight coupling between Rust memory layout and durable history.

The benchmark must compare synchronization cost separately from codec CPU cost. An `fsync`-dominated benchmark must not be presented as evidence that codecs are equivalent.

Do not retain experimental alternate-codec code in the production crate unless it wins a separately approved evaluation.

---

## 29. Performance and binary-size requirements

Add reproducible generated fixtures representing at least:

### Small

* One user operation.
* One assistant response.
* Several operation records.
* No large artifacts.

### Medium

* Thousands of semantic entries.
* Thousands of operation records.
* Repeated tool use.
* Repeated compaction.
* Several harness revisions.
* Hundreds of artifacts.

### Long

* At least tens of thousands of total mutations.
* Enough data to expose whole-file reads, quadratic reduction, or unnecessary allocation.
* Large tool outputs stored in CAS rather than inline.
* A large accumulated usage ledger.

### Session directory set

* Hundreds or thousands of session directories.
* Valid metadata caches.
* Missing metadata caches.
* Stale metadata caches.
* Corrupt metadata caches.

Measure:

* Canonical encode time excluding synchronization.
* Strict append latency including synchronization.
* Buffered append throughput.
* Full verify time.
* Full replay/reduction time.
* Snapshot-assisted reopen time if retained.
* Peak memory during replay.
* JSONL bytes.
* Object-store bytes.
* Session-list latency.
* Release `tea` binary size.
* Persistence-related linked dependencies.

Required asymptotic behavior:

* Append writes O(new record bytes), not O(session bytes).
* Replay reads O(log bytes).
* Replay memory is O(reduced live state plus indexes), not O(raw log bytes).
* Session listing reads bounded metadata, not every log.
* Artifact lookup is direct by digest.
* Provider context construction does not rescan every artifact.
* Cache rebuild is linear and explicit.
* GC is linear in known roots and object entries.
* No hidden whole-session serialization occurs on every commit.

Do not use microbenchmark improvements to justify code that weakens correctness or substantially increases binary size.

---

## 30. Deterministic test program

Use fixture-first development. Add the failing fixture before each behavior.

### 30.1 Golden wire fixtures

Preserve exact committed bytes for:

* Header.
* Each top-level mutation class.
* Representative semantic entry variants.
* Representative operation records.
* Artifact reference.
* Harness revision transition.
* Terminal operation outcome.

Assert exact bytes, including field order and newline.

Round-trip testing alone is insufficient.

### 30.2 Canonical JSON rejection

Test rejection of:

* Duplicate keys.
* Reordered fields.
* Additional whitespace.
* Alternate numeric forms.
* Unknown fields.
* Unknown variants.
* Missing fields.
* Invalid UTF-8.
* Trailing data.
* CRLF when canonical format requires LF.
* Oversized lines.
* Invalid IDs.
* Invalid digest spelling.

### 30.3 Integrity-chain corruption

Test:

* Header bit flip.
* Payload bit flip that remains valid JSON.
* Sequence bit flip.
* Timestamp bit flip.
* Digest bit flip.
* Previous-digest bit flip.
* Record deletion.
* Record duplication.
* Record reordering.
* Record insertion from another session.
* Sequence gap.
* Sequence zero.
* Correct JSON with incorrect semantic references.

Every case must fail at a deterministic line and invariant.

### 30.4 Torn-write matrix

For representative files, truncate at every byte offset.

Assert:

* Truncation after a newline yields the exact complete prefix.
* Truncation inside the next line is recognized as an incomplete tail.
* Read-only open does not mutate.
* Read-write repair truncates only the incomplete tail.
* The repaired file reopens cleanly.
* A complete newline-terminated invalid line is never truncated automatically.

### 30.5 Write-failure matrix

Inject failure at:

* Before append.
* During every byte position of a short append.
* After JSON bytes but before newline.
* After newline but before flush.
* During flush.
* After flush but before synchronization.
* During synchronization.
* After synchronization but before returning success.
* During derived metadata update.

Assert:

* The writer becomes poisoned after an indeterminate failure.
* No dependent effect begins.
* Reopen produces either the prior prefix or the prefix including the complete new record.
* No hybrid record is accepted.
* Metadata failure does not rewrite authority.

### 30.6 Artifact publication matrix

Kill or inject failure:

* Before temporary creation.
* During object write.
* Before file sync.
* After file sync.
* Before rename.
* After rename.
* Before directory sync.
* After directory sync.
* Before log reference.
* After log reference.

Assert:

* A committed reference always resolves.
* An unreferenced published object is merely an orphan.
* A partial temporary file is never mistaken for an object.
* Identical objects deduplicate.
* Conflicting existing bytes fault.

### 30.7 Session creation matrix

Terminate after each creation step.

Assert:

* No published session appears without a valid v1 header.
* Temporary directories are ignored.
* Repeated cleanup is idempotent.

### 30.8 Locking

Test:

* One writer succeeds.
* A second writer fails.
* Lock release after normal close.
* Lock release after child-process termination.
* GC cannot run concurrently with a writer.
* Export sees a consistent prefix.
* Session listing remains available.

### 30.9 Cache deletion and corruption

For `meta.json`, `HEAD`, persisted indexes, and snapshots:

* Delete each one independently.
* Empty it.
* Truncate it.
* Replace it with valid but stale content.
* Replace it with another session’s content.
* Give it a future schema version.

Assert:

* Authoritative replay remains correct.
* Stale caches never change session state.
* Rebuild produces deterministic bytes.
* Snapshot failure falls back to replay.
* Missing cache never becomes session corruption.

### 30.10 Reducer fixed point

After every committed mutation in generated sessions:

1. Capture live reduced state.
2. Close.
3. Reopen.
4. Freshly replay.
5. Compare exact reduced state.

Repeat across:

* Provider retries.
* Tool retries.
* Non-replay-safe interruption.
* Parallel tool-result source order.
* Deferred writes.
* Abort.
* Compaction.
* Harness activation.
* Rollback.
* Core-run rollover.
* Operation settlement.

### 30.11 No-effect-before-intent

Retain the completed harness’s writer-conformance and manual-effect-gate matrices.

Instrument exact ordering and assert that no external effect begins before its required intent record has completed strict commit.

### 30.12 Append-only behavior

Add an instrumented writer test proving:

* Commit writes only the new canonical line and newline.
* Existing prefix bytes remain unchanged.
* The store does not serialize the entire reduced session.
* The store does not replace `session.jsonl`.
* Commit allocation is bounded by the new record plus fixed overhead.

### 30.13 Session listing behavior

Use an instrumented filesystem or narrow spy abstraction.

Assert that listing a large session directory:

* Reads directory entries.
* Reads bounded metadata files.
* Does not open every `session.jsonl`.
* Does not traverse every object directory.
* Handles missing metadata predictably.

### 30.14 GC

Test:

* Exact root computation.
* Orphan deletion.
* Referenced object retention.
* Snapshot deletion.
* Worktree deletion.
* Symlink rejection.
* Dry-run equivalence.
* Repeated GC idempotence.
* No writer/GC publication race.

### 30.15 Export and restore

Test:

* Exact prefix export.
* Reachable-object inclusion.
* Unreachable-object exclusion.
* Manifest corruption.
* Missing object.
* Wrong object bytes.
* Atomic restore publication.
* Derived cache reconstruction.
* Restored reducer equality.

Avoid nondeterministic sleeps. Use explicit failpoints and child-process coordination.

---

## 31. Implementation sequence

Keep every phase compiling and passing all earlier tests.

### Phase 0 — Baseline and authority map

* Capture baseline.
* Inventory persisted files.
* Identify compatibility code.
* Identify authority ambiguities.
* Confirm actual verification commands.

Gate:

* Existing suite passes before edits.
* Baseline is recorded.
* No implementation changes yet.

### Phase 1 — Clean v1 reset

* Remove v3/v4 persistence code.
* Remove import paths.
* Rename documentation.
* Set sole format version to 1.
* Add unsupported-version error.
* Remove obsolete fixtures.

Gate:

* No persistence compatibility branch remains.
* Existing current-format fixtures are rewritten as v1.
* Workspace compiles and tests pass.

### Phase 2 — Explicit wire boundary

* Separate wire types from domain types.
* Implement canonical JSON encoding.
* Implement strict bounded decoding.
* Add exact golden fixtures.
* Remove duplicated envelope fields.

Gate:

* Golden bytes pass.
* Canonical rejection suite passes.
* No full-file replay allocation remains.

### Phase 3 — Integrity chain

* Add header digest.
* Add previous-record and record digests.
* Enforce consecutive sequences.
* Integrate prefix identity with replay and exports.

Gate:

* Complete corruption matrix passes.
* Existing semantic fixtures retain the same reduced meaning.

### Phase 4 — Commit and recovery hardening

* Make storage assign sequence, timestamp, parent, and digest.
* Enforce exact append/flush/sync ordering.
* Poison writer after ambiguous failure.
* Correct torn-tail handling.
* Make atomic session creation explicit.

Gate:

* Byte-offset truncation matrix passes.
* Write-failure matrix passes.
* No external effect can cross an uncommitted boundary.

### Phase 5 — Object-store hardening

* Verify exact publication order.
* Enforce immutable object paths.
* Reject symlinks and conflicting bytes.
* Integrate object verification.

Gate:

* Artifact publication crash matrix passes.
* Every committed reference resolves.
* Duplicate content is idempotent.

### Phase 6 — Derived state cleanup

* Add or harden `meta.json`.
* Make `HEAD` provably disposable.
* Build indexes during replay.
* Benchmark snapshots.
* Remove snapshots if they fail the value gate.
* Harden retained snapshots if they pass.

Gate:

* Deleting all derived files preserves recovery.
* Session listing does not replay all sessions.
* Snapshot decision is supported by measurements.

### Phase 7 — Locking, verification, GC, export

* Enforce one writer.
* Add verify and repair commands.
* Make GC race-free.
* Verify exact exports and restores.

Gate:

* Lock child-process tests pass.
* GC matrices pass.
* Export/restore fixed-point equality passes.

### Phase 8 — Performance and size

* Run all representative fixtures.
* Compare before and after.
* Inspect release dependency tree.
* Inspect release binary size.
* Remove unnecessary abstractions and dependencies.
* Confirm there is no hidden database or alternate codec.

Gate:

* Required asymptotic behavior is demonstrated.
* No unexplained binary-size increase remains.
* JSONL and object-store measurements are documented.

### Phase 9 — Documentation and final cleanup

* Complete persistence inventory.
* Complete session-format v1 documentation.
* Update recovery, export, GC, and durability documentation.
* Remove stale comments and dead code.
* Update `plan.md` persistence sections to the implemented v1 reality.

Gate:

* Code, tests, docs, and CLI behavior agree.
* Search finds no stale v3/v4 persistence language.
* Full verification suite passes.

---

## 32. Required documentation

Produce or update:

```text
docs/persistence.md
docs/session-format-v1.md
docs/persistence-recovery.md
docs/persistence-benchmarks.md
```

Fold these into existing documents when that produces a cleaner documentation hierarchy, but do not omit the content.

Document:

* Persistence authority inventory.
* Exact v1 directory layout.
* Exact header and mutation schemas.
* Canonical JSON rules.
* Integrity digest definitions.
* Sequence allocation.
* Commit ordering.
* Synchronization semantics.
* Platform-specific durability limitations.
* Single-writer locking.
* Torn-tail recovery.
* Interior corruption behavior.
* Object publication ordering.
* Artifact identity.
* `HEAD` and metadata-cache semantics.
* Snapshot decision and measurements.
* Index reconstruction.
* GC roots.
* Export and restore.
* Privacy and file permissions.
* Explicit non-guarantees.
* Future format-version policy.
* Why Rust types are not themselves the wire format.
* Why SQLite and embedded KV engines are absent from the core.
* Why `rkyv` is not authoritative.
* What evidence would justify a future alternate codec.

Do not leave active docs describing discarded v3 or v4 formats.

---

## 33. Verification commands

Inspect and use the repository’s actual current commands.

At minimum run the equivalents of:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- --deny warnings
cargo test --workspace --all-features
git diff --check
make quality-fast
make quality-resources
python3 -m evals.quality compaction
```

Also run focused commands for:

```text
session wire golden fixtures
canonical JSON rejection
integrity corruption matrix
torn-tail matrix
write-failure matrix
artifact publication matrix
single-writer locking
metadata and snapshot deletion
GC
export and restore
long-session replay benchmarks
session-list benchmarks
release binary-size comparison
```

Do not require a live model provider or local inference backend for persistence acceptance.

Do not weaken existing tests, increase arbitrary timeouts, or bless changed golden bytes without inspecting each semantic difference.

Run a focused source search proving the cleanup, scoped to persistence code and docs:

```text
v3
v4
legacy session
Pi-compatible session
session import
format conversion
```

Legitimate unrelated protocol terms must not be changed merely because they contain similar text.

---

## 34. Definition of done

The task is complete only when all of the following are true:

1. Tea supports exactly one session format: version 1.
2. No old session importer or compatibility path remains.
3. Session authority is one append-only JSONL log plus referenced immutable objects.
4. Every complete mutation is canonically encoded.
5. Every complete mutation participates in a verified BLAKE3 integrity chain.
6. Sequence allocation occurs only inside the store commit.
7. Lane parentage is assigned inside the commit.
8. Appending never rewrites the full session.
9. Replay is streaming and bounded.
10. Only an incomplete non-newline tail may be automatically truncated.
11. Every complete malformed line faults closed.
12. Every dependent external effect begins after its durable intent.
13. Artifact publication precedes every durable reference.
14. `meta.json`, `HEAD`, indexes, worktrees, and snapshots are disposable.
15. Deleting all derived state still permits exact recovery.
16. Session listing does not replay every session.
17. One writer is enforced by an operating-system lock.
18. GC cannot race object publication.
19. Export and restore preserve exact reduced state.
20. No database engine is linked into the normal `tea` binary.
21. No `rkyv` dependency is added.
22. No unsafe code is added.
23. Release binary size and dependency changes are reported.
24. Crash, corruption, and recovery matrices pass.
25. Live reduced state equals fresh replayed state at every required fixed point.
26. Documentation matches the implementation.
27. The full existing harness, compaction, Luau, evaluation, and quality suites remain passing.
28. No placeholder or knowingly incomplete persistence path remains.

---

## 35. Final response

When finished, report:

1. The final authority model.
2. The final on-disk layout.
3. The exact v1 wire contract.
4. Old compatibility code removed.
5. The integrity and durability protocol.
6. Snapshot decision and benchmark evidence.
7. Release binary size before and after.
8. Release dependency-tree changes.
9. Replay, listing, and storage benchmark results.
10. Crash and corruption matrices implemented.
11. Verification commands run and their results.
12. Any platform-specific durability limitation that remains.
13. Any deliberate deviation from this prompt and the concrete reason.

Do not claim completion if any required crash boundary, corruption case, or fixed-point invariant remains untested.

