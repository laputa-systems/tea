//! Guarded Codex subscription infrastructure for the optional live verification suite.
//!
//! This module is compiled only with `live-verification`. It does not update the provider
//! catalog or select a model for ordinary Tea sessions. It pins the requested Codex model and
//! reasoning effort while recording each request before provider transport.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::scheduler::{
    CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture, ModelProvider,
    ModelRequest, ModelStreamEvent,
};
use tea_core::runtime::{DurableOperation, SessionSupervisor};
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_protocol::JsonValue;
use tea_providers::{ConfiguredProvider, RetryPolicy};
use tea_providers::codex::{
    CodexAuthManager, CodexClientCredentialStore, CodexConfig, CodexProvider, CredentialStore,
    FileCredentialStore,
};
use tea_session::SessionEntry;

pub(crate) mod scenarios_compaction;
pub(crate) mod scenarios_children;
pub(crate) mod scenarios_evolution;
pub use scenarios_compaction::{
    LiveCompactionScenario, LiveCompactionScenarioOutcome, run_live_compaction_scenario,
};
pub use scenarios_children::{
    LiveChildScenario, LiveChildScenarioOutcome, run_live_child_scenario,
};
pub use scenarios_evolution::{
    LiveEvolutionScenario, LiveEvolutionScenarioOutcome, run_live_evolution_scenario,
};

/// The sole provider identifier accepted by live verification.
pub const CODEX_PROVIDER_ID: &str = "codex";
/// The exact model selected for live verification.
pub const CODEX_MODEL_ID: &str = "gpt-5.6-luna";
/// The fixed ChatGPT subscription endpoint owned by the Codex adapter.
pub const CODEX_RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Official Codex model documentation used for this selection.
pub const CODEX_MODEL_SOURCE: &str = "https://learn.chatgpt.com/docs/models";
/// The only reasoning effort admitted by the live suite.
pub const CODEX_REASONING_EFFORT: ThinkingLevel = ThinkingLevel::Low;

const EVIDENCE_SCHEMA: &str = "tea-codex-luna-model-evidence/v1";
const LEDGER_SCHEMA: &str = "tea-codex-live-verification-ledger/v2";

/// A checked official model record accepted by the guarded factory.
/// Availability for a specific Tea originator remains a live transport question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexModelEvidence {
    checked_on: String,
}

impl CodexModelEvidence {
    /// Read and strictly validate a sanitized official model record.
    pub fn read(path: &Path) -> Result<Self, LiveVerificationError> {
        let source = fs::read_to_string(path).map_err(|error| {
            LiveVerificationError::new(format!(
                "cannot read live-verification model evidence {}: {error}",
                path.display()
            ))
        })?;
        let value = JsonValue::parse(&source).map_err(|error| {
            LiveVerificationError::new(format!(
                "live-verification model evidence is not valid JSON: {error}"
            ))
        })?;
        Self::from_json(&value)
    }

    /// Validate a parsed sanitized official model record.
    pub fn from_json(value: &JsonValue) -> Result<Self, LiveVerificationError> {
        let record = object(value, "model evidence")?;
        required_string(record, "schema_version", "model evidence")
            .and_then(|schema| require_exact(schema, EVIDENCE_SCHEMA, "schema_version"))?;
        required_string(record, "model_source", "model evidence")
            .and_then(|source| require_exact(source, CODEX_MODEL_SOURCE, "model_source"))?;
        required_string(record, "provider", "model evidence")
            .and_then(|provider| require_exact(provider, CODEX_PROVIDER_ID, "provider"))?;
        required_string(record, "model", "model evidence")
            .and_then(|model| require_exact(model, CODEX_MODEL_ID, "model"))?;
        required_string(record, "endpoint", "model evidence")
            .and_then(|endpoint| require_exact(endpoint, CODEX_RESPONSES_ENDPOINT, "endpoint"))?;
        required_string(record, "reasoning_effort", "model evidence")
            .and_then(|effort| require_exact(effort, "low", "reasoning_effort"))?;
        let checked_on = required_string(record, "checked_on", "model evidence")?;
        if !is_iso_date(checked_on) {
            return Err(LiveVerificationError::new(
                "model evidence checked_on must use YYYY-MM-DD",
            ));
        }
        if record
            .get("synthetic_or_public_fixture_only")
            .and_then(JsonValue::as_bool)
            != Some(true)
        {
            return Err(LiveVerificationError::new(
                "model evidence must acknowledge synthetic-or-public-fixture-only live input",
            ));
        }
        Ok(Self {
            checked_on: checked_on.to_owned(),
        })
    }

    /// Return the UTC date on which the official model record was checked.
    pub fn checked_on(&self) -> &str {
        &self.checked_on
    }

    fn require_current_utc_date(&self) -> Result<(), LiveVerificationError> {
        let current = current_utc_date()?;
        if self.checked_on != current {
            return Err(LiveVerificationError::new(format!(
                "live verification model evidence is stale (checked_on={}, current_utc_date={current}); refresh the official record before transport",
                self.checked_on,
            )));
        }
        Ok(())
    }
}

/// A live-model consumer that must obtain its provider from the restricted factory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationConsumer {
    /// The primary root run.
    Root,
    /// An isolated child run.
    Child,
    /// A history compaction or summary request.
    Compaction,
    /// A harness-candidate evaluation.
    CandidateEvaluation,
    /// An optional model-based comparison or judge.
    Comparison,
}

impl VerificationConsumer {
    fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Child => "child",
            Self::Compaction => "compaction",
            Self::CandidateEvaluation => "candidate_evaluation",
            Self::Comparison => "comparison",
        }
    }
}

/// One recorded request reservation made before provider transport begins.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveAttemptReservation {
    /// One-based request attempt sequence for the aggregate suite.
    pub sequence: u64,
    /// The model consumer that requested this attempt.
    pub consumer: VerificationConsumer,
    /// Exact model routed for this attempt, including attempts before an
    /// explicitly authorized model change within the same aggregate ledger.
    pub model: String,
}

