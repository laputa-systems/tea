//! Durable session construction and one-shot streaming for the terminal host.
//!
//! The host owns filesystem placement and provider configuration, while
//! `tea-session` owns the append-only WAL and `tea-harness` owns execution.
//! This module deliberately starts every one-shot request through the same
//! managed harness boundary used by an interactive session; it never creates
//! a second, direct-core persistence path.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::event::AgentEventKind;
use tea_core::scheduler::ModelProvider;
use tea_core::state::{
    AgentMessage, AgentToolCall, MessageId, ModelDescriptor, SerializedJson, ThinkingLevel,
    ToolCallId,
};
use tea_core::tool::ToolExecutionMode;
use tea_core::AgentConfiguration;
use tea_harness::{
    CoreEpochTemplate, DurableHarness, HarnessActor, HarnessIdentity, HarnessManager,
    HarnessRepository, HarnessResourceLimits, HarnessSnapshotSpec, ModelHarnessProfile,
    SelfExtensionMode, TeaEvent, TeaEventSubscription, ToolPresentationDescriptor,
    SELF_EXTENSION_MODE_METADATA_KEY,
};
use tea_protocol::JsonValue;
use tea_session::{
    CanonicalHashWriter, Digest, DurabilityMode, EntryId, HarnessRevisionChangedEntry,
    JsonlSession, LaneId, ModelChangedEntry, PayloadRef, ProvisionedEntry, SessionEntry,
    SessionHeader, SessionId, SessionSnapshot, SessionWriter, ThinkingChangedEntry,
    SESSION_FORMAT_VERSION,
};

use super::compaction::ProviderCompactor;
use super::error::AppError;

/// Concrete durable supervisor used by the terminal application.
pub(super) type HostHarness = DurableHarness<JsonlSession>;

/// Bounded metadata shown by the terminal session picker. The complete
/// durable snapshot remains behind the writer/reopen boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DurableSessionSummary {
    pub(super) id: String,
    pub(super) model: Option<ModelDescriptor>,
}

static NEXT_SESSION_NONCE: AtomicU64 = AtomicU64::new(1);

