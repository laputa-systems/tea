//! Stable host-owned control surface for session-local harness lineage.
//!
//! This module intentionally translates a model-facing JSON command into the
//! narrower manager API.  It does not expose repository locks, mutable source
//! paths, or a generic command channel to the model.  In particular, a
//! successful mutation records only a durable activation obligation; the
//! supervisor remains the sole owner of revision activation at an epoch
//! boundary.

use super::events::EventHub;
use crate::harness::resolver::HarnessSourceDiff;
use crate::harness::{
    AUTHORING_AUTHORIZATION_METADATA_KEY, CandidateHypothesis, HarnessActor, HarnessApplyRequest,
    HarnessCandidateV1, HarnessError, HarnessFilePatch, HarnessResolver, HarnessRevisionReason,
    HarnessSurface, RegistryOperation, SelfExtensionMode,
};
use crate::runtime::{DiagnosticCode, HarnessEvent, HarnessIdentity, TeaEvent, ValidationStage};
use crate::runtime::RuntimeServices;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tea_core::error::ToolError;
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolExecutionMode, ToolFailure, ToolFuture,
    ToolRegistry, ToolUpdateSink,
};
use tea_protocol::{JsonNumber, JsonValue};
use tea_session::{
    ArtifactId, EntryId, EpochFinishReason, EpochFinishedRecord, HarnessRevisionId, LaneId,
    LaneRecord, NormalizedPath, OperationId, SessionWriter, reduce_lane,
};

pub(crate) const STABLE_HARNESS_TOOL_NAME: &str = "tea_harness";

const MAXIMUM_LIST_ITEMS: usize = 100;
const MAXIMUM_SOURCE_PAGE_BYTES: usize = 8 * 1024;

/// Add the fixed harness-control capability to a host-owned registry.
pub(crate) fn stable_harness_tools<S>(
    session: Arc<Mutex<S>>,
    artifacts: Arc<dyn tea_session::ArtifactStore>,
    manager: Arc<HarnessResolver>,
    lane_id: LaneId,
    identity: HarnessIdentity,
    operation_id: OperationId,
    rollover_budget: u32,
    runtime_services: RuntimeServices,
    events: Arc<EventHub>,
) -> ToolRegistry
where
    S: SessionWriter + Send + 'static,
{
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(TeaHarnessTool {
        session,
        artifacts,
        manager,
        lane_id,
        identity,
        operation_id,
        rollover_budget,
        runtime_services,
        events,
    }));
    tools
}

struct TeaHarnessTool<S> {
    session: Arc<Mutex<S>>,
    artifacts: Arc<dyn tea_session::ArtifactStore>,
    manager: Arc<HarnessResolver>,
    lane_id: LaneId,
    identity: HarnessIdentity,
    operation_id: OperationId,
    rollover_budget: u32,
    runtime_services: RuntimeServices,
    events: Arc<EventHub>,
}

impl<S> AgentTool for TeaHarnessTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn name(&self) -> &str {
        STABLE_HARNESS_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Inspect or atomically stage a session-local Tea harness candidate. Mutating apply and rollback calls must be issued alone; accepted changes activate automatically at the next epoch boundary."
    }

    fn schema(&self) -> &JsonValue {
        harness_schema()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        // This retains source-order behavior for a valid single-call batch;
        // `requires_exclusive_batch` rejects a mixed batch before any call
        // starts, including a sibling that appeared earlier in source order.
        ToolExecutionMode::Sequential
    }

    fn requires_exclusive_batch(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let result = self.dispatch(call);
        Box::pin(std::future::ready(result))
    }
}