/// A content-free aggregate view of the live request ledger.
///
/// The public type retains its original name for existing report callers; it
/// records attempts and imposes no count, time, or concurrency ceiling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveVerificationBudgetSnapshot {
    /// Attempts reserved by this aggregate live suite.
    pub attempted_requests: u64,
    /// Currently live provider streams.
    pub active_requests: u64,
    /// Per-attempt records, in reservation order.
    pub attempts: Vec<LiveAttemptReservation>,
}

/// A model/provider pair admitted only by [`RestrictedCodexFactory`].
#[derive(Clone)]
pub struct RestrictedCodexConsumer {
    model: ModelDescriptor,
    provider: Arc<dyn ModelProvider>,
    role: VerificationConsumer,
}

impl RestrictedCodexConsumer {
    /// Borrow the exact provider-neutral model descriptor installed in this consumer.
    pub fn model(&self) -> &ModelDescriptor {
        &self.model
    }

    /// Clone the guarded provider handle.
    ///
    /// This is a wrapper, not the concrete Codex adapter: every stream still validates the exact
    /// descriptor and records its request in the aggregate ledger before transport.
    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    /// Return the fixed live-suite role attached to this guarded consumer.
    pub fn role(&self) -> VerificationConsumer {
        self.role
    }
}

/// A single-model, fixed-effort provider factory for optional live verification.
///
/// The raw configured provider is retained privately. Root, child, compaction, candidate, and
/// comparison consumers receive only independently labelled wrappers that share this factory's
/// descriptor, reasoning guard, and persisted request ledger.
pub struct RestrictedCodexFactory {
    model: ModelDescriptor,
    provider: Arc<dyn ModelProvider>,
    budget: Arc<Mutex<LiveRequestLedger>>,
    evidence: CodexModelEvidence,
    ledger_path: PathBuf,
}

impl RestrictedCodexFactory {
    /// Construct the restricted factory from an explicit Codex credential path and
    /// a caller-owned ledger outside the source tree.
    ///
    /// The factory never discovers credentials or sends a request during construction.
    /// Installed Codex client auth is read-only and reloaded at each request.
    pub fn new(
        credential_path: PathBuf,
        evidence: CodexModelEvidence,
        ledger_path: PathBuf,
    ) -> Result<Self, LiveVerificationError> {
        evidence.require_current_utc_date()?;
        let tea_owned = credential_path.file_name().is_some_and(|name| name == "codex.json")
            && credential_path.parent().and_then(Path::file_name).is_some_and(|name| name == "auth");
        let client_owned = credential_path.file_name().is_some_and(|name| name == "auth.json")
            && credential_path.parent().and_then(Path::file_name).is_some_and(|name| name == ".codex");
        if !credential_path.is_absolute() || !credential_path.is_file() || !(tea_owned || client_owned) {
            return Err(LiveVerificationError::new(
                "live verification requires an absolute Codex auth.json or Tea auth/codex.json credential path",
            ));
        }
        let store: Arc<dyn CredentialStore> = if client_owned {
            Arc::new(CodexClientCredentialStore::new(credential_path))
        } else {
            Arc::new(FileCredentialStore::new(credential_path))
        };
        let auth = Arc::new(CodexAuthManager::with_system_clock(store));
        let config = CodexConfig::try_new(auth, CODEX_MODEL_ID)
            .map_err(|error| LiveVerificationError::new(error.to_string()))?
            .with_retry_policy(RetryPolicy::new(0, Duration::ZERO, Duration::ZERO));
        // The normal provider catalog intentionally remains a product-selection
        // surface and may lag this independently reviewed verification candidate.
        // This feature-only factory therefore constructs the exact descriptor
        // itself rather than falling back to a catalog neighbor.
        let model = ModelDescriptor {
            provider: CODEX_PROVIDER_ID.into(),
            model: CODEX_MODEL_ID.into(),
            revision: None,
        };
        if !is_exact_codex_descriptor(&model) {
            return Err(LiveVerificationError::new(
                "live verification could not construct the exact selected Codex descriptor",
            ));
        }
        let configured = ConfiguredProvider {
            descriptor: model.clone(),
            provider: Arc::new(CodexProvider::new(config)),
        };
        if configured.descriptor != model {
            return Err(LiveVerificationError::new(
                "live verification configured a descriptor different from its exact route",
            ));
        }
        let budget = LiveRequestLedger::open(ledger_path.clone(), &evidence)?;
        Ok(Self {
            model,
            provider: configured.provider,
            budget: Arc::new(Mutex::new(budget)),
            evidence,
            ledger_path,
        })
    }

    /// Create one guarded provider/model consumer for a required live-suite role.
    pub fn consumer(&self, consumer: VerificationConsumer) -> RestrictedCodexConsumer {
        RestrictedCodexConsumer {
            model: self.model.clone(),
            provider: Arc::new(LedgeredCodexProvider {
                inner: Arc::clone(&self.provider),
                expected_model: self.model.clone(),
                consumer,
                budget: Arc::clone(&self.budget),
            }),
            role: consumer,
        }
    }

    /// Return content-free aggregate request-ledger evidence.
    pub fn budget_snapshot(&self) -> Result<LiveVerificationBudgetSnapshot, LiveVerificationError> {
        self.budget
            .lock()
            .map_err(|_| LiveVerificationError::new("live-verification ledger lock is poisoned"))?
            .snapshot()
    }

    /// Reload persisted aggregate reservations after a serialized verification child process.
    ///
    /// A separate one-shot executable has its own guarded factory instance, so it persists its
    /// reservation in the same ledger before transport. The parent must reload only while it has
    /// no active stream; that prevents a stale in-memory snapshot from understating the suite
    /// request history after the child exits.
    pub fn reload_budget_snapshot(
        &self,
    ) -> Result<LiveVerificationBudgetSnapshot, LiveVerificationError> {
        let refreshed = LiveRequestLedger::open(self.ledger_path.clone(), &self.evidence)?;
        let mut budget = self
            .budget
            .lock()
            .map_err(|_| LiveVerificationError::new("live-verification ledger lock is poisoned"))?;
        if budget.active_requests != 0 {
            return Err(LiveVerificationError::new(
                "live verification cannot reload its persisted ledger while a provider stream is active",
            ));
        }
        *budget = refreshed;
        budget.snapshot()
    }
}