/// Create one fresh session-local managed harness under the host-selected Tea
/// home. The committed initial branch revision and immutable catalog exist
/// before the harness can begin an operation.
pub(super) fn create_host_harness(
    tea_home: &Path,
    workspace: &Path,
    configuration: AgentConfiguration,
    model: ModelDescriptor,
    provider: Arc<dyn ModelProvider>,
    thinking_level: ThinkingLevel,
    compactor: Option<Arc<ProviderCompactor>>,
    automatic_compaction: AutomaticCompactionPolicy,
) -> Result<Arc<HostHarness>, AppError> {
    let sessions_root = tea_home.join("sessions");
    ensure_private_directory(tea_home)?;
    ensure_private_directory(&sessions_root)?;
    let workspace_key = workspace_key(workspace);
    let workspace_root = sessions_root.join(&workspace_key);
    ensure_private_directory(&workspace_root)?;

    let profile = model_profile(&model)?;
    let template = epoch_template(
        Arc::clone(&provider),
        configuration.clone(),
        model.clone(),
        thinking_level,
        compactor,
        automatic_compaction,
    );
    let snapshot_spec = snapshot_spec(&configuration, &profile);
    let created_at_ms = now_ms()?;

    // Session identity is only an opaque directory/name key. A collision is
    // still rejected by `JsonlSession::create`; retrying preserves the rule
    // that a new host request never opens or overwrites an unrelated session.
    for _ in 0..8 {
        let nonce = NEXT_SESSION_NONCE.fetch_add(1, Ordering::Relaxed);
        let session_id = new_session_id(workspace, created_at_ms, nonce)?;
        let directory = workspace_root.join(format!("{}.tea", session_id.as_str()));
        let mut metadata = BTreeMap::new();
        metadata.insert(
            SELF_EXTENSION_MODE_METADATA_KEY.into(),
            SelfExtensionMode::Off.metadata_value(),
        );
        metadata.insert(
            "tea.model.provider".into(),
            JsonValue::String(model.provider.clone()),
        );
        metadata.insert(
            "tea.model.requested".into(),
            JsonValue::String(model.model.clone()),
        );
        if let Some(revision) = &model.revision {
            metadata.insert(
                "tea.model.returned_revision".into(),
                JsonValue::String(revision.clone()),
            );
        }
        metadata.insert(
            "tea.thinking".into(),
            JsonValue::String(thinking_level_name(thinking_level).into()),
        );
        let header = SessionHeader::new(
            session_id,
            workspace.to_string_lossy().into_owned(),
            metadata,
        );
        let mut session = match JsonlSession::create(&directory, header, DurabilityMode::Strict) {
            Ok(session) => session,
            Err(tea_session::SessionError::Io { message, .. })
                if message.contains("existing directory") =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let artifacts: Arc<dyn tea_session::ArtifactStore> =
            Arc::new(session.artifact_store().map_err(AppError::from)?);
        let mut repository = HarnessRepository::new(Arc::clone(&artifacts));
        let snapshot = repository
            .stage_snapshot(snapshot_spec.clone())
            .map_err(|error| AppError::Setup(error.to_string()))?;
        let revision = repository
            .seed_revision(snapshot.id.clone(), HarnessActor::Host, created_at_ms)
            .map_err(|error| AppError::Setup(error.to_string()))?;
        let model_entry_id = EntryId::new(format!("initial-model-{}", nonce))
            .map_err(|error| AppError::Setup(error.to_string()))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: model_entry_id,
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: model.provider.clone(),
                    model: model.model.clone(),
                    revision: model.revision.clone(),
                }),
            },
        )?;
        let thinking_entry_id = EntryId::new(format!("initial-thinking-{}", nonce))
            .map_err(|error| AppError::Setup(error.to_string()))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: thinking_entry_id,
                body: SessionEntry::ThinkingChanged(ThinkingChangedEntry {
                    level: thinking_level_name(thinking_level).into(),
                }),
            },
        )?;
        let revision_entry_id = EntryId::new(format!("initial-revision-{}", nonce))
            .map_err(|error| AppError::Setup(error.to_string()))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: revision_entry_id,
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id.clone(),
                    snapshot_id: snapshot.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let manager = Arc::new(
            HarnessManager::new(repository, template, Default::default())
                .self_extension_mode(SelfExtensionMode::Off),
        );
        let identity = HarnessIdentity::new(revision.revision_id, snapshot.id, profile.profile_id);
        let harness =
            DurableHarness::new_with_artifact_store(session, manager, identity, artifacts)?;
        return Ok(Arc::new(harness));
    }

    Err(AppError::Setup(
        "could not allocate a unique durable session directory".into(),
    ))
}

/// List durable sessions scoped to one explicit workspace. This deliberately
/// reads only each directory's fixed header; opening a session would acquire
/// its sole writer lock and make an idle, already-open session unlistable.
pub(super) fn list_host_sessions(
    tea_home: &Path,
    workspace: &Path,
) -> Result<Vec<DurableSessionSummary>, AppError> {
    let root = session_workspace_root(tea_home, workspace);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::Setup(format!(
                "could not list durable sessions in {}: {error}",
                root.display()
            )));
        }
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::Setup(format!("could not inspect {}: {error}", root.display()))
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            AppError::Setup(format!(
                "durable session directory under {} must have a UTF-8 name",
                root.display()
            ))
        })?;
        if !name.ends_with(".tea") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            AppError::Setup(format!("could not inspect {}: {error}", path.display()))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::Setup(format!(
                "durable session path {} must be a real directory",
                path.display()
            )));
        }
        let header = read_host_session_header(&path)?;
        if header.workspace != workspace.to_string_lossy() {
            return Err(AppError::Setup(format!(
                "durable session {} belongs to a different workspace",
                header.session_id
            )));
        }
        sessions.push(DurableSessionSummary {
            id: header.session_id,
            model: header.model,
        });
    }
    sessions.sort_by(|left, right| right.id.cmp(&left.id));
    Ok(sessions)
}

