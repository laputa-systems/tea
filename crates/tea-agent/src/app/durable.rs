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
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::agent::AgentConfiguration;
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::event::AgentEventKind;
use tea_core::harness::extension::{
    ExtensionEngine, ExtensionHostCommandDescription, ExtensionLimits, ExtensionStateHandle,
    ExtensionToolLimits,
};
use tea_core::harness::{
    CapabilityBindingRef, ExtensionStateCapability, HarnessActor, HarnessRepository,
    HarnessResolver, HarnessResourceLimits, HarnessSeedBuilder, HarnessSeedExtension,
    HarnessSeedExtensionScope, ModelHarnessProfile, PluginCapabilityBinding,
    PluginCapabilityCatalog, SelfExtensionMode, ToolPresentationDescriptor,
    SELF_EXTENSION_MODE_METADATA_KEY,
};
use tea_core::runtime::{
    HarnessIdentity, RuntimeServices, SessionSupervisor, SessionSupervisorInput,
    SessionSupervisorReopenInput, SubagentPolicy, SubagentServices, TeaEvent, TeaEventSubscription,
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
    reduce_agent_graph, reduce_lane, CanonicalHashWriter, Digest, DurabilityMode, EntryId,
    HarnessRevisionChangedEntry, JsonlSession, LaneId, ModelChangedEntry, PayloadRef,
    ProvisionedEntry, SessionEntry, SessionFact, SessionHeader, SessionId, SessionSnapshot,
    SessionWriter, SubagentModelRecord, SubagentPolicyFact, ThinkingChangedEntry,
    SESSION_FORMAT_VERSION,
};

use super::compaction::ProviderCompactor;
use super::config::{SubagentTuiConfig, TuiConfig};
use super::error::AppError;
use super::super::build_info;
use super::provider_factory::ProviderFactory;
use super::subagents::{SmolTaskRuntime, TuiSubagentHost};
use super::support::{parse_thinking_level as parse_thinking_level_name, thinking_level_name};

/// Concrete durable supervisor used by the terminal application.
pub(super) type HostHarness = SessionSupervisor<JsonlSession>;

/// Describe bundled host commands before a lazy session exists. This uses the
/// same immutable source tree and extension engine later pinned in the
/// durable harness; it does not create a shadow command implementation.
pub(super) fn bundled_host_commands() -> Result<Vec<ExtensionHostCommandDescription>, AppError> {
    LuauExtensionEngine
        .describe(&tea_luau::builtins::goal(extension_limits(
            &HarnessResourceLimits::default(),
        )))
        .map(|descriptor| descriptor.host_commands)
        .map_err(|error| AppError::Setup(error.to_string()))
}

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
/// Existing host policy: permit exactly one automatic immutable activation per operation.
const HOST_HARNESS_ROLLOVER_BUDGET: u32 = 1;

pub(super) struct HostHarnessConfig<'a> {
    pub(super) tea_home: &'a Path,
    pub(super) workspace: &'a Path,
    pub(super) configuration: AgentConfiguration,
    pub(super) model: ModelDescriptor,
    pub(super) provider: Arc<dyn ModelProvider>,
    pub(super) thinking_level: Option<ThinkingLevel>,
    pub(super) compactor: Option<Arc<ProviderCompactor>>,
    pub(super) automatic_compaction: AutomaticCompactionPolicy,
    /// Terminal-only optional child authority. `None` leaves root prompt,
    /// tools, catalog and executable services byte-identical to the legacy
    /// feature-disabled path.
    pub(super) subagents: Option<HostSubagentConfig>,
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
    pub(super) subagents: Option<HostSubagentConfig>,
}

/// Global terminal authorization retained only while an enabled TUI session
/// is being assembled. The effective `SubagentPolicy` is derived and then
/// persisted before its root collaboration schema becomes active.
#[derive(Clone)]
pub(super) struct HostSubagentConfig {
    pub(super) factory: Arc<ProviderFactory>,
    pub(super) config: SubagentTuiConfig,
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
        subagents,
    } = config;
    let sessions_root = tea_home.join("sessions");
    ensure_private_directory(tea_home)?;
    ensure_private_directory(&sessions_root)?;
    let workspace_key = workspace_key(workspace);
    let workspace_root = sessions_root.join(&workspace_key);
    ensure_private_directory(&workspace_root)?;
    let thinking_level = thinking_level.unwrap_or(ThinkingLevel::Off);
    let subagent_policy = subagents
        .as_ref()
        .map(|subagents| {
            subagents
                .factory
                .resolve_subagent_policy(&model, &subagents.config)
        })
        .transpose()?;

    let profile = model_profile(&model)?;
    let template = epoch_template(
        Arc::clone(&provider),
        configuration.clone(),
        model.clone(),
        thinking_level,
        compactor,
        automatic_compaction,
    );
    let child_configuration = if subagent_policy.is_some() {
        let tools = tea_core::coding::TeaCodingToolsV2::with_operations(
            workspace,
            Arc::new(super::nonblocking_operations::NonblockingCodingOperations),
        )
        .map_err(|error| AppError::Setup(format!("invalid child workspace template: {error}")))?;
        Some(super::host::host_configuration(
            tools,
            &workspace.to_string_lossy(),
        )?)
    } else {
        None
    };
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
        metadata.insert(
            build_info::SESSION_VERSION_METADATA_KEY.into(),
            JsonValue::String(build_info::PACKAGE_VERSION.into()),
        );
        metadata.insert(
            build_info::SESSION_GIT_SHA_METADATA_KEY.into(),
            JsonValue::String(build_info::GIT_SHA.into()),
        );
        let header = SessionHeader::new(
            session_id.clone(),
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
        let resource_limits = HarnessResourceLimits::default();
        let (state_handle, goal_binding) = goal_state_binding()?;
        let goal_binding_ref = CapabilityBindingRef {
            plugin_id: goal_binding.plugin_id().to_owned(),
            capability: goal_binding.capability().to_owned(),
            capability_version: goal_binding.capability_version().to_owned(),
            binding_digest: goal_binding.binding_digest(),
        };
        let mut capability_catalog = PluginCapabilityCatalog::new();
        capability_catalog
            .insert(goal_binding)
            .map_err(|error| AppError::Setup(error.to_string()))?;
        let (root_prompt, root_presentations) =
            root_harness_surface(&configuration, subagent_policy.as_ref())?;
        let seeded = HarnessSeedBuilder::new(
            Arc::clone(&artifacts),
            Arc::new(LuauExtensionEngine),
            host_profile_digest(&configuration),
            root_prompt,
            profile.clone(),
            SelfExtensionMode::Off,
            resource_limits.clone(),
            template.runtime_policy_identities(),
        )
        .extensions(vec![HarnessSeedExtension {
            scope: HarnessSeedExtensionScope::Global,
            source: tea_luau::builtins::goal(extension_limits(&resource_limits)),
        }])
        .capability_bindings(vec![goal_binding_ref.clone()])
        .trusted_tool_presentations(root_presentations)
        .seed(HarnessActor::Host, created_at_ms)
        .map_err(|error| AppError::Setup(error.to_string()))?;
        let mut repository = seeded.repository;
        let snapshot = seeded.snapshot;
        let revision = seeded.revision;
        let child_harnesses = match (&subagents, &subagent_policy, &child_configuration) {
            (Some(_subagents), Some(policy), Some(configuration)) => seed_child_harnesses(
                &mut repository,
                Arc::clone(&artifacts),
                Arc::clone(&provider),
                configuration,
                policy,
                &resource_limits,
                &goal_binding_ref,
                created_at_ms,
            )?,
            (None, None, None) => Vec::new(),
            _ => {
                return Err(AppError::Setup(
                    "subagent host setup is internally inconsistent".into(),
                ))
            }
        };
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
        if let Some(policy) = &subagent_policy {
            session.append_fact(SessionFact::SubagentPolicy(subagent_policy_fact(policy)?))?;
        }
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
            HarnessResolver::new(repository, Default::default())
                .capability_catalog(capability_catalog)
                .reserved_extension_command_names(super::commands::names())
                .self_extension_mode(SelfExtensionMode::Off),
        );
        let identity = HarnessIdentity::new(revision.revision_id, snapshot.id, profile.profile_id);
        let subagent_services = match (&subagents, &subagent_policy) {
            (Some(subagents), Some(policy)) => {
                let host: Arc<dyn tea_core::runtime::SubagentHost> =
                    Arc::new(TuiSubagentHost::new(
                        workspace.to_path_buf(),
                        session.directory().to_path_buf(),
                        session_id.clone(),
                        workspace.to_string_lossy().into_owned(),
                        Arc::clone(&subagents.factory),
                        Arc::clone(&artifacts),
                        child_harnesses,
                    ));
                let tasks: Arc<dyn tea_core::runtime::TaskRuntime> =
                    Arc::new(SmolTaskRuntime::new());
                Some(SubagentServices {
                    policy: policy.clone(),
                    host,
                    tasks,
                })
            }
            (None, None) => None,
            _ => {
                return Err(AppError::Setup(
                    "subagent services are internally inconsistent".into(),
                ))
            }
        };
        let harness = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: template,
            artifacts,
            rollover_budget: HOST_HARNESS_ROLLOVER_BUDGET,
            subagents: subagent_services,
        })?;
        let state_store: Arc<dyn tea_core::harness::extension::ExtensionStateStore> =
            Arc::clone(&harness) as Arc<dyn tea_core::harness::extension::ExtensionStateStore>;
        state_handle
            .attach(state_store)
            .map_err(|error| AppError::Setup(error.to_string()))?;
        if let Err(error) = write_host_session_metadata(&directory, &harness.snapshot()?) {
            eprintln!(
                "warning: durable session metadata cache was not written for {}: {error}",
                directory.display()
            );
        }
        return Ok(harness);
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