impl<S> TeaHarnessTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn dispatch(&self, call: ToolCall) -> Result<AgentToolResult, ToolError> {
        let object = match parse_object(&call) {
            Ok(object) => object,
            Err(message) => return Ok(error_result(call, "invalid_arguments", message)),
        };
        let operation = match required_string(&object, "operation") {
            Ok(operation) => operation,
            Err(message) => return Ok(error_result(call, "invalid_arguments", message)),
        };
        let response = match operation.as_str() {
            "status" => self.status(&object),
            "help" => self.help(&object),
            "list" => self.list(&object),
            "read" => self.read(&object),
            "diff" => self.diff(&object),
            "apply" => self.apply(&call, &object),
            "rollback" => self.rollback(&call, &object),
            _ => Err(format!(
                "operation {operation:?} is unknown; use tea_harness.help for the stable command set",
            )),
        };
        match response {
            Ok(response) => Ok(control_result(
                call,
                response.value,
                response.terminate,
                response.is_error,
            )),
            Err(message) => {
                if matches!(operation.as_str(), "apply" | "rollback") {
                    let (stage, code) = rejection_category(&message);
                    self.publish_candidate_rejected(
                        None,
                        stage,
                        code,
                        "candidate staging was rejected",
                    );
                }
                Ok(error_result(call, "rejected", message))
            }
        }
    }

    fn status(&self, object: &BTreeMap<String, JsonValue>) -> Result<ControlResponse, String> {
        require_exact_fields(object, &["operation"])?;
        let snapshot = self.session_snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), self.lane_id.clone())
            .map_err(|error| error.to_string())?;
        let active_revision = reduction
            .lane_state
            .active_harness_revision
            .ok_or_else(|| "managed harness branch has no active revision".to_owned())?;
        let active = self
            .manager
            .revision(&active_revision)
            .map_err(harness_error)?;
        let used = rollover_count(&snapshot, &self.operation_id);
        let pending = reduction
            .pending_harness_activation
            .map(|pending| {
                JsonValue::object([
                    (
                        "candidate_id",
                        JsonValue::String(pending.request.candidate_id.to_string()),
                    ),
                    (
                        "proposed_snapshot_id",
                        JsonValue::String(pending.request.proposed_snapshot_id.to_string()),
                    ),
                ])
            })
            .unwrap_or(JsonValue::Null);
        Ok(ControlResponse::plain(JsonValue::object([
            ("operation", JsonValue::String("status".into())),
            (
                "active_revision",
                JsonValue::String(active_revision.to_string()),
            ),
            (
                "active_snapshot",
                JsonValue::String(active.snapshot_id.to_string()),
            ),
            (
                "epoch_revision",
                JsonValue::String(self.identity.revision_id().to_string()),
            ),
            ("pending_activation", pending),
            (
                "rollovers",
                JsonValue::object([
                    ("used", unsigned(u64::from(used))),
                    ("maximum", unsigned(u64::from(self.rollover_budget))),
                    (
                        "remaining",
                        unsigned(u64::from(self.rollover_budget.saturating_sub(used))),
                    ),
                ]),
            ),
            (
                "capability_ceiling",
                JsonValue::Array(
                    self.manager
                        .capability_ceiling()
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
        ])))
    }

    fn help(&self, object: &BTreeMap<String, JsonValue>) -> Result<ControlResponse, String> {
        require_exact_fields(object, &["operation"])?;
        Ok(ControlResponse::plain(JsonValue::object([
            ("operation", JsonValue::String("help".into())),
            (
                "commands",
                JsonValue::Array(vec![
                    command("status", "Show the active immutable revision and rollover budget."),
                    command("help", "Describe this fixed control surface."),
                    command("list", "List bounded immutable revision and candidate metadata."),
                    command("read", "Read one bounded immutable source page."),
                    command("diff", "Compare immutable source identities between revisions."),
                    command("apply", "Stage one validated atomic source and registry patch, then schedule activation."),
                    command("rollback", "Stage an ancestor snapshot as a normal immutable rollback candidate."),
                ]),
            ),
            (
                "mutation_rule",
                JsonValue::String(
                    "apply and rollback must be the only tool call in their assistant batch; Tea continues automatically after a scheduled activation.".into(),
                ),
            ),
        ])))
    }

    fn list(&self, object: &BTreeMap<String, JsonValue>) -> Result<ControlResponse, String> {
        require_exact_fields(object, &["operation", "kind", "maximum"])?;
        let kind = optional_string(object, "kind")?.unwrap_or_else(|| "all".into());
        if !matches!(kind.as_str(), "all" | "revisions" | "candidates") {
            return Err("list kind must be one of all, revisions, or candidates".into());
        }
        let maximum = optional_usize(object, "maximum", 20, MAXIMUM_LIST_ITEMS)?;
        let revisions = if matches!(kind.as_str(), "all" | "revisions") {
            self.manager
                .revisions()
                .map_err(harness_error)?
                .into_iter()
                .take(maximum)
                .map(|revision| {
                    JsonValue::object([
                        (
                            "revision_id",
                            JsonValue::String(revision.revision_id.to_string()),
                        ),
                        (
                            "snapshot_id",
                            JsonValue::String(revision.snapshot_id.to_string()),
                        ),
                        (
                            "reason",
                            JsonValue::String(revision_reason(&revision.reason).into()),
                        ),
                        (
                            "actor",
                            JsonValue::String(actor_name(revision.actor).into()),
                        ),
                        (
                            "parent_revision_ids",
                            JsonValue::Array(
                                revision
                                    .parent_revision_ids
                                    .into_iter()
                                    .map(|id| JsonValue::String(id.to_string()))
                                    .collect(),
                            ),
                        ),
                    ])
                })
                .collect()
        } else {
            Vec::new()
        };
        let candidates = if matches!(kind.as_str(), "all" | "candidates") {
            self.manager
                .candidates()
                .map_err(harness_error)?
                .into_iter()
                .take(maximum)
                .map(candidate_value)
                .collect()
        } else {
            Vec::new()
        };
        Ok(ControlResponse::plain(JsonValue::object([
            ("operation", JsonValue::String("list".into())),
            ("kind", JsonValue::String(kind)),
            ("revisions", JsonValue::Array(revisions)),
            ("candidates", JsonValue::Array(candidates)),
        ])))
    }

    fn read(&self, object: &BTreeMap<String, JsonValue>) -> Result<ControlResponse, String> {
        require_exact_fields(
            object,
            &["operation", "revision", "path", "offset", "maximum_bytes"],
        )?;
        let revision = optional_revision(object, "revision")?
            .unwrap_or_else(|| self.identity.revision_id().clone());
        let path = NormalizedPath::new(required_string(object, "path")?)
            .map_err(|error| format!("path must be a canonical portable source path: {error}"))?;
        let offset = optional_u64(object, "offset", 0)?;
        let maximum_bytes = optional_usize(
            object,
            "maximum_bytes",
            MAXIMUM_SOURCE_PAGE_BYTES,
            MAXIMUM_SOURCE_PAGE_BYTES,
        )?;
        let source = self
            .manager
            .read_source(&revision, &path)
            .map_err(harness_error)?;
        let offset = usize::try_from(offset)
            .map_err(|_| "read offset exceeds platform bounds".to_owned())?;
        if offset > source.bytes.len() {
            return Err("read offset is beyond the immutable source length".into());
        }
        let end = offset.saturating_add(maximum_bytes).min(source.bytes.len());
        Ok(ControlResponse::plain(JsonValue::object([
            ("operation", JsonValue::String("read".into())),
            ("revision", JsonValue::String(revision.to_string())),
            ("path", JsonValue::String(path.to_string())),
            (
                "artifact_id",
                JsonValue::String(source.artifact_id.to_hex()),
            ),
            ("offset", unsigned(offset as u64)),
            ("eof", JsonValue::Bool(end == source.bytes.len())),
            ("page", bytes_value(&source.bytes[offset..end])),
        ])))
    }

    fn diff(&self, object: &BTreeMap<String, JsonValue>) -> Result<ControlResponse, String> {
        require_exact_fields(
            object,
            &["operation", "base_revision", "target_revision", "maximum"],
        )?;
        let base = required_revision(object, "base_revision")?;
        let target = required_revision(object, "target_revision")?;
        let maximum = optional_usize(object, "maximum", 50, MAXIMUM_LIST_ITEMS)?;
        let changed = self
            .manager
            .diff_revisions(&base, &target)
            .map_err(harness_error)?;
        let total = changed.len();
        let changes = changed.into_iter().take(maximum).map(diff_value).collect();
        Ok(ControlResponse::plain(JsonValue::object([
            ("operation", JsonValue::String("diff".into())),
            ("base_revision", JsonValue::String(base.to_string())),
            ("target_revision", JsonValue::String(target.to_string())),
            ("changed_path_count", unsigned(total as u64)),
            ("changes", JsonValue::Array(changes)),
        ])))
    }

    fn apply(
        &self,
        call: &ToolCall,
        object: &BTreeMap<String, JsonValue>,
    ) -> Result<ControlResponse, String> {
        require_exact_fields(
            object,
            &[
                "operation",
                "base_revision",
                "hypothesis",
                "files",
                "registry_operations",
            ],
        )?;
        self.require_mutation_authorization()?;
        let base_revision_id = required_revision(object, "base_revision")?;
        self.require_current_base(&base_revision_id)?;
        let request = HarnessApplyRequest {
            base_revision_id,
            hypothesis: parse_hypothesis(object)?,
            files: parse_file_patches(object)?,
            registry_operations: parse_registry_operations(object)?,
            operation_id: Some(self.operation_id.clone()),
            tool_invocation_id: call.id.to_string(),
        };
        let candidate = self
            .manager
            .apply(request, &self.runtime_services)
            .map_err(harness_error)?;
        self.schedule_candidate(call, candidate, "apply")
    }

    fn rollback(
        &self,
        call: &ToolCall,
        object: &BTreeMap<String, JsonValue>,
    ) -> Result<ControlResponse, String> {
        require_exact_fields(
            object,
            &[
                "operation",
                "base_revision",
                "target_revision",
                "hypothesis",
            ],
        )?;
        self.require_mutation_authorization()?;
        let base_revision_id = required_revision(object, "base_revision")?;
        self.require_current_base(&base_revision_id)?;
        let target_revision_id = required_revision(object, "target_revision")?;
        let candidate = self
            .manager
            .stage_rollback(
                base_revision_id,
                target_revision_id,
                parse_hypothesis(object)?,
                Some(self.operation_id.clone()),
                call.id.to_string(),
            )
            .map_err(harness_error)?;
        self.schedule_candidate(call, candidate, "rollback")
    }

    fn require_current_base(&self, base_revision_id: &HarnessRevisionId) -> Result<(), String> {
        if base_revision_id != self.identity.revision_id() {
            return Err(format!(
                "base_revision {base_revision_id} is stale for this epoch; use the active revision {} from tea_harness.status",
                self.identity.revision_id(),
            ));
        }
        Ok(())
    }

    /// Enforce the trusted session mode at the model-facing mutation boundary.
    /// `Author` is deliberately not a prompt-text heuristic: only the host can
    /// write the immutable user-entry marker at operation acceptance.
    fn require_mutation_authorization(&self) -> Result<(), String> {
        let mode = self.manager.self_extension_mode_value();
        if mode == SelfExtensionMode::Off {
            return Err("self-extension is disabled for this session".into());
        }
        if !mode.requires_explicit_user_authorization() {
            return Ok(());
        }
        let snapshot = self.session_snapshot()?;
        if operation_authorizes_harness_mutation(&snapshot, &self.operation_id) {
            return Ok(());
        }
        Err(
            "author mode accepts tea_harness mutations only for a user request explicitly authorized by the host"
                .into(),
        )
    }

    fn schedule_candidate(
        &self,
        call: &ToolCall,
        candidate: HarnessCandidateV1,
        operation: &str,
    ) -> Result<ControlResponse, String> {
        // Candidate staging only becomes durable when its complete immutable
        // catalog is committed. If this write fails, the model sees a normal
        // tool error and the in-memory objects are merely orphan candidates;
        // no activation record can point at them.
        self.persist_catalog()?;
        self.publish_candidate_staged(&candidate);
        let mut value = candidate_value(candidate.clone());
        let object = value
            .as_object_mut()
            .expect("candidate response is always a JSON object");
        object.insert("operation".into(), JsonValue::String(operation.into()));
        if !candidate.validation.accepted || candidate.validation.is_noop {
            let (stage, code, diagnostic) = if candidate.validation.is_noop {
                (
                    ValidationStage::Static,
                    "candidate.noop",
                    "candidate makes no immutable harness change",
                )
            } else if candidate
                .draft
                .changed_surfaces
                .contains(&HarnessSurface::CapabilityBindings)
            {
                (
                    ValidationStage::Capability,
                    "candidate.capability_rejected",
                    "candidate exceeds the frozen capability boundary",
                )
            } else {
                (
                    ValidationStage::Static,
                    "candidate.static_rejected",
                    "candidate failed immutable harness validation",
                )
            };
            self.publish_candidate_rejected(
                Some(candidate.candidate_id.clone()),
                stage,
                code,
                diagnostic,
            );
            object.insert("activation_scheduled".into(), JsonValue::Bool(false));
            object.insert(
                "continuation".into(),
                JsonValue::String(
                    "candidate remains staged for inspection; no activation was scheduled".into(),
                ),
            );
            return Ok(ControlResponse {
                value,
                terminate: false,
                is_error: true,
            });
        }

        let revision_entry_id = EntryId::new(super::supervisor::durable_identifier(
            "entry-harness-revision",
            [self.operation_id.as_str(), call.id.as_str()],
        ))
        .map_err(|error| error.to_string())?;
        let request = tea_session::HarnessActivationRequestedRecord {
            operation_id: self.operation_id.clone(),
            candidate_id: candidate.candidate_id.clone(),
            parent_revision_id: candidate.draft.parent_revision_id.clone(),
            proposed_snapshot_id: candidate.draft.proposed_snapshot_id.clone(),
            revision_entry_id,
        };
        let mut session = self
            .session
            .lock()
            .map_err(|_| "durable session mutex is poisoned".to_owned())?;
        let snapshot = session.snapshot().map_err(|error| error.to_string())?;
        let reduction = reduce_lane(snapshot.clone(), self.lane_id.clone())
            .map_err(|error| error.to_string())?;
        if reduction.lane_state.active_operation.as_ref() != Some(&self.operation_id) {
            return Err("harness mutation belongs to an operation that is no longer active".into());
        }
        if reduction.lane_state.active_harness_revision.as_ref()
            != Some(self.identity.revision_id())
        {
            return Err(
                "harness mutation epoch no longer owns the active immutable branch revision".into(),
            );
        }
        let used = rollover_count(&snapshot, &self.operation_id);
        if used >= self.rollover_budget {
            object.insert("activation_scheduled".into(), JsonValue::Bool(false));
            object.insert(
                "continuation".into(),
                JsonValue::String(format!(
                    "candidate remains staged; this operation exhausted its {} automatic harness rollover budget",
                    self.rollover_budget,
                )),
            );
            return Ok(ControlResponse {
                value,
                terminate: false,
                is_error: true,
            });
        }
        let existing = snapshot
            .records()
            .iter()
            .filter_map(|stored| match &stored.record {
                LaneRecord::HarnessActivationRequested(existing)
                    if existing.operation_id == self.operation_id =>
                {
                    Some(existing)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some(existing) = existing.first() {
            if *existing != &request {
                return Err(
                    "this operation already has a different harness activation request; inspect tea_harness.status before issuing another mutation"
                        .into(),
                );
            }
            object.insert("activation_scheduled".into(), JsonValue::Bool(true));
            object.insert(
                "continuation".into(),
                JsonValue::String("activation was already scheduled idempotently; Tea will continue automatically".into()),
            );
            return Ok(ControlResponse {
                value,
                terminate: true,
                is_error: false,
            });
        }
        session
            .append_record(LaneRecord::HarnessActivationRequested(request))
            .map_err(|error| error.to_string())?;
        object.insert("activation_scheduled".into(), JsonValue::Bool(true));
        object.insert(
            "continuation".into(),
            JsonValue::String("activation is scheduled; Tea will continue automatically under the new immutable snapshot".into()),
        );
        Ok(ControlResponse {
            value,
            terminate: true,
            is_error: false,
        })
    }

    fn session_snapshot(&self) -> Result<tea_session::SessionSnapshot, String> {
        self.session
            .lock()
            .map_err(|_| "durable session mutex is poisoned".to_owned())?
            .snapshot()
            .map_err(|error| error.to_string())
    }

    fn persist_catalog(&self) -> Result<(), String> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| "durable session mutex is poisoned".to_owned())?;
        self.manager
            .persist_catalog(&mut *session, self.artifacts.as_ref())
            .map_err(harness_error)
    }

    fn publish_candidate_staged(&self, candidate: &HarnessCandidateV1) {
        self.events
            .publish(TeaEvent::Harness(HarnessEvent::CandidateStaged {
                lane_id: self.lane_id.clone(),
                candidate_id: candidate.candidate_id.clone(),
                parent_revision_id: candidate.draft.parent_revision_id.clone(),
                snapshot_id: candidate.draft.proposed_snapshot_id.clone(),
                changed_paths: candidate.draft.changed_paths.clone(),
            }));
    }

    fn publish_candidate_rejected(
        &self,
        candidate_id: Option<tea_session::HarnessCandidateId>,
        stage: ValidationStage,
        code: &str,
        diagnostic: &str,
    ) {
        let Ok(code) = DiagnosticCode::new(code) else {
            return;
        };
        self.events
            .publish(TeaEvent::Harness(HarnessEvent::CandidateRejected {
                lane_id: self.lane_id.clone(),
                candidate_id,
                active_revision_id: self.identity.revision_id().clone(),
                stage,
                code,
                diagnostic: bounded_diagnostic(diagnostic),
            }));
    }
}

fn rejection_category(message: &str) -> (ValidationStage, &'static str) {
    if message.contains("capability")
        || message.contains("author mode")
        || message.contains("self-extension is disabled")
    {
        (ValidationStage::Capability, "candidate.capability_rejected")
    } else if message.contains("stale") || message.contains("active immutable branch") {
        (ValidationStage::Activation, "candidate.activation_rejected")
    } else {
        (ValidationStage::Static, "candidate.stage_rejected")
    }
}

fn bounded_diagnostic(value: &str) -> String {
    value.chars().take(240).collect()
}

fn operation_authorizes_harness_mutation(
    snapshot: &tea_session::SessionSnapshot,
    operation_id: &OperationId,
) -> bool {
    snapshot.records().iter().any(|stored| {
        let LaneRecord::OperationStarted(operation) = &stored.record else {
            return false;
        };
        operation.id == *operation_id
            && operation.original_input.iter().any(|entry| {
                matches!(
                    &entry.body,
                    tea_session::SessionEntry::UserMessage(message)
                        if message.metadata.get(AUTHORING_AUTHORIZATION_METADATA_KEY)
                            == Some(&JsonValue::Bool(true))
                )
            })
    })
}

struct ControlResponse {
    value: JsonValue,
    terminate: bool,
    is_error: bool,
}

impl ControlResponse {
    fn plain(value: JsonValue) -> Self {
        Self {
            value,
            terminate: false,
            is_error: false,
        }
    }
}

fn parse_object(call: &ToolCall) -> Result<BTreeMap<String, JsonValue>, String> {
    match JsonValue::parse(call.arguments.as_str()).map_err(|error| error.to_string())? {
        JsonValue::Object(object) => Ok(object),
        other => Err(format!(
            "tea_harness arguments must be an object, got {:?}",
            other.kind()
        )),
    }
}

fn require_exact_fields(
    object: &BTreeMap<String, JsonValue>,
    allowed: &[&str],
) -> Result<(), String> {
    for name in object.keys() {
        if !allowed.contains(&name.as_str()) {
            return Err(format!(
                "tea_harness operation does not accept field {name:?}"
            ));
        }
    }
    Ok(())
}

fn required_string(object: &BTreeMap<String, JsonValue>, name: &str) -> Result<String, String> {
    object
        .get(name)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{name} must be a non-empty string"))
}

fn optional_string(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<Option<String>, String> {
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        _ => Err(format!("{name} must be a non-empty string when supplied")),
    }
}

fn required_object<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, String> {
    object
        .get(name)
        .and_then(JsonValue::as_object)
        .ok_or_else(|| format!("{name} must be an object"))
}

fn required_array<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<&'a [JsonValue], String> {
    object
        .get(name)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| format!("{name} must be an array"))
}