/// Reopen one durable session with the exact model selected by its immutable
/// header. The supervisor reconstructs its active revision from the committed
/// catalog and semantic branch, then the caller may resume any open operation.
pub(super) fn reopen_host_harness(
    tea_home: &Path,
    workspace: &Path,
    session_id: &str,
    configuration: AgentConfiguration,
    selected_model: ModelDescriptor,
    provider: Arc<dyn ModelProvider>,
    compactor: Option<Arc<ProviderCompactor>>,
    automatic_compaction: AutomaticCompactionPolicy,
) -> Result<Arc<HostHarness>, AppError> {
    let session_id = SessionId::new(session_id.to_owned())
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let directory =
        session_workspace_root(tea_home, workspace).join(format!("{}.tea", session_id.as_str()));
    let session = JsonlSession::open(&directory, DurabilityMode::Strict)?;
    let snapshot = session.snapshot()?;
    let header = host_session_header_from_snapshot(&snapshot)?;
    if header.session_id != session_id.as_str() {
        return Err(AppError::Setup(format!(
            "durable session directory {} disagrees with its immutable header",
            directory.display()
        )));
    }
    if header.workspace != workspace.to_string_lossy() {
        return Err(AppError::Setup(format!(
            "durable session {} belongs to workspace {}; current workspace is {}",
            session_id,
            header.workspace,
            workspace.display()
        )));
    }
    let stored_model = header.model.ok_or_else(|| {
        AppError::Setup("durable session header is missing its immutable model identity".into())
    })?;
    if stored_model != selected_model {
        return Err(AppError::Setup(format!(
            "durable session {} requires model {}/{}; select that exact model before reopening",
            session_id, stored_model.provider, stored_model.model
        )));
    }
    let artifacts: Arc<dyn tea_session::ArtifactStore> =
        Arc::new(session.artifact_store().map_err(AppError::from)?);
    let template = epoch_template(
        provider,
        configuration,
        selected_model,
        header.thinking_level,
        compactor,
        automatic_compaction,
    );
    let manager = Arc::new(
        HarnessManager::new(
            HarnessRepository::new(Arc::clone(&artifacts)),
            template,
            Default::default(),
        )
        .self_extension_mode(header.self_extension_mode),
    );
    let harness = DurableHarness::reopen_with_artifact_store(session, manager, artifacts)?;
    harness.verify_durable_state()?;
    Ok(Arc::new(harness))
}

