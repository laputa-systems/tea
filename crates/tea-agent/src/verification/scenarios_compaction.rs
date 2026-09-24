//! Durable automatic-compaction verification with injected restricted consumers.
//!
//! This feature-only scenario uses a deliberately bounded synthetic history to
//! request an automatic checkpoint. It returns only durable facts; neither the
//! root response, the summary response, nor the caller's marker is returned.

use super::{
    is_exact_codex_descriptor, LiveVerificationError, RestrictedCodexConsumer, VerificationConsumer,
};
use std::num::NonZeroU64;
use std::path::Path;
use tea_core::compaction::{AutomaticCompactionPolicy, ContextBudgetSource, OverflowRecovery};
use tea_session::{
    LaneRecord, PayloadRef, ProviderSettlementClassification, SessionEntry, SessionFact,
    SessionSnapshot, StepKind,
};

const SYNTHETIC_HISTORY_BYTES: usize = 4 * 1024;

/// Explicit inputs for one feature-only live compaction verification run.
///
/// The caller owns both directories and supplies only restricted consumers
/// from the verification factory. `critical_fact` is synthetic or public
/// input; it is used only as an internal retention oracle and never appears
/// in returned evidence or diagnostics.
pub struct LiveCompactionScenario<'a> {
    /// Existing caller-owned Tea home for the disposable verification session.
    pub tea_home: &'a Path,
    /// Existing caller-owned disposable workspace for the verification run.
    pub workspace: &'a Path,
    /// Restricted consumer that drives the root conversation.
    pub root: &'a RestrictedCodexConsumer,
    /// Restricted consumer that produces the compaction checkpoint.
    pub compactor: &'a RestrictedCodexConsumer,
    /// Synthetic marker that the compactor must retain in its checkpoint.
    pub critical_fact: &'a str,
}

/// Content-free evidence from one live automatic-compaction verification run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveCompactionScenarioOutcome {
    /// A compaction provider intent and exact request-material fact were committed.
    pub compaction_request_observed: bool,
    /// A checkpoint links that request to a completed provider settlement.
    pub checkpoint_committed: bool,
    /// The exact checkpoint replacement retains the supplied synthetic marker.
    pub critical_fact_retained: bool,
    /// Both the settled and passively reopened durable snapshots verify.
    pub durable_state_verified: bool,
}

/// Run a feature-only root-plus-compactor verification through the terminal's
/// normal durable composition path.
///
/// The prompt asks the root model to issue a harmless read request before its
/// answer so a second model boundary exists for the forced automatic policy.
/// A model that declines that request yields `false` checkpoint evidence rather
/// than silently fabricating a compaction result.
pub fn run_live_compaction_scenario(
    scenario: LiveCompactionScenario<'_>,
) -> Result<LiveCompactionScenarioOutcome, LiveVerificationError> {
    validate_scenario(&scenario)?;
    let policy = forced_compaction_policy();
    let harness = crate::app::create_live_verification_compaction_harness(
        scenario.tea_home,
        scenario.workspace,
        scenario.root.model().clone(),
        scenario.root.provider(),
        (
            scenario.compactor.model().clone(),
            scenario.compactor.provider(),
        ),
        policy.clone(),
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let operation =
        smol::block_on(harness.run_root_prompt(synthetic_prompt(scenario.critical_fact)))
            .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    if !operation.is_completed() {
        return Err(LiveVerificationError::new(
            "live compaction verification root operation did not complete",
        ));
    }
    let verification = harness
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()));
    let snapshot = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let initial_evidence = compaction_evidence(&snapshot, scenario.critical_fact);
    let session_id = snapshot.header().session_id.to_string();
    let closed = smol::block_on(harness.close())
        .map_err(|error| LiveVerificationError::new(error.to_string()));
    // Closing joins work; only dropping the last handle releases the single
    // session writer that the passive reopen below must acquire.
    drop(harness);
    verification?;
    closed?;

    let reopened = crate::app::reopen_live_verification_compaction_harness(
        scenario.tea_home,
        scenario.workspace,
        &session_id,
        scenario.root.model().clone(),
        scenario.root.provider(),
        (
            scenario.compactor.model().clone(),
            scenario.compactor.provider(),
        ),
        policy,
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    reopened
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let reopened_snapshot = reopened
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let reopened_evidence = compaction_evidence(&reopened_snapshot, scenario.critical_fact);
    smol::block_on(reopened.close())
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;

    Ok(LiveCompactionScenarioOutcome {
        compaction_request_observed: initial_evidence.compaction_request_observed
            && reopened_evidence.compaction_request_observed,
        checkpoint_committed: initial_evidence.checkpoint_committed
            && reopened_evidence.checkpoint_committed,
        critical_fact_retained: initial_evidence.critical_fact_retained
            && reopened_evidence.critical_fact_retained,
        durable_state_verified: true,
    })
}