/// Validate whether the current terminal configuration may execute a durable
/// session before selecting its provider or replacing an already-open writer.
///
/// This inspection is deliberately read-only: a persisted child policy is an
/// authorization boundary, so rejecting it must not load credentials, create
/// an adapter factory, or discard the application's current idle harness.
pub(super) fn authorize_host_session_reopen(
    tea_home: &Path,
    workspace: &Path,
    session_id: &str,
    config: &TuiConfig,
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
    let graph = reduce_agent_graph(&inspection.snapshot)
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let current_policy = config.features.subagents.then_some(&config.subagents);
    reopen_subagent_policy(graph.policy.as_ref(), current_policy)?;
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
        subagents,
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
    let agent_graph =
        reduce_agent_graph(&snapshot).map_err(|error| AppError::Setup(error.to_string()))?;
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
        Arc::clone(&provider),
        configuration,
        model,
        thinking_level,
        compactor,
        automatic_compaction,
    );
    let (state_handle, goal_binding) = goal_state_binding()?;
    let goal_binding_ref = CapabilityBindingRef {
        plugin_id: goal_binding.plugin_id().to_owned(),
        capability: goal_binding.capability().to_owned(),
        capability_version: goal_binding.capability_version().to_owned(),
        binding_digest: goal_binding.binding_digest(),
    };
    let mut capability_catalog = PluginCapabilityCatalog::new();
    capability_catalog
        .insert(goal_binding)
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let persisted_subagent_policy = reopen_subagent_policy(
        agent_graph.policy.as_ref(),
        subagents.as_ref().map(|subagents| &subagents.config),
    )?;
    let subagent_services = match (persisted_subagent_policy, subagents.as_ref()) {
        // A later global enablement must not rewrite the immutable surface of
        // a session that was created without optional child services.
        (None, _) => None,
        (Some(policy), Some(subagents)) => {
            let tools = tea_core::coding::TeaCodingToolsV2::with_operations(
                workspace,
                Arc::new(super::nonblocking_operations::NonblockingCodingOperations),
            )
            .map_err(|error| {
                AppError::Setup(format!("invalid child workspace template: {error}"))
            })?;
            // Keep model-facing context anchored to the durable, original
            // workspace spelling. Only child tool execution gets a physical
            // lease worktree later in `TuiSubagentHost::prepared`.
            let child_configuration = super::host::host_configuration(tools, &header.workspace)?;
            let resource_limits = HarnessResourceLimits::default();
            let child_harnesses = derive_child_harnesses(
                Arc::clone(&artifacts),
                Arc::clone(&provider),
                &child_configuration,
                &policy,
                &resource_limits,
                &goal_binding_ref,
                snapshot.header().created_at_ms,
            )?;
            let host: Arc<dyn tea_core::runtime::SubagentHost> = Arc::new(TuiSubagentHost::new(
                workspace.to_path_buf(),
                session.directory().to_path_buf(),
                session_id.clone(),
                header.workspace.clone(),
                Arc::clone(&subagents.factory),
                Arc::clone(&artifacts),
                child_harnesses,
            ));
            let tasks: Arc<dyn tea_core::runtime::TaskRuntime> = Arc::new(SmolTaskRuntime::new());
            Some(SubagentServices {
                policy,
                host,
                tasks,
            })
        }
        (Some(_), None) => unreachable!("validated enabled session needs a host config"),
    };
    let manager = Arc::new(
        HarnessResolver::new(
            HarnessRepository::with_extension_engine(
                Arc::clone(&artifacts),
                Arc::new(LuauExtensionEngine),
            ),
            Default::default(),
        )
        .capability_catalog(capability_catalog)
        .reserved_extension_command_names(super::commands::names())
        .self_extension_mode(header.self_extension_mode),
    );
    let harness = SessionSupervisor::reopen(SessionSupervisorReopenInput {
        session,
        resolver: manager,
        root_services: template,
        lane_services: BTreeMap::new(),
        artifacts,
        rollover_budget: HOST_HARNESS_ROLLOVER_BUDGET,
        subagents: subagent_services,
    })?;
    let state_store: Arc<dyn tea_core::harness::extension::ExtensionStateStore> =
        Arc::clone(&harness) as Arc<dyn tea_core::harness::extension::ExtensionStateStore>;
    state_handle
        .attach(state_store)
        .map_err(|error| AppError::Setup(error.to_string()))?;
    harness.verify_durable_state()?;
    Ok(harness)
}

fn extension_limits(resource_limits: &HarnessResourceLimits) -> ExtensionLimits {
    ExtensionLimits {
        max_source_bytes: resource_limits.source_bytes,
        max_memory_bytes: resource_limits.memory_bytes,
        max_interrupt_checks: resource_limits.instruction_checks as usize,
    }
}

fn goal_state_binding() -> Result<(ExtensionStateHandle, PluginCapabilityBinding), AppError> {
    let state = ExtensionStateHandle::new();
    let limits = ExtensionToolLimits::default();
    let capability = ExtensionStateCapability::new("goal", state.clone())
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let binding = PluginCapabilityBinding::new(
        "goal",
        "extension.state",
        "v1",
        Digest::from_bytes("tea-extension-state-capability-v1"),
        limits,
        Arc::new(capability),
    )
    .map_err(|error| AppError::Setup(error.to_string()))?;
    Ok((state, binding))
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
    let stdout = io::stdout();
    let mut output = stdout.lock();
    stream_host_prompt_to(harness, subscription, prompt, &mut output).await
}

