use super::*;
use std::sync::Arc;

#[derive(Debug)]
struct FixedSessionClock(u64);

impl SessionClock for FixedSessionClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

#[test]
fn injected_session_clocks_make_header_and_commit_timestamps_reproducible() {
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_000_123));
    let header = SessionHeader::new_at(
        SessionId::new("fixed-clock").expect("valid session ID"),
        "workspace-test",
        Metadata::new(),
        1_700_000_000_000,
    );

    let mut memory = MemorySession::create_with_clock(header.clone(), Arc::clone(&clock))
        .expect("memory session creates");
    let memory_entry = memory
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("fixed-clock-memory-entry").expect("valid entry ID"),
                "deterministic",
            ),
        )
        .expect("memory entry commits");
    assert_eq!(
        memory
            .snapshot()
            .expect("snapshot succeeds")
            .header()
            .created_at_ms,
        1_700_000_000_000
    );
    assert_eq!(memory_entry.header.timestamp_ms, 1_700_000_000_123);

    let directory = temporary_session_directory("fixed-clock-jsonl");
    let mut jsonl =
        JsonlSession::create_with_clock(&directory, header, DurabilityMode::Strict, clock)
            .expect("JSONL session creates");
    let jsonl_entry = jsonl
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("fixed-clock-jsonl-entry").expect("valid entry ID"),
                "deterministic",
            ),
        )
        .expect("JSONL entry commits");
    assert_eq!(jsonl_entry.header.timestamp_ms, 1_700_000_000_123);
    drop(jsonl);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn append_rejects_a_user_entry_that_disagrees_with_its_accepted_operation_input() {
    let lane = LaneId::main();
    let input_id = EntryId::new("accepted-input").expect("valid entry ID");
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("accepted-input-contract").expect("valid session ID"),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creates");

    session
        .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
            OperationId::new("accepted-input-operation").expect("valid operation ID"),
            lane.clone(),
            None,
            OperationKind::Run,
            vec![ProvisionedEntry::user(input_id.clone(), "accepted content")],
            HarnessRevisionId::new("accepted-input-revision").expect("valid revision ID"),
            ModelHarnessProfileId::new("accepted-input-profile").expect("valid profile ID"),
        )))
        .expect("operation accepts its provisioned input");

    let error = session
        .append_entry(&lane, ProvisionedEntry::user(input_id, "different content"))
        .expect_err("materialized input must match the accepted operation input");
    assert!(matches!(error, SessionError::Corruption(_)));
    assert_eq!(
        session
            .snapshot()
            .expect("rejected append leaves snapshot readable")
            .last_sequence(),
        Sequence(1)
    );

    let directory = temporary_session_directory("accepted-input-jsonl-contract");
    let mut jsonl = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("accepted-input-jsonl-contract").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Development,
    )
    .expect("JSONL session creates");
    let persisted_input = EntryId::new("accepted-input-jsonl").expect("valid entry ID");
    jsonl
        .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
            OperationId::new("accepted-input-jsonl-operation").expect("valid operation ID"),
            lane.clone(),
            None,
            OperationKind::Run,
            vec![ProvisionedEntry::user(
                persisted_input.clone(),
                "accepted content",
            )],
            HarnessRevisionId::new("accepted-input-jsonl-revision").expect("valid revision ID"),
            ModelHarnessProfileId::new("accepted-input-jsonl-profile").expect("valid profile ID"),
        )))
        .expect("JSONL operation accepts its provisioned input");
    let before = std::fs::read(directory.join("session.jsonl")).expect("prefix reads");
    assert!(matches!(
        jsonl.append_entry(
            &lane,
            ProvisionedEntry::user(persisted_input, "different content"),
        ),
        Err(SessionError::Corruption(_))
    ));
    assert_eq!(
        std::fs::read(directory.join("session.jsonl")).expect("rejected prefix reads"),
        before,
        "the rejected locally validated mutation never reaches the JSONL authority"
    );
    drop(jsonl);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_writes_the_exact_v1_header_golden_fixture() {
    let directory = temporary_session_directory("v1-header-golden");
    let header = SessionHeader {
        kind: "session".into(),
        version: SESSION_FORMAT_VERSION,
        session_id: SessionId::new("fixture-session").expect("valid session ID"),
        created_at_ms: 1_700_000_000_000,
        workspace: "fixture-workspace".into(),
        metadata: Metadata::new(),
        initial_lane: LaneId::main(),
        digest: Digest::zero(),
    };
    let session =
        JsonlSession::create(&directory, header, DurabilityMode::Strict).expect("session creates");
    drop(session);

    assert_eq!(
        std::fs::read(directory.join("session.jsonl")).expect("header reads"),
        include_bytes!("../fixtures/wire/v1-header.golden.jsonl"),
        "the header is the exact canonical v1 wire fixture"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_writes_the_exact_v1_user_message_golden_fixture() {
    let directory = temporary_session_directory("v1-user-message-golden");
    let header = SessionHeader {
        kind: "session".into(),
        version: SESSION_FORMAT_VERSION,
        session_id: SessionId::new("fixture-session").expect("valid session ID"),
        created_at_ms: 1_700_000_000_000,
        workspace: "fixture-workspace".into(),
        metadata: Metadata::new(),
        initial_lane: LaneId::main(),
        digest: Digest::zero(),
    };
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_000_001));
    let mut session =
        JsonlSession::create_with_clock(&directory, header, DurabilityMode::Strict, clock)
            .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("fixture-entry").expect("valid entry ID"),
                "fixture message",
            ),
        )
        .expect("entry commits");
    drop(session);

    assert_eq!(
        std::fs::read(directory.join("session.jsonl")).expect("session file reads"),
        include_bytes!("../fixtures/wire/v1-user-message.golden.jsonl"),
        "the header and committed mutation are exact canonical v1 wire fixtures"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_writes_exact_v1_fixtures_for_every_mutation_family() {
    let directory = temporary_session_directory("v1-mutation-golden");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_000_001));
    let mut session = JsonlSession::create_with_clock(
        &directory,
        SessionHeader {
            kind: "session".into(),
            version: SESSION_FORMAT_VERSION,
            session_id: SessionId::new("fixture-session").expect("valid session ID"),
            created_at_ms: 1_700_000_000_000,
            workspace: "fixture-workspace".into(),
            metadata: Metadata::new(),
            initial_lane: LaneId::main(),
            digest: Digest::zero(),
        },
        DurabilityMode::Strict,
        clock,
    )
    .expect("session creates");
    let revision = HarnessRevisionId::new("fixture-revision").expect("valid revision ID");
    let profile = ModelHarnessProfileId::new("fixture-profile").expect("valid profile ID");
    let operation = OperationId::new("fixture-operation").expect("valid operation ID");
    session
        .append_fact(SessionFact::HarnessCatalog(HarnessCatalogFact {
            schema_version: 1,
            artifact_id: ArtifactId::from_bytes(b"fixture catalog"),
            byte_len: b"fixture catalog".len() as u64,
        }))
        .expect("artifact fact commits");
    session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: LaneId::new("fixture-lane").expect("valid lane ID"),
            base_leaf_id: None,
        })
        .expect("lane mutation commits");
    session
        .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
            operation.clone(),
            LaneId::main(),
            None,
            OperationKind::Run,
            Vec::new(),
            revision.clone(),
            profile,
        )))
        .expect("operation record commits");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("fixture-revision-entry").expect("valid entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision,
                    snapshot_id: HarnessSnapshotId::new("fixture-snapshot")
                        .expect("valid snapshot ID"),
                    rollback_from: None,
                }),
            },
        )
        .expect("harness revision entry commits");
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: operation,
            outcome: OperationOutcome::Completed,
        }))
        .expect("terminal operation record commits");
    drop(session);

    assert_eq!(
        std::fs::read(directory.join("session.jsonl")).expect("session log reads"),
        include_bytes!("../fixtures/wire/v1-representative-mutations.golden.jsonl"),
        "the fixture retains exact header, fact, lane, record, and entry bytes"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_creation_refuses_an_existing_session_directory_without_overwriting_it() {
    let directory = temporary_session_directory("create-no-overwrite");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("create-no-overwrite").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("initial session creates");
    drop(session);
    let before = std::fs::read(directory.join("session.jsonl")).expect("initial log reads");

    assert!(matches!(
        JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new("create-no-overwrite-second").expect("valid session ID"),
                "different-workspace",
                Metadata::new(),
            ),
            DurabilityMode::Strict,
        ),
        Err(SessionError::Io { .. })
    ));
    assert_eq!(
        std::fs::read(directory.join("session.jsonl")).expect("original log rereads"),
        before,
        "an existing published session is never a creation target"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn injected_creation_failures_publish_nothing_or_one_reopenable_v1_directory() {
    use crate::jsonl::{TestCreationFailpoint, install_test_creation_failpoint};

    let cases = [
        (TestCreationFailpoint::BeforeTemporaryDirectory, false),
        (TestCreationFailpoint::AfterTemporaryDirectory, false),
        (TestCreationFailpoint::AfterLayout, false),
        (TestCreationFailpoint::AfterHeaderWrite, false),
        (TestCreationFailpoint::AfterHeadCache, false),
        (TestCreationFailpoint::BeforeTemporaryDirectorySync, false),
        (TestCreationFailpoint::AfterTemporaryDirectorySync, false),
        (TestCreationFailpoint::BeforePublication, false),
        (TestCreationFailpoint::AfterPublication, true),
        (TestCreationFailpoint::BeforeParentDirectorySync, true),
        (TestCreationFailpoint::AfterParentDirectorySync, true),
    ];

    for (index, (failpoint, publication_may_have_happened)) in cases.into_iter().enumerate() {
        let directory = temporary_session_directory(&format!("create-failpoint-{index}"));
        let session_id = SessionId::new(format!("create-failpoint-{index}"))
            .expect("generated session ID is valid");
        let temporary_prefix = format!(
            ".{}.create-",
            directory
                .file_name()
                .expect("temporary test directory has a name")
                .to_string_lossy()
        );
        let parent = directory
            .parent()
            .expect("temporary test directory has a parent");

        let failpoint_guard = install_test_creation_failpoint(failpoint);
        let creation = JsonlSession::create(
            &directory,
            SessionHeader::new(session_id.clone(), "workspace-test", Metadata::new()),
            DurabilityMode::Strict,
        );
        drop(failpoint_guard);

        assert!(
            matches!(creation, Err(SessionError::Io { .. })),
            "{failpoint:?} reports an interrupted creation"
        );
        assert!(
            std::fs::read_dir(parent)
                .expect("temporary session parent reads")
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&temporary_prefix)),
            "{failpoint:?} leaves no private creation directory behind"
        );

        if publication_may_have_happened {
            let inspection = JsonlSession::inspect(&directory)
                .expect("a published creation prefix remains inspectable");
            assert_eq!(inspection.snapshot.header().session_id, session_id);
            assert_eq!(inspection.snapshot.last_sequence(), Sequence(0));
            assert_eq!(inspection.torn_tail_offset, None);
            let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
                .expect("a published creation prefix remains reopenable");
            drop(reopened);
        } else {
            assert!(
                !directory.exists(),
                "{failpoint:?} must not publish an incomplete directory"
            );
        }
        let _ = std::fs::remove_dir_all(&directory);
    }
}

