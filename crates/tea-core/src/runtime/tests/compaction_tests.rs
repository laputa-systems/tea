use super::*;
use crate::compaction::{
    AutomaticCompactionPolicy, CompactionContext, CompactionError, CompactionFuture,
    CompactionRequestPort, CompactionResult, CompactionStrategy, Compactor, ContextBudgetSource,
    OverflowRecovery,
};
use crate::effect::CompactionProviderEffectOutcome;
use crate::runtime::context::{derive_default_snapshot_context, encode_compaction_replacement};
use crate::scheduler::ModelRequest;
use crate::state::{AgentMessage, MessageId};
use tea_session::{ProviderSettlementClassification, StepKind};

const COMPACTION_SYSTEM_PROMPT: &str = "fixture compaction system prompt";
const COMPACTION_CONTEXT: &str = "fixture exact post-hook compaction context";
const COMPACTION_REPLACEMENT: &str = "fixture exact durable compaction replacement";

struct RequestMaterialCompactor {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

/// A deliberately legacy provider strategy that never asks the core to admit
/// its claimed provider request. Durable compaction must reject this rather
/// than writing a checkpoint that falsely appears provider-free.
struct UngatedProviderStrategyCompactor;

impl Compactor for RequestMaterialCompactor {
    fn strategy(&self) -> CompactionStrategy {
        CompactionStrategy::cache_replay_summary_v1(17)
    }

    fn compact<'a>(
        &'a self,
        _context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Err(CompactionError::failed(
            "fixture compactor requires the automatic request port",
        ))))
    }

    fn compact_automatic_with_requests<'a>(
        &'a self,
        _context: CompactionContext,
        _request: crate::compaction::AutomaticCompactionRequest,
        _cancellation: CancellationToken,
        requests: &'a dyn CompactionRequestPort,
    ) -> CompactionFuture<'a> {
        let observed_requests = Arc::clone(&self.requests);
        Box::pin(async move {
            let request = ModelRequest {
                system_prompt: COMPACTION_SYSTEM_PROMPT.into(),
                context: COMPACTION_CONTEXT.into(),
                ..ModelRequest::default()
            };
            let ticket = requests.begin_provider_request(request.clone()).await?;
            observed_requests
                .lock()
                .expect("fixture compactor request mutex")
                .push(request);
            requests
                .settle_provider_request(
                    ticket,
                    CompactionProviderEffectOutcome::Succeeded {
                        usage: None,
                        request_observation: None,
                    },
                )
                .await?;
            Ok(CompactionResult::new(vec![AgentMessage::User {
                id: MessageId(1),
                content: COMPACTION_REPLACEMENT.into(),
            }]))
        })
    }
}

impl Compactor for UngatedProviderStrategyCompactor {
    fn strategy(&self) -> CompactionStrategy {
        CompactionStrategy::cache_replay_summary_v1(23)
    }

    fn compact<'a>(
        &'a self,
        _context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Ok(CompactionResult::new(vec![
            AgentMessage::User {
                id: MessageId(1),
                content: "ungated provider strategy checkpoint".into(),
            },
        ]))))
    }

    fn compact_automatic<'a>(
        &'a self,
        _context: CompactionContext,
        _request: crate::compaction::AutomaticCompactionRequest,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        self.compact(_context, _cancellation)
    }
}

fn compaction_fixture_runtime(
    requests: Arc<Mutex<Vec<ModelRequest>>>,
) -> (Arc<SessionSupervisor<MemorySession>>, Arc<MemoryArtifactStore>) {
    let compactor: Arc<dyn Compactor> = Arc::new(RequestMaterialCompactor { requests });
    compaction_fixture_runtime_with_compactor(compactor)
}

