#![forbid(unsafe_code)]
//! Frozen, capability-neutral control-plane contracts for harness evolution.
//!
//! This crate deliberately cannot execute a target agent, invoke a provider,
//! modify a session, or activate a harness revision. It records the immutable
//! experiment inputs and evaluates evidence supplied by a trusted runner. A
//! model may construct a [`CandidateProposalV1`], but only an operator-owned
//! [`Campaign`] can accept an evaluation or select a promotion target.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use tea_harness::{CandidateHypothesis, HarnessSurface};
use tea_session::{
    ArtifactId, CanonicalHashWriter, Digest, ExperimentId, FailureSignatureId, HarnessCandidateId,
    HarnessSnapshotId, ModelHarnessProfileId,
};

mod store;

pub use store::{
    EvolutionStore, EvolutionStoreError, GlobalProfilePointerV1, GlobalProfileTransitionActionV1,
    GlobalProfileTransitionV1,
};

/// Schema version for immutable experiment and campaign records.
pub const EVOLUTION_SCHEMA_VERSION: u16 = 1;

/// Error at the frozen evolution-control boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvolutionError {
    /// A supplied value would make the experiment ambiguous or unbounded.
    Invalid { message: String },
    /// A proposal or result did not belong to the current frozen experiment.
    Foreign { message: String },
    /// A promotion precondition was not met.
    PromotionDenied { message: String },
}

impl EvolutionError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }
}

impl fmt::Display for EvolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { message }
            | Self::Foreign { message }
            | Self::PromotionDenied { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EvolutionError {}

/// Frozen upper bounds for proposal search and evaluation work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchBudget {
    /// Maximum addressable candidates in this campaign.
    pub maximum_candidates: u32,
    /// Maximum provider requests consumed while searching/evaluating.
    pub maximum_provider_requests: u64,
    /// Maximum immutable artifact bytes retained for search evidence.
    pub maximum_artifact_bytes: u64,
}

