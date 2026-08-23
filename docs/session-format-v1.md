# Session format v1

tea-session owns Tea's only durable session format. A session is a new
directory containing:

~~~
<session>.tea/
  session.jsonl
  HEAD
  objects/
~~~

session.jsonl is the authoritative append-only v1 log. Every entry and effect
record is globally sequenced, and HEAD is only an atomically replaced cache of
the last committed sequence. The object store holds immutable content-addressed
bytes referenced by semantic facts.

## Creation and recovery

JsonlSession::create refuses an existing directory. JsonlSession::open accepts
only a v1 header, verifies the complete committed prefix, and truncates only an
incomplete final write. It does not interpret another format, import a
transcript, or silently create a replacement session.

SessionHeader::new creates the required main lane. The current schema constant
is SESSION_FORMAT_VERSION = 1; every JSONL record carries that version through
its header and is rejected if it disagrees.

## Evidence and artifacts

Payloads that exceed an explicit policy may be stored in objects/ and
represented by a PayloadRef. Artifact identities are BLAKE3 hashes of their
exact bytes. The session verification path checks each reachable artifact's
identity and length.

SessionFact::TraceArtifact retains a redacted trace's artifact ID, byte length,
media type, operation, epoch, core run, and resolved harness identity. That
makes a trace recoverable evidence rather than best-effort telemetry.

Use verify_session for the durable prefix and session_artifact_roots for the
exact session-owned immutable roots. See [artifact recovery](artifact-recovery.md)
for export and collection rules.
