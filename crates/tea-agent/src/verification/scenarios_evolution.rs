//! Feature-only live verification for immutable Luau evolution and rollback.
//!
//! The scenario does not author a candidate itself. A guarded candidate-evaluation
//! consumer must use the normal `tea_harness` control tool in an author-mode
//! session. The runner then makes a separate ordinary tool-use request and a
//! separately authorized rollback request, checking only durable source and
//! state identities rather than retaining model or tool text.

use super::{
    is_exact_codex_descriptor, LiveVerificationError, RestrictedCodexConsumer, VerificationConsumer,
};
use std::path::Path;
use tea_core::state::ModelDescriptor;
use tea_session::{
    reduce_lane, HarnessRevisionChangedEntry, HarnessRevisionId, LaneId, LaneRecord, SessionEntry,
    SessionFact, SessionSnapshot,
};

/// Explicit disposable inputs for a live immutable-harness evolution exercise.
pub struct LiveEvolutionScenario<'a> {
    /// Existing caller-owned Tea home for the disposable authoring session.
    pub tea_home: &'a Path,
    /// Existing caller-owned public or synthetic workspace.
    pub workspace: &'a Path,
    /// Guarded consumer that actually evaluates and authors the candidate.
    pub candidate_evaluator: &'a RestrictedCodexConsumer,
    /// Public request that instructs the model to inspect and apply one bounded
    /// candidate. Operator-pinned global plugins such as `todo` are not
    /// editable, so the checked-in prompt adds a capability-free session plugin.
    pub activation_prompt: &'a str,
    /// Public request that exercises the resulting stateful extension after activation.
    pub use_prompt: &'a str,
    /// Public request that stages a normal immutable rollback to the original revision.
    pub rollback_prompt: &'a str,
}

/// Content-free evidence from a live immutable-harness evolution exercise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveEvolutionScenarioOutcome {
    /// A validated non-initial immutable revision became active.
    pub candidate_activated: bool,
    /// A later root request used that activated revision and committed todo state.
    pub revised_source_used: bool,
    /// The private todo value remained identical across the rollback transition.
    pub state_retained_across_rollback: bool,
    /// A normal immutable rollback revision restored the original source snapshot.
    pub rollback_activated: bool,
    /// The settled and passively reopened durable snapshots verified.
    pub durable_state_verified: bool,
}

/// Drive the real authoring, use, rollback, and passive-reopen boundaries.
///
/// Each model-facing request goes through the same single restricted consumer
/// from the suite factory. The trusted host, not this scenario, exposes
/// `tea_harness`, verifies candidate source, switches revisions at an epoch
/// boundary, invokes the extension capability, and proves the rollback lineage.
pub fn run_live_evolution_scenario(
    scenario: LiveEvolutionScenario<'_>,
) -> Result<LiveEvolutionScenarioOutcome, LiveVerificationError> {
    validate_scenario(&scenario)?;
    run_evolution_scenario(
        scenario.tea_home,
        scenario.workspace,
        scenario.candidate_evaluator.model().clone(),
        scenario.candidate_evaluator.provider(),
        scenario.activation_prompt,
        scenario.use_prompt,
        scenario.rollback_prompt,
    )
}

fn validate_scenario(scenario: &LiveEvolutionScenario<'_>) -> Result<(), LiveVerificationError> {
    if !scenario.tea_home.is_dir() || !scenario.workspace.is_dir() {
        return Err(LiveVerificationError::new(
            "live evolution verification requires explicit existing temporary home and workspace directories",
        ));
    }
    if scenario.candidate_evaluator.role() != VerificationConsumer::CandidateEvaluation
        || !is_exact_codex_descriptor(scenario.candidate_evaluator.model())
    {
        return Err(LiveVerificationError::new(
            "live evolution verification requires the restricted canonical candidate-evaluation consumer",
        ));
    }
    for (label, prompt) in [
        ("activation", scenario.activation_prompt),
        ("use", scenario.use_prompt),
        ("rollback", scenario.rollback_prompt),
    ] {
        if prompt.trim().is_empty() || prompt.len() > 16 * 1024 {
            return Err(LiveVerificationError::new(format!(
                "live evolution verification requires a bounded non-empty public {label} prompt",
            )));
        }
    }
    Ok(())
}

