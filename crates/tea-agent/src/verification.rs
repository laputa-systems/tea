//! Guarded OpenCode Zen infrastructure for the repository's optional live verification suite.
//!
//! This module is compiled only with `live-verification`. It does not update the provider
//! catalog, select a model for ordinary Tea sessions, or provide a pricing abstraction. Its one
//! purpose is to keep an explicitly authorized verification run on the exact checked Zen route
//! while reserving the task-wide request budget before provider transport begins.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::scheduler::{
    CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture, ModelProvider,
    ModelRequest, ModelStreamEvent,
};
use tea_core::runtime::SessionSupervisor;
use tea_core::state::ModelDescriptor;
use tea_protocol::JsonValue;
use tea_providers::{ConfiguredProvider, RetryPolicy};
use tea_providers::opencode_zen::{OpencodeZenConfig, OpencodeZenProvider};
use tea_session::SessionEntry;

pub(crate) mod scenarios_compaction;
pub(crate) mod scenarios_children;
pub use scenarios_compaction::{
    LiveCompactionScenario, LiveCompactionScenarioOutcome, run_live_compaction_scenario,
};
pub use scenarios_children::{
    LiveChildScenario, LiveChildScenarioOutcome, run_live_child_scenario,
};

/// The sole provider identifier accepted by live verification.
pub const ZEN_PROVIDER_ID: &str = "opencode-zen";
/// The current deliberately selected free Zen model identifier.
pub const ZEN_FREE_MODEL_ID: &str = "muse-spark-1.3-contributor-free";
/// The sole Zen endpoint accepted by the guarded factory.
pub const ZEN_RESPONSES_ENDPOINT: &str = "https://opencode.ai/zen/v1/responses";
/// The official catalog page that must be rechecked before a live invocation.
pub const ZEN_CATALOG_SOURCE: &str = "https://opencode.ai/docs/zen";

const EVIDENCE_SCHEMA: &str = "tea-free-zen-catalog-evidence/v1";
const LEDGER_SCHEMA: &str = "tea-live-verification-ledger/v1";
const MAX_ATTEMPTS: u64 = 40;
const MAX_REQUESTED_OUTPUT_TOKENS: u64 = 100_000;
const MAX_WALL_TIME: Duration = Duration::from_secs(30 * 60);
const MAX_CONCURRENT_REQUESTS: u64 = 2;

/// A checked official Zen catalog record accepted by the guarded factory.
///
/// The source document changes independently of this repository. Operators must refresh this
/// record from the official page immediately before live verification; a model-name suffix or a
/// free boolean does not satisfy this contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FreeZenCatalogEvidence {
    checked_on: String,
    data_use_source: String,
    data_use_summary: String,
}

impl FreeZenCatalogEvidence {
    /// Read and strictly validate a sanitized official catalog record.
    pub fn read(path: &Path) -> Result<Self, LiveVerificationError> {
        let source = fs::read_to_string(path).map_err(|error| {
            LiveVerificationError::new(format!(
                "cannot read live-verification catalog evidence {}: {error}",
                path.display()
            ))
        })?;
        let value = JsonValue::parse(&source).map_err(|error| {
            LiveVerificationError::new(format!(
                "live-verification catalog evidence is not valid JSON: {error}"
            ))
        })?;
        Self::from_json(&value)
    }