fn optional_u64(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    default: u64,
) -> Result<u64, String> {
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{name} must be a nonnegative integer")),
    }
}

fn optional_usize(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
    default: usize,
    maximum: usize,
) -> Result<usize, String> {
    let value = usize::try_from(optional_u64(object, name, default as u64)?)
        .map_err(|_| format!("{name} exceeds platform bounds"))?;
    if value == 0 || value > maximum {
        return Err(format!("{name} must be within 1..={maximum}"));
    }
    Ok(value)
}

fn required_revision(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<HarnessRevisionId, String> {
    HarnessRevisionId::new(required_string(object, name)?)
        .map_err(|error| format!("{name} must be a valid harness revision ID: {error}"))
}

fn optional_revision(
    object: &BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<Option<HarnessRevisionId>, String> {
    optional_string(object, name)?
        .map(HarnessRevisionId::new)
        .transpose()
        .map_err(|error| format!("{name} must be a valid harness revision ID: {error}"))
}

fn parse_hypothesis(object: &BTreeMap<String, JsonValue>) -> Result<CandidateHypothesis, String> {
    let hypothesis = required_object(object, "hypothesis")?;
    require_exact_fields(
        hypothesis,
        &["failure_signature", "expected_effect", "regression_risk"],
    )?;
    Ok(CandidateHypothesis {
        targeted_evidence: required_string(hypothesis, "failure_signature")?,
        expected_effect: required_string(hypothesis, "expected_effect")?,
        regression_risk: required_string(hypothesis, "regression_risk")?,
    })
}

fn parse_file_patches(
    object: &BTreeMap<String, JsonValue>,
) -> Result<Vec<HarnessFilePatch>, String> {
    required_array(object, "files")?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let patch = value
                .as_object()
                .ok_or_else(|| format!("files[{index}] must be an object"))?;
            let operation = required_string(patch, "operation")?;
            let path = NormalizedPath::new(required_string(patch, "path")?)
                .map_err(|error| format!("files[{index}].path is invalid: {error}"))?;
            match operation.as_str() {
                "upsert" => {
                    require_exact_fields(patch, &["operation", "path", "content"])?;
                    Ok(HarnessFilePatch::Upsert {
                        path,
                        content: required_string(patch, "content")?,
                    })
                }
                "delete" => {
                    require_exact_fields(patch, &["operation", "path", "expected_artifact_id"])?;
                    let artifact_id = ArtifactId::from_hex(&required_string(
                        patch,
                        "expected_artifact_id",
                    )?)
                    .map_err(|error| {
                        format!("files[{index}].expected_artifact_id must be BLAKE3 hex: {error}")
                    })?;
                    Ok(HarnessFilePatch::Delete {
                        path,
                        expected_artifact_id: artifact_id,
                    })
                }
                _ => Err(format!("files[{index}].operation must be upsert or delete",)),
            }
        })
        .collect()
}