fn compaction_fixture_runtime_with_compactor(
    compactor: Arc<dyn Compactor>,
) -> (Arc<SessionSupervisor<MemorySession>>, Arc<MemoryArtifactStore>) {
    let provider: Arc<dyn ModelProvider> = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::from([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("first bounded answer".into()),
                    ModelStreamEvent::End(StopReason::Length),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("final answer after compaction".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ])),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let artifact_store: Arc<dyn ArtifactStore> = store.clone();
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(RecordingTool));
    let services = RuntimeServices::new(provider, tools)
        .compactor(compactor)
        .automatic_compaction(AutomaticCompactionPolicy {
            enabled: true,
            context_budget: ContextBudgetSource::ContextBudget(
                NonZeroU64::new(64).expect("fixture context budget is nonzero"),
            ),
            reserved_tokens: 1,
            minimum_headroom_tokens: 1,
            recent_tokens: 0,
            overflow_recovery: OverflowRecovery::Disabled,
            max_compactions_per_run: 1,
            max_overflow_retries_per_run: 0,
        });
    let mut repository =
        HarnessRepository::with_extension_engine(artifact_store.clone(), Arc::new(NoExtensions));
    let snapshot = repository
        .stage_snapshot(snapshot_spec(services.runtime_policy_identities()))
        .expect("compaction fixture snapshot stages");
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("compaction fixture revision stages");
    let identity = HarnessIdentity::new(
        revision.revision_id.clone(),
        snapshot.id,
        snapshot.spec.model_harness_profile,
    );
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("runtime-durable-compaction").expect("fixture session ID"),
        "runtime-compaction-workspace",
        fixture_metadata(),
    ))
    .expect("compaction fixture session creates");
    append_initial_revision(&mut session, &identity);
    (
        SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: Arc::new(HarnessResolver::new(repository, Default::default())),
            root_identity: identity,
            root_services: services,
            artifacts: artifact_store,
            rollover_budget: 1,
            subagents: None,
        })
        .expect("compaction fixture supervisor creates"),
        store,
    )
}

#[test]
fn provider_strategy_without_request_intent_cannot_commit_a_checkpoint() {
    smol::block_on(async {
        let (runtime, _artifacts) =
            compaction_fixture_runtime_with_compactor(Arc::new(UngatedProviderStrategyCompactor));
        let original_prompt = "ungated provider strategy source ".repeat(64);

        // Every durable gate rejection is an integrity fault: it fails closed
        // rather than silently recording a provider-free checkpoint.
        let error = runtime
            .run_root_prompt(original_prompt.clone())
            .await
            .expect_err("the missing provider intent rejects the checkpoint");
        assert!(
            error.to_string().contains("no admitted request intent"),
            "{error}"
        );

        let snapshot = runtime.snapshot().expect("settled snapshot reads");
        assert!(
            !snapshot
                .entries()
                .iter()
                .any(|entry| matches!(&entry.body, SessionEntry::Compaction(_))),
            "a provider strategy without a gated request cannot change durable context"
        );
        assert!(snapshot.entries().iter().any(|entry| matches!(
            &entry.body,
            SessionEntry::UserMessage(message) if message.content == original_prompt
        )));
        assert!(
            !snapshot.records().iter().any(|record| matches!(
                &record.record,
                LaneRecord::StepAttempted(step) if step.kind == StepKind::Compaction
            )),
            "no request intent is not relabelled as a deterministic compaction step"
        );
    });
}