    /// Validate a parsed sanitized official catalog record.
    pub fn from_json(value: &JsonValue) -> Result<Self, LiveVerificationError> {
        let record = object(value, "catalog evidence")?;
        required_string(record, "schema_version", "catalog evidence")
            .and_then(|schema| require_exact(schema, EVIDENCE_SCHEMA, "schema_version"))?;
        required_string(record, "catalog_source", "catalog evidence")
            .and_then(|source| require_exact(source, ZEN_CATALOG_SOURCE, "catalog_source"))?;
        required_string(record, "provider", "catalog evidence")
            .and_then(|provider| require_exact(provider, ZEN_PROVIDER_ID, "provider"))?;
        required_string(record, "model", "catalog evidence")
            .and_then(|model| require_exact(model, ZEN_FREE_MODEL_ID, "model"))?;
        required_string(record, "endpoint", "catalog evidence")
            .and_then(|endpoint| require_exact(endpoint, ZEN_RESPONSES_ENDPOINT, "endpoint"))?;
        let pricing = object(
            record
                .get("pricing_per_million")
                .ok_or_else(|| LiveVerificationError::new("catalog evidence omits pricing_per_million"))?,
            "pricing_per_million",
        )?;
        for field in ["input", "output", "cached_read"] {
            required_string(pricing, field, "pricing_per_million")
                .and_then(|charge| require_exact(charge, "Free", field))?;
        }
        if !pricing.get("cached_write").is_some_and(JsonValue::is_null) {
            return Err(LiveVerificationError::new(
                "catalog evidence must record cached_write as unavailable for the selected model",
            ));
        }
        let checked_on = required_string(record, "checked_on", "catalog evidence")?;
        if !is_iso_date(checked_on) {
            return Err(LiveVerificationError::new(
                "catalog evidence checked_on must use YYYY-MM-DD",
            ));
        }
        let data_use_source = required_string(record, "data_use_source", "catalog evidence")?;
        let data_use_summary = required_string(record, "data_use_summary", "catalog evidence")?;
        if data_use_source.trim().is_empty() || data_use_summary.trim().is_empty() {
            return Err(LiveVerificationError::new(
                "catalog evidence must record the selected route's data-use terms",
            ));
        }
        if record
            .get("synthetic_or_public_fixture_only")
            .and_then(JsonValue::as_bool)
            != Some(true)
        {
            return Err(LiveVerificationError::new(
                "catalog evidence must acknowledge synthetic-or-public-fixture-only live input",
            ));
        }
        Ok(Self {
            checked_on: checked_on.to_owned(),
            data_use_source: data_use_source.to_owned(),
            data_use_summary: data_use_summary.to_owned(),
        })
    }

    /// Return the date on which the official catalog was checked.
    pub fn checked_on(&self) -> &str {
        &self.checked_on
    }

    /// Return the official source for the selected route's data-use terms.
    pub fn data_use_source(&self) -> &str {
        &self.data_use_source
    }

    /// Return the sanitized data-use limitation recorded for the selected route.
    pub fn data_use_summary(&self) -> &str {
        &self.data_use_summary
    }

