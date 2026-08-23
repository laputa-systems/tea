//! Durable v1 storage for frozen evolution campaigns.
//!
//! The evolution control plane is intentionally separate from a session's
//! effect log, but it must retain the same evidence discipline: accepted
//! state changes are atomically persisted, every cited trace is verified on
//! open, and global profile changes retain an explicit rollback lineage.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tea_session::{ArtifactError, ArtifactStore, JsonValue};

const STORE_FILE: &str = "campaign.json";
const STORE_KIND: &str = "tea_evolution_campaign";
const STORE_SCHEMA_VERSION: u16 = 1;

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);

/// The active immutable snapshot selected for one global model-harness
/// profile. It is an operator-controlled pointer, never an activation side
/// effect of candidate evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalProfilePointerV1 {
    /// Profile whose global harness selection changed.
    pub profile: ModelHarnessProfileId,
    /// Frozen campaign that produced the selected candidate.
    pub experiment_id: ExperimentId,
    /// Operator-selected, promotable candidate.
    pub candidate_id: HarnessCandidateId,
    /// Exact immutable harness snapshot selected by that candidate.
    pub snapshot_id: HarnessSnapshotId,
}

/// One explicit global-profile state transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlobalProfileTransitionActionV1 {
    /// An operator selected the campaign champion for a profile.
    Promotion,
    /// An operator restored the immediately preceding profile selection.
    Rollback,
}

/// Durable lineage for a global profile pointer. `current: None` means a
/// rollback restored the pre-promotion state where no global pointer existed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalProfileTransitionV1 {
    /// Monotonic campaign-local transition sequence, beginning at one.
    pub sequence: u64,
    /// The explicit operator action.
    pub action: GlobalProfileTransitionActionV1,
    /// Profile affected by this transition.
    pub profile: ModelHarnessProfileId,
    /// Pointer immediately before the transition.
    pub previous: Option<GlobalProfilePointerV1>,
    /// Pointer immediately after the transition.
    pub current: Option<GlobalProfilePointerV1>,
}

/// Durable evolution-store failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvolutionStoreError {
    /// The frozen control-plane contract rejected a requested transition.
    Evolution(EvolutionError),
    /// A cited immutable trace artifact was absent or corrupt.
    Artifact(ArtifactError),
    /// Filesystem storage failed at a concrete path.
    Io { path: String, message: String },
    /// Persisted state was malformed, incomplete, or contradicted its lineage.
    Corruption { message: String },
}

impl fmt::Display for EvolutionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Evolution(error) => write!(formatter, "evolution store rejected state: {error}"),
            Self::Artifact(error) => write!(formatter, "evolution evidence artifact failed: {error}"),
            Self::Io { path, message } => write!(formatter, "evolution store I/O failed at {path}: {message}"),
            Self::Corruption { message } => write!(formatter, "evolution store is corrupt: {message}"),
        }
    }
}

impl std::error::Error for EvolutionStoreError {}

impl From<EvolutionError> for EvolutionStoreError {
    fn from(value: EvolutionError) -> Self {
        Self::Evolution(value)
    }
}

impl From<ArtifactError> for EvolutionStoreError {
    fn from(value: ArtifactError) -> Self {
        Self::Artifact(value)
    }
}

#[derive(Clone, Debug)]
struct StoreState {
    campaign: Campaign,
    signatures: BTreeMap<FailureSignatureId, FailureSignatureV1>,
    global_profiles: BTreeMap<ModelHarnessProfileId, GlobalProfilePointerV1>,
    transitions: Vec<GlobalProfileTransitionV1>,
}

/// One single-writer durable campaign directory.
///
/// `create` refuses an existing directory and every accepted transition writes
/// an fsynced temporary snapshot before atomically replacing `campaign.json`.
/// Hosts should retain one mutable instance per root instead of opening two
/// writers for the same campaign concurrently.
pub struct EvolutionStore {
    root: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    state: StoreState,
}

impl fmt::Debug for EvolutionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EvolutionStore")
            .field("root", &self.root)
            .field("experiment_id", &self.state.campaign.lock.experiment_id)
            .field("failure_signature_count", &self.state.signatures.len())
            .field("transition_count", &self.state.transitions.len())
            .finish()
    }
}

impl EvolutionStore {
    /// Create one new empty v1 campaign directory. Existing paths are never
    /// overwritten because a caller must choose an explicit fresh root.
    pub fn create(
        root: impl AsRef<Path>,
        artifacts: Arc<dyn ArtifactStore>,
        lock: ExperimentLockV1,
        policy: PromotionPolicy,
    ) -> Result<Self, EvolutionStoreError> {
        let root = root.as_ref().to_path_buf();
        if root.exists() {
            return Err(io_error(
                &root,
                "refusing to create an evolution campaign over an existing directory",
            ));
        }
        fs::create_dir(&root).map_err(|error| io_error(&root, error))?;
        let state = StoreState {
            campaign: Campaign::new(lock, policy)?,
            signatures: BTreeMap::new(),
            global_profiles: BTreeMap::new(),
            transitions: Vec::new(),
        };
        let store = Self {
            root,
            artifacts,
            state,
        };
        store.validate_state(&store.state)?;
        store.persist(&store.state)?;
        Ok(store)
    }

