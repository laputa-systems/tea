use super::*;

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
    assert_eq!(session.snapshot().expect("snapshot succeeds").next_sequence(), Sequence(3));
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
fn jsonl_round_trips_a_harness_catalog_fact_and_pins_its_manifest() {
    let directory = temporary_session_directory("harness-catalog-fact");
    let catalog_bytes = br#"{\"schema_version\":1,\"kind\":\"fixture\"}"#;
    let catalog_id = ArtifactId::from_bytes(catalog_bytes);
    {
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
    }

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("session reopens");
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
        let verification = verify_session(
            &snapshot,
            &store,
            [transitive.artifact_id],
        )
        .expect("source prefix and all roots verify");
        assert_eq!(verification.artifact_count, 3);
        assert!(!verification.artifact_roots.contains(&orphan.artifact_id));

        let export = session
            .export_to(&export_directory, [transitive.artifact_id])
            .expect("complete export succeeds");
        assert_eq!(export.directory, export_directory);
        assert_eq!(export.verification, verification);
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
    let store = exported.artifact_store().expect("export object store opens");
    for artifact_id in [retained, catalog, transitive] {
        assert!(store.get(artifact_id).is_ok(), "reachable object {artifact_id} copied");
    }
    assert!(matches!(store.get(orphan), Err(ArtifactError::NotFound { .. })));
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
            model_harness_profile: ModelHarnessProfileId::new("profile-1").expect("valid profile ID"),
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

    let reduction = reduce_lane(session.snapshot().expect("snapshot succeeds"), lane)
        .expect("prefix is valid");
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
    {
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
        session
            .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
                id: epoch_id.clone(),
                operation_id: operation_id.clone(),
                epoch_index: 0,
                source_leaf_id: None,
                harness_revision_id: HarnessRevisionId::new("jsonl-revision").expect("valid revision ID"),
                harness_snapshot_id: HarnessSnapshotId::new("jsonl-snapshot").expect("valid snapshot ID"),
                model_harness_profile: ModelHarnessProfileId::new("jsonl-profile").expect("valid profile ID"),
                core_run_id: CoreRunId::new("jsonl-core-run").expect("valid core run ID"),
                epoch_resume_data: std::collections::BTreeMap::new(),
            }))
            .expect("epoch starts");
        session
            .append_entry(
                &lane,
                ProvisionedEntry::assistant(
                    assistant_id.clone(),
                    "",
                    vec![AssistantToolCall::new("call-jsonl", "write", JsonValue::Null)],
                ),
            )
            .expect("assistant entry persists");
        session
            .append_record(LaneRecord::tool_started(ToolStartedRecord::new(
                RecordId::new("jsonl-tool-record").expect("valid record ID"),
                operation_id,
                epoch_id,
                assistant_id,
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
    }
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict).expect("session reopens");
    let reduction = reduce_lane(reopened.snapshot().expect("snapshot succeeds"), lane)
        .expect("durable prefix reduces");
    assert_eq!(
        reduction.recovery_plan,
        Some(RecoveryPlan::SynthesizeInterruptedToolResult {
            result_entry_id: result_id,
        })
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn jsonl_v1_truncates_an_uncommitted_torn_tail_without_losing_the_durable_prefix() {
    let directory = temporary_session_directory("torn-tail");
    let session_id = SessionId::new("session-jsonl").expect("valid session ID");
    let header = SessionHeader::new(session_id, "workspace-test", Metadata::new());
    {
        let mut session = JsonlSession::create(&directory, header, DurabilityMode::Strict)
            .expect("v1 session creation succeeds");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry::user(EntryId::new("entry-jsonl").expect("valid entry ID"), "durable"),
            )
            .expect("entry append is durable");
    }
    let path = directory.join("session.jsonl");
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("session file opens")
        .write_all(b"{\"kind\":\"entry\"}\n")
        .expect("torn tail is injected");

    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict)
        .expect("torn final line is discarded on open");
    assert_eq!(reopened.snapshot().expect("snapshot succeeds").entries().len(), 1);
    let bytes = std::fs::read(&path).expect("session file reads");
    assert_eq!(bytes.last(), Some(&b'\n'));
    drop(reopened);
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
    assert!(matches!(session.append_record(invalid), Err(SessionError::Corruption(_))));
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
    let first = store.put("one two one".as_bytes(), "text/plain").expect("first object persists");
    let second = store.put("one two one".as_bytes(), "text/plain").expect("duplicate object persists idempotently");
    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(store.read_page(first.artifact_id, 4, 3).expect("page reads").bytes, b"two");
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
        store.get(retained.artifact_id).expect("reachable root survives"),
        b"reachable durable evidence"
    );
    assert!(matches!(
        store.get(abandoned.artifact_id),
        Err(ArtifactError::NotFound { .. })
    ));
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

fn temporary_session_directory(label: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tea-session-{label}-{}-{sequence:016x}.tea",
        std::process::id()
    ))
}

fn append_parent_chain<S: SessionWriter>(session: &mut S) -> Result<Vec<StoredEntry>, SessionError> {
    let lane = LaneId::main();
    let first = session.append_entry(
        &lane,
        ProvisionedEntry::user(EntryId::new("conformance-one").expect("valid entry ID"), "one"),
    )?;
    let second = session.append_entry(
        &lane,
        ProvisionedEntry::user(EntryId::new("conformance-two").expect("valid entry ID"), "two"),
    )?;
    Ok(vec![first, second])
}