#[test]
fn jsonl_commit_appends_only_one_new_canonical_line_without_replacing_the_log() {
    let directory = temporary_session_directory("append-only-file");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("append-only-file").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("append-only-first").expect("valid entry ID"),
                "first durable line",
            ),
        )
        .expect("first entry commits");
    let path = directory.join("session.jsonl");
    let before = std::fs::read(&path).expect("initial prefix reads");
    #[cfg(unix)]
    let before_inode = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(&path)
            .expect("initial metadata reads")
            .ino()
    };

    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("append-only-second").expect("valid entry ID"),
                "second durable line",
            ),
        )
        .expect("second entry commits");
    let after = std::fs::read(&path).expect("extended prefix reads");
    assert!(
        after.starts_with(&before),
        "a commit preserves every byte of the committed prefix"
    );
    let appended = &after[before.len()..];
    assert_eq!(
        appended.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "one commit appends exactly one newline-terminated mutation line"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            std::fs::metadata(&path)
                .expect("extended metadata reads")
                .ino(),
            before_inode,
            "committing a mutation never replaces session.jsonl"
        );
    }
    drop(session);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_an_unsupported_header_before_reading_or_repairing_records() {
    let directory = temporary_session_directory("unsupported-version");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("unsupported-version").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session layout creates");
    drop(session);
    let path = directory.join("session.jsonl");
    let fixture = include_bytes!("../fixtures/wire/unsupported-version.jsonl");
    std::fs::write(&path, fixture).expect("unsupported-format fixture writes");

    let error = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect_err("only format 1 is supported");

    assert_eq!(
        error.to_string(),
        format!(
            "unsupported session format at {}: observed version 2; current build supports only session format 1; no automatic migration is available",
            path.display()
        )
    );
    assert_eq!(
        std::fs::read(&path).expect("fixture remains readable"),
        fixture,
        "unsupported formats are never interpreted or repaired"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn memory_append_assigns_commit_sequence_and_lane_parent() {
    let session_id = SessionId::new("session-test").expect("valid session ID");
    let mut session = MemorySession::create(SessionHeader::new(
        session_id,
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creation succeeds");
    let lane = LaneId::main();

    let first = session
        .append_entry(
            &lane,
            ProvisionedEntry::user(EntryId::new("entry-1").expect("valid entry ID"), "one"),
        )
        .expect("first append succeeds");
    let second = session
        .append_entry(
            &lane,
            ProvisionedEntry::user(EntryId::new("entry-2").expect("valid entry ID"), "two"),
        )
        .expect("second append succeeds");

    assert_eq!(first.header.seq, Sequence(1));
    assert_eq!(first.header.parent_id, None);
    assert_eq!(second.header.seq, Sequence(2));
    assert_eq!(second.header.parent_id, Some(first.header.id));
    assert_eq!(
        session
            .snapshot()
            .expect("snapshot succeeds")
            .next_sequence(),
        Sequence(3)
    );
}

#[test]
fn ordered_mutation_view_borrows_the_single_typed_entry_payload() {
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("ordered-mutation-view").expect("valid session ID"),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("memory session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("ordered-mutation-entry").expect("valid entry ID"),
                "one owned semantic payload",
            ),
        )
        .expect("entry commits");
    let snapshot = session.snapshot().expect("snapshot succeeds");
    let typed_entry = snapshot.entries().first().expect("typed entry exists");
    let ordered = snapshot
        .mutations()
        .next()
        .expect("ordered envelope exists");
    assert_eq!(ordered.seq, typed_entry.header.seq);
    let SessionMutationRef::Entry(ordered_entry) = ordered.mutation else {
        panic!("first mutation must be the committed entry");
    };
    assert!(
        std::ptr::eq(typed_entry, ordered_entry),
        "ordered replay borrows the typed entry instead of retaining a second owned payload"
    );
}

#[test]
fn jsonl_nonincremental_append_validates_without_cloning_the_retained_snapshot() {
    let directory = temporary_session_directory("borrowed-append-validation");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("borrowed-append-validation").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Development,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("borrowed-append-user").expect("valid entry ID"),
                "establish a retained prefix",
            ),
        )
        .expect("user entry commits");
    assert_eq!(
        crate::model::take_session_snapshot_clone_count(),
        0,
        "setup performs no public snapshot reads"
    );

    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                EntryId::new("borrowed-append-assistant").expect("valid entry ID"),
                "x".repeat(512 * 1024),
                Vec::new(),
            ),
        )
        .expect("non-incremental assistant entry commits");

    assert_eq!(
        crate::model::take_session_snapshot_clone_count(),
        0,
        "the pre-write pure-reducer validation borrows history instead of cloning a complete snapshot"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn memory_and_jsonl_backends_conform_on_lane_parent_and_sequence_allocation() {
    let header = || {
        SessionHeader::new(
            SessionId::new("session-conformance").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        )
    };
    let mut memory = MemorySession::create(header()).expect("memory backend creates");
    let memory_entries = append_parent_chain(&mut memory).expect("memory appends");

    let directory = temporary_session_directory("conformance");
    let mut jsonl = JsonlSession::create(&directory, header(), DurabilityMode::Strict)
        .expect("JSONL backend creates");
    let jsonl_entries = append_parent_chain(&mut jsonl).expect("JSONL appends");

    assert_eq!(
        memory_entries
            .iter()
            .map(|entry| (&entry.header.id, &entry.header.parent_id, entry.header.seq))
            .collect::<Vec<_>>(),
        jsonl_entries
            .iter()
            .map(|entry| (&entry.header.id, &entry.header.parent_id, entry.header.seq))
            .collect::<Vec<_>>()
    );
    drop(jsonl);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rebuilds_its_append_index_from_a_validated_branch_history() {
    let directory = temporary_session_directory("append-index-reopen");
    let branch = LaneId::new("branch-a").expect("valid branch lane ID");
    let first = {
        let mut session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new("append-index-reopen").expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Strict,
        )
        .expect("session creates");
        let first = session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(
                    EntryId::new("append-index-root").expect("valid entry ID"),
                    "root",
                ),
            )
            .expect("root entry commits");
        session
            .append_lane_mutation(LaneMutation::Created {
                lane_id: branch.clone(),
                base_leaf_id: Some(first.header.id.clone()),
            })
            .expect("branch creation commits");
        let branch_entry = session
            .append_entry(
                &branch,
                ProvisionedEntry::user(
                    EntryId::new("append-index-branch-1").expect("valid entry ID"),
                    "branch entry",
                ),
            )
            .expect("branch entry commits");
        assert_eq!(branch_entry.header.parent_id, Some(first.header.id.clone()));
        first
    };

    let mut reopened =
        JsonlSession::open(&directory, DurabilityMode::Strict).expect("validated session reopens");
    let next = reopened
        .append_entry(
            &branch,
            ProvisionedEntry::user(
                EntryId::new("append-index-branch-2").expect("valid entry ID"),
                "next branch entry",
            ),
        )
        .expect("entry after reopen commits");
    assert_eq!(
        next.header.parent_id.as_ref().map(EntryId::as_str),
        Some("append-index-branch-1")
    );
    assert_ne!(next.header.parent_id, Some(first.header.id));
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

/// Reproducible long-log fixture for explicit persistence measurements.
///
/// It is ignored because strict per-record durability intentionally makes the
/// generation phase expensive. Use `DurabilityMode::Development` here only to
/// measure codec/reduction work separately from `fsync` latency.
#[test]
#[ignore = "run explicitly to measure the generated 10,000-mutation persistence fixture"]
fn generated_long_session_fixture_measures_buffered_append_and_replay() {
    const MUTATION_COUNT: u64 = 10_000;
    const CREATED_AT_MS: u64 = 1_700_000_000_000;
    let directory = temporary_session_directory("generated-long");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(CREATED_AT_MS + 1));
    let append_started = std::time::Instant::now();
    {
        let mut session = JsonlSession::create_with_clock(
            &directory,
            SessionHeader::new_at(
                SessionId::new("generated-long").expect("valid session ID"),
                "generated-long-workspace",
                Metadata::new(),
                CREATED_AT_MS,
            ),
            DurabilityMode::Development,
            Arc::clone(&clock),
        )
        .expect("long fixture session creates");
        for index in 0..MUTATION_COUNT {
            session
                .append_entry(
                    &LaneId::main(),
                    ProvisionedEntry::user(
                        EntryId::new(format!("generated-entry-{index:05}"))
                            .expect("generated entry ID is valid"),
                        format!("generated durable message {index:05}"),
                    ),
                )
                .expect("generated entry commits");
        }
    }
    let append_elapsed = append_started.elapsed();
    let jsonl_bytes = std::fs::metadata(directory.join("session.jsonl"))
        .expect("fixture JSONL exists")
        .len();
    let replay_started = std::time::Instant::now();
    let reopened = JsonlSession::open(&directory, DurabilityMode::Development)
        .expect("generated fixture reopens");
    assert_eq!(
        reopened
            .snapshot()
            .expect("snapshot succeeds")
            .last_sequence(),
        Sequence(MUTATION_COUNT)
    );
    let replay_elapsed = replay_started.elapsed();
    eprintln!(
        "generated-long mutations={MUTATION_COUNT} jsonl_bytes={jsonl_bytes} append_ms={} replay_ms={}",
        append_elapsed.as_millis(),
        replay_elapsed.as_millis()
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

/// Reproducible long log with a CAS-backed tool result. This keeps raw tool
/// bytes out of JSONL while making the complete prefix and object root part of
/// one measured verify/reopen workload.
#[test]
#[ignore = "run explicitly to measure a long session with CAS-backed tool output"]
fn generated_artifact_tool_session_fixture_measures_buffered_replay_and_verification() {
    const USER_ENTRY_COUNT: u64 = 10_000;
    const TOOL_OUTPUT_BYTES: usize = 256 * 1024;
    const CREATED_AT_MS: u64 = 1_700_000_300_000;
    let directory = temporary_session_directory("generated-artifact-tool-session");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(CREATED_AT_MS + 1));
    let lane = LaneId::main();
    let append_started = std::time::Instant::now();
    let (expected_sequence, object_bytes) = {
        let mut session = JsonlSession::create_with_clock(
            &directory,
            SessionHeader::new_at(
                SessionId::new("generated-artifact-tool-session").expect("valid session ID"),
                "generated-artifact-tool-workspace",
                Metadata::new(),
                CREATED_AT_MS,
            ),
            DurabilityMode::Development,
            Arc::clone(&clock),
        )
        .expect("artifact workload session creates");
        let mut source_leaf = None;
        for index in 0..USER_ENTRY_COUNT {
            let entry_id = EntryId::new(format!("generated-artifact-user-{index:05}"))
                .expect("generated user entry ID is valid");
            session
                .append_entry(
                    &lane,
                    ProvisionedEntry::user(entry_id.clone(), format!("generated user {index:05}")),
                )
                .expect("generated user entry commits");
            source_leaf = Some(entry_id);
        }

        let operation = OperationId::new("generated-artifact-operation")
            .expect("generated operation ID is valid");
        let epoch = EpochId::new("generated-artifact-epoch").expect("generated epoch ID is valid");
        let assistant =
            EntryId::new("generated-artifact-assistant").expect("generated assistant ID is valid");
        let result =
            EntryId::new("generated-artifact-tool-result").expect("generated result ID is valid");
        let revision = HarnessRevisionId::new("generated-artifact-revision")
            .expect("generated revision ID is valid");
        let profile = ModelHarnessProfileId::new("generated-artifact-profile")
            .expect("generated profile ID is valid");
        let snapshot = HarnessSnapshotId::new("generated-artifact-snapshot")
            .expect("generated snapshot ID is valid");
        session
            .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
                operation.clone(),
                lane.clone(),
                source_leaf.clone(),
                OperationKind::Run,
                Vec::new(),
                revision.clone(),
                profile.clone(),
            )))
            .expect("generated operation starts");
        session
            .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
                id: epoch.clone(),
                operation_id: operation.clone(),
                epoch_index: 0,
                source_leaf_id: source_leaf,
                harness_revision_id: revision.clone(),
                harness_snapshot_id: snapshot,
                model_harness_profile: profile.clone(),
                core_run_id: CoreRunId::new("generated-artifact-core-run")
                    .expect("generated core-run ID is valid"),
                epoch_resume_data: std::collections::BTreeMap::new(),
            }))
            .expect("generated epoch starts");
        session
            .append_entry(
                &lane,
                ProvisionedEntry::assistant(
                    assistant.clone(),
                    "",
                    vec![AssistantToolCall::new(
                        "generated-artifact-call",
                        "fixture",
                        JsonValue::Null,
                    )],
                ),
            )
            .expect("generated assistant entry commits");
        session
            .append_record(LaneRecord::tool_started(ToolStartedRecord::new(
                RecordId::new("generated-artifact-tool-start").expect("generated record ID"),
                operation.clone(),
                epoch.clone(),
                assistant,
                0,
                "generated-artifact-call",
                "fixture",
                JsonValue::Null,
                result.clone(),
                ToolReplayPolicy::Safe,
                Digest::from_bytes(b"generated-artifact-tool-definition"),
                revision,
                "generated-artifact-idempotency",
            )))
            .expect("generated tool intent commits");

        let artifact_bytes = (0..TOOL_OUTPUT_BYTES)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let artifact = session
            .artifact_store()
            .expect("artifact store opens")
            .put(&artifact_bytes, "application/octet-stream")
            .expect("large tool output publishes before its durable reference");
        session
            .append_entry(
                &lane,
                ProvisionedEntry {
                    id: result,
                    body: SessionEntry::ToolResult(ToolResultEntry {
                        tool_call_id: "generated-artifact-call".into(),
                        tool_name: "fixture".into(),
                        full_result: PayloadRef::Artifact {
                            artifact_id: artifact.artifact_id,
                            byte_len: artifact.byte_len,
                            media_type: artifact.media_type.clone(),
                        },
                        model_projection: JsonValue::String("bounded fixture projection".into()),
                        is_error: false,
                        terminate: false,
                        usage: Usage::default(),
                        projection_strategy_id: "generated-artifact-projection".into(),
                        artifact_policy_id: ArtifactPolicyId::new("generated-artifact-policy")
                            .expect("generated policy ID is valid"),
                    }),
                },
            )
            .expect("generated tool result commits");
        session
            .append_record(LaneRecord::Usage(UsageRecord {
                operation_id: operation.clone(),
                request_id: None,
                usage: Usage {
                    total_tokens: Some(USER_ENTRY_COUNT + 512),
                    input_tokens: Some(USER_ENTRY_COUNT),
                    output_tokens: Some(512),
                    reasoning_tokens: Some(128),
                    cache_read_tokens: Some(256),
                    cache_write_tokens: Some(64),
                    cost: Some("0.002".into()),
                },
            }))
            .expect("generated usage commits");
        session
            .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
                epoch_id: epoch,
                operation_id: operation.clone(),
                reason: EpochFinishReason::Settled,
            }))
            .expect("generated epoch finishes");
        session
            .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                operation_id: operation,
                outcome: OperationOutcome::Completed,
            }))
            .expect("generated operation finishes");
        let snapshot = session.snapshot().expect("generated snapshot succeeds");
        (snapshot.last_sequence(), artifact_bytes.len() as u64)
    };
    let append_elapsed = append_started.elapsed();
    let jsonl_bytes = std::fs::metadata(directory.join("session.jsonl"))
        .expect("generated JSONL exists")
        .len();
    let replay_started = std::time::Instant::now();
    let reopened = JsonlSession::open(&directory, DurabilityMode::Development)
        .expect("generated artifact workload reopens");
    let replay_elapsed = replay_started.elapsed();
    let verify_started = std::time::Instant::now();
    let verification = verify_session(
        &reopened.snapshot().expect("reopened snapshot succeeds"),
        &reopened
            .artifact_store()
            .expect("reopened artifact store opens"),
        std::iter::empty(),
    )
    .expect("CAS-backed tool result verifies");
    let verify_elapsed = verify_started.elapsed();
    assert_eq!(verification.artifact_count, 1);
    assert_eq!(verification.artifact_bytes, object_bytes);
    assert_eq!(
        reopened
            .snapshot()
            .expect("reopened snapshot succeeds")
            .last_sequence(),
        expected_sequence
    );
    eprintln!(
        "generated-artifact-tool mutations={} jsonl_bytes={jsonl_bytes} object_bytes={object_bytes} append_ms={} replay_ms={} verify_ms={}",
        expected_sequence.0,
        append_elapsed.as_millis(),
        replay_elapsed.as_millis(),
        verify_elapsed.as_millis(),
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

/// Self-contained medium profile for the persistence boundary. It combines
/// thousands of semantic entries and operation records with repeated tool
/// lifecycles, compaction, harness revisions, and many CAS objects.
#[test]
#[ignore = "run explicitly to measure the generated mixed medium persistence fixture"]
fn generated_mixed_medium_session_fixture_measures_replay_and_verification() {
    const USER_ENTRY_COUNT: u64 = 2_000;
    const ARTIFACT_COUNT: u64 = 200;
    const TOOL_OPERATION_COUNT: u64 = 250;
    const REVISION_INTERVAL: u64 = 100;
    let directory = temporary_session_directory("generated-mixed-medium");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_400_001));
    let lane = LaneId::main();
    let append_started = std::time::Instant::now();
    let (expected_sequence, expected_object_bytes) = {
        let mut session = JsonlSession::create_with_clock(
            &directory,
            SessionHeader::new_at(
                SessionId::new("generated-mixed-medium").expect("fixture ID is valid"),
                "generated-mixed-medium-workspace",
                Metadata::new(),
                1_700_000_400_000,
            ),
            DurabilityMode::Development,
            Arc::clone(&clock),
        )
        .expect("mixed fixture session creates");
        let store = session.artifact_store().expect("object store opens");
        let artifacts = (0..ARTIFACT_COUNT)
            .map(|index| {
                store
                    .put(
                        format!("mixed immutable artifact {index:04}").as_bytes(),
                        "text/plain",
                    )
                    .expect("fixture object publishes")
            })
            .collect::<Vec<_>>();
        let mut first = None;
        let mut source_leaf = None;
        for index in 0..USER_ENTRY_COUNT {
            let id = EntryId::new(format!("mixed-user-{index:05}")).expect("entry ID is valid");
            session
                .append_entry(
                    &lane,
                    ProvisionedEntry::user(id.clone(), format!("mixed user {index:05}")),
                )
                .expect("user entry commits");
            if first.is_none() {
                first = Some(id.clone());
            }
            source_leaf = Some(id.clone());
            if index % REVISION_INTERVAL == REVISION_INTERVAL - 1 {
                let revision = HarnessRevisionId::new(format!(
                    "mixed-revision-{:03}",
                    index / REVISION_INTERVAL
                ))
                .expect("revision ID is valid");
                let revision_entry_id = EntryId::new(format!("mixed-revision-entry-{index:05}"))
                    .expect("entry ID is valid");
                session
                    .append_entry(
                        &lane,
                        ProvisionedEntry {
                            id: revision_entry_id,
                            body: SessionEntry::HarnessRevisionChanged(
                                HarnessRevisionChangedEntry {
                                    revision_id: revision.clone(),
                                    snapshot_id: HarnessSnapshotId::new(format!(
                                        "mixed-snapshot-{:03}",
                                        index / REVISION_INTERVAL
                                    ))
                                    .expect("snapshot ID is valid"),
                                    rollback_from: None,
                                },
                            ),
                        },
                    )
                    .expect("revision transition commits");
                let compaction_id = EntryId::new(format!("mixed-compaction-{index:05}"))
                    .expect("entry ID is valid");
                session
                    .append_entry(
                        &lane,
                        ProvisionedEntry {
                            id: compaction_id.clone(),
                            body: SessionEntry::Compaction(CompactionEntry {
                                covered_from: first.clone(),
                                covered_to: Some(id.clone()),
                                retained_tail_boundary: Some(id.clone()),
                                summary: format!("mixed checkpoint {index:05}"),
                                strategy_id: "generated-mixed-compaction".into(),
                                recovery_index_artifact: None,
                                harness_revision_id: Some(revision),
                            }),
                        },
                    )
                    .expect("compaction checkpoint commits");
                source_leaf = Some(compaction_id);
            }
        }
        let revision = HarnessRevisionId::new(format!(
            "mixed-revision-{:03}",
            USER_ENTRY_COUNT / REVISION_INTERVAL - 1
        ))
        .expect("revision ID is valid");
        let harness_snapshot = HarnessSnapshotId::new(format!(
            "mixed-snapshot-{:03}",
            USER_ENTRY_COUNT / REVISION_INTERVAL - 1
        ))
        .expect("snapshot ID is valid");
        let profile =
            ModelHarnessProfileId::new("mixed-workload-profile").expect("profile ID is valid");
        for index in 0..TOOL_OPERATION_COUNT {
            let operation = OperationId::new(format!("mixed-operation-{index:04}"))
                .expect("operation ID is valid");
            let epoch = EpochId::new(format!("mixed-epoch-{index:04}")).expect("epoch ID is valid");
            let assistant =
                EntryId::new(format!("mixed-assistant-{index:04}")).expect("entry ID is valid");
            let result =
                EntryId::new(format!("mixed-tool-result-{index:04}")).expect("entry ID is valid");
            let call_id = format!("mixed-tool-call-{index:04}");
            session
                .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
                    operation.clone(),
                    lane.clone(),
                    source_leaf.clone(),
                    OperationKind::Run,
                    Vec::new(),
                    revision.clone(),
                    profile.clone(),
                )))
                .expect("tool operation starts");
            session
                .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
                    id: epoch.clone(),
                    operation_id: operation.clone(),
                    epoch_index: 0,
                    source_leaf_id: source_leaf.clone(),
                    harness_revision_id: revision.clone(),
                    harness_snapshot_id: harness_snapshot.clone(),
                    model_harness_profile: profile.clone(),
                    core_run_id: CoreRunId::new(format!("mixed-core-run-{index:04}"))
                        .expect("core run ID is valid"),
                    epoch_resume_data: std::collections::BTreeMap::new(),
                }))
                .expect("tool epoch starts");
            session
                .append_entry(
                    &lane,
                    ProvisionedEntry::assistant(
                        assistant.clone(),
                        "",
                        vec![AssistantToolCall::new(
                            call_id.clone(),
                            "fixture",
                            JsonValue::Null,
                        )],
                    ),
                )
                .expect("tool assistant entry commits");
            session
                .append_record(LaneRecord::tool_started(ToolStartedRecord::new(
                    RecordId::new(format!("mixed-tool-start-{index:04}"))
                        .expect("record ID is valid"),
                    operation.clone(),
                    epoch.clone(),
                    assistant,
                    0,
                    call_id.clone(),
                    "fixture",
                    JsonValue::Null,
                    result.clone(),
                    ToolReplayPolicy::Safe,
                    Digest::from_bytes(format!("mixed-tool-definition-{index:04}").as_bytes()),
                    revision.clone(),
                    format!("mixed-tool-idempotency-{index:04}"),
                )))
                .expect("tool intent commits");
            let artifact = &artifacts[(index % ARTIFACT_COUNT) as usize];
            session
                .append_entry(
                    &lane,
                    ProvisionedEntry {
                        id: result.clone(),
                        body: SessionEntry::ToolResult(ToolResultEntry {
                            tool_call_id: call_id,
                            tool_name: "fixture".into(),
                            full_result: PayloadRef::Artifact {
                                artifact_id: artifact.artifact_id,
                                byte_len: artifact.byte_len,
                                media_type: artifact.media_type.clone(),
                            },
                            model_projection: JsonValue::String("bounded mixed projection".into()),
                            is_error: false,
                            terminate: false,
                            usage: Usage::default(),
                            projection_strategy_id: "generated-mixed-projection".into(),
                            artifact_policy_id: ArtifactPolicyId::new("generated-mixed-policy")
                                .expect("policy ID is valid"),
                        }),
                    },
                )
                .expect("tool result commits");
            source_leaf = Some(result);
            session
                .append_record(LaneRecord::Usage(UsageRecord {
                    operation_id: operation.clone(),
                    request_id: None,
                    usage: Usage {
                        total_tokens: Some(2_200),
                        input_tokens: Some(2_000),
                        output_tokens: Some(200),
                        reasoning_tokens: Some(100),
                        cache_read_tokens: Some(500),
                        cache_write_tokens: Some(50),
                        cost: Some("0.001".into()),
                    },
                }))
                .expect("tool usage commits");
            session
                .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
                    epoch_id: epoch,
                    operation_id: operation.clone(),
                    reason: EpochFinishReason::Settled,
                }))
                .expect("tool epoch finishes");
            session
                .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                    operation_id: operation,
                    outcome: OperationOutcome::Completed,
                }))
                .expect("tool operation finishes");
        }
        (
            session
                .snapshot()
                .expect("snapshot succeeds")
                .last_sequence(),
            artifacts
                .iter()
                .map(|artifact| artifact.byte_len)
                .sum::<u64>(),
        )
    };
    let append_elapsed = append_started.elapsed();
    let jsonl_bytes = std::fs::metadata(directory.join("session.jsonl"))
        .expect("JSONL exists")
        .len();
    let replay_started = std::time::Instant::now();
    let reopened =
        JsonlSession::open(&directory, DurabilityMode::Development).expect("fixture reopens");
    let replay_elapsed = replay_started.elapsed();
    let verify_started = std::time::Instant::now();
    let verification = verify_session(
        &reopened.snapshot().expect("snapshot succeeds"),
        &reopened.artifact_store().expect("artifact store opens"),
        std::iter::empty(),
    )
    .expect("mixed immutable roots verify");
    assert_eq!(verification.artifact_count, ARTIFACT_COUNT as usize);
    assert_eq!(verification.artifact_bytes, expected_object_bytes);
    assert_eq!(
        reopened
            .snapshot()
            .expect("snapshot succeeds")
            .last_sequence(),
        expected_sequence
    );
    eprintln!(
        "generated-mixed-medium mutations={} tool_operations={TOOL_OPERATION_COUNT} jsonl_bytes={jsonl_bytes} object_bytes={} append_ms={} replay_ms={} verify_ms={}",
        expected_sequence.0,
        verification.artifact_bytes,
        append_elapsed.as_millis(),
        replay_elapsed.as_millis(),
        verify_started.elapsed().as_millis(),
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

/// Reproducible operation-lifecycle fixture for the record-validation path.
///
/// Each generated operation records an accepted input, an epoch, one assistant
/// step, and a settled provider request before it reaches its terminal state.
/// This keeps the fixture independent of a live provider while covering the
/// durable operation facts that ordinary user-entry benchmarks do not touch.
#[test]
#[ignore = "run explicitly to measure the generated 3,000-operation persistence fixture"]
fn generated_operation_session_fixture_measures_buffered_append_and_replay() {
    const OPERATION_COUNT: u64 = 3_000;
    const MUTATIONS_PER_OPERATION: u64 = 9;
    const CREATED_AT_MS: u64 = 1_700_000_200_000;
    let directory = temporary_session_directory("generated-operations");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(CREATED_AT_MS + 1));
    let lane = LaneId::main();
    let revision = HarnessRevisionId::new("generated-operation-revision")
        .expect("generated revision ID is valid");
    let snapshot = HarnessSnapshotId::new("generated-operation-snapshot")
        .expect("generated snapshot ID is valid");
    let profile = ModelHarnessProfileId::new("generated-operation-profile")
        .expect("generated profile ID is valid");
    let append_started = std::time::Instant::now();
    {
        let mut session = JsonlSession::create_with_clock(
            &directory,
            SessionHeader::new_at(
                SessionId::new("generated-operations").expect("valid session ID"),
                "generated-operations-workspace",
                Metadata::new(),
                CREATED_AT_MS,
            ),
            DurabilityMode::Development,
            Arc::clone(&clock),
        )
        .expect("operation fixture session creates");
        let mut source_leaf_id = None;
        for index in 0..OPERATION_COUNT {
            let operation_id = OperationId::new(format!("generated-operation-{index:05}"))
                .expect("generated operation ID is valid");
            let epoch_id = EpochId::new(format!("generated-epoch-{index:05}"))
                .expect("generated epoch ID is valid");
            let input_id = EntryId::new(format!("generated-operation-input-{index:05}"))
                .expect("generated input ID is valid");
            let step_id = StepId::new(format!("generated-step-{index:05}"))
                .expect("generated step ID is valid");
            let request_id = ProviderRequestId::new(format!("generated-request-{index:05}"))
                .expect("generated request ID is valid");

            session
                .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
                    operation_id.clone(),
                    lane.clone(),
                    source_leaf_id.clone(),
                    OperationKind::Run,
                    vec![ProvisionedEntry::user(
                        input_id.clone(),
                        format!("generated operation input {index:05}"),
                    )],
                    revision.clone(),
                    profile.clone(),
                )))
                .expect("generated operation starts");
            session
                .append_entry(
                    &lane,
                    ProvisionedEntry::user(
                        input_id.clone(),
                        format!("generated operation input {index:05}"),
                    ),
                )
                .expect("generated operation input materializes");
            source_leaf_id = Some(input_id);
            session
                .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
                    id: epoch_id.clone(),
                    operation_id: operation_id.clone(),
                    epoch_index: 0,
                    source_leaf_id: source_leaf_id.clone(),
                    harness_revision_id: revision.clone(),
                    harness_snapshot_id: snapshot.clone(),
                    model_harness_profile: profile.clone(),
                    core_run_id: CoreRunId::new(format!("generated-core-run-{index:05}"))
                        .expect("generated core run ID is valid"),
                    epoch_resume_data: std::collections::BTreeMap::new(),
                }))
                .expect("generated epoch starts");
            session
                .append_record(LaneRecord::StepAttempted(StepAttemptedRecord {
                    id: step_id.clone(),
                    operation_id: operation_id.clone(),
                    epoch_id: epoch_id.clone(),
                    kind: StepKind::Assistant,
                    attempt: 1,
                    result_entry_id: EntryId::new(format!("generated-result-{index:05}"))
                        .expect("generated result ID is valid"),
                    reason: None,
                }))
                .expect("generated step starts");
            session
                .append_record(LaneRecord::ProviderRequestStarted(
                    ProviderRequestStartedRecord {
                        request_id: request_id.clone(),
                        operation_id: operation_id.clone(),
                        epoch_id: epoch_id.clone(),
                        step_id,
                        physical_attempt: 1,
                        model_harness_profile: profile.clone(),
                        request_surface_digest: Digest::from_bytes(
                            format!("generated-request-surface-{index:05}").as_bytes(),
                        ),
                        idempotency_key: Some(format!("generated-request-key-{index:05}")),
                    },
                ))
                .expect("generated provider request starts");
            session
                .append_record(LaneRecord::ProviderRequestSettled(
                    ProviderRequestSettledRecord {
                        request_id: request_id.clone(),
                        operation_id: operation_id.clone(),
                        outcome: JsonValue::Null,
                        provider_error: Some(ProviderErrorRecord {
                            source: "response".into(),
                            message: Some("upstream rejected request".into()),
                            status_code: Some(429),
                            attempt: Some(1),
                            logical_request_id: Some("generated-request-id".into()),
                            visible_stream_event: Some(false),
                            auth_refresh_attempted: Some(false),
                            quota_reset_at_unix_seconds: Some(1_704_069_000),
                            error_type: Some("rate_limit".into()),
                            error_code: Some("too_many_requests".into()),
                            retryable: Some(true),
                            response_bytes: Some(128),
                            request_bytes: Some(512),
                            response_body: Some("{\"error\":\"redacted\"}".into()),
                        }),
                        usage: None,
                        response_artifact: None,
                        classification: ProviderSettlementClassification::Completed,
                    },
                ))
                .expect("generated provider request settles");
            session
                .append_record(LaneRecord::Usage(UsageRecord {
                    operation_id: operation_id.clone(),
                    request_id: Some(request_id),
                    usage: Usage {
                        total_tokens: Some(1_100),
                        input_tokens: Some(1_000),
                        output_tokens: Some(100),
                        reasoning_tokens: Some(50),
                        cache_read_tokens: Some(250),
                        cache_write_tokens: Some(25),
                        cost: Some("0.001".into()),
                    },
                }))
                .expect("generated usage persists");
            session
                .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
                    epoch_id,
                    operation_id: operation_id.clone(),
                    reason: EpochFinishReason::Settled,
                }))
                .expect("generated epoch finishes");
            session
                .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                    operation_id,
                    outcome: OperationOutcome::Completed,
                }))
                .expect("generated operation finishes");
        }
    }
    let append_elapsed = append_started.elapsed();
    let jsonl_bytes = std::fs::metadata(directory.join("session.jsonl"))
        .expect("fixture JSONL exists")
        .len();
    let replay_started = std::time::Instant::now();
    let reopened = JsonlSession::open(&directory, DurabilityMode::Development)
        .expect("generated operation fixture reopens");
    assert_eq!(
        reopened
            .snapshot()
            .expect("snapshot succeeds")
            .last_sequence(),
        Sequence(OPERATION_COUNT * MUTATIONS_PER_OPERATION)
    );
    let persisted_provider_error = reopened
        .snapshot()
        .expect("snapshot succeeds")
        .mutations()
        .any(|mutation| {
            let StoredMutationRef {
                mutation: SessionMutationRef::Record(record),
                ..
            } = mutation
            else {
                return false;
            };
            matches!(
                &record.record,
                LaneRecord::ProviderRequestSettled(settled)
                    if settled.provider_error.as_ref().is_some_and(|error|
                        error.status_code == Some(429)
                            && error.attempt == Some(1)
                            && error.response_body.as_deref() == Some("{\"error\":\"redacted\"}"))
            )
        });
    assert!(
        persisted_provider_error,
        "typed provider error survives JSONL replay"
    );
    let replay_elapsed = replay_started.elapsed();
    eprintln!(
        "generated-operations operations={OPERATION_COUNT} mutations={} jsonl_bytes={jsonl_bytes} append_ms={} replay_ms={}",
        OPERATION_COUNT * MUTATIONS_PER_OPERATION,
        append_elapsed.as_millis(),
        replay_elapsed.as_millis()
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

/// Short strict-mode companion to the long buffered fixture. Keeping this
/// count small makes the per-record synchronization cost observable without
/// turning routine persistence checks into a disk benchmark.
#[test]
#[ignore = "run explicitly to measure strict per-record synchronization latency"]
fn generated_strict_append_fixture_measures_synchronization() {
    const MUTATION_COUNT: u64 = 32;
    let directory = temporary_session_directory("generated-strict");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_100_000));
    let started = std::time::Instant::now();
    {
        let mut session = JsonlSession::create_with_clock(
            &directory,
            SessionHeader::new_at(
                SessionId::new("generated-strict").expect("valid session ID"),
                "generated-strict-workspace",
                Metadata::new(),
                1_700_000_099_999,
            ),
            DurabilityMode::Strict,
            clock,
        )
        .expect("strict fixture session creates");
        for index in 0..MUTATION_COUNT {
            session
                .append_entry(
                    &LaneId::main(),
                    ProvisionedEntry::user(
                        EntryId::new(format!("strict-entry-{index:05}"))
                            .expect("generated entry ID is valid"),
                        "strict durability sample",
                    ),
                )
                .expect("strict fixture entry commits");
        }
    }
    let elapsed = started.elapsed();
    eprintln!(
        "generated-strict mutations={MUTATION_COUNT} total_ms={} average_us={}",
        elapsed.as_millis(),
        elapsed.as_micros() / u128::from(MUTATION_COUNT)
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_round_trips_a_harness_catalog_fact_and_pins_its_manifest() {
    let directory = temporary_session_directory("harness-catalog-fact");
    let catalog_bytes = br#"{\"schema_version\":1,\"kind\":\"fixture\"}"#;
    let catalog_id = ArtifactId::from_bytes(catalog_bytes);
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("session-harness-catalog").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_fact(SessionFact::HarnessCatalog(HarnessCatalogFact {
            schema_version: 1,
            artifact_id: catalog_id,
            byte_len: catalog_bytes.len() as u64,
        }))
        .expect("catalog manifest reference persists");
    drop(session);
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict).expect("session reopens");
    let snapshot = reopened.snapshot().expect("snapshot succeeds");
    let fact = snapshot
        .facts()
        .first()
        .expect("catalog fact remains durable");
    assert!(matches!(
        &fact.fact,
        SessionFact::HarnessCatalog(HarnessCatalogFact {
            schema_version: 1,
            artifact_id,
            byte_len,
        }) if *artifact_id == catalog_id && *byte_len == catalog_bytes.len() as u64
    ));
    assert_eq!(fact.fact.artifact_references(), vec![catalog_id]);
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn session_export_copies_only_verified_reachable_objects_and_reopens_as_the_same_prefix() {
    let directory = temporary_session_directory("export-source");
    let export_directory = temporary_session_directory("export-destination");
    let (snapshot, retained, catalog, transitive, orphan) = {
        let mut session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new("session-export").expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Strict,
        )
        .expect("source session creates");
        let store = session.artifact_store().expect("source object store opens");
        let retained = store
            .put(b"recoverable tool evidence", "text/plain")
            .expect("retained object persists");
        let catalog = store
            .put(b"{\"schema_version\":1}", "application/json")
            .expect("catalog object persists");
        let transitive = store
            .put(b"immutable harness source", "text/plain")
            .expect("transitive harness root persists");
        let orphan = store
            .put(b"orphaned temporary object", "text/plain")
            .expect("orphan object persists");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::assistant(
                    EntryId::new("export-assistant").expect("valid entry ID"),
                    "",
                    vec![AssistantToolCall::new(
                        "call-export",
                        "fixture",
                        JsonValue::Null,
                    )],
                ),
            )
            .expect("assistant tool call persists");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry {
                    id: EntryId::new("export-tool-result").expect("valid entry ID"),
                    body: SessionEntry::ToolResult(ToolResultEntry {
                        tool_call_id: "call-export".into(),
                        tool_name: "fixture".into(),
                        full_result: PayloadRef::Artifact {
                            artifact_id: retained.artifact_id,
                            byte_len: retained.byte_len,
                            media_type: retained.media_type,
                        },
                        model_projection: JsonValue::String("bounded projection".into()),
                        is_error: false,
                        terminate: false,
                        usage: Usage::default(),
                        projection_strategy_id: "fixture-projection".into(),
                        artifact_policy_id: ArtifactPolicyId::new("fixture-policy")
                            .expect("fixture policy ID"),
                    }),
                },
            )
            .expect("tool result persists");
        session
            .append_fact(SessionFact::HarnessCatalog(HarnessCatalogFact {
                schema_version: 1,
                artifact_id: catalog.artifact_id,
                byte_len: catalog.byte_len,
            }))
            .expect("catalog fact persists");
        let snapshot = session.snapshot().expect("source snapshot");
        let verification = verify_session(&snapshot, &store, [transitive.artifact_id])
            .expect("source prefix and all roots verify");
        assert_eq!(verification.artifact_count, 3);
        assert_eq!(verification.last_digest, snapshot.last_digest());
        assert!(!verification.artifact_roots.contains(&orphan.artifact_id));
        assert_eq!(
            verification.orphaned_artifacts,
            vec![ArtifactInventoryItem {
                artifact_id: orphan.artifact_id,
                byte_len: orphan.byte_len,
            }],
            "verification reports finalized but unreachable immutable bytes separately"
        );

        let export = session
            .export_to(&export_directory, [transitive.artifact_id])
            .expect("complete export succeeds");
        assert_eq!(export.directory, export_directory);
        assert_eq!(
            export.verification.artifact_roots,
            verification.artifact_roots
        );
        assert!(
            export.verification.orphaned_artifacts.is_empty(),
            "an export copies reachable objects but omits source orphans"
        );
        (
            snapshot,
            retained.artifact_id,
            catalog.artifact_id,
            transitive.artifact_id,
            orphan.artifact_id,
        )
    };

    let exported = JsonlSession::open(&export_directory, DurabilityMode::Strict)
        .expect("exported session reopens");
    assert_eq!(exported.snapshot().expect("export snapshot"), snapshot);
    let manifest = JsonValue::parse(
        &std::fs::read_to_string(export_directory.join("export.json"))
            .expect("export manifest reads"),
    )
    .expect("export manifest is JSON");
    let fields = manifest.as_object().expect("export manifest is an object");
    assert_eq!(
        fields.get("through_digest").and_then(JsonValue::as_str),
        Some(snapshot.last_digest().to_hex().as_str())
    );
    assert_eq!(
        fields.get("through_seq").and_then(JsonValue::as_u64),
        Some(snapshot.last_sequence().0)
    );
    let store = exported
        .artifact_store()
        .expect("export object store opens");
    let mut expected_artifacts = [catalog, retained, transitive]
        .into_iter()
        .map(|artifact_id| {
            JsonValue::object([
                ("artifact_id", JsonValue::String(artifact_id.to_hex())),
                (
                    "byte_len",
                    JsonValue::from(
                        store
                            .verify_object(artifact_id)
                            .expect("reachable export object verifies"),
                    ),
                ),
            ])
        })
        .collect::<Vec<_>>();
    expected_artifacts.sort_by(|left, right| {
        left.get("artifact_id")
            .and_then(JsonValue::as_str)
            .cmp(&right.get("artifact_id").and_then(JsonValue::as_str))
    });
    assert_eq!(
        fields
            .get("artifacts")
            .and_then(JsonValue::as_array)
            .expect("manifest artifacts are an array"),
        &expected_artifacts
    );
    for artifact_id in [retained, catalog, transitive] {
        assert!(
            store.get(artifact_id).is_ok(),
            "reachable object {artifact_id} copied"
        );
    }
    assert!(matches!(
        store.get(orphan),
        Err(ArtifactError::NotFound { .. })
    ));
    drop(exported);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::remove_dir_all(&export_directory);
}