    /// Reopen and completely validate one existing v1 campaign directory.
    /// Missing trace evidence, changed bytes, invalid span offsets, and any
    /// impossible campaign or global-profile transition fail closed.
    pub fn open(
        root: impl AsRef<Path>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Self, EvolutionStoreError> {
        let root = root.as_ref().to_path_buf();
        ensure_directory(&root)?;
        let path = root.join(STORE_FILE);
        ensure_regular_file(&path)?;
        let contents = fs::read_to_string(&path).map_err(|error| io_error(&path, error))?;
        let state = decode_state(&contents)?;
        let store = Self {
            root,
            artifacts,
            state,
        };
        store.validate_state(&store.state)?;
        Ok(store)
    }

    /// Return the durable directory containing `campaign.json`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Borrow the frozen campaign and its retained candidate lineage.
    pub fn campaign(&self) -> &Campaign {
        &self.state.campaign
    }

    /// Borrow registered immutable failure signatures by content-derived ID.
    pub fn failure_signatures(&self) -> &BTreeMap<FailureSignatureId, FailureSignatureV1> {
        &self.state.signatures
    }

    /// Return every trace artifact retained as campaign evidence and therefore
    /// required by backup/export/collection roots.
    pub fn artifact_roots(&self) -> BTreeSet<ArtifactId> {
        self.state
            .signatures
            .values()
            .flat_map(|signature| signature.evidence.iter().map(|span| span.trace_artifact))
            .collect()
    }

    /// Register a content-addressed failure signature only after each cited
    /// redacted trace and byte span has been verified.
    pub fn register_failure_signature(
        &mut self,
        signature: FailureSignatureV1,
    ) -> Result<(), EvolutionStoreError> {
        signature.verify_identity()?;
        self.validate_signature_artifacts(&signature)?;
        let mut next = self.state.clone();
        match next.signatures.get(&signature.id) {
            Some(existing) if existing == &signature => return Ok(()),
            Some(_) => {
                return Err(EvolutionStoreError::Corruption {
                    message: format!(
                        "failure signature {} conflicts with an existing immutable record",
                        signature.id
                    ),
                })
            }
            None => {}
        }
        next.signatures.insert(signature.id.clone(), signature);
        self.commit(next)
    }

    /// Atomically retain a proposal after confirming that all its citations
    /// refer to registered, validated failure signatures.
    pub fn stage(&mut self, proposal: CandidateProposalV1) -> Result<(), EvolutionStoreError> {
        let mut next = self.state.clone();
        ensure_proposal_signatures(&proposal, &next.signatures)?;
        next.campaign.stage(proposal)?;
        self.commit(next)
    }

    /// Atomically retain a complete trusted candidate evaluation. Rejected
    /// candidates remain in the durable campaign lineage.
    pub fn record_evaluation(
        &mut self,
        evaluation: CandidateEvaluationV1,
    ) -> Result<PromotionDecision, EvolutionStoreError> {
        let mut next = self.state.clone();
        let decision = next.campaign.record_evaluation(evaluation)?;
        self.commit(next)?;
        Ok(decision)
    }

    /// Atomically select the campaign champion under the explicit operator
    /// capability. Selection alone does not alter a global profile pointer.
    pub fn select_champion(
        &mut self,
        authority: PromotionAuthority,
        candidate_id: &HarnessCandidateId,
    ) -> Result<(), EvolutionStoreError> {
        let mut next = self.state.clone();
        next.campaign.select_champion(authority, candidate_id)?;
        self.commit(next)
    }

    /// Atomically make the already selected campaign champion active for its
    /// target profile. The type-level `PromotionAuthority` is required even
    /// though all evaluation data was already trusted.
    pub fn promote_global(
        &mut self,
        authority: PromotionAuthority,
        candidate_id: &HarnessCandidateId,
    ) -> Result<GlobalProfilePointerV1, EvolutionStoreError> {
        let PromotionAuthority::Operator = authority;
        let mut next = self.state.clone();
        if next.campaign.champion() != Some(candidate_id) {
            return Err(EvolutionError::PromotionDenied {
                message: "global promotion requires an operator-selected campaign champion".into(),
            }
            .into());
        }
        let proposal = next
            .campaign
            .proposals
            .get(candidate_id)
            .ok_or_else(|| EvolutionStoreError::Corruption {
                message: "selected champion is absent from retained proposal lineage".into(),
            })?;
        let pointer = GlobalProfilePointerV1 {
            profile: proposal.target_profile.clone(),
            experiment_id: next.campaign.lock.experiment_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            snapshot_id: proposal.proposed_snapshot_id.clone(),
        };
        let previous = next.global_profiles.insert(pointer.profile.clone(), pointer.clone());
        let sequence = next.transitions.len() as u64 + 1;
        next.transitions.push(GlobalProfileTransitionV1 {
            sequence,
            action: GlobalProfileTransitionActionV1::Promotion,
            profile: pointer.profile.clone(),
            previous,
            current: Some(pointer.clone()),
        });
        self.commit(next)?;
        Ok(pointer)
    }

    /// Atomically restore the immediately preceding global pointer for one
    /// profile. A first promotion can therefore roll back to no active global
    /// pointer while retaining the promotion record itself as durable lineage.
    pub fn rollback_global(
        &mut self,
        authority: PromotionAuthority,
        profile: &ModelHarnessProfileId,
    ) -> Result<Option<GlobalProfilePointerV1>, EvolutionStoreError> {
        let PromotionAuthority::Operator = authority;
        let mut next = self.state.clone();
        let current = next.global_profiles.get(profile).cloned().ok_or_else(|| {
            EvolutionStoreError::Evolution(EvolutionError::PromotionDenied {
                message: "global profile has no active pointer to roll back".into(),
            })
        })?;
        let last = next
            .transitions
            .iter()
            .rev()
            .find(|transition| &transition.profile == profile)
            .ok_or_else(|| EvolutionStoreError::Corruption {
                message: "active global profile has no retained transition lineage".into(),
            })?;
        if last.current.as_ref() != Some(&current) {
            return Err(EvolutionStoreError::Corruption {
                message: "active global profile disagrees with its last retained transition".into(),
            });
        }
        let restored = last.previous.clone();
        match &restored {
            Some(pointer) => {
                next.global_profiles.insert(profile.clone(), pointer.clone());
            }
            None => {
                next.global_profiles.remove(profile);
            }
        }
        let sequence = next.transitions.len() as u64 + 1;
        next.transitions.push(GlobalProfileTransitionV1 {
            sequence,
            action: GlobalProfileTransitionActionV1::Rollback,
            profile: profile.clone(),
            previous: Some(current),
            current: restored.clone(),
        });
        self.commit(next)?;
        Ok(restored)
    }

    /// Return the current explicit global pointer for one profile, if active.
    pub fn global_profile(
        &self,
        profile: &ModelHarnessProfileId,
    ) -> Option<&GlobalProfilePointerV1> {
        self.state.global_profiles.get(profile)
    }

    /// Return the complete immutable promotion and rollback lineage.
    pub fn global_profile_transitions(&self) -> &[GlobalProfileTransitionV1] {
        &self.state.transitions
    }

    fn commit(&mut self, next: StoreState) -> Result<(), EvolutionStoreError> {
        self.validate_state(&next)?;
        self.persist(&next)?;
        self.state = next;
        Ok(())
    }

    fn persist(&self, state: &StoreState) -> Result<(), EvolutionStoreError> {
        let json = encode_state(state)
            .to_json_string_pretty()
            .map_err(|error| EvolutionStoreError::Corruption {
                message: format!("cannot encode v1 campaign state as JSON: {error}"),
            })?;
        let destination = self.root.join(STORE_FILE);
        let nonce = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = self.root.join(format!(".{STORE_FILE}.{nonce:016x}.tmp"));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary).map_err(|error| io_error(&temporary, error))?;
            file.write_all(json.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.flush())
                .map_err(|error| io_error(&temporary, error))?;
            file.sync_all().map_err(|error| io_error(&temporary, error))?;
            drop(file);
            fs::rename(&temporary, &destination).map_err(|error| io_error(&destination, error))?;
            sync_directory(&self.root)?;
            Ok(())
        })();
        if result.is_err() && temporary.exists() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn validate_state(&self, state: &StoreState) -> Result<(), EvolutionStoreError> {
        state.campaign.lock.verify_identity()?;
        state.campaign.policy.validate()?;

        for signature in state.signatures.values() {
            signature.verify_identity()?;
            self.validate_signature_artifacts(signature)?;
        }

        let mut reconstructed = Campaign::new(
            state.campaign.lock.clone(),
            state.campaign.policy.clone(),
        )?;
        for proposal in state.campaign.proposals.values() {
            ensure_proposal_signatures(proposal, &state.signatures)?;
            reconstructed.stage(proposal.clone())?;
        }
        for evaluation in state.campaign.evaluations.values() {
            reconstructed.record_evaluation(evaluation.clone())?;
        }
        if let Some(champion) = &state.campaign.champion {
            reconstructed.select_champion(PromotionAuthority::Operator, champion)?;
        }
        if reconstructed.champion != state.campaign.champion
            || reconstructed.pareto_frontier != state.campaign.pareto_frontier
        {
            return Err(EvolutionStoreError::Corruption {
                message: "stored campaign derived state does not match retained proposals and evaluations"
                    .into(),
            });
        }

        let mut profiles = BTreeMap::new();
        let mut prior_sequence = 0_u64;
        for transition in &state.transitions {
            if transition.sequence != prior_sequence + 1 {
                return Err(EvolutionStoreError::Corruption {
                    message: "global profile transitions must have contiguous monotonic sequences".into(),
                });
            }
            prior_sequence = transition.sequence;
            validate_transition_pointer(&transition.profile, transition.previous.as_ref(), state)?;
            validate_transition_pointer(&transition.profile, transition.current.as_ref(), state)?;
            let observed_previous = profiles.get(&transition.profile).cloned();
            if observed_previous != transition.previous {
                return Err(EvolutionStoreError::Corruption {
                    message: format!(
                        "global profile transition {} does not begin at its retained prior pointer",
                        transition.sequence
                    ),
                });
            }
            match transition.action {
                GlobalProfileTransitionActionV1::Promotion => {
                    if transition.current.is_none() {
                        return Err(EvolutionStoreError::Corruption {
                            message: "global promotion cannot produce an absent pointer".into(),
                        });
                    }
                }
                GlobalProfileTransitionActionV1::Rollback => {
                    if transition.previous.is_none() {
                        return Err(EvolutionStoreError::Corruption {
                            message: "global rollback requires an active pointer to replace".into(),
                        });
                    }
                }
            }
            match &transition.current {
                Some(pointer) => {
                    profiles.insert(transition.profile.clone(), pointer.clone());
                }
                None => {
                    profiles.remove(&transition.profile);
                }
            }
        }
        if profiles != state.global_profiles {
            return Err(EvolutionStoreError::Corruption {
                message: "stored global profile pointers do not match retained transition lineage".into(),
            });
        }
        Ok(())
    }

    fn validate_signature_artifacts(
        &self,
        signature: &FailureSignatureV1,
    ) -> Result<(), EvolutionStoreError> {
        for span in &signature.evidence {
            let bytes = self.artifacts.get(span.trace_artifact)?;
            if ArtifactId::from_bytes(&bytes) != span.trace_artifact {
                return Err(EvolutionStoreError::Corruption {
                    message: format!(
                        "trace artifact {} does not match its content-derived identity",
                        span.trace_artifact
                    ),
                });
            }
            if span.end_byte > bytes.len() as u64 {
                return Err(EvolutionStoreError::Corruption {
                    message: format!(
                        "trace evidence range {}..{} exceeds artifact {} length {}",
                        span.start_byte,
                        span.end_byte,
                        span.trace_artifact,
                        bytes.len()
                    ),
                });
            }
        }
        Ok(())
    }
}

