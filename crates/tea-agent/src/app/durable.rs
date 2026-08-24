//! Durable session construction and one-shot streaming for the terminal host.
//!
//! The host owns filesystem placement and provider configuration, while
//! `tea-session` owns the append-only WAL and `tea-core` owns execution.
//! This module deliberately starts every one-shot request through the same
//! managed harness boundary used by an interactive session; it never creates
//! a second, direct-core persistence path.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tea_core::agent::AgentConfiguration;
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::event::AgentEventKind;
use tea_core::harness::{
    HarnessActor, HarnessRepository, HarnessResolver, HarnessResourceLimits,
    HarnessSeedBuilder, ModelHarnessProfile, SelfExtensionMode, ToolPresentationDescriptor,
    SELF_EXTENSION_MODE_METADATA_KEY,
};
use tea_core::runtime::{
    HarnessIdentity, RuntimeServices, SessionRuntime, TeaEvent, TeaEventSubscription,
};
use tea_core::scheduler::ModelProvider;
use tea_core::state::{
    AgentMessage, AgentToolCall, MessageId, ModelDescriptor, SerializedJson, ThinkingLevel,
    ToolCallId, Usage,
};
use tea_core::tool::ToolExecutionMode;
use tea_luau::LuauExtensionEngine;
use tea_protocol::JsonValue;
use tea_session::{
    reduce_lane, CanonicalHashWriter, Digest, DurabilityMode, EntryId, HarnessRevisionChangedEntry,
    JsonlSession, LaneId, ModelChangedEntry, PayloadRef, ProvisionedEntry, SessionEntry,
    SessionHeader, SessionId, SessionSnapshot, SessionWriter, ThinkingChangedEntry,
    SESSION_FORMAT_VERSION,
};

use super::compaction::ProviderCompactor;
use super::error::AppError;
use super::support::{parse_thinking_level as parse_thinking_level_name, thinking_level_name};

/// Concrete durable supervisor used by the terminal application.
pub(super) type HostHarness = SessionRuntime<JsonlSession>;

/// Bounded metadata shown by the terminal session picker. The complete
/// durable snapshot remains behind the writer/reopen boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DurableSessionSummary {
    pub(super) id: String,
    pub(super) model: Option<ModelDescriptor>,
}

static NEXT_SESSION_NONCE: AtomicU64 = AtomicU64::new(1);
const HOST_SESSION_METADATA_VERSION: u64 = 1;
const MAX_HOST_SESSION_METADATA_BYTES: u64 = 65_536;

pub(super) struct HostHarnessConfig<'a> {
    pub(super) tea_home: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) configuration: AgentConfiguration,
    pub(super) model: ModelDescriptor,
    pub(super) provider: Arc<dyn ModelProvider>,
    pub(super) thinking_level: Option<ThinkingLevel>,
    pub(super) compactor: Option<Arc<ProviderCompactor>>,
    pub(super) automatic_compaction: AutomaticCompactionPolicy,
}

pub(super) struct HostHarnessReopen<'a> {
    pub(super) tea_home: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) session_id: &'a str,
    pub(super) configuration: AgentConfiguration,
    pub(super) model: ModelDescriptor,
    pub(super) provider: Arc<dyn ModelProvider>,
    pub(super) compactor: Option<Arc<ProviderCompactor>>,
    pub(super) automatic_compaction: AutomaticCompactionPolicy,
}

