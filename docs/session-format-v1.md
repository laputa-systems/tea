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

`session.jsonl` is the authoritative append-only v1 log. Its header is sealed
with a BLAKE3 digest; every committed mutation then names its sequence,
timestamp, previous digest, semantic payload, and resulting digest. The reader
accepts canonical JSON only, validates the closed v1 schema by lossless
decode/re-encode, and checks consecutive sequence and digest-chain links.
Checked-in wire fixtures preserve exact bytes for the header, a user entry,
and a representative artifact fact, lane mutation, operation start, harness
revision transition, and terminal operation result. The rejection matrix covers
noncanonical ordering, duplicate fields, whitespace, numeric spelling, CRLF,
invalid text, schema violations, and invalid identities. Integrity corruption
tests cover header and envelope changes, deletion, duplication, reordering,
foreign insertion, sequence failure, and a correctly sealed record that the
pure reducer still rejects.

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

## Wire, integrity, and commit contract

The header is one object with exactly these fields: kind, version, session_id,
created_at_ms, workspace, metadata, initial_lane, and digest. kind is
session and version is the sole supported value, 1. Every subsequent line is
an envelope with exactly seq, timestamp_ms, prev_digest, mutation, and digest.
The closed mutation taxonomy has only entry, record, lane, and fact top-level
kinds; their versioned payload schemas are encoded by the narrow JSONL
boundary in tea-session, rather than by serializing Rust domain structs
directly.

Tea writes UTF-8, minified JSON with one LF per complete line. Object keys and
metadata maps are deterministically ordered, required nullability is explicit,
and integers use the schema's canonical unsigned representation. Reading first
parses, then re-encodes, and requires byte-for-byte equality before reduction.
Thus a hand edit, duplicate key, alternate field order, extra whitespace,
CRLF, unknown field, missing field, or alternate numeric spelling cannot become
durable history. A complete line is limited to 1 MiB; large evidence belongs in
the immutable artifact store.

The header digest is BLAKE3 over the canonical unsigned header through the
length-delimited tea-session-header-v1 domain. A record digest uses the
tea-session-record-v1 domain and length-delimited session ID, sequence,
timestamp, previous digest, and canonical mutation-payload bytes. The first
record names the header digest and later records name the preceding record.
These digests detect accidental or partial corruption; they are not signatures
and do not protect against an attacker able to coherently replace the entire
directory.

The writer validates the semantic transition, assigns the next consecutive
sequence and one clock reading, derives lane parentage, seals and writes one
line plus LF, flushes, and in strict mode synchronizes the file. Only then does
it update live reduction state or permit a dependent effect. A post-write
failure faults that writer: it must be closed and reopened, which decides
whether the complete line became part of the prefix. Development durability is
explicitly buffered and can lose recent acknowledged data; strict mode uses the
available file and directory synchronization calls but cannot promise hardware
behavior beyond the operating system's guarantees.

Failures before a non-empty append attempt remain ordinary I/O rejections; the
same writer retains its validated prefix and may retry a later transition.
Once the writer has attempted to append bytes, flush, or synchronize the log,
Tea reports an indeterminate-write error and faults that handle. Reopening is
the only way to determine whether a complete newline-terminated record joined
the authoritative prefix; it never retries that append in place.

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
invariant. Once a mutation envelope has decoded, they also retain its sequence
and top-level kind (`entry`, `record`, `lane`, or `fact`) without printing the
potentially sensitive payload. Syntax errors before those fields are available
report the same location with that context absent.

The storage test matrix injects interruptions before append, after every byte
of a representative JSON record, after the JSON body, after the newline,
during flush, and on both sides of strict synchronization. An interrupted
writer is faulted for the rest of its lifetime. Reopening accepts only the
prior prefix or the complete newline-terminated record; an unterminated
suffix remains an explicit repair decision.

`SessionHeader::new` creates the required main lane; `SessionHeader::new_at`
and the `SessionClock` constructors make creation and commit timestamps
reproducible in fixtures. The current schema constant is
`SESSION_FORMAT_VERSION = 1`; every record is bounded to 1 MiB and a header
that disagrees is rejected before record decoding or repair.

## Evidence and artifacts

Payloads that exceed an explicit policy may be stored in objects/ and
represented by a PayloadRef. Artifact identities are BLAKE3 hashes of their
exact bytes. The session verification path checks each reachable artifact's
identity and length.

SessionFact::TraceArtifact retains a redacted trace's artifact ID, byte length,
media type, operation, epoch, core run, and resolved harness identity. That
makes a trace recoverable evidence rather than best-effort telemetry.

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
one JSON object to stdout and never discovers a session implicitly:

```sh
tea session inspect <session-dir>
tea session repair <session-dir>
tea session rebuild-meta <session-dir>
tea session verify <session-dir> [--root <artifact-id>]...
tea session gc <session-dir> [--root <artifact-id>]... [--apply]
tea session export <session-dir> <destination> [--root <artifact-id>]...
tea session restore <export-dir> <destination>
```

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