fn ensure_proposal_signatures(
    proposal: &CandidateProposalV1,
    signatures: &BTreeMap<FailureSignatureId, FailureSignatureV1>,
) -> Result<(), EvolutionStoreError> {
    proposal.validate()?;
    for signature in &proposal.failure_signatures {
        if !signatures.contains_key(signature) {
            return Err(EvolutionStoreError::Evolution(EvolutionError::Foreign {
                message: format!(
                    "candidate {} cites an unregistered failure signature {signature}",
                    proposal.candidate_id
                ),
            }));
        }
    }
    Ok(())
}

fn validate_transition_pointer(
    profile: &ModelHarnessProfileId,
    pointer: Option<&GlobalProfilePointerV1>,
    state: &StoreState,
) -> Result<(), EvolutionStoreError> {
    let Some(pointer) = pointer else {
        return Ok(());
    };
    if &pointer.profile != profile {
        return Err(EvolutionStoreError::Corruption {
            message: "global profile transition pointer names a different profile".into(),
        });
    }
    if pointer.experiment_id != state.campaign.lock.experiment_id {
        return Err(EvolutionStoreError::Corruption {
            message: "global profile pointer belongs to another experiment".into(),
        });
    }
    let proposal = state
        .campaign
        .proposals
        .get(&pointer.candidate_id)
        .ok_or_else(|| EvolutionStoreError::Corruption {
            message: "global profile pointer names no retained candidate".into(),
        })?;
    let evaluation = state
        .campaign
        .evaluations
        .get(&pointer.candidate_id)
        .ok_or_else(|| EvolutionStoreError::Corruption {
            message: "global profile pointer names a candidate without an evaluation".into(),
        })?;
    if proposal.target_profile != pointer.profile
        || proposal.proposed_snapshot_id != pointer.snapshot_id
        || !state
            .campaign
            .policy
            .decide(&state.campaign.lock, evaluation)?
            .is_promotable()
    {
        return Err(EvolutionStoreError::Corruption {
            message: "global profile pointer is not a promotable candidate snapshot".into(),
        });
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), EvolutionStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EvolutionStoreError::Corruption {
            message: format!("campaign root {} is not a real directory", path.display()),
        });
    }
    Ok(())
}

fn ensure_regular_file(path: &Path) -> Result<(), EvolutionStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EvolutionStoreError::Corruption {
            message: format!("campaign state {} is not a regular file", path.display()),
        });
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), EvolutionStoreError> {
    let directory = File::open(path).map_err(|error| io_error(path, error))?;
    match directory.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(io_error(path, error)),
    }
}

