# Session format v1

tea-session owns Tea's only durable session format. A session is a new
directory containing:

~~~
<session>.tea/
  session.jsonl
  HEAD
  meta.json              # host-derived picker cache, when a host writes one
  objects/
~~~

`session.jsonl` is the authoritative append-only v1 log. Its sealed header
identifies the format as `tea-session-jsonl`; a header from the retired
`kind:"session"` shape is incompatible and is never imported or repaired.
Every committed semantic group then names one sequence, timestamp, previous
digest, ordered nonempty payload, and resulting digest. The reader accepts
canonical JSON only, validates the closed v1 schema by lossless decode/re-encode,
and checks consecutive commit sequence and digest-chain links. Checked-in wire
fixtures preserve exact bytes for the header, a user entry, and representative
artifact, lane, operation, harness revision, and terminal-result groups. The
rejection matrix covers noncanonical ordering, duplicate fields, whitespace,
numeric spelling, CRLF, invalid text, schema violations, invalid identities,
and retired format identities. Integrity corruption tests cover header and
envelope changes, deletion, duplication, reordering, foreign insertion,
sequence failure, and a correctly sealed group that the pure reducer still
rejects.

`HEAD` is a disposable, atomically replaced active-harness cache. It names its
session by sealed header digest and session ID plus the current main-lane
harness revision; it intentionally does not mirror each newest log prefix.
It is written after session creation and after a durable main-lane harness
revision selection, so ordinary record commits do not add a second synchronous
replace. Opening a validated log retains an exact cache or rebuilds a missing,
malformed, stale, or foreign one. It is never consulted as authority. Terminal
hosts may additionally write a bounded `meta.json` picker cache. Version 1
records the sealed header digest plus `created_at_ms`, `active_lane`,
`through_seq`, and `through_digest` for the exact committed prefix it
summarizes. A missing, malformed, stale, or foreign cache falls back to the
directory-derived summary; opening the session reconstructs it from the v1
log. Both `HEAD` and `meta.json` are exercised as missing, empty, truncated,
foreign, and future-schema inputs; none can alter authoritative replay.
The object store holds immutable content-addressed bytes referenced by semantic
facts.

The terminal host allocates new session IDs as 64 lowercase hexadecimal characters
derived from its allocation digest; newly created IDs have no `session-` prefix.
Older sessions whose IDs include that prefix remain valid opaque IDs and are not
renamed during inspection or reopen.

Host-created headers also retain `tea.build.version` and `tea.build.git_sha`.
Read-only session reports expose those values as `tea_version` and `tea_git_sha`,
or `null` for sessions created before build identity was recorded.

## Wire, integrity, and commit contract

The header is one object with exactly these fields: kind, format, version,
session_id, created_at_ms, workspace, metadata, initial_lane, and digest.
`kind` is `tea-session`, `format` is `tea-session-jsonl`, and version is the
sole supported value, 1. Every subsequent line is a commit envelope with
exactly seq, timestamp_ms, prev_digest, mutation, and digest. Its `mutation`
is a `commit` object with one nonempty, ordered `items` array. Each item has
one of the closed `entry`, `record`, `lane`, or `fact` kinds; those versioned
payload schemas are encoded by the narrow JSONL boundary in tea-session,
rather than by serializing Rust domain structs directly.

Tea writes UTF-8, minified JSON with one LF per complete line. Object keys and
metadata maps are deterministically ordered, required nullability is explicit,
and integers use the schema's canonical unsigned representation. Reading first
parses, then re-encodes, and requires byte-for-byte equality before reduction.
Thus a hand edit, duplicate key, alternate field order, extra whitespace,
CRLF, unknown field, missing field, or alternate numeric spelling cannot become
durable history. A complete line is limited to 2 MiB; large evidence belongs in
the immutable artifact store.

The header digest is BLAKE3 over the canonical unsigned header through the
length-delimited `tea-session-header-v1` domain. A commit digest uses the
`tea-session-commit-v1` domain and length-delimited session ID, sequence,
timestamp, previous digest, and canonical complete commit-payload bytes. The
first commit names the header digest and later commits name the preceding
commit.
These digests detect accidental or partial corruption; they are not signatures
and do not protect against an attacker able to coherently replace the entire
directory.

`SessionWriter::commit` validates the complete semantic transition, assigns the
next consecutive sequence and one clock reading to every item, derives ordered
lane parentage, seals and writes one line plus LF, flushes, and in strict mode
synchronizes the file. Only then does it update live reduction state or permit
a dependent effect. The convenience `append_*` methods are one-item commits;
there is no parallel single-item wire format. A post-write failure faults that
writer: it must be closed and reopened, which decides whether the complete
line became part of the prefix. Development durability is explicitly buffered
and can lose recent acknowledged data; strict mode uses the available file and
directory synchronization calls but cannot promise hardware behavior beyond the
operating system's guarantees.

Failures before a non-empty commit append attempt remain ordinary I/O
rejections; the same writer retains its validated prefix and may retry a later
transition. Once the writer has attempted to append bytes, flush, or
synchronize the log, Tea reports an indeterminate-write error and faults that
handle. Reopening is the only way to determine whether the complete
newline-terminated commit joined the authoritative prefix; it never retries
that commit in place. Thus a failed batch is never surfaced as a durable
semantic prefix through that writer: recovery sees either its preceding prefix
or all ordered items.