/// Convert the active semantic branch to a presentation-only core-message
/// projection. It is intentionally not used to restore a core agent: the
/// durable supervisor remains the sole execution and recovery authority.
pub(super) fn project_host_messages(
    snapshot: &SessionSnapshot,
) -> Result<Vec<AgentMessage>, AppError> {
    let reduction = tea_session::reduce_lane(snapshot.clone(), LaneId::main())
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let entries = snapshot
        .entries()
        .iter()
        .map(|entry| (entry.header.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut branch = Vec::new();
    let mut cursor = reduction.lane_state.leaf_id;
    while let Some(entry_id) = cursor {
        let entry = entries.get(&entry_id).ok_or_else(|| {
            AppError::Setup(format!(
                "durable session branch refers to missing entry {entry_id}",
            ))
        })?;
        cursor = entry.header.parent_id.clone();
        branch.push(*entry);
    }
    branch.reverse();
    let mut messages = Vec::new();
    for entry in branch {
        let message_id = MessageId(messages.len() as u64 + 1);
        match &entry.body {
            SessionEntry::UserMessage(user) => messages.push(AgentMessage::User {
                id: message_id,
                content: user.content.clone(),
            }),
            SessionEntry::AssistantMessage(assistant) => {
                let tool_calls = assistant
                    .tool_calls
                    .iter()
                    .map(|call| {
                        Ok(AgentToolCall {
                            id: ToolCallId::new(call.id.clone()).map_err(|error| {
                                AppError::Setup(format!(
                                    "durable assistant entry has invalid tool call ID: {error}",
                                ))
                            })?,
                            name: call.name.clone(),
                            arguments: SerializedJson::new(
                                call.arguments.to_json_string().map_err(|error| {
                                    AppError::Setup(format!(
                                        "durable assistant arguments cannot encode: {error}",
                                    ))
                                })?,
                            ),
                        })
                    })
                    .collect::<Result<Vec<_>, AppError>>()?;
                messages.push(AgentMessage::Assistant {
                    id: message_id,
                    content: assistant.content.clone(),
                    tool_calls,
                    stop_reason: None,
                    error_message: assistant.error_message.clone(),
                });
            }
            SessionEntry::ToolResult(result) => messages.push(AgentMessage::ToolResult {
                id: message_id,
                tool_call_id: ToolCallId::new(result.tool_call_id.clone()).map_err(|error| {
                    AppError::Setup(format!(
                        "durable tool result has invalid tool call ID: {error}",
                    ))
                })?,
                tool_name: result.tool_name.clone(),
                content: projected_tool_content(result),
                details: None,
                usage: Some(core_usage(&result.usage)),
                added_tool_names: Vec::new(),
                terminate: result.terminate,
                is_error: result.is_error,
                failure: None,
            }),
            SessionEntry::Compaction(compaction) => messages.push(AgentMessage::Assistant {
                id: message_id,
                content: compaction.summary.clone(),
                tool_calls: Vec::new(),
                stop_reason: None,
                error_message: None,
            }),
            SessionEntry::BranchSummary(summary) => messages.push(AgentMessage::Assistant {
                id: message_id,
                content: summary.summary.clone(),
                tool_calls: Vec::new(),
                stop_reason: None,
                error_message: None,
            }),
            SessionEntry::ModelChanged(_)
            | SessionEntry::ThinkingChanged(_)
            | SessionEntry::ToolActivationChanged(_)
            | SessionEntry::HarnessRevisionChanged(_)
            | SessionEntry::PluginMemory(_)
            | SessionEntry::Custom(_) => {}
        }
    }
    Ok(messages)
}

/// Drive one durable prompt while forwarding only assistant text to stdout.
pub(super) async fn stream_host_prompt(
    harness: Arc<HostHarness>,
    subscription: TeaEventSubscription,
    prompt: String,
) -> Result<(), AppError> {
    let mut drive = Box::pin(harness.run_prompt(prompt));
    loop {
        drain_prompt_events(&subscription)?;
        if let Some(result) = smol::future::poll_once(&mut drive).await {
            drain_prompt_events(&subscription)?;
            result?;
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(b"\n")
                .map_err(|error| AppError::Setup(format!("could not write response: {error}")))?;
            stdout
                .flush()
                .map_err(|error| AppError::Setup(format!("could not flush response: {error}")))?;
            return Ok(());
        }
        smol::future::yield_now().await;
    }
}

fn drain_prompt_events(subscription: &TeaEventSubscription) -> Result<(), AppError> {
    let mut stdout = io::stdout().lock();
    let mut wrote = false;
    while let Ok(event) = subscription.try_recv() {
        if let TeaEvent::Agent(event) = event {
            if let AgentEventKind::MessageUpdate {
                text_delta: Some(text),
                ..
            } = event.kind
            {
                stdout.write_all(text.as_bytes()).map_err(|error| {
                    AppError::Setup(format!("could not write response: {error}"))
                })?;
                wrote = true;
            }
        }
    }
    if wrote {
        stdout
            .flush()
            .map_err(|error| AppError::Setup(format!("could not flush response: {error}")))?;
    }
    Ok(())
}

fn epoch_template(
    provider: Arc<dyn ModelProvider>,
    configuration: AgentConfiguration,
    model: ModelDescriptor,
    thinking_level: ThinkingLevel,
    compactor: Option<Arc<ProviderCompactor>>,
    automatic_compaction: AutomaticCompactionPolicy,
) -> CoreEpochTemplate {
    let mut template = CoreEpochTemplate::from_agent_configuration(provider, configuration)
        .model(model)
        .thinking_level(thinking_level)
        .automatic_compaction(automatic_compaction);
    if let Some(compactor) = compactor {
        let compactor: Arc<dyn tea_core::compaction::Compactor> = compactor;
        template = template.compactor(compactor);
    }
    template
}

fn model_profile(model: &ModelDescriptor) -> Result<ModelHarnessProfile, AppError> {
    ModelHarnessProfile::new(
        model.provider.clone(),
        model.model.clone(),
        model.revision.clone(),
        "tea-terminal-host-v1",
        "tea-core-canonical-v1",
        "tea-provider-summary-v1",
        "tea-recoverable-projection-v1",
    )
    .map_err(|error| AppError::Setup(error.to_string()))
}

fn snapshot_spec(
    configuration: &AgentConfiguration,
    profile: &ModelHarnessProfile,
) -> HarnessSnapshotSpec {
    let tools = configuration
        .tools
        .definitions()
        .into_iter()
        .map(|tool| ToolPresentationDescriptor {
            name: tool.name,
            description: tool.description,
            schema: tool.schema,
            execution_mode: match tool.execution_mode {
                ToolExecutionMode::Sequential => "sequential".into(),
                ToolExecutionMode::Parallel => "parallel".into(),
            },
        })
        .collect::<Vec<_>>();
    HarnessSnapshotSpec {
        base_profile_digest: host_profile_digest(configuration),
        base_system_prompt: configuration.system_prompt.clone(),
        model_harness_profile: profile.profile_id.clone(),
        self_extension_addendum: None,
        ordered_global_plugins: Vec::new(),
        ordered_session_plugins: Vec::new(),
        prompt_sections: Vec::new(),
        plugin_prompt_sections: Vec::new(),
        tool_presentations: tools,
        plugin_tool_presentations: Vec::new(),
        hook_bundle_digest: Digest::from_bytes("tea-agent-openai-context-hook-v1"),
        capability_bindings: Vec::new(),
        resource_limits: HarnessResourceLimits::default(),
        compaction_policy_digest: Digest::from_bytes("tea-agent-provider-compactor-v1"),
        tool_projection_digest: Digest::from_bytes("tea-core-recoverable-projection-v1"),
        failure_policy_digest: Digest::from_bytes("tea-core-tool-failure-policy-v1"),
    }
}

fn host_profile_digest(configuration: &AgentConfiguration) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-agent-host-profile", 1, 1);
    writer.string("system_prompt", &configuration.system_prompt);
    let definitions = configuration.tools.definitions();
    writer.u64("tool_count", definitions.len() as u64);
    for tool in definitions {
        writer.string("tool_name", &tool.name);
        writer.string("tool_description", &tool.description);
        writer.string(
            "tool_schema",
            &tool
                .schema
                .to_json_string()
                .expect("registered host tool schemas are JSON encodable"),
        );
        writer.string(
            "tool_execution_mode",
            match tool.execution_mode {
                ToolExecutionMode::Sequential => "sequential",
                ToolExecutionMode::Parallel => "parallel",
            },
        );
    }
    writer.finish()
}

#[derive(Clone, Debug)]
struct HostSessionHeader {
    session_id: String,
    workspace: String,
    model: Option<ModelDescriptor>,
    thinking_level: ThinkingLevel,
    self_extension_mode: SelfExtensionMode,
}

fn session_workspace_root(tea_home: &Path, workspace: &Path) -> PathBuf {
    tea_home.join("sessions").join(workspace_key(workspace))
}

fn read_host_session_header(directory: &Path) -> Result<HostSessionHeader, AppError> {
    let path = directory.join("session.jsonl");
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        AppError::Setup(format!("could not inspect {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Setup(format!(
            "durable session header {} must be a regular file",
            path.display()
        )));
    }
    let source = fs::read_to_string(&path)
        .map_err(|error| AppError::Setup(format!("could not read {}: {error}", path.display())))?;
    let line = source.lines().next().ok_or_else(|| {
        AppError::Setup(format!(
            "durable session {} has no header",
            directory.display()
        ))
    })?;
    let value = JsonValue::parse(line).map_err(|error| {
        AppError::Setup(format!(
            "durable session header {} is invalid JSON: {error}",
            path.display()
        ))
    })?;
    host_session_header_from_value(&value)
}