    fn require_current_utc_date(&self) -> Result<(), LiveVerificationError> {
        let current = current_utc_date()?;
        if self.checked_on != current {
            return Err(LiveVerificationError::new(format!(
                "live verification catalog evidence is stale (checked_on={}, current_utc_date={current}); refresh the official record before transport",
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
    /// The output-token allowance reserved before transport.
    pub requested_output_tokens: u64,
}

/// A content-free aggregate view of the guarded live budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveVerificationBudgetSnapshot {
    /// Attempts reserved by this aggregate live suite.
    pub attempted_requests: u64,
    /// Requested output tokens reserved by this aggregate live suite.
    pub requested_output_tokens: u64,
    /// Currently live provider streams.
    pub active_requests: u64,
    /// Bounded per-attempt records, in reservation order.
    pub attempts: Vec<LiveAttemptReservation>,
}

/// A model/provider pair admitted only by [`RestrictedZenFactory`].
#[derive(Clone)]
pub struct RestrictedZenConsumer {
    model: ModelDescriptor,
    provider: Arc<dyn ModelProvider>,
    role: VerificationConsumer,
}

impl RestrictedZenConsumer {
    /// Borrow the exact provider-neutral model descriptor installed in this consumer.
    pub fn model(&self) -> &ModelDescriptor {
        &self.model
    }

    /// Clone the guarded provider handle.
    ///
    /// This is a wrapper, not the concrete Zen adapter: every stream still validates the exact
    /// descriptor and reserves its aggregate live budget before reaching transport.
    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    /// Return the fixed live-suite role attached to this guarded consumer.
    pub fn role(&self) -> VerificationConsumer {
        self.role
    }
}

/// A single-model, single-endpoint provider factory for optional live verification.
///
/// The raw configured provider is retained privately. Root, child, compaction, candidate, and
/// comparison consumers receive only independently labelled wrappers that share this factory's
/// descriptor, endpoint validation, and persisted budget ledger.
pub struct RestrictedZenFactory {
    model: ModelDescriptor,
    provider: Arc<dyn ModelProvider>,
    budget: Arc<Mutex<LiveBudget>>,
    evidence: FreeZenCatalogEvidence,
    ledger_path: PathBuf,
    output_tokens_per_request: NonZeroU64,
}

impl RestrictedZenFactory {
    /// Construct the restricted factory from an explicitly supplied Zen key and a caller-owned
    /// ledger outside the source tree.
    ///
    /// The factory does not read environment variables or credential stores. A terminal/example
    /// boundary may load an explicitly supplied key using its existing secret mechanism, then
    /// pass the owned value here. No request is sent during construction.
    pub fn new(
        api_key: String,
        evidence: FreeZenCatalogEvidence,
        ledger_path: PathBuf,
        output_tokens_per_request: NonZeroU64,
    ) -> Result<Self, LiveVerificationError> {
        evidence.require_current_utc_date()?;
        if output_tokens_per_request.get() > MAX_REQUESTED_OUTPUT_TOKENS {
            return Err(LiveVerificationError::new(format!(
                "per-request output allowance exceeds the {MAX_REQUESTED_OUTPUT_TOKENS}-token live budget"
            )));
        }
        let config = OpencodeZenConfig::try_new(api_key, ZEN_FREE_MODEL_ID)
            .map_err(|error| LiveVerificationError::new(error.to_string()))?
            .with_max_tokens(output_tokens_per_request.get())
            .with_retry_policy(RetryPolicy::new(0, Duration::ZERO, Duration::ZERO));
        if config.responses_url() != ZEN_RESPONSES_ENDPOINT {
            return Err(LiveVerificationError::new(
                "live verification refused a non-canonical OpenCode Zen endpoint before transport",
            ));
        }
        // The normal provider catalog intentionally remains a product-selection
        // surface and may lag this independently reviewed verification candidate.
        // This feature-only factory therefore constructs the exact descriptor
        // itself rather than falling back to a catalog neighbor.
        let model = ModelDescriptor {
            provider: ZEN_PROVIDER_ID.into(),
            model: ZEN_FREE_MODEL_ID.into(),
            revision: None,
        };
        if !is_exact_zen_descriptor(&model) {
            return Err(LiveVerificationError::new(
                "live verification could not construct the exact selected Zen descriptor",
            ));
        }
        let configured = ConfiguredProvider {
            descriptor: model.clone(),
            provider: Arc::new(OpencodeZenProvider::new(config)),
        };
        if configured.descriptor != model {
            return Err(LiveVerificationError::new(
                "live verification configured a descriptor different from its exact route",
            ));
        }
        let budget = LiveBudget::open(ledger_path.clone(), &evidence)?;
        Ok(Self {
            model,
            provider: configured.provider,
            budget: Arc::new(Mutex::new(budget)),
            evidence,
            ledger_path,
            output_tokens_per_request,
        })
    }

    /// Create one guarded provider/model consumer for a required live-suite role.
    pub fn consumer(&self, consumer: VerificationConsumer) -> RestrictedZenConsumer {
        RestrictedZenConsumer {
            model: self.model.clone(),
            provider: Arc::new(BudgetedZenProvider {
                inner: Arc::clone(&self.provider),
                expected_model: self.model.clone(),
                consumer,
                output_tokens_per_request: self.output_tokens_per_request,
                budget: Arc::clone(&self.budget),
            }),
            role: consumer,
        }
    }

    /// Return content-free aggregate request-budget evidence.
    pub fn budget_snapshot(&self) -> Result<LiveVerificationBudgetSnapshot, LiveVerificationError> {
        self.budget
            .lock()
            .map_err(|_| LiveVerificationError::new("live-verification budget lock is poisoned"))?
            .snapshot()
    }

    /// Reload persisted aggregate reservations after a serialized verification child process.
    ///
    /// A separate one-shot executable has its own guarded factory instance, so it persists its
    /// reservation in the same ledger before transport. The parent must reload only while it has
    /// no active stream; that prevents a stale in-memory snapshot from understating the suite
    /// budget after the child exits.
    pub fn reload_budget_snapshot(
        &self,
    ) -> Result<LiveVerificationBudgetSnapshot, LiveVerificationError> {
        let refreshed = LiveBudget::open(self.ledger_path.clone(), &self.evidence)?;
        let mut budget = self
            .budget
            .lock()
            .map_err(|_| LiveVerificationError::new("live-verification budget lock is poisoned"))?;
        if budget.active_requests != 0 {
            return Err(LiveVerificationError::new(
                "live verification cannot reload its persisted budget while a provider stream is active",
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
/// workspace, terminal configuration, or credential source.
pub struct HeadlessLiveCase<'a> {
    /// Existing caller-owned Tea home for this one case.
    pub tea_home: &'a Path,
    /// Existing caller-owned disposable workspace for this one case.
    pub workspace: &'a Path,
    /// Fixed role expected to own the model request.
    pub role: VerificationConsumer,
    /// Restricted consumer constructed by the one suite factory.
    pub consumer: &'a RestrictedZenConsumer,
    /// Restricted compaction consumer, required only by the compaction case.
    pub compactor: Option<&'a RestrictedZenConsumer>,
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
/// path when called, so callers must have completed catalog/terms review and must use disposable
/// input. It retains no model output in the returned evidence.
pub fn run_headless_live_case(
    case: HeadlessLiveCase<'_>,
) -> Result<HeadlessLiveCaseOutcome, LiveVerificationError> {
    if case.role != case.consumer.role() {
        return Err(LiveVerificationError::new(
            "live verification headless case received a consumer for a different role",
        ));
    }
    if !is_exact_zen_descriptor(case.consumer.model()) {
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
                || !is_exact_zen_descriptor(compactor.model())
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
    pub consumer: &'a RestrictedZenConsumer,
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
        || !is_exact_zen_descriptor(case.consumer.model())
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
    let first_operation = smol::block_on(async {
        for _ in 0..4_096 {
            if has_durable_provider_request(&harness)? {
                if !harness
                    .abort_root()
                    .map_err(|error| LiveVerificationError::new(error.to_string()))?
                {
                    return Err(LiveVerificationError::new(
                        "live recovery lost the active root operation before controlled cancellation",
                    ));
                }
                return drive
                    .await
                    .map_err(|error| LiveVerificationError::new(error.to_string()));
            }
            smol::future::yield_now().await;
        }
        Err(LiveVerificationError::new(
            "live recovery did not observe durable provider admission before cancellation",
        ))
    })?;
    if first_operation.is_completed() {
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

struct BudgetedZenProvider {
    inner: Arc<dyn ModelProvider>,
    expected_model: ModelDescriptor,
    consumer: VerificationConsumer,
    output_tokens_per_request: NonZeroU64,
    budget: Arc<Mutex<LiveBudget>>,
}

enum StreamAdmission {
    Ready(Result<Box<dyn ModelEventStream>, tea_core::error::SchedulerError>),
    TimedOut,
}

impl ModelProvider for BudgetedZenProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        if request.model.as_ref() != Some(&self.expected_model) {
            return Box::pin(std::future::ready(Ok(Box::new(RejectedEventStream::new(
                "live verification refused a paid, unknown, or mismatched provider route before transport",
            )) as Box<dyn ModelEventStream>)));
        }
        match self
            .budget
            .lock()
            .map_err(|_| LiveVerificationError::new("live-verification budget lock is poisoned"))
            .and_then(|mut budget| budget.reserve(self.consumer, self.output_tokens_per_request))
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
        let budget = Arc::clone(&self.budget);
        Box::pin(async move {
            let remaining = match budget
                .lock()
                .map_err(|_| LiveVerificationError::new("live-verification budget lock is poisoned"))
                .and_then(|budget| budget.wall_time_remaining())
            {
                Ok(Some(remaining)) => remaining,
                Ok(None) | Err(_) => {
                    cancellation.cancel();
                    return Ok(Box::new(BudgetedEventStream {
                        inner: Box::new(RejectedEventStream::new(
                            "live verification wall-time budget expired before provider transport",
                        )),
                        permit: Some(permit),
                    }) as Box<dyn ModelEventStream>);
                }
            };
            let timeout_cancellation = cancellation.clone();
            let admission = smol::future::or(
                async move { StreamAdmission::Ready(inner.stream(request, cancellation).await) },
                async move {
                    smol::Timer::after(remaining).await;
                    timeout_cancellation.cancel();
                    StreamAdmission::TimedOut
                },
            )
            .await;
            match admission {
                StreamAdmission::Ready(Ok(stream)) => Ok(Box::new(BudgetedEventStream {
                    inner: stream,
                    permit: Some(permit),
                }) as Box<dyn ModelEventStream>),
                StreamAdmission::Ready(Err(error)) => Err(error),
                StreamAdmission::TimedOut => Ok(Box::new(BudgetedEventStream {
                    inner: Box::new(RejectedEventStream::new(
                        "live verification wall-time budget expired during provider transport",
                    )),
                    permit: Some(permit),
                }) as Box<dyn ModelEventStream>),
            }
        })
    }
}

struct BudgetedEventStream {
    inner: Box<dyn ModelEventStream>,
    permit: Option<LivePermit>,
}

impl BudgetedEventStream {
    fn release(&mut self) {
        self.permit.take();
    }
}

impl ModelEventStream for BudgetedEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(async move {
            let Some(remaining) = self.wall_time_remaining() else {
                cancellation.cancel();
                self.release();
                return Ok(Some(ModelStreamEvent::Error {
                    message: "live verification wall-time budget expired during provider stream".into(),
                }));
            };
            let timeout_cancellation = cancellation.clone();
            let event = smol::future::or(
                self.inner.next_event(cancellation.clone()),
                async move {
                    smol::Timer::after(remaining).await;
                    timeout_cancellation.cancel();
                    Ok(Some(ModelStreamEvent::Error {
                        message: "live verification wall-time budget expired during provider stream".into(),
                    }))
                },
            )
            .await;
            if event.is_err()
                || matches!(
                    &event,
                    Ok(None)
                        | Ok(Some(ModelStreamEvent::End(_)))
                        | Ok(Some(ModelStreamEvent::Error { .. }))
                        | Ok(Some(ModelStreamEvent::Aborted { .. }))
                )
            {
                self.release();
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

struct LiveBudget {
    ledger_path: PathBuf,
    checked_on: String,
    started_at_unix_seconds: u64,
    attempted_requests: u64,
    requested_output_tokens: u64,
    active_requests: u64,
    attempts: Vec<LiveAttemptReservation>,
}

impl LiveBudget {
    fn open(path: PathBuf, evidence: &FreeZenCatalogEvidence) -> Result<Self, LiveVerificationError> {
        let now = unix_seconds()?;
        match fs::read_to_string(&path) {
            Ok(source) => Self::from_ledger_json(&path, evidence, &source, now),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let budget = Self {
                    ledger_path: path,
                    checked_on: evidence.checked_on.clone(),
                    started_at_unix_seconds: now,
                    attempted_requests: 0,
                    requested_output_tokens: 0,
                    active_requests: 0,
                    attempts: Vec::new(),
                };
                budget.persist()?;
                Ok(budget)
            }
            Err(error) => Err(LiveVerificationError::new(format!(
                "cannot read live-verification budget ledger {}: {error}",
                path.display()
            ))),
        }
    }

    fn from_ledger_json(
        path: &Path,
        evidence: &FreeZenCatalogEvidence,
        source: &str,
        now: u64,
    ) -> Result<Self, LiveVerificationError> {
        let value = JsonValue::parse(source)
            .map_err(|error| LiveVerificationError::new(format!("invalid live-verification ledger: {error}")))?;
        let record = object(&value, "live-verification ledger")?;
        required_string(record, "schema_version", "live-verification ledger")
            .and_then(|schema| require_exact(schema, LEDGER_SCHEMA, "schema_version"))?;
        required_string(record, "provider", "live-verification ledger")
            .and_then(|provider| require_exact(provider, ZEN_PROVIDER_ID, "provider"))?;
        required_string(record, "model", "live-verification ledger")
            .and_then(|model| require_exact(model, ZEN_FREE_MODEL_ID, "model"))?;
        required_string(record, "endpoint", "live-verification ledger")
            .and_then(|endpoint| require_exact(endpoint, ZEN_RESPONSES_ENDPOINT, "endpoint"))?;
        required_string(record, "checked_on", "live-verification ledger")
            .and_then(|checked_on| require_exact(checked_on, evidence.checked_on(), "checked_on"))?;
        let started_at_unix_seconds = required_u64(record, "started_at_unix_seconds", "live-verification ledger")?;
        if now.saturating_sub(started_at_unix_seconds) >= MAX_WALL_TIME.as_secs() {
            return Err(LiveVerificationError::new(
                "live verification wall-time budget is exhausted; start a new task with new evidence rather than resetting this ledger",
            ));
        }
        let attempted_requests = required_u64(record, "attempted_requests", "live-verification ledger")?;
        let requested_output_tokens = required_u64(record, "requested_output_tokens", "live-verification ledger")?;
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
        if attempted_requests > MAX_ATTEMPTS || requested_output_tokens > MAX_REQUESTED_OUTPUT_TOKENS {
            return Err(LiveVerificationError::new(
                "live-verification ledger already exceeds the aggregate task budget",
            ));
        }
        Ok(Self {
            ledger_path: path.to_owned(),
            checked_on: evidence.checked_on.clone(),
            started_at_unix_seconds,
            attempted_requests,
            requested_output_tokens,
            active_requests: 0,
            attempts,
        })
    }

    fn reserve(
        &mut self,
        consumer: VerificationConsumer,
        output_tokens: NonZeroU64,
    ) -> Result<(), LiveVerificationError> {
        if unix_seconds()?.saturating_sub(self.started_at_unix_seconds) >= MAX_WALL_TIME.as_secs() {
            return Err(LiveVerificationError::new(
                "live verification wall-time budget is exhausted before provider transport",
            ));
        }
        let next_attempt = self.attempted_requests.saturating_add(1);
        let next_tokens = self
            .requested_output_tokens
            .checked_add(output_tokens.get())
            .ok_or_else(|| LiveVerificationError::new("live-verification output budget overflow"))?;
        if next_attempt > MAX_ATTEMPTS {
            return Err(LiveVerificationError::new(
                "live verification attempt budget is exhausted before provider transport",
            ));
        }
        if next_tokens > MAX_REQUESTED_OUTPUT_TOKENS {
            return Err(LiveVerificationError::new(
                "live verification requested-output budget is exhausted before provider transport",
            ));
        }
        if self.active_requests >= MAX_CONCURRENT_REQUESTS {
            return Err(LiveVerificationError::new(
                "live verification concurrent-request budget is exhausted before provider transport",
            ));
        }
        self.attempted_requests = next_attempt;
        self.requested_output_tokens = next_tokens;
        self.active_requests = self.active_requests.saturating_add(1);
        self.attempts.push(LiveAttemptReservation {
            sequence: next_attempt,
            consumer,
            requested_output_tokens: output_tokens.get(),
        });
        if let Err(error) = self.persist() {
            self.attempted_requests = self.attempted_requests.saturating_sub(1);
            self.requested_output_tokens = self
                .requested_output_tokens
                .saturating_sub(output_tokens.get());
            self.active_requests = self.active_requests.saturating_sub(1);
            self.attempts.pop();
            return Err(error);
        }
        Ok(())
    }

    fn wall_time_remaining(&self) -> Result<Option<Duration>, LiveVerificationError> {
        let elapsed = unix_seconds()?.saturating_sub(self.started_at_unix_seconds);
        Ok((elapsed < MAX_WALL_TIME.as_secs())
            .then(|| Duration::from_secs(MAX_WALL_TIME.as_secs() - elapsed)))
    }

    fn snapshot(&self) -> Result<LiveVerificationBudgetSnapshot, LiveVerificationError> {
        Ok(LiveVerificationBudgetSnapshot {
            attempted_requests: self.attempted_requests,
            requested_output_tokens: self.requested_output_tokens,
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
                "cannot persist live-verification budget before transport: {error}"
            )));
        }
        fs::rename(&temporary, &self.ledger_path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            LiveVerificationError::new(format!(
                "cannot publish live-verification budget before transport: {error}"
            ))
        })
    }

    fn ledger_json(&self) -> JsonValue {
        JsonValue::object([
            ("schema_version", JsonValue::from(LEDGER_SCHEMA)),
            ("provider", JsonValue::from(ZEN_PROVIDER_ID)),
            ("model", JsonValue::from(ZEN_FREE_MODEL_ID)),
            ("endpoint", JsonValue::from(ZEN_RESPONSES_ENDPOINT)),
            ("checked_on", JsonValue::from(self.checked_on.clone())),
            (
                "started_at_unix_seconds",
                JsonValue::from(self.started_at_unix_seconds),
            ),
            ("attempted_requests", JsonValue::from(self.attempted_requests)),
            (
                "requested_output_tokens",
                JsonValue::from(self.requested_output_tokens),
            ),
            (
                "attempts",
                JsonValue::Array(
                    self.attempts
                        .iter()
                        .map(|attempt| {
                            JsonValue::object([
                                ("sequence", JsonValue::from(attempt.sequence)),
                                ("consumer", JsonValue::from(attempt.consumer.as_str())),
                                (
                                    "requested_output_tokens",
                                    JsonValue::from(attempt.requested_output_tokens),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

struct LivePermit {
    budget: Option<Arc<Mutex<LiveBudget>>>,
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

impl BudgetedEventStream {
    fn wall_time_remaining(&self) -> Option<Duration> {
        self.permit.as_ref().and_then(|permit| {
            permit
                .budget
                .as_ref()
                .and_then(|budget| budget.lock().ok())
                .and_then(|budget| budget.wall_time_remaining().ok().flatten())
        })
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
        requested_output_tokens: required_u64(
            record,
            "requested_output_tokens",
            "live-verification attempt",
        )?,
    })
}

fn is_exact_zen_descriptor(model: &ModelDescriptor) -> bool {
    model.provider == ZEN_PROVIDER_ID && model.model == ZEN_FREE_MODEL_ID && model.revision.is_none()
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

    fn evidence() -> FreeZenCatalogEvidence {
        let checked_on = current_utc_date().expect("system UTC date is available");
        FreeZenCatalogEvidence::from_json(&JsonValue::object([
            ("schema_version", JsonValue::from(EVIDENCE_SCHEMA)),
            ("catalog_source", JsonValue::from(ZEN_CATALOG_SOURCE)),
            ("provider", JsonValue::from(ZEN_PROVIDER_ID)),
            ("model", JsonValue::from(ZEN_FREE_MODEL_ID)),
            ("endpoint", JsonValue::from(ZEN_RESPONSES_ENDPOINT)),
            (
                "pricing_per_million",
                JsonValue::object([
                    ("input", JsonValue::from("Free")),
                    ("output", JsonValue::from("Free")),
                    ("cached_read", JsonValue::from("Free")),
                    ("cached_write", JsonValue::Null),
                ]),
            ),
            ("checked_on", JsonValue::from(checked_on)),
            ("data_use_source", JsonValue::from(ZEN_CATALOG_SOURCE)),
            (
                "data_use_summary",
                JsonValue::from("Synthetic public fixture only; prompts and completions may train future Meta models."),
            ),
            ("synthetic_or_public_fixture_only", JsonValue::Bool(true)),
        ]))
        .expect("fixture evidence is exact")
    }

    fn temporary_ledger(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tea-live-verification-{name}-{}-{}.json",
            std::process::id(),
            unix_seconds().expect("clock is available")
        ))
    }

    fn guarded_provider(inner: Arc<CountingProvider>) -> Arc<dyn ModelProvider> {
        let ledger = temporary_ledger("guard");
        let budget = LiveBudget::open(ledger.clone(), &evidence()).expect("ledger opens");
        let provider: Arc<dyn ModelProvider> = Arc::new(BudgetedZenProvider {
            inner,
            expected_model: ModelDescriptor {
                provider: ZEN_PROVIDER_ID.into(),
                model: ZEN_FREE_MODEL_ID.into(),
                revision: None,
            },
            consumer: VerificationConsumer::Root,
            output_tokens_per_request: NonZeroU64::new(1).expect("nonzero"),
            budget: Arc::new(Mutex::new(budget)),
        });
        let _ = fs::remove_file(ledger);
        provider
    }

    fn request(provider: &str, model: &str) -> ModelRequest {
        ModelRequest {
            model: Some(ModelDescriptor {
                provider: provider.into(),
                model: model.into(),
                revision: None,
            }),
            ..ModelRequest::default()
        }
    }

    #[test]
    fn catalog_evidence_rejects_paid_or_incomplete_charges_before_provider_construction() {
        let mut record = JsonValue::object([
            ("schema_version", JsonValue::from(EVIDENCE_SCHEMA)),
            ("catalog_source", JsonValue::from(ZEN_CATALOG_SOURCE)),
            ("provider", JsonValue::from(ZEN_PROVIDER_ID)),
            ("model", JsonValue::from(ZEN_FREE_MODEL_ID)),
            ("endpoint", JsonValue::from(ZEN_RESPONSES_ENDPOINT)),
            (
                "pricing_per_million",
                JsonValue::object([
                    ("input", JsonValue::from("$0.01")),
                    ("output", JsonValue::from("Free")),
                    ("cached_read", JsonValue::from("Free")),
                    ("cached_write", JsonValue::Null),
                ]),
            ),
            ("checked_on", JsonValue::from("2026-09-21")),
            ("data_use_source", JsonValue::from(ZEN_CATALOG_SOURCE)),
            ("data_use_summary", JsonValue::from("synthetic only")),
            ("synthetic_or_public_fixture_only", JsonValue::Bool(true)),
        ]);
        assert!(FreeZenCatalogEvidence::from_json(&record).is_err());
        record
            .as_object_mut()
            .expect("object")
            .insert("endpoint".into(), JsonValue::from("https://other.invalid/responses"));
        assert!(FreeZenCatalogEvidence::from_json(&record).is_err());
    }

    #[test]
    fn paid_unknown_and_mismatched_requests_do_not_reach_transport() {
        let inner = Arc::new(CountingProvider::default());
        let provider = guarded_provider(Arc::clone(&inner));
        for (provider_id, model) in [
            (ZEN_PROVIDER_ID, "muse-spark-1.3"),
            (ZEN_PROVIDER_ID, "unknown-free-looking-model"),
            ("openrouter", ZEN_FREE_MODEL_ID),
        ] {
            let cancellation = CancellationToken::new();
            let mut stream = smol::block_on(provider.stream(request(provider_id, model), cancellation.clone()))
                .expect("guard always yields a local rejection stream");
            assert!(matches!(
                smol::block_on(stream.next_event(cancellation)),
                Ok(Some(ModelStreamEvent::Error { .. }))
            ));
        }
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            0,
            "rejected routes must not initiate provider transport"
        );
    }

    #[test]
    fn all_consumer_roles_share_one_exact_descriptor_and_budget() {
        let ledger = temporary_ledger("factory");
        let factory = RestrictedZenFactory::new(
            "test-key".into(),
            evidence(),
            ledger.clone(),
            NonZeroU64::new(1024).expect("nonzero"),
        )
        .expect("factory configures without transport");
        for consumer in [
            VerificationConsumer::Root,
            VerificationConsumer::Child,
            VerificationConsumer::Compaction,
            VerificationConsumer::CandidateEvaluation,
            VerificationConsumer::Comparison,
        ] {
            let handle = factory.consumer(consumer);
            assert_eq!(handle.model().provider, ZEN_PROVIDER_ID);
            assert_eq!(handle.model().model, ZEN_FREE_MODEL_ID);
            assert!(handle.model().revision.is_none());
        }
        assert_eq!(factory.budget_snapshot().expect("snapshot").attempted_requests, 0);
        let _ = fs::remove_file(ledger);
    }

    #[test]
    fn stale_catalog_evidence_blocks_factory_before_transport() {
        let record = JsonValue::object([
            ("schema_version", JsonValue::from(EVIDENCE_SCHEMA)),
            ("catalog_source", JsonValue::from(ZEN_CATALOG_SOURCE)),
            ("provider", JsonValue::from(ZEN_PROVIDER_ID)),
            ("model", JsonValue::from(ZEN_FREE_MODEL_ID)),
            ("endpoint", JsonValue::from(ZEN_RESPONSES_ENDPOINT)),
            (
                "pricing_per_million",
                JsonValue::object([
                    ("input", JsonValue::from("Free")),
                    ("output", JsonValue::from("Free")),
                    ("cached_read", JsonValue::from("Free")),
                    ("cached_write", JsonValue::Null),
                ]),
            ),
            ("checked_on", JsonValue::from("2000-01-01")),
            ("data_use_source", JsonValue::from(ZEN_CATALOG_SOURCE)),
            ("data_use_summary", JsonValue::from("synthetic only")),
            ("synthetic_or_public_fixture_only", JsonValue::Bool(true)),
        ]);
        let evidence = FreeZenCatalogEvidence::from_json(&record).expect("stale evidence is syntactically valid");
        let ledger = temporary_ledger("stale-evidence");
        assert!(RestrictedZenFactory::new(
            "test-key".into(),
            evidence,
            ledger.clone(),
            NonZeroU64::new(1).expect("nonzero"),
        )
        .is_err());
        assert!(!ledger.exists(), "stale evidence must fail before creating a budget ledger");
    }

    #[test]
    fn invalid_headless_paths_are_rejected_before_provider_transport() {
        let inner = Arc::new(CountingProvider::default());
        let consumer = RestrictedZenConsumer {
            model: ModelDescriptor {
                provider: ZEN_PROVIDER_ID.into(),
                model: ZEN_FREE_MODEL_ID.into(),
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