On supported Unix platforms Tea creates session directories as owner-only and
durable files as owner read/write. The store is not encrypted, and operator
filesystem access remains outside Tea's threat model. The core deliberately
does not link SQLite, an embedded key/value engine, or rkyv: JSONL keeps schema
diagnosis and crash behavior narrow. A future codec would need measured binary,
size, replay, memory, and corruption-diagnosis improvements without weakening
these semantics.

## Creation and recovery

`JsonlSession::create` prepares a private sibling directory, seals and syncs
the header and caches there, then renames it into place; it refuses an existing
destination through atomic no-replace publication. `JsonlSession::open`
accepts only a v1 header and verifies the complete committed prefix. It does
not interpret another format, import a
transcript, silently create a replacement session, or mutate a torn log.

The creation interruption matrix covers the private directory, layout, header,
cache, strict directory sync, publication, and parent sync boundaries. Before
publication it leaves no candidate directory; after publication it may return
an interrupted result but leaves only a complete v1 directory that inspection
and reopening can validate.

Writable opens hold an operating-system exclusive lock on `session.jsonl`.
Another writer fails while the owner is live; the lock is released by normal
close and by child-process termination, without a PID-file recovery path.

`JsonlSession::inspect` is the read-only replay path. If it reports an
unterminated final tail, `JsonlSession::repair_torn_tail` is the only repair
operation: it holds the writer lock and truncates exactly that suffix. A
newline-terminated malformed line, noncanonical bytes, unknown schema field,
bad digest, or non-consecutive sequence is corruption, not a repair case.
The required header is never a repairable tail: an incomplete or missing
header leaves the session directory invalid and requires operator recovery,
rather than truncation to an empty log.

Format diagnostics always name the path, line, byte offset, and violated
invariant. Once a commit envelope has decoded, they also retain its sequence
and `commit` kind without printing the potentially sensitive payload. Syntax
errors before those fields are available report the same location with that
context absent.

The storage test matrix injects interruptions before append, after every byte
of a representative JSON commit, after the JSON body, after the newline,
during flush, and on both sides of strict synchronization. An interrupted
writer is faulted for the rest of its lifetime. Reopening accepts only the
prior prefix or the complete newline-terminated commit; an unterminated
suffix remains an explicit repair decision.

`SessionHeader::new` creates the required main lane; `SessionHeader::new_at`
and the `SessionClock` constructors make creation and commit timestamps
reproducible in fixtures. The current schema constant is
`SESSION_FORMAT_VERSION = 1`; `SESSION_FORMAT_IDENTITY` is
`tea-session-jsonl`; every commit is bounded to 2 MiB and an incompatible
header is rejected before commit decoding or repair.

`provider_request_settled` records may carry a `provider_error` object. Its
`source`, optional `message`, `status_code`, attempt number, retry
classification, request and response byte counts, and `response_body` are
trusted-host diagnostics. The provider adapter must redact credentials and
bound the response text before writing it; an absent field preserves the
original v1 wire shape and digest of older settlements. Before every physical
provider invocation, the host commits `provider_request_started` and the exact
`ProviderRequestMaterialFact` in one semantic commit. Under the supervised
writer contract, that pair is the durable admission boundary: a request intent
with no material was never admitted, while a materialized un-settled request is
indeterminate and must be reconciled rather than retried from inference.
`CompactionEntry.provider_request_id` is valid only once, for the exact
`StepKind::Compaction` request's durable `Completed` settlement; an interrupted,
discarded, retryable, or ordinary assistant request cannot change reconstructed
context.

## Inputs, extension state, and turn checkpoints

`InputAcceptedRecord` stores the final user `ProvisionedEntry` before dispatch;
its entry ID is the stable input ID and its commit sequence plus group order is
the queue order. `InputWithdrawnRecord` can remove only pending input.
`OperationStartedRecord.input_ids` binds one nonempty ordered dispatch group to
the exact `original_input`; terminal `InputSettledRecord` facts preserve one
outcome for each member. `reduce_lane` exposes these facts as
`LaneReduction::input_reduction`, including deterministic pending input order
and every input's pending, withdrawn, dispatched, or settled state.

An `ExtensionControlEnqueuedRecord` captures the target operation, immutable
harness revision, extension ID, command, and bounded arguments while an
operation is active. It remains in `LaneReduction::pending_extension_controls`
in exact source order, including controls accepted in one commit, until exactly
one `ExtensionControlAppliedRecord` follows terminal settlement.
The state mutation that applies a control and its applied record belong in the
same semantic commit.