/// Explicit filesystem and consumer inputs for one synthetic headless live case.
///
/// Both directories must already exist and be caller-owned temporary locations. The prompt must
/// be disposable synthetic or deliberately public text; this type never accepts an ambient
/// workspace, terminal configuration, or ambient credential source.
pub struct HeadlessLiveCase<'a> {
    /// Existing caller-owned Tea home for this one case.
    pub tea_home: &'a Path,
    /// Existing caller-owned disposable workspace for this one case.
    pub workspace: &'a Path,
    /// Fixed role expected to own the model request.
    pub role: VerificationConsumer,
    /// Restricted consumer constructed by the one suite factory.
    pub consumer: &'a RestrictedCodexConsumer,
    /// Restricted compaction consumer, required only by the compaction case.
    pub compactor: Option<&'a RestrictedCodexConsumer>,
    /// Drive through the terminal's distinct one-shot completion path.
    pub one_shot: bool,
    /// Close and passively reopen the resulting durable session before returning.
    pub passive_reopen: bool,
    /// Optional exact synthetic assistant response that is checked without retaining it.
    pub expected_response: Option<&'a str>,
    /// Public or synthetic prompt sent through the real headless harness.
    pub prompt: &'a str,
}

/// Content-free settlement evidence from one headless real-provider case.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessLiveCaseOutcome {
    /// Whether the durable operation settled normally.
    pub operation_completed: bool,
    /// Whether the durable session verifies after settlement.
    pub durable_state_verified: bool,
    /// Whether an optional exact synthetic response oracle matched.
    pub response_oracle_verified: bool,
}

/// Run one explicit synthetic prompt through Tea's feature-only headless host.
///
/// The terminal composition seam receives only the guarded consumer's descriptor and provider;
/// it cannot select a fallback model or recover a credential. This is a real provider transport
/// path when called, so callers must supply an explicit credential and disposable input.
/// It retains no model output in the returned evidence.
pub fn run_headless_live_case(
    case: HeadlessLiveCase<'_>,
) -> Result<HeadlessLiveCaseOutcome, LiveVerificationError> {
    if case.role != case.consumer.role() {
        return Err(LiveVerificationError::new(
            "live verification headless case received a consumer for a different role",
        ));
    }
    if !is_exact_codex_descriptor(case.consumer.model()) {
        return Err(LiveVerificationError::new(
            "live verification headless case refused a non-canonical model descriptor",
        ));
    }
    if !case.tea_home.is_dir() || !case.workspace.is_dir() {
        return Err(LiveVerificationError::new(
            "live verification headless case requires explicit existing temporary home and workspace directories",
        ));
    }
    if case.prompt.trim().is_empty() {
        return Err(LiveVerificationError::new(
            "live verification headless case refuses an empty synthetic prompt",
        ));
    }
    let compactor_provider = match case.compactor {
        None => None,
        Some(compactor) => {
            if compactor.role() != VerificationConsumer::Compaction
                || !is_exact_codex_descriptor(compactor.model())
            {
                return Err(LiveVerificationError::new(
                    "live verification headless case refused a non-canonical compaction consumer",
                ));
            }
            Some((compactor.model().clone(), compactor.provider()))
        }
    };
    let harness = crate::app::create_live_verification_harness(
        case.tea_home,
        case.workspace,
        case.consumer.model().clone(),
        case.consumer.provider(),
        compactor_provider.clone(),
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let operation_completed = if case.one_shot {
        smol::block_on(crate::app::run_live_verification_one_shot(
            Arc::clone(&harness),
            case.prompt.to_owned(),
        ))
        .map(|()| true)
        .map_err(|error| LiveVerificationError::new(error.to_string()))
    } else {
        smol::block_on(harness.run_root_prompt(case.prompt))
            .map(|operation| operation.is_completed())
            .map_err(|error| LiveVerificationError::new(error.to_string()))
    };
    let verification = harness
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()));
    let snapshot = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let response_oracle_verified = case
        .expected_response
        .is_none_or(|expected| has_exact_assistant_response(&snapshot, expected));
    let session_id = snapshot.header().session_id.to_string();
    let closed = smol::block_on(harness.close()).map_err(|error| LiveVerificationError::new(error.to_string()));
    let operation_completed = operation_completed?;
    verification?;
    closed?;
    if case.passive_reopen {
        let reopened = crate::app::reopen_live_verification_harness(
            case.tea_home,
            case.workspace,
            &session_id,
            case.consumer.model().clone(),
            case.consumer.provider(),
            compactor_provider,
        )
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
        reopened
            .verify_durable_state()
            .map_err(|error| LiveVerificationError::new(error.to_string()))?;
        smol::block_on(reopened.close())
            .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    }
    Ok(HeadlessLiveCaseOutcome {
        operation_completed,
        durable_state_verified: true,
        response_oracle_verified,
    })
}

/// Explicit inputs for a controlled interruption, passive reopen, and fresh continuation.
///
/// The first prompt is deliberately interrupted only after a provider request has become
/// durable. The continuation is a separate user input on a newly reopened harness, so this
/// helper cannot turn a partial answer into an implicit retry.
pub struct ControlledRecoveryLiveCase<'a> {
    /// Existing caller-owned Tea home for this one case.
    pub tea_home: &'a Path,
    /// Existing caller-owned disposable workspace for this one case.
    pub workspace: &'a Path,
    /// Restricted root consumer constructed by the one suite factory.
    pub consumer: &'a RestrictedCodexConsumer,
    /// Synthetic request intentionally interrupted after durable provider admission.
    pub interrupted_prompt: &'a str,
    /// Fresh safe user request driven only after passive reopen.
    pub continuation_prompt: &'a str,
    /// Exact synthetic continuation response checked without retaining it.
    pub expected_continuation_response: &'a str,
}

/// Content-free evidence from a controlled live recovery exercise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlledRecoveryLiveCaseOutcome {
    /// A provider request was durably admitted before cancellation was requested.
    pub provider_request_observed: bool,
    /// The interrupted operation did not complete normally.
    pub interruption_settled: bool,
    /// Passive reopen verified durable state before any fresh input was admitted.
    pub passive_reopen_verified: bool,
    /// The explicit fresh continuation completed normally.
    pub continuation_completed: bool,
    /// The continuation's output-free synthetic response oracle matched.
    pub continuation_response_verified: bool,
}