async fn stream_host_prompt_to<W: Write>(
    harness: Arc<HostHarness>,
    subscription: TeaEventSubscription,
    prompt: String,
    output: &mut W,
) -> Result<(), AppError> {
    let mut drive = Box::pin(harness.run_root_prompt(prompt));
    loop {
        if let Err(output_error) = drain_prompt_events_to(&subscription, output) {
            if let Err(cleanup_error) =
                settle_prompt_after_output_failure(&harness, &mut drive).await
            {
                return Err(AppError::Setup(format!(
                    "{output_error}; durable root cleanup requires recovery: {cleanup_error}"
                )));
            }
            return Err(output_error);
        }
        if let Some(result) = smol::future::poll_once(&mut drive).await {
            let output_result = drain_prompt_events_to(&subscription, output);
            let root_result = result.map(|_| ()).map_err(AppError::from);
            if let Err(cleanup_error) = require_root_settled(&harness) {
                let completed_diagnostic = match (&output_result, &root_result) {
                    (Err(output_error), Err(root_error)) => {
                        format!("{output_error}; root drive failed: {root_error}")
                    }
                    (Err(output_error), Ok(())) => output_error.to_string(),
                    (Ok(()), Err(root_error)) => root_error.to_string(),
                    (Ok(()), Ok(())) => "root driver returned".into(),
                };
                return Err(AppError::Setup(format!(
                    "{completed_diagnostic}; durable root cleanup requires recovery: {cleanup_error}"
                )));
            }
            output_result?;
            root_result?;
            output
                .write_all(b"\n")
                .map_err(|error| AppError::Setup(format!("could not write response: {error}")))?;
            output
                .flush()
                .map_err(|error| AppError::Setup(format!("could not flush response: {error}")))?;
            return Ok(());
        }
        smol::future::yield_now().await;
    }
}

async fn settle_prompt_after_output_failure(
    harness: &Arc<HostHarness>,
    drive: &mut std::pin::Pin<
        Box<
            impl std::future::Future<
                Output = Result<
                    tea_core::runtime::DurableOperation,
                    tea_core::harness::HarnessError,
                >,
            >,
        >,
    >,
) -> Result<(), tea_core::harness::HarnessError> {
    loop {
        let _ = harness.abort_root();
        if smol::future::poll_once(&mut *drive).await.is_some() {
            return require_root_settled(harness);
        }
        smol::future::yield_now().await;
    }
}

pub(super) fn require_root_settled(
    harness: &HostHarness,
) -> Result<(), tea_core::harness::HarnessError> {
    let reduction = reduce_lane(harness.snapshot()?, LaneId::main())?;
    if reduction.lane_state.active_operation.is_none() {
        return Ok(());
    }
    let plan = reduction.recovery_plan.ok_or_else(|| {
        tea_core::harness::HarnessError::invalid_state(
            "root driver returned while its durable operation remained open without recovery",
        )
    })?;
    Err(tea_core::harness::HarnessError::RecoveryRequired { plan })
}