fn parse_registry_operations(
    object: &BTreeMap<String, JsonValue>,
) -> Result<Vec<RegistryOperation>, String> {
    required_array(object, "registry_operations")?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let operation = value
                .as_object()
                .ok_or_else(|| format!("registry_operations[{index}] must be an object"))?;
            require_exact_fields(operation, &["operation", "plugin_id"])?;
            let plugin_id = required_string(operation, "plugin_id")?;
            match required_string(operation, "operation")?.as_str() {
                "add" => Ok(RegistryOperation::Add { plugin_id }),
                "remove" => Ok(RegistryOperation::Remove { plugin_id }),
                _ => Err(format!(
                    "registry_operations[{index}].operation must be add or remove",
                )),
            }
        })
        .collect()
}

fn candidate_value(candidate: HarnessCandidateV1) -> JsonValue {
    JsonValue::object([
        (
            "candidate_id",
            JsonValue::String(candidate.candidate_id.to_string()),
        ),
        (
            "snapshot_id",
            JsonValue::String(candidate.draft.proposed_snapshot_id.to_string()),
        ),
        (
            "validation",
            JsonValue::object([
                ("accepted", JsonValue::Bool(candidate.validation.accepted)),
                ("is_noop", JsonValue::Bool(candidate.validation.is_noop)),
                (
                    "diagnostics",
                    JsonValue::Array(
                        candidate
                            .validation
                            .diagnostics
                            .into_iter()
                            .map(JsonValue::String)
                            .collect(),
                    ),
                ),
            ]),
        ),
    ])
}