/// Exercise controlled cancellation and recovery with only the restricted root provider.
///
/// This is intentionally separate from [`run_headless_live_case`]: recovery is observable only
/// when cancellation follows a durable provider-request admission and a new supervisor verifies
/// the prefix before a fresh continuation is accepted.
pub fn run_controlled_recovery_live_case(
    case: ControlledRecoveryLiveCase<'_>,
) -> Result<ControlledRecoveryLiveCaseOutcome, LiveVerificationError> {
    if case.consumer.role() != VerificationConsumer::Root
        || !is_exact_codex_descriptor(case.consumer.model())
    {
        return Err(LiveVerificationError::new(
            "live recovery requires the restricted canonical root consumer",
        ));
    }
    if !case.tea_home.is_dir() || !case.workspace.is_dir() {
        return Err(LiveVerificationError::new(
            "live recovery requires explicit existing temporary home and workspace directories",
        ));
    }
    for (label, value) in [
        ("interrupted prompt", case.interrupted_prompt),
        ("continuation prompt", case.continuation_prompt),
        ("expected continuation response", case.expected_continuation_response),
    ] {
        if value.trim().is_empty() {
            return Err(LiveVerificationError::new(format!(
                "live recovery refuses an empty synthetic {label}",
            )));
        }
    }

    let harness = crate::app::create_live_verification_harness(
        case.tea_home,
        case.workspace,
        case.consumer.model().clone(),
        case.consumer.provider(),
        None,
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let drive_harness = Arc::clone(&harness);
    let interrupted_prompt = case.interrupted_prompt.to_owned();
    let drive = smol::spawn(async move { drive_harness.run_root_prompt(interrupted_prompt).await });
    let interruption_settled = smol::block_on(async {
        if wait_for_provider_admission(|| has_durable_provider_request(&harness)).await? {
            if !harness
                .abort_root()
                .map_err(|error| LiveVerificationError::new(error.to_string()))?
            {
                return Err(LiveVerificationError::new(
                    "live recovery lost the active root operation before controlled cancellation",
                ));
            }
            return interrupted_run_settled(drive.await);
        }
        Err(LiveVerificationError::new(
            "live recovery did not observe durable provider admission before cancellation",
        ))
    })?;
    if !interruption_settled {
        return Err(LiveVerificationError::new(
            "live recovery operation completed before controlled interruption",
        ));
    }
    harness
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let session_id = harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?
        .header()
        .session_id
        .to_string();
    smol::block_on(harness.close()).map_err(|error| LiveVerificationError::new(error.to_string()))?;
    // The passive reopen must acquire a fresh writer after the old supervisor
    // has closed and released its last session handle.
    drop(harness);

    let reopened = crate::app::reopen_live_verification_harness(
        case.tea_home,
        case.workspace,
        &session_id,
        case.consumer.model().clone(),
        case.consumer.provider(),
        None,
    )
    .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    reopened
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    if !reopened
        .recovery_report()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?
        .lanes
        .is_empty()
    {
        return Err(LiveVerificationError::new(
            "passively reopened live recovery session still requires reconciliation",
        ));
    }
    let continuation = smol::block_on(reopened.run_root_prompt(case.continuation_prompt))
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let continuation_completed = continuation.is_completed();
    let snapshot = reopened
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    let continuation_response_verified = has_exact_assistant_response(
        &snapshot,
        case.expected_continuation_response,
    );
    reopened
        .verify_durable_state()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?;
    smol::block_on(reopened.close()).map_err(|error| LiveVerificationError::new(error.to_string()))?;
    Ok(ControlledRecoveryLiveCaseOutcome {
        provider_request_observed: true,
        interruption_settled: true,
        passive_reopen_verified: true,
        continuation_completed,
        continuation_response_verified,
    })
}

fn interrupted_run_settled(
    result: Result<DurableOperation, tea_core::harness::HarnessError>,
) -> Result<bool, LiveVerificationError> {
    match result {
        Ok(operation) => Ok(!operation.is_completed()),
        Err(tea_core::harness::HarnessError::Core(tea_core::error::CoreError::Cancelled)) => Ok(true),
        Err(error) => Err(LiveVerificationError::new(error.to_string())),
    }
}

fn has_durable_provider_request(
    harness: &SessionSupervisor<tea_session::JsonlSession>,
) -> Result<bool, LiveVerificationError> {
    Ok(harness
        .snapshot()
        .map_err(|error| LiveVerificationError::new(error.to_string()))?
        .records()
        .iter()
        .any(|record| matches!(record.record, tea_session::LaneRecord::ProviderRequestStarted(_))))
}

async fn wait_for_provider_admission(
    mut observed: impl FnMut() -> Result<bool, LiveVerificationError>,
) -> Result<bool, LiveVerificationError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if observed()? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        smol::Timer::after(Duration::from_millis(10)).await;
    }
}

fn has_exact_assistant_response(
    snapshot: &tea_session::SessionSnapshot,
    expected: &str,
) -> bool {
    snapshot
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match &entry.body {
            SessionEntry::AssistantMessage(message) => Some(message.content.trim() == expected),
            _ => None,
        })
        .unwrap_or(false)
}

/// A failure that blocks live verification without weakening its offline counterpart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveVerificationError {
    message: String,
}

impl LiveVerificationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for LiveVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LiveVerificationError {}

struct LedgeredCodexProvider {
    inner: Arc<dyn ModelProvider>,
    expected_model: ModelDescriptor,
    consumer: VerificationConsumer,
    budget: Arc<Mutex<LiveRequestLedger>>,
}