impl SearchBudget {
    /// Validate nonzero, explicit campaign ceilings.
    pub fn validate(&self) -> Result<(), EvolutionError> {
        if self.maximum_candidates == 0
            || self.maximum_provider_requests == 0
            || self.maximum_artifact_bytes == 0
        {
            return Err(EvolutionError::invalid(
                "search budget ceilings must all be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Frozen serving limits used to reject an otherwise accurate but impractical
/// harness candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServingBudget {
    /// Maximum provider requests per task attempt.
    pub maximum_provider_requests_per_task: u32,
    /// Maximum durable model-visible bytes per request.
    pub maximum_context_bytes: u64,
    /// Maximum immutable plugin source bytes in the serving snapshot.
    pub maximum_plugin_source_bytes: u64,
}

impl ServingBudget {
    /// Validate nonzero, explicit serving ceilings.
    pub fn validate(&self) -> Result<(), EvolutionError> {
        if self.maximum_provider_requests_per_task == 0
            || self.maximum_context_bytes == 0
            || self.maximum_plugin_source_bytes == 0
        {
            return Err(EvolutionError::invalid(
                "serving budget ceilings must all be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Build/environment identity frozen before any target-agent work begins.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildIdentity {
    /// Exact Tea source commit supplied by the trusted runner.
    pub tea_git_commit: String,
    /// Content identity of uncommitted Tea changes, when present.
    pub tea_dirty_patch_digest: Option<Digest>,
    /// Rust compiler version selected by the runner.
    pub rust_version: String,
    /// Operating-system identity.
    pub operating_system: String,
    /// CPU architecture identity.
    pub architecture: String,
    /// Provider adapter implementation identity.
    pub provider_adapter_version: String,
    /// Requested model identifier.
    pub requested_model: String,
    /// Returned provider model/revision metadata, when exposed.
    pub returned_model_revision: Option<String>,
    /// Workspace source revision supplied to each fresh task workspace.
    pub workspace_commit: String,
    /// Content identity of initial workspace changes, when present.
    pub workspace_dirty_patch_digest: Option<Digest>,
}

impl BuildIdentity {
    fn validate(&self) -> Result<(), EvolutionError> {
        for (name, value) in [
            ("tea_git_commit", self.tea_git_commit.as_str()),
            ("rust_version", self.rust_version.as_str()),
            ("operating_system", self.operating_system.as_str()),
            ("architecture", self.architecture.as_str()),
            (
                "provider_adapter_version",
                self.provider_adapter_version.as_str(),
            ),
            ("requested_model", self.requested_model.as_str()),
            ("workspace_commit", self.workspace_commit.as_str()),
        ] {
            bounded_text(name, value)?;
        }
        if let Some(value) = &self.returned_model_revision {
            bounded_text("returned_model_revision", value)?;
        }
        Ok(())
    }
}

/// Immutable campaign input lock. Every requested profile, evaluator, task
/// split, capability ceiling, budget, and build identity participates in its
/// content-derived ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExperimentLockV1 {
    /// Content-derived immutable experiment identity.
    pub experiment_id: ExperimentId,
    /// Target profiles evaluated by this campaign.
    pub target_profiles: Vec<ModelHarnessProfileId>,
    /// Profile used by an optional proposer/evolver agent.
    pub evolver_profile: ModelHarnessProfileId,
    /// Immutable initial harness snapshot.
    pub initial_harness: HarnessSnapshotId,
    /// Exact task manifest identity.
    pub task_manifest_digest: Digest,
    /// Exact split/partition manifest identity.
    pub split_manifest_digest: Digest,
    /// Exact evaluator implementation identity.
    pub evaluator_digest: Digest,
    /// Exact environment/image identity.
    pub environment_digest: Digest,
    /// Exact authority/capability ceiling identity.
    pub capability_envelope_digest: Digest,
    /// Search budget frozen before proposals.
    pub search_budget: SearchBudget,
    /// Serving budget frozen before proposals.
    pub serving_budget: ServingBudget,
    /// Exact promotion policy identity.
    pub promotion_policy_digest: Digest,
    /// Trusted runner/build identity.
    pub tea_build_identity: BuildIdentity,
}

impl ExperimentLockV1 {
    /// Construct and content-address a frozen experiment lock.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target_profiles: Vec<ModelHarnessProfileId>,
        evolver_profile: ModelHarnessProfileId,
        initial_harness: HarnessSnapshotId,
        task_manifest_digest: Digest,
        split_manifest_digest: Digest,
        evaluator_digest: Digest,
        environment_digest: Digest,
        capability_envelope_digest: Digest,
        search_budget: SearchBudget,
        serving_budget: ServingBudget,
        promotion_policy_digest: Digest,
        tea_build_identity: BuildIdentity,
    ) -> Result<Self, EvolutionError> {
        let mut lock = Self {
            experiment_id: ExperimentId::new("pending")
                .expect("fixed experiment placeholder is a valid opaque ID"),
            target_profiles,
            evolver_profile,
            initial_harness,
            task_manifest_digest,
            split_manifest_digest,
            evaluator_digest,
            environment_digest,
            capability_envelope_digest,
            search_budget,
            serving_budget,
            promotion_policy_digest,
            tea_build_identity,
        };
        lock.validate_fields()?;
        lock.experiment_id = ExperimentId::new(lock.identity_digest().to_hex())
            .map_err(|error| EvolutionError::invalid(error.to_string()))?;
        Ok(lock)
    }

    /// Verify that a decoded/stored record still has its canonical identity.
    pub fn verify_identity(&self) -> Result<(), EvolutionError> {
        self.validate_fields()?;
        let expected = self.identity_digest().to_hex();
        if self.experiment_id.as_str() != expected {
            return Err(EvolutionError::invalid(format!(
                "experiment ID {} does not match canonical frozen inputs {expected}",
                self.experiment_id
            )));
        }
        Ok(())
    }

    /// Return the canonical immutable input digest.
    pub fn identity_digest(&self) -> Digest {
        let mut writer =
            CanonicalHashWriter::new("tea-evolution-experiment-lock", EVOLUTION_SCHEMA_VERSION, 1);
        writer.u64("target_profile_count", self.target_profiles.len() as u64);
        for profile in &self.target_profiles {
            writer.string("target_profile", profile.as_str());
        }
        writer.string("evolver_profile", self.evolver_profile.as_str());
        writer.string("initial_harness", self.initial_harness.as_str());
        digest_field(&mut writer, "task_manifest", self.task_manifest_digest);
        digest_field(&mut writer, "split_manifest", self.split_manifest_digest);
        digest_field(&mut writer, "evaluator", self.evaluator_digest);
        digest_field(&mut writer, "environment", self.environment_digest);
        digest_field(
            &mut writer,
            "capability_envelope",
            self.capability_envelope_digest,
        );
        writer.u64(
            "search.maximum_candidates",
            self.search_budget.maximum_candidates as u64,
        );
        writer.u64(
            "search.maximum_provider_requests",
            self.search_budget.maximum_provider_requests,
        );
        writer.u64(
            "search.maximum_artifact_bytes",
            self.search_budget.maximum_artifact_bytes,
        );
        writer.u64(
            "serving.maximum_provider_requests_per_task",
            self.serving_budget.maximum_provider_requests_per_task as u64,
        );
        writer.u64(
            "serving.maximum_context_bytes",
            self.serving_budget.maximum_context_bytes,
        );
        writer.u64(
            "serving.maximum_plugin_source_bytes",
            self.serving_budget.maximum_plugin_source_bytes,
        );
        digest_field(
            &mut writer,
            "promotion_policy",
            self.promotion_policy_digest,
        );
        build_identity_hash(&mut writer, &self.tea_build_identity);
        writer.finish()
    }

    fn validate_fields(&self) -> Result<(), EvolutionError> {
        if self.target_profiles.is_empty() {
            return Err(EvolutionError::invalid(
                "an experiment must name at least one target profile",
            ));
        }
        let mut seen = BTreeSet::new();
        for profile in &self.target_profiles {
            if !seen.insert(profile.clone()) {
                return Err(EvolutionError::invalid(format!(
                    "experiment repeats target profile {profile}"
                )));
            }
        }
        self.search_budget.validate()?;
        self.serving_budget.validate()?;
        self.tea_build_identity.validate()
    }
}

/// Verifier-grounded terminal category for a failure cluster.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum VerifierFailureCode {
    /// A deterministic contract failed.
    Contract,
    /// A task verifier rejected the submitted implementation.
    TaskVerifier,
    /// A provider/transport failure was classified separately from model work.
    Provider,
    /// A host/harness failure occurred.
    Harness,
    /// A trusted evaluator-defined stable category.
    Other(String),
}

/// Whether evidence supports a causal claim rather than merely correlation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CausalStatus {
    /// Only an observed association is known.
    Observed,
    /// A replay or contrast supports the proposed mechanism.
    Supported,
    /// The claim is contradicted by trusted evidence.
    Contradicted,
    /// The runner could not establish a causal classification.
    Unknown,
}

/// Where in the target behavior the failure was located.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FailureLocus {
    TaskUnderstanding,
    RepositoryDiscovery,
    ContextRetrieval,
    ToolSelection,
    ToolArguments,
    ToolExecution,
    ToolResultInterpretation,
    Implementation,
    Verification,
    FailureRecovery,
    Memory,
    Compaction,
    Termination,
    HarnessRuntime,
}

/// Normalized explanation category owned by a trusted evaluator.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MechanismCode(String);

impl MechanismCode {
    /// Construct a bounded portable evaluator category.
    pub fn new(value: impl Into<String>) -> Result<Self, EvolutionError> {
        let value = value.into();
        portable_label("mechanism code", &value)?;
        Ok(Self(value))
    }

    /// Borrow the stable category spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded confidence stated by the trusted evidence pipeline.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EvidenceConfidence {
    Low,
    Moderate,
    High,
}

/// Whether the failure is plausibly addressable by an editable policy.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Addressability {
    LuauPolicy,
    HostSubstrate,
    RustCoreGap,
    TaskSpecific,
    ModelCapabilityLimit,
    UnstableOrUnknown,
}

/// Bounded immutable span reference into a redacted trace artifact.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TraceSpanRef {
    /// Immutable redacted trace artifact.
    pub trace_artifact: ArtifactId,
    /// Inclusive byte offset within that artifact.
    pub start_byte: u64,
    /// Exclusive byte offset within that artifact.
    pub end_byte: u64,
}

impl TraceSpanRef {
    /// Construct a nonempty bounded range.
    pub fn new(
        trace_artifact: ArtifactId,
        start_byte: u64,
        end_byte: u64,
    ) -> Result<Self, EvolutionError> {
        if start_byte >= end_byte {
            return Err(EvolutionError::invalid(
                "trace evidence range must have a positive length",
            ));
        }
        Ok(Self {
            trace_artifact,
            start_byte,
            end_byte,
        })
    }
}

/// Immutable, verifier-grounded failure-signature record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureSignatureV1 {
    /// Content-derived immutable cluster identity.
    pub id: FailureSignatureId,
    pub terminal_cause: VerifierFailureCode,
    pub causal_status: CausalStatus,
    pub locus: FailureLocus,
    pub mechanism: MechanismCode,
    pub evidence: Vec<TraceSpanRef>,
    pub confidence: EvidenceConfidence,
    pub addressability: Addressability,
}