fn host_session_header_from_snapshot(
    snapshot: &SessionSnapshot,
) -> Result<HostSessionHeader, AppError> {
    let header = snapshot.header();
    if header.kind != "session" || header.version != SESSION_FORMAT_VERSION {
        return Err(AppError::Setup("unsupported durable session format".into()));
    }
    host_session_metadata(
        header.session_id.to_string(),
        header.workspace.clone(),
        &header.metadata,
    )
}

fn host_session_header_from_value(value: &JsonValue) -> Result<HostSessionHeader, AppError> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::Setup("durable session header must be a JSON object".into()))?;
    let kind = required_header_string(object, "kind")?;
    let version = object
        .get("version")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| {
            AppError::Setup("durable session header version must be an integer".into())
        })?;
    if kind != "session" || version != u64::from(SESSION_FORMAT_VERSION) {
        return Err(AppError::Setup("unsupported durable session format".into()));
    }
    let session_id = required_header_string(object, "session_id")?;
    SessionId::new(session_id.clone()).map_err(|error| AppError::Setup(error.to_string()))?;
    let workspace = required_header_string(object, "workspace")?;
    let metadata = object
        .get("metadata")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| {
            AppError::Setup("durable session header metadata must be an object".into())
        })?;
    host_session_metadata(session_id, workspace, metadata)
}