fn validate_scenario(scenario: &LiveCompactionScenario<'_>) -> Result<(), LiveVerificationError> {
    if !scenario.tea_home.is_dir() || !scenario.workspace.is_dir() {
        return Err(LiveVerificationError::new(
            "live compaction verification requires explicit existing temporary home and workspace directories",
        ));
    }
    if scenario.root.role() != VerificationConsumer::Root
        || !is_exact_codex_descriptor(scenario.root.model())
    {
        return Err(LiveVerificationError::new(
            "live compaction verification requires the restricted canonical root consumer",
        ));
    }
    if scenario.compactor.role() != VerificationConsumer::Compaction
        || !is_exact_codex_descriptor(scenario.compactor.model())
    {
        return Err(LiveVerificationError::new(
            "live compaction verification requires the restricted canonical compaction consumer",
        ));
    }
    if scenario.critical_fact.trim().is_empty() || scenario.critical_fact.len() > 256 {
        return Err(LiveVerificationError::new(
            "live compaction verification requires a non-empty bounded synthetic marker",
        ));
    }
    Ok(())
}

fn forced_compaction_policy() -> AutomaticCompactionPolicy {
    AutomaticCompactionPolicy {
        enabled: true,
        context_budget: ContextBudgetSource::ContextBudget(
            // The synthetic history crosses this threshold before its first
            // tool result. A completed checkpoint leaves enough room for the
            // fixed coding-tool surface and a short follow-up tool call.
            NonZeroU64::new(2_400).expect("fixed live verification context budget is nonzero"),
        ),
        reserved_tokens: 1,
        minimum_headroom_tokens: 1,
        recent_tokens: 0,
        overflow_recovery: OverflowRecovery::Disabled,
        max_compactions_per_run: 3,
        max_overflow_retries_per_run: 0,
    }
}