#[test]
fn automatic_compaction_persists_exact_request_and_reopens_without_reinvocation() {
    smol::block_on(async {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (runtime, artifacts) = compaction_fixture_runtime(Arc::clone(&requests));
        let original_prompt = "durable compaction source ".repeat(64);

        let completed = runtime
            .run_root_prompt(original_prompt.clone())
            .await
            .expect("automatic compaction operation completes");
        assert!(completed.is_completed());

        let snapshot = runtime.snapshot().expect("completed snapshot reads");
        let compaction_entry = snapshot
            .entries()
            .iter()
            .find_map(|entry| match &entry.body {
                SessionEntry::Compaction(entry) => Some(entry),
                _ => None,
            })
            .expect("automatic compaction persists one checkpoint");
        let request_id = compaction_entry
            .provider_request_id
            .as_ref()
            .expect("provider-backed compaction records its request identity");
        let step = snapshot
            .records()
            .iter()
            .find_map(|record| match &record.record {
                LaneRecord::StepAttempted(step)
                    if step.kind == StepKind::Compaction =>
                {
                    Some(step)
                }
                _ => None,
            })
            .expect("compaction provider request has a durable step");
        let started = snapshot
            .records()
            .iter()
            .find_map(|record| match &record.record {
                LaneRecord::ProviderRequestStarted(started)
                    if started.request_id == *request_id =>
                {
                    Some(started)
                }
                _ => None,
            })
            .expect("compaction provider request intent persists before dispatch");
        assert_eq!(started.step_id, step.id);
        let settled = snapshot
            .records()
            .iter()
            .find_map(|record| match &record.record {
                LaneRecord::ProviderRequestSettled(settled)
                    if settled.request_id == *request_id =>
                {
                    Some(settled)
                }
                _ => None,
            })
            .expect("compaction provider request settles before checkpoint");
        assert_eq!(
            settled.classification,
            ProviderSettlementClassification::Completed
        );
        let material = snapshot
            .facts()
            .iter()
            .find_map(|fact| match &fact.fact {
                SessionFact::ProviderRequestMaterial(material)
                    if material.request_id == *request_id =>
                {
                    Some(material)
                }
                _ => None,
            })
            .expect("compaction provider request retains exact material");
        let artifact_id = match &material.request {
            PayloadRef::Artifact { artifact_id, .. } => *artifact_id,
            PayloadRef::Inline(_) => panic!("compaction request material must use an immutable artifact"),
        };
        let material = JsonValue::parse(
            std::str::from_utf8(
                &artifacts
                    .get(artifact_id)
                    .expect("compaction request artifact remains available"),
            )
            .expect("compaction request artifact is UTF-8"),
        )
        .expect("compaction request artifact is canonical JSON");
        assert_eq!(
            material,
            JsonValue::object([
                ("format", JsonValue::String("tea-model-request".into())),
                ("version", JsonValue::from(1_u64)),
                (
                    "system_prompt",
                    JsonValue::String(COMPACTION_SYSTEM_PROMPT.into()),
                ),
                ("context", JsonValue::String(COMPACTION_CONTEXT.into())),
                ("tools", JsonValue::Array(Vec::new())),
                ("model", JsonValue::Null),
                ("thinking_level", JsonValue::String("off".into())),
                ("session_id", JsonValue::Null),
            ])
        );
        assert_eq!(
            requests
                .lock()
                .expect("fixture compactor request mutex")
                .as_slice(),
            &[ModelRequest {
                system_prompt: COMPACTION_SYSTEM_PROMPT.into(),
                context: COMPACTION_CONTEXT.into(),
                ..ModelRequest::default()
            }]
        );
        assert!(
            snapshot.entries().iter().any(|entry| matches!(
                &entry.body,
                SessionEntry::UserMessage(message) if message.content == original_prompt
            )),
            "compaction changes context projection but leaves raw source history immutable"
        );

        let before_reopen = derive_default_snapshot_context(&snapshot, LaneId::main())
            .expect("committed checkpoint derives a durable context");
        let before_reopen_material = encode_compaction_replacement(&before_reopen.messages)
            .expect("derived durable context encodes canonically");
        assert!(before_reopen.messages.iter().any(|message| matches!(
            message,
            AgentMessage::User { content, .. } if content == COMPACTION_REPLACEMENT
        )));
        assert!(before_reopen.messages.iter().all(|message| !matches!(
            message,
            AgentMessage::User { content, .. } if content == &original_prompt
        )));

        let session = runtime
            .clone_session_for_test()
            .expect("completed session clones for reopen");
        let (resolver, root_services, reopen_artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen inputs remain available");
        drop(runtime);
        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services: BTreeMap::new(),
            artifacts: reopen_artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("compacted session passively reopens");
        let reopened_snapshot = reopened.snapshot().expect("reopened snapshot reads");
        let after_reopen = derive_default_snapshot_context(&reopened_snapshot, LaneId::main())
            .expect("reopened checkpoint derives durable context");
        assert_eq!(
            encode_compaction_replacement(&after_reopen.messages)
                .expect("reopened durable context encodes canonically"),
            before_reopen_material
        );
        assert_eq!(
            requests
                .lock()
                .expect("fixture compactor request mutex")
                .len(),
            1,
            "passive reopen reconstructs the checkpoint without another compactor request"
        );
    });
}
