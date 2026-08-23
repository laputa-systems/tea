//! Deterministic model-context derivation from immutable semantic history.
//!
//! This module intentionally knows nothing about operation records or mutable
//! scheduler state. A context is a read-only projection from one branch leaf;
//! compaction and policy patches can omit model-visible entries, but neither
//! path deletes the underlying session tree.

use crate::lineage::LoadedPluginPolicy;
use crate::{HarnessError, HarnessSnapshotV1};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tea_core::state::{AgentMessage, AgentToolCall, MessageId, SerializedJson, StopReason, ToolCallId};
use tea_luau::{
    LuaPolicy, PolicyContextEntry, PolicyContextInput,
};
use tea_session::{
    reduce_lane, EntryId, LaneId, MemoryVisibility, PayloadRef, SessionEntry, SessionReader,
    SessionSnapshot, StoredEntry,
};

/// Explicit provider-facing context ceiling selected by the host profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderLimits {
    /// Maximum deterministic serialized bytes for the derived semantic
    /// context. This is independent of the immutable system-prompt surface.
    pub maximum_context_bytes: usize,
}

impl ProviderLimits {
    /// Construct one nonzero explicit context bound.
    pub fn new(maximum_context_bytes: usize) -> Result<Self, HarnessError> {
        if maximum_context_bytes == 0 {
            return Err(HarnessError::invalid_state(
                "provider context byte limit must be greater than zero",
            ));
        }
        Ok(Self {
            maximum_context_bytes,
        })
    }
}

/// A bounded non-semantic annotation selected by a context policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextAnnotation {
    /// Policy-local stable annotation ID.
    pub id: String,
    /// Exact bounded model-facing annotation content.
    pub content: String,
}

/// The only context-mutation vocabulary a policy may propose.
///
/// IDs name immutable semantic entries, not mutable vector offsets. Rust
/// validates all protected user/tool/recovery invariants before applying this
/// patch and retains every omitted entry in durable storage.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextProjectionPatch {
    /// Explicit model-visible entries to retain. An empty list retains the
    /// default eligible branch projection.
    pub retain_entries: Vec<EntryId>,
    /// Eligible entries to omit from the model projection only.
    pub omit_eligible_entries: Vec<EntryId>,
    /// Bounded policy annotations appended after selected semantic entries.
    pub annotations: Vec<ContextAnnotation>,
    /// Typed plugin-memory entries selected for this projection.
    pub selected_memory: Vec<EntryId>,
    /// A registered strategy selected for a future Rust compaction proposal.
    /// This value does not itself mutate semantic history.
    pub requested_compaction_strategy: Option<String>,
}

/// Complete immutable result of context derivation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedContext {
    /// Ordered core-compatible semantic messages.
    pub messages: Vec<AgentMessage>,
    /// Model-visible entries included before annotations.
    pub included_entries: Vec<EntryId>,
    /// Eligible branch entries intentionally left out of this projection.
    pub omitted_entries: Vec<EntryId>,
    /// Validated policy annotations that were appended to the projection.
    pub annotations: Vec<ContextAnnotation>,
    /// Exact deterministic byte estimate used for the provider limit check.
    pub serialized_bytes: usize,
    /// Canonical provider-neutral message bytes used for the limit check.
    ///
    /// A provider adapter may wrap these messages in its own request envelope,
    /// but this stable inner surface makes cache-prefix evidence testable
    /// without leaking provider-specific wire formats into the harness.
    pub serialized_context: String,
}

const MAX_CONTEXT_POLICY_ENTRIES: usize = 512;
const MAX_COMPOSED_CONTEXT_ANNOTATIONS: usize = 64;

/// Source-pinned policy VMs that may make only typed context proposals for an
/// immutable snapshot. This registry has no session writer, capability
/// binding, provider, or activation handle.
#[derive(Clone, Default)]
pub(crate) struct ContextPolicyRegistry {
    policies: Vec<ContextPolicyBinding>,
}

#[derive(Clone)]
struct ContextPolicyBinding {
    plugin_id: String,
    policy: Arc<LuaPolicy>,
}

impl std::fmt::Debug for ContextPolicyRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContextPolicyRegistry")
            .field("policy_count", &self.policies.len())
            .finish()
    }
}

impl ContextPolicyRegistry {
    pub(crate) fn from_loaded(policies: &[LoadedPluginPolicy]) -> Self {
        Self {
            policies: policies
                .iter()
                .map(|loaded| ContextPolicyBinding {
                    plugin_id: loaded.plugin.plugin_id.clone(),
                    policy: Arc::clone(&loaded.policy),
                })
                .collect(),
        }
    }