#[test]
fn verification_rejects_an_artifact_length_that_disagrees_with_durable_metadata() {
    let store = MemoryArtifactStore::default();
    let artifact = store
        .put(b"actual immutable bytes", "text/plain")
        .expect("artifact persists");
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("verification-length").expect("session ID"),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("verification-length-entry").expect("entry ID"),
                body: SessionEntry::Custom(CustomEntry {
                    type_name: "trusted.length-check".into(),
                    payload: PayloadRef::Artifact {
                        artifact_id: artifact.artifact_id,
                        byte_len: artifact.byte_len.saturating_add(1),
                        media_type: artifact.media_type,
                    },
                    model_visible: false,
                }),
            },
        )
        .expect("entry persists");
    assert!(matches!(
        verify_session(
            &session.snapshot().expect("snapshot"),
            &store,
            std::iter::empty(),
        ),
        Err(SessionVerificationError::LengthMismatch { .. })
    ));
}

#[test]
fn verification_rejects_missing_and_wrong_filesystem_artifact_objects() {
    let root = temporary_session_directory("verification-artifact-integrity");
    let store = FileArtifactStore::open(&root).expect("filesystem artifact store opens");
    let bytes = b"immutable verification evidence";
    let artifact = store.put(bytes, "text/plain").expect("artifact persists");
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("verification-artifact-integrity").expect("session ID"),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("verification-artifact-integrity-entry").expect("entry ID"),
                body: SessionEntry::Custom(CustomEntry {
                    type_name: "trusted.artifact-integrity".into(),
                    payload: PayloadRef::Artifact {
                        artifact_id: artifact.artifact_id,
                        byte_len: artifact.byte_len,
                        media_type: artifact.media_type.clone(),
                    },
                    model_visible: false,
                }),
            },
        )
        .expect("artifact reference persists");
    let snapshot = session.snapshot().expect("snapshot succeeds");
    let digest = artifact.artifact_id.to_hex();
    let object_path = root.join("blake3").join(&digest[..2]).join(&digest);

    std::fs::remove_file(&object_path).expect("artifact object removes");
    assert!(matches!(
        verify_session(&snapshot, &store, std::iter::empty()),
        Err(SessionVerificationError::Artifact(ArtifactError::NotFound { artifact_id }))
            if artifact_id == artifact.artifact_id
    ));

    store
        .put(bytes, "text/plain")
        .expect("missing immutable artifact republishes");
    std::fs::write(&object_path, b"different bytes under a trusted digest")
        .expect("test corrupts object bytes");
    assert!(matches!(
        verify_session(&snapshot, &store, std::iter::empty()),
        Err(SessionVerificationError::Artifact(ArtifactError::Corruption { artifact_id, .. }))
            if artifact_id == artifact.artifact_id
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reducer_derives_interrupted_tool_recovery_without_replaying_never_policy() {
    let session_id = SessionId::new("session-recovery").expect("valid session ID");
    let lane = LaneId::main();
    let operation_id = OperationId::new("operation-1").expect("valid operation ID");
    let epoch_id = EpochId::new("epoch-1").expect("valid epoch ID");
    let assistant_entry = EntryId::new("assistant-1").expect("valid entry ID");
    let result_entry = EntryId::new("result-1").expect("valid entry ID");
    let mut session = MemorySession::create(SessionHeader::new(
        session_id,
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creation succeeds");

    session
        .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
            operation_id.clone(),
            lane.clone(),
            None,
            OperationKind::Run,
            Vec::new(),
            HarnessRevisionId::new("revision-1").expect("valid revision ID"),
            ModelHarnessProfileId::new("profile-1").expect("valid profile ID"),
        )))
        .expect("operation is durable");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: None,
            harness_revision_id: HarnessRevisionId::new("revision-1").expect("valid revision ID"),
            harness_snapshot_id: HarnessSnapshotId::new("snapshot-1").expect("valid snapshot ID"),
            model_harness_profile: ModelHarnessProfileId::new("profile-1")
                .expect("valid profile ID"),
            core_run_id: CoreRunId::new("core-run-1").expect("valid core run ID"),
            epoch_resume_data: std::collections::BTreeMap::new(),
        }))
        .expect("epoch is durable");
    session
        .append_entry(
            &lane,
            ProvisionedEntry::assistant(
                assistant_entry.clone(),
                "",
                vec![AssistantToolCall::new("call-1", "write", JsonValue::Null)],
            ),
        )
        .expect("assistant entry persists");
    session
        .append_record(LaneRecord::tool_started(ToolStartedRecord::new(
            RecordId::new("record-1").expect("valid record ID"),
            operation_id,
            epoch_id,
            assistant_entry,
            0,
            "call-1",
            "write",
            JsonValue::Null,
            result_entry.clone(),
            ToolReplayPolicy::Never,
            Digest::from_bytes(b"tool-definition"),
            HarnessRevisionId::new("revision-1").expect("valid revision ID"),
            "tool-idempotency-key",
        )))
        .expect("tool intent is durable");

    let reduction =
        reduce_lane(session.snapshot().expect("snapshot succeeds"), lane).expect("prefix is valid");
    assert_eq!(
        reduction.recovery_plan,
        Some(RecoveryPlan::SynthesizeInterruptedToolResult {
            result_entry_id: result_entry,
        })
    );
}