fn io_error(path: &Path, error: impl ToString) -> EvolutionStoreError {
    EvolutionStoreError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

fn encode_state(state: &StoreState) -> JsonValue {
    JsonValue::object([
        ("kind", STORE_KIND.into()),
        ("schema_version", u64::from(STORE_SCHEMA_VERSION).into()),
        ("campaign", encode_campaign(&state.campaign)),
        (
            "failure_signatures",
            JsonValue::Array(
                state
                    .signatures
                    .values()
                    .map(encode_failure_signature)
                    .collect(),
            ),
        ),
        (
            "global_profile_transitions",
            JsonValue::Array(state.transitions.iter().map(encode_transition).collect()),
        ),
    ])
}

fn encode_campaign(campaign: &Campaign) -> JsonValue {
    JsonValue::object([
        ("lock", encode_lock(&campaign.lock)),
        ("policy", encode_policy(&campaign.policy)),
        (
            "proposals",
            JsonValue::Array(campaign.proposals.values().map(encode_proposal).collect()),
        ),
        (
            "evaluations",
            JsonValue::Array(
                campaign
                    .evaluations
                    .values()
                    .map(encode_evaluation)
                    .collect(),
            ),
        ),
        (
            "champion",
            campaign
                .champion
                .as_ref()
                .map_or(JsonValue::Null, |value| value.as_str().into()),
        ),
    ])
}

fn encode_lock(lock: &ExperimentLockV1) -> JsonValue {
    JsonValue::object([
        ("experiment_id", lock.experiment_id.as_str().into()),
        (
            "target_profiles",
            JsonValue::Array(
                lock.target_profiles
                    .iter()
                    .map(|value| value.as_str().into())
                    .collect(),
            ),
        ),
        ("evolver_profile", lock.evolver_profile.as_str().into()),
        ("initial_harness", lock.initial_harness.as_str().into()),
        ("task_manifest_digest", encode_digest(lock.task_manifest_digest)),
        ("split_manifest_digest", encode_digest(lock.split_manifest_digest)),
        ("evaluator_digest", encode_digest(lock.evaluator_digest)),
        ("environment_digest", encode_digest(lock.environment_digest)),
        (
            "capability_envelope_digest",
            encode_digest(lock.capability_envelope_digest),
        ),
        ("search_budget", encode_search_budget(&lock.search_budget)),
        ("serving_budget", encode_serving_budget(&lock.serving_budget)),
        (
            "promotion_policy_digest",
            encode_digest(lock.promotion_policy_digest),
        ),
        ("tea_build_identity", encode_build_identity(&lock.tea_build_identity)),
    ])
}

fn encode_search_budget(budget: &SearchBudget) -> JsonValue {
    JsonValue::object([
        ("maximum_candidates", u64::from(budget.maximum_candidates).into()),
        (
            "maximum_provider_requests",
            budget.maximum_provider_requests.into(),
        ),
        ("maximum_artifact_bytes", budget.maximum_artifact_bytes.into()),
    ])
}

fn encode_serving_budget(budget: &ServingBudget) -> JsonValue {
    JsonValue::object([
        (
            "maximum_provider_requests_per_task",
            u64::from(budget.maximum_provider_requests_per_task).into(),
        ),
        ("maximum_context_bytes", budget.maximum_context_bytes.into()),
        (
            "maximum_plugin_source_bytes",
            budget.maximum_plugin_source_bytes.into(),
        ),
    ])
}

fn encode_build_identity(identity: &BuildIdentity) -> JsonValue {
    JsonValue::object([
        ("tea_git_commit", identity.tea_git_commit.clone().into()),
        (
            "tea_dirty_patch_digest",
            encode_optional_digest(identity.tea_dirty_patch_digest),
        ),
        ("rust_version", identity.rust_version.clone().into()),
        ("operating_system", identity.operating_system.clone().into()),
        ("architecture", identity.architecture.clone().into()),
        (
            "provider_adapter_version",
            identity.provider_adapter_version.clone().into(),
        ),
        ("requested_model", identity.requested_model.clone().into()),
        (
            "returned_model_revision",
            identity
                .returned_model_revision
                .clone()
                .map_or(JsonValue::Null, Into::into),
        ),
        ("workspace_commit", identity.workspace_commit.clone().into()),
        (
            "workspace_dirty_patch_digest",
            encode_optional_digest(identity.workspace_dirty_patch_digest),
        ),
    ])
}

fn encode_policy(policy: &PromotionPolicy) -> JsonValue {
    JsonValue::object([
        (
            "minimum_material_improvements",
            u64::from(policy.minimum_material_improvements).into(),
        ),
        (
            "maximum_regression_numerator",
            u64::from(policy.maximum_regression_numerator).into(),
        ),
        (
            "maximum_regression_denominator",
            u64::from(policy.maximum_regression_denominator).into(),
        ),
        (
            "maximum_search_provider_requests",
            policy.maximum_search_provider_requests.into(),
        ),
    ])
}

fn encode_failure_signature(signature: &FailureSignatureV1) -> JsonValue {
    JsonValue::object([
        ("id", signature.id.as_str().into()),
        ("terminal_cause", encode_verifier_failure(&signature.terminal_cause)),
        (
            "causal_status",
            causal_status_name(signature.causal_status).into(),
        ),
        ("locus", failure_locus_name(signature.locus).into()),
        ("mechanism", signature.mechanism.as_str().into()),
        (
            "evidence",
            JsonValue::Array(
                signature
                    .evidence
                    .iter()
                    .map(|span| {
                        JsonValue::object([
                            ("trace_artifact", span.trace_artifact.to_hex().into()),
                            ("start_byte", span.start_byte.into()),
                            ("end_byte", span.end_byte.into()),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "confidence",
            evidence_confidence_name(signature.confidence).into(),
        ),
        (
            "addressability",
            addressability_name(signature.addressability).into(),
        ),
    ])
}

fn encode_verifier_failure(code: &VerifierFailureCode) -> JsonValue {
    match code {
        VerifierFailureCode::Contract => JsonValue::object([("kind", "contract".into())]),
        VerifierFailureCode::TaskVerifier => {
            JsonValue::object([("kind", "task_verifier".into())])
        }
        VerifierFailureCode::Provider => JsonValue::object([("kind", "provider".into())]),
        VerifierFailureCode::Harness => JsonValue::object([("kind", "harness".into())]),
        VerifierFailureCode::Other(value) => JsonValue::object([
            ("kind", "other".into()),
            ("value", value.clone().into()),
        ]),
    }
}

fn encode_proposal(proposal: &CandidateProposalV1) -> JsonValue {
    JsonValue::object([
        ("candidate_id", proposal.candidate_id.as_str().into()),
        (
            "parent_snapshot_id",
            proposal.parent_snapshot_id.as_str().into(),
        ),
        (
            "proposed_snapshot_id",
            proposal.proposed_snapshot_id.as_str().into(),
        ),
        ("target_profile", proposal.target_profile.as_str().into()),
        ("author", proposal_author_name(proposal.author).into()),
        (
            "hypothesis",
            JsonValue::object([
                (
                    "targeted_evidence",
                    proposal.hypothesis.targeted_evidence.clone().into(),
                ),
                (
                    "expected_effect",
                    proposal.hypothesis.expected_effect.clone().into(),
                ),
                (
                    "regression_risk",
                    proposal.hypothesis.regression_risk.clone().into(),
                ),
            ]),
        ),
        (
            "failure_signatures",
            JsonValue::Array(
                proposal
                    .failure_signatures
                    .iter()
                    .map(|value| value.as_str().into())
                    .collect(),
            ),
        ),
        (
            "changed_surfaces",
            JsonValue::Array(
                proposal
                    .changed_surfaces
                    .iter()
                    .map(|value| harness_surface_name(*value).into())
                    .collect(),
            ),
        ),
        (
            "provider_surface_byte_delta",
            proposal.provider_surface_byte_delta.to_string().into(),
        ),
    ])
}

fn encode_evaluation(evaluation: &CandidateEvaluationV1) -> JsonValue {
    JsonValue::object([
        ("experiment_id", evaluation.experiment_id.as_str().into()),
        ("candidate_id", evaluation.candidate_id.as_str().into()),
        (
            "gates",
            JsonValue::Array(
                evaluation
                    .gates
                    .iter()
                    .map(|(gate, result)| {
                        JsonValue::object([
                            ("gate", candidate_gate_name(*gate).into()),
                            ("passed", result.passed.into()),
                            ("evidence_digest", encode_digest(result.evidence_digest)),
                            ("summary", result.summary.clone().into()),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("metrics", encode_metrics(&evaluation.metrics)),
    ])
}

fn encode_metrics(metrics: &CandidateMetrics) -> JsonValue {
    JsonValue::object([
        (
            "material_improvements",
            u64::from(metrics.material_improvements).into(),
        ),
        ("regressions", u64::from(metrics.regressions).into()),
        (
            "hard_contract_regressions",
            u64::from(metrics.hard_contract_regressions).into(),
        ),
        ("capability_expansion", metrics.capability_expansion.into()),
        (
            "search_provider_requests",
            metrics.search_provider_requests.into(),
        ),
        (
            "serving_provider_requests_per_task",
            metrics.serving_provider_requests_per_task.into(),
        ),
        ("serving_context_bytes", metrics.serving_context_bytes.into()),
        ("plugin_source_bytes", metrics.plugin_source_bytes.into()),
    ])
}

fn encode_transition(transition: &GlobalProfileTransitionV1) -> JsonValue {
    JsonValue::object([
        ("sequence", transition.sequence.into()),
        (
            "action",
            transition_action_name(transition.action).into(),
        ),
        ("profile", transition.profile.as_str().into()),
        (
            "previous",
            transition
                .previous
                .as_ref()
                .map_or(JsonValue::Null, encode_pointer),
        ),
        (
            "current",
            transition
                .current
                .as_ref()
                .map_or(JsonValue::Null, encode_pointer),
        ),
    ])
}

fn encode_pointer(pointer: &GlobalProfilePointerV1) -> JsonValue {
    JsonValue::object([
        ("profile", pointer.profile.as_str().into()),
        ("experiment_id", pointer.experiment_id.as_str().into()),
        ("candidate_id", pointer.candidate_id.as_str().into()),
        ("snapshot_id", pointer.snapshot_id.as_str().into()),
    ])
}

fn encode_digest(digest: Digest) -> JsonValue {
    digest.to_hex().into()
}

fn encode_optional_digest(digest: Option<Digest>) -> JsonValue {
    digest.map_or(JsonValue::Null, encode_digest)
}

fn decode_state(input: &str) -> Result<StoreState, EvolutionStoreError> {
    let value = JsonValue::parse(input).map_err(|error| EvolutionStoreError::Corruption {
        message: format!("campaign JSON cannot parse: {error}"),
    })?;
    let object = object(&value, "campaign root")?;
    if string(field(object, "kind")?, "campaign kind")? != STORE_KIND {
        return Err(corruption("campaign state has an unknown kind"));
    }
    if number(field(object, "schema_version")?, "campaign schema version")?
        != u64::from(STORE_SCHEMA_VERSION)
    {
        return Err(corruption("campaign state is not v1"));
    }

    let signature_values = array(field(object, "failure_signatures")?, "failure signatures")?;
    let mut signatures = BTreeMap::new();
    for value in signature_values {
        let signature = decode_failure_signature(value)?;
        if signatures.insert(signature.id.clone(), signature).is_some() {
            return Err(corruption("campaign repeats a failure signature ID"));
        }
    }

    let campaign_value = object_value(field(object, "campaign")?, "campaign")?;
    let lock = decode_lock(field(campaign_value, "lock")?)?;
    let policy = decode_policy(field(campaign_value, "policy")?)?;
    let mut campaign = Campaign::new(lock, policy)?;
    for value in array(field(campaign_value, "proposals")?, "campaign proposals")? {
        campaign.stage(decode_proposal(value)?)?;
    }
    for value in array(field(campaign_value, "evaluations")?, "campaign evaluations")? {
        campaign.record_evaluation(decode_evaluation(value)?)?;
    }
    let champion = optional_string(field(campaign_value, "champion")?, "campaign champion")?;
    if let Some(champion) = champion {
        let champion = HarnessCandidateId::new(champion).map_err(|error| corruption(error.to_string()))?;
        campaign.select_champion(PromotionAuthority::Operator, &champion)?;
    }

    let mut transitions = Vec::new();
    for value in array(
        field(object, "global_profile_transitions")?,
        "global profile transitions",
    )? {
        transitions.push(decode_transition(value)?);
    }
    let mut global_profiles = BTreeMap::new();
    for transition in &transitions {
        match &transition.current {
            Some(pointer) => {
                global_profiles.insert(transition.profile.clone(), pointer.clone());
            }
            None => {
                global_profiles.remove(&transition.profile);
            }
        }
    }
    Ok(StoreState {
        campaign,
        signatures,
        global_profiles,
        transitions,
    })
}

fn decode_lock(value: &JsonValue) -> Result<ExperimentLockV1, EvolutionStoreError> {
    let object = object_value(value, "experiment lock")?;
    let target_profiles = array(field(object, "target_profiles")?, "target profiles")?
        .iter()
        .map(|value| {
            ModelHarnessProfileId::new(string(value, "target profile")?.to_owned())
                .map_err(|error| corruption(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let lock = ExperimentLockV1::new(
        target_profiles,
        model_harness_profile_id(field(object, "evolver_profile")?, "evolver profile")?,
        harness_snapshot_id(field(object, "initial_harness")?, "initial harness")?,
        digest(field(object, "task_manifest_digest")?, "task manifest digest")?,
        digest(field(object, "split_manifest_digest")?, "split manifest digest")?,
        digest(field(object, "evaluator_digest")?, "evaluator digest")?,
        digest(field(object, "environment_digest")?, "environment digest")?,
        digest(
            field(object, "capability_envelope_digest")?,
            "capability envelope digest",
        )?,
        decode_search_budget(field(object, "search_budget")?)?,
        decode_serving_budget(field(object, "serving_budget")?)?,
        digest(field(object, "promotion_policy_digest")?, "promotion policy digest")?,
        decode_build_identity(field(object, "tea_build_identity")?)?,
    )?;
    let claimed = experiment_id(field(object, "experiment_id")?, "experiment ID")?;
    if claimed != lock.experiment_id {
        return Err(corruption("experiment lock ID does not match its frozen inputs"));
    }
    Ok(lock)
}

fn decode_search_budget(value: &JsonValue) -> Result<SearchBudget, EvolutionStoreError> {
    let object = object_value(value, "search budget")?;
    Ok(SearchBudget {
        maximum_candidates: u32_value(field(object, "maximum_candidates")?, "maximum candidates")?,
        maximum_provider_requests: number(
            field(object, "maximum_provider_requests")?,
            "maximum provider requests",
        )?,
        maximum_artifact_bytes: number(
            field(object, "maximum_artifact_bytes")?,
            "maximum artifact bytes",
        )?,
    })
}

fn decode_serving_budget(value: &JsonValue) -> Result<ServingBudget, EvolutionStoreError> {
    let object = object_value(value, "serving budget")?;
    Ok(ServingBudget {
        maximum_provider_requests_per_task: u32_value(
            field(object, "maximum_provider_requests_per_task")?,
            "maximum provider requests per task",
        )?,
        maximum_context_bytes: number(
            field(object, "maximum_context_bytes")?,
            "maximum context bytes",
        )?,
        maximum_plugin_source_bytes: number(
            field(object, "maximum_plugin_source_bytes")?,
            "maximum plugin source bytes",
        )?,
    })
}

fn decode_build_identity(value: &JsonValue) -> Result<BuildIdentity, EvolutionStoreError> {
    let object = object_value(value, "build identity")?;
    Ok(BuildIdentity {
        tea_git_commit: string(field(object, "tea_git_commit")?, "tea git commit")?.to_owned(),
        tea_dirty_patch_digest: optional_digest(
            field(object, "tea_dirty_patch_digest")?,
            "tea dirty patch digest",
        )?,
        rust_version: string(field(object, "rust_version")?, "Rust version")?.to_owned(),
        operating_system: string(field(object, "operating_system")?, "operating system")?.to_owned(),
        architecture: string(field(object, "architecture")?, "architecture")?.to_owned(),
        provider_adapter_version: string(
            field(object, "provider_adapter_version")?,
            "provider adapter version",
        )?
        .to_owned(),
        requested_model: string(field(object, "requested_model")?, "requested model")?.to_owned(),
        returned_model_revision: optional_string(
            field(object, "returned_model_revision")?,
            "returned model revision",
        )?
        .map(str::to_owned),
        workspace_commit: string(field(object, "workspace_commit")?, "workspace commit")?.to_owned(),
        workspace_dirty_patch_digest: optional_digest(
            field(object, "workspace_dirty_patch_digest")?,
            "workspace dirty patch digest",
        )?,
    })
}

fn decode_policy(value: &JsonValue) -> Result<PromotionPolicy, EvolutionStoreError> {
    let object = object_value(value, "promotion policy")?;
    Ok(PromotionPolicy {
        minimum_material_improvements: u32_value(
            field(object, "minimum_material_improvements")?,
            "minimum material improvements",
        )?,
        maximum_regression_numerator: u32_value(
            field(object, "maximum_regression_numerator")?,
            "maximum regression numerator",
        )?,
        maximum_regression_denominator: u32_value(
            field(object, "maximum_regression_denominator")?,
            "maximum regression denominator",
        )?,
        maximum_search_provider_requests: number(
            field(object, "maximum_search_provider_requests")?,
            "maximum search provider requests",
        )?,
    })
}

fn decode_failure_signature(value: &JsonValue) -> Result<FailureSignatureV1, EvolutionStoreError> {
    let object = object_value(value, "failure signature")?;
    let evidence = array(field(object, "evidence")?, "failure evidence")?
        .iter()
        .map(|value| {
            let object = object_value(value, "trace span")?;
            TraceSpanRef::new(
                artifact_id(field(object, "trace_artifact")?, "trace artifact")?,
                number(field(object, "start_byte")?, "trace start byte")?,
                number(field(object, "end_byte")?, "trace end byte")?,
            )
            .map_err(EvolutionStoreError::Evolution)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let signature = FailureSignatureV1::new(
        decode_verifier_failure(field(object, "terminal_cause")?)?,
        decode_causal_status(field(object, "causal_status")?)?,
        decode_failure_locus(field(object, "locus")?)?,
        MechanismCode::new(string(field(object, "mechanism")?, "mechanism")?.to_owned())?,
        evidence,
        decode_evidence_confidence(field(object, "confidence")?)?,
        decode_addressability(field(object, "addressability")?)?,
    )?;
    let claimed = failure_signature_id(field(object, "id")?, "failure signature ID")?;
    if signature.id != claimed {
        return Err(corruption(
            "failure signature ID does not match its immutable evidence",
        ));
    }
    Ok(signature)
}

fn decode_verifier_failure(value: &JsonValue) -> Result<VerifierFailureCode, EvolutionStoreError> {
    let object = object_value(value, "verifier failure code")?;
    match string(field(object, "kind")?, "verifier failure kind")? {
        "contract" => Ok(VerifierFailureCode::Contract),
        "task_verifier" => Ok(VerifierFailureCode::TaskVerifier),
        "provider" => Ok(VerifierFailureCode::Provider),
        "harness" => Ok(VerifierFailureCode::Harness),
        "other" => Ok(VerifierFailureCode::Other(
            string(field(object, "value")?, "other verifier failure")?.to_owned(),
        )),
        _ => Err(corruption("unknown verifier failure code")),
    }
}

fn decode_proposal(value: &JsonValue) -> Result<CandidateProposalV1, EvolutionStoreError> {
    let object = object_value(value, "candidate proposal")?;
    let hypothesis = object_value(field(object, "hypothesis")?, "candidate hypothesis")?;
    let failure_signatures = array(
        field(object, "failure_signatures")?,
        "candidate failure signatures",
    )?
    .iter()
    .map(|value| failure_signature_id(value, "candidate failure signature"))
    .collect::<Result<Vec<_>, _>>()?;
    let changed_surfaces = array(field(object, "changed_surfaces")?, "changed surfaces")?
        .iter()
        .map(decode_harness_surface)
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(CandidateProposalV1 {
        candidate_id: harness_candidate_id(field(object, "candidate_id")?, "candidate ID")?,
        parent_snapshot_id: harness_snapshot_id(
            field(object, "parent_snapshot_id")?,
            "parent snapshot ID",
        )?,
        proposed_snapshot_id: harness_snapshot_id(
            field(object, "proposed_snapshot_id")?,
            "proposed snapshot ID",
        )?,
        target_profile: model_harness_profile_id(
            field(object, "target_profile")?,
            "candidate target profile",
        )?,
        author: decode_proposal_author(field(object, "author")?)?,
        hypothesis: CandidateHypothesis {
            targeted_evidence: string(
                field(hypothesis, "targeted_evidence")?,
                "candidate targeted evidence",
            )?
            .to_owned(),
            expected_effect: string(
                field(hypothesis, "expected_effect")?,
                "candidate expected effect",
            )?
            .to_owned(),
            regression_risk: string(
                field(hypothesis, "regression_risk")?,
                "candidate regression risk",
            )?
            .to_owned(),
        },
        failure_signatures,
        changed_surfaces,
        provider_surface_byte_delta: string(
            field(object, "provider_surface_byte_delta")?,
            "provider surface byte delta",
        )?
        .parse()
        .map_err(|_| corruption("provider surface byte delta is not an i64"))?,
    })
}

fn decode_evaluation(value: &JsonValue) -> Result<CandidateEvaluationV1, EvolutionStoreError> {
    let object = object_value(value, "candidate evaluation")?;
    let mut gates = BTreeMap::new();
    for value in array(field(object, "gates")?, "candidate gates")? {
        let gate = object_value(value, "candidate gate")?;
        let name = decode_candidate_gate(field(gate, "gate")?)?;
        let result = GateResult {
            passed: boolean(field(gate, "passed")?, "gate passed")?,
            evidence_digest: digest(field(gate, "evidence_digest")?, "gate evidence digest")?,
            summary: string(field(gate, "summary")?, "gate summary")?.to_owned(),
        };
        if gates.insert(name, result).is_some() {
            return Err(corruption("candidate evaluation repeats a gate"));
        }
    }
    Ok(CandidateEvaluationV1 {
        experiment_id: experiment_id(field(object, "experiment_id")?, "evaluation experiment ID")?,
        candidate_id: harness_candidate_id(field(object, "candidate_id")?, "evaluation candidate ID")?,
        gates,
        metrics: decode_metrics(field(object, "metrics")?)?,
    })
}

fn decode_metrics(value: &JsonValue) -> Result<CandidateMetrics, EvolutionStoreError> {
    let object = object_value(value, "candidate metrics")?;
    Ok(CandidateMetrics {
        material_improvements: u32_value(
            field(object, "material_improvements")?,
            "material improvements",
        )?,
        regressions: u32_value(field(object, "regressions")?, "regressions")?,
        hard_contract_regressions: u32_value(
            field(object, "hard_contract_regressions")?,
            "hard contract regressions",
        )?,
        capability_expansion: boolean(
            field(object, "capability_expansion")?,
            "capability expansion",
        )?,
        search_provider_requests: number(
            field(object, "search_provider_requests")?,
            "search provider requests",
        )?,
        serving_provider_requests_per_task: number(
            field(object, "serving_provider_requests_per_task")?,
            "serving provider requests per task",
        )?,
        serving_context_bytes: number(
            field(object, "serving_context_bytes")?,
            "serving context bytes",
        )?,
        plugin_source_bytes: number(field(object, "plugin_source_bytes")?, "plugin source bytes")?,
    })
}

fn decode_transition(value: &JsonValue) -> Result<GlobalProfileTransitionV1, EvolutionStoreError> {
    let object = object_value(value, "global profile transition")?;
    Ok(GlobalProfileTransitionV1 {
        sequence: number(field(object, "sequence")?, "transition sequence")?,
        action: decode_transition_action(field(object, "action")?)?,
        profile: model_harness_profile_id(field(object, "profile")?, "transition profile")?,
        previous: optional_pointer(field(object, "previous")?, "transition previous pointer")?,
        current: optional_pointer(field(object, "current")?, "transition current pointer")?,
    })
}

fn optional_pointer(
    value: &JsonValue,
    context: &str,
) -> Result<Option<GlobalProfilePointerV1>, EvolutionStoreError> {
    if value.is_null() {
        return Ok(None);
    }
    let object = object_value(value, context)?;
    Ok(Some(GlobalProfilePointerV1 {
        profile: model_harness_profile_id(field(object, "profile")?, "pointer profile")?,
        experiment_id: experiment_id(field(object, "experiment_id")?, "pointer experiment ID")?,
        candidate_id: harness_candidate_id(field(object, "candidate_id")?, "pointer candidate ID")?,
        snapshot_id: harness_snapshot_id(field(object, "snapshot_id")?, "pointer snapshot ID")?,
    }))
}

fn field<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<&'a JsonValue, EvolutionStoreError> {
    object
        .get(name)
        .ok_or_else(|| corruption(format!("campaign state is missing required field {name}")))
}

fn object<'a>(
    value: &'a JsonValue,
    context: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, EvolutionStoreError> {
    value
        .as_object()
        .ok_or_else(|| corruption(format!("{context} must be a JSON object")))
}

fn object_value<'a>(
    value: &'a JsonValue,
    context: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, EvolutionStoreError> {
    object(value, context)
}

fn array<'a>(value: &'a JsonValue, context: &str) -> Result<&'a [JsonValue], EvolutionStoreError> {
    value
        .as_array()
        .ok_or_else(|| corruption(format!("{context} must be a JSON array")))
}

fn string<'a>(value: &'a JsonValue, context: &str) -> Result<&'a str, EvolutionStoreError> {
    value
        .as_str()
        .ok_or_else(|| corruption(format!("{context} must be a JSON string")))
}

fn optional_string<'a>(
    value: &'a JsonValue,
    context: &str,
) -> Result<Option<&'a str>, EvolutionStoreError> {
    if value.is_null() {
        Ok(None)
    } else {
        string(value, context).map(Some)
    }
}

fn number(value: &JsonValue, context: &str) -> Result<u64, EvolutionStoreError> {
    value
        .as_u64()
        .ok_or_else(|| corruption(format!("{context} must be a nonnegative JSON integer")))
}

fn u32_value(value: &JsonValue, context: &str) -> Result<u32, EvolutionStoreError> {
    u32::try_from(number(value, context)?)
        .map_err(|_| corruption(format!("{context} exceeds u32")))
}

fn boolean(value: &JsonValue, context: &str) -> Result<bool, EvolutionStoreError> {
    value
        .as_bool()
        .ok_or_else(|| corruption(format!("{context} must be a JSON boolean")))
}

fn digest(value: &JsonValue, context: &str) -> Result<Digest, EvolutionStoreError> {
    Digest::from_hex(string(value, context)?).map_err(|error| corruption(error.to_string()))
}

fn optional_digest(value: &JsonValue, context: &str) -> Result<Option<Digest>, EvolutionStoreError> {
    if value.is_null() {
        Ok(None)
    } else {
        digest(value, context).map(Some)
    }
}

fn artifact_id(value: &JsonValue, context: &str) -> Result<ArtifactId, EvolutionStoreError> {
    ArtifactId::from_hex(string(value, context)?).map_err(|error| corruption(error.to_string()))
}

fn experiment_id(value: &JsonValue, context: &str) -> Result<ExperimentId, EvolutionStoreError> {
    ExperimentId::new(string(value, context)?.to_owned()).map_err(|error| corruption(error.to_string()))
}

fn failure_signature_id(
    value: &JsonValue,
    context: &str,
) -> Result<FailureSignatureId, EvolutionStoreError> {
    FailureSignatureId::new(string(value, context)?.to_owned())
        .map_err(|error| corruption(error.to_string()))
}

fn harness_candidate_id(
    value: &JsonValue,
    context: &str,
) -> Result<HarnessCandidateId, EvolutionStoreError> {
    HarnessCandidateId::new(string(value, context)?.to_owned())
        .map_err(|error| corruption(error.to_string()))
}

fn harness_snapshot_id(
    value: &JsonValue,
    context: &str,
) -> Result<HarnessSnapshotId, EvolutionStoreError> {
    HarnessSnapshotId::new(string(value, context)?.to_owned())
        .map_err(|error| corruption(error.to_string()))
}

fn model_harness_profile_id(
    value: &JsonValue,
    context: &str,
) -> Result<ModelHarnessProfileId, EvolutionStoreError> {
    ModelHarnessProfileId::new(string(value, context)?.to_owned())
        .map_err(|error| corruption(error.to_string()))
}

fn decode_causal_status(value: &JsonValue) -> Result<CausalStatus, EvolutionStoreError> {
    match string(value, "causal status")? {
        "observed" => Ok(CausalStatus::Observed),
        "supported" => Ok(CausalStatus::Supported),
        "contradicted" => Ok(CausalStatus::Contradicted),
        "unknown" => Ok(CausalStatus::Unknown),
        _ => Err(corruption("unknown causal status")),
    }
}

fn decode_failure_locus(value: &JsonValue) -> Result<FailureLocus, EvolutionStoreError> {
    match string(value, "failure locus")? {
        "task_understanding" => Ok(FailureLocus::TaskUnderstanding),
        "repository_discovery" => Ok(FailureLocus::RepositoryDiscovery),
        "context_retrieval" => Ok(FailureLocus::ContextRetrieval),
        "tool_selection" => Ok(FailureLocus::ToolSelection),
        "tool_arguments" => Ok(FailureLocus::ToolArguments),
        "tool_execution" => Ok(FailureLocus::ToolExecution),
        "tool_result_interpretation" => Ok(FailureLocus::ToolResultInterpretation),
        "implementation" => Ok(FailureLocus::Implementation),
        "verification" => Ok(FailureLocus::Verification),
        "failure_recovery" => Ok(FailureLocus::FailureRecovery),
        "memory" => Ok(FailureLocus::Memory),
        "compaction" => Ok(FailureLocus::Compaction),
        "termination" => Ok(FailureLocus::Termination),
        "harness_runtime" => Ok(FailureLocus::HarnessRuntime),
        _ => Err(corruption("unknown failure locus")),
    }
}

fn decode_evidence_confidence(value: &JsonValue) -> Result<EvidenceConfidence, EvolutionStoreError> {
    match string(value, "evidence confidence")? {
        "low" => Ok(EvidenceConfidence::Low),
        "moderate" => Ok(EvidenceConfidence::Moderate),
        "high" => Ok(EvidenceConfidence::High),
        _ => Err(corruption("unknown evidence confidence")),
    }
}

fn decode_addressability(value: &JsonValue) -> Result<Addressability, EvolutionStoreError> {
    match string(value, "addressability")? {
        "luau_policy" => Ok(Addressability::LuauPolicy),
        "host_substrate" => Ok(Addressability::HostSubstrate),
        "rust_core_gap" => Ok(Addressability::RustCoreGap),
        "task_specific" => Ok(Addressability::TaskSpecific),
        "model_capability_limit" => Ok(Addressability::ModelCapabilityLimit),
        "unstable_or_unknown" => Ok(Addressability::UnstableOrUnknown),
        _ => Err(corruption("unknown failure addressability")),
    }
}

fn decode_proposal_author(value: &JsonValue) -> Result<ProposalAuthor, EvolutionStoreError> {
    match string(value, "proposal author")? {
        "model" => Ok(ProposalAuthor::Model),
        "operator" => Ok(ProposalAuthor::Operator),
        _ => Err(corruption("unknown proposal author")),
    }
}

fn decode_harness_surface(value: &JsonValue) -> Result<HarnessSurface, EvolutionStoreError> {
    match string(value, "harness surface")? {
        "system_prompt" => Ok(HarnessSurface::SystemPrompt),
        "tool_definitions" => Ok(HarnessSurface::ToolDefinitions),
        "hooks" => Ok(HarnessSurface::Hooks),
        "capability_bindings" => Ok(HarnessSurface::CapabilityBindings),
        "compaction" => Ok(HarnessSurface::Compaction),
        "tool_projection" => Ok(HarnessSurface::ToolProjection),
        "failure_policy" => Ok(HarnessSurface::FailurePolicy),
        _ => Err(corruption("unknown harness surface")),
    }
}

fn decode_candidate_gate(value: &JsonValue) -> Result<CandidateGate, EvolutionStoreError> {
    match string(value, "candidate gate")? {
        "static_validity" => Ok(CandidateGate::StaticValidity),
        "deterministic_contracts" => Ok(CandidateGate::DeterministicContracts),
        "trace_replay" => Ok(CandidateGate::TraceReplay),
        "targeted_diagnostics" => Ok(CandidateGate::TargetedDiagnostics),
        "replay_and_retention" => Ok(CandidateGate::ReplayAndRetention),
        "paired_promotion_validation" => Ok(CandidateGate::PairedPromotionValidation),
        "composite_validation" => Ok(CandidateGate::CompositeValidation),
        "canary" => Ok(CandidateGate::Canary),
        _ => Err(corruption("unknown candidate gate")),
    }
}

fn decode_transition_action(
    value: &JsonValue,
) -> Result<GlobalProfileTransitionActionV1, EvolutionStoreError> {
    match string(value, "global profile transition action")? {
        "promotion" => Ok(GlobalProfileTransitionActionV1::Promotion),
        "rollback" => Ok(GlobalProfileTransitionActionV1::Rollback),
        _ => Err(corruption("unknown global profile transition action")),
    }
}

fn causal_status_name(value: CausalStatus) -> &'static str {
    match value {
        CausalStatus::Observed => "observed",
        CausalStatus::Supported => "supported",
        CausalStatus::Contradicted => "contradicted",
        CausalStatus::Unknown => "unknown",
    }
}

fn failure_locus_name(value: FailureLocus) -> &'static str {
    match value {
        FailureLocus::TaskUnderstanding => "task_understanding",
        FailureLocus::RepositoryDiscovery => "repository_discovery",
        FailureLocus::ContextRetrieval => "context_retrieval",
        FailureLocus::ToolSelection => "tool_selection",
        FailureLocus::ToolArguments => "tool_arguments",
        FailureLocus::ToolExecution => "tool_execution",
        FailureLocus::ToolResultInterpretation => "tool_result_interpretation",
        FailureLocus::Implementation => "implementation",
        FailureLocus::Verification => "verification",
        FailureLocus::FailureRecovery => "failure_recovery",
        FailureLocus::Memory => "memory",
        FailureLocus::Compaction => "compaction",
        FailureLocus::Termination => "termination",
        FailureLocus::HarnessRuntime => "harness_runtime",
    }
}

fn evidence_confidence_name(value: EvidenceConfidence) -> &'static str {
    match value {
        EvidenceConfidence::Low => "low",
        EvidenceConfidence::Moderate => "moderate",
        EvidenceConfidence::High => "high",
    }
}

fn addressability_name(value: Addressability) -> &'static str {
    match value {
        Addressability::LuauPolicy => "luau_policy",
        Addressability::HostSubstrate => "host_substrate",
        Addressability::RustCoreGap => "rust_core_gap",
        Addressability::TaskSpecific => "task_specific",
        Addressability::ModelCapabilityLimit => "model_capability_limit",
        Addressability::UnstableOrUnknown => "unstable_or_unknown",
    }
}

fn proposal_author_name(value: ProposalAuthor) -> &'static str {
    match value {
        ProposalAuthor::Model => "model",
        ProposalAuthor::Operator => "operator",
    }
}

fn harness_surface_name(value: HarnessSurface) -> &'static str {
    match value {
        HarnessSurface::SystemPrompt => "system_prompt",
        HarnessSurface::ToolDefinitions => "tool_definitions",
        HarnessSurface::Hooks => "hooks",
        HarnessSurface::CapabilityBindings => "capability_bindings",
        HarnessSurface::Compaction => "compaction",
        HarnessSurface::ToolProjection => "tool_projection",
        HarnessSurface::FailurePolicy => "failure_policy",
    }
}

fn candidate_gate_name(value: CandidateGate) -> &'static str {
    match value {
        CandidateGate::StaticValidity => "static_validity",
        CandidateGate::DeterministicContracts => "deterministic_contracts",
        CandidateGate::TraceReplay => "trace_replay",
        CandidateGate::TargetedDiagnostics => "targeted_diagnostics",
        CandidateGate::ReplayAndRetention => "replay_and_retention",
        CandidateGate::PairedPromotionValidation => "paired_promotion_validation",
        CandidateGate::CompositeValidation => "composite_validation",
        CandidateGate::Canary => "canary",
    }
}

fn transition_action_name(value: GlobalProfileTransitionActionV1) -> &'static str {
    match value {
        GlobalProfileTransitionActionV1::Promotion => "promotion",
        GlobalProfileTransitionActionV1::Rollback => "rollback",
    }
}

fn corruption(message: impl Into<String>) -> EvolutionStoreError {
    EvolutionStoreError::Corruption {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tea_session::{ArtifactStore, MemoryArtifactStore};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn id<T>(value: &str, construct: impl FnOnce(String) -> Result<T, tea_session::IdError>) -> T {
        construct(value.into()).expect("fixture opaque ID")
    }

    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tea-evolve-{label}-{}-{:016x}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ))
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

    fn policy() -> PromotionPolicy {
        PromotionPolicy {
            minimum_material_improvements: 1,
            maximum_regression_numerator: 0,
            maximum_regression_denominator: 1,
            maximum_search_provider_requests: 20,
        }
    }

    fn signature(trace: ArtifactId) -> FailureSignatureV1 {
        FailureSignatureV1::new(
            VerifierFailureCode::TaskVerifier,
            CausalStatus::Supported,
            FailureLocus::ToolArguments,
            MechanismCode::new("wrong_argument_name").expect("mechanism"),
            vec![TraceSpanRef::new(trace, 1, 4).expect("trace span")],
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

    fn evaluation(
        lock: &ExperimentLockV1,
        candidate_id: HarnessCandidateId,
        material_improvements: u32,
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
                material_improvements,
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

    fn trace_store() -> (Arc<MemoryArtifactStore>, ArtifactId) {
        let artifacts = Arc::new(MemoryArtifactStore::default());
        let trace = artifacts
            .put(b"0123456789", "application/x-ndjson")
            .expect("trace object")
            .artifact_id;
        (artifacts, trace)
    }

    #[test]
    fn campaign_survives_reopen_with_trace_roots_and_rejected_lineage() {
        let path = root("reopen");
        let (artifacts, trace) = trace_store();
        let lock = lock();
        let mut store = EvolutionStore::create(&path, artifacts.clone(), lock.clone(), policy())
            .expect("new durable evolution campaign");
        let signature = signature(trace);
        let candidate = proposal(signature.id.clone());
        let candidate_id = candidate.candidate_id.clone();
        store
            .register_failure_signature(signature.clone())
            .expect("trace-backed signature persists");
        store.stage(candidate).expect("candidate persists");
        assert!(store
            .record_evaluation(evaluation(&lock, candidate_id.clone(), 0))
            .expect("rejected evaluation persists")
            .is_promotable()
            == false);
        drop(store);

        let reopened = EvolutionStore::open(&path, artifacts).expect("campaign reopens");
        assert!(reopened.campaign().proposals().contains_key(&candidate_id));
        assert!(reopened.failure_signatures().contains_key(&signature.id));
        assert_eq!(reopened.artifact_roots(), [trace].into_iter().collect());
        assert!(matches!(
            reopened.campaign().champion(),
            None
        ));
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn global_promotion_requires_selected_operator_champion_and_rollback_keeps_lineage() {
        let path = root("promotion");
        let (artifacts, trace) = trace_store();
        let lock = lock();
        let mut store = EvolutionStore::create(&path, artifacts.clone(), lock.clone(), policy())
            .expect("new durable evolution campaign");
        let signature = signature(trace);
        let candidate = proposal(signature.id.clone());
        let candidate_id = candidate.candidate_id.clone();
        let profile = candidate.target_profile.clone();
        store
            .register_failure_signature(signature)
            .expect("signature persists");
        store.stage(candidate).expect("candidate persists");
        store
            .record_evaluation(evaluation(&lock, candidate_id.clone(), 2))
            .expect("promotable evaluation persists");
        assert!(matches!(
            store.promote_global(PromotionAuthority::Operator, &candidate_id),
            Err(EvolutionStoreError::Evolution(EvolutionError::PromotionDenied { .. }))
        ));
        store
            .select_champion(PromotionAuthority::Operator, &candidate_id)
            .expect("operator selects champion");
        let active = store
            .promote_global(PromotionAuthority::Operator, &candidate_id)
            .expect("explicit operator promotion");
        assert_eq!(store.global_profile(&profile), Some(&active));
        assert_eq!(
            store
                .rollback_global(PromotionAuthority::Operator, &profile)
                .expect("operator rollback"),
            None
        );
        assert!(store.global_profile(&profile).is_none());
        assert_eq!(store.global_profile_transitions().len(), 2);
        assert_eq!(
            store.global_profile_transitions()[1].previous.as_ref(),
            Some(&active)
        );
        drop(store);

        let reopened = EvolutionStore::open(&path, artifacts).expect("lineage reopens");
        assert!(reopened.global_profile(&profile).is_none());
        assert_eq!(reopened.global_profile_transitions().len(), 2);
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn missing_trace_artifact_fails_closed_before_signature_is_retained() {
        let path = root("missing-trace");
        let artifacts = Arc::new(MemoryArtifactStore::default());
        let mut store = EvolutionStore::create(&path, artifacts, lock(), policy())
            .expect("new durable evolution campaign");
        let absent = ArtifactId::from_bytes("absent trace");
        assert!(matches!(
            store.register_failure_signature(signature(absent)),
            Err(EvolutionStoreError::Artifact(ArtifactError::NotFound { .. }))
        ));
        assert!(store.failure_signatures().is_empty());
        let _ = fs::remove_dir_all(path);
    }
}