fn run_evolution_scenario(
    tea_home: &Path,
    workspace: &Path,
    model: ModelDescriptor,
    provider: std::sync::Arc<dyn tea_core::scheduler::ModelProvider>,
    activation_prompt: &str,
    use_prompt: &str,
    rollback_prompt: &str,
) -> Result<LiveEvolutionScenarioOutcome, LiveVerificationError> {
    let harness = crate::app::create_live_verification_authoring_harness(
        tea_home,
        workspace,
        model.clone(),
        std::sync::Arc::clone(&provider),
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let initial_snapshot = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let initial_revision = initial_revision(&initial_snapshot)?;

    let activation = smol::block_on(harness.run_authoring_prompt(activation_prompt))
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    if !activation.is_completed() {
        return Err(LiveVerificationError::new(
            "live evolution activation operation did not complete",
        ));
    }
    let activated_snapshot = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let activated_revision = activated_revision(&activated_snapshot, &initial_revision)?;

    let mut state_before_rollback = None;
    let mut revised_source_used = false;
    // A completed model turn may still contain a rejected todo call. Admit one
    // fresh explicit request if the durable state oracle did not change.
    for _ in 0..2 {
        let use_operation = smol::block_on(harness.run_root_prompt(use_prompt))
            .map_err(|error| LiveVerificationError::new(error.to_string()))?;
        if !use_operation.is_completed() {
            return Err(LiveVerificationError::new(
                "live evolution source-use operation did not complete",
            ));
        }
        let used_snapshot = harness
            .snapshot()
            .map_err(|error| LiveVerificationError::new(error.to_string()))?;
        state_before_rollback = todo_state(&used_snapshot)?;
        revised_source_used =
            operation_uses_revision(&used_snapshot, use_operation.id(), &activated_revision)
                && state_before_rollback.is_some();
        if revised_source_used {
            break;
        }
    }

    let rollback = smol::block_on(harness.run_authoring_prompt(rollback_prompt))
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    if !rollback.is_completed() {
        return Err(LiveVerificationError::new(
            "live evolution rollback operation did not complete",
        ));
    }
    harness
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let rolled_back_snapshot = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let rollback_activated = rollback_revision(
        &rolled_back_snapshot,
        &initial_revision,
        &activated_revision,
    );
    let state_retained_across_rollback =
        state_before_rollback == todo_state(&rolled_back_snapshot)?;
    let session_id = rolled_back_snapshot.header().session_id.to_string();
    smol::block_on(harness.close())
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    // Closing joins work; only dropping the last handle releases the single
    // session writer that the passive reopen below must acquire.
    drop(harness);

    let reopened = crate::app::reopen_live_verification_authoring_harness(
        tea_home,
        workspace,
        &session_id,
        model,
        provider,
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    reopened
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let reopened_snapshot = reopened
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let state_retained_across_reopen = state_before_rollback == todo_state(&reopened_snapshot)?;
    smol::block_on(reopened.close())
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;

    Ok(LiveEvolutionScenarioOutcome {
        candidate_activated: true,
        revised_source_used,
        state_retained_across_rollback: state_retained_across_rollback
            && state_retained_across_reopen,
        rollback_activated,
        durable_state_verified: true,
    })
}

fn initial_revision(
    snapshot: &SessionSnapshot,
) -> Result<HarnessRevisionId, LiveVerificationError> {
    revision_entries(snapshot)
        .into_iter()
        .next()
        .map(|entry| entry.revision_id.clone())
        .ok_or_else(|| {
            LiveVerificationError::new("live evolution session has no initial harness revision")
        })
}

fn activated_revision(
    snapshot: &SessionSnapshot,
    initial: &HarnessRevisionId,
) -> Result<HarnessRevisionId, LiveVerificationError> {
    revision_entries(snapshot)
        .into_iter()
        .skip(1)
        .find(|entry| entry.rollback_from.is_none() && entry.revision_id != *initial)
        .map(|entry| entry.revision_id.clone())
        .ok_or_else(|| {
            LiveVerificationError::new(
                "live evolution did not commit a validated immutable candidate activation",
            )
        })
}

fn rollback_revision(
    snapshot: &SessionSnapshot,
    initial: &HarnessRevisionId,
    activated: &HarnessRevisionId,
) -> bool {
    revision_entries(snapshot).into_iter().any(|entry| {
        entry.rollback_from.as_ref() == Some(activated)
            && entry.revision_id != *activated
            && entry.revision_id != *initial
    })
}

fn revision_entries(snapshot: &SessionSnapshot) -> Vec<&HarnessRevisionChangedEntry> {
    snapshot
        .entries()
        .iter()
        .filter_map(|entry| match &entry.body {
            SessionEntry::HarnessRevisionChanged(revision) => Some(revision),
            _ => None,
        })
        .collect()
}

fn operation_uses_revision(
    snapshot: &SessionSnapshot,
    operation_id: &tea_session::OperationId,
    revision: &HarnessRevisionId,
) -> bool {
    snapshot.records().iter().any(|record| {
        matches!(
            &record.record,
            LaneRecord::EpochStarted(epoch)
                if epoch.operation_id == *operation_id && epoch.harness_revision_id == *revision
        )
    })
}

fn todo_state(
    snapshot: &SessionSnapshot,
) -> Result<Option<tea_protocol::JsonValue>, LiveVerificationError> {
    let reduction = reduce_lane(snapshot.clone(), LaneId::main())
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let Some(state) = reduction.extension_state.get("todo") else {
        return Ok(None);
    };
    let has_durable_fact = snapshot.facts().iter().any(|fact| {
        matches!(
            &fact.fact,
            SessionFact::ExtensionStateValueSet(value)
                if value.lane_id == LaneId::main() && value.extension_id == "todo"
        )
    });
    Ok(has_durable_fact.then(|| state.value.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tea_core::scheduler::{
        CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
    };
    use tea_core::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId};
    use tea_protocol::JsonValue;

    const MARKER: &str = "live verification evolution marker";

    /// Scripted candidate evaluator for the three evolution operations. It
    /// derives each `base_revision` from the latest `tea_harness` status
    /// result in the request context, exactly as a model must.
    #[derive(Default)]
    struct EvolutionScriptProvider {
        requests: AtomicUsize,
        initial_revision: Mutex<Option<String>>,
        /// Whether each request's system prompt carried the authored section.
        marker_in_prompt: Mutex<Vec<bool>>,
    }

    fn latest_status_revision(context: &str) -> Option<String> {
        fn visit(value: &JsonValue, found: &mut Option<String>) {
            match value {
                JsonValue::String(text) => {
                    if let Ok(JsonValue::Object(fields)) = JsonValue::parse(text) {
                        if fields.get("operation").and_then(JsonValue::as_str) == Some("status") {
                            if let Some(revision) =
                                fields.get("active_revision").and_then(JsonValue::as_str)
                            {
                                *found = Some(revision.to_owned());
                            }
                        }
                    }
                }
                JsonValue::Array(values) => values.iter().for_each(|value| visit(value, found)),
                JsonValue::Object(fields) => fields.values().for_each(|value| visit(value, found)),
                JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
            }
        }
        let mut found = None;
        for line in context.lines() {
            if let Ok(value) = JsonValue::parse(line) {
                visit(&value, &mut found);
            }
        }
        if found.is_none() {
            visit(&JsonValue::parse(context).ok()?, &mut found);
        }
        found
    }

    /// The authoring ceiling admits no capability, so the candidate adds a
    /// new capability-free session plugin rather than editing a builtin: the
    /// operator-pinned `todo` and capability-bearing coding builtins are both
    /// rejected by candidate validation.
    fn marker_plugin_files() -> Vec<JsonValue> {
        let upsert = |path: &str, content: String| {
            JsonValue::object([
                ("operation", JsonValue::String("upsert".into())),
                ("path", JsonValue::String(path.into())),
                ("content", JsonValue::String(content)),
            ])
        };
        vec![
            upsert(
                "plugins/evolution_marker/manifest.json",
                r#"{"schema_version":1,"abi_version":3,"id":"evolution_marker","entrypoint":"init.luau","modules":["init.luau"],"requested_capabilities":[]}"#.into(),
            ),
            upsert(
                "plugins/evolution_marker/init.luau",
                format!(
                    "return {{ prompt_sections = {{{{ id = \"evolution_marker\", content = \"{MARKER}\" }}}} }}\n"
                ),
            ),
        ]
    }

    fn hypothesis() -> JsonValue {
        JsonValue::object([
            (
                "failure_signature",
                JsonValue::String("scripted evolution counterpart".into()),
            ),
            (
                "expected_effect",
                JsonValue::String("behavior-preserving marker".into()),
            ),
            (
                "regression_risk",
                JsonValue::String("none; comment only".into()),
            ),
        ])
    }

    fn tool_call(index: usize, name: &str, arguments: JsonValue) -> ModelStream {
        ModelStream {
            events: vec![
                ModelStreamEvent::ToolCall(AgentToolCall {
                    id: ToolCallId::new(format!("evolution-call-{index}"))
                        .expect("fixture call ID"),
                    name: name.into(),
                    arguments: SerializedJson::new(
                        arguments
                            .to_json_string()
                            .expect("fixture arguments encode"),
                    ),
                }),
                ModelStreamEvent::End(StopReason::ToolUse),
            ],
        }
    }

    fn text(content: &str) -> ModelStream {
        ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta(content.into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }
    }

    impl ModelProvider for EvolutionScriptProvider {
        fn stream<'a>(
            &'a self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            let index = self.requests.fetch_add(1, Ordering::SeqCst);
            self.marker_in_prompt
                .lock()
                .expect("prompt observation slot")
                .push(request.system_prompt.contains(MARKER));
            let status = || JsonValue::object([("operation", JsonValue::String("status".into()))]);
            let current = || {
                latest_status_revision(&request.context)
                    .expect("the preceding status result names the active revision")
            };
            let stream = match index {
                // Activation: inspect, then apply one behavior-preserving edit.
                0 => tool_call(index, "tea_harness", status()),
                1 => {
                    let base = current();
                    *self.initial_revision.lock().expect("initial revision slot") =
                        Some(base.clone());
                    tool_call(
                        index,
                        "tea_harness",
                        JsonValue::object([
                            ("operation", JsonValue::String("apply".into())),
                            ("base_revision", JsonValue::String(base)),
                            ("hypothesis", hypothesis()),
                            ("files", JsonValue::Array(marker_plugin_files())),
                            (
                                "registry_operations",
                                JsonValue::Array(vec![JsonValue::object([
                                    ("operation", JsonValue::String("add".into())),
                                    ("plugin_id", JsonValue::String("evolution_marker".into())),
                                ])]),
                            ),
                        ]),
                    )
                }
                2 => text("activated"),
                // First use attempt has a syntactically invalid todo row.
                3 => tool_call(
                    index,
                    "todo",
                    JsonValue::object([(
                        "markdown",
                        JsonValue::String("- public evolution state marker".into()),
                    )]),
                ),
                4 => text("used"),
                // A fresh explicit request repairs the rejected state update.
                5 => tool_call(
                    index,
                    "todo",
                    JsonValue::object([(
                        "markdown",
                        JsonValue::String("- [ ] public evolution state marker".into()),
                    )]),
                ),
                6 => text("used after repair"),
                // Rollback: inspect, then roll back to the original revision.
                7 => tool_call(index, "tea_harness", status()),
                8 => {
                    let initial = self
                        .initial_revision
                        .lock()
                        .expect("initial revision slot")
                        .clone()
                        .expect("activation recorded the initial revision");
                    tool_call(
                        index,
                        "tea_harness",
                        JsonValue::object([
                            ("operation", JsonValue::String("rollback".into())),
                            ("base_revision", JsonValue::String(current())),
                            ("target_revision", JsonValue::String(initial)),
                            ("hypothesis", hypothesis()),
                        ]),
                    )
                }
                9 => text("rolled back"),
                other => panic!("evolution script received unexpected request {other}"),
            };
            Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
        }
    }

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let path = std::env::temp_dir().join(format!(
            "tea-live-evolution-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).expect("temporary verification directory creates");
        path
    }

    #[test]
    fn deterministic_evolution_fixture_uses_the_same_durable_oracles() {
        let provider = Arc::new(EvolutionScriptProvider::default());
        let evaluator = super::super::RestrictedCodexConsumer {
            model: ModelDescriptor {
                provider: super::super::CODEX_PROVIDER_ID.into(),
                model: super::super::CODEX_MODEL_ID.into(),
                revision: None,
            },
            provider: provider.clone(),
            role: VerificationConsumer::CandidateEvaluation,
        };
        let tea_home = temporary_directory("home");
        let workspace = temporary_directory("workspace");

        let outcome = run_live_evolution_scenario(LiveEvolutionScenario {
            tea_home: &tea_home,
            workspace: &workspace,
            candidate_evaluator: &evaluator,
            activation_prompt: "scripted activation",
            use_prompt: "scripted use",
            rollback_prompt: "scripted rollback",
        })
        .expect("deterministic provider satisfies the durable evolution scenario");

        assert_eq!(
            outcome,
            LiveEvolutionScenarioOutcome {
                candidate_activated: true,
                revised_source_used: true,
                state_retained_across_rollback: true,
                rollback_activated: true,
                durable_state_verified: true,
            }
        );
        assert_eq!(provider.requests.load(Ordering::SeqCst), 10);
        assert_eq!(
            *provider
                .marker_in_prompt
                .lock()
                .expect("prompt observations"),
            vec![false, false, true, true, true, true, true, true, true, false],
            "only epochs under the activated revision carry the authored section"
        );
        fs::remove_dir_all(tea_home).expect("temporary Tea home removes");
        fs::remove_dir_all(workspace).expect("temporary workspace removes");
    }
}