`ExtensionStateValueSetFact` replaces one complete bounded JSON state value
for a stable extension ID and pins its `state_version`. A
`TurnCheckpointFact` follows a settled operation and its required input/control
cleanup, captures the exact lane leaf and complete version-pinned extension
state map, and permits no unresolved owned tool, provider, child, or control
work. A `ForkedLaneFact` must immediately consume a fresh matching
`LaneMutation::Created` branch at that checkpoint leaf. The child begins from
the captured state, so later parent state writes cannot leak into it.
`extension_state_for_lane` and `LaneReduction::extension_state` are the
authoritative state projections. `preview_session_commit` validates a proposed
commit without writing it; its caller-supplied timestamp is planning data, not
a durable receipt, and the actual commit must still contend with the writer.

## Durable agent graph

The v1 schema is updated in place with first-class subagent facts; there is no
decoder for a pre-subagent intermediate shape and no v2 migration. Absence of a
`SubagentPolicy` fact means the optional capability is absent. A session that
contains one persists the ordered full model descriptors and display metadata,
known context windows, concurrency and total-spawn limits, timeout, and exact
root collaboration-tool surface digest before any child spawn.

`AgentSpawned` binds a deterministic `AgentId` to its root lane and operation,
unique task name, child lane, model/thinking/context mode, exact optional parent
source leaf, workspace lease, immutable harness identity, and durable spawn tool
intent. The accepted child uses `OperationKind::Subagent`; ordinary operation,
epoch, provider, tool, usage, and completion records remain authoritative for
its execution rather than being duplicated as agent-state facts.

`WorkspaceDelta` retains the child and lease, synthetic base and result Git
commits, strictly sorted repository-relative paths, and an immutable binary
patch `PayloadRef`. `AgentTaskFinished` follows the child's terminal operation
record and retains its final entry, inline-or-artifact report, and optional
delta. `WorkspaceDeltaApplied` is appended only after an exact parent
application is proven committed.

`reduce_agent_graph` is a pure projection over the append-only prefix. It
accepts documented crash prefixes such as a spawn without an operation or a
finished operation awaiting workspace finalization. It rejects duplicate
agent/lane and task-name bindings, unknown parents or harness identities,
models outside the persisted policy, mismatched child operations or leases,
terminal facts preceding completion, unknown or mismatched deltas, invalid
application references, and paths with NUL, absolute/parent components,
duplicates, or nondeterministic ordering. Running, finalizing, completed,
delta-ready, interrupted, and applied views are derived rather than persisted
as redundant transition records.

## Evidence and artifacts

Payloads that exceed an explicit policy may be stored in objects/ and
represented by a PayloadRef. Artifact identities are BLAKE3 hashes of their
exact bytes. The session verification path checks each reachable artifact's
identity and length.

SessionFact::TraceArtifact retains a redacted trace's artifact ID, byte length,
media type, operation, epoch, core run, and resolved harness identity. That
makes a trace recoverable evidence rather than best-effort telemetry.

Subagent report and patch artifacts are equally first-class direct session
roots. Verification checks their declared identity and length; garbage
collection and export cannot omit them while their graph facts remain.

Use `verify_session` for the durable prefix and `session_artifact_roots` for
the exact direct session-owned immutable roots. The terminal operator commands
also decode every retained harness catalog canonically, recompute its immutable
lineage, verify its source artifacts, and add those transitive roots before
verification, export, or collection. Verification recomputes every required
object digest and reports finalized unreachable objects separately; an orphan
is not corruption and remains subject to reviewed collection. See [artifact
recovery](artifact-recovery.md) for export and collection rules.

## Operator commands

The terminal binary exposes explicit machine-readable operations; each writes
one JSON object to stdout. `inspect` and `dump` take a session ID and resolve
it below the Tea home (or the path supplied by `--tea-home`), scanning the
workspace-hash roots and validating the immutable session header. A missing
or ambiguous ID is an error. The remaining operations continue to take
explicit session directories and never discover a session implicitly:

```sh
tea session inspect <session-id> [--tea-home <path>]
tea session dump <session-id> [--tea-home <path>]
tea session repair <session-dir>
tea session rebuild-meta <session-dir>
tea session verify <session-dir> [--root <artifact-id>]...
tea session gc <session-dir> [--root <artifact-id>]... [--apply]
tea session export <session-dir> <destination> [--root <artifact-id>]...
tea session restore <export-dir> <destination>
```

`dump` returns the validated JSONL prefix in a `records` array together with
the session identity, authenticated prefix digest/sequence, and any
`torn_tail_offset`; it does not present an uncommitted tail as durable state.

`verify` is read-only. It validates each immutable harness catalog and its
transitive source roots as well as the prefix and direct objects. Alongside
those results it reports whether the disposable `HEAD` and terminal-host
`meta.json` caches exactly match the validated snapshot and lists any finalized
orphan objects; neither diagnostic repairs, trusts, or changes authoritative
history.

`gc` is a dry run unless `--apply` is supplied. `--root` is required for
external transitive immutable roots that are not recoverable from a session's
retained harness catalogs; Tea does not guess experiment or retention roots.
`restore` reads the exported manifest, validates that it names the source
prefix and every required harness source root, then publishes a new
destination.

`rebuild-meta` is the explicit derived-cache maintenance operation. It first
replays and validates the named session, refreshes `HEAD`, then atomically
replaces the terminal host's picker `meta.json` from that same committed
prefix. Neither cache is used as recovery authority.
