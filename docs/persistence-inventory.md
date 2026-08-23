# Persistence inventory

This is the authority map for repository-owned Tea files. A session is always
understood from its validated `session.jsonl` prefix plus the immutable objects
that prefix retains. Paths below are relative to the Tea home or to one session
directory unless noted otherwise.

| Path or namespace | Owner crate | Classification | Authoritative? | Mutability | Commit protocol | Recovery behavior | GC behavior | Sensitive data? |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `sessions/<workspace>/<session>.tea/session.jsonl` | `tea-session` | append-only session history | Yes | append-only | canonical line, flush, optional `fsync`; strict mode syncs every accepted mutation | replay canonical lines, schema, sequence, and digest chain; explicit torn-tail repair only | never collected independently | May contain user, model, and tool evidence |
| `objects/blake3/<prefix>/<digest>` | `tea-session` | immutable content-addressed bytes | Yes when referenced | immutable | stream/hash to private file, sync, no-replace hard-link publication | rehash before use; bad/missing object is a typed error | reviewed root-plan only; `JsonlSession::collect_unreferenced_artifacts` holds writer lock | May contain retained payload or trace evidence |
| `harness/trees`, `harness/snapshots`, `harness/revisions`, `harness/candidates` | `tea-session` / `tea-harness` | namespace reservation for immutable harness material | Only referenced immutable objects are authoritative | immutable or reconstructible placement | content identities are retained through session facts and objects | catalog facts and objects reconstruct the active harness | retained through referenced artifacts; empty directories are disposable | May contain prompt and tool configuration |
| `HEAD` | `tea-session` | derived/disposable | No | atomically replaceable active-harness cache | private temp, sync, rename after session creation or a committed main-lane revision selection | an exact cache is retained; missing, malformed, stale, or foreign content is rebuilt from a validated log, and failure becomes a warning | delete freely | Header, session, and revision identifiers only |
| `meta.json` | `tea-agent` | derived/disposable | No | atomically replaceable cache | private temp, sync, rename | session listing treats missing, stale, corrupt, or foreign cache as an unexpanded directory summary; create/reopen rebuilds it from the validated v1 prefix | delete freely | Workspace, model, lane, and prefix digest identifiers |
| `worktrees/` | host/harness | derived/disposable | No | reconstructible projection | host-defined | rebuild from immutable harness and session state | delete freely | May contain generated tool/plugin material |
| `traces/`, `evals/` placement | host/evaluation layer | optional telemetry or external analytical state | No for session reduction | append-only or external | trace/evaluation owner protocol | never used to reconstruct session semantics | own retention policy; referenced trace artifacts remain roots | Often contains redacted evidence |
| `export.json` in an export | `tea-session` | export manifest | Yes for the exported package description, not live session execution | immutable after package publication | create in prepared export, sync, no-replace directory publish | compare named prefix digest plus every artifact identity and length with reopened export | removed with the export directory | Prefix and artifact identities only |
| `.HEAD.*.tmp`, `.meta-*.tmp`, `.artifact.tmp`, `.create-*`, `.export-*` | respective owner | ephemeral | No | short-lived | create-new private file/directory, cleanup on failure | ignored by discovery/replay | remove after publish or failed operation | May transiently contain durable bytes |
| `last-model.json` at Tea home | `tea-agent` | atomically replaceable operator configuration | Yes for terminal preferences only | replaceable | exclusive private temporary file, sync, rename, then sync parent directory | reject malformed preference contract; never used as session history | superseded values are replaced | Provider/model preferences |
| external-editor temporary file | `tea-agent` | ephemeral | No | short-lived | create-new owner-private file; removed on drop | never replayed | removed on close | May contain edited prompt text |

The table intentionally excludes provider-managed state and evaluation systems
outside the normal `tea` binary. They are not a fallback authority for session
recovery.

`tea-session` also builds a process-local append index from a validated prefix.
It derives entry identities and lane leaves, records accepted provisioned input,
and tracks the closed high-volume operation/epoch/step/provider lifecycle
subset. That subset can be checked before a durable append without replaying
the whole prefix; all other mutation classes run the same pure reducer over
borrowed retained history plus the pending mutation, without cloning a second
complete `SessionSnapshot`. The index is never persisted, is rebuilt after
replay, and advances only after a mutation commits.

`SessionSnapshot` retains typed entry, record, lane, and fact views plus a
small ordered envelope/location index. Replay iterates borrowed payload views
from that index, so semantic payload bytes are retained once rather than once
again in a second all-mutation timeline. The log reader remains streaming; no
raw JSONL buffer is retained after decoding.