/// Create one fresh session-local managed harness under the host-selected Tea
/// home. The committed initial branch revision and immutable catalog exist
/// before the harness can begin an operation.
pub(super) fn create_host_harness(
    config: HostHarnessConfig<'_>,
) -> Result<Arc<HostHarness>, AppError> {
    let HostHarnessConfig {
        tea_home,
        workspace,
        configuration,
        model,
        provider,
        thinking_level,
        compactor,
        automatic_compaction,
    } = config;
    let sessions_root = tea_home.join("sessions");
    ensure_private_directory(tea_home)?;
    ensure_private_directory(&sessions_root)?;
    let workspace_key = workspace_key(workspace);
    let workspace_root = sessions_root.join(&workspace_key);
    ensure_private_directory(&workspace_root)?;
    let thinking_level = thinking_level.unwrap_or(ThinkingLevel::Off);

    let profile = model_profile(&model)?;
    let template = epoch_template(
        Arc::clone(&provider),
        configuration.clone(),
        model.clone(),
        thinking_level,
        compactor,
        automatic_compaction,
    );
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
        let seeded = HarnessSeedBuilder::new(
            Arc::clone(&artifacts),
            Arc::new(LuauExtensionEngine),
            host_profile_digest(&configuration),
            configuration.system_prompt.clone(),
            profile.clone(),
            SelfExtensionMode::Off,
            HarnessResourceLimits::default(),
            template.runtime_policy_identities(),
        )
        .trusted_tool_presentations(tool_presentations(&configuration))
        .seed(HarnessActor::Host, created_at_ms)
        .map_err(|error| AppError::Setup(error.to_string()))?;
        let repository = seeded.repository;
        let snapshot = seeded.snapshot;
        let revision = seeded.revision;
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
            HarnessResolver::new(repository, template, Default::default())
                .self_extension_mode(SelfExtensionMode::Off),
        );
        let identity = HarnessIdentity::new(revision.revision_id, snapshot.id, profile.profile_id);
        let harness =
            SessionRuntime::new_with_artifact_store(session, manager, identity, artifacts)?;
        if let Err(error) = write_host_session_metadata(&directory, &harness.snapshot()?) {
            eprintln!(
                "warning: durable session metadata cache was not written for {}: {error}",
                directory.display()
            );
        }
        return Ok(Arc::new(harness));
    }

    Err(AppError::Setup(
        "could not allocate a unique durable session directory".into(),
    ))
}

/// List durable sessions scoped to one explicit workspace. This deliberately
/// reads only each session's bounded derived metadata cache; opening a session
/// would acquire its sole writer lock and make an idle, already-open session
/// unlistable.
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
        let directory_id = name.trim_end_matches(".tea");
        SessionId::new(directory_id.to_owned())
            .map_err(|error| AppError::Setup(error.to_string()))?;
        let metadata = read_host_session_metadata(&path)?;
        let (id, model) = match metadata {
            Some(metadata)
                if metadata.session_id == directory_id
                    && metadata.workspace == workspace.to_string_lossy() =>
            {
                (metadata.session_id, metadata.model)
            }
            // `meta.json` is only a bounded discovery cache. A stale or
            // foreign cache must not alter the directory-derived session
            // identity or make listing fail; opening the session rebuilds it
            // from the authoritative v1 header.
            Some(_) => (directory_id.to_owned(), None),
            None => (directory_id.to_owned(), None),
        };
        sessions.push(DurableSessionSummary { id, model });
    }
    sessions.sort_by(|left, right| right.id.cmp(&left.id));
    Ok(sessions)
}

/// Read the model required by one named session from the validated durable
/// header. Session-picker metadata is intentionally not consulted here: it is
/// a disposable discovery cache and must not determine resume behavior.
pub(super) fn read_host_session_model(
    tea_home: &Path,
    workspace: &Path,
    session_id: &str,
) -> Result<ModelDescriptor, AppError> {
    let session_id = SessionId::new(session_id.to_owned())
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let directory =
        session_workspace_root(tea_home, workspace).join(format!("{}.tea", session_id.as_str()));
    let inspection = JsonlSession::inspect(&directory)?;
    let header = host_session_header_from_snapshot(&inspection.snapshot)?;
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
    header.model.ok_or_else(|| {
        AppError::Setup("durable session header is missing its immutable model identity".into())
    })
}

/// Reconstruct the terminal host's disposable caches from one validated v1
/// session prefix. The JSONL opener refreshes `HEAD`; `meta.json` is then
/// replaced from the same snapshot while the writer lock remains held.
pub(super) fn rebuild_host_session_metadata(
    directory: &Path,
) -> Result<(SessionSnapshot, Option<String>), AppError> {
    let session = JsonlSession::open(directory, DurabilityMode::Strict)?;
    let snapshot = session.snapshot()?;
    let head_cache_warning = session.cache_warning().map(str::to_owned);
    write_host_session_metadata(directory, &snapshot)?;
    Ok((snapshot, head_cache_warning))
}