fn host_session_metadata(
    session_id: String,
    workspace: String,
    metadata: &BTreeMap<String, JsonValue>,
) -> Result<HostSessionHeader, AppError> {
    let provider = metadata
        .get("tea.model.provider")
        .and_then(JsonValue::as_str);
    let model = metadata
        .get("tea.model.requested")
        .and_then(JsonValue::as_str);
    let model = match (provider, model) {
        (Some(provider), Some(model)) => Some(ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            revision: metadata
                .get("tea.model.returned_revision")
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        AppError::Setup(
                            "durable session returned model revision must be a string".into(),
                        )
                    })
                })
                .transpose()?,
        }),
        (None, None) => None,
        _ => {
            return Err(AppError::Setup(
                "durable session model metadata must name both provider and requested model".into(),
            ));
        }
    };
    let thinking_level = metadata
        .get("tea.thinking")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| AppError::Setup("durable session is missing its thinking level".into()))
        .and_then(parse_thinking_level)?;
    let self_extension_mode = metadata
        .get(SELF_EXTENSION_MODE_METADATA_KEY)
        .and_then(JsonValue::as_str)
        .and_then(SelfExtensionMode::parse)
        .ok_or_else(|| {
            AppError::Setup("durable session has an invalid self-extension mode".into())
        })?;
    Ok(HostSessionHeader {
        session_id,
        workspace,
        model,
        thinking_level,
        self_extension_mode,
    })
}

fn required_header_string(
    object: &BTreeMap<String, JsonValue>,
    field: &str,
) -> Result<String, AppError> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .ok_or_else(|| AppError::Setup(format!("durable session header {field} must be a string")))
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn parse_thinking_level(value: &str) -> Result<ThinkingLevel, AppError> {
    match value {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(AppError::Setup(format!(
            "durable session has unsupported thinking level {value:?}",
        ))),
    }
}

fn projected_tool_content(result: &tea_session::ToolResultEntry) -> String {
    result
        .model_projection
        .get("content")
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| match &result.full_result {
            PayloadRef::Inline(value) => value
                .to_json_string()
                .unwrap_or_else(|_| "[unrenderable durable tool result]".into()),
            PayloadRef::Artifact { artifact_id, .. } => {
                format!("[full durable tool result: tea-artifact://blake3/{artifact_id}]")
            }
        })
}

fn core_usage(usage: &tea_session::Usage) -> tea_core::Usage {
    tea_core::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost: usage.cost.clone(),
    }
}

fn workspace_key(workspace: &Path) -> String {
    Digest::from_bytes(workspace.to_string_lossy().as_bytes()).to_hex()
}

fn new_session_id(workspace: &Path, created_at_ms: u64, nonce: u64) -> Result<SessionId, AppError> {
    let mut writer = CanonicalHashWriter::new("tea-agent-session-allocation", 1, 1);
    writer.string("workspace", &workspace.to_string_lossy());
    writer.u64("created_at_ms", created_at_ms);
    writer.u64("process_id", u64::from(std::process::id()));
    writer.u64("nonce", nonce);
    SessionId::new(format!("session-{}", writer.finish().to_hex()))
        .map_err(|error| AppError::Setup(error.to_string()))
}

fn now_ms() -> Result<u64, AppError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .map_err(|error| AppError::Setup(format!("system clock is before Unix epoch: {error}")))
}