#[test]
fn jsonl_reopen_preserves_tool_intent_and_derives_the_same_recovery_plan() {
    let directory = temporary_session_directory("tool-recovery");
    let lane = LaneId::main();
    let operation_id = OperationId::new("jsonl-operation").expect("valid operation ID");
    let epoch_id = EpochId::new("jsonl-epoch").expect("valid epoch ID");
    let assistant_id = EntryId::new("jsonl-assistant").expect("valid entry ID");
    let result_id = EntryId::new("jsonl-result").expect("valid entry ID");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("session-jsonl-recovery").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
            operation_id.clone(),
            lane.clone(),
            None,
            OperationKind::Run,
            Vec::new(),
            HarnessRevisionId::new("jsonl-revision").expect("valid revision ID"),
            ModelHarnessProfileId::new("jsonl-profile").expect("valid profile ID"),
        )))
        .expect("operation starts");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: None,
            harness_revision_id: HarnessRevisionId::new("jsonl-revision")
                .expect("valid revision ID"),
            harness_snapshot_id: HarnessSnapshotId::new("jsonl-snapshot")
                .expect("valid snapshot ID"),
            model_harness_profile: ModelHarnessProfileId::new("jsonl-profile")
                .expect("valid profile ID"),
            core_run_id: CoreRunId::new("jsonl-core-run").expect("valid core run ID"),
            epoch_resume_data: std::collections::BTreeMap::new(),
        }))
        .expect("epoch starts");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_entry(
            &lane,
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                "",
                vec![AssistantToolCall::new(
                    "call-jsonl",
                    "write",
                    JsonValue::Null,
                )],
            ),
        )
        .expect("assistant entry persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::tool_started(ToolStartedRecord::new(
            RecordId::new("jsonl-tool-record").expect("valid record ID"),
            operation_id.clone(),
            epoch_id.clone(),
            assistant_id.clone(),
            0,
            "call-jsonl",
            "write",
            JsonValue::Null,
            result_id.clone(),
            ToolReplayPolicy::Never,
            Digest::from_bytes(b"jsonl-tool-definition"),
            HarnessRevisionId::new("jsonl-revision").expect("valid revision ID"),
            "jsonl-tool-idempotency",
        )))
        .expect("tool intent persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    let reduction = reduce_lane(session.snapshot().expect("snapshot succeeds"), lane.clone())
        .expect("durable prefix reduces");
    assert_eq!(
        reduction.recovery_plan,
        Some(RecoveryPlan::SynthesizeInterruptedToolResult {
            result_entry_id: result_id,
        })
    );
    let first_step = StepId::new("jsonl-provider-step-one").expect("valid step ID");
    let first_request =
        ProviderRequestId::new("jsonl-provider-request-one").expect("valid request ID");
    session
        .append_record(LaneRecord::StepAttempted(StepAttemptedRecord {
            id: first_step.clone(),
            operation_id: operation_id.clone(),
            epoch_id: epoch_id.clone(),
            kind: StepKind::Assistant,
            attempt: 1,
            result_entry_id: EntryId::new("jsonl-provider-result-one")
                .expect("valid result entry ID"),
            reason: None,
        }))
        .expect("first provider step persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::ProviderRequestStarted(
            ProviderRequestStartedRecord {
                request_id: first_request.clone(),
                operation_id: operation_id.clone(),
                epoch_id: epoch_id.clone(),
                step_id: first_step,
                physical_attempt: 1,
                model_harness_profile: ModelHarnessProfileId::new("jsonl-profile")
                    .expect("valid profile ID"),
                request_surface_digest: Digest::from_bytes(b"jsonl-provider-surface-one"),
                idempotency_key: Some("jsonl-provider-key-one".into()),
            },
        ))
        .expect("first provider intent persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::ProviderRequestSettled(
            ProviderRequestSettledRecord {
                request_id: first_request,
                operation_id: operation_id.clone(),
                outcome: JsonValue::Null,
                provider_error: None,
                usage: None,
                response_artifact: None,
                classification: ProviderSettlementClassification::Retryable,
            },
        ))
        .expect("retryable provider settlement persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    let second_step = StepId::new("jsonl-provider-step-two").expect("valid step ID");
    let second_request =
        ProviderRequestId::new("jsonl-provider-request-two").expect("valid request ID");
    session
        .append_record(LaneRecord::StepAttempted(StepAttemptedRecord {
            id: second_step.clone(),
            operation_id: operation_id.clone(),
            epoch_id: epoch_id.clone(),
            kind: StepKind::Assistant,
            attempt: 2,
            result_entry_id: EntryId::new("jsonl-provider-result-two")
                .expect("valid result entry ID"),
            reason: Some("retry after transport settlement".into()),
        }))
        .expect("second provider step persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::ProviderRequestStarted(
            ProviderRequestStartedRecord {
                request_id: second_request.clone(),
                operation_id: operation_id.clone(),
                epoch_id: epoch_id.clone(),
                step_id: second_step,
                physical_attempt: 2,
                model_harness_profile: ModelHarnessProfileId::new("jsonl-profile")
                    .expect("valid profile ID"),
                request_surface_digest: Digest::from_bytes(b"jsonl-provider-surface-two"),
                idempotency_key: Some("jsonl-provider-key-two".into()),
            },
        ))
        .expect("second provider intent persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::ProviderRequestSettled(
            ProviderRequestSettledRecord {
                request_id: second_request.clone(),
                operation_id: operation_id.clone(),
                outcome: JsonValue::Null,
                provider_error: None,
                usage: None,
                response_artifact: None,
                classification: ProviderSettlementClassification::Completed,
            },
        ))
        .expect("completed provider settlement persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    let wire = std::fs::read_to_string(directory.join("session.jsonl"))
        .expect("session JSONL can be read");
    let completed_settlement = wire
        .lines()
        .find(|line| line.contains("jsonl-provider-request-two"))
        .expect("completed settlement is present");
    assert!(
        !completed_settlement.contains("provider_error"),
        "absent diagnostics must not alter the v1 authenticated wire shape"
    );
    session
        .append_record(LaneRecord::Usage(UsageRecord {
            operation_id: operation_id.clone(),
            request_id: Some(second_request),
            usage: Usage {
                total_tokens: Some(5),
                input_tokens: Some(3),
                output_tokens: Some(2),
                reasoning_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cost: None,
            },
        }))
        .expect("usage persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id,
            operation_id: operation_id.clone(),
            reason: EpochFinishReason::Settled,
        }))
        .expect("epoch settlement persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id,
            outcome: OperationOutcome::Completed,
        }))
        .expect("operation settlement persists");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    let completed = reduce_lane(
        session.snapshot().expect("completed snapshot succeeds"),
        lane.clone(),
    )
    .expect("completed prefix reduces");
    assert_eq!(completed.lane_state.status, LaneStatus::Idle);
    assert_eq!(completed.usage_totals.input_tokens, Some(3));
    assert_eq!(completed.usage_totals.output_tokens, Some(2));
    drop(session);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_reopen_fixed_point_covers_compaction_harness_activation_and_core_rollover() {
    let directory = temporary_session_directory("activation-rollover-fixed-point");
    let lane = LaneId::main();
    let revision_a = HarnessRevisionId::new("fixed-point-revision-a").expect("valid revision ID");
    let revision_b = HarnessRevisionId::new("fixed-point-revision-b").expect("valid revision ID");
    let snapshot_a = HarnessSnapshotId::new("fixed-point-snapshot-a").expect("valid snapshot ID");
    let snapshot_b = HarnessSnapshotId::new("fixed-point-snapshot-b").expect("valid snapshot ID");
    let profile = ModelHarnessProfileId::new("fixed-point-profile").expect("valid profile ID");
    let operation_id = OperationId::new("fixed-point-operation").expect("valid operation ID");
    let context_id = EntryId::new("fixed-point-context").expect("valid entry ID");
    let revision_entry_id = EntryId::new("fixed-point-revision-entry").expect("valid entry ID");
    let compaction_id = EntryId::new("fixed-point-compaction").expect("valid entry ID");
    let activation_entry_id = EntryId::new("fixed-point-activation-entry").expect("valid entry ID");
    let first_epoch = EpochId::new("fixed-point-epoch-one").expect("valid epoch ID");
    let second_epoch = EpochId::new("fixed-point-epoch-two").expect("valid epoch ID");

    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("activation-rollover-fixed-point").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &lane,
            ProvisionedEntry::user(context_id.clone(), "checkpoint source context"),
        )
        .expect("context entry commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: revision_entry_id.clone(),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision_a.clone(),
                    snapshot_id: snapshot_a.clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("initial revision commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: compaction_id.clone(),
                body: SessionEntry::Compaction(CompactionEntry {
                    covered_from: Some(context_id),
                    covered_to: Some(revision_entry_id),
                    retained_tail_boundary: None,
                    summary: "checkpointed source context".into(),
                    strategy_id: "fixed-point-compaction".into(),
                    recovery_index_artifact: None,
                    harness_revision_id: Some(revision_a.clone()),
                }),
            },
        )
        .expect("compaction commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::operation_started(OperationStartedRecord::new(
            operation_id.clone(),
            lane.clone(),
            Some(compaction_id.clone()),
            OperationKind::Run,
            Vec::new(),
            revision_a.clone(),
            profile.clone(),
        )))
        .expect("operation commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: first_epoch.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(compaction_id),
            harness_revision_id: revision_a.clone(),
            harness_snapshot_id: snapshot_a,
            model_harness_profile: profile.clone(),
            core_run_id: CoreRunId::new("fixed-point-core-run-one").expect("valid core run ID"),
            epoch_resume_data: Default::default(),
        }))
        .expect("first epoch commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::HarnessActivationRequested(
            HarnessActivationRequestedRecord {
                operation_id: operation_id.clone(),
                candidate_id: HarnessCandidateId::new("fixed-point-candidate")
                    .expect("valid candidate ID"),
                parent_revision_id: revision_a.clone(),
                proposed_snapshot_id: snapshot_b.clone(),
                revision_entry_id: activation_entry_id.clone(),
            },
        ))
        .expect("activation request commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id: first_epoch,
            operation_id: operation_id.clone(),
            reason: EpochFinishReason::ActivationPending,
        }))
        .expect("first epoch settles for activation");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: activation_entry_id.clone(),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision_b.clone(),
                    snapshot_id: snapshot_b.clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("activation revision commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: second_epoch.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 1,
            source_leaf_id: Some(activation_entry_id),
            harness_revision_id: revision_b,
            harness_snapshot_id: snapshot_b,
            model_harness_profile: profile,
            core_run_id: CoreRunId::new("fixed-point-core-run-two").expect("valid core run ID"),
            epoch_resume_data: Default::default(),
        }))
        .expect("rollover epoch commits");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id: second_epoch,
            operation_id: operation_id.clone(),
            reason: EpochFinishReason::Settled,
        }))
        .expect("rollover epoch settles");
    session = assert_reopen_fixed_point(session, &directory, &lane);
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id,
            outcome: OperationOutcome::Completed,
        }))
        .expect("operation terminates");
    let _ = assert_reopen_fixed_point(session, &directory, &lane);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_a_newline_terminated_malformed_tail_without_repairing_it() {
    let directory = temporary_session_directory("torn-tail");
    let session_id = SessionId::new("session-jsonl").expect("valid session ID");
    let header = SessionHeader::new(session_id, "workspace-test", Metadata::new());
    {
        let mut session = JsonlSession::create(&directory, header, DurabilityMode::Strict)
            .expect("v1 session creation succeeds");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(
                    EntryId::new("entry-jsonl").expect("valid entry ID"),
                    "durable",
                ),
            )
            .expect("entry append is durable");
    }
    let path = directory.join("session.jsonl");
    use std::io::Write as _;
    let malformed = include_bytes!("../fixtures/wire/malformed-complete-mutation.json");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("session file opens")
        .write_all(malformed)
        .expect("committed-looking malformed tail is injected");

    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::Format { line: 3, .. })
    ));
    let bytes = std::fs::read(&path).expect("session file remains readable");
    assert!(bytes.ends_with(malformed));
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_format_errors_include_the_committed_line_byte_offset() {
    use std::io::Write as _;

    let directory = temporary_session_directory("format-offset");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("format-offset").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    drop(session);
    let path = directory.join("session.jsonl");
    let offset = std::fs::metadata(&path)
        .expect("header metadata reads")
        .len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("session file opens")
        .write_all(include_bytes!(
            "../fixtures/wire/malformed-complete-mutation.json"
        ))
        .expect("malformed line injects");

    let error = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect_err("malformed complete line faults closed");
    assert!(
        error
            .to_string()
            .contains(&format!("line 2 at byte {offset}")),
        "format errors report the exact committed-line offset without payload text: {error}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_requires_explicit_repair_for_an_unterminated_torn_tail() {
    use std::io::Write as _;

    let directory = temporary_session_directory("explicit-torn-tail-repair");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("explicit-torn-tail-repair").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("explicit-torn-tail-entry").expect("valid entry ID"),
                "durable prefix",
            ),
        )
        .expect("entry commits");
    drop(session);
    let path = directory.join("session.jsonl");
    let prefix = std::fs::read(&path).expect("durable prefix reads");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("session file opens")
        .write_all(b"{\"digest\":\"")
        .expect("torn tail writes");

    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::RecoveryRequired { offset, .. }) if offset == prefix.len() as u64
    ));
    assert!(
        std::fs::read(&path)
            .expect("session file reads")
            .ends_with(b"{\"digest\":\""),
        "ordinary writer open does not mutate a torn tail"
    );
    let inspection = JsonlSession::inspect(&directory).expect("read-only inspection succeeds");
    assert_eq!(inspection.snapshot.last_sequence().0, 1);
    assert_eq!(inspection.torn_tail_offset, Some(prefix.len() as u64));

    let repaired = JsonlSession::repair_torn_tail(&directory, DurabilityMode::Strict)
        .expect("explicit repair succeeds");
    assert_eq!(repaired.truncated_tail_offset, Some(prefix.len() as u64));
    assert_eq!(std::fs::read(&path).expect("repaired file reads"), prefix);
    assert!(JsonlSession::open(&directory, DurabilityMode::Strict).is_ok());
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_repairs_every_truncation_inside_one_committed_record_to_the_header_prefix() {
    let source_directory = temporary_session_directory("torn-tail-matrix-source");
    let source = {
        let mut session = JsonlSession::create(
            &source_directory,
            SessionHeader::new(
                SessionId::new("torn-tail-matrix-source").expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Development,
        )
        .expect("source session creates");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(
                    EntryId::new("torn-tail-matrix-entry").expect("valid entry ID"),
                    "torn-tail matrix payload",
                ),
            )
            .expect("source entry commits");
        drop(session);
        std::fs::read(source_directory.join("session.jsonl")).expect("source JSONL reads")
    };
    let header_len = source
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("source has a header newline")
        + 1;

    for offset in (header_len + 1)..source.len() {
        let directory = temporary_session_directory("torn-tail-matrix-case");
        let session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new(format!("torn-tail-matrix-{offset}")).expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Development,
        )
        .expect("case session creates");
        drop(session);
        let path = directory.join("session.jsonl");
        std::fs::write(&path, &source[..offset]).expect("truncated fixture writes");

        let inspection = JsonlSession::inspect(&directory).expect("inspection succeeds");
        assert_eq!(inspection.snapshot.last_sequence(), Sequence(0));
        assert_eq!(inspection.torn_tail_offset, Some(header_len as u64));
        assert_eq!(
            std::fs::read(&path).expect("inspection leaves bytes readable"),
            source[..offset],
            "inspection must not mutate truncation offset {offset}"
        );

        let repaired = JsonlSession::repair_torn_tail(&directory, DurabilityMode::Development)
            .expect("explicit repair succeeds");
        assert_eq!(repaired.truncated_tail_offset, Some(header_len as u64));
        assert_eq!(
            std::fs::read(&path).expect("repaired prefix reads"),
            source[..header_len],
            "repair must retain only the complete header at truncation offset {offset}"
        );
        assert!(JsonlSession::open(&directory, DurabilityMode::Development).is_ok());
        let _ = std::fs::remove_dir_all(&directory);
    }
    let _ = std::fs::remove_dir_all(&source_directory);
}

#[test]
fn jsonl_never_repairs_any_truncation_of_the_required_header() {
    let source_directory = temporary_session_directory("torn-header-matrix-source");
    let source = {
        let session = JsonlSession::create(
            &source_directory,
            SessionHeader::new(
                SessionId::new("torn-header-matrix-source").expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Development,
        )
        .expect("source session creates");
        drop(session);
        std::fs::read(source_directory.join("session.jsonl")).expect("source JSONL reads")
    };

    for offset in 0..source.len() {
        let directory = temporary_session_directory("torn-header-matrix-case");
        let session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new(format!("torn-header-matrix-{offset}")).expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Development,
        )
        .expect("case session creates");
        drop(session);
        let path = directory.join("session.jsonl");
        std::fs::write(&path, &source[..offset]).expect("truncated header fixture writes");

        assert!(matches!(
            JsonlSession::inspect(&directory),
            Err(SessionError::Format {
                line: 1,
                offset: 0,
                ..
            })
        ));
        assert!(matches!(
            JsonlSession::repair_torn_tail(&directory, DurabilityMode::Development),
            Err(SessionError::Format {
                line: 1,
                offset: 0,
                ..
            })
        ));
        assert_eq!(
            std::fs::read(&path).expect("rejected header remains readable"),
            source[..offset],
            "a truncated header is never a repairable tail at offset {offset}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
    let _ = std::fs::remove_dir_all(&source_directory);
}

#[test]
fn injected_write_failures_poison_only_indeterminate_writers_without_hybrid_records() {
    use crate::jsonl::{TestWriteFailpoint, install_test_write_failpoint};

    let reference_directory = temporary_session_directory("write-failure-reference");
    let reference_bytes = {
        let mut session = JsonlSession::create(
            &reference_directory,
            SessionHeader::new(
                SessionId::new("write-failure-reference").expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Development,
        )
        .expect("reference session creates");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(
                    EntryId::new("write-failure-entry").expect("valid entry ID"),
                    "write failure fixture",
                ),
            )
            .expect("reference entry commits");
        drop(session);
        std::fs::read(reference_directory.join("session.jsonl")).expect("reference JSONL reads")
    };
    let reference_header_len = reference_bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("reference header newline")
        + 1;
    let json_len = reference_bytes.len() - reference_header_len - 1;

    let mut cases = (0..=json_len)
        .map(|offset| {
            (
                TestWriteFailpoint::AfterJsonBytes(offset),
                DurabilityMode::Development,
                false,
            )
        })
        .collect::<Vec<_>>();
    cases.extend([
        (
            TestWriteFailpoint::BeforeAppend,
            DurabilityMode::Strict,
            false,
        ),
        (
            TestWriteFailpoint::AfterJsonBeforeNewline,
            DurabilityMode::Strict,
            false,
        ),
        (
            TestWriteFailpoint::AfterNewlineBeforeFlush,
            DurabilityMode::Strict,
            true,
        ),
        (
            TestWriteFailpoint::DuringFlush,
            DurabilityMode::Strict,
            true,
        ),
        (
            TestWriteFailpoint::AfterFlushBeforeSync,
            DurabilityMode::Strict,
            true,
        ),
        (TestWriteFailpoint::DuringSync, DurabilityMode::Strict, true),
        (
            TestWriteFailpoint::AfterSyncBeforeReturn,
            DurabilityMode::Strict,
            true,
        ),
    ]);

    for (case_index, (failpoint, durability, complete_record)) in cases.into_iter().enumerate() {
        let indeterminate = !matches!(
            &failpoint,
            TestWriteFailpoint::BeforeAppend | TestWriteFailpoint::AfterJsonBytes(0)
        );
        let directory = temporary_session_directory("write-failure-case");
        let mut session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new(format!("write-failure-case-{case_index}"))
                    .expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            durability,
        )
        .expect("case session creates");
        let header_len = std::fs::metadata(directory.join("session.jsonl"))
            .expect("case header metadata reads")
            .len();
        let failpoint_guard = install_test_write_failpoint(failpoint.clone());
        let error = session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(
                    EntryId::new("write-failure-entry").expect("valid entry ID"),
                    "write failure fixture",
                ),
            )
            .expect_err("injected append interruption fails");
        if indeterminate {
            assert!(
                matches!(error, SessionError::IndeterminateWrite { .. }),
                "{failpoint:?} occurs after an append attempt and may have changed the durable prefix"
            );
        } else {
            assert!(
                matches!(error, SessionError::Io { .. }),
                "{failpoint:?} is rejected before a non-empty append attempt"
            );
        }
        drop(failpoint_guard);

        let expected_sequence = if indeterminate {
            assert!(matches!(
                session.append_entry(
                    &LaneId::main(),
                    ProvisionedEntry::user(
                        EntryId::new("write-failure-after-fault").expect("valid entry ID"),
                        "must not start a dependent append",
                    ),
                ),
                Err(SessionError::Faulted { .. })
            ));
            u64::from(complete_record)
        } else {
            assert_eq!(
                session
                    .snapshot()
                    .expect("pre-write rejection leaves the live prefix unchanged")
                    .last_sequence(),
                Sequence(0)
            );
            session
                .append_entry(
                    &LaneId::main(),
                    ProvisionedEntry::user(
                        EntryId::new("write-failure-retry").expect("valid entry ID"),
                        "retry after a pre-write rejection",
                    ),
                )
                .expect("pre-write rejection leaves the same writer retryable");
            1
        };
        drop(session);

        let inspection = JsonlSession::inspect(&directory).expect("inspection succeeds");
        assert_eq!(
            inspection.snapshot.last_sequence(),
            Sequence(expected_sequence),
            "the durable prefix follows the append-failure classification for case {case_index}"
        );
        if expected_sequence == 1 {
            assert_eq!(inspection.torn_tail_offset, None);
            let reopened = JsonlSession::open(&directory, durability)
                .expect("complete record reopens after interrupted return");
            assert_eq!(
                reopened
                    .snapshot()
                    .expect("reopened snapshot succeeds")
                    .last_sequence(),
                Sequence(1)
            );
            drop(reopened);
        } else {
            if inspection.torn_tail_offset.is_some() {
                let repaired = JsonlSession::repair_torn_tail(&directory, durability)
                    .expect("only the uncommitted tail is repairable");
                assert_eq!(repaired.truncated_tail_offset, Some(header_len));
            }
            let reopened = JsonlSession::open(&directory, durability)
                .expect("prior complete prefix reopens after repair or empty interruption");
            assert_eq!(
                reopened
                    .snapshot()
                    .expect("reopened snapshot succeeds")
                    .last_sequence(),
                Sequence(0)
            );
            drop(reopened);
        }
        let _ = std::fs::remove_dir_all(&directory);
    }
    let _ = std::fs::remove_dir_all(&reference_directory);
}

