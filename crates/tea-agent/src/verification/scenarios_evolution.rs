//! Feature-only live verification for immutable Luau evolution and rollback.
//!
//! The scenario does not author a candidate itself. A guarded candidate-evaluation
//! consumer must use the normal `tea_harness` control tool in an author-mode
//! session. The runner then makes a separate ordinary tool-use request and a
//! separately authorized rollback request, checking only durable source and
//! state identities rather than retaining model or tool text.

use super::{
    LiveVerificationError, RestrictedZenConsumer, VerificationConsumer, is_exact_zen_descriptor,
};
use std::path::Path;
use tea_core::state::ModelDescriptor;
use tea_session::{
    HarnessRevisionChangedEntry, HarnessRevisionId, LaneId, LaneRecord, SessionEntry,
    SessionFact, SessionSnapshot, reduce_lane,
};

/// Explicit disposable inputs for a live immutable-harness evolution exercise.
pub struct LiveEvolutionScenario<'a> {
    /// Existing caller-owned Tea home for the disposable authoring session.
    pub tea_home: &'a Path,
    /// Existing caller-owned public or synthetic workspace.
    pub workspace: &'a Path,
    /// Guarded consumer that actually evaluates and authors the candidate.
    pub candidate_evaluator: &'a RestrictedZenConsumer,
    /// Public request that instructs the model to inspect and apply one bounded Luau edit.
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
        || !is_exact_zen_descriptor(scenario.candidate_evaluator.model())
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
    let state_before_rollback = todo_state(&used_snapshot)?;
    let revised_source_used = operation_uses_revision(
        &used_snapshot,
        use_operation.id(),
        &activated_revision,
    ) && state_before_rollback.is_some();

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
    let state_retained_across_rollback = state_before_rollback == todo_state(&rolled_back_snapshot)?;
    let session_id = rolled_back_snapshot.header().session_id.to_string();
    smol::block_on(harness.close()).map_err(|error| LiveVerificationError::new(error.to_string()))?;

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
    smol::block_on(reopened.close()).map_err(|error| LiveVerificationError::new(error.to_string()))?;

    Ok(LiveEvolutionScenarioOutcome {
        candidate_activated: true,
        revised_source_used,
        state_retained_across_rollback: state_retained_across_rollback && state_retained_across_reopen,
        rollback_activated,
        durable_state_verified: true,
    })
}

fn initial_revision(snapshot: &SessionSnapshot) -> Result<HarnessRevisionId, LiveVerificationError> {
    revision_entries(snapshot)
        .into_iter()
        .next()
        .map(|entry| entry.revision_id.clone())
        .ok_or_else(|| LiveVerificationError::new("live evolution session has no initial harness revision"))
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

fn todo_state(snapshot: &SessionSnapshot) -> Result<Option<tea_protocol::JsonValue>, LiveVerificationError> {
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