/// Reopen one durable session with the exact model selected by its immutable
/// header. The supervisor reconstructs its active revision from the committed
/// catalog and semantic branch, then the caller may resume any open operation.
pub(super) fn reopen_host_harness(
    input: HostHarnessReopen<'_>,
) -> Result<Arc<HostHarness>, AppError> {
    let HostHarnessReopen {
        tea_home,
        workspace,
        session_id,
        configuration,
        model,
        provider,
        compactor,
        automatic_compaction,
    } = input;
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
    if stored_model != model {
        return Err(AppError::Setup(format!(
            "durable session {} requires model {}/{}; select that exact model before reopening",
            session_id, stored_model.provider, stored_model.model
        )));
    }
    if let Err(error) = write_host_session_metadata(session.directory(), &snapshot) {
        eprintln!(
            "warning: durable session metadata cache was not refreshed for {}: {error}",
            session.directory().display()
        );
    }
    let artifacts: Arc<dyn tea_session::ArtifactStore> =
        Arc::new(session.artifact_store().map_err(AppError::from)?);
    let reduction = reduce_lane(snapshot.clone(), LaneId::main())
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let thinking_level = reduction
        .effective_configuration
        .thinking_level
        .as_deref()
        .map(|value| {
            parse_thinking_level_name(value).ok_or_else(|| {
                AppError::Setup(format!(
                    "durable session has unsupported thinking level {value:?}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(header.thinking_level);
    let template = epoch_template(
        provider,
        configuration,
        model,
        thinking_level,
        compactor,
        automatic_compaction,
    );
    let manager = Arc::new(
        HarnessResolver::new(
            HarnessRepository::with_extension_engine(
                Arc::clone(&artifacts),
                Arc::new(LuauExtensionEngine),
            ),
            template,
            Default::default(),
        )
        .self_extension_mode(header.self_extension_mode),
    );
    let harness = SessionRuntime::reopen_with_artifact_store(session, manager, artifacts)?;
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
                usage: Box::new(Some(core_usage(&result.usage))),
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
) -> RuntimeServices {
    let mut template = RuntimeServices::from_agent_configuration(provider, configuration)
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
        "tea-terminal-host-v2",
        "tea-core-canonical-v1",
        "tea-provider-summary-v1",
        "tea-recoverable-projection-v1",
    )
    .map_err(|error| AppError::Setup(error.to_string()))
}

fn tool_presentations(configuration: &AgentConfiguration) -> Vec<ToolPresentationDescriptor> {
    configuration
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
            requires_exclusive_batch: tool.requires_exclusive_batch,
            cancellation_settlement_mode: match tool.cancellation_settlement_mode {
                tea_core::tool::CancellationSettlementMode::DropFuture => "drop_future".into(),
                tea_core::tool::CancellationSettlementMode::AwaitFuture => "await_future".into(),
            },
        })
        .collect()
}

fn host_profile_digest(configuration: &AgentConfiguration) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-agent-host-profile", 2, 1);
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
        writer.boolean(
            "tool_requires_exclusive_batch",
            tool.requires_exclusive_batch,
        );
        writer.string(
            "tool_cancellation_settlement_mode",
            match tool.cancellation_settlement_mode {
                tea_core::tool::CancellationSettlementMode::DropFuture => "drop_future",
                tea_core::tool::CancellationSettlementMode::AwaitFuture => "await_future",
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

#[derive(Clone, Debug)]
struct HostSessionMetadata {
    session_id: String,
    workspace: String,
    model: Option<ModelDescriptor>,
    header_digest: Digest,
    created_at_ms: u64,
    active_lane: LaneId,
    through_seq: u64,
    through_digest: Digest,
}

fn session_workspace_root(tea_home: &Path, workspace: &Path) -> PathBuf {
    tea_home.join("sessions").join(workspace_key(workspace))
}

fn read_host_session_metadata(directory: &Path) -> Result<Option<HostSessionMetadata>, AppError> {
    let path = directory.join("meta.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Setup(format!(
                "could not inspect {}: {error}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Setup(format!(
            "durable session metadata {} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_HOST_SESSION_METADATA_BYTES {
        return Ok(None);
    }
    let source = fs::read_to_string(&path)
        .map_err(|error| AppError::Setup(format!("could not read {}: {error}", path.display())))?;
    let value = match JsonValue::parse(&source) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let object = match value.as_object() {
        Some(object) if object.len() == 9 => object,
        _ => return Ok(None),
    };
    let version = object.get("version").and_then(JsonValue::as_u64);
    let session_id = object.get("session_id").and_then(JsonValue::as_str);
    let workspace = object.get("workspace").and_then(JsonValue::as_str);
    let model = object.get("model");
    let header_digest = object.get("header_digest").and_then(JsonValue::as_str);
    let created_at_ms = object.get("created_at_ms").and_then(JsonValue::as_u64);
    let active_lane = object.get("active_lane").and_then(JsonValue::as_str);
    let through_seq = object.get("through_seq").and_then(JsonValue::as_u64);
    let through_digest = object.get("through_digest").and_then(JsonValue::as_str);
    let (
        Some(HOST_SESSION_METADATA_VERSION),
        Some(session_id),
        Some(workspace),
        Some(model),
        Some(header_digest),
        Some(created_at_ms),
        Some(active_lane),
        Some(through_seq),
        Some(through_digest),
    ) = (
        version,
        session_id,
        workspace,
        model,
        header_digest,
        created_at_ms,
        active_lane,
        through_seq,
        through_digest,
    )
    else {
        return Ok(None);
    };
    if SessionId::new(session_id.to_owned()).is_err() {
        return Ok(None);
    }
    let model = match decode_cached_model(model) {
        Ok(model) => model,
        Err(()) => return Ok(None),
    };
    let Ok(header_digest) = Digest::from_hex(header_digest) else {
        return Ok(None);
    };
    let Ok(active_lane) = LaneId::new(active_lane.to_owned()) else {
        return Ok(None);
    };
    let Ok(through_digest) = Digest::from_hex(through_digest) else {
        return Ok(None);
    };
    Ok(Some(HostSessionMetadata {
        session_id: session_id.into(),
        workspace: workspace.into(),
        model,
        header_digest,
        created_at_ms,
        active_lane,
        through_seq,
        through_digest,
    }))
}

/// Compare the terminal host's bounded discovery cache to a validated v1
/// prefix. A missing, malformed, foreign, or stale cache is simply not
/// current: callers must continue to treat the JSONL prefix as authority.
pub(super) fn host_session_metadata_is_current(
    directory: &Path,
    snapshot: &SessionSnapshot,
) -> bool {
    let Ok(Some(metadata)) = read_host_session_metadata(directory) else {
        return false;
    };
    let Ok(header) = host_session_header_from_snapshot(snapshot) else {
        return false;
    };
    metadata.session_id == header.session_id
        && metadata.workspace == header.workspace
        && metadata.model == header.model
        && metadata.header_digest == snapshot.header().digest
        && metadata.created_at_ms == snapshot.header().created_at_ms
        && metadata.active_lane == snapshot.header().initial_lane
        && metadata.through_seq == snapshot.last_sequence().0
        && metadata.through_digest == snapshot.last_digest()
}

fn write_host_session_metadata(
    directory: &Path,
    snapshot: &SessionSnapshot,
) -> Result<(), AppError> {
    let header = host_session_header_from_snapshot(snapshot)?;
    let model = match &header.model {
        Some(model) => JsonValue::object([
            ("provider", JsonValue::String(model.provider.clone())),
            ("model", JsonValue::String(model.model.clone())),
            (
                "revision",
                model
                    .revision
                    .as_ref()
                    .map(|revision| JsonValue::String(revision.clone()))
                    .unwrap_or(JsonValue::Null),
            ),
        ]),
        None => JsonValue::Null,
    };
    let bytes = JsonValue::object([
        (
            "active_lane",
            JsonValue::String(snapshot.header().initial_lane.to_string()),
        ),
        (
            "created_at_ms",
            JsonValue::from(snapshot.header().created_at_ms),
        ),
        (
            "header_digest",
            JsonValue::String(snapshot.header().digest.to_hex()),
        ),
        ("version", JsonValue::from(HOST_SESSION_METADATA_VERSION)),
        ("session_id", JsonValue::String(header.session_id.clone())),
        (
            "through_digest",
            JsonValue::String(snapshot.last_digest().to_hex()),
        ),
        ("through_seq", JsonValue::from(snapshot.last_sequence().0)),
        ("workspace", JsonValue::String(header.workspace.clone())),
        ("model", model),
    ])
    .to_json_string()
    .map_err(|error| {
        AppError::Setup(format!("could not encode session metadata cache: {error}"))
    })?;
    let temporary = directory.join(format!(
        ".meta-{}-{:016x}.tmp",
        std::process::id(),
        NEXT_SESSION_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let destination = directory.join("meta.json");
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| {
            AppError::Setup(format!("could not create {}: {error}", temporary.display()))
        })?;
        file.write_all(bytes.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|error| {
                AppError::Setup(format!("could not write {}: {error}", temporary.display()))
            })?;
        drop(file);
        fs::rename(&temporary, &destination).map_err(|error| {
            AppError::Setup(format!(
                "could not publish {}: {error}",
                destination.display()
            ))
        })?;
        sync_host_directory(directory)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn decode_cached_model(value: &JsonValue) -> Result<Option<ModelDescriptor>, ()> {
    if matches!(value, JsonValue::Null) {
        return Ok(None);
    }
    let object = value.as_object().ok_or(())?;
    if object.len() != 3 {
        return Err(());
    }
    let provider = object
        .get("provider")
        .and_then(JsonValue::as_str)
        .ok_or(())?;
    let model = object.get("model").and_then(JsonValue::as_str).ok_or(())?;
    let revision = match object.get("revision") {
        Some(JsonValue::Null) => None,
        Some(value) => Some(value.as_str().ok_or(())?.to_owned()),
        None => return Err(()),
    };
    Ok(Some(ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        revision,
    }))
}

fn sync_host_directory(directory: &Path) -> Result<(), AppError> {
    let file = fs::File::open(directory).map_err(|error| {
        AppError::Setup(format!("could not open {}: {error}", directory.display()))
    })?;
    match file.sync_all() {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(AppError::Setup(format!(
            "could not synchronize {}: {error}",
            directory.display()
        ))),
    }
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

pub(super) fn core_usage(usage: &tea_session::Usage) -> Usage {
    Usage {
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
    use tea_core::coding::TeaCodingToolsV2;
    use tea_core::hooks::NoHooks;
    use tea_core::scheduler::{
        CancellationToken, ModelFuture, ModelRequest, ModelStream, ModelStreamEvent,
    };
    use tea_core::state::StopReason;
    use tea_core::tool::ToolRegistry;

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
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model,
            provider: Arc::new(StopProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
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
        let configuration =
            host_configuration(TeaCodingToolsV2::new(&workspace).expect("Tea v2 tools configure"))
                .expect("durable host configuration assembles");
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model: ModelDescriptor {
                provider: "openrouter".into(),
                model: "fixture-model".into(),
                revision: None,
            },
            provider: Arc::new(ContextCheckingProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
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
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration: configuration.clone(),
            model: model.clone(),
            provider: Arc::clone(&provider),
            thinking_level: Some(ThinkingLevel::High),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
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

        let reopened = reopen_host_harness(HostHarnessReopen {
            tea_home: &home,
            workspace: &workspace,
            session_id: &session_id,
            configuration,
            model,
            provider,
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
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

    #[test]
    fn session_listing_reads_derived_metadata_without_opening_the_log() {
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
            revision: None,
        };
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model: model.clone(),
            provider: Arc::new(StopProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
        .expect("host harness creates");
        let session_id = harness
            .snapshot()
            .expect("session snapshot")
            .header()
            .session_id
            .to_string();
        drop(harness);

        let directory = session_workspace_root(&home, &workspace).join(format!("{session_id}.tea"));
        fs::rename(
            directory.join("session.jsonl"),
            directory.join("session.jsonl.hidden"),
        )
        .expect("authoritative log is hidden from the listing test");
        fs::create_dir(
            session_workspace_root(&home, &workspace).join(".interrupted.create-fixture"),
        )
        .expect("unpublished creation directory creates");

        let listed = list_host_sessions(&home, &workspace).expect("listing reads meta.json");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session_id);
        assert_eq!(listed[0].model, Some(model));
        let _ = fs::remove_dir_all(home);
    }

    /// Reproducible session-picker workload that contains no `session.jsonl`
    /// files. A listing regression that tries to replay every session will
    /// therefore fail rather than silently measuring a different path.
    #[test]
    #[ignore = "run explicitly to measure generated bounded-metadata session listing"]
    fn generated_session_listing_fixture_measures_bounded_metadata_reads() {
        const SESSION_COUNT: usize = 1_000;
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let root = session_workspace_root(&home, &workspace);
        fs::create_dir_all(&root).expect("session root creates");
        let workspace_json = workspace.to_string_lossy();

        for index in 0..SESSION_COUNT {
            let session_id = format!("generated-list-{index:04}");
            let directory = root.join(format!("{session_id}.tea"));
            fs::create_dir(&directory).expect("generated session directory creates");
            match index % 4 {
                0 => fs::write(
                    directory.join("meta.json"),
                    format!(
                        r#"{{"active_lane":"main","created_at_ms":0,"header_digest":"{}","model":{{"model":"fixture-model","provider":"fixture","revision":null}},"session_id":"{session_id}","through_digest":"{}","through_seq":0,"version":{},"workspace":"{}"}}"#,
                        "0".repeat(64),
                        "0".repeat(64),
                        HOST_SESSION_METADATA_VERSION,
                        workspace_json,
                    ),
                )
                .expect("valid generated metadata writes"),
                1 => {}
                2 => fs::write(
                    directory.join("meta.json"),
                    format!(
                        r#"{{"active_lane":"main","created_at_ms":0,"header_digest":"{}","model":null,"session_id":"{session_id}","through_digest":"{}","through_seq":0,"version":{},"workspace":"other-workspace"}}"#,
                        "0".repeat(64),
                        "0".repeat(64),
                        HOST_SESSION_METADATA_VERSION,
                    ),
                )
                .expect("stale generated metadata writes"),
                _ => fs::write(directory.join("meta.json"), "not JSON")
                    .expect("malformed generated metadata writes"),
            }
        }

        let started = std::time::Instant::now();
        let listed = list_host_sessions(&home, &workspace).expect("listing succeeds without logs");
        let elapsed = started.elapsed();
        assert_eq!(listed.len(), SESSION_COUNT);
        assert_eq!(
            listed
                .iter()
                .filter(|summary| summary.model.is_some())
                .count(),
            SESSION_COUNT / 4,
            "only valid, matching metadata supplies the optional picker model"
        );
        eprintln!(
            "generated-session-listing sessions={SESSION_COUNT} valid_metadata={} listing_ms={}",
            SESSION_COUNT / 4,
            elapsed.as_millis()
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn host_metadata_names_the_committed_prefix_it_describes() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration: AgentConfiguration::new(
                "trusted system prompt",
                ToolRegistry::default(),
                Arc::new(NoHooks),
            ),
            model: ModelDescriptor {
                provider: "fixture".into(),
                model: "fixture-model".into(),
                revision: None,
            },
            provider: Arc::new(StopProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
        .expect("host harness creates");
        let snapshot = harness.snapshot().expect("session snapshot");
        let directory = session_workspace_root(&home, &workspace)
            .join(format!("{}.tea", snapshot.header().session_id));
        let metadata = JsonValue::parse(
            &fs::read_to_string(directory.join("meta.json")).expect("metadata cache reads"),
        )
        .expect("metadata cache is JSON");
        let fields = metadata.as_object().expect("metadata cache is an object");

        assert_eq!(
            fields.get("through_seq").and_then(JsonValue::as_u64),
            Some(snapshot.last_sequence().0)
        );
        assert_eq!(
            fields.get("through_digest").and_then(JsonValue::as_str),
            Some(snapshot.last_digest().to_hex().as_str())
        );
        drop(harness);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn rebuild_host_session_metadata_replays_the_named_authoritative_prefix() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration: AgentConfiguration::new(
                "trusted system prompt",
                ToolRegistry::default(),
                Arc::new(NoHooks),
            ),
            model: ModelDescriptor {
                provider: "fixture".into(),
                model: "fixture-model".into(),
                revision: None,
            },
            provider: Arc::new(StopProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
        .expect("host harness creates");
        let expected = harness.snapshot().expect("session snapshot");
        let directory = session_workspace_root(&home, &workspace)
            .join(format!("{}.tea", expected.header().session_id));
        drop(harness);
        fs::write(directory.join("meta.json"), "stale cache").expect("stale metadata writes");
        fs::write(directory.join("HEAD"), "stale cache").expect("stale HEAD writes");

        let (rebuilt, cache_warning) =
            rebuild_host_session_metadata(&directory).expect("derived caches rebuild");
        assert_eq!(cache_warning, None);
        assert_eq!(rebuilt.last_sequence(), expected.last_sequence());
        assert_eq!(rebuilt.last_digest(), expected.last_digest());
        let metadata = JsonValue::parse(
            &fs::read_to_string(directory.join("meta.json")).expect("metadata cache reads"),
        )
        .expect("metadata cache is JSON");
        let fields = metadata.as_object().expect("metadata cache is an object");
        assert_eq!(
            fields.get("through_seq").and_then(JsonValue::as_u64),
            Some(expected.last_sequence().0)
        );
        assert_eq!(
            fields.get("through_digest").and_then(JsonValue::as_str),
            Some(expected.last_digest().to_hex().as_str())
        );
        assert!(
            JsonValue::parse(&fs::read_to_string(directory.join("HEAD")).expect("HEAD reads"))
                .is_ok(),
            "the same validated replay replaces the disposable active-head cache"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn session_listing_ignores_foreign_metadata_identity() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let configuration = AgentConfiguration::new(
            "trusted system prompt",
            ToolRegistry::default(),
            Arc::new(NoHooks),
        );
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model: ModelDescriptor {
                provider: "fixture".into(),
                model: "fixture-model".into(),
                revision: None,
            },
            provider: Arc::new(StopProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
        .expect("host harness creates");
        let session_id = harness
            .snapshot()
            .expect("session snapshot")
            .header()
            .session_id
            .to_string();
        drop(harness);

        let directory = session_workspace_root(&home, &workspace).join(format!("{session_id}.tea"));
        fs::write(
            directory.join("meta.json"),
            format!(
                r#"{{"active_lane":"main","created_at_ms":0,"header_digest":"{}","model":null,"session_id":"foreign-session","through_digest":"{}","through_seq":0,"version":{},"workspace":"{}"}}"#,
                "0".repeat(64),
                "0".repeat(64),
                HOST_SESSION_METADATA_VERSION,
                workspace.display(),
            ),
        )
        .expect("foreign metadata cache writes");

        let listed = list_host_sessions(&home, &workspace).expect("listing succeeds");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session_id);
        assert_eq!(listed[0].model, None);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn session_listing_treats_corrupt_or_unknown_metadata_as_a_disposable_cache() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let model = ModelDescriptor {
            provider: "fixture".into(),
            model: "fixture-model".into(),
            revision: None,
        };
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration: AgentConfiguration::new(
                "trusted system prompt",
                ToolRegistry::default(),
                Arc::new(NoHooks),
            ),
            model,
            provider: Arc::new(StopProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
        })
        .expect("host harness creates");
        let session_id = harness
            .snapshot()
            .expect("session snapshot")
            .header()
            .session_id
            .to_string();
        drop(harness);

        let directory = session_workspace_root(&home, &workspace).join(format!("{session_id}.tea"));
        let metadata_path = directory.join("meta.json");
        let valid_metadata = fs::read_to_string(&metadata_path).expect("metadata cache reads");
        fs::rename(
            directory.join("session.jsonl"),
            directory.join("session.jsonl.hidden"),
        )
        .expect("authoritative log is hidden from metadata corruption cases");

        let foreign_identity = valid_metadata.replace(
            &format!(r#""session_id":"{session_id}""#),
            r#""session_id":"foreign-session""#,
        );
        let future_schema = valid_metadata.replace(
            &format!(r#""version":{HOST_SESSION_METADATA_VERSION}"#),
            &format!(r#""version":{}"#, HOST_SESSION_METADATA_VERSION + 1),
        );
        let cases = [
            ("missing", None),
            ("empty", Some(String::new())),
            ("truncated", Some("{\"version\":".into())),
            ("foreign identity", Some(foreign_identity)),
            ("future schema", Some(future_schema)),
        ];

        for (name, metadata) in cases {
            match metadata {
                Some(metadata) => fs::write(&metadata_path, metadata)
                    .unwrap_or_else(|error| panic!("{name} metadata cache writes: {error}")),
                None => match fs::remove_file(&metadata_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => panic!("{name} metadata cache removes: {error}"),
                },
            }

            let listed = list_host_sessions(&home, &workspace)
                .unwrap_or_else(|error| panic!("{name} metadata lists: {error}"));
            assert_eq!(
                listed.len(),
                1,
                "{name} metadata preserves the directory entry"
            );
            assert_eq!(
                listed[0].id, session_id,
                "{name} metadata cannot rename a session"
            );
            assert_eq!(
                listed[0].model, None,
                "{name} metadata cannot supply a trusted picker model"
            );
        }
        let _ = fs::remove_dir_all(home);
    }
}