#[test]
fn jsonl_rejects_an_oversized_complete_line_without_repairing_it() {
    use std::io::Write as _;

    let directory = temporary_session_directory("oversized-complete-line");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("oversized-complete-line").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    drop(session);
    let path = directory.join("session.jsonl");
    let mut oversized = vec![b' '; 1_048_577];
    oversized.push(b'\n');
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("session file opens")
        .write_all(&oversized)
        .expect("oversized line injects");

    let error = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect_err("complete oversized line is corruption");
    assert!(matches!(
        error,
        SessionError::Format { line: 2, ref message, .. }
            if message == "session line exceeds the 1048576-byte line limit"
    ));
    assert!(
        std::fs::read(&path)
            .expect("session file remains readable")
            .ends_with(&oversized),
        "complete oversized lines are never repaired"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_an_oversized_mutation_before_it_reaches_the_log() {
    let directory = temporary_session_directory("oversized-mutation-write");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("oversized-mutation-write").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let prefix = std::fs::read(directory.join("session.jsonl")).expect("header reads");
    let oversized_content = "x".repeat(1_048_576);

    assert!(matches!(
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("oversized-mutation-entry").expect("valid entry ID"),
                oversized_content,
            ),
        ),
        Err(SessionError::InvalidInput { ref message })
            if message == "session line exceeds the 1048576-byte line limit"
    ));
    assert_eq!(
        std::fs::read(directory.join("session.jsonl")).expect("session file reads"),
        prefix,
        "the oversized mutation was never appended"
    );
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("bounded-mutation-entry").expect("valid entry ID"),
                "still writable",
            ),
        )
        .expect("rejected input does not fault the writer");
    drop(session);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_a_semantically_valid_payload_tampered_after_commit() {
    let directory = temporary_session_directory("integrity-payload-tamper");
    {
        let mut session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new("integrity-payload-tamper").expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Strict,
        )
        .expect("session creates");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(
                    EntryId::new("integrity-payload-entry").expect("valid entry ID"),
                    "one",
                ),
            )
            .expect("entry commits");
    }
    let path = directory.join("session.jsonl");
    let source = String::from_utf8(std::fs::read(&path).expect("session file reads"))
        .expect("session file is UTF-8");
    let tampered = source.replacen("\"content\":\"one\"", "\"content\":\"two\"", 1);
    assert_ne!(source, tampered, "fixture targets a complete JSON payload");
    std::fs::write(&path, tampered).expect("tampered JSON writes");

    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::Format {
            line: 2,
            sequence: Some(Sequence(1)),
            mutation_kind: Some(ref kind),
            ref message,
            ..
        }) if kind == "entry" && message == "record digest mismatch"
    ));
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_integrity_chain_corruption_at_a_deterministic_boundary() {
    let directory = temporary_session_directory("integrity-corruption-matrix");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_000_001));
    let mut session = JsonlSession::create_with_clock(
        &directory,
        SessionHeader::new_at(
            SessionId::new("integrity-corruption-matrix").expect("valid session ID"),
            "integrity-workspace",
            Metadata::new(),
            1_700_000_000_000,
        ),
        DurabilityMode::Strict,
        clock,
    )
    .expect("session creates");
    let empty_snapshot = session.snapshot().expect("sealed header snapshot");
    for (id, content) in [
        ("integrity-entry-one", "content-one"),
        ("integrity-entry-two", "content-two"),
        ("integrity-entry-three", "content-three"),
    ] {
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(EntryId::new(id).expect("valid entry ID"), content),
            )
            .expect("entry commits");
    }
    drop(session);
    let lines = std::fs::read_to_string(directory.join("session.jsonl"))
        .expect("session log reads")
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    let foreign_directory = temporary_session_directory("integrity-corruption-foreign");
    let mut foreign = JsonlSession::create_with_clock(
        &foreign_directory,
        SessionHeader::new_at(
            SessionId::new("integrity-corruption-foreign").expect("valid session ID"),
            "integrity-workspace",
            Metadata::new(),
            1_700_000_000_000,
        ),
        DurabilityMode::Strict,
        Arc::new(FixedSessionClock(1_700_000_000_001)),
    )
    .expect("foreign session creates");
    foreign
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("integrity-foreign-entry").expect("valid entry ID"),
                "foreign content",
            ),
        )
        .expect("foreign entry commits");
    drop(foreign);
    let foreign_record = std::fs::read_to_string(foreign_directory.join("session.jsonl"))
        .expect("foreign log reads")
        .lines()
        .nth(1)
        .expect("foreign record exists")
        .to_owned();

    let alter_digest = |line: &str, field: &str| {
        let marker = format!(r#""{field}":""#);
        let value = line
            .split_once(&marker)
            .expect("digest field exists")
            .1
            .split_once('"')
            .expect("digest field terminates")
            .0;
        let replacement = format!(
            "{}{}",
            if value.starts_with('0') { "1" } else { "0" },
            &value[1..]
        );
        line.replacen(value, &replacement, 1)
    };
    let semantic_parent = EntryId::new("integrity-missing-parent").expect("valid entry ID");
    let semantic_mutation = crate::jsonl::seal_mutation(
        &empty_snapshot,
        SessionMutation::Entry(StoredEntry {
            lane_id: LaneId::main(),
            header: EntryHeader {
                id: EntryId::new("integrity-orphan-entry").expect("valid entry ID"),
                parent_id: Some(semantic_parent),
                seq: Sequence(1),
                timestamp_ms: 1_700_000_000_001,
            },
            body: SessionEntry::UserMessage(UserMessageEntry {
                content: "semantically orphaned but integrity-sealed".into(),
                metadata: Metadata::new(),
            }),
        }),
    )
    .expect("semantic corruption can be integrity sealed");
    let semantic_line = crate::jsonl::encode_mutation(&semantic_mutation)
        .to_json_string()
        .expect("semantic corruption encodes canonically");

    let mut deleted = lines.clone();
    deleted.remove(1);
    let mut duplicated = lines.clone();
    duplicated.insert(2, lines[1].clone());
    let mut reordered = lines.clone();
    reordered.swap(1, 2);
    let mut inserted_foreign = lines.clone();
    inserted_foreign.insert(1, foreign_record);
    let cases = vec![
        (
            "header bit flip",
            format!(
                "{}\n",
                lines[0].replace("integrity-workspace", "integritY-workspace")
            )
            .into_bytes(),
            1,
            false,
        ),
        (
            "payload bit flip",
            [
                lines[0].clone(),
                lines[1].replace("content-one", "content-One"),
                lines[2].clone(),
                lines[3].clone(),
            ]
            .join("\n")
            .into_bytes(),
            2,
            false,
        ),
        (
            "sequence bit flip",
            [
                lines[0].clone(),
                lines[1].replace(r#""seq":1"#, r#""seq":9"#),
                lines[2].clone(),
                lines[3].clone(),
            ]
            .join("\n")
            .into_bytes(),
            2,
            false,
        ),
        (
            "timestamp bit flip",
            [
                lines[0].clone(),
                lines[1].replace(
                    r#""timestamp_ms":1700000000001"#,
                    r#""timestamp_ms":1700000000002"#,
                ),
                lines[2].clone(),
                lines[3].clone(),
            ]
            .join("\n")
            .into_bytes(),
            2,
            false,
        ),
        (
            "record digest bit flip",
            [
                lines[0].clone(),
                alter_digest(&lines[1], "digest"),
                lines[2].clone(),
                lines[3].clone(),
            ]
            .join("\n")
            .into_bytes(),
            2,
            false,
        ),
        (
            "previous digest bit flip",
            [
                lines[0].clone(),
                alter_digest(&lines[1], "prev_digest"),
                lines[2].clone(),
                lines[3].clone(),
            ]
            .join("\n")
            .into_bytes(),
            2,
            false,
        ),
        ("record deletion", deleted.join("\n").into_bytes(), 2, false),
        (
            "record duplication",
            duplicated.join("\n").into_bytes(),
            3,
            false,
        ),
        (
            "record reordering",
            reordered.join("\n").into_bytes(),
            2,
            false,
        ),
        (
            "record insertion from another session",
            inserted_foreign.join("\n").into_bytes(),
            2,
            false,
        ),
        (
            "sequence zero",
            [
                lines[0].clone(),
                lines[1].replace(r#""seq":1"#, r#""seq":0"#),
                lines[2].clone(),
                lines[3].clone(),
            ]
            .join("\n")
            .into_bytes(),
            2,
            false,
        ),
        (
            "correct JSON with invalid semantic parent",
            format!("{}\n{semantic_line}\n", lines[0]).into_bytes(),
            2,
            true,
        ),
    ];

    for (name, bytes, line, semantic_corruption) in cases {
        std::fs::write(directory.join("session.jsonl"), bytes)
            .unwrap_or_else(|error| panic!("{name} fixture writes: {error}"));
        let result = JsonlSession::inspect(&directory);
        if semantic_corruption {
            assert!(
                matches!(result, Err(SessionError::Corruption(_))),
                "{name} reaches the pure reducer: {result:?}"
            );
        } else {
            assert!(
                matches!(&result, Err(SessionError::Format { line: observed, .. }) if *observed == line),
                "{name} is rejected at line {line}: {result:?}"
            );
            if name == "record deletion" {
                assert!(
                    matches!(
                        &result,
                        Err(SessionError::Format {
                            sequence: Some(sequence),
                            mutation_kind: Some(kind),
                            message,
                            ..
                        }) if *sequence == Sequence(2) && kind == "entry" && message.contains("non-consecutive sequence")
                    ),
                    "the decoded sequence and mutation kind remain available for a transition error: {result:?}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::remove_dir_all(&foreign_directory);
}

#[test]
fn jsonl_rejects_canonical_unknown_header_fields() {
    let directory = temporary_session_directory("unknown-header-field");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("unknown-header-field").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    drop(session);
    let path = directory.join("session.jsonl");
    let source = String::from_utf8(std::fs::read(&path).expect("session file reads"))
        .expect("session file is UTF-8");
    let modified = source.replacen("\"version\":1", "\"unexpected\":null,\"version\":1", 1);
    assert_ne!(source, modified, "fixture adds a canonical extra field");
    std::fs::write(&path, modified).expect("modified header writes");

    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::Format { line: 1, ref message, .. })
            if message == "unknown or missing header fields"
    ));
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_noncanonical_and_invalid_v1_wire_forms_at_their_first_line() {
    let header =
        String::from_utf8(include_bytes!("../fixtures/wire/v1-header.golden.jsonl").to_vec())
            .expect("golden header is UTF-8");
    let user_session =
        String::from_utf8(include_bytes!("../fixtures/wire/v1-user-message.golden.jsonl").to_vec())
            .expect("golden user session is UTF-8");
    let unknown_variant = user_session.replacen(r#""kind":"entry""#, r#""kind":"future""#, 1);
    let cases = vec![
        (
            "duplicate key",
            header
                .replace(
                    r#""kind":"session","metadata"#,
                    r#""kind":"session","kind":"session","metadata"#,
                )
                .into_bytes(),
            1,
        ),
        (
            "reordered fields",
            header
                .replace(
                    r#""initial_lane":"main","kind":"session""#,
                    r#""kind":"session","initial_lane":"main""#,
                )
                .into_bytes(),
            1,
        ),
        ("whitespace", header.replacen('{', "{ ", 1).into_bytes(), 1),
        (
            "alternate numeric spelling",
            header
                .replace(r#""version":1"#, r#""version":1.0"#)
                .into_bytes(),
            1,
        ),
        (
            "unknown field",
            header
                .replace(r#""metadata":{}"#, r#""metadata":{},"unknown":null"#)
                .into_bytes(),
            1,
        ),
        (
            "missing field",
            header
                .replace(r#","workspace":"fixture-workspace""#, "")
                .into_bytes(),
            1,
        ),
        (
            "invalid ID",
            header
                .replace("fixture-session", "invalid session ID")
                .into_bytes(),
            1,
        ),
        (
            "invalid digest spelling",
            header
                .replace(
                    "df2cdbd7aa4eb3c4be5f4fedcd7fffe8632adf8770a38960ed633860f7ab1ad6",
                    "z".repeat(64).as_str(),
                )
                .into_bytes(),
            1,
        ),
        (
            "trailing data",
            format!("{}x\n", header.trim_end_matches('\n')).into_bytes(),
            1,
        ),
        ("CRLF", header.replace('\n', "\r\n").into_bytes(), 1),
        ("unknown variant", unknown_variant.into_bytes(), 2),
        (
            "invalid UTF-8",
            {
                let mut bytes = header.clone().into_bytes();
                bytes[0] = 0xff;
                bytes
            },
            1,
        ),
    ];

    for (index, (name, bytes, line)) in cases.into_iter().enumerate() {
        let directory = temporary_session_directory(&format!("wire-rejection-{index}"));
        let session = JsonlSession::create(
            &directory,
            SessionHeader::new(
                SessionId::new(format!("wire-rejection-{index}")).expect("valid session ID"),
                "workspace-test",
                Metadata::new(),
            ),
            DurabilityMode::Strict,
        )
        .expect("session layout creates");
        drop(session);
        std::fs::write(directory.join("session.jsonl"), bytes)
            .unwrap_or_else(|error| panic!("{name} fixture writes: {error}"));

        let result = JsonlSession::open(&directory, DurabilityMode::Strict);
        assert!(
            matches!(result, Err(SessionError::Format { line: observed, .. }) if observed == line),
            "{name} is rejected at its first invalid line: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}

#[test]
fn head_cache_changes_only_after_a_committed_harness_revision_selection() {
    let directory = temporary_session_directory("head-prefix-digest");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("head-prefix-digest").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let before_user_append = std::fs::read(directory.join("HEAD")).expect("initial HEAD reads");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("head-prefix-entry").expect("valid entry ID"),
                "cache identity",
            ),
        )
        .expect("entry commits");
    assert_eq!(
        std::fs::read(directory.join("HEAD")).expect("HEAD reads"),
        before_user_append,
        "an ordinary semantic append does not replace the active-harness cache"
    );
    let revision = HarnessRevisionId::new("head-prefix-revision").expect("revision ID is valid");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("head-prefix-revision-entry").expect("entry ID is valid"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.clone(),
                    snapshot_id: HarnessSnapshotId::new("head-prefix-snapshot")
                        .expect("snapshot ID is valid"),
                    rollback_from: None,
                }),
            },
        )
        .expect("harness revision commits");
    let snapshot = session.snapshot().expect("snapshot succeeds");
    let head = JsonValue::parse(
        std::str::from_utf8(&std::fs::read(directory.join("HEAD")).expect("HEAD reads"))
            .expect("HEAD is UTF-8"),
    )
    .expect("HEAD is JSON");
    let fields = head.as_object().expect("HEAD is an object");
    assert_eq!(
        fields.get("session_id").and_then(JsonValue::as_str),
        Some(snapshot.header().session_id.as_str())
    );
    assert_eq!(
        fields.get("header_digest").and_then(JsonValue::as_str),
        Some(snapshot.header().digest.to_hex().as_str())
    );
    assert_eq!(
        fields
            .get("active_harness_revision")
            .and_then(JsonValue::as_str),
        Some(revision.as_str())
    );
    let after_revision_selection = std::fs::read(directory.join("HEAD")).expect("HEAD reads");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("head-prefix-post-revision-entry").expect("entry ID is valid"),
                "ordinary append after selection",
            ),
        )
        .expect("entry commits");
    assert_eq!(
        std::fs::read(directory.join("HEAD")).expect("HEAD reads"),
        after_revision_selection,
        "only the durable revision-selection transition refreshes HEAD"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn opening_a_validated_session_rebuilds_a_malformed_head_cache() {
    let directory = temporary_session_directory("head-cache-rebuild");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("head-cache-rebuild").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("head-cache-rebuild-entry").expect("valid entry ID"),
                "cache repair",
            ),
        )
        .expect("entry commits");
    drop(session);
    std::fs::write(directory.join("HEAD"), "not a cache\n").expect("malformed cache writes");

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict).expect("session opens");
    let snapshot = reopened.snapshot().expect("snapshot succeeds");
    assert_eq!(reopened.cache_warning(), None, "cache rebuild succeeded");
    let head = JsonValue::parse(
        std::str::from_utf8(&std::fs::read(directory.join("HEAD")).expect("HEAD reads"))
            .expect("HEAD is UTF-8"),
    )
    .expect("HEAD is rebuilt JSON");
    assert_eq!(
        head.as_object()
            .and_then(|fields| fields.get("header_digest"))
            .and_then(JsonValue::as_str),
        Some(snapshot.header().digest.to_hex().as_str())
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(unix)]
#[test]
fn opening_a_validated_session_retains_an_exact_head_cache() {
    use std::os::unix::fs::MetadataExt as _;

    let directory = temporary_session_directory("head-cache-retain");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("head-cache-retain").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("head-cache-retain-entry").expect("entry ID is valid"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: HarnessRevisionId::new("head-cache-retain-revision")
                        .expect("revision ID is valid"),
                    snapshot_id: HarnessSnapshotId::new("head-cache-retain-snapshot")
                        .expect("snapshot ID is valid"),
                    rollback_from: None,
                }),
            },
        )
        .expect("revision selection commits");
    drop(session);

    let head_path = directory.join("HEAD");
    let before = std::fs::read(&head_path).expect("HEAD reads");
    let before_inode = std::fs::metadata(&head_path)
        .expect("HEAD metadata reads")
        .ino();
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("session with exact cache opens");
    drop(reopened);
    assert_eq!(std::fs::read(&head_path).expect("HEAD rereads"), before);
    assert_eq!(
        std::fs::metadata(&head_path)
            .expect("HEAD metadata rereads")
            .ino(),
        before_inode,
        "opening retains an exact active-harness cache instead of replacing it"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn opening_a_validated_session_replaces_every_untrusted_head_cache_variant() {
    let directory = temporary_session_directory("head-cache-variants");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("head-cache-variants").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("head-cache-variants-entry").expect("valid entry ID"),
                "derived state is disposable",
            ),
        )
        .expect("entry commits");
    let expected = session.snapshot().expect("snapshot succeeds");
    drop(session);

    let head_path = directory.join("HEAD");
    let cases = [
        ("missing", None),
        ("empty", Some(String::new())),
        ("truncated", Some("{\"through_seq\":".into())),
        (
            "foreign prefix",
            Some(format!(
                r#"{{"active_harness_revision":null,"through_digest":"{}","through_seq":999,"version":1}}"#,
                "0".repeat(64),
            )),
        ),
        (
            "future cache schema",
            Some(format!(
                r#"{{"active_harness_revision":null,"through_digest":"{}","through_seq":0,"version":2}}"#,
                "0".repeat(64),
            )),
        ),
    ];

    for (name, contents) in cases {
        match contents {
            Some(contents) => std::fs::write(&head_path, contents)
                .unwrap_or_else(|error| panic!("{name} HEAD writes: {error}")),
            None => match std::fs::remove_file(&head_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("{name} HEAD removes: {error}"),
            },
        }

        let reopened =
            JsonlSession::open(&directory, DurabilityMode::Strict).unwrap_or_else(|error| {
                panic!("{name} cache does not affect authoritative reopen: {error}")
            });
        let actual = reopened.snapshot().expect("snapshot succeeds");
        assert_eq!(
            actual.last_sequence(),
            expected.last_sequence(),
            "{name} cache"
        );
        assert_eq!(actual.last_digest(), expected.last_digest(), "{name} cache");
        assert_eq!(reopened.cache_warning(), None, "{name} cache is replaced");
        drop(reopened);

        let rebuilt = JsonValue::parse(
            std::str::from_utf8(&std::fs::read(&head_path).expect("rebuilt HEAD reads"))
                .expect("rebuilt HEAD is UTF-8"),
        )
        .expect("rebuilt HEAD is JSON");
        assert_eq!(
            rebuilt
                .as_object()
                .and_then(|fields| fields.get("header_digest"))
                .and_then(JsonValue::as_str),
            Some(expected.header().digest.to_hex().as_str()),
            "{name} cache is replaced from the authoritative header"
        );
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn head_cache_failure_does_not_rollback_a_committed_mutation() {
    let directory = temporary_session_directory("head-cache-failure");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("head-cache-failure").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Development,
    )
    .expect("session creates");
    std::fs::remove_file(directory.join("HEAD")).expect("initial HEAD removes");
    std::fs::create_dir(directory.join("HEAD")).expect("HEAD failure fixture creates");

    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("head-cache-failure-entry").expect("entry ID is valid"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: HarnessRevisionId::new("head-cache-failure-revision")
                        .expect("revision ID is valid"),
                    snapshot_id: HarnessSnapshotId::new("head-cache-failure-snapshot")
                        .expect("snapshot ID is valid"),
                    rollback_from: None,
                }),
            },
        )
        .expect("authoritative revision selection succeeds despite cache failure");
    assert!(session.cache_warning().is_some());
    assert_eq!(
        session
            .snapshot()
            .expect("snapshot succeeds")
            .last_sequence(),
        Sequence(1)
    );
    drop(session);

    std::fs::remove_dir(directory.join("HEAD")).expect("failure fixture clears");
    let reopened = JsonlSession::open(&directory, DurabilityMode::Development)
        .expect("authoritative session reopens");
    assert_eq!(
        reopened
            .snapshot()
            .expect("snapshot succeeds")
            .last_sequence(),
        Sequence(1)
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(unix)]
#[test]
fn head_cache_refresh_does_not_follow_a_planted_temporary_symlink() {
    use std::os::unix::fs::symlink;

    let directory = temporary_session_directory("head-cache-symlink");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("head-cache-symlink").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    drop(session);
    let outside = temporary_session_directory("head-cache-symlink-target");
    std::fs::write(&outside, "preserve this file").expect("outside file writes");
    symlink(&outside, directory.join(".HEAD.tmp")).expect("temporary symlink plants");
    std::fs::write(directory.join("HEAD"), "stale\n").expect("stale cache writes");

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict).expect("session opens");
    assert_eq!(
        reopened.cache_warning(),
        None,
        "cache refresh succeeds safely"
    );
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside file reads"),
        "preserve this file",
        "a cache refresh never follows the planted temporary symlink"
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn jsonl_rejects_unknown_nested_mutation_fields_even_when_the_digest_still_matches() {
    let directory = temporary_session_directory("unknown-nested-mutation-field");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("unknown-nested-mutation-field").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("unknown-nested-mutation-entry").expect("valid entry ID"),
                "schema boundary",
            ),
        )
        .expect("entry commits");
    drop(session);

    let path = directory.join("session.jsonl");
    let source = String::from_utf8(std::fs::read(&path).expect("session file reads"))
        .expect("session file is UTF-8");
    let modified = source.replacen(
        "\"type\":\"user_message\"",
        "\"type\":\"user_message\",\"unexpected\":null",
        1,
    );
    assert_ne!(source, modified, "fixture adds a canonical nested field");
    std::fs::write(&path, modified).expect("modified session writes");

    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::Format { line: 2, ref message, .. })
            if message == "mutation does not match the v1 schema"
    ));
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_rejects_a_second_live_writer_and_releases_the_lock_after_drop() {
    let directory = temporary_session_directory("writer-lock");
    let header = SessionHeader::new(
        SessionId::new("session-lock").expect("valid session ID"),
        "workspace-test",
        Metadata::new(),
    );
    let first = JsonlSession::create(&directory, header, DurabilityMode::Strict)
        .expect("first writer opens");
    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::WriterBusy { .. })
    ));
    drop(first);
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("writer lock is released when first process handle closes");
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_drop_unlocks_before_an_inherited_descriptor_can_delay_close() {
    let directory = temporary_session_directory("writer-lock-inherited-descriptor");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("session-lock-inherited-descriptor").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("writer opens");
    let inherited = session.duplicate_writer_descriptor_for_test();

    drop(session);
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("dropping the owner explicitly unlocks before inherited descriptors close");
    drop(reopened);
    drop(inherited);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_writer_lock_releases_after_child_process_termination() {
    const CHILD_DIRECTORY: &str = "TEA_SESSION_LOCK_CHILD_DIRECTORY";
    const READY_MARKER: &str = "tea-session-lock-child-ready";

    if let Ok(directory) = std::env::var(CHILD_DIRECTORY) {
        let _session = JsonlSession::open(directory, DurabilityMode::Strict)
            .expect("child owns the session writer lock");
        println!("{READY_MARKER}");
        use std::io::Write as _;
        std::io::stdout().flush().expect("child readiness flushes");
        let mut byte = [0_u8; 1];
        use std::io::Read as _;
        let _ = std::io::stdin().read(&mut byte);
        return;
    }

    let directory = temporary_session_directory("writer-lock-child");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("session-lock-child").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates before child starts");
    drop(session);

    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "tests::jsonl_writer_lock_releases_after_child_process_termination",
            "--nocapture",
        ])
        .env(CHILD_DIRECTORY, &directory)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("lock-holding child starts");
    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut output = std::io::BufReader::new(stdout);
    let mut ready = String::new();
    loop {
        let mut line = String::new();
        let read =
            std::io::BufRead::read_line(&mut output, &mut line).expect("child readiness reads");
        assert_ne!(read, 0, "lock-holding child exited before becoming ready");
        ready.push_str(&line);
        if ready.contains(READY_MARKER) {
            break;
        }
    }

    assert!(matches!(
        JsonlSession::open(&directory, DurabilityMode::Strict),
        Err(SessionError::WriterBusy { .. })
    ));
    child.kill().expect("lock-holding child terminates");
    child.wait().expect("child termination is observed");
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("operating system releases a terminated child's lock");
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn rejected_jsonl_mutation_does_not_poison_the_preceding_durable_prefix() {
    let directory = temporary_session_directory("invalid-record");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("session-invalid-record").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creation succeeds");
    let invalid = LaneRecord::tool_started(ToolStartedRecord::new(
        RecordId::new("orphan-tool").expect("valid record ID"),
        OperationId::new("missing-operation").expect("valid operation ID"),
        EpochId::new("missing-epoch").expect("valid epoch ID"),
        EntryId::new("missing-assistant").expect("valid entry ID"),
        0,
        "call-1",
        "write",
        JsonValue::Null,
        EntryId::new("missing-result").expect("valid entry ID"),
        ToolReplayPolicy::Never,
        Digest::from_bytes(b"tool"),
        HarnessRevisionId::new("revision-invalid").expect("valid revision ID"),
        "idempotency",
    ));
    assert!(matches!(
        session.append_record(invalid),
        Err(SessionError::Corruption(_))
    ));
    drop(session);

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("invalid uncommitted mutation was never written");
    let snapshot = reopened.snapshot().expect("snapshot succeeds");
    assert!(snapshot.records().is_empty());
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn artifact_stores_deduplicate_exact_bytes_and_return_bounded_direct_pages() {
    let store = MemoryArtifactStore::default();
    let first = store
        .put("one two one".as_bytes(), "text/plain")
        .expect("first object persists");
    let second = store
        .put("one two one".as_bytes(), "text/plain")
        .expect("duplicate object persists idempotently");
    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(
        store
            .read_page(first.artifact_id, 4, 3)
            .expect("page reads")
            .bytes,
        b"two"
    );
    assert_eq!(
        store
            .search_literal(first.artifact_id, b"one", 10, 0)
            .expect("literal search succeeds")
            .iter()
            .map(|found| found.offset)
            .collect::<Vec<_>>(),
        vec![0, 8]
    );
}

#[test]
fn file_artifact_store_streams_and_hashes_large_input_before_publication() {
    let root = temporary_session_directory("streamed-artifact-store");
    let store = FileArtifactStore::open(&root).expect("file store opens");
    let mut source = (0..(2 * 1_048_576 + 17))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    source[65_533..65_539].copy_from_slice(b"needle");
    let mut input = std::io::Cursor::new(source.clone());

    let descriptor = store
        .put_reader(&mut input, "application/octet-stream")
        .expect("streamed object persists");
    assert_eq!(descriptor.artifact_id, ArtifactId::from_bytes(&source));
    assert_eq!(descriptor.byte_len, source.len() as u64);
    assert_eq!(
        store
            .read_page(descriptor.artifact_id, 1_048_576, 17)
            .expect("bounded page reads")
            .bytes,
        source[1_048_576..1_048_593],
    );
    assert_eq!(
        store
            .search_literal(descriptor.artifact_id, b"needle", 1, 3)
            .expect("cross-chunk search succeeds")
            .iter()
            .map(|found| found.offset)
            .collect::<Vec<_>>(),
        vec![65_533],
        "the streaming search retains enough overlap for a boundary-spanning match"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn interrupted_artifact_stream_never_publishes_or_leaves_a_temporary_object() {
    struct InterruptedReader {
        bytes: Vec<u8>,
        cursor: usize,
        fail_after: usize,
    }

    impl std::io::Read for InterruptedReader {
        fn read(&mut self, destination: &mut [u8]) -> std::io::Result<usize> {
            if self.cursor >= self.fail_after {
                return Err(std::io::Error::other(
                    "injected artifact stream interruption",
                ));
            }
            let available = self.fail_after.saturating_sub(self.cursor);
            let count = destination
                .len()
                .min(available)
                .min(self.bytes.len().saturating_sub(self.cursor));
            destination[..count].copy_from_slice(&self.bytes[self.cursor..self.cursor + count]);
            self.cursor = self.cursor.saturating_add(count);
            Ok(count)
        }
    }

    let root = temporary_session_directory("interrupted-artifact-stream");
    let store = FileArtifactStore::open(&root).expect("artifact store opens");
    let bytes = b"interrupted artifact bytes must never become immutable".to_vec();
    let artifact_id = ArtifactId::from_bytes(&bytes);
    let mut reader = InterruptedReader {
        bytes,
        cursor: 0,
        fail_after: 11,
    };

    assert!(matches!(
        store.put_reader(&mut reader, "text/plain"),
        Err(ArtifactError::Io { .. })
    ));
    assert!(matches!(
        store.get(artifact_id),
        Err(ArtifactError::NotFound { .. })
    ));
    assert!(
        std::fs::read_dir(root.join("blake3"))
            .expect("object root reads")
            .next()
            .is_none(),
        "a failed stream removes its private temporary instead of exposing a partial object"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn injected_artifact_publication_failures_leave_only_valid_orphan_objects() {
    use crate::artifact::{TestArtifactWriteFailpoint, install_test_artifact_write_failpoint};

    let bytes = b"artifact publication failure matrix";
    let artifact_id = ArtifactId::from_bytes(bytes);
    for (index, (failpoint, publication_may_have_happened)) in [
        (TestArtifactWriteFailpoint::BeforeTemporaryCreation, false),
        (TestArtifactWriteFailpoint::BeforeFileSync, false),
        (TestArtifactWriteFailpoint::AfterFileSync, false),
        (TestArtifactWriteFailpoint::BeforePublication, false),
        (TestArtifactWriteFailpoint::AfterPublication, true),
        (TestArtifactWriteFailpoint::BeforeDirectorySync, true),
        (TestArtifactWriteFailpoint::AfterDirectorySync, true),
    ]
    .into_iter()
    .enumerate()
    {
        let root = temporary_session_directory(&format!("artifact-failpoint-{index}"));
        let store = FileArtifactStore::open(&root).expect("artifact store opens");
        let failpoint_guard = install_test_artifact_write_failpoint(failpoint);
        assert!(
            store.put(bytes, "text/plain").is_err(),
            "{failpoint:?} interrupts publication"
        );
        drop(failpoint_guard);

        let temporary_entries = std::fs::read_dir(root.join("blake3"))
            .expect("object root reads")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".artifact.tmp")
            })
            .count();
        assert_eq!(
            temporary_entries, 0,
            "{failpoint:?} cleans up the private temporary file"
        );
        if publication_may_have_happened {
            assert_eq!(
                store
                    .get(artifact_id)
                    .expect("published orphan remains exact"),
                bytes,
                "{failpoint:?} can leave only a valid immutable orphan"
            );
        } else {
            assert!(matches!(
                store.get(artifact_id),
                Err(ArtifactError::NotFound { .. })
            ));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn artifact_publication_precedes_a_reference_and_a_failed_reference_leaves_an_orphan() {
    use crate::jsonl::{TestWriteFailpoint, install_test_write_failpoint};

    let directory = temporary_session_directory("artifact-before-reference");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("artifact-before-reference").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let store = session.artifact_store().expect("artifact store opens");
    let artifact = store
        .put(b"already immutable evidence", "text/plain")
        .expect("artifact publishes before a session reference");
    let entry = ProvisionedEntry {
        id: EntryId::new("artifact-before-reference-entry").expect("valid entry ID"),
        body: SessionEntry::Custom(CustomEntry {
            type_name: "trusted.artifact-ordering".into(),
            payload: PayloadRef::Artifact {
                artifact_id: artifact.artifact_id,
                byte_len: artifact.byte_len,
                media_type: artifact.media_type.clone(),
            },
            model_visible: false,
        }),
    };
    let failpoint_guard = install_test_write_failpoint(TestWriteFailpoint::BeforeAppend);
    assert!(matches!(
        session.append_entry(&LaneId::main(), entry.clone()),
        Err(SessionError::Io { .. })
    ));
    drop(failpoint_guard);
    drop(session);

    let mut reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("pre-write failure leaves the prior prefix reopenable");
    assert!(
        reopened
            .snapshot()
            .expect("snapshot succeeds")
            .entries()
            .is_empty()
    );
    let store = reopened.artifact_store().expect("artifact store reopens");
    assert_eq!(
        store
            .get(artifact.artifact_id)
            .expect("published but unreferenced evidence remains an orphan"),
        b"already immutable evidence"
    );
    reopened
        .append_entry(&LaneId::main(), entry)
        .expect("the reopened writer can commit the pre-published artifact reference");
    verify_session(
        &reopened.snapshot().expect("referenced snapshot succeeds"),
        &store,
        std::iter::empty(),
    )
    .expect("a committed artifact reference resolves and verifies");
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn artifact_publication_rejects_conflicting_existing_bytes() {
    let root = temporary_session_directory("artifact-publication-conflict");
    let store = FileArtifactStore::open(&root).expect("artifact store opens");
    let expected = b"expected immutable artifact bytes";
    let artifact_id = ArtifactId::from_bytes(expected);
    let digest = artifact_id.to_hex();
    let destination = root.join("blake3").join(&digest[..2]).join(&digest);
    std::fs::create_dir(destination.parent().expect("object bucket parent"))
        .expect("object bucket creates");
    std::fs::write(&destination, b"conflicting bytes").expect("conflicting object writes");

    assert!(matches!(
        store.put(expected, "text/plain"),
        Err(ArtifactError::Corruption {
            artifact_id: observed,
            ..
        }) if observed == artifact_id
    ));
    assert_eq!(
        std::fs::read(&destination).expect("conflicting object remains readable"),
        b"conflicting bytes",
        "failed content-addressed publication never overwrites existing bytes"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn artifact_gc_keeps_session_roots_and_removes_only_reviewed_unreferenced_objects() {
    let store = MemoryArtifactStore::default();
    let retained = store
        .put(b"reachable durable evidence", "text/plain")
        .expect("reachable object persists");
    let abandoned = store
        .put(b"abandoned temporary evidence", "text/plain")
        .expect("unreferenced object persists");
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("gc-session").expect("session ID"),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("gc-root-entry").expect("entry ID"),
                body: SessionEntry::Custom(CustomEntry {
                    type_name: "trusted.gc-root".into(),
                    payload: PayloadRef::Artifact {
                        artifact_id: retained.artifact_id,
                        byte_len: retained.byte_len,
                        media_type: retained.media_type.clone(),
                    },
                    model_visible: false,
                }),
            },
        )
        .expect("artifact root entry persists");
    let snapshot = session.snapshot().expect("snapshot");
    let plan = plan_artifact_gc(
        &store,
        &snapshot,
        std::iter::empty(),
        ArtifactQuota {
            maximum_objects: Some(1),
            maximum_bytes: None,
        },
    )
    .expect("GC plan derives from immutable roots");
    assert_eq!(plan.reachable, [retained.artifact_id].into_iter().collect());
    assert_eq!(plan.unreferenced.len(), 1);
    assert_eq!(plan.unreferenced[0].artifact_id, abandoned.artifact_id);
    assert!(!plan.quota_status.is_within_limit());

    let report = apply_artifact_gc(
        &store,
        &plan,
        ArtifactQuota {
            maximum_objects: Some(1),
            maximum_bytes: None,
        },
    )
    .expect("reviewed plan removes only unreferenced object");
    assert_eq!(report.removed, plan.unreferenced);
    assert!(report.quota_status.is_within_limit());
    assert_eq!(
        store
            .get(retained.artifact_id)
            .expect("reachable root survives"),
        b"reachable durable evidence"
    );
    assert!(matches!(
        store.get(abandoned.artifact_id),
        Err(ArtifactError::NotFound { .. })
    ));
}

#[test]
fn jsonl_session_gc_runs_under_the_held_writer_lock() {
    let directory = temporary_session_directory("session-scoped-gc");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("session-scoped-gc").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let store = session.artifact_store().expect("object store opens");
    let orphan = store
        .put(b"unreferenced object", "text/plain")
        .expect("orphan persists");

    let report = session
        .collect_unreferenced_artifacts(std::iter::empty(), ArtifactQuota::default())
        .expect("session-scoped GC succeeds");
    assert_eq!(
        report
            .removed
            .iter()
            .map(|item| item.artifact_id)
            .collect::<Vec<_>>(),
        vec![orphan.artifact_id]
    );
    assert!(matches!(
        store.get(orphan.artifact_id),
        Err(ArtifactError::NotFound { .. })
    ));
    drop(session);
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(unix)]
#[test]
fn artifact_gc_inventory_rejects_a_symlinked_object_bucket_without_traversing_it() {
    use std::os::unix::fs::symlink;

    let root = temporary_session_directory("artifact-gc-symlink");
    let outside = temporary_session_directory("artifact-gc-symlink-outside");
    std::fs::create_dir(&outside).expect("outside directory creates");
    let store = FileArtifactStore::open(&root).expect("object store opens");
    symlink(&outside, root.join("blake3").join("aa")).expect("symlinked bucket plants");

    assert!(matches!(
        store.inventory(),
        Err(ArtifactError::UnsafePath { .. })
    ));
    assert!(
        outside.exists(),
        "inventory rejection does not traverse or mutate the symlink target"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn artifact_inventory_ignores_a_crashed_private_publication_temporary() {
    let root = temporary_session_directory("artifact-inventory-temporary");
    let store = FileArtifactStore::open(&root).expect("object store opens");
    let object_root = root.join("blake3");
    std::fs::write(
        object_root.join(format!(".{}-{:016x}.artifact.tmp", std::process::id(), 1)),
        b"partial unpublished artifact bytes",
    )
    .expect("crashed private temporary writes");

    assert!(
        store
            .inventory()
            .expect("ephemeral temporary is not an object")
            .is_empty(),
        "object inventory ignores only the known unpublished temporary namespace"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn artifact_inventory_rejects_an_unrecognized_temporary_looking_root_entry() {
    let root = temporary_session_directory("artifact-inventory-unrecognized-temporary");
    let store = FileArtifactStore::open(&root).expect("object store opens");
    std::fs::write(
        root.join("blake3").join(".-0000000000000001.artifact.tmp"),
        b"unexpected root entry",
    )
    .expect("unexpected root entry writes");

    assert!(matches!(
        store.inventory(),
        Err(ArtifactError::UnsafePath { .. })
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn canonical_hashes_are_domain_separated_and_length_delimited() {
    let mut first = CanonicalHashWriter::new("tea-artifact-v1", 1, 1);
    first.string("left", "ab");
    first.string("right", "c");
    let mut second = CanonicalHashWriter::new("tea-artifact-v1", 1, 1);
    second.string("left", "a");
    second.string("right", "bc");
    let mut different_domain = CanonicalHashWriter::new("tea-harness-tree-v1", 1, 1);
    different_domain.string("left", "ab");
    different_domain.string("right", "c");
    assert_ne!(first.finish(), second.finish());
    let mut same = CanonicalHashWriter::new("tea-artifact-v1", 1, 1);
    same.string("left", "ab");
    same.string("right", "c");
    assert_ne!(same.finish(), different_domain.finish());
    assert!(NormalizedPath::new("plugins/session.verify/main.luau").is_ok());
    assert!(NormalizedPath::new("../escape.luau").is_err());
}

#[test]
fn jsonl_reopen_retains_redacted_opaque_provider_context_with_its_assistant_turn() {
    let directory = temporary_session_directory("opaque-provider-context");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("opaque-provider-context").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("opaque-provider-context-assistant")
                    .expect("valid assistant entry ID"),
                body: SessionEntry::AssistantMessage(AssistantMessageEntry {
                    content: "visible assistant answer".into(),
                    tool_calls: Vec::new(),
                    stop_reason: Some("stop".into()),
                    error_message: None,
                    opaque_context: vec![OpaqueProviderContextEntry {
                        provider: "codex".into(),
                        kind: "reasoning".into(),
                        item_id: Some("rs_1".into()),
                        payload: "encrypted-provider-state".into(),
                    }],
                    metadata: Metadata::new(),
                }),
            },
        )
        .expect("assistant entry commits");
    drop(session);

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("session with opaque provider state reopens");
    let snapshot = reopened.snapshot().expect("snapshot");
    let assistant = match &snapshot.entries()[0].body {
        SessionEntry::AssistantMessage(entry) => entry,
        other => panic!("expected assistant entry, got {other:?}"),
    };
    assert_eq!(assistant.opaque_context.len(), 1);
    assert_eq!(assistant.opaque_context[0].provider, "codex");
    assert_eq!(assistant.opaque_context[0].kind, "reasoning");
    assert_eq!(assistant.opaque_context[0].item_id.as_deref(), Some("rs_1"));
    assert_eq!(
        assistant.opaque_context[0].payload,
        "encrypted-provider-state"
    );
    assert!(!format!("{:?}", assistant.opaque_context[0]).contains("encrypted-provider-state"));
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

fn temporary_session_directory(label: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tea-session-{label}-{}-{sequence:016x}.tea",
        std::process::id()
    ))
}

fn assert_active_workspace_lease_export_is_refused(session: &mut JsonlSession, state: &str) {
    let destination = temporary_session_directory(&format!("agent-graph-active-{state}-export"));
    let result = session.export_to(&destination, std::iter::empty());
    let _ = std::fs::remove_dir_all(&destination);
    assert!(
        matches!(
            result,
            Err(SessionExportError::Session(
                SessionError::InvalidInput { .. }
            ))
        ),
        "export must refuse a session with an unresolved {state} workspace lease: {result:?}"
    );
}

fn assert_reopen_fixed_point(
    session: JsonlSession,
    directory: &std::path::Path,
    lane: &LaneId,
) -> JsonlSession {
    let live_snapshot = session.snapshot().expect("live snapshot succeeds");
    let live_reduction =
        reduce_lane(live_snapshot.clone(), lane.clone()).expect("live prefix reduces");
    drop(session);

    let reopened = JsonlSession::open(directory, DurabilityMode::Strict)
        .expect("fresh writer reopens the committed prefix");
    let replayed_snapshot = reopened.snapshot().expect("replayed snapshot succeeds");
    assert_eq!(replayed_snapshot, live_snapshot);
    assert_eq!(
        reduce_lane(replayed_snapshot, lane.clone()).expect("replayed prefix reduces"),
        live_reduction,
        "the pure reducer has the same fixed point after fresh disk replay"
    );
    reopened
}

fn append_parent_chain<S: SessionWriter>(
    session: &mut S,
) -> Result<Vec<StoredEntry>, SessionError> {
    let lane = LaneId::main();
    let first = session.append_entry(
        &lane,
        ProvisionedEntry::user(
            EntryId::new("conformance-one").expect("valid entry ID"),
            "one",
        ),
    )?;
    let second = session.append_entry(
        &lane,
        ProvisionedEntry::user(
            EntryId::new("conformance-two").expect("valid entry ID"),
            "two",
        ),
    )?;
    Ok(vec![first, second])
}

#[test]
fn usage_totals_add_decimal_costs_without_filling_unknown_fields() {
    let mut total = Usage {
        input_tokens: Some(2),
        cost: Some("0.20".into()),
        ..Usage::default()
    };
    total.saturating_add_assign(&Usage {
        input_tokens: Some(3),
        output_tokens: Some(4),
        cost: Some("0.005".into()),
        ..Usage::default()
    });

    assert_eq!(total.input_tokens, Some(5));
    assert_eq!(total.output_tokens, Some(4));
    assert_eq!(total.reasoning_tokens, None);
    assert_eq!(total.cost.as_deref(), Some("0.205"));
}

#[test]
fn subagent_ids_are_deterministic_domain_separated_and_form_the_child_lane() {
    let session_id = SessionId::new("subagent-id-session").expect("valid session ID");
    let parent_lane = LaneId::main();
    let parent_operation =
        OperationId::new("subagent-id-parent-operation").expect("valid parent operation ID");
    let agent = AgentId::derive(
        &session_id,
        &parent_lane,
        &parent_operation,
        "spawn-idempotency-key",
    );

    assert_eq!(
        agent,
        AgentId::derive(
            &session_id,
            &parent_lane,
            &parent_operation,
            "spawn-idempotency-key",
        )
    );
    assert_ne!(
        agent,
        AgentId::derive(
            &session_id,
            &parent_lane,
            &parent_operation,
            "another-spawn-idempotency-key",
        )
    );
    assert_eq!(agent.lane_id().as_str(), agent.as_str());

    let lease = WorkspaceLeaseId::derive(&agent);
    assert_ne!(agent.as_str(), lease.as_str());
    assert_eq!(
        WorkspaceDeltaId::derive(&lease, "base-commit", "result-commit"),
        WorkspaceDeltaId::derive(&lease, "base-commit", "result-commit")
    );
    assert_ne!(
        WorkspaceDeltaId::derive(&lease, "base-commit", "result-commit"),
        WorkspaceDeltaId::derive(&lease, "base-commit", "other-result-commit")
    );
}

#[test]
fn agent_graph_rejects_an_applied_delta_that_was_never_persisted() {
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("agent-graph-corruption").expect("valid session ID"),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creates");
    let delta = WorkspaceDeltaId::new("delta-missing").expect("valid delta ID");

    assert!(matches!(
        session.append_fact(SessionFact::WorkspaceDeltaApplied(
            WorkspaceDeltaAppliedFact {
                delta_id: delta,
                target_lane_id: LaneId::main(),
                tool_call_id: "apply-call".into(),
                changed_paths: vec!["src/lib.rs".into()],
            }
        )),
        Err(SessionError::Corruption(_))
    ));
    assert_eq!(
        session
            .snapshot()
            .expect("rejected fact keeps the previous prefix")
            .last_sequence(),
        Sequence(0)
    );
}

#[test]
fn jsonl_round_trips_a_complete_child_agent_graph_with_an_oversized_artifact_report() {
    let directory = temporary_session_directory("agent-graph-round-trip");
    let session_id = SessionId::new("agent-graph-round-trip").expect("valid session ID");
    let clock: Arc<dyn SessionClock> = Arc::new(FixedSessionClock(1_700_000_000_001));
    let mut session = JsonlSession::create_with_clock(
        &directory,
        SessionHeader::new_at(
            session_id.clone(),
            "workspace-test",
            Metadata::new(),
            1_700_000_000_000,
        ),
        DurabilityMode::Strict,
        clock,
    )
    .expect("session creates");
    let model = SubagentModelRecord {
        provider: "scripted".into(),
        model: "child-model".into(),
        revision: Some("2026-08-23".into()),
        display_name: "Scripted child".into(),
        context_window: Some(128_000),
    };
    session
        .append_fact(SessionFact::SubagentPolicy(SubagentPolicyFact {
            schema_version: 1,
            models: vec![model.clone()],
            max_concurrent: 1,
            max_total_per_operation: 1,
            timeout_ms: 30_000,
            tool_surface_digest: Digest::from_bytes(b"subagent tool schema"),
        }))
        .expect("policy commits");

    let parent_lane = LaneId::main();
    let parent_operation =
        OperationId::new("agent-graph-parent-operation").expect("valid parent operation ID");
    let root_revision =
        HarnessRevisionId::new("agent-graph-root-revision").expect("valid root revision ID");
    let root_profile =
        ModelHarnessProfileId::new("agent-graph-root-profile").expect("valid root profile ID");
    let parent_epoch = EpochId::new("agent-graph-parent-epoch").expect("valid parent epoch ID");
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            parent_operation.clone(),
            parent_lane.clone(),
            None,
            OperationKind::Run,
            Vec::new(),
            root_revision.clone(),
            root_profile.clone(),
        )))
        .expect("parent operation commits");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: parent_epoch.clone(),
            operation_id: parent_operation.clone(),
            epoch_index: 0,
            source_leaf_id: None,
            harness_revision_id: root_revision.clone(),
            harness_snapshot_id: HarnessSnapshotId::new("agent-graph-root-snapshot")
                .expect("valid root snapshot ID"),
            model_harness_profile: root_profile,
            core_run_id: CoreRunId::new("agent-graph-parent-core-run").expect("valid core run ID"),
            epoch_resume_data: std::collections::BTreeMap::new(),
        }))
        .expect("parent epoch commits");
    let parent_assistant =
        EntryId::new("agent-graph-parent-assistant").expect("valid assistant entry ID");
    let spawn_args = JsonValue::Object(std::collections::BTreeMap::from([
        ("context".into(), JsonValue::String("task".into())),
        ("model".into(), JsonValue::String(model.model.clone())),
        (
            "task".into(),
            JsonValue::String("audit the durable session".into()),
        ),
        (
            "task_name".into(),
            JsonValue::String("audit_session".into()),
        ),
        ("thinking".into(), JsonValue::String("high".into())),
    ]));
    session
        .append_entry(
            &parent_lane,
            ProvisionedEntry::assistant(
                parent_assistant.clone(),
                "",
                vec![AssistantToolCall::new(
                    "agent-graph-spawn-call",
                    "spawn_agent",
                    spawn_args.clone(),
                )],
            ),
        )
        .expect("parent assistant commits");
    session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("agent-graph-spawn-record").expect("valid tool record ID"),
            parent_operation.clone(),
            parent_epoch,
            parent_assistant,
            0,
            "agent-graph-spawn-call",
            "spawn_agent",
            spawn_args,
            EntryId::new("agent-graph-spawn-result").expect("valid result entry ID"),
            ToolReplayPolicy::Never,
            Digest::from_bytes(b"spawn definition"),
            root_revision,
            "agent-graph-spawn-key",
        )))
        .expect("parent spawn intent commits");

    let agent = AgentId::derive(
        &session_id,
        &parent_lane,
        &parent_operation,
        "agent-graph-spawn-key",
    );
    let child_lane = agent.lane_id();
    let child_revision =
        HarnessRevisionId::new("agent-graph-child-revision").expect("valid child revision ID");
    let child_snapshot =
        HarnessSnapshotId::new("agent-graph-child-snapshot").expect("valid child snapshot ID");
    let child_profile =
        ModelHarnessProfileId::new("agent-graph-child-profile").expect("valid child profile ID");
    session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: child_lane.clone(),
            base_leaf_id: None,
        })
        .expect("child lane commits");
    session
        .append_entry(
            &child_lane,
            ProvisionedEntry {
                id: EntryId::new("agent-graph-child-model").expect("valid model entry ID"),
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: model.provider.clone(),
                    model: model.model.clone(),
                    revision: model.revision.clone(),
                }),
            },
        )
        .expect("child model commits");
    session
        .append_entry(
            &child_lane,
            ProvisionedEntry {
                id: EntryId::new("agent-graph-child-thinking").expect("valid thinking entry ID"),
                body: SessionEntry::ThinkingChanged(ThinkingChangedEntry {
                    level: "high".into(),
                }),
            },
        )
        .expect("child thinking commits");
    session
        .append_entry(
            &child_lane,
            ProvisionedEntry {
                id: EntryId::new("agent-graph-child-revision-entry")
                    .expect("valid revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: child_revision.clone(),
                    snapshot_id: child_snapshot.clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("child harness commits");
    session
        .append_fact(SessionFact::AgentSpawned(AgentSpawnedFact {
            agent_id: agent.clone(),
            parent_lane_id: parent_lane,
            parent_operation_id: parent_operation,
            lane_id: child_lane.clone(),
            task_name: "audit_session".into(),
            model,
            thinking: "high".into(),
            context_mode: AgentContextMode::Task,
            base_leaf_id: None,
            workspace_lease_id: WorkspaceLeaseId::derive(&agent),
            harness_revision_id: child_revision.clone(),
            harness_snapshot_id: child_snapshot,
            model_harness_profile_id: child_profile.clone(),
            spawn_tool_call_id: "agent-graph-spawn-call".into(),
        }))
        .expect("agent spawn fact commits");
    assert_active_workspace_lease_export_is_refused(&mut session, "spawned");
    let child_operation = derive_subagent_operation_id(&agent, "audit the durable session");
    let assignment_id =
        EntryId::new("agent-graph-child-assignment").expect("valid assignment entry ID");
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            child_operation.clone(),
            child_lane.clone(),
            Some(
                EntryId::new("agent-graph-child-revision-entry").expect("valid revision entry ID"),
            ),
            OperationKind::Subagent {
                agent_id: agent.clone(),
                parent_operation_id: OperationId::new("agent-graph-parent-operation")
                    .expect("valid parent operation ID"),
            },
            vec![ProvisionedEntry::user(
                assignment_id.clone(),
                "audit the durable session",
            )],
            child_revision.clone(),
            child_profile.clone(),
        )))
        .expect("child operation commits");
    session
        .append_entry(
            &child_lane,
            ProvisionedEntry::user(assignment_id, "audit the durable session"),
        )
        .expect("child assignment commits");

    let live = reduce_agent_graph(&session.snapshot().expect("live snapshot"))
        .expect("live agent graph reduces");
    assert!(matches!(
        live.agents.get(&agent).expect("agent is present").state,
        AgentState::Running
    ));
    assert_active_workspace_lease_export_is_refused(&mut session, "running");

    let child_epoch = EpochId::new("agent-graph-child-epoch").expect("valid child epoch ID");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: child_epoch.clone(),
            operation_id: child_operation.clone(),
            epoch_index: 0,
            source_leaf_id: Some(
                EntryId::new("agent-graph-child-assignment").expect("valid assignment entry ID"),
            ),
            harness_revision_id: child_revision.clone(),
            harness_snapshot_id: HarnessSnapshotId::new("agent-graph-child-snapshot")
                .expect("valid child snapshot ID"),
            model_harness_profile: child_profile.clone(),
            core_run_id: CoreRunId::new("agent-graph-child-core-run")
                .expect("valid child core run ID"),
            epoch_resume_data: std::collections::BTreeMap::new(),
        }))
        .expect("child epoch commits");
    let oversized_report = "complete durable child report\n".repeat(2_048);
    assert!(
        oversized_report.len() > 32 * 1024,
        "the retained report must exceed the inline limit"
    );
    let child_final = EntryId::new("agent-graph-child-final").expect("valid final entry ID");
    session
        .append_entry(
            &child_lane,
            ProvisionedEntry::assistant(child_final.clone(), oversized_report.clone(), Vec::new()),
        )
        .expect("child final assistant entry commits");
    session
        .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id: child_epoch,
            operation_id: child_operation.clone(),
            reason: EpochFinishReason::Settled,
        }))
        .expect("child epoch finishes");
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: child_operation.clone(),
            outcome: OperationOutcome::Completed,
        }))
        .expect("child operation finishes");
    assert_active_workspace_lease_export_is_refused(&mut session, "finalizing");
    let patch = session
        .artifact_store()
        .expect("artifact store opens")
        .put(b"diff --git a/src/lib.rs b/src/lib.rs\n", "text/x-diff")
        .expect("patch artifact persists");
    let report = session
        .artifact_store()
        .expect("artifact store opens")
        .put(oversized_report.as_bytes(), "text/plain;charset=utf-8")
        .expect("oversized child report artifact persists");
    let delta_id = WorkspaceDeltaId::derive(
        &WorkspaceLeaseId::derive(&agent),
        "agent-graph-base-commit",
        "agent-graph-result-commit",
    );
    session
        .append_fact(SessionFact::WorkspaceDelta(WorkspaceDeltaFact {
            delta_id: delta_id.clone(),
            agent_id: agent.clone(),
            workspace_lease_id: WorkspaceLeaseId::derive(&agent),
            base_commit: "agent-graph-base-commit".into(),
            result_commit: "agent-graph-result-commit".into(),
            changed_paths: vec!["src/lib.rs".into()],
            patch: PayloadRef::Artifact {
                artifact_id: patch.artifact_id,
                byte_len: patch.byte_len,
                media_type: patch.media_type,
            },
        }))
        .expect("workspace delta fact commits");
    session
        .append_fact(SessionFact::AgentTaskFinished(AgentTaskFinishedFact {
            agent_id: agent.clone(),
            operation_id: child_operation,
            outcome: OperationOutcome::Completed,
            final_entry_id: Some(child_final),
            report: PayloadRef::Artifact {
                artifact_id: report.artifact_id,
                byte_len: report.byte_len,
                media_type: report.media_type,
            },
            workspace_delta_id: Some(delta_id.clone()),
        }))
        .expect("child terminal result commits");
    let apply_assistant =
        EntryId::new("agent-graph-apply-assistant").expect("valid apply assistant entry ID");
    let apply_args = JsonValue::Object(std::collections::BTreeMap::from([(
        "delta_id".into(),
        JsonValue::String(delta_id.to_string()),
    )]));
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                apply_assistant.clone(),
                "",
                vec![AssistantToolCall::new(
                    "agent-graph-apply-call",
                    "apply_agent_changes",
                    apply_args.clone(),
                )],
            ),
        )
        .expect("parent apply assistant commits");
    session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("agent-graph-apply-record").expect("valid apply record ID"),
            OperationId::new("agent-graph-parent-operation").expect("valid parent operation ID"),
            EpochId::new("agent-graph-parent-epoch").expect("valid parent epoch ID"),
            apply_assistant,
            0,
            "agent-graph-apply-call",
            "apply_agent_changes",
            apply_args,
            EntryId::new("agent-graph-apply-result").expect("valid apply result entry ID"),
            ToolReplayPolicy::Never,
            Digest::from_bytes(b"apply definition"),
            HarnessRevisionId::new("agent-graph-root-revision").expect("valid root revision ID"),
            "agent-graph-apply-key",
        )))
        .expect("parent apply intent commits");
    session
        .append_fact(SessionFact::WorkspaceDeltaApplied(
            WorkspaceDeltaAppliedFact {
                delta_id: delta_id.clone(),
                target_lane_id: LaneId::main(),
                tool_call_id: "agent-graph-apply-call".into(),
                changed_paths: vec!["src/lib.rs".into()],
            },
        ))
        .expect("applied delta fact commits");
    let verified = verify_session(
        &session.snapshot().expect("snapshot verifies"),
        &session.artifact_store().expect("artifact store opens"),
        std::iter::empty(),
    )
    .expect("direct delta artifact verifies");
    assert!(verified.artifact_roots.contains(&patch.artifact_id));
    assert!(verified.artifact_roots.contains(&report.artifact_id));
    let gc_plan = plan_artifact_gc(
        &session.artifact_store().expect("artifact store opens"),
        &session.snapshot().expect("snapshot plans GC"),
        std::iter::empty(),
        ArtifactQuota::default(),
    )
    .expect("GC plan derives session-owned roots");
    assert!(gc_plan.reachable.contains(&patch.artifact_id));
    assert!(gc_plan.reachable.contains(&report.artifact_id));
    let export_directory = temporary_session_directory("agent-graph-export");
    let export = session
        .export_to(&export_directory, std::iter::empty())
        .expect("export retains the child patch artifact");
    assert!(
        export
            .verification
            .artifact_roots
            .contains(&patch.artifact_id)
    );
    assert!(
        export
            .verification
            .artifact_roots
            .contains(&report.artifact_id)
    );

    let expected_delta_id = delta_id.clone();
    let live = reduce_agent_graph(&session.snapshot().expect("live final snapshot"))
        .expect("final agent graph reduces");
    assert!(matches!(
        live.agents.get(&agent).expect("agent is present").state,
        AgentState::Applied { ref delta_id, .. } if delta_id == &expected_delta_id
    ));
    drop(session);

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict).expect("session reopens");
    assert_eq!(
        reduce_agent_graph(&reopened.snapshot().expect("replayed snapshot"))
            .expect("replayed agent graph reduces"),
        live
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
    let _ = std::fs::remove_dir_all(&export_directory);
}