impl FailureSignatureV1 {
    /// Construct and content-address a failure signature.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        terminal_cause: VerifierFailureCode,
        causal_status: CausalStatus,
        locus: FailureLocus,
        mechanism: MechanismCode,
        evidence: Vec<TraceSpanRef>,
        confidence: EvidenceConfidence,
        addressability: Addressability,
    ) -> Result<Self, EvolutionError> {
        if evidence.is_empty() {
            return Err(EvolutionError::invalid(
                "failure signatures require at least one trace-span citation",
            ));
        }
        let mut signature = Self {
            id: FailureSignatureId::new("pending")
                .expect("fixed signature placeholder is a valid opaque ID"),
            terminal_cause,
            causal_status,
            locus,
            mechanism,
            evidence,
            confidence,
            addressability,
        };
        signature.normalize_evidence();
        signature.id = FailureSignatureId::new(signature.identity_digest().to_hex())
            .map_err(|error| EvolutionError::invalid(error.to_string()))?;
        Ok(signature)
    }

    /// Verify the canonical content-derived identity.
    pub fn verify_identity(&self) -> Result<(), EvolutionError> {
        if self.evidence.is_empty() {
            return Err(EvolutionError::invalid(
                "failure signatures require at least one trace-span citation",
            ));
        }
        let expected = self.identity_digest().to_hex();
        if self.id.as_str() != expected {
            return Err(EvolutionError::invalid(format!(
                "failure signature ID {} does not match canonical evidence {expected}",
                self.id
            )));
        }
        Ok(())
    }

    fn normalize_evidence(&mut self) {
        self.evidence.sort();
        self.evidence.dedup();
    }

    fn identity_digest(&self) -> Digest {
        let mut writer = CanonicalHashWriter::new(
            "tea-evolution-failure-signature",
            EVOLUTION_SCHEMA_VERSION,
            1,
        );
        writer.string("terminal_cause", verifier_code_name(&self.terminal_cause));
        writer.discriminant("causal_status", self.causal_status as u16);
        writer.discriminant("locus", self.locus as u16);
        writer.string("mechanism", self.mechanism.as_str());
        writer.discriminant("confidence", self.confidence as u16);
        writer.discriminant("addressability", self.addressability as u16);
        let mut evidence = self.evidence.clone();
        evidence.sort();
        writer.u64("evidence_count", evidence.len() as u64);
        for span in evidence {
            writer.bytes("trace_artifact", span.trace_artifact.digest().as_bytes());
            writer.u64("start_byte", span.start_byte);
            writer.u64("end_byte", span.end_byte);
        }
        writer.finish()
    }
}

