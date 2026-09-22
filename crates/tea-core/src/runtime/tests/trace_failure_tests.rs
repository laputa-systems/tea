use super::*;
use tea_session::{ArtifactDescriptor, ArtifactError};

#[derive(Clone)]
struct TraceArtifactFailureStore {
    delegate: Arc<MemoryArtifactStore>,
}

impl ArtifactStore for TraceArtifactFailureStore {
    fn put(&self, bytes: &[u8], media_type: &str) -> Result<ArtifactDescriptor, ArtifactError> {
        if media_type == "application/x-ndjson" {
            return Err(ArtifactError::Io {
                path: "trace-artifact-fixture".into(),
                message: "trace artifact fixture secret must not be retained".into(),
            });
        }
        self.delegate.put(bytes, media_type)
    }

    fn get(&self, artifact_id: tea_session::ArtifactId) -> Result<Vec<u8>, ArtifactError> {
        self.delegate.get(artifact_id)
    }

    fn inventory(&self) -> Result<Vec<tea_session::ArtifactInventoryItem>, ArtifactError> {
        self.delegate.inventory()
    }
}

#[test]
fn trace_artifact_failure_records_content_free_diagnostic_without_blocking_completion() {
    smol::block_on(async {
        let backing_store = Arc::new(MemoryArtifactStore::default());
        let artifact_store: Arc<dyn ArtifactStore> = Arc::new(TraceArtifactFailureStore {
            delegate: Arc::clone(&backing_store),
        });
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("model response secret must not leak".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            }])),
        });
        let (resolver, identity, services) = fixture_manager(provider, Arc::clone(&backing_store));
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("runtime-trace-artifact-failure").expect("fixture session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ))
        .expect("fixture session creates");
        append_initial_revision(&mut session, &identity);
        let runtime = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver,
            root_identity: identity,
            root_services: services,
            artifacts: artifact_store,
            rollover_budget: 1,
            subagents: None,
        })
        .expect("supervisor creates");

        let operation = runtime
            .run_root_prompt("complete despite optional trace retention")
            .await
            .expect("trace publication failure cannot reopen completed semantics");
        assert!(operation.is_completed());

        let snapshot = runtime.snapshot().expect("completed session snapshots");
        assert!(snapshot.records().iter().any(|stored| {
            matches!(
                &stored.record,
                LaneRecord::OperationFinished(OperationFinishedRecord {
                    operation_id,
                    outcome: OperationOutcome::Completed,
                    ..
                }) if operation_id == operation.id()
            )
        }));
        assert!(snapshot
            .facts()
            .iter()
            .all(|stored| !matches!(stored.fact, SessionFact::TraceArtifact(_))));

        let diagnostic = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::Custom { type_name, payload }
                    if type_name == "tea.trace-unavailable.v1" =>
                {
                    Some(payload)
                }
                _ => None,
            })
            .expect("trace failure records an explicit durable diagnostic");
        let fields = diagnostic
            .as_object()
            .expect("trace diagnostic is a JSON object");
        assert_eq!(
            fields.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["core_run_id", "epoch_id", "operation_id", "reason", "schema_version"]
        );
        assert_eq!(
            fields.get("operation_id").and_then(JsonValue::as_str),
            Some(operation.id().as_str())
        );
        assert_eq!(
            fields.get("reason").and_then(JsonValue::as_str),
            Some("artifact_store_unavailable")
        );
        assert_eq!(fields.get("schema_version").and_then(JsonValue::as_u64), Some(1));
        let encoded = diagnostic
            .to_json_string()
            .expect("diagnostic canonically encodes");
        assert!(!encoded.contains("fixture secret"));
        assert!(!encoded.contains("model response secret"));

        runtime
            .verify_durable_state()
            .expect("diagnostic-only trace failure preserves a verified session");
    });
}