#[test]
fn agent_graph_rejects_invalid_policy_models_and_parent_operations() {
    let invalid_policy = SubagentPolicyFact {
        schema_version: 1,
        models: vec![SubagentModelRecord {
            provider: "scripted".into(),
            model: "child".into(),
            revision: None,
            display_name: "Child".into(),
            context_window: Some(16_000),
        }],
        max_concurrent: 1,
        max_total_per_operation: 1,
        timeout_ms: 30_000,
        tool_surface_digest: Digest::from_bytes(b"policy"),
    };
    let mut duplicate_revision = invalid_policy.clone();
    duplicate_revision
        .models
        .push(duplicate_revision.models[0].clone());
    assert!(duplicate_revision.validate().is_err());

    let mut duplicate_model_id = invalid_policy.clone();
    duplicate_model_id.models[0].revision = Some("first-revision".into());
    let mut later_revision = duplicate_model_id.models[0].clone();
    later_revision.revision = Some("later-revision".into());
    duplicate_model_id.models.push(later_revision);
    assert!(
        duplicate_model_id.validate().is_err(),
        "the model-only spawn enum must not select ambiguously across revisions"
    );

    let fixture = spawned_agent_fixture();
    let mut disallowed_model = fixture.spawn.clone();
    disallowed_model.model.revision = Some("not-authorized".into());
    assert_rejected_agent_fact(
        fixture.session,
        disallowed_model,
        "the policy identity includes the descriptor revision",
    );

    let fixture = spawned_agent_fixture();
    let mut unknown_harness = fixture.spawn.clone();
    unknown_harness.harness_revision_id = HarnessRevisionId::new("unknown-child-revision")
        .expect("valid unknown harness revision ID");
    assert_rejected_agent_fact(
        fixture.session,
        unknown_harness,
        "the spawn harness must already be known in the child lane",
    );

    let mut fixture = spawned_agent_fixture();
    let mut missing_parent = fixture.spawn.clone();
    missing_parent.parent_operation_id =
        OperationId::new("missing-parent-operation").expect("valid missing operation ID");
    let fixture_session_id = fixture
        .session
        .snapshot()
        .expect("snapshot")
        .header()
        .session_id
        .clone();
    missing_parent.agent_id = AgentId::derive(
        &fixture_session_id,
        &missing_parent.parent_lane_id,
        &missing_parent.parent_operation_id,
        "missing-parent-key",
    );
    missing_parent.lane_id = missing_parent.agent_id.lane_id();
    missing_parent.workspace_lease_id = WorkspaceLeaseId::derive(&missing_parent.agent_id);
    fixture
        .session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: missing_parent.lane_id.clone(),
            base_leaf_id: None,
        })
        .expect("isolated lane may be prepared before its rejected graph binding");
    append_child_configuration(&mut fixture.session, &missing_parent);
    assert!(matches!(
        fixture
            .session
            .append_fact(SessionFact::AgentSpawned(missing_parent)),
        Err(SessionError::Corruption(_))
    ));
}