/// Source of a candidate proposal. It is descriptive only; it grants no
/// authority to accept an evaluation or promote a profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalAuthor {
    Model,
    Operator,
}

/// Minimal, evidence-cited candidate proposal accepted into a frozen
/// campaign's retained lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateProposalV1 {
    /// Immutable candidate object staged in the harness repository.
    pub candidate_id: HarnessCandidateId,
    /// Exact parent snapshot evaluated as the paired baseline.
    pub parent_snapshot_id: HarnessSnapshotId,
    /// Exact proposed snapshot evaluated as the candidate.
    pub proposed_snapshot_id: HarnessSnapshotId,
    /// Profile this proposal targets; promotion does not imply transfer.
    pub target_profile: ModelHarnessProfileId,
    /// Origin of the proposal.
    pub author: ProposalAuthor,
    /// Required behavior/evidence/risk claim from immutable harness lineage.
    pub hypothesis: CandidateHypothesis,
    /// Trace-grounded recurrent failures this proposal addresses.
    pub failure_signatures: Vec<FailureSignatureId>,
    /// Explicit model/host surfaces changed by the candidate.
    pub changed_surfaces: BTreeSet<HarnessSurface>,
    /// Candidate's declared provider-visible overhead delta in bytes.
    pub provider_surface_byte_delta: i64,
}

impl CandidateProposalV1 {
    /// Validate that a candidate is specific enough for retained lineage.
    pub fn validate(&self) -> Result<(), EvolutionError> {
        if self.parent_snapshot_id == self.proposed_snapshot_id {
            return Err(EvolutionError::invalid(
                "candidate proposal must name a snapshot different from its parent",
            ));
        }
        if self.hypothesis.targeted_evidence.trim().is_empty()
            || self.hypothesis.expected_effect.trim().is_empty()
            || self.hypothesis.regression_risk.trim().is_empty()
        {
            return Err(EvolutionError::invalid(
                "candidate proposal must retain a complete evidence/effect/risk hypothesis",
            ));
        }
        if self.failure_signatures.is_empty() {
            return Err(EvolutionError::invalid(
                "candidate proposal must cite at least one failure signature",
            ));
        }
        if self.changed_surfaces.is_empty() {
            return Err(EvolutionError::invalid(
                "candidate proposal must name its changed surfaces",
            ));
        }
        Ok(())
    }
}

/// Ordered validation gates required before a candidate can be considered for
/// global profile promotion.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CandidateGate {
    StaticValidity,
    DeterministicContracts,
    TraceReplay,
    TargetedDiagnostics,
    ReplayAndRetention,
    PairedPromotionValidation,
    CompositeValidation,
    Canary,
}

impl CandidateGate {
    /// Return every required gate in execution-independent canonical order.
    pub const fn all() -> [Self; 8] {
        [
            Self::StaticValidity,
            Self::DeterministicContracts,
            Self::TraceReplay,
            Self::TargetedDiagnostics,
            Self::ReplayAndRetention,
            Self::PairedPromotionValidation,
            Self::CompositeValidation,
            Self::Canary,
        ]
    }
}

/// Trusted gate result that never embeds a prompt, completion, source file,
/// or raw tool data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateResult {
    /// Gate passed all hard requirements.
    pub passed: bool,
    /// Stable evaluator result digest.
    pub evidence_digest: Digest,
    /// Bounded summary suitable for campaign inspection.
    pub summary: String,
}

impl GateResult {
    /// Validate bounded operator-visible output.
    pub fn validate(&self) -> Result<(), EvolutionError> {
        bounded_text("gate summary", &self.summary)
    }
}

/// Aggregate comparison evidence for a candidate relative to its paired
/// parent baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateMetrics {
    /// Number of paired task attempts where candidate succeeded and parent did not.
    pub material_improvements: u32,
    /// Number of paired task attempts where parent succeeded and candidate did not.
    pub regressions: u32,
    /// Deterministic hard-contract regressions.
    pub hard_contract_regressions: u32,
    /// Any authority/capability expansion detected by a trusted validator.
    pub capability_expansion: bool,
    /// Search/evaluation provider-request cost charged to this candidate.
    pub search_provider_requests: u64,
    /// Serving provider requests per task in the selected condition.
    pub serving_provider_requests_per_task: u64,
    /// Serving model-visible context bytes per request.
    pub serving_context_bytes: u64,
    /// Serving immutable plugin source bytes.
    pub plugin_source_bytes: u64,
}

impl CandidateMetrics {
    /// Return the observed paired regression probability. Zero comparisons are
    /// not treated as evidence of safety.
    pub fn regression_probability(&self) -> Option<(u32, u32)> {
        let total = self.material_improvements.saturating_add(self.regressions);
        (total > 0).then_some((self.regressions, total))
    }
}

/// Complete trusted evaluation record for one candidate under one frozen lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateEvaluationV1 {
    /// Campaign to which all evidence belongs.
    pub experiment_id: ExperimentId,
    /// Candidate evaluated by the trusted runner.
    pub candidate_id: HarnessCandidateId,
    /// Result for every required gate.
    pub gates: BTreeMap<CandidateGate, GateResult>,
    /// Aggregate paired correctness/cost/authority metrics.
    pub metrics: CandidateMetrics,
}

