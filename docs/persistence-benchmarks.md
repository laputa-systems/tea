# Persistence measurement fixtures

These are reproducible local measurements, not performance guarantees. They
separate buffered codec/reduction work from strict per-record synchronization,
and deliberately avoid placing machine-specific latency claims in the session
format contract.

The fixtures live in `crates/tea-session/src/tests.rs` and use a fixed session
clock, fixed IDs, and fixed payload text. They create their session directory,
measure it, and remove it after a successful run.

| Fixture | Shape | Command |
| --- | --- | --- |
| `generated_long_session_fixture_measures_buffered_append_and_replay` | 10,000 canonical user-entry mutations; development durability; then validated reopen | `cargo test -p tea-session --lib generated_long_session_fixture_measures_buffered_append_and_replay -- --ignored --nocapture` |
| `generated_artifact_tool_session_fixture_measures_buffered_replay_and_verification` | 10,000 user entries plus one complete operation/epoch/tool-intent/result lifecycle; a 256 KiB tool output is retained in CAS, then reopen and full artifact verification | `cargo test -p tea-session --lib generated_artifact_tool_session_fixture_measures_buffered_replay_and_verification -- --ignored --nocapture` |
| `generated_mixed_medium_session_fixture_measures_replay_and_verification` | 2,000 user entries; 250 complete tool lifecycles with 1,500 operation records; 20 harness revisions and compaction checkpoints; and 200 CAS-backed tool payloads (4,040 mutations total); then validated reopen and full artifact verification | `cargo test -p tea-session --lib generated_mixed_medium_session_fixture_measures_replay_and_verification -- --ignored --nocapture` |
| `generated_operation_session_fixture_measures_buffered_append_and_replay` | 3,000 complete operation lifecycles: accepted input, epoch, assistant step, provider request/settlement, usage, and terminal records (27,000 mutations total); development durability; then validated reopen | `cargo test -p tea-session --lib generated_operation_session_fixture_measures_buffered_append_and_replay -- --ignored --nocapture` |
| `generated_strict_append_fixture_measures_synchronization` | 32 canonical user-entry mutations with strict file synchronization | `cargo test -p tea-session --lib generated_strict_append_fixture_measures_synchronization -- --ignored --nocapture` |
| `generated_session_listing_fixture_measures_bounded_metadata_reads` | 1,000 session directories with a mix of valid, missing, stale, and malformed `meta.json` caches; deliberately no `session.jsonl` files | `cargo test -p tea-agent --lib generated_session_listing_fixture_measures_bounded_metadata_reads -- --ignored --nocapture` |

To observe process memory on macOS, prefix the long command with
`/usr/bin/time -l`. Use an equivalent platform tool elsewhere. The long
fixture’s development mode is intentional: strict latency is measured by the
separate fixture, so an `fsync`-dominated result is not presented as a codec
comparison.

## Recorded local baseline

On the current Apple Silicon development machine, using
`nightly-2026-07-24` in the debug test profile, the long fixture produced
4,109,117 bytes of JSONL. Buffered append took 588 ms and validated reopen
took 809 ms. `/usr/bin/time -l` reported a 46,481,408-byte maximum resident
set size for the process. The strict fixture took 207 ms for 32 mutations,
or 6,488 µs per mutation.

The 27,000-mutation operation fixture produced 13,396,090 bytes of JSONL.
Buffered append took 1,813 ms and validated reopen took 2,474 ms.
`/usr/bin/time -l` reported a 105,693,184-byte maximum resident set size. Its
records deliberately settle without a live provider, but exercise the
operation, epoch, retry-step, provider request, and accumulated usage
relationships that the user-entry fixture does not.

The artifact-tool fixture produced 10,008 mutations, 4,163,772 bytes of JSONL,
and 262,144 bytes of immutable object storage. Buffered append took 2,246 ms,
validated reopen took 835 ms, and full artifact verification took 45 ms. It
keeps the raw tool bytes out of the live log while measuring the complete
prefix/object relationship.

The mixed-medium fixture produced 4,040 mutations, 1,892,001 bytes of JSONL,
and 5,800 bytes of immutable object storage. Buffered append took 8,812 ms,
validated reopen took 371 ms, and full artifact verification took 27 ms. It
keeps the required medium workload in one profile: repeated tool operations,
harness transitions, compaction, and a many-object CAS shape.

The session-listing fixture completed in 32 ms for 1,000 directories (250
valid metadata caches and 750 missing, stale, or malformed caches). It has no
`session.jsonl` files, so its success demonstrates that the picker reads only
directory entries and bounded metadata, never replaying session logs.

Those figures establish a before/after comparison point only. They are not
portable targets: filesystem synchronization, build profile, CPU, and kernel
cache state materially affect them. The long fixture isolates the common
user-entry path, while the operation fixture covers one repeated lifecycle
shape. The mixed-medium fixture adds repeated compaction/revision and a
many-object shape. The picker fixture covers bounded cache discovery, while the
artifact fixture covers one CAS-backed tool-result lifecycle. These workload
definitions are reproducible evidence, not portable service-level objectives.

Tea currently writes no persisted replay snapshots. The baseline therefore has
no snapshot-assisted reopen measurement, and the implementation deliberately
uses bounded streaming replay rather than shipping an unmeasured second cache
format. Immutable harness snapshots are separate content-addressed domain
objects, not replay accelerators for `session.jsonl`.

The locked release `tea` binary measured 6,518,864 bytes before this
persistence work and 6,667,968 bytes afterward on the same machine (a
149,104-byte local increase). Its normal dependency tree still includes the
pre-existing `blake3` and `rustix` crates; this work added no runtime
dependency.