#[test]
fn agent_graph_rejects_bijection_task_operation_and_terminal_violations() {
    let mut duplicate = spawned_agent_fixture();
    assert!(matches!(
        duplicate
            .session
            .append_fact(SessionFact::AgentSpawned(duplicate.spawn.clone())),
        Err(SessionError::Corruption(_))
    ));

    let mut task_duplicate = spawned_agent_fixture();
    let duplicate_task_name = task_duplicate.spawn.task_name.clone();
    let second = append_second_spawn(&mut task_duplicate, duplicate_task_name);
    assert!(matches!(
        task_duplicate
            .session
            .append_fact(SessionFact::AgentSpawned(second)),
        Err(SessionError::Corruption(_))
    ));

    let mut lane_duplicate = spawned_agent_fixture();
    let mut second = append_second_spawn(&mut lane_duplicate, "second_task".into());
    second.lane_id = lane_duplicate.child_lane.clone();
    assert!(matches!(
        lane_duplicate
            .session
            .append_fact(SessionFact::AgentSpawned(second)),
        Err(SessionError::Corruption(_))
    ));

    let mut mismatched_spawn_args = spawned_agent_fixture();
    let mut second = append_second_spawn(&mut mismatched_spawn_args, "declared_task".into());
    second.task_name = "different_task".into();
    assert!(
        matches!(
            mismatched_spawn_args
                .session
                .append_fact(SessionFact::AgentSpawned(second)),
            Err(SessionError::Corruption(_))
        ),
        "a spawn fact must remain bound to its durable effective tool arguments"
    );

    let mut wrong_profile = spawned_agent_fixture();
    let operation = derive_subagent_operation_id(&wrong_profile.agent, &wrong_profile.assignment);
    let child_lane = wrong_profile.child_lane.clone();
    assert!(matches!(
        append_child_operation(
            &mut wrong_profile,
            child_lane,
            operation,
            ModelHarnessProfileId::new("wrong-child-profile").expect("valid wrong profile ID"),
        ),
        Err(SessionError::Corruption(_))
    ));
    assert!(
        wrong_profile.session.snapshot().is_ok(),
        "the rejected append must leave the validated prefix readable"
    );

    let mut wrong_assignment = spawned_agent_fixture();
    wrong_assignment.assignment = "audit different durable facts".into();
    let operation =
        derive_subagent_operation_id(&wrong_assignment.agent, &wrong_assignment.assignment);
    let child_lane = wrong_assignment.child_lane.clone();
    let child_profile = wrong_assignment.child_profile.clone();
    assert!(
        matches!(
            append_child_operation(&mut wrong_assignment, child_lane, operation, child_profile,),
            Err(SessionError::Corruption(_))
        ),
        "the child assignment must equal the durable spawn_agent task"
    );

    let mut foreign_operation = spawned_agent_fixture();
    let foreign_result = foreign_operation
        .session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            OperationId::new("foreign-run-on-child-lane").expect("valid operation ID"),
            foreign_operation.child_lane.clone(),
            Some(foreign_operation.child_revision_entry.clone()),
            OperationKind::Run,
            Vec::new(),
            foreign_operation.child_revision.clone(),
            foreign_operation.child_profile.clone(),
        )));
    assert!(
        matches!(foreign_result, Err(SessionError::Corruption(_))),
        "an agent-bound lane cannot hide an unrelated operation kind: {foreign_result:?}"
    );

    let mut wrong_lane = spawned_agent_fixture();
    let wrong_lane_id = LaneId::new("wrong-child-operation-lane").expect("valid lane ID");
    wrong_lane
        .session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: wrong_lane_id.clone(),
            base_leaf_id: None,
        })
        .expect("unbound lane commits");
    let operation = derive_subagent_operation_id(&wrong_lane.agent, &wrong_lane.assignment);
    let child_profile = wrong_lane.child_profile.clone();
    assert!(matches!(
        append_child_operation_with_source(
            &mut wrong_lane,
            wrong_lane_id,
            operation,
            child_profile,
            None,
        ),
        Err(SessionError::Corruption(_))
    ));

    let mut terminal_before_finish = running_agent_fixture();
    assert!(matches!(
        terminal_before_finish
            .session
            .append_fact(SessionFact::AgentTaskFinished(AgentTaskFinishedFact {
                agent_id: terminal_before_finish.agent.clone(),
                operation_id: terminal_before_finish.child_operation.clone(),
                outcome: OperationOutcome::Completed,
                final_entry_id: None,
                report: PayloadRef::Inline(JsonValue::String("unfinished".into())),
                workspace_delta_id: None,
            })),
        Err(SessionError::Corruption(_))
    ));

    let mut wrong_final_entry = running_agent_fixture();
    let child_lane = wrong_final_entry.agent.lane_id();
    let intermediate =
        EntryId::new("agent-intermediate-assistant").expect("valid intermediate assistant ID");
    wrong_final_entry
        .session
        .append_entry(
            &child_lane,
            ProvisionedEntry::assistant(intermediate.clone(), "intermediate", Vec::new()),
        )
        .expect("intermediate assistant commits");
    wrong_final_entry
        .session
        .append_entry(
            &child_lane,
            ProvisionedEntry::assistant(
                EntryId::new("agent-settled-assistant").expect("valid final assistant ID"),
                "settled report",
                Vec::new(),
            ),
        )
        .expect("settled assistant commits");
    wrong_final_entry
        .session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: wrong_final_entry.child_operation.clone(),
            outcome: OperationOutcome::Completed,
        }))
        .expect("child operation settles");
    assert!(
        matches!(
            wrong_final_entry
                .session
                .append_fact(SessionFact::AgentTaskFinished(AgentTaskFinishedFact {
                    agent_id: wrong_final_entry.agent,
                    operation_id: wrong_final_entry.child_operation,
                    outcome: OperationOutcome::Completed,
                    final_entry_id: Some(intermediate),
                    report: PayloadRef::Inline(JsonValue::String("intermediate".into())),
                    workspace_delta_id: None,
                })),
            Err(SessionError::Corruption(_))
        ),
        "the retained report must name the operation's final assistant entry"
    );
}