impl CandidateEvaluationV1 {
    /// Reject incomplete or malformed result records before campaign state can
    /// retain them as a promotion candidate.
    pub fn validate(&self) -> Result<(), EvolutionError> {
        for gate in CandidateGate::all() {
            let result = self.gates.get(&gate).ok_or_else(|| {
                EvolutionError::invalid("candidate evaluation is missing a required gate")
            })?;
            result.validate()?;
        }
        Ok(())
    }

    /// Return whether every required hard gate passed.
    pub fn all_gates_passed(&self) -> bool {
        CandidateGate::all()
            .into_iter()
            .all(|gate| self.gates.get(&gate).is_some_and(|result| result.passed))
    }
}

/// Frozen policy used by the campaign to decide promotion eligibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromotionPolicy {
    /// At least this many paired material improvements are required.
    pub minimum_material_improvements: u32,
    /// Numerator/denominator upper bound for paired regressions.
    pub maximum_regression_numerator: u32,
    pub maximum_regression_denominator: u32,
    /// Maximum search provider requests consumed by one candidate.
    pub maximum_search_provider_requests: u64,
}

impl PromotionPolicy {
    /// Validate a non-degenerate statistical guardrail.
    pub fn validate(&self) -> Result<(), EvolutionError> {
        if self.maximum_regression_denominator == 0
            || self.maximum_regression_numerator > self.maximum_regression_denominator
        {
            return Err(EvolutionError::invalid(
                "promotion policy has an invalid regression probability bound",
            ));
        }
        Ok(())
    }

    /// Return a fail-closed promotion decision for one fully evaluated candidate.
    pub fn decide(
        &self,
        lock: &ExperimentLockV1,
        evaluation: &CandidateEvaluationV1,
    ) -> Result<PromotionDecision, EvolutionError> {
        self.validate()?;
        evaluation.validate()?;
        if evaluation.experiment_id != lock.experiment_id {
            return Err(EvolutionError::Foreign {
                message: "candidate evaluation belongs to another experiment".into(),
            });
        }
        let metrics = &evaluation.metrics;
        let mut reasons = Vec::new();
        if !evaluation.all_gates_passed() {
            reasons.push("one or more required evaluation gates failed".into());
        }
        if metrics.hard_contract_regressions != 0 {
            reasons.push("candidate has deterministic contract regressions".into());
        }
        if metrics.capability_expansion {
            reasons.push("candidate expands the frozen capability envelope".into());
        }
        if metrics.material_improvements < self.minimum_material_improvements {
            reasons.push("candidate has no required material paired improvement".into());
        }
        if metrics.search_provider_requests > self.maximum_search_provider_requests
            || metrics.search_provider_requests > lock.search_budget.maximum_provider_requests
        {
            reasons.push("candidate exceeds frozen search budget".into());
        }
        if metrics.serving_provider_requests_per_task
            > u64::from(lock.serving_budget.maximum_provider_requests_per_task)
            || metrics.serving_context_bytes > lock.serving_budget.maximum_context_bytes
            || metrics.plugin_source_bytes > lock.serving_budget.maximum_plugin_source_bytes
        {
            reasons.push("candidate exceeds frozen serving budget".into());
        }
        let Some((regressions, total)) = metrics.regression_probability() else {
            reasons.push("candidate has no paired comparison evidence".into());
            return Ok(PromotionDecision::rejected(reasons));
        };
        if u128::from(regressions) * u128::from(self.maximum_regression_denominator)
            > u128::from(total) * u128::from(self.maximum_regression_numerator)
        {
            reasons.push("paired regression probability exceeds the frozen limit".into());
        }
        if reasons.is_empty() {
            Ok(PromotionDecision::Promotable)
        } else {
            Ok(PromotionDecision::rejected(reasons))
        }
    }
}

/// Non-authoritative evaluation conclusion. An operator still explicitly
/// selects a promotable candidate; models receive no method that mutates this
/// campaign state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromotionDecision {
    Promotable,
    Rejected { reasons: Vec<String> },
}

impl PromotionDecision {
    fn rejected(reasons: Vec<String>) -> Self {
        Self::Rejected { reasons }
    }

    /// Return whether the decision meets every frozen guardrail.
    pub const fn is_promotable(&self) -> bool {
        matches!(self, Self::Promotable)
    }
}

/// Explicit capability required to choose an active global profile pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromotionAuthority {
    Operator,
}

/// One retained campaign snapshot. [`EvolutionStore`] persists every accepted
/// transition atomically while this type remains a pure policy boundary.
#[derive(Clone, Debug)]
pub struct Campaign {
    lock: ExperimentLockV1,
    policy: PromotionPolicy,
    proposals: BTreeMap<HarnessCandidateId, CandidateProposalV1>,
    evaluations: BTreeMap<HarnessCandidateId, CandidateEvaluationV1>,
    champion: Option<HarnessCandidateId>,
    pareto_frontier: BTreeSet<HarnessCandidateId>,
}

impl Campaign {
    /// Start an empty campaign under immutable inputs and frozen policy.
    pub fn new(lock: ExperimentLockV1, policy: PromotionPolicy) -> Result<Self, EvolutionError> {
        lock.verify_identity()?;
        policy.validate()?;
        Ok(Self {
            lock,
            policy,
            proposals: BTreeMap::new(),
            evaluations: BTreeMap::new(),
            champion: None,
            pareto_frontier: BTreeSet::new(),
        })
    }