fn ensure_private_directory(path: &Path) -> Result<(), AppError> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| {
                AppError::Setup(format!("could not create {}: {error}", path.display()))
            })?;
        }
        Err(error) => {
            return Err(AppError::Setup(format!(
                "could not create {}: {error}",
                path.display()
            )));
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        AppError::Setup(format!("could not inspect {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::Setup(format!(
            "{} must be a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            AppError::Setup(format!(
                "could not make {} private: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::host::host_configuration;
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tea_core::hooks::NoHooks;
    use tea_core::scheduler::{
        CancellationToken, ModelFuture, ModelRequest, ModelStream, ModelStreamEvent,
    };
    use tea_core::state::StopReason;
    use tea_core::tool::ToolRegistry;
    use tea_core::DefaultCodingTools;

    #[derive(Debug)]
    struct StopProvider;

    impl ModelProvider for StopProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            Box::pin(std::future::ready(Ok(Box::new(ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("durable".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            }) as _)))
        }
    }

    #[derive(Debug)]
    struct ContextCheckingProvider;

    impl ModelProvider for ContextCheckingProvider {
        fn stream<'a>(
            &'a self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            let events = if request.context == r#"[{"content":"hello","role":"user"}]"# {
                vec![
                    ModelStreamEvent::TextDelta("ok".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ]
            } else {
                vec![ModelStreamEvent::Error {
                    message: "durable host sent invalid OpenAI-compatible context".into(),
                }]
            };
            Box::pin(std::future::ready(
                Ok(Box::new(ModelStream { events }) as _),
            ))
        }
    }

    fn temporary_home() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "tea-durable-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("temporary home creates");
        path
    }

    #[test]
    fn fresh_host_harness_commits_its_initial_lineage_before_running() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let configuration = AgentConfiguration::new(
            "trusted system prompt",
            ToolRegistry::default(),
            Arc::new(NoHooks),
        );
        let model = ModelDescriptor {
            provider: "fixture".into(),
            model: "fixture-model".into(),
            revision: Some("fixture-revision".into()),
        };
        let harness = create_host_harness(
            &home,
            &workspace,
            configuration,
            model,
            Arc::new(StopProvider),
            ThinkingLevel::Off,
            None,
            AutomaticCompactionPolicy::disabled(),
        )
        .expect("host harness creates");
        let before = harness.snapshot().expect("initial session snapshot");
        assert!(before
            .entries()
            .iter()
            .any(|entry| matches!(entry.body, SessionEntry::HarnessRevisionChanged(_))));
        assert!(before
            .facts()
            .iter()
            .any(|fact| matches!(fact.fact, tea_session::SessionFact::HarnessCatalog(_))));
        let operation =
            smol::block_on(harness.run_prompt("persisted prompt")).expect("durable prompt settles");
        assert!(operation.is_completed());
        let after = harness.snapshot().expect("completed session snapshot");
        assert!(after
            .records()
            .iter()
            .any(|record| matches!(record.record, tea_session::LaneRecord::OperationStarted(_))));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn durable_host_sends_openai_compatible_context() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let configuration = host_configuration(
            DefaultCodingTools::new(&workspace).expect("default tools configure"),
        )
        .expect("durable host configuration assembles");
        let harness = create_host_harness(
            &home,
            &workspace,
            configuration,
            ModelDescriptor {
                provider: "openrouter".into(),
                model: "fixture-model".into(),
                revision: None,
            },
            Arc::new(ContextCheckingProvider),
            ThinkingLevel::Off,
            None,
            AutomaticCompactionPolicy::disabled(),
        )
        .expect("durable host harness creates");

        smol::block_on(harness.run_prompt("hello"))
            .expect("durable host request should use compatible context");

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn host_reopen_uses_the_durable_catalog_and_semantic_branch() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let configuration = AgentConfiguration::new(
            "trusted system prompt",
            ToolRegistry::default(),
            Arc::new(NoHooks),
        );
        let model = ModelDescriptor {
            provider: "fixture".into(),
            model: "fixture-model".into(),
            revision: Some("fixture-revision".into()),
        };
        let provider: Arc<dyn ModelProvider> = Arc::new(StopProvider);
        let harness = create_host_harness(
            &home,
            &workspace,
            configuration.clone(),
            model.clone(),
            Arc::clone(&provider),
            ThinkingLevel::High,
            None,
            AutomaticCompactionPolicy::disabled(),
        )
        .expect("host harness creates");
        smol::block_on(harness.run_prompt("retain this durable prompt"))
            .expect("durable prompt settles");
        let before = harness.snapshot().expect("completed durable snapshot");
        let session_id = before.header().session_id.to_string();
        drop(harness);

        let listed = list_host_sessions(&home, &workspace).expect("durable sessions list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session_id);
        assert_eq!(listed[0].model, Some(model.clone()));

        let reopened = reopen_host_harness(
            &home,
            &workspace,
            &session_id,
            configuration,
            model,
            provider,
            None,
            AutomaticCompactionPolicy::disabled(),
        )
        .expect("host reopen reconstructs the durable manager");
        let after = reopened.snapshot().expect("reopened durable snapshot");
        assert_eq!(after.last_sequence(), before.last_sequence());
        let messages = project_host_messages(&after).expect("presentation projection");
        assert!(matches!(
            messages.first(),
            Some(AgentMessage::User { content, .. }) if content == "retain this durable prompt"
        ));
        assert!(matches!(
            messages.get(1),
            Some(AgentMessage::Assistant { content, .. }) if content == "durable"
        ));

        drop(reopened);
        let _ = fs::remove_dir_all(home);
    }
}