#[test]
fn agent_graph_rejects_delta_lease_and_path_invariants() {
    let mut completed = running_agent_fixture();
    completed
        .session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: completed.child_operation.clone(),
            outcome: OperationOutcome::Completed,
        }))
        .expect("child operation completes before finalization");
    let foreign_lease =
        WorkspaceLeaseId::new("foreign-workspace-lease").expect("valid foreign lease ID");
    let unknown_agent = AgentId::new("unknown-delta-agent").expect("valid unknown agent ID");
    assert!(matches!(
        completed
            .session
            .append_fact(SessionFact::WorkspaceDelta(workspace_delta_fact(
                &unknown_agent,
                WorkspaceLeaseId::derive(&unknown_agent),
                vec!["src/lib.rs".into()],
            ))),
        Err(SessionError::Corruption(_))
    ));
    assert!(matches!(
        completed
            .session
            .append_fact(SessionFact::WorkspaceDelta(workspace_delta_fact(
                &completed.agent,
                foreign_lease,
                vec!["src/lib.rs".into()],
            ))),
        Err(SessionError::Corruption(_))
    ));

    for paths in [
        vec!["../escape".into()],
        vec!["src/z.rs".into(), "src/a.rs".into()],
        vec!["src/lib.rs".into(), "src/lib.rs".into()],
        vec!["src\\windows.rs".into()],
        vec!["./src/lib.rs".into()],
        vec!["src/\0nul.rs".into()],
    ] {
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("agent-path-rejection").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ))
        .expect("session creates");
        let agent = AgentId::new("agent-path-rejection").expect("valid agent ID");
        let lease = WorkspaceLeaseId::derive(&agent);
        assert!(matches!(
            session.append_fact(SessionFact::WorkspaceDelta(workspace_delta_fact(
                &agent, lease, paths,
            ))),
            Err(SessionError::Corruption(_))
        ));
    }
}

#[test]
fn agent_graph_rejects_apply_intent_for_a_different_delta() {
    let mut fixture = running_agent_fixture();
    fixture
        .session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: fixture.child_operation.clone(),
            outcome: OperationOutcome::Completed,
        }))
        .expect("child operation finishes");
    let delta = workspace_delta_fact(
        &fixture.agent,
        WorkspaceLeaseId::derive(&fixture.agent),
        vec!["src/lib.rs".into()],
    );
    fixture
        .session
        .append_fact(SessionFact::WorkspaceDelta(delta.clone()))
        .expect("delta commits");
    fixture
        .session
        .append_fact(SessionFact::AgentTaskFinished(AgentTaskFinishedFact {
            agent_id: fixture.agent,
            operation_id: fixture.child_operation,
            outcome: OperationOutcome::Completed,
            final_entry_id: None,
            report: PayloadRef::Inline(JsonValue::String(String::new())),
            workspace_delta_id: Some(delta.delta_id.clone()),
        }))
        .expect("terminal fact commits");
    let assistant_id = EntryId::new("mismatched-apply-assistant").expect("assistant ID");
    let wrong_args = JsonValue::Object(std::collections::BTreeMap::from([(
        "delta_id".into(),
        JsonValue::String("different-delta".into()),
    )]));
    fixture
        .session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                "",
                vec![AssistantToolCall::new(
                    "mismatched-apply-call",
                    "apply_agent_changes",
                    wrong_args.clone(),
                )],
            ),
        )
        .expect("apply assistant commits");
    fixture
        .session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("mismatched-apply-record").expect("record ID"),
            OperationId::new("agent-fixture-parent-operation").expect("parent operation ID"),
            EpochId::new("agent-fixture-parent-epoch").expect("parent epoch ID"),
            assistant_id,
            0,
            "mismatched-apply-call",
            "apply_agent_changes",
            wrong_args,
            EntryId::new("mismatched-apply-result").expect("result ID"),
            ToolReplayPolicy::Never,
            Digest::from_bytes(b"apply definition"),
            HarnessRevisionId::new("agent-fixture-root-revision").expect("root revision ID"),
            "mismatched-apply-key",
        )))
        .expect("apply intent commits");
    assert!(matches!(
        fixture
            .session
            .append_fact(SessionFact::WorkspaceDeltaApplied(
                WorkspaceDeltaAppliedFact {
                    delta_id: delta.delta_id,
                    target_lane_id: LaneId::main(),
                    tool_call_id: "mismatched-apply-call".into(),
                    changed_paths: delta.changed_paths,
                }
            )),
        Err(SessionError::Corruption(_))
    ));
}

struct SpawnedAgentFixture {
    session: MemorySession,
    agent: AgentId,
    child_lane: LaneId,
    child_profile: ModelHarnessProfileId,
    child_revision: HarnessRevisionId,
    child_revision_entry: EntryId,
    parent_operation: OperationId,
    assignment: String,
    spawn: AgentSpawnedFact,
}

struct RunningAgentFixture {
    session: MemorySession,
    agent: AgentId,
    child_operation: OperationId,
}

fn spawned_agent_fixture() -> SpawnedAgentFixture {
    let session_id = SessionId::new("agent-fixture-session").expect("valid session ID");
    let mut session = MemorySession::create(SessionHeader::new(
        session_id.clone(),
        "workspace-test",
        Metadata::new(),
    ))
    .expect("session creates");
    let model = fixture_subagent_model();
    session
        .append_fact(SessionFact::SubagentPolicy(SubagentPolicyFact {
            schema_version: 1,
            models: vec![model.clone()],
            max_concurrent: 2,
            max_total_per_operation: 2,
            timeout_ms: 30_000,
            tool_surface_digest: Digest::from_bytes(b"fixture tools"),
        }))
        .expect("policy commits");
    let parent_operation =
        OperationId::new("agent-fixture-parent-operation").expect("valid parent operation ID");
    let root_revision =
        HarnessRevisionId::new("agent-fixture-root-revision").expect("valid root revision ID");
    let root_profile =
        ModelHarnessProfileId::new("agent-fixture-root-profile").expect("valid root profile ID");
    let parent_epoch = EpochId::new("agent-fixture-parent-epoch").expect("valid parent epoch ID");
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            parent_operation.clone(),
            LaneId::main(),
            None,
            OperationKind::Run,
            Vec::new(),
            root_revision.clone(),
            root_profile.clone(),
        )))
        .expect("parent operation commits");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: parent_epoch.clone(),
            operation_id: parent_operation.clone(),
            epoch_index: 0,
            source_leaf_id: None,
            harness_revision_id: root_revision.clone(),
            harness_snapshot_id: HarnessSnapshotId::new("agent-fixture-root-snapshot")
                .expect("valid root snapshot ID"),
            model_harness_profile: root_profile,
            core_run_id: CoreRunId::new("agent-fixture-parent-core-run")
                .expect("valid core run ID"),
            epoch_resume_data: std::collections::BTreeMap::new(),
        }))
        .expect("parent epoch commits");
    append_parent_tool_intent(
        &mut session,
        &parent_operation,
        &parent_epoch,
        &root_revision,
        "agent-fixture-spawn-call",
        "agent-fixture-spawn-key",
        "audit_session",
    );

    let agent = AgentId::derive(
        &session_id,
        &LaneId::main(),
        &parent_operation,
        "agent-fixture-spawn-key",
    );
    let child_lane = agent.lane_id();
    let child_revision =
        HarnessRevisionId::new("agent-fixture-child-revision").expect("valid child revision ID");
    let child_snapshot =
        HarnessSnapshotId::new("agent-fixture-child-snapshot").expect("valid child snapshot ID");
    let child_profile =
        ModelHarnessProfileId::new("agent-fixture-child-profile").expect("valid child profile ID");
    let spawn = AgentSpawnedFact {
        agent_id: agent.clone(),
        parent_lane_id: LaneId::main(),
        parent_operation_id: parent_operation.clone(),
        lane_id: child_lane.clone(),
        task_name: "audit_session".into(),
        model,
        thinking: "high".into(),
        context_mode: AgentContextMode::Task,
        base_leaf_id: None,
        workspace_lease_id: WorkspaceLeaseId::derive(&agent),
        harness_revision_id: child_revision.clone(),
        harness_snapshot_id: child_snapshot,
        model_harness_profile_id: child_profile.clone(),
        spawn_tool_call_id: "agent-fixture-spawn-call".into(),
    };
    session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: child_lane.clone(),
            base_leaf_id: None,
        })
        .expect("child lane commits");
    append_child_configuration(&mut session, &spawn);
    session
        .append_fact(SessionFact::AgentSpawned(spawn.clone()))
        .expect("spawn fact commits");
    SpawnedAgentFixture {
        session,
        agent: agent.clone(),
        child_lane,
        child_profile,
        child_revision,
        child_revision_entry: child_revision_entry_id(&agent),
        parent_operation,
        assignment: "audit durable facts".into(),
        spawn,
    }
}

fn running_agent_fixture() -> RunningAgentFixture {
    let mut fixture = spawned_agent_fixture();
    let child_operation = derive_subagent_operation_id(&fixture.agent, &fixture.assignment);
    let child_lane = fixture.child_lane.clone();
    let child_profile = fixture.child_profile.clone();
    append_child_operation(
        &mut fixture,
        child_lane,
        child_operation.clone(),
        child_profile,
    )
    .expect("child operation commits");
    RunningAgentFixture {
        session: fixture.session,
        agent: fixture.agent,
        child_operation,
    }
}

fn append_child_operation(
    fixture: &mut SpawnedAgentFixture,
    lane_id: LaneId,
    operation_id: OperationId,
    profile: ModelHarnessProfileId,
) -> Result<StoredRecord, SessionError> {
    let source_leaf_id = Some(fixture.child_revision_entry.clone());
    append_child_operation_with_source(fixture, lane_id, operation_id, profile, source_leaf_id)
}

fn append_child_operation_with_source(
    fixture: &mut SpawnedAgentFixture,
    lane_id: LaneId,
    operation_id: OperationId,
    profile: ModelHarnessProfileId,
    source_leaf_id: Option<EntryId>,
) -> Result<StoredRecord, SessionError> {
    fixture
        .session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id,
            lane_id,
            source_leaf_id,
            OperationKind::Subagent {
                agent_id: fixture.agent.clone(),
                parent_operation_id: fixture.parent_operation.clone(),
            },
            vec![ProvisionedEntry::user(
                EntryId::new("agent-fixture-assignment").expect("valid assignment entry ID"),
                fixture.assignment.clone(),
            )],
            fixture.child_revision.clone(),
            profile,
        )))
}

fn append_parent_tool_intent(
    session: &mut MemorySession,
    parent_operation: &OperationId,
    parent_epoch: &EpochId,
    root_revision: &HarnessRevisionId,
    tool_call_id: &str,
    idempotency_key: &str,
    task_name: &str,
) {
    let arguments = JsonValue::Object(std::collections::BTreeMap::from([
        ("context".into(), JsonValue::String("task".into())),
        ("model".into(), JsonValue::String("fixture-child".into())),
        (
            "task".into(),
            JsonValue::String("audit durable facts".into()),
        ),
        ("task_name".into(), JsonValue::String(task_name.into())),
        ("thinking".into(), JsonValue::String("high".into())),
    ]));
    let assistant_id =
        EntryId::new(format!("{tool_call_id}-assistant")).expect("valid tool assistant entry ID");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                "",
                vec![AssistantToolCall::new(
                    tool_call_id,
                    "spawn_agent",
                    arguments.clone(),
                )],
            ),
        )
        .expect("parent tool assistant commits");
    session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new(format!("{tool_call_id}-record")).expect("valid tool record ID"),
            parent_operation.clone(),
            parent_epoch.clone(),
            assistant_id,
            0,
            tool_call_id,
            "spawn_agent",
            arguments,
            EntryId::new(format!("{tool_call_id}-result")).expect("valid tool result entry ID"),
            ToolReplayPolicy::Never,
            Digest::from_bytes(tool_call_id.as_bytes()),
            root_revision.clone(),
            idempotency_key,
        )))
        .expect("parent tool intent commits");
}

fn append_child_configuration(session: &mut MemorySession, spawn: &AgentSpawnedFact) {
    session
        .append_entry(
            &spawn.lane_id,
            ProvisionedEntry {
                id: EntryId::new(format!("{}-model", spawn.agent_id))
                    .expect("valid model entry ID"),
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: spawn.model.provider.clone(),
                    model: spawn.model.model.clone(),
                    revision: spawn.model.revision.clone(),
                }),
            },
        )
        .expect("child model commits");
    session
        .append_entry(
            &spawn.lane_id,
            ProvisionedEntry {
                id: EntryId::new(format!("{}-thinking", spawn.agent_id))
                    .expect("valid thinking entry ID"),
                body: SessionEntry::ThinkingChanged(ThinkingChangedEntry {
                    level: spawn.thinking.clone(),
                }),
            },
        )
        .expect("child thinking commits");
    session
        .append_entry(
            &spawn.lane_id,
            ProvisionedEntry {
                id: child_revision_entry_id(&spawn.agent_id),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: spawn.harness_revision_id.clone(),
                    snapshot_id: spawn.harness_snapshot_id.clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("child harness commits");
}

fn child_revision_entry_id(agent_id: &AgentId) -> EntryId {
    EntryId::new(format!("{agent_id}-revision")).expect("valid child revision entry ID")
}

fn append_second_spawn(fixture: &mut SpawnedAgentFixture, task_name: String) -> AgentSpawnedFact {
    let parent_epoch = EpochId::new("agent-fixture-parent-epoch").expect("valid parent epoch ID");
    let root_revision =
        HarnessRevisionId::new("agent-fixture-root-revision").expect("valid root revision ID");
    append_parent_tool_intent(
        &mut fixture.session,
        &fixture.parent_operation,
        &parent_epoch,
        &root_revision,
        "agent-fixture-second-spawn-call",
        "agent-fixture-second-spawn-key",
        &task_name,
    );
    let session_id = fixture
        .session
        .snapshot()
        .expect("snapshot")
        .header()
        .session_id
        .clone();
    let agent = AgentId::derive(
        &session_id,
        &LaneId::main(),
        &fixture.parent_operation,
        "agent-fixture-second-spawn-key",
    );
    let spawn = AgentSpawnedFact {
        agent_id: agent.clone(),
        parent_lane_id: LaneId::main(),
        parent_operation_id: fixture.parent_operation.clone(),
        lane_id: agent.lane_id(),
        task_name,
        model: fixture.spawn.model.clone(),
        thinking: fixture.spawn.thinking.clone(),
        context_mode: AgentContextMode::Task,
        base_leaf_id: None,
        workspace_lease_id: WorkspaceLeaseId::derive(&agent),
        harness_revision_id: HarnessRevisionId::new("agent-fixture-second-revision")
            .expect("valid child revision ID"),
        harness_snapshot_id: HarnessSnapshotId::new("agent-fixture-second-snapshot")
            .expect("valid child snapshot ID"),
        model_harness_profile_id: ModelHarnessProfileId::new("agent-fixture-second-profile")
            .expect("valid child profile ID"),
        spawn_tool_call_id: "agent-fixture-second-spawn-call".into(),
    };
    fixture
        .session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: spawn.lane_id.clone(),
            base_leaf_id: None,
        })
        .expect("second child lane commits");
    append_child_configuration(&mut fixture.session, &spawn);
    spawn
}

fn fixture_subagent_model() -> SubagentModelRecord {
    SubagentModelRecord {
        provider: "scripted".into(),
        model: "fixture-child".into(),
        revision: Some("fixture-revision".into()),
        display_name: "Fixture child".into(),
        context_window: Some(16_000),
    }
}

fn workspace_delta_fact(
    agent_id: &AgentId,
    workspace_lease_id: WorkspaceLeaseId,
    changed_paths: Vec<String>,
) -> WorkspaceDeltaFact {
    WorkspaceDeltaFact {
        delta_id: WorkspaceDeltaId::derive(&workspace_lease_id, "base", "result"),
        agent_id: agent_id.clone(),
        workspace_lease_id,
        base_commit: "base".into(),
        result_commit: "result".into(),
        changed_paths,
        patch: PayloadRef::Artifact {
            artifact_id: ArtifactId::from_bytes(b"p"),
            byte_len: 1,
            media_type: "text/x-diff".into(),
        },
    }
}

fn assert_rejected_agent_fact(mut session: MemorySession, fact: AgentSpawnedFact, _reason: &str) {
    assert!(matches!(
        session.append_fact(SessionFact::AgentSpawned(fact)),
        Err(SessionError::Corruption(_))
    ));
}