    /// Borrow frozen experiment inputs.
    pub fn lock(&self) -> &ExperimentLockV1 {
        &self.lock
    }

    /// Stage a proposal without evaluating or activating it.
    pub fn stage(&mut self, proposal: CandidateProposalV1) -> Result<(), EvolutionError> {
        proposal.validate()?;
        if !self.lock.target_profiles.contains(&proposal.target_profile) {
            return Err(EvolutionError::Foreign {
                message: "candidate targets a profile outside the frozen experiment lock".into(),
            });
        }
        if self.proposals.contains_key(&proposal.candidate_id) {
            return Err(EvolutionError::invalid(
                "candidate is already retained in this campaign",
            ));
        }
        if self.proposals.len() >= self.lock.search_budget.maximum_candidates as usize {
            return Err(EvolutionError::invalid(
                "campaign exhausted its frozen candidate-count budget",
            ));
        }
        self.proposals
            .insert(proposal.candidate_id.clone(), proposal);
        Ok(())
    }

    /// Record a complete trusted evaluation. Rejected candidates remain in
    /// `proposals` and are deliberately not discarded from lineage.
    pub fn record_evaluation(
        &mut self,
        evaluation: CandidateEvaluationV1,
    ) -> Result<PromotionDecision, EvolutionError> {
        evaluation.validate()?;
        if evaluation.experiment_id != self.lock.experiment_id {
            return Err(EvolutionError::Foreign {
                message: "evaluation belongs to another experiment".into(),
            });
        }
        if !self.proposals.contains_key(&evaluation.candidate_id) {
            return Err(EvolutionError::Foreign {
                message: "evaluation names a candidate not retained by this campaign".into(),
            });
        }
        if self.evaluations.contains_key(&evaluation.candidate_id) {
            return Err(EvolutionError::invalid(
                "candidate already has an immutable evaluation in this campaign",
            ));
        }
        let decision = self.policy.decide(&self.lock, &evaluation)?;
        self.evaluations
            .insert(evaluation.candidate_id.clone(), evaluation);
        self.rebuild_pareto_frontier();
        Ok(decision)
    }

    /// Select one already evaluated, promotable candidate. This does not
    /// mutate a global profile pointer; callers persist the returned identity
    /// through a separate operator-controlled promotion transaction.
    pub fn select_champion(
        &mut self,
        authority: PromotionAuthority,
        candidate_id: &HarnessCandidateId,
    ) -> Result<(), EvolutionError> {
        let PromotionAuthority::Operator = authority;
        let evaluation =
            self.evaluations
                .get(candidate_id)
                .ok_or_else(|| EvolutionError::PromotionDenied {
                    message: "candidate has no complete frozen evaluation".into(),
                })?;
        let decision = self.policy.decide(&self.lock, evaluation)?;
        if !decision.is_promotable() {
            return Err(EvolutionError::PromotionDenied {
                message: "candidate did not satisfy frozen promotion gates".into(),
            });
        }
        self.champion = Some(candidate_id.clone());
        Ok(())
    }

    /// Return the operator-selected candidate, if one exists.
    pub fn champion(&self) -> Option<&HarnessCandidateId> {
        self.champion.as_ref()
    }

    /// Return the bounded nondominated evaluated-candidate set.
    pub fn pareto_frontier(&self) -> &BTreeSet<HarnessCandidateId> {
        &self.pareto_frontier
    }

    /// Borrow retained candidate lineage, including rejected candidates.
    pub fn proposals(&self) -> &BTreeMap<HarnessCandidateId, CandidateProposalV1> {
        &self.proposals
    }

    fn rebuild_pareto_frontier(&mut self) {
        self.pareto_frontier.clear();
        for (candidate_id, evaluation) in &self.evaluations {
            let dominated = self.evaluations.iter().any(|(other_id, other)| {
                other_id != candidate_id && dominates(&other.metrics, &evaluation.metrics)
            });
            if !dominated {
                self.pareto_frontier.insert(candidate_id.clone());
            }
        }
    }
}

/// Compare amortized evolution cost with a compute-matched serving baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AmortizationCost {
    pub search_cost: u64,
    pub evolved_serving_cost: u64,
    pub baseline_serving_cost: u64,
}

impl AmortizationCost {
    /// Return total evolved and baseline cost at `task_count` tasks.
    pub fn totals(self, task_count: u64) -> (u64, u64) {
        (
            self.search_cost
                .saturating_add(task_count.saturating_mul(self.evolved_serving_cost)),
            task_count.saturating_mul(self.baseline_serving_cost),
        )
    }

    /// Return the first whole task count at which evolved total cost is no
    /// greater than baseline, or `None` when it cannot break even.
    pub fn break_even_task_count(self) -> Option<u64> {
        if self.evolved_serving_cost >= self.baseline_serving_cost {
            return (self.search_cost == 0).then_some(0);
        }
        let saving = self.baseline_serving_cost - self.evolved_serving_cost;
        Some(self.search_cost.saturating_add(saving - 1) / saving)
    }
}