fn synthetic_prompt(critical_fact: &str) -> String {
    let mut history = String::with_capacity(SYNTHETIC_HISTORY_BYTES);
    while history.len() < SYNTHETIC_HISTORY_BYTES {
        history.push_str(critical_fact);
        history.push('\n');
    }
    format!(
        "This is a disposable public verification fixture. Before your final response, call the read tool exactly once with path `compaction-verification-missing.txt`. Preserve the repeated critical marker in your work.\n\n{history}"
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CompactionEvidence {
    compaction_request_observed: bool,
    checkpoint_committed: bool,
    critical_fact_retained: bool,
}

fn compaction_evidence(snapshot: &SessionSnapshot, critical_fact: &str) -> CompactionEvidence {
    let mut evidence = CompactionEvidence::default();
    for record in snapshot.records() {
        let LaneRecord::ProviderRequestStarted(started) = &record.record else {
            continue;
        };
        if compaction_request_has_intent_and_material(snapshot, &started.request_id) {
            evidence.compaction_request_observed = true;
        }
    }

    for stored in snapshot.entries() {
        let SessionEntry::Compaction(entry) = &stored.body else {
            continue;
        };
        let Some(request_id) = &entry.provider_request_id else {
            continue;
        };
        if !compaction_request_completed(snapshot, request_id) {
            continue;
        }
        evidence.checkpoint_committed = true;
        evidence.critical_fact_retained |= replacement_contains(&entry.replacement, critical_fact);
    }
    evidence
}

fn compaction_request_has_intent_and_material(
    snapshot: &SessionSnapshot,
    request_id: &tea_session::ProviderRequestId,
) -> bool {
    let Some(started) = snapshot
        .records()
        .iter()
        .find_map(|record| match &record.record {
            LaneRecord::ProviderRequestStarted(started) if &started.request_id == request_id => {
                Some(started)
            }
            _ => None,
        })
    else {
        return false;
    };
    let is_compaction_step = snapshot.records().iter().any(|candidate| {
        matches!(
            &candidate.record,
            LaneRecord::StepAttempted(step)
                if step.id == started.step_id
                    && step.kind == StepKind::Compaction
                    && step.operation_id == started.operation_id
                    && step.epoch_id == started.epoch_id
        )
    });
    let has_material = snapshot.facts().iter().any(|fact| {
        matches!(
            &fact.fact,
            SessionFact::ProviderRequestMaterial(material)
                if material.request_id == started.request_id
                    && material.operation_id == started.operation_id
                    && material.epoch_id == started.epoch_id
        )
    });
    is_compaction_step && has_material
}

fn compaction_request_completed(
    snapshot: &SessionSnapshot,
    request_id: &tea_session::ProviderRequestId,
) -> bool {
    compaction_request_has_intent_and_material(snapshot, request_id)
        && snapshot.records().iter().any(|record| {
            matches!(
                &record.record,
                LaneRecord::ProviderRequestSettled(settled)
                    if settled.request_id == *request_id
                        && settled.classification == ProviderSettlementClassification::Completed
            )
        })
}

fn replacement_contains(replacement: &PayloadRef, critical_fact: &str) -> bool {
    match replacement {
        PayloadRef::Inline(value) => json_contains_text(value, critical_fact),
        PayloadRef::Artifact { .. } => false,
    }
}

fn json_contains_text(value: &tea_protocol::JsonValue, critical_fact: &str) -> bool {
    match value {
        tea_protocol::JsonValue::String(text) => text.contains(critical_fact),
        tea_protocol::JsonValue::Array(values) => values
            .iter()
            .any(|value| json_contains_text(value, critical_fact)),
        tea_protocol::JsonValue::Object(fields) => fields
            .values()
            .any(|value| json_contains_text(value, critical_fact)),
        tea_protocol::JsonValue::Null
        | tea_protocol::JsonValue::Bool(_)
        | tea_protocol::JsonValue::Number(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tea_core::scheduler::{
        CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
    };
    use tea_core::state::{
        AgentToolCall, ModelDescriptor, SerializedJson, StopReason, ToolCallId, Usage,
    };

    const CRITICAL_FACT: &str = "verification-critical-fact";

    struct QueuedProvider {
        streams: Mutex<VecDeque<ModelStream>>,
        requests: AtomicUsize,
    }

    impl QueuedProvider {
        fn new(streams: Vec<ModelStream>) -> Self {
            Self {
                streams: Mutex::new(streams.into()),
                requests: AtomicUsize::new(0),
            }
        }
    }

    impl ModelProvider for QueuedProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let stream = self
                .streams
                .lock()
                .expect("queued verification provider mutex")
                .pop_front()
                .expect("queued verification provider has a stream");
            Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
        }
    }

    fn model() -> ModelDescriptor {
        ModelDescriptor {
            provider: super::super::CODEX_PROVIDER_ID.into(),
            model: super::super::CODEX_MODEL_ID.into(),
            revision: None,
        }
    }

    fn consumer(
        role: VerificationConsumer,
        provider: Arc<dyn ModelProvider>,
    ) -> super::super::RestrictedCodexConsumer {
        super::super::RestrictedCodexConsumer {
            model: model(),
            provider,
            role,
        }
    }

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let path = std::env::temp_dir().join(format!(
            "tea-live-compaction-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).expect("temporary verification directory creates");
        path
    }

    #[test]
    fn deterministic_compaction_fixture_uses_the_same_durable_oracles() {
        let root_provider = Arc::new(QueuedProvider::new(vec![
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("compaction-verification-read")
                            .expect("fixture tool ID"),
                        name: "read".into(),
                        arguments: SerializedJson::new(
                            r#"{"path":"compaction-verification-missing.txt"}"#,
                        ),
                    }),
                    // Match the provider-confirmed input levels observed in
                    // the synthetic live fixture before and after compaction.
                    ModelStreamEvent::Usage(Usage {
                        input_tokens: Some(2_764),
                        ..Usage::default()
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("compaction-verification-extra-tool")
                            .expect("fixture tool ID"),
                        name: "get_goal".into(),
                        arguments: SerializedJson::new(r#"{}"#),
                    }),
                    ModelStreamEvent::Usage(Usage {
                        input_tokens: Some(2_045),
                        ..Usage::default()
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("fixture root completion".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let compactor_provider = Arc::new(QueuedProvider::new(vec![ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta(CRITICAL_FACT.into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]));
        let tea_home = temporary_directory("home");
        let workspace = temporary_directory("workspace");
        let root = consumer(VerificationConsumer::Root, root_provider.clone());
        let compactor = consumer(VerificationConsumer::Compaction, compactor_provider.clone());

        let outcome = run_live_compaction_scenario(LiveCompactionScenario {
            tea_home: &tea_home,
            workspace: &workspace,
            root: &root,
            compactor: &compactor,
            critical_fact: CRITICAL_FACT,
        })
        .expect("deterministic providers satisfy the durable scenario");

        assert_eq!(
            outcome,
            LiveCompactionScenarioOutcome {
                compaction_request_observed: true,
                checkpoint_committed: true,
                critical_fact_retained: true,
                durable_state_verified: true,
            }
        );
        assert_eq!(root_provider.requests.load(Ordering::SeqCst), 3);
        assert_eq!(compactor_provider.requests.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(tea_home).expect("temporary Tea home removes");
        fs::remove_dir_all(workspace).expect("temporary workspace removes");
    }
}