impl ModelProvider for LedgeredCodexProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        if request.model.as_ref() != Some(&self.expected_model)
            || request.thinking_level != CODEX_REASONING_EFFORT
        {
            return Box::pin(std::future::ready(Ok(Box::new(RejectedEventStream::new(
                "live verification refused a mismatched Codex model or reasoning effort before transport",
            )) as Box<dyn ModelEventStream>)));
        }
        match self
            .budget
            .lock()
            .map_err(|_| LiveVerificationError::new("live-verification ledger lock is poisoned"))
            .and_then(|mut budget| budget.reserve(self.consumer))
        {
            Ok(()) => {}
            Err(error) => {
                return Box::pin(std::future::ready(Ok(Box::new(RejectedEventStream::new(
                    error.to_string(),
                )) as Box<dyn ModelEventStream>)));
            }
        }
        let permit = LivePermit {
            budget: Some(Arc::clone(&self.budget)),
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match inner.stream(request, cancellation).await {
                Ok(stream) => Ok(Box::new(LedgeredEventStream {
                    inner: stream,
                    permit: Some(permit),
                    finished: false,
                }) as Box<dyn ModelEventStream>),
                Err(error) => Err(error),
            }
        })
    }
}

struct LedgeredEventStream {
    inner: Box<dyn ModelEventStream>,
    permit: Option<LivePermit>,
    finished: bool,
}

impl LedgeredEventStream {
    fn release(&mut self) {
        self.permit.take();
    }
}

impl ModelEventStream for LedgeredEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(async move {
            if self.finished {
                return Ok(None);
            }
            let event = self.inner.next_event(cancellation).await;
            if event.is_err()
                || matches!(
                    &event,
                    Ok(None)
                        | Ok(Some(ModelStreamEvent::End(_)))
                        | Ok(Some(ModelStreamEvent::Error { .. }))
                        | Ok(Some(ModelStreamEvent::ContextOverflow { .. }))
                        | Ok(Some(ModelStreamEvent::Aborted { .. }))
                )
            {
                self.release();
                self.finished = true;
            }
            event
        })
    }
}

struct RejectedEventStream {
    events: VecDeque<ModelStreamEvent>,
}

impl RejectedEventStream {
    fn new(message: impl Into<String>) -> Self {
        Self {
            events: [ModelStreamEvent::Error {
                message: message.into(),
            }]
            .into(),
        }
    }
}

impl ModelEventStream for RejectedEventStream {
    fn next_event<'a>(&'a mut self, _cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(std::future::ready(Ok(self.events.pop_front())))
    }
}

struct LiveRequestLedger {
    ledger_path: PathBuf,
    checked_on: String,
    started_at_unix_seconds: u64,
    attempted_requests: u64,
    active_requests: u64,
    attempts: Vec<LiveAttemptReservation>,
}

impl LiveRequestLedger {
    fn open(path: PathBuf, evidence: &CodexModelEvidence) -> Result<Self, LiveVerificationError> {
        let now = unix_seconds()?;
        match fs::read_to_string(&path) {
            Ok(source) => Self::from_ledger_json(&path, evidence, &source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let budget = Self {
                    ledger_path: path,
                    checked_on: evidence.checked_on.clone(),
                    started_at_unix_seconds: now,
                    attempted_requests: 0,
                    active_requests: 0,
                    attempts: Vec::new(),
                };
                budget.persist()?;
                Ok(budget)
            }
            Err(error) => Err(LiveVerificationError::new(format!(
                "cannot read live-verification request ledger {}: {error}",
                path.display()
            ))),
        }
    }