fn dominates(left: &CandidateMetrics, right: &CandidateMetrics) -> bool {
    let no_worse = left.material_improvements >= right.material_improvements
        && left.regressions <= right.regressions
        && left.search_provider_requests <= right.search_provider_requests
        && left.serving_provider_requests_per_task <= right.serving_provider_requests_per_task
        && left.serving_context_bytes <= right.serving_context_bytes
        && left.plugin_source_bytes <= right.plugin_source_bytes
        && left.hard_contract_regressions <= right.hard_contract_regressions
        && !left.capability_expansion;
    let strictly_better = left.material_improvements > right.material_improvements
        || left.regressions < right.regressions
        || left.search_provider_requests < right.search_provider_requests
        || left.serving_provider_requests_per_task < right.serving_provider_requests_per_task
        || left.serving_context_bytes < right.serving_context_bytes
        || left.plugin_source_bytes < right.plugin_source_bytes
        || left.hard_contract_regressions < right.hard_contract_regressions
        || (right.capability_expansion && !left.capability_expansion);
    no_worse && strictly_better
}

fn bounded_text(name: &str, value: &str) -> Result<(), EvolutionError> {
    if value.is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
        return Err(EvolutionError::invalid(format!(
            "{name} must be nonempty bounded non-control text"
        )));
    }
    Ok(())
}

fn portable_label(name: &str, value: &str) -> Result<(), EvolutionError> {
    if value.is_empty()
        || value.len() > 120
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(EvolutionError::invalid(format!(
            "{name} must use [A-Za-z0-9._-] and be at most 120 bytes"
        )));
    }
    Ok(())
}

fn digest_field(writer: &mut CanonicalHashWriter, name: &str, digest: Digest) {
    writer.bytes(name, digest.as_bytes());
}

fn optional_digest_field(writer: &mut CanonicalHashWriter, name: &str, digest: Option<Digest>) {
    writer.boolean(&format!("{name}.present"), digest.is_some());
    if let Some(digest) = digest {
        digest_field(writer, name, digest);
    }
}

fn build_identity_hash(writer: &mut CanonicalHashWriter, identity: &BuildIdentity) {
    writer.string("build.tea_git_commit", &identity.tea_git_commit);
    optional_digest_field(
        writer,
        "build.tea_dirty_patch",
        identity.tea_dirty_patch_digest,
    );
    writer.string("build.rust_version", &identity.rust_version);
    writer.string("build.operating_system", &identity.operating_system);
    writer.string("build.architecture", &identity.architecture);
    writer.string(
        "build.provider_adapter_version",
        &identity.provider_adapter_version,
    );
    writer.string("build.requested_model", &identity.requested_model);
    writer.boolean(
        "build.returned_model_revision.present",
        identity.returned_model_revision.is_some(),
    );
    if let Some(revision) = &identity.returned_model_revision {
        writer.string("build.returned_model_revision", revision);
    }
    writer.string("build.workspace_commit", &identity.workspace_commit);
    optional_digest_field(
        writer,
        "build.workspace_dirty_patch",
        identity.workspace_dirty_patch_digest,
    );
}