fn diff_value(diff: HarnessSourceDiff) -> JsonValue {
    JsonValue::object([
        ("path", JsonValue::String(diff.path.to_string())),
        (
            "before_artifact_id",
            diff.before
                .map(|id| JsonValue::String(id.to_hex()))
                .unwrap_or(JsonValue::Null),
        ),
        (
            "after_artifact_id",
            diff.after
                .map(|id| JsonValue::String(id.to_hex()))
                .unwrap_or(JsonValue::Null),
        ),
    ])
}

fn command(name: &str, description: &str) -> JsonValue {
    JsonValue::object([
        ("name", JsonValue::String(name.into())),
        ("description", JsonValue::String(description.into())),
    ])
}

fn actor_name(actor: HarnessActor) -> &'static str {
    match actor {
        HarnessActor::Host => "host",
        HarnessActor::Operator => "operator",
        HarnessActor::Model => "model",
    }
}

fn revision_reason(reason: &HarnessRevisionReason) -> &'static str {
    match reason {
        HarnessRevisionReason::Initial => "initial",
        HarnessRevisionReason::CandidateActivation => "candidate_activation",
        HarnessRevisionReason::GlobalRebase => "global_rebase",
        HarnessRevisionReason::Rollback => "rollback",
    }
}

fn rollover_count(snapshot: &tea_session::SessionSnapshot, operation_id: &OperationId) -> u32 {
    snapshot
        .records()
        .iter()
        .filter(|stored| {
            matches!(
                &stored.record,
                LaneRecord::EpochFinished(EpochFinishedRecord {
                    operation_id: finished_operation,
                    reason: EpochFinishReason::ActivationPending,
                    ..
                }) if finished_operation == operation_id
            )
        })
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn control_result(
    call: ToolCall,
    value: JsonValue,
    terminate: bool,
    is_error: bool,
) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id,
        content: json_text(value).expect("control responses are canonical JSON"),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate,
        is_error,
        failure: is_error.then(ToolFailure::recoverable),
    }
}