    fn from_ledger_json(
        path: &Path,
        evidence: &CodexModelEvidence,
        source: &str,
    ) -> Result<Self, LiveVerificationError> {
        let value = JsonValue::parse(source)
            .map_err(|error| LiveVerificationError::new(format!("invalid live-verification ledger: {error}")))?;
        let record = object(&value, "live-verification ledger")?;
        required_string(record, "schema_version", "live-verification ledger")
            .and_then(|schema| require_exact(schema, LEDGER_SCHEMA, "schema_version"))?;
        required_string(record, "provider", "live-verification ledger")
            .and_then(|provider| require_exact(provider, CODEX_PROVIDER_ID, "provider"))?;
        required_string(record, "model", "live-verification ledger")
            .and_then(|model| require_exact(model, CODEX_MODEL_ID, "model"))?;
        required_string(record, "endpoint", "live-verification ledger")
            .and_then(|endpoint| require_exact(endpoint, CODEX_RESPONSES_ENDPOINT, "endpoint"))?;
        required_string(record, "reasoning_effort", "live-verification ledger")
            .and_then(|effort| require_exact(effort, "low", "reasoning_effort"))?;
        required_string(record, "checked_on", "live-verification ledger")
            .and_then(|checked_on| require_exact(checked_on, evidence.checked_on(), "checked_on"))?;
        let started_at_unix_seconds = required_u64(record, "started_at_unix_seconds", "live-verification ledger")?;
        let attempted_requests = required_u64(record, "attempted_requests", "live-verification ledger")?;
        let attempts = record
            .get("attempts")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| LiveVerificationError::new("live-verification ledger omits attempts"))?
            .iter()
            .map(parse_attempt)
            .collect::<Result<Vec<_>, _>>()?;
        if attempted_requests != attempts.len() as u64 {
            return Err(LiveVerificationError::new(
                "live-verification ledger attempt count does not match attempt records",
            ));
        }
        if attempts.iter().enumerate().any(|(index, attempt)| {
            attempt.sequence != index as u64 + 1
                || (attempt.model != CODEX_MODEL_ID && attempt.model != "gpt-6-luna")
        }) {
            return Err(LiveVerificationError::new(
                "live-verification ledger has an invalid sequence or unapproved model history",
            ));
        }
        let mut current_model_seen = false;
        for attempt in &attempts {
            if attempt.model == CODEX_MODEL_ID {
                current_model_seen = true;
            } else if current_model_seen {
                return Err(LiveVerificationError::new(
                    "live-verification ledger cannot return to the rejected model",
                ));
            }
        }
        Ok(Self {
            ledger_path: path.to_owned(),
            checked_on: evidence.checked_on.clone(),
            started_at_unix_seconds,
            attempted_requests,
            active_requests: 0,
            attempts,
        })
    }

    fn reserve(&mut self, consumer: VerificationConsumer) -> Result<(), LiveVerificationError> {
        let next_attempt = self.attempted_requests.saturating_add(1);
        if next_attempt == self.attempted_requests {
            return Err(LiveVerificationError::new("live verification attempt counter overflow"));
        }
        self.attempted_requests = next_attempt;
        self.active_requests = self.active_requests.saturating_add(1);
        self.attempts.push(LiveAttemptReservation {
            sequence: next_attempt,
            consumer,
            model: CODEX_MODEL_ID.into(),
        });
        if let Err(error) = self.persist() {
            self.attempted_requests = self.attempted_requests.saturating_sub(1);
            self.active_requests = self.active_requests.saturating_sub(1);
            self.attempts.pop();
            return Err(error);
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<LiveVerificationBudgetSnapshot, LiveVerificationError> {
        Ok(LiveVerificationBudgetSnapshot {
            attempted_requests: self.attempted_requests,
            active_requests: self.active_requests,
            attempts: self.attempts.clone(),
        })
    }

    fn persist(&self) -> Result<(), LiveVerificationError> {
        let parent = self.ledger_path.parent().ok_or_else(|| {
            LiveVerificationError::new("live-verification ledger must have an explicit parent directory")
        })?;
        if !parent.is_dir() {
            return Err(LiveVerificationError::new(format!(
                "live-verification ledger parent is not a directory: {}",
                parent.display()
            )));
        }
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            self.ledger_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| LiveVerificationError::new("live-verification ledger file name is invalid"))?,
            std::process::id(),
        ));
        let value = self.ledger_json();
        let encoded = value.to_json_string_pretty().map_err(|error| {
            LiveVerificationError::new(format!("cannot encode live-verification ledger: {error}"))
        })?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| {
                LiveVerificationError::new(format!(
                    "cannot create live-verification ledger temporary file {}: {error}",
                    temporary.display()
                ))
            })?;
        if let Err(error) = file
            .write_all(encoded.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
        {
            let _ = fs::remove_file(&temporary);
            return Err(LiveVerificationError::new(format!(
                "cannot persist live-verification request ledger before transport: {error}"
            )));
        }
        fs::rename(&temporary, &self.ledger_path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            LiveVerificationError::new(format!(
                "cannot publish live-verification request ledger before transport: {error}"
            ))
        })
    }

    fn ledger_json(&self) -> JsonValue {
        JsonValue::object([
            ("schema_version", JsonValue::from(LEDGER_SCHEMA)),
            ("provider", JsonValue::from(CODEX_PROVIDER_ID)),
            ("model", JsonValue::from(CODEX_MODEL_ID)),
            ("endpoint", JsonValue::from(CODEX_RESPONSES_ENDPOINT)),
            ("reasoning_effort", JsonValue::from("low")),
            ("checked_on", JsonValue::from(self.checked_on.clone())),
            (
                "started_at_unix_seconds",
                JsonValue::from(self.started_at_unix_seconds),
            ),
            ("attempted_requests", JsonValue::from(self.attempted_requests)),
            (
                "attempts",
                JsonValue::Array(
                    self.attempts
                        .iter()
                        .map(|attempt| {
                            JsonValue::object([
                                ("sequence", JsonValue::from(attempt.sequence)),
                                ("consumer", JsonValue::from(attempt.consumer.as_str())),
                                ("model", JsonValue::from(attempt.model.clone())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

struct LivePermit {
    budget: Option<Arc<Mutex<LiveRequestLedger>>>,
}

impl Drop for LivePermit {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.take() {
            if let Ok(mut budget) = budget.lock() {
                budget.active_requests = budget.active_requests.saturating_sub(1);
            }
        }
    }
}

fn parse_attempt(value: &JsonValue) -> Result<LiveAttemptReservation, LiveVerificationError> {
    let record = object(value, "live-verification attempt")?;
    let consumer = match required_string(record, "consumer", "live-verification attempt")? {
        "root" => VerificationConsumer::Root,
        "child" => VerificationConsumer::Child,
        "compaction" => VerificationConsumer::Compaction,
        "candidate_evaluation" => VerificationConsumer::CandidateEvaluation,
        "comparison" => VerificationConsumer::Comparison,
        _ => return Err(LiveVerificationError::new("live-verification ledger has an unknown consumer")),
    };
    Ok(LiveAttemptReservation {
        sequence: required_u64(record, "sequence", "live-verification attempt")?,
        consumer,
        model: required_string(record, "model", "live-verification attempt")?.to_owned(),
    })
}

fn is_exact_codex_descriptor(model: &ModelDescriptor) -> bool {
    model.provider == CODEX_PROVIDER_ID && model.model == CODEX_MODEL_ID && model.revision.is_none()
}

fn object<'a>(
    value: &'a JsonValue,
    label: &str,
) -> Result<&'a std::collections::BTreeMap<String, JsonValue>, LiveVerificationError> {
    value
        .as_object()
        .ok_or_else(|| LiveVerificationError::new(format!("{label} must be a JSON object")))
}

fn required_string<'a>(
    object: &'a std::collections::BTreeMap<String, JsonValue>,
    field: &str,
    label: &str,
) -> Result<&'a str, LiveVerificationError> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| LiveVerificationError::new(format!("{label} must contain string {field:?}")))
}

fn required_u64(
    object: &std::collections::BTreeMap<String, JsonValue>,
    field: &str,
    label: &str,
) -> Result<u64, LiveVerificationError> {
    object
        .get(field)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| LiveVerificationError::new(format!("{label} must contain unsigned {field:?}")))
}

fn require_exact(value: &str, expected: &str, field: &str) -> Result<(), LiveVerificationError> {
    if value == expected {
        Ok(())
    } else {
        Err(LiveVerificationError::new(format!(
            "live verification refused unexpected {field:?}: {value:?}"
        )))
    }
}

fn is_iso_date(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn current_utc_date() -> Result<String, LiveVerificationError> {
    let seconds = unix_seconds()?;
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| LiveVerificationError::new("system clock date exceeds supported range"))?;
    let (year, month, day) = civil_date_from_unix_days(days);
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

// Convert days after 1970-01-01 to a proleptic Gregorian UTC calendar date.
// This compact integer algorithm keeps the guard dependency-free and avoids a locale-sensitive
// external `date` command at the security boundary.
fn civil_date_from_unix_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

fn unix_seconds() -> Result<u64, LiveVerificationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LiveVerificationError::new("system clock predates the Unix epoch"))
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingProvider {
        calls: AtomicUsize,
    }

    impl ModelProvider for CountingProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(Ok(Box::new(RejectedEventStream::new("fixture")) as _)))
        }
    }

    fn evidence_record(checked_on: &str) -> JsonValue {
        JsonValue::object([
            ("schema_version", JsonValue::from(EVIDENCE_SCHEMA)),
            ("model_source", JsonValue::from(CODEX_MODEL_SOURCE)),
            ("provider", JsonValue::from(CODEX_PROVIDER_ID)),
            ("model", JsonValue::from(CODEX_MODEL_ID)),
            ("endpoint", JsonValue::from(CODEX_RESPONSES_ENDPOINT)),
            ("reasoning_effort", JsonValue::from("low")),
            ("checked_on", JsonValue::from(checked_on)),
            ("synthetic_or_public_fixture_only", JsonValue::Bool(true)),
        ])
    }

    fn evidence() -> CodexModelEvidence {
        let checked_on = current_utc_date().expect("system UTC date is available");
        CodexModelEvidence::from_json(&evidence_record(&checked_on))
            .expect("fixture evidence is exact")
    }

    fn temporary_ledger(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tea-codex-live-verification-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is available").as_nanos()
        ))
    }

    fn temporary_credential(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "tea-codex-live-auth-{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is available").as_nanos()
        ));
        fs::create_dir_all(&directory).expect("temporary auth directory creates");
        let auth_directory = directory.join("auth");
        fs::create_dir(&auth_directory).expect("temporary auth subdirectory creates");
        let path = auth_directory.join("codex.json");
        fs::write(&path, "{}\n").expect("placeholder credential creates");
        path
    }

    fn guarded_provider(inner: Arc<CountingProvider>) -> Arc<dyn ModelProvider> {
        let ledger = temporary_ledger("guard");
        let budget = LiveRequestLedger::open(ledger.clone(), &evidence()).expect("ledger opens");
        let provider: Arc<dyn ModelProvider> = Arc::new(LedgeredCodexProvider {
            inner,
            expected_model: ModelDescriptor {
                provider: CODEX_PROVIDER_ID.into(),
                model: CODEX_MODEL_ID.into(),
                revision: None,
            },
            consumer: VerificationConsumer::Root,
            budget: Arc::new(Mutex::new(budget)),
        });
        let _ = fs::remove_file(ledger);
        provider
    }

    fn request(provider: &str, model: &str, thinking_level: ThinkingLevel) -> ModelRequest {
        ModelRequest {
            model: Some(ModelDescriptor {
                provider: provider.into(),
                model: model.into(),
                revision: None,
            }),
            thinking_level,
            ..ModelRequest::default()
        }
    }

    #[test]
    fn model_evidence_rejects_wrong_model_effort_or_endpoint() {
        let mut record = evidence_record("2026-09-21");
        for (field, invalid) in [
            ("model", "gpt-6-sol"),
            ("reasoning_effort", "medium"),
            ("endpoint", "https://other.invalid/responses"),
        ] {
            record.as_object_mut().expect("object").insert(field.into(), JsonValue::from(invalid));
            assert!(CodexModelEvidence::from_json(&record).is_err(), "{field} must be exact");
            record = evidence_record("2026-09-21");
        }
    }

    #[test]
    fn mismatched_model_or_effort_never_reaches_transport() {
        let inner = Arc::new(CountingProvider::default());
        let provider = guarded_provider(Arc::clone(&inner));
        for (provider_id, model, effort) in [
            (CODEX_PROVIDER_ID, "gpt-6-sol", ThinkingLevel::Low),
            ("openrouter", CODEX_MODEL_ID, ThinkingLevel::Low),
            (CODEX_PROVIDER_ID, CODEX_MODEL_ID, ThinkingLevel::Off),
            (CODEX_PROVIDER_ID, CODEX_MODEL_ID, ThinkingLevel::Medium),
        ] {
            let cancellation = CancellationToken::new();
            let mut stream = smol::block_on(provider.stream(request(provider_id, model, effort), cancellation.clone()))
                .expect("guard always yields a local rejection stream");
            assert!(matches!(
                smol::block_on(stream.next_event(cancellation)),
                Ok(Some(ModelStreamEvent::Error { .. }))
            ));
        }
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn guarded_stream_closes_after_terminal_provider_error() {
        let inner = Arc::new(CountingProvider::default());
        let provider = guarded_provider(Arc::clone(&inner));
        let cancellation = CancellationToken::new();
        let mut stream = smol::block_on(provider.stream(
            request(CODEX_PROVIDER_ID, CODEX_MODEL_ID, ThinkingLevel::Low),
            cancellation.clone(),
        ))
        .expect("guarded stream opens");
        assert!(matches!(
            smol::block_on(stream.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::Error { .. }))
        ));
        assert!(matches!(
            smol::block_on(stream.next_event(cancellation)),
            Ok(None)
        ));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ledger_retains_rejected_model_attempts_across_authorized_model_change() {
        let now = unix_seconds().expect("clock is available");
        let attempts = ["gpt-6-luna", CODEX_MODEL_ID]
            .into_iter()
            .enumerate()
            .map(|(index, model)| JsonValue::object([
                ("sequence", JsonValue::from(index as u64 + 1)),
                ("consumer", JsonValue::from("root")),
                ("model", JsonValue::from(model)),
            ]))
            .collect();
        let ledger = JsonValue::object([
            ("schema_version", JsonValue::from(LEDGER_SCHEMA)),
            ("provider", JsonValue::from(CODEX_PROVIDER_ID)),
            ("model", JsonValue::from(CODEX_MODEL_ID)),
            ("endpoint", JsonValue::from(CODEX_RESPONSES_ENDPOINT)),
            ("reasoning_effort", JsonValue::from("low")),
            ("checked_on", JsonValue::from(evidence().checked_on())),
            ("started_at_unix_seconds", JsonValue::from(now)),
            ("attempted_requests", JsonValue::from(2_u64)),
            ("attempts", JsonValue::Array(attempts)),
        ]);
        let budget = LiveRequestLedger::from_ledger_json(
            Path::new("/tmp/tea-codex-ledger-test.json"),
            &evidence(),
            &ledger.to_json_string().expect("ledger encodes"),
        )
        .expect("explicit model change retains earlier attempts");
        assert_eq!(budget.attempted_requests, 2);
        assert_eq!(budget.attempts[0].model, "gpt-6-luna");
        assert_eq!(budget.attempts[1].model, CODEX_MODEL_ID);
    }

    #[test]
    fn recorded_attempts_remain_usable_after_the_former_task_limits() {
        let now = unix_seconds().expect("clock is available");
        let attempts = (1..=41_u64)
            .map(|sequence| JsonValue::object([
                ("sequence", JsonValue::from(sequence)),
                ("consumer", JsonValue::from("root")),
                ("model", JsonValue::from(CODEX_MODEL_ID)),
            ]))
            .collect();
        let ledger = JsonValue::object([
            ("schema_version", JsonValue::from(LEDGER_SCHEMA)),
            ("provider", JsonValue::from(CODEX_PROVIDER_ID)),
            ("model", JsonValue::from(CODEX_MODEL_ID)),
            ("endpoint", JsonValue::from(CODEX_RESPONSES_ENDPOINT)),
            ("reasoning_effort", JsonValue::from("low")),
            ("checked_on", JsonValue::from(evidence().checked_on())),
            ("started_at_unix_seconds", JsonValue::from(now - 3_600)),
            ("attempted_requests", JsonValue::from(41_u64)),
            ("attempts", JsonValue::Array(attempts)),
        ]);
        let mut record = LiveRequestLedger::from_ledger_json(
            Path::new("/tmp/tea-codex-recording-test.json"),
            &evidence(),
            &ledger.to_json_string().expect("ledger encodes"),
        )
        .expect("prior request count and elapsed time do not block the next run");
        record.ledger_path = temporary_ledger("continued-recording");
        record.reserve(VerificationConsumer::Root).expect("next attempt records");
        assert_eq!(record.attempted_requests, 42);
        let _ = fs::remove_file(record.ledger_path);
    }

    #[test]
    fn recovery_wait_observes_delayed_durable_provider_admission() {
        use std::sync::atomic::AtomicBool;
        let admitted = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&admitted);
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            signal.store(true, Ordering::SeqCst);
        });
        let observed = smol::block_on(wait_for_provider_admission(|| {
            Ok(admitted.load(Ordering::SeqCst))
        }))
        .expect("provider-admission wait succeeds");
        producer.join().expect("delayed admission producer exits");
        assert!(observed, "a real provider needs wall time to commit admission");
    }

    #[test]
    fn controlled_cancellation_settles_the_interrupted_operation() {
        let interrupted = interrupted_run_settled(Err(tea_core::harness::HarnessError::Core(
            tea_core::error::CoreError::Cancelled,
        )));
        assert!(interrupted.expect("intentional cancellation is a settled interruption"));
    }

    #[test]
    fn all_consumer_roles_share_exact_codex_descriptor_and_ledger() {
        let ledger = temporary_ledger("factory");
        let credential = temporary_credential("factory");
        let factory = RestrictedCodexFactory::new(credential.clone(), evidence(), ledger.clone())
            .expect("factory configures without transport");
        for consumer in [
            VerificationConsumer::Root,
            VerificationConsumer::Child,
            VerificationConsumer::Compaction,
            VerificationConsumer::CandidateEvaluation,
            VerificationConsumer::Comparison,
        ] {
            let handle = factory.consumer(consumer);
            assert_eq!(handle.model().provider, CODEX_PROVIDER_ID);
            assert_eq!(handle.model().model, CODEX_MODEL_ID);
            assert!(handle.model().revision.is_none());
        }
        assert_eq!(factory.budget_snapshot().expect("snapshot").attempted_requests, 0);
        let persisted = JsonValue::parse(&fs::read_to_string(&ledger).expect("ledger reads"))
            .expect("ledger is JSON");
        assert_eq!(persisted.get("reasoning_effort").and_then(JsonValue::as_str), Some("low"));
        assert!(persisted.get("requested_output_tokens").is_none());
        let _ = fs::remove_file(ledger);
        let _ = fs::remove_dir_all(credential.parent().and_then(Path::parent).expect("credential has a Tea home"));
    }

    #[test]
    fn stale_model_evidence_blocks_factory_before_creating_a_ledger() {
        let evidence = CodexModelEvidence::from_json(&evidence_record("2000-01-01"))
            .expect("stale evidence is syntactically valid");
        let ledger = temporary_ledger("stale-evidence");
        let credential = temporary_credential("stale-evidence");
        assert!(RestrictedCodexFactory::new(credential.clone(), evidence, ledger.clone()).is_err());
        assert!(!ledger.exists());
        let _ = fs::remove_dir_all(credential.parent().and_then(Path::parent).expect("credential has a Tea home"));
    }

    #[test]
    fn invalid_headless_paths_are_rejected_before_provider_transport() {
        let inner = Arc::new(CountingProvider::default());
        let consumer = RestrictedCodexConsumer {
            model: ModelDescriptor {
                provider: CODEX_PROVIDER_ID.into(),
                model: CODEX_MODEL_ID.into(),
                revision: None,
            },
            provider: guarded_provider(Arc::clone(&inner)),
            role: VerificationConsumer::Root,
        };
        let missing = std::env::temp_dir().join(format!(
            "tea-live-verification-missing-{}",
            unix_seconds().expect("clock is available")
        ));
        let result = run_headless_live_case(HeadlessLiveCase {
            tea_home: &missing,
            workspace: &missing,
            role: VerificationConsumer::Root,
            consumer: &consumer,
            compactor: None,
            one_shot: false,
            passive_reopen: false,
            expected_response: None,
            prompt: "public fixture",
        });
        assert!(result.is_err());
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            0,
            "invalid explicit paths must fail before provider transport"
        );
    }
}