fn drain_prompt_events_to<W: Write>(
    subscription: &TeaEventSubscription,
    output: &mut W,
) -> Result<(), AppError> {
    let mut wrote = false;
    while let Ok(event) = subscription.try_recv() {
        if let TeaEvent::Agent { lane_id, event } = event {
            if lane_id != LaneId::main() {
                continue;
            }
            if let AgentEventKind::MessageUpdate {
                text_delta: Some(text),
                ..
            } = event.kind
            {
                output.write_all(text.as_bytes()).map_err(|error| {
                    AppError::Setup(format!("could not write response: {error}"))
                })?;
                wrote = true;
            }
        }
    }
    if wrote {
        output
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

/// Construct the exact immutable root collaboration surface without changing
/// the feature-disabled host bytes. Live root collaboration tools are added
/// by the core supervisor only after this matching policy fact is durable.
fn root_harness_surface(
    configuration: &AgentConfiguration,
    policy: Option<&SubagentPolicy>,
) -> Result<(String, Vec<ToolPresentationDescriptor>), AppError> {
    let mut prompt = configuration.system_prompt.clone();
    let mut definitions = configuration.tools.definitions();
    tea_core::runtime::append_root_subagent_surface(&mut prompt, &mut definitions, policy)
        .map_err(|error| AppError::Setup(error.to_string()))?;
    let mut presentations = tool_presentations(configuration);
    if let Some(policy) = policy {
        presentations.extend(
            tea_core::runtime::root_subagent_tool_presentations(policy)
                .map_err(|error| AppError::Setup(error.to_string()))?,
        );
    }
    debug_assert_eq!(definitions.len(), presentations.len());
    Ok((prompt, presentations))
}

/// Encode the core-neutral policy in the session's v1 durable spelling.
fn subagent_policy_fact(policy: &SubagentPolicy) -> Result<SubagentPolicyFact, AppError> {
    policy
        .validate()
        .map_err(|error| AppError::Setup(format!("invalid subagent policy: {error}")))?;
    Ok(SubagentPolicyFact {
        schema_version: 1,
        models: policy
            .models
            .iter()
            .map(|model| SubagentModelRecord {
                provider: model.descriptor.provider.clone(),
                model: model.descriptor.model.clone(),
                revision: model.descriptor.revision.clone(),
                display_name: model.display_name.clone(),
                context_window: model.context_window.map(NonZeroU64::get),
            })
            .collect(),
        max_concurrent: policy.max_concurrent.get(),
        max_total_per_operation: policy.max_total_per_operation.get(),
        timeout_ms: policy.timeout.as_millis().min(u128::from(u64::MAX)) as u64,
        tool_surface_digest: tea_core::runtime::root_subagent_tool_surface_digest(policy)
            .map_err(|error| AppError::Setup(error.to_string()))?,
    })
}

/// Decode the durable fact before installing any process-local host state.
/// It deliberately does not re-resolve the current provider registry: the
/// session catalog, rather than changed global configuration, authorizes a
/// reopened session's previously committed child model domain.
fn subagent_policy_from_fact(fact: &SubagentPolicyFact) -> Result<SubagentPolicy, AppError> {
    fact.validate()
        .map_err(|error| AppError::Setup(format!("invalid durable subagent policy: {error}")))?;
    let models = fact
        .models
        .iter()
        .map(|model| {
            Ok(tea_core::runtime::SubagentModel {
                descriptor: ModelDescriptor {
                    provider: model.provider.clone(),
                    model: model.model.clone(),
                    revision: model.revision.clone(),
                },
                display_name: model.display_name.clone(),
                context_window: match model.context_window {
                    Some(value) => Some(NonZeroU64::new(value).ok_or_else(|| {
                        AppError::Setup("durable subagent context window must be nonzero".into())
                    })?),
                    None => None,
                },
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    let max_concurrent = NonZeroU32::new(fact.max_concurrent).ok_or_else(|| {
        AppError::Setup("durable subagent concurrent limit must be nonzero".into())
    })?;
    let max_total_per_operation = NonZeroU32::new(fact.max_total_per_operation)
        .ok_or_else(|| AppError::Setup("durable subagent total limit must be nonzero".into()))?;
    let policy = SubagentPolicy {
        models,
        max_concurrent,
        max_total_per_operation,
        timeout: Duration::from_millis(fact.timeout_ms),
    };
    policy
        .validate()
        .map_err(|error| AppError::Setup(format!("invalid durable subagent policy: {error}")))?;
    Ok(policy)
}

/// Current terminal configuration may restrict access to a reopened catalog,
/// but it must never silently expand or replace the session's committed one.
fn authorize_reopened_subagent_policy(
    config: &SubagentTuiConfig,
    policy: &SubagentPolicy,
) -> Result<(), AppError> {
    let provider = &policy
        .models
        .first()
        .ok_or_else(|| AppError::Setup("durable subagent policy has no models".into()))?
        .descriptor
        .provider;
    if let Some(configured) = &config.provider {
        if configured != provider {
            return Err(AppError::Setup(format!(
                "durable subagent session requires provider {provider:?}; current TUI policy selects {configured:?}"
            )));
        }
    }
    if let Some(allowed_models) = &config.models {
        for model in &policy.models {
            if !allowed_models
                .iter()
                .any(|allowed| allowed == &model.descriptor.model)
            {
                return Err(AppError::Setup(format!(
                    "durable subagent session requires model {:?}, absent from current TUI policy",
                    model.descriptor.model
                )));
            }
        }
    }
    Ok(())
}

/// Select the immutable child policy for reopen. A current enabled config is
/// an authorization ceiling only; a current disabled config cannot retrofit
/// an old disabled session or execute an enabled durable one.
fn reopen_subagent_policy(
    fact: Option<&SubagentPolicyFact>,
    config: Option<&SubagentTuiConfig>,
) -> Result<Option<SubagentPolicy>, AppError> {
    let Some(fact) = fact else {
        return Ok(None);
    };
    let config = config.ok_or_else(|| {
        AppError::Setup("durable subagent session requires features.subagents = true".into())
    })?;
    let policy = subagent_policy_from_fact(fact)?;
    authorize_reopened_subagent_policy(config, &policy)?;
    Ok(Some(policy))
}

/// One child snapshot/revision pair before it is inserted into the root
/// resolver catalog. Building it independently gives reopen a deterministic
/// catalog derivation for every permitted model, including models never
/// previously spawned in this process.
fn child_harness_seeds(
    artifacts: Arc<dyn tea_session::ArtifactStore>,
    provider: Arc<dyn ModelProvider>,
    configuration: &AgentConfiguration,
    policy: &SubagentPolicy,
    resource_limits: &HarnessResourceLimits,
    goal_binding_ref: &CapabilityBindingRef,
    created_at_ms: u64,
) -> Result<Vec<(ModelDescriptor, tea_core::harness::SeededHarness)>, AppError> {
    policy
        .models
        .iter()
        .map(|model| {
            let mut prompt = configuration.system_prompt.clone();
            tea_core::runtime::append_child_subagent_instruction_suffix(&mut prompt);
            let automatic_compaction = model
                .context_window
                .map(super::picker::automatic_compaction_policy)
                .unwrap_or_else(AutomaticCompactionPolicy::disabled);
            // A child thinking level is a live lane choice resolved by core
            // at prepare/reopen time, not an immutable snapshot identity.
            let child_template = RuntimeServices::from_agent_configuration(
                Arc::clone(&provider),
                configuration.clone(),
            )
            .model(model.descriptor.clone())
            .automatic_compaction(automatic_compaction);
            let profile = model_profile(&model.descriptor)?;
            let seeded = HarnessSeedBuilder::new(
                Arc::clone(&artifacts),
                Arc::new(LuauExtensionEngine),
                host_profile_digest(configuration),
                prompt,
                profile,
                SelfExtensionMode::Off,
                resource_limits.clone(),
                child_template.runtime_policy_identities(),
            )
            .extensions(vec![HarnessSeedExtension {
                scope: HarnessSeedExtensionScope::Global,
                source: tea_luau::builtins::goal(extension_limits(resource_limits)),
            }])
            .capability_bindings(vec![goal_binding_ref.clone()])
            .trusted_tool_presentations(tool_presentations(configuration))
            .seed(HarnessActor::Host, created_at_ms)
            .map_err(|error| AppError::Setup(error.to_string()))?;
            Ok((model.descriptor.clone(), seeded))
        })
        .collect()
}

/// Stage every permitted child profile in the same immutable resolver catalog
/// as the root. The resulting identities are the host lookup table used for
/// an exact descriptor at child preparation time.
fn seed_child_harnesses(
    repository: &mut HarnessRepository,
    artifacts: Arc<dyn tea_session::ArtifactStore>,
    provider: Arc<dyn ModelProvider>,
    configuration: &AgentConfiguration,
    policy: &SubagentPolicy,
    resource_limits: &HarnessResourceLimits,
    goal_binding_ref: &CapabilityBindingRef,
    created_at_ms: u64,
) -> Result<Vec<(ModelDescriptor, HarnessIdentity)>, AppError> {
    child_harness_seeds(
        artifacts,
        provider,
        configuration,
        policy,
        resource_limits,
        goal_binding_ref,
        created_at_ms,
    )?
    .into_iter()
    .map(|(descriptor, seeded)| {
        let snapshot = repository
            .stage_snapshot(seeded.snapshot.spec)
            .map_err(|error| AppError::Setup(error.to_string()))?;
        let revision = repository
            .seed_revision(snapshot.id.clone(), HarnessActor::Host, created_at_ms)
            .map_err(|error| AppError::Setup(error.to_string()))?;
        if snapshot.id != seeded.snapshot.id || revision.revision_id != seeded.revision.revision_id
        {
            return Err(AppError::Setup(
                "child harness catalog staging did not preserve its deterministic identity".into(),
            ));
        }
        Ok((
            descriptor,
            HarnessIdentity::new(revision.revision_id, snapshot.id, seeded.profile.profile_id),
        ))
    })
    .collect()
}

/// Recompute the immutable child catalog identities during session reopen.
/// `AgentSpawned` facts are intentionally not consulted: the policy's closed
/// ordered model catalog is enough to recover models never used before exit.
fn derive_child_harnesses(
    artifacts: Arc<dyn tea_session::ArtifactStore>,
    provider: Arc<dyn ModelProvider>,
    configuration: &AgentConfiguration,
    policy: &SubagentPolicy,
    resource_limits: &HarnessResourceLimits,
    goal_binding_ref: &CapabilityBindingRef,
    created_at_ms: u64,
) -> Result<Vec<(ModelDescriptor, HarnessIdentity)>, AppError> {
    child_harness_seeds(
        artifacts,
        provider,
        configuration,
        policy,
        resource_limits,
        goal_binding_ref,
        created_at_ms,
    )?
    .into_iter()
    .map(|(descriptor, seeded)| {
        Ok((
            descriptor,
            HarnessIdentity::new(
                seeded.revision.revision_id,
                seeded.snapshot.id,
                seeded.profile.profile_id,
            ),
        ))
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
    SessionId::new(writer.finish().to_hex())
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, Instant};
    use tea_core::coding::TeaCodingToolsV2;
    use tea_core::hooks::NoHooks;
    use tea_core::scheduler::{
        CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture, ModelRequest,
        ModelStream, ModelStreamEvent,
    };
    use tea_core::state::StopReason;
    use tea_core::tool::ToolRegistry;
    use tea_session::ProviderErrorRecord;

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
    struct ProviderErrorFixture;

    impl ModelProvider for ProviderErrorFixture {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            Box::pin(std::future::ready(Ok(Box::new(ModelStream {
                events: vec![
                    ModelStreamEvent::ProviderError(ProviderErrorRecord {
                        source: "response".into(),
                        message: Some("OpenRouter rejected the request".into()),
                        status_code: Some(400),
                        attempt: Some(1),
                        error_type: None,
                        error_code: None,
                        retryable: Some(false),
                        response_bytes: Some(71),
                        request_bytes: Some(11_341),
                        response_body: Some(
                            r#"{"error":{"message":"invalid tool arguments"}}"#.into(),
                        ),
                    }),
                    ModelStreamEvent::Error {
                        message: "OpenRouter rejected the request".into(),
                    },
                ],
            }) as _)))
        }
    }

    #[derive(Debug)]
    struct TextThenCancellationProvider;

    struct TextThenCancellationStream {
        sent_text: bool,
    }

    impl ModelEventStream for TextThenCancellationStream {
        fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
            if !self.sent_text {
                self.sent_text = true;
                return Box::pin(std::future::ready(Ok(Some(ModelStreamEvent::TextDelta(
                    "partial".into(),
                )))));
            }
            Box::pin(async move {
                cancellation.cancelled().await;
                Ok(Some(ModelStreamEvent::Aborted {
                    message: "cancelled after output failure".into(),
                }))
            })
        }
    }

    impl ModelProvider for TextThenCancellationProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            Box::pin(std::future::ready(Ok(
                Box::new(TextThenCancellationStream { sent_text: false }) as _,
            )))
        }
    }

    #[derive(Debug)]
    struct PendingProvider;

    struct PendingStream;

    impl ModelEventStream for PendingStream {
        fn next_event<'a>(&'a mut self, _cancellation: CancellationToken) -> ModelEventFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    impl ModelProvider for PendingProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            Box::pin(std::future::ready(Ok(Box::new(PendingStream) as _)))
        }
    }

    struct FailingOutput;

    impl Write for FailingOutput {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "fixture output closed",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct RootSpawnThenPendingTextProvider {
        calls: AtomicU64,
    }

    impl ModelProvider for RootSpawnThenPendingTextProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            let stream: Box<dyn ModelEventStream> = match self.calls.fetch_add(1, Ordering::SeqCst)
            {
                0 => Box::new(ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("root-spawn-live-child")
                                .expect("fixture tool call ID"),
                            name: "spawn_agent".into(),
                            arguments: SerializedJson::new(format!(
                                r#"{{"task_name":"child","task":"Hold the isolated assignment until cancellation.","model":"{}","context":"task"}}"#,
                                tea_providers::local::LAGUNA_XS_2_1_MODEL,
                            )),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                }),
                1 => Box::new(TextThenCancellationStream { sent_text: false }),
                _ => Box::new(ModelStream {
                    events: vec![ModelStreamEvent::Error {
                        message: "root fixture received an unexpected extra request".into(),
                    }],
                }),
            };
            Box::pin(std::future::ready(Ok(stream)))
        }
    }

    struct WaitForChildThenFailOutput {
        child_started: Arc<AtomicBool>,
    }

    impl Write for WaitForChildThenFailOutput {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.child_started.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture child did not start before output",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "fixture output closed with a live child",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
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

    #[derive(Debug, Default)]
    struct RootSubagentScriptProvider {
        calls: AtomicU64,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl ModelProvider for RootSubagentScriptProvider {
        fn stream<'a>(
            &'a self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            self.requests
                .lock()
                .expect("root fixture request lock")
                .push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = match call {
                0 => vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("root-spawn-child").expect("fixture tool call ID"),
                        name: "spawn_agent".into(),
                        arguments: SerializedJson::new(format!(
                            r#"{{"task_name":"child","task":"Replace the tracked fixture text, then report completion.","model":"{}","context":"task"}}"#,
                            tea_providers::local::LAGUNA_XS_2_1_MODEL,
                        )),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
                1 => vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("root-wait-child").expect("fixture tool call ID"),
                        name: "wait_agent".into(),
                        arguments: SerializedJson::new(
                            r#"{"targets":["child"],"return_when":"all","timeout_ms":5000}"#,
                        ),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
                2 => vec![
                    ModelStreamEvent::TextDelta("root received child report".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
                _ => vec![ModelStreamEvent::Error {
                    message: "root fixture received an unexpected extra request".into(),
                }],
            };
            Box::pin(std::future::ready(
                Ok(Box::new(ModelStream { events }) as _),
            ))
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("Git fixture command starts");
        assert!(
            output.status.success(),
            "Git fixture command failed in {}: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn read_http_request(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let mut header_end = None;
        let mut expected = None;
        loop {
            let read = socket.read(&mut chunk).expect("fixture request reads");
            assert_ne!(read, 0, "provider request must not close before its body");
            bytes.extend_from_slice(&chunk[..read]);
            if header_end.is_none() {
                header_end = bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|offset| offset + 4);
                if let Some(end) = header_end {
                    let headers =
                        std::str::from_utf8(&bytes[..end]).expect("fixture HTTP headers are UTF-8");
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then_some(value.trim())
                        })
                        .expect("provider request has content length")
                        .parse::<usize>()
                        .expect("provider content length is numeric");
                    expected = Some(end + content_length);
                }
            }
            if expected.is_some_and(|length| bytes.len() >= length) {
                return String::from_utf8(bytes).expect("fixture request is UTF-8");
            }
        }
    }

    fn write_sse_response(socket: &mut TcpStream, body: &str) {
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("fixture response headers write");
        socket
            .write_all(body.as_bytes())
            .expect("fixture response body writes");
    }

    fn session_directory_for(home: &Path, session_id: &str) -> PathBuf {
        let sessions = home.join("sessions");
        let workspace = fs::read_dir(&sessions)
            .expect("session workspace root lists")
            .next()
            .expect("session workspace root exists")
            .expect("session workspace root is readable")
            .path();
        workspace.join(format!("{session_id}.tea"))
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

    fn durable_test_policy() -> SubagentPolicy {
        SubagentPolicy {
            models: vec![tea_core::runtime::SubagentModel {
                descriptor: ModelDescriptor {
                    provider: "fixture".into(),
                    model: "fixture-model".into(),
                    revision: Some("fixture-revision".into()),
                },
                display_name: "Fixture model".into(),
                context_window: NonZeroU64::new(16_384),
            }],
            max_concurrent: NonZeroU32::new(2).expect("nonzero fixture limit"),
            max_total_per_operation: NonZeroU32::new(4).expect("nonzero fixture limit"),
            timeout: Duration::from_secs(90),
        }
    }

    fn durable_test_subagent_config(config: SubagentTuiConfig) -> HostSubagentConfig {
        HostSubagentConfig {
            factory: Arc::new(ProviderFactory::new(
                tea_providers::ProviderRegistry::new(),
                None,
                None,
                "fixture logical workspace".into(),
            )),
            config,
        }
    }

    #[test]
    fn reopen_subagent_policy_allows_a_current_model_superset() {
        let policy = durable_test_policy();
        let fact = subagent_policy_fact(&policy).expect("durable fact encodes");
        let config = SubagentTuiConfig {
            provider: Some("fixture".into()),
            models: Some(vec!["fixture-model".into(), "newer-model".into()]),
            ..SubagentTuiConfig::default()
        };

        let reopened = reopen_subagent_policy(Some(&fact), Some(&config))
            .expect("a current policy superset remains an authorization ceiling")
            .expect("durable policy remains enabled");

        assert_eq!(reopened, policy);
    }

    #[test]
    fn newly_allocated_session_ids_are_unprefixed_digests() {
        let session_id = new_session_id(Path::new("/workspace"), 123, 7)
            .expect("session ID allocation should produce a valid ID");

        assert!(!session_id.as_str().starts_with("session-"));
        assert_eq!(session_id.as_str().len(), 64);
        assert!(session_id.as_str().bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn reopen_subagent_policy_rejects_current_provider_or_model_restrictions() {
        let fact = subagent_policy_fact(&durable_test_policy()).expect("durable fact encodes");
        let wrong_provider = SubagentTuiConfig {
            provider: Some("other".into()),
            ..SubagentTuiConfig::default()
        };
        assert!(reopen_subagent_policy(Some(&fact), Some(&wrong_provider),).is_err());

        let missing_model = SubagentTuiConfig {
            provider: Some("fixture".into()),
            models: Some(vec!["other-model".into()]),
            ..SubagentTuiConfig::default()
        };
        assert!(reopen_subagent_policy(Some(&fact), Some(&missing_model),).is_err());
    }

    #[test]
    fn reopen_subagent_policy_does_not_retrofit_a_disabled_session() {
        let enabled_later = SubagentTuiConfig::default();

        assert!(reopen_subagent_policy(None, Some(&enabled_later))
            .expect("disabled session does not need child services")
            .is_none());
    }

    #[test]
    fn enabled_host_persists_policy_before_root_revision_and_reopens_without_provider_io() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let model = ModelDescriptor {
            provider: super::super::mock::PROVIDER_ID.into(),
            model: super::super::mock::DEFAULT_MODEL_ID.into(),
            revision: None,
        };
        let subagents = durable_test_subagent_config(SubagentTuiConfig::default());
        let provider: Arc<dyn ModelProvider> = Arc::new(StopProvider);
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration: super::super::mock::configuration(),
            model: model.clone(),
            provider: Arc::clone(&provider),
            thinking_level: Some(ThinkingLevel::High),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: Some(subagents.clone()),
        })
        .expect("enabled host seeds the fixed root and child catalogs");
        let snapshot = harness.snapshot().expect("enabled session snapshots");
        let policy_sequence = snapshot
            .mutations()
            .find_map(|mutation| match mutation.mutation {
                tea_session::SessionMutationRef::Fact(fact)
                    if matches!(fact.fact, SessionFact::SubagentPolicy(_)) =>
                {
                    Some(mutation.seq)
                }
                _ => None,
            })
            .expect("policy fact persists");
        let revision_sequence = snapshot
            .mutations()
            .find_map(|mutation| match mutation.mutation {
                tea_session::SessionMutationRef::Entry(entry)
                    if matches!(entry.body, SessionEntry::HarnessRevisionChanged(_)) =>
                {
                    Some(mutation.seq)
                }
                _ => None,
            })
            .expect("initial root revision persists");
        assert!(policy_sequence < revision_sequence);
        assert_eq!(
            reduce_agent_graph(&snapshot)
                .expect("enabled graph reduces")
                .policy
                .expect("enabled graph retains policy")
                .models
                .len(),
            1
        );
        let session_id = snapshot.header().session_id.to_string();
        drop(harness);

        let reopened = reopen_host_harness(HostHarnessReopen {
            tea_home: &home,
            workspace: &workspace,
            session_id: &session_id,
            configuration: super::super::mock::configuration(),
            model,
            provider,
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: Some(subagents),
        })
        .expect("enabled catalog reopens without constructing a child adapter");
        assert_eq!(
            reopened
                .snapshot()
                .expect("reopened session snapshots")
                .last_sequence(),
            snapshot.last_sequence()
        );
        drop(reopened);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn scripted_subagent_lifecycle_keeps_the_physical_lease_out_of_provider_requests_and_reopens_results(
    ) {
        let home = temporary_home();
        let workspace = home.join("repository");
        fs::create_dir(&workspace).expect("fixture repository creates");
        git(&workspace, &["init"]);
        git(&workspace, &["config", "user.name", "Tea Fixture"]);
        git(
            &workspace,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(workspace.join("tracked.txt"), "original\n").expect("fixture file writes");
        git(&workspace, &["add", "tracked.txt"]);
        git(&workspace, &["commit", "-m", "initial"]);
        let workspace = fs::canonicalize(workspace).expect("fixture repository canonicalizes");

        let listener = TcpListener::bind("127.0.0.1:0").expect("child fixture listener binds");
        let address = listener
            .local_addr()
            .expect("child fixture listener address resolves");
        let provider_requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&provider_requests);
        let server = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("fixture listener becomes nonblocking");
            let edit_arguments = r#"{"files":[{"path":"tracked.txt","edits":[{"oldText":"original\n","newText":"child result\n"}]}]}"#;
            let serialized_arguments = tea_protocol::JsonValue::String(edit_arguments.into())
                .to_json_string()
                .expect("tool arguments encode");
            let edit = [
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"child-edit","function":{"name":"edit","arguments":"#,
                &serialized_arguments,
                r#"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
            ]
            .concat();
            let report = r#"data: {"choices":[{"delta":{"content":"child finished isolated edit"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":4}}

data: [DONE]

"#;
            for body in [&edit, report] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "child provider request did not arrive before the fixture deadline"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("child provider accept fails: {error}"),
                    }
                };
                // On platforms where an accepted stream inherits the listener's
                // nonblocking flag, parallel test load can leave the request body
                // momentarily unreadable after `accept`. The listener remains
                // nonblocking for its fail-fast deadline; each established HTTP
                // stream uses ordinary blocking request semantics.
                socket
                    .set_nonblocking(false)
                    .expect("accepted child provider stream becomes blocking");
                recorded_requests
                    .lock()
                    .expect("fixture request lock")
                    .push(read_http_request(&mut socket));
                write_sse_response(&mut socket, body);
            }
        });

        let root_model = ModelDescriptor {
            provider: "local".into(),
            model: "root-scripted-fixture".into(),
            revision: None,
        };
        let configuration = host_configuration(
            TeaCodingToolsV2::new(&workspace).expect("root coding tools configure"),
            &workspace.to_string_lossy(),
        )
        .expect("root host configuration builds");
        let subagents = HostSubagentConfig {
            factory: Arc::new(ProviderFactory::new(
                tea_providers::ProviderRegistry::new(),
                Some(format!("http://{address}/v1")),
                None,
                workspace.to_string_lossy().into_owned(),
            )),
            config: SubagentTuiConfig {
                provider: Some("local".into()),
                models: Some(vec![tea_providers::local::LAGUNA_XS_2_1_MODEL.into()]),
                ..SubagentTuiConfig::default()
            },
        };
        let root_provider = Arc::new(RootSubagentScriptProvider::default());
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration: configuration.clone(),
            model: root_model.clone(),
            provider: Arc::clone(&root_provider) as Arc<dyn ModelProvider>,
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: Some(subagents.clone()),
        })
        .expect("enabled fixture host creates");
        let session_id = harness
            .snapshot()
            .expect("fixture snapshot reads")
            .header()
            .session_id
            .to_string();

        let run = smol::block_on(harness.run_root_prompt("delegate the fixture edit"));
        run.expect("root spawn and wait complete through the supervisor");
        server.join().expect("child provider fixture settles");
        assert_eq!(
            fs::read(workspace.join("tracked.txt")).expect("parent fixture reads"),
            b"original\n",
            "the child edit must remain isolated until a later explicit apply"
        );

        let snapshot = harness.snapshot().expect("settled fixture snapshot reads");
        let graph = reduce_agent_graph(&snapshot).expect("child graph reduces");
        let child = graph
            .agents
            .values()
            .next()
            .expect("one child was spawned through the real root tool");
        assert!(
            matches!(child.state, tea_session::AgentState::DeltaReady { .. }),
            "child terminal report and isolated workspace delta must both be durable"
        );
        assert!(
            child
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.workspace_delta_id.is_some()),
            "wait_agent must observe the durable result rather than volatile task completion"
        );
        let expected_physical_lease = session_directory_for(&home, &session_id)
            .join("subagents")
            .join(tea_session::WorkspaceLeaseId::derive(&child.spawned.agent_id).as_str())
            .join("worktree");
        let delta_id = child
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.workspace_delta_id.as_ref())
            .expect("terminal child result names its durable delta")
            .to_string();
        let root_requests = root_provider
            .requests
            .lock()
            .expect("root fixture request lock");
        assert_eq!(
            root_requests.len(),
            3,
            "root makes spawn, wait, and post-wait turns"
        );
        let post_wait = &root_requests[2].context;
        assert!(
            post_wait.contains("child finished isolated edit"),
            "the root's next provider request receives the durable child report"
        );
        assert!(
            post_wait.contains(&delta_id) && post_wait.contains("tracked.txt"),
            "the root's next provider request receives the delta identity and changed path"
        );
        assert!(
            !post_wait.contains("diff --git")
                && !post_wait.contains(expected_physical_lease.to_string_lossy().as_ref()),
            "wait_agent exposes bounded result metadata, never patch bytes or the lease path"
        );
        drop(root_requests);
        let requests = provider_requests.lock().expect("fixture request lock");
        assert_eq!(
            requests.len(),
            2,
            "the child made an edit turn and a report turn"
        );
        for request in requests.iter() {
            assert!(
                request.contains(workspace.to_string_lossy().as_ref()),
                "the child request retains the stable original workspace label"
            );
            assert!(
                !request.contains(expected_physical_lease.to_string_lossy().as_ref()),
                "the prepared child provider request must never disclose its authority-bearing lease path"
            );
        }
        drop(requests);

        drop(harness);
        let reopened = reopen_host_harness(HostHarnessReopen {
            tea_home: &home,
            workspace: &workspace,
            session_id: &session_id,
            configuration,
            model: root_model,
            provider: root_provider as Arc<dyn ModelProvider>,
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: Some(subagents),
        })
        .expect("terminal child result reopens without replaying provider work");
        let reopened_graph = reduce_agent_graph(
            &reopened
                .snapshot()
                .expect("reopened fixture snapshot reads"),
        )
        .expect("reopened child graph reduces");
        assert!(
            matches!(
                reopened_graph
                    .agents
                    .values()
                    .next()
                    .expect("reopened child remains present")
                    .state,
                tea_session::AgentState::DeltaReady { .. }
            ),
            "reopen must retain the child report and unapplied isolated delta"
        );
        drop(reopened);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn child_catalog_derivation_recovers_unspawned_model_identities() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let configuration = host_configuration(
            TeaCodingToolsV2::new(&workspace).expect("coding tools configure"),
            &workspace.to_string_lossy(),
        )
        .expect("child configuration builds");
        let policy = SubagentPolicy {
            models: vec![
                tea_core::runtime::SubagentModel {
                    descriptor: ModelDescriptor {
                        provider: "fixture".into(),
                        model: "first-model".into(),
                        revision: Some("r1".into()),
                    },
                    display_name: "First fixture".into(),
                    context_window: NonZeroU64::new(8_192),
                },
                tea_core::runtime::SubagentModel {
                    descriptor: ModelDescriptor {
                        provider: "fixture".into(),
                        model: "never-spawned-model".into(),
                        revision: Some("r2".into()),
                    },
                    display_name: "Never spawned fixture".into(),
                    context_window: NonZeroU64::new(16_384),
                },
            ],
            max_concurrent: NonZeroU32::new(2).expect("nonzero fixture limit"),
            max_total_per_operation: NonZeroU32::new(4).expect("nonzero fixture limit"),
            timeout: Duration::from_secs(90),
        };
        let artifacts: Arc<dyn tea_session::ArtifactStore> =
            Arc::new(tea_session::MemoryArtifactStore::default());
        let provider: Arc<dyn ModelProvider> = Arc::new(StopProvider);
        let (_, binding) = goal_state_binding().expect("goal binding builds");
        let binding_ref = CapabilityBindingRef {
            plugin_id: binding.plugin_id().to_owned(),
            capability: binding.capability().to_owned(),
            capability_version: binding.capability_version().to_owned(),
            binding_digest: binding.binding_digest(),
        };
        let mut seeds = child_harness_seeds(
            Arc::clone(&artifacts),
            Arc::clone(&provider),
            &configuration,
            &policy,
            &HarnessResourceLimits::default(),
            &binding_ref,
            1,
        )
        .expect("initial child catalog seeds");
        for (_, seeded) in &seeds {
            let prompt = &seeded.snapshot.spec.base_system_prompt;
            assert!(prompt.ends_with(tea_core::runtime::CHILD_SUBAGENT_INSTRUCTION_SUFFIX));
            assert_eq!(
                prompt
                    .matches(tea_core::runtime::CHILD_SUBAGENT_INSTRUCTION_SUFFIX)
                    .count(),
                1
            );
            assert!(seeded.snapshot.spec.tool_presentations.iter().all(|tool| {
                !matches!(
                    tool.name.as_str(),
                    "spawn_agent"
                        | "wait_agent"
                        | "list_agents"
                        | "interrupt_agent"
                        | "apply_agent_changes"
                )
            }));
        }
        let (_, first) = seeds.remove(0);
        let mut repository = first.repository;
        let staged = seed_child_harnesses(
            &mut repository,
            Arc::clone(&artifacts),
            Arc::clone(&provider),
            &configuration,
            &policy,
            &HarnessResourceLimits::default(),
            &binding_ref,
            1,
        )
        .expect("every authorized child is staged in the resolver catalog");
        let reopened = derive_child_harnesses(
            artifacts,
            provider,
            &configuration,
            &policy,
            &HarnessResourceLimits::default(),
            &binding_ref,
            99,
        )
        .expect("reopen derives the complete catalog without spawn facts");

        assert_eq!(staged, reopened);
        assert!(reopened
            .iter()
            .any(|(model, _)| model.model == "never-spawned-model"));
        let _ = fs::remove_dir_all(home);
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
            subagents: None,
        })
        .expect("host harness creates");
        let before = harness.snapshot().expect("initial session snapshot");
        assert_eq!(
            before
                .header()
                .metadata
                .get(crate::build_info::SESSION_VERSION_METADATA_KEY)
                .and_then(JsonValue::as_str),
            Some(crate::build_info::PACKAGE_VERSION)
        );
        assert_eq!(
            before
                .header()
                .metadata
                .get(crate::build_info::SESSION_GIT_SHA_METADATA_KEY)
                .and_then(JsonValue::as_str),
            Some(crate::build_info::GIT_SHA)
        );
        assert!(before
            .entries()
            .iter()
            .any(|entry| matches!(entry.body, SessionEntry::HarnessRevisionChanged(_))));
        assert!(before
            .facts()
            .iter()
            .any(|fact| matches!(fact.fact, tea_session::SessionFact::HarnessCatalog(_))));
        let operation = smol::block_on(harness.run_root_prompt("persisted prompt"))
            .expect("durable prompt settles");
        assert!(operation.is_completed());
        let after = harness.snapshot().expect("completed session snapshot");
        assert!(after
            .records()
            .iter()
            .any(|record| matches!(record.record, tea_session::LaneRecord::OperationStarted(_))));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn one_shot_output_failure_aborts_and_settles_the_owned_root_drive() {
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
                revision: Some("fixture-revision".into()),
            },
            provider: Arc::new(TextThenCancellationProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: None,
        })
        .expect("host harness creates");
        let subscription = harness
            .subscribe_events()
            .expect("event subscription creates");
        let mut output = FailingOutput;
        let error = smol::block_on(stream_host_prompt_to(
            Arc::clone(&harness),
            subscription,
            "exercise output failure".into(),
            &mut output,
        ))
        .expect_err("closed output is reported");
        assert!(
            error.to_string().contains("fixture output closed"),
            "{error}"
        );
        let reduction =
            tea_session::reduce_lane(harness.snapshot().expect("snapshot reads"), LaneId::main())
                .expect("root lane reduces");
        assert!(
            reduction.lane_state.active_operation.is_none(),
            "the one-shot driver must settle before returning its output error"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn one_shot_output_failure_cancels_a_live_child_and_removes_its_worktree() {
        let home = temporary_home();
        let repository = home.join("repository");
        fs::create_dir(&repository).expect("fixture repository creates");
        git(&repository, &["init"]);
        git(&repository, &["config", "user.name", "Tea Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(repository.join("tracked.txt"), "original\n").expect("fixture file writes");
        git(&repository, &["add", "tracked.txt"]);
        git(&repository, &["commit", "-m", "initial"]);
        let workspace = fs::canonicalize(repository).expect("fixture repository canonicalizes");

        let listener = TcpListener::bind("127.0.0.1:0").expect("child fixture listener binds");
        let address = listener
            .local_addr()
            .expect("child fixture listener address resolves");
        let child_started = Arc::new(AtomicBool::new(false));
        let server_started = Arc::clone(&child_started);
        let server = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("fixture listener becomes nonblocking");
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "child provider request did not arrive before the fixture deadline"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("child provider accept fails: {error}"),
                }
            };
            socket
                .set_nonblocking(false)
                .expect("accepted child provider stream becomes blocking");
            let _request = read_http_request(&mut socket);
            server_started.store(true, Ordering::Release);
            thread::sleep(Duration::from_millis(500));
        });

        let root_model = ModelDescriptor {
            provider: "local".into(),
            model: "root-output-failure-fixture".into(),
            revision: None,
        };
        let configuration = host_configuration(
            TeaCodingToolsV2::new(&workspace).expect("root coding tools configure"),
            &workspace.to_string_lossy(),
        )
        .expect("root host configuration builds");
        let subagents = HostSubagentConfig {
            factory: Arc::new(ProviderFactory::new(
                tea_providers::ProviderRegistry::new(),
                Some(format!("http://{address}/v1")),
                None,
                workspace.to_string_lossy().into_owned(),
            )),
            config: SubagentTuiConfig {
                provider: Some("local".into()),
                models: Some(vec![tea_providers::local::LAGUNA_XS_2_1_MODEL.into()]),
                ..SubagentTuiConfig::default()
            },
        };
        let harness = create_host_harness(HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model: root_model,
            provider: Arc::new(RootSpawnThenPendingTextProvider::default()),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: Some(subagents),
        })
        .expect("enabled fixture host creates");
        let session_id = harness
            .snapshot()
            .expect("fixture snapshot reads")
            .header()
            .session_id
            .to_string();
        let subscription = harness
            .subscribe_events()
            .expect("event subscription creates");
        let mut output = WaitForChildThenFailOutput {
            child_started: Arc::clone(&child_started),
        };

        let error = smol::block_on(stream_host_prompt_to(
            Arc::clone(&harness),
            subscription,
            "spawn a child before output closes".into(),
            &mut output,
        ))
        .expect_err("closed output is reported");
        assert!(
            error
                .to_string()
                .contains("fixture output closed with a live child"),
            "{error}"
        );
        server.join().expect("held child provider fixture settles");

        let snapshot = harness.snapshot().expect("settled fixture snapshot reads");
        let root = reduce_lane(snapshot.clone(), LaneId::main()).expect("root lane reduces");
        assert!(
            root.lane_state.active_operation.is_none(),
            "the one-shot driver closes the root before returning"
        );
        let graph = reduce_agent_graph(&snapshot).expect("child graph reduces");
        let child = graph.agents.values().next().expect("one child was spawned");
        assert!(
            child.terminal.is_some(),
            "output failure retains a durable child terminal fact"
        );
        let worktree = session_directory_for(&home, &session_id)
            .join("subagents")
            .join(tea_session::WorkspaceLeaseId::derive(&child.spawned.agent_id).as_str())
            .join("worktree");
        assert!(
            !worktree.exists(),
            "output failure cleanup removes the authority-bearing child worktree"
        );

        drop(harness);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn root_settlement_check_reports_an_open_durable_recovery_obligation() {
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
                revision: Some("fixture-revision".into()),
            },
            provider: Arc::new(PendingProvider),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: None,
        })
        .expect("host harness creates");
        let mut drive = Box::pin(harness.run_root_prompt("leave a recoverable root open"));
        assert!(
            smol::block_on(smol::future::poll_once(&mut drive)).is_none(),
            "the fixture drive reaches its pending provider boundary"
        );

        let error = require_root_settled(&harness)
            .expect_err("an active durable operation cannot be reported as settled");
        assert!(
            matches!(
                error,
                tea_core::harness::HarnessError::RecoveryRequired { .. }
            ),
            "the host preserves the reducer-derived recovery obligation"
        );

        drop(drive);
        drop(harness);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn durable_host_sends_openai_compatible_context() {
        let home = temporary_home();
        let workspace = home.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        let configuration = host_configuration(
            TeaCodingToolsV2::new(&workspace).expect("Tea v2 tools configure"),
            &workspace.to_string_lossy(),
        )
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
            subagents: None,
        })
        .expect("durable host harness creates");

        smol::block_on(harness.run_root_prompt("hello"))
            .expect("durable host request should use compatible context");

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn durable_provider_error_is_retained_in_request_settlement() {
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
                provider: "openrouter".into(),
                model: "fixture-model".into(),
                revision: None,
            },
            provider: Arc::new(ProviderErrorFixture),
            thinking_level: Some(ThinkingLevel::Off),
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            subagents: None,
        })
        .expect("host harness creates");

        let error = smol::block_on(harness.run_root_prompt("trigger a provider error"))
            .expect_err("provider error must fail the durable operation");
        assert!(error.to_string().contains("OpenRouter rejected the request"));
        let snapshot = harness.snapshot().expect("durable snapshot reads");
        assert!(snapshot.records().iter().any(|stored| {
            matches!(
                &stored.record,
                tea_session::LaneRecord::ProviderRequestSettled(settled)
                    if settled.provider_error.as_ref().is_some_and(|error|
                        error.status_code == Some(400)
                            && error.request_bytes == Some(11_341)
                            && error.response_body.as_deref()
                                == Some(r#"{"error":{"message":"invalid tool arguments"}}"#))
            )
        }));
        drop(harness);
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
            subagents: None,
        })
        .expect("host harness creates");
        smol::block_on(harness.run_root_prompt("retain this durable prompt"))
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
            subagents: None,
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
            subagents: None,
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
            subagents: None,
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
            subagents: None,
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
            subagents: None,
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
            subagents: None,
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