fn error_result(call: ToolCall, code: &str, message: String) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id,
        content: json_text(JsonValue::object([
            ("ok", JsonValue::Bool(false)),
            ("code", JsonValue::String(code.into())),
            ("message", JsonValue::String(message)),
        ]))
        .expect("control errors are canonical JSON"),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: true,
        failure: Some(ToolFailure::recoverable()),
    }
}

fn json_text(value: JsonValue) -> Result<String, ToolError> {
    value
        .to_json_string()
        .map_err(|error| ToolError::Execution {
            tool: STABLE_HARNESS_TOOL_NAME.into(),
            message: error.to_string(),
        })
}

fn unsigned(value: u64) -> JsonValue {
    JsonValue::Number(JsonNumber::Unsigned(value))
}

fn bytes_value(bytes: &[u8]) -> JsonValue {
    match std::str::from_utf8(bytes) {
        Ok(value) => JsonValue::object([
            ("encoding", JsonValue::String("utf8".into())),
            ("data", JsonValue::String(value.into())),
        ]),
        Err(_) => JsonValue::object([
            ("encoding", JsonValue::String("hex".into())),
            ("data", JsonValue::String(hex(bytes))),
        ]),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn harness_error(error: HarnessError) -> String {
    error.to_string()
}

fn harness_schema() -> &'static JsonValue {
    static SCHEMA: std::sync::OnceLock<JsonValue> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        JsonValue::parse(
            r#"{"type":"object","required":["operation"],"properties":{"operation":{"type":"string","enum":["status","help","list","read","diff","apply","rollback"]},"kind":{"type":"string"},"maximum":{"type":"integer","minimum":1},"revision":{"type":"string"},"path":{"type":"string"},"offset":{"type":"integer","minimum":0},"maximum_bytes":{"type":"integer","minimum":1},"base_revision":{"type":"string"},"target_revision":{"type":"string"},"hypothesis":{"type":"object"},"files":{"type":"array"},"registry_operations":{"type":"array"}},"additionalProperties":false}"#,
        )
        .expect("stable tea_harness schema is valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tea_session::{
        HarnessRevisionId, MemorySession, ModelHarnessProfileId, OperationKind,
        OperationStartedRecord, ProvisionedEntry, SessionHeader, SessionId, SessionWriter,
    };

    #[test]
    fn authoring_authorization_is_a_host_recorded_user_entry_fact() {
        let operation_id = OperationId::new("authoring-operation").expect("fixture operation ID");
        let make_session = |authorized: bool| {
            let mut session = MemorySession::create(SessionHeader::new(
                SessionId::new(if authorized {
                    "authorized-authoring-session"
                } else {
                    "ordinary-authoring-session"
                })
                .expect("fixture session ID"),
                "fixture-workspace",
                Default::default(),
            ))
            .expect("fixture session creates");
            let mut entry = ProvisionedEntry::user(
                EntryId::new(if authorized {
                    "authorized-authoring-input"
                } else {
                    "ordinary-authoring-input"
                })
                .expect("fixture entry ID"),
                "please edit the harness",
            );
            if authorized {
                let tea_session::SessionEntry::UserMessage(message) = &mut entry.body else {
                    unreachable!("user constructor must create a user entry");
                };
                message.metadata.insert(
                    AUTHORING_AUTHORIZATION_METADATA_KEY.into(),
                    JsonValue::Bool(true),
                );
            }
            session
                .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
                    operation_id.clone(),
                    LaneId::main(),
                    None,
                    OperationKind::Run,
                    vec![entry],
                    HarnessRevisionId::new("authoring-revision").expect("fixture revision ID"),
                    ModelHarnessProfileId::new("authoring-profile").expect("fixture profile ID"),
                )))
                .expect("fixture operation record appends");
            session.snapshot().expect("fixture snapshot")
        };

        assert!(!operation_authorizes_harness_mutation(
            &make_session(false),
            &operation_id,
        ));
        assert!(operation_authorizes_harness_mutation(
            &make_session(true),
            &operation_id,
        ));
    }
}