fn verifier_code_name(code: &VerifierFailureCode) -> &str {
    match code {
        VerifierFailureCode::Contract => "contract",
        VerifierFailureCode::TaskVerifier => "task_verifier",
        VerifierFailureCode::Provider => "provider",
        VerifierFailureCode::Harness => "harness",
        VerifierFailureCode::Other(value) => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id<T>(value: &str, construct: impl FnOnce(String) -> Result<T, tea_session::IdError>) -> T {
        construct(value.into()).expect("fixture opaque ID")
    }

    fn lock() -> ExperimentLockV1 {
        ExperimentLockV1::new(
            vec![id("target-profile", ModelHarnessProfileId::new)],
            id("evolver-profile", ModelHarnessProfileId::new),
            id("initial-snapshot", HarnessSnapshotId::new),
            Digest::from_bytes("tasks"),
            Digest::from_bytes("splits"),
            Digest::from_bytes("evaluator"),
            Digest::from_bytes("environment"),
            Digest::from_bytes("capabilities"),
            SearchBudget {
                maximum_candidates: 3,
                maximum_provider_requests: 100,
                maximum_artifact_bytes: 100_000,
            },
            ServingBudget {
                maximum_provider_requests_per_task: 4,
                maximum_context_bytes: 10_000,
                maximum_plugin_source_bytes: 20_000,
            },
            Digest::from_bytes("promotion-policy"),
            BuildIdentity {
                tea_git_commit: "abc123".into(),
                tea_dirty_patch_digest: None,
                rust_version: "rustc fixture".into(),
                operating_system: "fixture-os".into(),
                architecture: "fixture-arch".into(),
                provider_adapter_version: "fixture-provider-v1".into(),
                requested_model: "fixture-model".into(),
                returned_model_revision: None,
                workspace_commit: "workspace-abc".into(),
                workspace_dirty_patch_digest: None,
            },
        )
        .expect("fixture lock")
    }

    fn signature() -> FailureSignatureV1 {
        FailureSignatureV1::new(
            VerifierFailureCode::TaskVerifier,
            CausalStatus::Supported,
            FailureLocus::ToolArguments,
            MechanismCode::new("wrong_argument_name").expect("mechanism"),
            vec![TraceSpanRef::new(ArtifactId::from_bytes("trace"), 1, 4).expect("trace span")],
            EvidenceConfidence::High,
            Addressability::LuauPolicy,
        )
        .expect("signature")
    }

    fn proposal(signature: FailureSignatureId) -> CandidateProposalV1 {
        CandidateProposalV1 {
            candidate_id: id("candidate", HarnessCandidateId::new),
            parent_snapshot_id: id("parent-snapshot", HarnessSnapshotId::new),
            proposed_snapshot_id: id("candidate-snapshot", HarnessSnapshotId::new),
            target_profile: id("target-profile", ModelHarnessProfileId::new),
            author: ProposalAuthor::Model,
            hypothesis: CandidateHypothesis {
                targeted_evidence: "wrong field names recur in verified traces".into(),
                expected_effect: "canonical field names are selected".into(),
                regression_risk: "normalization could hide a real mismatch".into(),
            },
            failure_signatures: vec![signature],
            changed_surfaces: [HarnessSurface::Hooks].into_iter().collect(),
            provider_surface_byte_delta: 0,
        }
    }

    fn complete_evaluation(
        lock: &ExperimentLockV1,
        candidate_id: HarnessCandidateId,
    ) -> CandidateEvaluationV1 {
        CandidateEvaluationV1 {
            experiment_id: lock.experiment_id.clone(),
            candidate_id,
            gates: CandidateGate::all()
                .into_iter()
                .map(|gate| {
                    (
                        gate,
                        GateResult {
                            passed: true,
                            evidence_digest: Digest::from_bytes(format!("{gate:?}")),
                            summary: "verified".into(),
                        },
                    )
                })
                .collect(),
            metrics: CandidateMetrics {
                material_improvements: 2,
                regressions: 0,
                hard_contract_regressions: 0,
                capability_expansion: false,
                search_provider_requests: 10,
                serving_provider_requests_per_task: 2,
                serving_context_bytes: 500,
                plugin_source_bytes: 1_000,
            },
        }
    }

    #[test]
    fn lock_and_failure_signatures_are_content_addressed() {
        let lock = lock();
        lock.verify_identity().expect("canonical lock identity");
        let signature = signature();
        signature
            .verify_identity()
            .expect("canonical signature identity");

        let changed = ExperimentLockV1::new(
            lock.target_profiles.clone(),
            lock.evolver_profile.clone(),
            lock.initial_harness.clone(),
            Digest::from_bytes("different tasks"),
            lock.split_manifest_digest,
            lock.evaluator_digest,
            lock.environment_digest,
            lock.capability_envelope_digest,
            lock.search_budget.clone(),
            lock.serving_budget.clone(),
            lock.promotion_policy_digest,
            lock.tea_build_identity.clone(),
        )
        .expect("changed lock");
        assert_ne!(lock.experiment_id, changed.experiment_id);
    }

    #[test]
    fn campaign_retains_rejected_lineage_and_requires_operator_selection() {
        let lock = lock();
        let policy = PromotionPolicy {
            minimum_material_improvements: 1,
            maximum_regression_numerator: 0,
            maximum_regression_denominator: 1,
            maximum_search_provider_requests: 20,
        };
        let mut campaign = Campaign::new(lock.clone(), policy).expect("campaign");
        let proposal = proposal(signature().id);
        let candidate_id = proposal.candidate_id.clone();
        campaign.stage(proposal).expect("proposal stages");
        let decision = campaign
            .record_evaluation(complete_evaluation(&lock, candidate_id.clone()))
            .expect("evaluation records");
        assert!(decision.is_promotable());
        assert!(campaign.champion().is_none());
        campaign
            .select_champion(PromotionAuthority::Operator, &candidate_id)
            .expect("only explicit operator selection chooses champion");
        assert_eq!(campaign.champion(), Some(&candidate_id));
        assert!(campaign.proposals().contains_key(&candidate_id));
    }

    #[test]
    fn promotion_fails_closed_on_missing_gate_or_authority_expansion() {
        let lock = lock();
        let policy = PromotionPolicy {
            minimum_material_improvements: 1,
            maximum_regression_numerator: 0,
            maximum_regression_denominator: 1,
            maximum_search_provider_requests: 20,
        };
        let candidate_id = id("candidate", HarnessCandidateId::new);
        let mut evaluation = complete_evaluation(&lock, candidate_id);
        evaluation.gates.remove(&CandidateGate::Canary);
        assert!(matches!(
            policy.decide(&lock, &evaluation),
            Err(EvolutionError::Invalid { .. })
        ));

        let candidate_id = id("candidate-expanded", HarnessCandidateId::new);
        let mut evaluation = complete_evaluation(&lock, candidate_id);
        evaluation.metrics.capability_expansion = true;
        assert!(matches!(
            policy.decide(&lock, &evaluation),
            Ok(PromotionDecision::Rejected { .. })
        ));
    }

    #[test]
    fn amortization_never_claims_break_even_when_serving_is_not_cheaper() {
        assert_eq!(
            AmortizationCost {
                search_cost: 100,
                evolved_serving_cost: 8,
                baseline_serving_cost: 10,
            }
            .break_even_task_count(),
            Some(50)
        );
        assert_eq!(
            AmortizationCost {
                search_cost: 100,
                evolved_serving_cost: 10,
                baseline_serving_cost: 10,
            }
            .break_even_task_count(),
            None
        );
    }
}