    fn derive_patch(&self, branch: &[StoredEntry]) -> Result<ContextProjectionPatch, HarnessError> {
        if self.policies.is_empty() {
            return Ok(ContextProjectionPatch::default());
        }
        let input = policy_context_input(branch);
        let mut retain: Option<BTreeSet<EntryId>> = None;
        let mut omit = BTreeSet::new();
        let mut selected_memory = BTreeSet::new();
        let mut annotations = Vec::new();
        let mut requested_compaction_strategy = None;
        for binding in &self.policies {
            let proposal = binding
                .policy
                .context_projection(&input)
                .map_err(|error| HarnessError::invalid_state(format!(
                    "context policy {} rejected its bounded proposal: {error}",
                    binding.plugin_id,
                )))?;
            if !proposal.retain_entries.is_empty() {
                let proposed = proposal
                    .retain_entries
                    .into_iter()
                    .map(parse_context_entry_id)
                    .collect::<Result<BTreeSet<_>, _>>()?;
                retain = match retain {
                    Some(existing) => {
                        let intersection = existing
                            .intersection(&proposed)
                            .cloned()
                            .collect::<BTreeSet<_>>();
                        if intersection.is_empty() {
                            return Err(HarnessError::invalid_state(
                                "context policies have incompatible explicit retain sets",
                            ));
                        }
                        Some(intersection)
                    }
                    None => Some(proposed),
                };
            }
            omit.extend(
                proposal
                    .omit_eligible_entries
                    .into_iter()
                    .map(parse_context_entry_id)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            selected_memory.extend(
                proposal
                    .selected_memory
                    .into_iter()
                    .map(parse_context_entry_id)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            for annotation in proposal.annotations {
                if annotations.len() == MAX_COMPOSED_CONTEXT_ANNOTATIONS {
                    return Err(HarnessError::invalid_state(format!(
                        "composed context annotations exceed {MAX_COMPOSED_CONTEXT_ANNOTATIONS}",
                    )));
                }
                annotations.push(ContextAnnotation {
                    id: format!("{}.{}", binding.plugin_id, annotation.id),
                    content: annotation.content,
                });
            }
            if let Some(strategy) = proposal.requested_compaction_strategy {
                match &requested_compaction_strategy {
                    Some(existing) if existing != &strategy => {
                        return Err(HarnessError::invalid_state(format!(
                            "context policies request conflicting compaction strategies {existing:?} and {strategy:?}",
                        )));
                    }
                    _ => requested_compaction_strategy = Some(strategy),
                }
            }
        }
        Ok(ContextProjectionPatch {
            retain_entries: retain
                .map(|values| values.into_iter().collect())
                .unwrap_or_default(),
            omit_eligible_entries: omit.into_iter().collect(),
            annotations,
            selected_memory: selected_memory.into_iter().collect(),
            requested_compaction_strategy,
        })
    }
}

/// Derive the default model context from one lane leaf.
///
/// The harness snapshot is intentionally an argument even though this first
/// Rust-owned default does not inspect editable source. It pins the API to an
/// immutable provider surface so a future v1 policy patch cannot accidentally
/// derive context against a mutable active configuration.
pub fn derive_model_context(
    session: &dyn SessionReader,
    lane: LaneId,
    harness: &HarnessSnapshotV1,
    limits: ProviderLimits,
) -> Result<DerivedContext, HarnessError> {
    derive_model_context_with_patch(session, lane, harness, limits, &ContextProjectionPatch::default())
}

/// Derive one context after validating a typed policy patch.
pub fn derive_model_context_with_patch(
    session: &dyn SessionReader,
    lane: LaneId,
    harness: &HarnessSnapshotV1,
    limits: ProviderLimits,
    patch: &ContextProjectionPatch,
) -> Result<DerivedContext, HarnessError> {
    let snapshot = session.snapshot()?;
    derive_snapshot_context_with_patch(&snapshot, lane, harness, limits, patch)
}

/// In-memory variant used by the durable supervisor after it has already
/// obtained one atomic session snapshot.
pub(crate) fn derive_snapshot_context_with_patch(
    snapshot: &SessionSnapshot,
    lane: LaneId,
    harness: &HarnessSnapshotV1,
    limits: ProviderLimits,
    patch: &ContextProjectionPatch,
) -> Result<DerivedContext, HarnessError> {
    derive_snapshot_context_with_patch_allowing_pending_tool_calls(
        snapshot,
        lane,
        harness,
        limits,
        patch,
        None,
    )
}

/// Derive a context after deterministic source-pinned policy composition.
/// The policy registry receives only metadata descriptors and its proposal is
/// still passed through the same Rust validation as a host-supplied patch.
pub(crate) fn derive_snapshot_context_with_policies(
    snapshot: &SessionSnapshot,
    lane: LaneId,
    harness: &HarnessSnapshotV1,
    limits: ProviderLimits,
    policies: &ContextPolicyRegistry,
    pending_tool_calls: Option<(&EntryId, &[AgentToolCall])>,
) -> Result<DerivedContext, HarnessError> {
    let branch = branch_entries(snapshot, &lane)?;
    let patch = policies.derive_patch(&branch)?;
    derive_snapshot_context_with_patch_allowing_pending_tool_calls(
        snapshot,
        lane,
        harness,
        limits,
        &patch,
        pending_tool_calls,
    )
}

/// Derive a context for a normal projection or a pending-tool recovery.
///
/// A recovery may include an assistant entry whose exact unresolved calls are
/// about to be restored into the core. Those calls are not yet durable results,
/// so they are the only unpaired calls this path accepts; every other selected
/// tool call/result pair remains protected.
fn derive_snapshot_context_with_patch_allowing_pending_tool_calls(
    snapshot: &SessionSnapshot,
    lane: LaneId,
    harness: &HarnessSnapshotV1,
    limits: ProviderLimits,
    patch: &ContextProjectionPatch,
    pending_tool_calls: Option<(&EntryId, &[AgentToolCall])>,
) -> Result<DerivedContext, HarnessError> {
    let _snapshot_identity = &harness.id;
    let branch = branch_entries(snapshot, &lane)?;
    validate_patch_shape(&branch, patch)?;
    let selected = select_entries(&branch, patch)?;
    let pending_tool_calls = pending_tool_calls
        .map(|(assistant_entry_id, calls)| {
            [(
                assistant_entry_id.clone(),
                calls
                    .iter()
                    .map(|call| call.id.to_string())
                    .collect::<BTreeSet<_>>(),
            )]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    validate_protected_context(&branch, &selected, &pending_tool_calls)?;

    let mut messages = Vec::new();
    let mut included_entries = Vec::new();
    for entry in &branch {
        if !selected.contains(&entry.header.id) {
            continue;
        }
        if let Some(message) = message_for_entry(entry, messages.len() as u64 + 1)? {
            messages.push(message);
            included_entries.push(entry.header.id.clone());
        }
    }
    for annotation in &patch.annotations {
        messages.push(AgentMessage::User {
            id: MessageId(messages.len() as u64 + 1),
            content: format!("[Context annotation {}]\n{}", annotation.id, annotation.content),
        });
    }
    let serialized_context = canonical_context_json_lines(&messages)?;
    let serialized_bytes = serialized_context.len();
    if serialized_bytes > limits.maximum_context_bytes {
        return Err(HarnessError::invalid_state(format!(
            "derived context is {serialized_bytes} bytes, exceeding provider limit {}",
            limits.maximum_context_bytes,
        )));
    }
    let omitted_entries = branch
        .iter()
        .filter(|entry| entry.body.is_model_visible() && !selected.contains(&entry.header.id))
        .map(|entry| entry.header.id.clone())
        .collect();
    Ok(DerivedContext {
        messages,
        included_entries,
        omitted_entries,
        annotations: patch.annotations.clone(),
        serialized_bytes,
        serialized_context,
    })
}

fn parse_context_entry_id(value: String) -> Result<EntryId, HarnessError> {
    EntryId::new(value).map_err(|error| {
        HarnessError::invalid_state(format!("context policy returned an invalid entry ID: {error}"))
    })
}

fn policy_context_input(branch: &[StoredEntry]) -> PolicyContextInput {
    let original_user = branch
        .iter()
        .find(|entry| matches!(entry.body, SessionEntry::UserMessage(_)))
        .map(|entry| entry.header.id.clone());
    let root_is_outside_tail = original_user.as_ref().is_some_and(|root_id| {
        branch
            .iter()
            .position(|entry| &entry.header.id == root_id)
            .is_some_and(|index| index < branch.len().saturating_sub(MAX_CONTEXT_POLICY_ENTRIES))
    });
    let tail_capacity = if root_is_outside_tail {
        MAX_CONTEXT_POLICY_ENTRIES.saturating_sub(1)
    } else {
        MAX_CONTEXT_POLICY_ENTRIES
    };
    let start = branch.len().saturating_sub(tail_capacity);
    let mut entries = branch[start..]
        .iter()
        .map(|entry| policy_context_entry(entry, original_user.as_ref()))
        .collect::<Vec<_>>();
    if let Some(root_id) = original_user
        && !entries.iter().any(|entry| entry.id == root_id.as_str())
    {
        let root = branch
            .iter()
            .find(|entry| entry.header.id == root_id)
            .expect("original user entry remains on its branch");
        entries.insert(0, policy_context_entry(root, Some(&root_id)));
    }
    PolicyContextInput { entries }
}

fn policy_context_entry(
    entry: &StoredEntry,
    original_user: Option<&EntryId>,
) -> PolicyContextEntry {
    PolicyContextEntry {
        id: entry.header.id.to_string(),
        kind: context_entry_kind(&entry.body).into(),
        model_visible: entry.body.is_model_visible(),
        protected: original_user.is_some_and(|id| id == &entry.header.id),
    }
}

fn context_entry_kind(entry: &SessionEntry) -> &'static str {
    match entry {
        SessionEntry::UserMessage(_) => "user",
        SessionEntry::AssistantMessage(_) => "assistant",
        SessionEntry::ToolResult(_) => "tool_result",
        SessionEntry::Compaction(_) => "compaction",
        SessionEntry::BranchSummary(_) => "branch_summary",
        SessionEntry::ModelChanged(_) => "model_changed",
        SessionEntry::ThinkingChanged(_) => "thinking_changed",
        SessionEntry::ToolActivationChanged(_) => "tool_activation_changed",
        SessionEntry::HarnessRevisionChanged(_) => "harness_revision_changed",
        SessionEntry::PluginMemory(_) => "plugin_memory",
        SessionEntry::Custom(_) => "custom",
    }
}

fn branch_entries(snapshot: &SessionSnapshot, lane: &LaneId) -> Result<Vec<StoredEntry>, HarnessError> {
    let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
    let entries = snapshot
        .entries()
        .iter()
        .map(|entry| (entry.header.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut chain = Vec::new();
    let mut cursor = reduction.lane_state.leaf_id;
    let mut seen = BTreeSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            return Err(HarnessError::invalid_state(format!(
                "semantic branch contains a parent cycle at entry {id}",
            )));
        }
        let entry = entries.get(&id).ok_or_else(|| {
            HarnessError::invalid_state(format!("branch leaf refers to missing entry {id}"))
        })?;
        cursor = entry.header.parent_id.clone();
        chain.push((*entry).clone());
    }
    chain.reverse();
    Ok(chain)
}

fn validate_patch_shape(branch: &[StoredEntry], patch: &ContextProjectionPatch) -> Result<(), HarnessError> {
    let known = branch
        .iter()
        .map(|entry| entry.header.id.clone())
        .collect::<BTreeSet<_>>();
    for (surface, ids) in [
        ("retain_entries", &patch.retain_entries),
        ("omit_eligible_entries", &patch.omit_eligible_entries),
        ("selected_memory", &patch.selected_memory),
    ] {
        let mut unique = BTreeSet::new();
        for id in ids {
            if !known.contains(id) {
                return Err(HarnessError::invalid_state(format!(
                    "context patch {surface} refers to entry {id} outside the current branch",
                )));
            }
            if !unique.insert(id) {
                return Err(HarnessError::invalid_state(format!(
                    "context patch {surface} repeats entry {id}",
                )));
            }
        }
    }
    let mut annotation_ids = BTreeSet::new();
    for annotation in &patch.annotations {
        if !portable_label(&annotation.id)
            || annotation.content.is_empty()
            || annotation.content.len() > 4 * 1024
        {
            return Err(HarnessError::invalid_state(
                "context patch annotation must use a bounded portable ID and non-empty <=4096 byte content",
            ));
        }
        if !annotation_ids.insert(&annotation.id) {
            return Err(HarnessError::invalid_state(format!(
                "context patch repeats annotation {}",
                annotation.id,
            )));
        }
    }
    if patch
        .requested_compaction_strategy
        .as_deref()
        .is_some_and(|id| !portable_label(id))
    {
        return Err(HarnessError::invalid_state(
            "context patch compaction strategy ID must use a portable bounded label",
        ));
    }
    Ok(())
}

fn select_entries(
    branch: &[StoredEntry],
    patch: &ContextProjectionPatch,
) -> Result<BTreeSet<EntryId>, HarnessError> {
    let by_id = branch
        .iter()
        .map(|entry| (entry.header.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut selected = if patch.retain_entries.is_empty() {
        default_visible_entries(branch)?
    } else {
        let mut retained = BTreeSet::new();
        for id in &patch.retain_entries {
            let entry = by_id.get(id).expect("shape validation checked entry ID");
            if matches!(
                entry.body,
                SessionEntry::PluginMemory(ref memory)
                    if memory.visibility == MemoryVisibility::ModelVisible
            ) {
                return Err(HarnessError::invalid_state(format!(
                    "context patch must select model-visible plugin memory {id} through selected_memory",
                )));
            }
            retained.insert(id.clone());
        }
        retained
    };
    for id in &patch.omit_eligible_entries {
        let entry = by_id.get(id).expect("shape validation checked entry ID");
        if !entry.body.is_model_visible() {
            return Err(HarnessError::invalid_state(format!(
                "context patch may omit only model-eligible entry {id}",
            )));
        }
        selected.remove(id);
    }
    for id in &patch.selected_memory {
        let entry = by_id.get(id).expect("shape validation checked entry ID");
        if !matches!(
            entry.body,
            SessionEntry::PluginMemory(ref memory)
                if memory.visibility == MemoryVisibility::ModelVisible
        ) {
            return Err(HarnessError::invalid_state(format!(
                "context patch selected_memory entry {id} is not model-visible plugin memory",
            )));
        }
        selected.insert(id.clone());
    }
    Ok(selected)
}

/// Select the host-owned default projection before a policy makes its bounded
/// retain/omit choice.  A committed compaction is semantic evidence that its
/// covered range has been replaced by a summary for model context; the source
/// entries remain available in the branch and in artifact/history tools.
fn default_visible_entries(branch: &[StoredEntry]) -> Result<BTreeSet<EntryId>, HarnessError> {
    let mut selected = branch
        .iter()
        .filter(|entry| {
            entry.body.is_model_visible()
                && !matches!(entry.body, SessionEntry::PluginMemory(_))
        })
        .map(|entry| entry.header.id.clone())
        .collect::<BTreeSet<_>>();
    let original_user = branch
        .iter()
        .find(|entry| matches!(entry.body, SessionEntry::UserMessage(_)))
        .map(|entry| entry.header.id.clone());

    for (compaction_index, entry) in branch.iter().enumerate() {
        let SessionEntry::Compaction(compaction) = &entry.body else {
            continue;
        };
        validate_compaction_range(branch, compaction_index, compaction)?;
        let Some(start) = compaction.covered_from.as_ref() else {
            continue;
        };
        let end = compaction
            .covered_to
            .as_ref()
            .expect("validated compaction range has matching endpoint");
        let start_index = branch
            .iter()
            .position(|candidate| &candidate.header.id == start)
            .expect("validated compaction start exists");
        let end_index = branch
            .iter()
            .position(|candidate| &candidate.header.id == end)
            .expect("validated compaction end exists");
        for covered in &branch[start_index..=end_index] {
            if original_user.as_ref() != Some(&covered.header.id) {
                selected.remove(&covered.header.id);
            }
        }
    }
    Ok(selected)
}

fn validate_compaction_range(
    branch: &[StoredEntry],
    compaction_index: usize,
    compaction: &tea_session::CompactionEntry,
) -> Result<(), HarnessError> {
    match (&compaction.covered_from, &compaction.covered_to) {
        (None, None) => return Ok(()),
        (Some(_), Some(_)) => {}
        _ => {
            return Err(HarnessError::invalid_state(
                "compaction context range must provide both covered endpoints or neither",
            ));
        }
    }
    let start = compaction.covered_from.as_ref().expect("matched above");
    let end = compaction.covered_to.as_ref().expect("matched above");
    let start_index = branch
        .iter()
        .position(|entry| &entry.header.id == start)
        .ok_or_else(|| HarnessError::invalid_state(format!(
            "compaction refers to covered start {start} outside its branch",
        )))?;
    let end_index = branch
        .iter()
        .position(|entry| &entry.header.id == end)
        .ok_or_else(|| HarnessError::invalid_state(format!(
            "compaction refers to covered end {end} outside its branch",
        )))?;
    if start_index > end_index || end_index >= compaction_index {
        return Err(HarnessError::invalid_state(
            "compaction coverage must be a nonempty earlier branch range",
        ));
    }
    if compaction.summary.is_empty() {
        return Err(HarnessError::invalid_state(
            "compaction summary must be non-empty when it replaces model context",
        ));
    }
    Ok(())
}

fn validate_protected_context(
    branch: &[StoredEntry],
    selected: &BTreeSet<EntryId>,
    pending_tool_calls: &BTreeMap<EntryId, BTreeSet<String>>,
) -> Result<(), HarnessError> {
    let first_user = branch.iter().find(|entry| matches!(entry.body, SessionEntry::UserMessage(_)));
    if let Some(first_user) = first_user
        && !selected.contains(&first_user.header.id)
    {
        return Err(HarnessError::invalid_state(
            "context patch may not remove the original user task",
        ));
    }

    let assistant_calls = branch
        .iter()
        .filter_map(|entry| match &entry.body {
            SessionEntry::AssistantMessage(assistant) => Some((
                entry.header.id.clone(),
                assistant
                    .tool_calls
                    .iter()
                    .map(|call| call.id.clone())
                    .collect::<BTreeSet<_>>(),
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let result_calls = branch
        .iter()
        .filter_map(|entry| match &entry.body {
            SessionEntry::ToolResult(result) => Some((
                entry.header.id.clone(),
                result.tool_call_id.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    for (assistant_id, calls) in &assistant_calls {
        if !selected.contains(assistant_id) {
            continue;
        }
        for call_id in calls {
            let paired = result_calls.iter().find(|(_, result_call)| result_call == call_id);
            let Some((result_id, _)) = paired else {
                if pending_tool_calls
                    .get(assistant_id)
                    .is_some_and(|pending| pending.contains(call_id))
                {
                    continue;
                }
                return Err(HarnessError::invalid_state(format!(
                    "selected assistant entry {assistant_id} has unpaired tool call {call_id}",
                )));
            };
            if !selected.contains(result_id) {
                return Err(HarnessError::invalid_state(format!(
                    "context patch separates tool call {call_id} from its durable result {result_id}",
                )));
            }
        }
    }
    for (result_id, call_id) in result_calls {
        if !selected.contains(&result_id) {
            continue;
        }
        let paired = assistant_calls
            .iter()
            .find(|(_, calls)| calls.contains(&call_id))
            .map(|(id, _)| id);
        let Some(assistant_id) = paired else {
            return Err(HarnessError::invalid_state(format!(
                "selected tool result {result_id} has no durable assistant call {call_id}",
            )));
        };
        if !selected.contains(assistant_id) {
            return Err(HarnessError::invalid_state(format!(
                "context patch separates durable tool result {result_id} from its assistant call {call_id}",
            )));
        }
    }
    Ok(())
}

fn message_for_entry(
    entry: &StoredEntry,
    message_number: u64,
) -> Result<Option<AgentMessage>, HarnessError> {
    let id = MessageId(message_number);
    match &entry.body {
        SessionEntry::UserMessage(user) => Ok(Some(AgentMessage::User {
            id,
            content: user.content.clone(),
        })),
        SessionEntry::AssistantMessage(assistant) => {
            let tool_calls = assistant
                .tool_calls
                .iter()
                .map(|call| {
                    Ok(AgentToolCall {
                        id: ToolCallId::new(call.id.clone()).map_err(|error| {
                            HarnessError::invalid_state(format!(
                                "durable assistant tool-call ID is invalid: {error}",
                            ))
                        })?,
                        name: call.name.clone(),
                        arguments: SerializedJson::new(call.arguments.to_json_string().map_err(
                            |error| {
                                HarnessError::invalid_state(format!(
                                    "durable assistant arguments cannot encode: {error}",
                                ))
                            },
                        )?),
                    })
                })
                .collect::<Result<Vec<_>, HarnessError>>()?;
            Ok(Some(AgentMessage::Assistant {
                id,
                content: assistant.content.clone(),
                tool_calls,
                stop_reason: assistant
                    .stop_reason
                    .as_deref()
                    .map(parse_stop_reason)
                    .transpose()?,
                error_message: assistant.error_message.clone(),
            }))
        }
        SessionEntry::ToolResult(result) => {
            let (content, details) = tool_projection_content(&result.model_projection)?;
            validate_recovery_locator(result)?;
            Ok(Some(AgentMessage::ToolResult {
                id,
                tool_call_id: ToolCallId::new(result.tool_call_id.clone()).map_err(|error| {
                    HarnessError::invalid_state(format!(
                        "durable tool-result call ID is invalid: {error}",
                    ))
                })?,
                tool_name: result.tool_name.clone(),
                content,
                details: details.map(SerializedJson::new),
                usage: Some(tea_core::state::Usage {
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    reasoning_tokens: result.usage.reasoning_tokens,
                    cache_read_tokens: result.usage.cache_read_tokens,
                    cache_write_tokens: result.usage.cache_write_tokens,
                    cost: result.usage.cost.clone(),
                }),
                added_tool_names: Vec::new(),
                terminate: result.terminate,
                is_error: result.is_error,
                failure: None,
            }))
        }
        SessionEntry::Compaction(entry) => Ok(Some(AgentMessage::User {
            id,
            content: format!("[Compaction summary]\n{}", entry.summary),
        })),
        SessionEntry::BranchSummary(entry) => Ok(Some(AgentMessage::User {
            id,
            content: format!("[Branch summary]\n{}", entry.summary),
        })),
        SessionEntry::PluginMemory(memory) if memory.visibility == MemoryVisibility::ModelVisible => {
            let PayloadRef::Inline(content) = &memory.content else {
                return Err(HarnessError::invalid_state(format!(
                    "model-visible plugin memory {}:{} is artifact-backed and needs an explicit artifact reader projection",
                    memory.plugin_id, memory.kind,
                )));
            };
            Ok(Some(AgentMessage::User {
                id,
                content: format!(
                    "[Plugin memory {}:{}]\n{}",
                    memory.plugin_id,
                    memory.kind,
                    content.to_json_string().map_err(|error| HarnessError::invalid_state(format!(
                        "model-visible plugin memory cannot encode: {error}",
                    )))?,
                ),
            }))
        }
        SessionEntry::Custom(custom) if custom.model_visible => Err(HarnessError::invalid_state(
            "model-visible custom semantic entry needs a trusted host context projection",
        )),
        SessionEntry::ModelChanged(_)
        | SessionEntry::ThinkingChanged(_)
        | SessionEntry::ToolActivationChanged(_)
        | SessionEntry::HarnessRevisionChanged(_)
        | SessionEntry::PluginMemory(_)
        | SessionEntry::Custom(_) => Ok(None),
    }
}

fn validate_recovery_locator(result: &tea_session::ToolResultEntry) -> Result<(), HarnessError> {
    let Some(locator) = result
        .model_projection
        .get("recovery_locator")
        .and_then(tea_protocol::JsonValue::as_str)
    else {
        return Ok(());
    };
    let PayloadRef::Artifact { artifact_id, .. } = result.full_result else {
        return Err(HarnessError::invalid_state(
            "tool-result projection names a recovery locator without retained artifact evidence",
        ));
    };
    let expected = format!("tea-artifact://blake3/{artifact_id}");
    if locator != expected {
        return Err(HarnessError::invalid_state(
            "tool-result recovery locator does not name its retained artifact",
        ));
    }
    Ok(())
}

fn tool_projection_content(projection: &tea_protocol::JsonValue) -> Result<(String, Option<String>), HarnessError> {
    let content = projection
        .get("content")
        .and_then(tea_protocol::JsonValue::as_str)
        .ok_or_else(|| HarnessError::invalid_state("tool-result model projection has no string content"))?
        .to_owned();
    let details = projection
        .get("details")
        .filter(|value| !value.is_null())
        .map(|value| {
            value.to_json_string().map_err(|error| {
                HarnessError::invalid_state(format!("tool-result projection details cannot encode: {error}"))
            })
        })
        .transpose()?;
    Ok((content, details))
}

fn canonical_context_json_lines(messages: &[AgentMessage]) -> Result<String, HarnessError> {
    let mut output = String::new();
    for message in messages {
        output.push_str(&canonical_message_json(message)?);
        output.push('\n');
    }
    Ok(output)
}

/// Encode the provider-neutral message envelope with deterministic JSON key
/// order. Runtime message IDs are deliberately not included: they are local
/// reconstruction counters, not provider-visible semantic content.
fn canonical_message_json(message: &AgentMessage) -> Result<String, HarnessError> {
    use tea_protocol::JsonValue;

    let value = match message {
        AgentMessage::User { content, .. } => object([
            ("content", JsonValue::String(content.clone())),
            ("role", JsonValue::String("user".into())),
        ]),
        AgentMessage::Assistant {
            content,
            tool_calls,
            stop_reason,
            error_message,
            ..
        } => object([
            ("content", JsonValue::String(content.clone())),
            (
                "error_message",
                error_message
                    .as_ref()
                    .map(|value| JsonValue::String(value.clone()))
                    .unwrap_or(JsonValue::Null),
            ),
            ("role", JsonValue::String("assistant".into())),
            (
                "stop_reason",
                stop_reason
                    .map(stop_reason_text)
                    .map(|value| JsonValue::String(value.into()))
                    .unwrap_or(JsonValue::Null),
            ),
            (
                "tool_calls",
                JsonValue::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            Ok(object([
                                (
                                    "arguments",
                                    JsonValue::parse(call.arguments.as_str()).map_err(|error| {
                                        HarnessError::invalid_state(format!(
                                            "derived assistant arguments cannot encode: {error}",
                                        ))
                                    })?,
                                ),
                                ("id", JsonValue::String(call.id.to_string())),
                                ("name", JsonValue::String(call.name.clone())),
                            ]))
                        })
                        .collect::<Result<Vec<_>, HarnessError>>()?,
                ),
            ),
        ]),
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            usage,
            added_tool_names,
            terminate,
            is_error,
            failure,
            ..
        } => {
            if failure.is_some() {
                return Err(HarnessError::invalid_state(
                    "durable context cannot serialize an unprojected host tool failure",
                ));
            }
            object([
                (
                    "added_tool_names",
                    JsonValue::Array(
                        added_tool_names
                            .iter()
                            .cloned()
                            .map(JsonValue::String)
                            .collect(),
                    ),
                ),
                ("content", JsonValue::String(content.clone())),
                (
                    "details",
                    details
                        .as_ref()
                        .map(|details| {
                            JsonValue::parse(details.as_str()).map_err(|error| {
                                HarnessError::invalid_state(format!(
                                    "derived tool details cannot encode: {error}",
                                ))
                            })
                        })
                        .transpose()?
                        .unwrap_or(JsonValue::Null),
                ),
                ("is_error", JsonValue::Bool(*is_error)),
                ("role", JsonValue::String("tool".into())),
                ("terminate", JsonValue::Bool(*terminate)),
                ("tool_call_id", JsonValue::String(tool_call_id.to_string())),
                ("tool_name", JsonValue::String(tool_name.clone())),
                (
                    "usage",
                    usage
                        .as_ref()
                        .map(canonical_usage)
                        .unwrap_or(JsonValue::Null),
                ),
            ])
        }
    };
    value.to_json_string().map_err(|error| {
        HarnessError::invalid_state(format!("derived context cannot encode canonically: {error}"))
    })
}

fn object<const N: usize>(fields: [(&str, tea_protocol::JsonValue); N]) -> tea_protocol::JsonValue {
    tea_protocol::JsonValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn canonical_usage(usage: &tea_core::state::Usage) -> tea_protocol::JsonValue {
    use tea_protocol::{JsonNumber, JsonValue};
    let number = |value: Option<u64>| {
        value
            .map(|value| JsonValue::Number(JsonNumber::Unsigned(value)))
            .unwrap_or(JsonValue::Null)
    };
    object([
        (
            "cache_read_tokens",
            number(usage.cache_read_tokens),
        ),
        (
            "cache_write_tokens",
            number(usage.cache_write_tokens),
        ),
        (
            "cost",
            usage
                .cost
                .as_ref()
                .map(|value| JsonValue::String(value.clone()))
                .unwrap_or(JsonValue::Null),
        ),
        ("input_tokens", number(usage.input_tokens)),
        ("output_tokens", number(usage.output_tokens)),
        ("reasoning_tokens", number(usage.reasoning_tokens)),
    ])
}

const fn stop_reason_text(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::ToolUse => "tool_use",
        StopReason::Length => "length",
        StopReason::Aborted => "aborted",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

fn parse_stop_reason(value: &str) -> Result<StopReason, HarnessError> {
    match value {
        "stop" => Ok(StopReason::Stop),
        "tool_use" => Ok(StopReason::ToolUse),
        "length" => Ok(StopReason::Length),
        "aborted" => Ok(StopReason::Aborted),
        "cancelled" => Ok(StopReason::Cancelled),
        "error" => Ok(StopReason::Error),
        _ => Err(HarnessError::invalid_state(format!(
            "durable assistant entry has unknown stop reason {value:?}",
        ))),
    }
}

fn portable_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 120
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
