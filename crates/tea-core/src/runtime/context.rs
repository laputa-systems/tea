//! Deterministic model-context derivation from immutable semantic history.
//!
//! This module intentionally knows nothing about operation records or mutable
//! scheduler state. A context is a read-only projection from one branch leaf;
//! compaction and policy patches can omit model-visible entries, but neither
//! path deletes the underlying session tree.

use crate::harness::{HarnessError, HarnessSnapshotV1};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tea_core::harness::extension::{
    ExtensionContextEntry, ExtensionContextInput, ExtensionContextPolicy,
};
use tea_core::state::{
    AgentMessage, AgentToolCall, MessageId, OpaqueProviderContextItem, SerializedJson, StopReason,
    ToolCallId, Usage,
};
use tea_session::{
    Digest, EntryId, LaneId, MemoryVisibility, PayloadRef, SessionEntry, SessionReader,
    SessionSnapshot, StoredEntry, reduce_lane,
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
    policy: Arc<dyn ExtensionContextPolicy>,
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
    pub(crate) fn from_resolved(
        policies: impl IntoIterator<Item = (String, Arc<dyn ExtensionContextPolicy>)>,
    ) -> Self {
        Self {
            policies: policies
                .into_iter()
                .map(|(plugin_id, policy)| ContextPolicyBinding { plugin_id, policy })
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.policies.is_empty()
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
            let proposal = binding.policy.project_context(&input).map_err(|error| {
                HarnessError::invalid_state(format!(
                    "context policy {} rejected its bounded proposal: {error}",
                    binding.plugin_id,
                ))
            })?;
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
    derive_model_context_with_patch(
        session,
        lane,
        harness,
        limits,
        &ContextProjectionPatch::default(),
    )
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
        snapshot, lane, harness, limits, patch, None,
    )
}

/// Derive the default durable context when an older host profile has no
/// source-pinned harness snapshot. This shares the exact compaction and
/// source-order projection used by the policy-aware path rather than
/// reinterpreting raw session entries in a supervisor-local fallback.
pub(crate) fn derive_default_snapshot_context(
    snapshot: &SessionSnapshot,
    lane: LaneId,
) -> Result<DerivedContext, HarnessError> {
    let branch = branch_entries(snapshot, &lane)?;
    let selected = default_visible_entries(&branch)?;
    validate_protected_context(&branch, &selected, &BTreeMap::new())?;
    let (messages, included_entries) = project_selected_entries(&branch, &selected, &lane)?;
    let serialized_context = canonical_context_json_lines(&messages)?;
    let omitted_entries = branch
        .iter()
        .filter(|entry| entry.body.is_model_visible() && !selected.contains(&entry.header.id))
        .map(|entry| entry.header.id.clone())
        .collect();
    Ok(DerivedContext {
        serialized_bytes: serialized_context.len(),
        serialized_context,
        messages,
        included_entries,
        omitted_entries,
        annotations: Vec::new(),
    })
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

    let (mut messages, included_entries) = project_selected_entries(&branch, &selected, &lane)?;
    for annotation in &patch.annotations {
        messages.push(AgentMessage::User {
            id: MessageId(messages.len() as u64 + 1),
            content: format!(
                "[Context annotation {}]\n{}",
                annotation.id, annotation.content
            ),
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

fn project_selected_entries(
    branch: &[StoredEntry],
    selected: &BTreeSet<EntryId>,
    lane: &LaneId,
) -> Result<(Vec<AgentMessage>, Vec<EntryId>), HarnessError> {
    let mut messages = Vec::new();
    let mut included_entries = Vec::new();
    for entry in source_ordered_selected_entries(branch, selected) {
        // A child lane may inherit canonical parent messages as visible
        // context, but provider-private continuation bytes belong only to the
        // lane that produced them. Do not copy a parent's encrypted reasoning
        // state into a child/provider boundary.
        let retain_opaque_context = entry.lane_id == *lane;
        let entry_messages = message_for_entry(
            entry,
            messages.len() as u64 + 1,
            retain_opaque_context,
        )?;
        if !entry_messages.is_empty() {
            messages.extend(entry_messages);
            included_entries.push(entry.header.id.clone());
        }
    }
    Ok((messages, included_entries))
}

fn parse_context_entry_id(value: String) -> Result<EntryId, HarnessError> {
    EntryId::new(value).map_err(|error| {
        HarnessError::invalid_state(format!(
            "context policy returned an invalid entry ID: {error}"
        ))
    })
}

fn policy_context_input(branch: &[StoredEntry]) -> ExtensionContextInput {
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
    ExtensionContextInput { entries }
}

fn policy_context_entry(
    entry: &StoredEntry,
    original_user: Option<&EntryId>,
) -> ExtensionContextEntry {
    ExtensionContextEntry {
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

fn branch_entries(
    snapshot: &SessionSnapshot,
    lane: &LaneId,
) -> Result<Vec<StoredEntry>, HarnessError> {
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

fn validate_patch_shape(
    branch: &[StoredEntry],
    patch: &ContextProjectionPatch,
) -> Result<(), HarnessError> {
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
            entry.body.is_model_visible() && !matches!(entry.body, SessionEntry::PluginMemory(_))
        })
        .map(|entry| entry.header.id.clone())
        .collect::<BTreeSet<_>>();
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
            selected.remove(&covered.header.id);
        }
    }
    Ok(selected)
}

/// Return selected entries in model source order without changing immutable
/// session history.
///
/// Parallel tools settle in completion order, so their `ToolResult` entries
/// may not be appended in the order the owning assistant emitted calls. A
/// normal turn does not begin its next assistant response until all of its
/// tool effects have settled. That boundary lets the projection order only
/// the results belonging to that one assistant, rather than globally matching
/// reusable provider call IDs across the entire branch.
fn source_ordered_selected_entries<'a>(
    branch: &'a [StoredEntry],
    selected: &BTreeSet<EntryId>,
) -> Vec<&'a StoredEntry> {
    let mut ordered = Vec::new();
    let mut cursor = 0;
    while cursor < branch.len() {
        let entry = &branch[cursor];
        let SessionEntry::AssistantMessage(assistant) = &entry.body else {
            if selected.contains(&entry.header.id) {
                ordered.push(entry);
            }
            cursor += 1;
            continue;
        };

        if selected.contains(&entry.header.id) {
            ordered.push(entry);
        }
        if assistant.tool_calls.is_empty() {
            cursor += 1;
            continue;
        }

        let mut result_entries = Vec::new();
        let mut next = cursor + 1;
        while next < branch.len() && !source_turn_boundary(&branch[next].body) {
            let candidate = &branch[next];
            if let SessionEntry::ToolResult(result) = &candidate.body
                && selected.contains(&candidate.header.id)
            {
                let source_index = assistant
                    .tool_calls
                    .iter()
                    .position(|call| {
                        call.id == result.tool_call_id && call.name == result.tool_name
                    })
                    .unwrap_or(usize::MAX);
                result_entries.push((source_index, next, candidate));
            }
            next += 1;
        }
        result_entries.sort_by_key(|(source_index, append_index, _)| {
            (*source_index, *append_index)
        });
        ordered.extend(result_entries.into_iter().map(|(_, _, entry)| entry));
        cursor = next;
    }
    ordered
}

/// Return whether this entry starts a new provider-visible semantic turn.
///
/// Model-invisible configuration records may be interleaved with concurrent
/// tool settlement and therefore do not break the ownership window.
fn source_turn_boundary(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::UserMessage(_)
        | SessionEntry::AssistantMessage(_)
        | SessionEntry::Compaction(_)
        | SessionEntry::BranchSummary(_) => true,
        SessionEntry::PluginMemory(memory) => memory.visibility == MemoryVisibility::ModelVisible,
        SessionEntry::Custom(custom) => custom.model_visible,
        SessionEntry::ToolResult(_)
        | SessionEntry::ModelChanged(_)
        | SessionEntry::ThinkingChanged(_)
        | SessionEntry::ToolActivationChanged(_)
        | SessionEntry::HarnessRevisionChanged(_) => false,
    }
}

fn validate_compaction_range(
    branch: &[StoredEntry],
    compaction_index: usize,
    compaction: &tea_session::CompactionEntry,
) -> Result<(), HarnessError> {
    if compaction.summary.is_empty() {
        return Err(HarnessError::invalid_state(
            "compaction summary must be non-empty when it replaces model context",
        ));
    }
    let PayloadRef::Inline(replacement) = &compaction.replacement else {
        return Err(HarnessError::invalid_state(
            "compaction replacement requires an inline canonical context payload",
        ));
    };
    if compaction_replacement_digest(replacement)? != compaction.replacement_digest {
        return Err(HarnessError::invalid_state(
            "compaction replacement digest does not match its exact context payload",
        ));
    }
    let replacement_messages = decode_compaction_replacement(replacement, 1)?;
    if replacement_messages.is_empty() {
        return Err(HarnessError::invalid_state(
            "compaction replacement must retain at least one canonical message",
        ));
    }
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
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "compaction refers to covered start {start} outside its branch",
            ))
        })?;
    let end_index = branch
        .iter()
        .position(|entry| &entry.header.id == end)
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "compaction refers to covered end {end} outside its branch",
            ))
        })?;
    if start_index > end_index || end_index >= compaction_index {
        return Err(HarnessError::invalid_state(
            "compaction coverage must be a nonempty earlier branch range",
        ));
    }
    Ok(())
}

fn validate_protected_context(
    branch: &[StoredEntry],
    selected: &BTreeSet<EntryId>,
    pending_tool_calls: &BTreeMap<EntryId, BTreeSet<String>>,
) -> Result<(), HarnessError> {
    let first_user = branch
        .iter()
        .find(|entry| matches!(entry.body, SessionEntry::UserMessage(_)));
    if let Some(first_user) = first_user
        && !selected.contains(&first_user.header.id)
        && !first_user_is_replaced_by_compaction(branch, selected, &first_user.header.id)?
    {
        return Err(HarnessError::invalid_state(
            "context patch may not remove the original user task",
        ));
    }

    for (assistant_index, entry) in branch.iter().enumerate() {
        let SessionEntry::AssistantMessage(assistant) = &entry.body else {
            continue;
        };
        if !selected.contains(&entry.header.id) {
            continue;
        }
        let mut seen_call_ids = BTreeSet::new();
        for call in &assistant.tool_calls {
            if !seen_call_ids.insert(&call.id) {
                return Err(HarnessError::invalid_state(format!(
                    "selected assistant entry {} repeats tool call {}",
                    entry.header.id, call.id,
                )));
            }
            let paired = source_results_for_tool_call(branch, assistant_index, &call.id, &call.name);
            if paired.is_empty()
                && pending_tool_calls
                    .get(&entry.header.id)
                    .is_some_and(|pending| pending.contains(&call.id))
            {
                continue;
            }
            if paired.len() != 1 {
                return Err(HarnessError::invalid_state(format!(
                    "selected assistant entry {} has {} durable results for tool call {}",
                    entry.header.id,
                    paired.len(),
                    call.id,
                )));
            }
            let result = &branch[paired[0]];
            if !selected.contains(&result.header.id) {
                return Err(HarnessError::invalid_state(format!(
                    "context patch separates tool call {} from its durable result {}",
                    call.id, result.header.id,
                )));
            }
        }
    }
    for (result_index, entry) in branch.iter().enumerate() {
        let SessionEntry::ToolResult(result) = &entry.body else {
            continue;
        };
        if !selected.contains(&entry.header.id) {
            continue;
        }
        let Some((assistant_index, assistant)) = source_assistant_for_tool_result(branch, result_index)
        else {
            return Err(HarnessError::invalid_state(format!(
                "selected tool result {} has no source-turn assistant",
                entry.header.id,
            )));
        };
        let matching_calls = assistant
            .tool_calls
            .iter()
            .filter(|call| call.id == result.tool_call_id && call.name == result.tool_name)
            .count();
        if matching_calls != 1 {
            return Err(HarnessError::invalid_state(format!(
                "selected tool result {} has no unique source assistant call {}",
                entry.header.id, result.tool_call_id,
            )));
        }
        let assistant_entry = &branch[assistant_index];
        if !selected.contains(&assistant_entry.header.id) {
            return Err(HarnessError::invalid_state(format!(
                "context patch separates durable tool result {} from its assistant call {}",
                entry.header.id, result.tool_call_id,
            )));
        }
    }
    Ok(())
}

fn source_results_for_tool_call(
    branch: &[StoredEntry],
    assistant_index: usize,
    call_id: &str,
    tool_name: &str,
) -> Vec<usize> {
    let mut results = Vec::new();
    for (index, entry) in branch.iter().enumerate().skip(assistant_index + 1) {
        if source_turn_boundary(&entry.body) {
            break;
        }
        if matches!(
            &entry.body,
            SessionEntry::ToolResult(result)
                if result.tool_call_id == call_id && result.tool_name == tool_name
        ) {
            results.push(index);
        }
    }
    results
}

fn source_assistant_for_tool_result(
    branch: &[StoredEntry],
    result_index: usize,
) -> Option<(usize, &tea_session::AssistantMessageEntry)> {
    for index in (0..result_index).rev() {
        match &branch[index].body {
            SessionEntry::AssistantMessage(assistant) => return Some((index, assistant)),
            entry if source_turn_boundary(entry) => return None,
            _ => {}
        }
    }
    None
}

fn first_user_is_replaced_by_compaction(
    branch: &[StoredEntry],
    selected: &BTreeSet<EntryId>,
    first_user: &EntryId,
) -> Result<bool, HarnessError> {
    let first_user_index = branch
        .iter()
        .position(|entry| &entry.header.id == first_user)
        .expect("protected first user is on its branch");
    for (compaction_index, entry) in branch.iter().enumerate() {
        if !selected.contains(&entry.header.id) {
            continue;
        }
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
            .expect("validated compaction coverage has an end");
        let start_index = branch
            .iter()
            .position(|candidate| &candidate.header.id == start)
            .expect("validated compaction start exists");
        let end_index = branch
            .iter()
            .position(|candidate| &candidate.header.id == end)
            .expect("validated compaction end exists");
        if (start_index..=end_index).contains(&first_user_index) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn message_for_entry(
    entry: &StoredEntry,
    message_number: u64,
    retain_opaque_context: bool,
) -> Result<Vec<AgentMessage>, HarnessError> {
    let id = MessageId(message_number);
    match &entry.body {
        SessionEntry::UserMessage(user) => Ok(vec![AgentMessage::User {
            id,
            content: user.content.clone(),
        }]),
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
            let opaque_context = if retain_opaque_context {
                assistant
                    .opaque_context
                    .iter()
                    .map(|item| {
                        OpaqueProviderContextItem::new(
                            item.provider.clone(),
                            item.kind.clone(),
                            item.item_id.clone(),
                            item.payload.clone(),
                        )
                        .map_err(|error| {
                            HarnessError::invalid_state(format!(
                                "durable assistant opaque provider context is invalid: {error}"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, HarnessError>>()?
            } else {
                Vec::new()
            };
            Ok(vec![AgentMessage::Assistant {
                id,
                content: assistant.content.clone(),
                tool_calls,
                stop_reason: assistant
                    .stop_reason
                    .as_deref()
                    .map(parse_stop_reason)
                    .transpose()?,
                error_message: assistant.error_message.clone(),
                opaque_context,
            }])
        }
        SessionEntry::ToolResult(result) => {
            let (content, details) = tool_projection_content(&result.model_projection)?;
            validate_recovery_locator(result)?;
            Ok(vec![AgentMessage::ToolResult {
                id,
                tool_call_id: ToolCallId::new(result.tool_call_id.clone()).map_err(|error| {
                    HarnessError::invalid_state(format!(
                        "durable tool-result call ID is invalid: {error}",
                    ))
                })?,
                tool_name: result.tool_name.clone(),
                content,
                details: details.map(SerializedJson::new),
                usage: Box::new(Some(tea_core::state::Usage {
                    total_tokens: result.usage.total_tokens,
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                    reasoning_tokens: result.usage.reasoning_tokens,
                    cache_read_tokens: result.usage.cache_read_tokens,
                    cache_write_tokens: result.usage.cache_write_tokens,
                    cost: result.usage.cost.clone(),
                })),
                added_tool_names: Vec::new(),
                terminate: result.terminate,
                is_error: result.is_error,
                failure: None,
            }])
        }
        SessionEntry::Compaction(entry) => {
            let PayloadRef::Inline(replacement) = &entry.replacement else {
                return Err(HarnessError::invalid_state(
                    "compaction replacement requires an inline canonical context payload",
                ));
            };
            if compaction_replacement_digest(replacement)? != entry.replacement_digest {
                return Err(HarnessError::invalid_state(
                    "compaction replacement digest does not match its exact context payload",
                ));
            }
            let mut messages = decode_compaction_replacement(replacement, message_number)?;
            if !retain_opaque_context {
                for message in &mut messages {
                    if let AgentMessage::Assistant { opaque_context, .. } = message {
                        opaque_context.clear();
                    }
                }
            }
            Ok(messages)
        }
        SessionEntry::BranchSummary(entry) => Ok(vec![AgentMessage::User {
            id,
            content: format!("[Branch summary]\n{}", entry.summary),
        }]),
        SessionEntry::PluginMemory(memory)
            if memory.visibility == MemoryVisibility::ModelVisible =>
        {
            let PayloadRef::Inline(content) = &memory.content else {
                return Err(HarnessError::invalid_state(format!(
                    "model-visible plugin memory {}:{} is artifact-backed and needs an explicit artifact reader projection",
                    memory.plugin_id, memory.kind,
                )));
            };
            Ok(vec![AgentMessage::User {
                id,
                content: format!(
                    "[Plugin memory {}:{}]\n{}",
                    memory.plugin_id,
                    memory.kind,
                    content
                        .to_json_string()
                        .map_err(|error| HarnessError::invalid_state(format!(
                            "model-visible plugin memory cannot encode: {error}",
                        )))?,
                ),
            }])
        }
        SessionEntry::Custom(custom) if custom.model_visible => Err(HarnessError::invalid_state(
            "model-visible custom semantic entry needs a trusted host context projection",
        )),
        SessionEntry::ModelChanged(_)
        | SessionEntry::ThinkingChanged(_)
        | SessionEntry::ToolActivationChanged(_)
        | SessionEntry::HarnessRevisionChanged(_)
        | SessionEntry::PluginMemory(_)
        | SessionEntry::Custom(_) => Ok(Vec::new()),
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

fn tool_projection_content(
    projection: &tea_protocol::JsonValue,
) -> Result<(String, Option<String>), HarnessError> {
    let content = projection
        .get("content")
        .and_then(tea_protocol::JsonValue::as_str)
        .ok_or_else(|| {
            HarnessError::invalid_state("tool-result model projection has no string content")
        })?
        .to_owned();
    let details = projection
        .get("details")
        .filter(|value| !value.is_null())
        .map(|value| {
            value.to_json_string().map_err(|error| {
                HarnessError::invalid_state(format!(
                    "tool-result projection details cannot encode: {error}"
                ))
            })
        })
        .transpose()?;
    Ok((content, details))
}

/// Encode one exact post-validation compaction replacement for durable
/// session context. Message IDs are intentionally omitted because they are
/// process-local reconstruction counters rather than provider-visible
/// material.
pub(crate) fn encode_compaction_replacement(
    messages: &[AgentMessage],
) -> Result<tea_protocol::JsonValue, HarnessError> {
    use tea_protocol::{JsonNumber, JsonValue};

    Ok(object([
        (
            "messages",
            JsonValue::Array(
                messages
                    .iter()
                    .map(compaction_replacement_message)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        ),
        ("version", JsonValue::Number(JsonNumber::Unsigned(1))),
    ]))
}

/// Return the stable digest recorded beside exact replacement material.
pub(crate) fn compaction_replacement_digest(
    replacement: &tea_protocol::JsonValue,
) -> Result<Digest, HarnessError> {
    let encoded = replacement.to_json_string().map_err(|error| {
        HarnessError::invalid_state(format!(
            "compaction replacement cannot encode canonically: {error}",
        ))
    })?;
    Ok(Digest::from_bytes(encoded))
}

fn compaction_replacement_message(
    message: &AgentMessage,
) -> Result<tea_protocol::JsonValue, HarnessError> {
    use tea_protocol::JsonValue;

    match message {
        AgentMessage::User { content, .. } => Ok(object([
            ("content", JsonValue::String(content.clone())),
            ("role", JsonValue::String("user".into())),
        ])),
        AgentMessage::Assistant {
            content,
            tool_calls,
            stop_reason,
            error_message,
            opaque_context,
            ..
        } => Ok(object([
            ("content", JsonValue::String(content.clone())),
            (
                "error_message",
                error_message
                    .as_ref()
                    .map(|value| JsonValue::String(value.clone()))
                    .unwrap_or(JsonValue::Null),
            ),
            (
                "opaque_context",
                JsonValue::Array(
                    opaque_context
                        .iter()
                        .map(|item| {
                            object([
                                ("item_id", optional_json_string(item.item_id())),
                                ("kind", JsonValue::String(item.kind().into())),
                                ("payload", JsonValue::String(item.payload().into())),
                                ("provider", JsonValue::String(item.provider().into())),
                            ])
                        })
                        .collect(),
                ),
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
                                    tea_protocol::JsonValue::parse(call.arguments.as_str()).map_err(
                                        |error| {
                                            HarnessError::invalid_state(format!(
                                                "compaction replacement tool arguments cannot encode: {error}",
                                            ))
                                        },
                                    )?,
                                ),
                                ("id", JsonValue::String(call.id.to_string())),
                                ("name", JsonValue::String(call.name.clone())),
                            ]))
                        })
                        .collect::<Result<Vec<_>, HarnessError>>()?,
                ),
            ),
        ])),
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            usage,
            added_tool_names,
            terminate,
            is_error,
            // `failure` is deliberately not encoded. It is the run-local host
            // classification behind the tool circuit breaker: never
            // model-visible and never stored in `ToolResultEntry`, so durable
            // reconstruction always yields `None`. Encoding it would make a
            // live source mismatch its own committed history.
            ..
        } => {
            Ok(object([
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
                                    "compaction replacement tool details cannot encode: {error}",
                                ))
                            })
                        })
                        .transpose()?
                        .unwrap_or(JsonValue::Null),
                ),
                ("is_error", JsonValue::Bool(*is_error)),
                ("role", JsonValue::String("tool_result".into())),
                ("terminate", JsonValue::Bool(*terminate)),
                ("tool_call_id", JsonValue::String(tool_call_id.to_string())),
                ("tool_name", JsonValue::String(tool_name.clone())),
                (
                    "usage",
                    // `ToolResultEntry` stores a non-optional usage, so an
                    // unreported value reopens as all-`None` fields rather
                    // than `None`. Both mean nothing was reported.
                    usage
                        .as_ref()
                        .as_ref()
                        .filter(|usage| usage.is_reported())
                        .map(compaction_replacement_usage)
                        .unwrap_or(JsonValue::Null),
                ),
            ]))
        }
    }
}

fn compaction_replacement_usage(usage: &Usage) -> tea_protocol::JsonValue {
    use tea_protocol::{JsonNumber, JsonValue};

    let number = |value: Option<u64>| {
        value
            .map(|value| JsonValue::Number(JsonNumber::Unsigned(value)))
            .unwrap_or(JsonValue::Null)
    };
    object([
        ("cache_read_tokens", number(usage.cache_read_tokens)),
        ("cache_write_tokens", number(usage.cache_write_tokens)),
        (
            "cost",
            optional_json_string(usage.cost.as_deref()),
        ),
        ("input_tokens", number(usage.input_tokens)),
        ("output_tokens", number(usage.output_tokens)),
        ("reasoning_tokens", number(usage.reasoning_tokens)),
        ("total_tokens", number(usage.total_tokens)),
    ])
}

fn optional_json_string(value: Option<&str>) -> tea_protocol::JsonValue {
    value
        .map(|value| tea_protocol::JsonValue::String(value.into()))
        .unwrap_or(tea_protocol::JsonValue::Null)
}

/// Decode exact replacement material and assign fresh process-local message
/// IDs beginning at `first_message_number`.
pub(crate) fn decode_compaction_replacement(
    replacement: &tea_protocol::JsonValue,
    first_message_number: u64,
) -> Result<Vec<AgentMessage>, HarnessError> {
    let object = replacement.as_object().ok_or_else(|| {
        HarnessError::invalid_state("compaction replacement payload must be an object")
    })?;
    if object.get("version").and_then(tea_protocol::JsonValue::as_u64) != Some(1) {
        return Err(HarnessError::invalid_state(
            "compaction replacement payload has an unsupported version",
        ));
    }
    let values = object
        .get("messages")
        .and_then(tea_protocol::JsonValue::as_array)
        .ok_or_else(|| {
            HarnessError::invalid_state("compaction replacement payload has no messages array")
        })?;
    let messages = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            decode_compaction_replacement_message(
                value,
                MessageId(first_message_number.saturating_add(index as u64)),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    tea_core::compaction::validate_messages(&messages).map_err(|error| {
        HarnessError::invalid_state(format!(
            "compaction replacement violates canonical message invariants: {error}",
        ))
    })?;
    Ok(messages)
}

fn decode_compaction_replacement_message(
    value: &tea_protocol::JsonValue,
    id: MessageId,
) -> Result<AgentMessage, HarnessError> {
    let object = value.as_object().ok_or_else(|| {
        HarnessError::invalid_state("compaction replacement message must be an object")
    })?;
    match required_compaction_string(object, "role")?.as_str() {
        "user" => Ok(AgentMessage::User {
            id,
            content: required_compaction_string(object, "content")?,
        }),
        "assistant" => {
            let tool_calls = required_compaction_array(object, "tool_calls")?
                .iter()
                .map(|call| {
                    let call = call.as_object().ok_or_else(|| {
                        HarnessError::invalid_state(
                            "compaction replacement tool call must be an object",
                        )
                    })?;
                    Ok(AgentToolCall {
                        id: ToolCallId::new(required_compaction_string(call, "id")?).map_err(
                            |error| {
                                HarnessError::invalid_state(format!(
                                    "compaction replacement tool-call ID is invalid: {error}",
                                ))
                            },
                        )?,
                        name: required_compaction_string(call, "name")?,
                        arguments: SerializedJson::new(
                            required_compaction_value(call, "arguments")?
                                .to_json_string()
                                .map_err(|error| {
                                    HarnessError::invalid_state(format!(
                                        "compaction replacement tool arguments cannot encode: {error}",
                                    ))
                                })?,
                        ),
                    })
                })
                .collect::<Result<Vec<_>, HarnessError>>()?;
            let opaque_context = required_compaction_array(object, "opaque_context")?
                .iter()
                .map(|item| {
                    let item = item.as_object().ok_or_else(|| {
                        HarnessError::invalid_state(
                            "compaction replacement opaque context item must be an object",
                        )
                    })?;
                    let item_id = optional_compaction_string(item, "item_id")?;
                    OpaqueProviderContextItem::new(
                        required_compaction_string(item, "provider")?,
                        required_compaction_string(item, "kind")?,
                        item_id,
                        required_compaction_string(item, "payload")?,
                    )
                    .map_err(|error| {
                        HarnessError::invalid_state(format!(
                            "compaction replacement opaque context is invalid: {error}",
                        ))
                    })
                })
                .collect::<Result<Vec<_>, HarnessError>>()?;
            Ok(AgentMessage::Assistant {
                id,
                content: required_compaction_string(object, "content")?,
                tool_calls,
                stop_reason: optional_compaction_string(object, "stop_reason")?
                    .as_deref()
                    .map(parse_stop_reason)
                    .transpose()?,
                error_message: optional_compaction_string(object, "error_message")?,
                opaque_context,
            })
        }
        "tool_result" => {
            let details_value = required_compaction_value(object, "details")?;
            let details = if details_value.is_null() {
                None
            } else {
                Some(
                    details_value
                        .to_json_string()
                        .map(SerializedJson::new)
                        .map_err(|error| {
                            HarnessError::invalid_state(format!(
                                "compaction replacement tool details cannot encode: {error}",
                            ))
                        })?,
                )
            };
            let added_tool_names = required_compaction_array(object, "added_tool_names")?
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        HarnessError::invalid_state(
                            "compaction replacement added tool name must be a string",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(AgentMessage::ToolResult {
                id,
                tool_call_id: ToolCallId::new(required_compaction_string(object, "tool_call_id")?)
                    .map_err(|error| {
                        HarnessError::invalid_state(format!(
                            "compaction replacement tool-result call ID is invalid: {error}",
                        ))
                    })?,
                tool_name: required_compaction_string(object, "tool_name")?,
                content: required_compaction_string(object, "content")?,
                details,
                usage: Box::new(decode_compaction_usage(required_compaction_value(
                    object, "usage",
                )?)?),
                added_tool_names,
                terminate: required_compaction_bool(object, "terminate")?,
                is_error: required_compaction_bool(object, "is_error")?,
                failure: None,
            })
        }
        other => Err(HarnessError::invalid_state(format!(
            "compaction replacement message has unknown role {other:?}",
        ))),
    }
}

fn decode_compaction_usage(value: &tea_protocol::JsonValue) -> Result<Option<Usage>, HarnessError> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or_else(|| {
        HarnessError::invalid_state("compaction replacement usage must be an object or null")
    })?;
    Ok(Some(Usage {
        total_tokens: optional_compaction_u64(object, "total_tokens")?,
        input_tokens: optional_compaction_u64(object, "input_tokens")?,
        output_tokens: optional_compaction_u64(object, "output_tokens")?,
        reasoning_tokens: optional_compaction_u64(object, "reasoning_tokens")?,
        cache_read_tokens: optional_compaction_u64(object, "cache_read_tokens")?,
        cache_write_tokens: optional_compaction_u64(object, "cache_write_tokens")?,
        cost: optional_compaction_string(object, "cost")?,
    }))
}

fn required_compaction_value<'a>(
    object: &'a std::collections::BTreeMap<String, tea_protocol::JsonValue>,
    field: &str,
) -> Result<&'a tea_protocol::JsonValue, HarnessError> {
    object.get(field).ok_or_else(|| {
        HarnessError::invalid_state(format!(
            "compaction replacement message is missing {field:?}",
        ))
    })
}

fn required_compaction_string(
    object: &std::collections::BTreeMap<String, tea_protocol::JsonValue>,
    field: &str,
) -> Result<String, HarnessError> {
    required_compaction_value(object, field)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "compaction replacement field {field:?} must be a string",
            ))
        })
}

fn optional_compaction_string(
    object: &std::collections::BTreeMap<String, tea_protocol::JsonValue>,
    field: &str,
) -> Result<Option<String>, HarnessError> {
    let value = required_compaction_value(object, field)?;
    if value.is_null() {
        return Ok(None);
    }
    value.as_str().map(str::to_owned).map(Some).ok_or_else(|| {
        HarnessError::invalid_state(format!(
            "compaction replacement field {field:?} must be a string or null",
        ))
    })
}

fn required_compaction_array<'a>(
    object: &'a std::collections::BTreeMap<String, tea_protocol::JsonValue>,
    field: &str,
) -> Result<&'a [tea_protocol::JsonValue], HarnessError> {
    required_compaction_value(object, field)?
        .as_array()
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "compaction replacement field {field:?} must be an array",
            ))
        })
}

fn required_compaction_bool(
    object: &std::collections::BTreeMap<String, tea_protocol::JsonValue>,
    field: &str,
) -> Result<bool, HarnessError> {
    required_compaction_value(object, field)?
        .as_bool()
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "compaction replacement field {field:?} must be a boolean",
            ))
        })
}

fn optional_compaction_u64(
    object: &std::collections::BTreeMap<String, tea_protocol::JsonValue>,
    field: &str,
) -> Result<Option<u64>, HarnessError> {
    let value = required_compaction_value(object, field)?;
    if value.is_null() {
        return Ok(None);
    }
    value.as_u64().map(Some).ok_or_else(|| {
        HarnessError::invalid_state(format!(
            "compaction replacement field {field:?} must be an unsigned integer or null",
        ))
    })
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
                        .as_ref()
                        .map(canonical_usage)
                        .unwrap_or(JsonValue::Null),
                ),
            ])
        }
    };
    value.to_json_string().map_err(|error| {
        HarnessError::invalid_state(format!(
            "derived context cannot encode canonically: {error}"
        ))
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
        ("cache_read_tokens", number(usage.cache_read_tokens)),
        ("cache_write_tokens", number(usage.cache_write_tokens)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_opaque_continuation_is_not_projected_into_a_child_lane() {
        let entry = StoredEntry {
            lane_id: LaneId::main(),
            header: tea_session::EntryHeader {
                id: EntryId::new("assistant-with-opaque").expect("fixture entry ID"),
                parent_id: None,
                seq: tea_session::Sequence(1),
                timestamp_ms: 1,
            },
            body: SessionEntry::AssistantMessage(tea_session::AssistantMessageEntry {
                content: "visible parent answer".into(),
                tool_calls: Vec::new(),
                stop_reason: Some("stop".into()),
                error_message: None,
                opaque_context: vec![tea_session::OpaqueProviderContextEntry {
                    provider: "codex".into(),
                    kind: "reasoning".into(),
                    item_id: Some("rs_1".into()),
                    payload: "opaque-parent-state".into(),
                }],
                metadata: BTreeMap::new(),
            }),
        };

        let child_projection = message_for_entry(&entry, 1, false)
            .expect("parent entry should project")
            .into_iter()
            .next()
            .expect("assistant is model visible");
        let own_lane_projection = message_for_entry(&entry, 1, true)
            .expect("same-lane entry should project")
            .into_iter()
            .next()
            .expect("assistant is model visible");
        let AgentMessage::Assistant {
            opaque_context: child_opaque,
            ..
        } = child_projection
        else {
            panic!("fixture must project as an assistant message");
        };
        let AgentMessage::Assistant {
            opaque_context: own_opaque,
            ..
        } = own_lane_projection
        else {
            panic!("fixture must project as an assistant message");
        };
        assert!(child_opaque.is_empty());
        assert_eq!(own_opaque.len(), 1);
        assert_eq!(own_opaque[0].provider(), "codex");
    }

    #[test]
    fn compaction_replacement_round_trips_exact_canonical_messages() {
        let messages = vec![
            AgentMessage::User {
                id: MessageId(7),
                content: "original task".into(),
            },
            AgentMessage::Assistant {
                id: MessageId(8),
                content: "calling a tool".into(),
                tool_calls: vec![AgentToolCall {
                    id: ToolCallId::new("compact-call").expect("fixture call ID"),
                    name: "read_file".into(),
                    arguments: SerializedJson::new(r#"{"path":"src/lib.rs"}"#),
                }],
                stop_reason: Some(StopReason::ToolUse),
                error_message: None,
                opaque_context: vec![
                    OpaqueProviderContextItem::new(
                        "fixture-provider",
                        "reasoning",
                        Some("opaque-1".into()),
                        "opaque-state",
                    )
                    .expect("fixture opaque state"),
                ],
            },
            AgentMessage::ToolResult {
                id: MessageId(9),
                tool_call_id: ToolCallId::new("compact-call").expect("fixture call ID"),
                tool_name: "read_file".into(),
                content: "file contents".into(),
                details: Some(SerializedJson::new(r#"{"line_count":3}"#)),
                usage: Box::new(Some(Usage {
                    input_tokens: Some(2),
                    output_tokens: Some(3),
                    total_tokens: Some(5),
                    ..Usage::default()
                })),
                added_tool_names: vec!["read_file".into()],
                terminate: false,
                is_error: false,
                failure: None,
            },
        ];

        let encoded = encode_compaction_replacement(&messages).expect("replacement encodes");
        let decoded = decode_compaction_replacement(&encoded, 1).expect("replacement decodes");

        assert_eq!(
            encode_compaction_replacement(&decoded).expect("decoded replacement re-encodes"),
            encoded,
        );
        assert_eq!(
            compaction_replacement_digest(&encoded).expect("replacement digest"),
            compaction_replacement_digest(
                &encode_compaction_replacement(&decoded).expect("decoded replacement encodes"),
            )
            .expect("decoded replacement digest"),
        );
    }

    #[test]
    fn compaction_replacement_encodes_a_live_tool_failure_like_its_durable_reconstruction() {
        let failed = |failure| AgentMessage::ToolResult {
            id: MessageId(1),
            tool_call_id: ToolCallId::new("failed-read").expect("fixture call ID"),
            tool_name: "read".into(),
            content: "no such file".into(),
            details: None,
            usage: Box::new(None),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: true,
            failure,
        };
        let live = encode_compaction_replacement(&[failed(Some(
            crate::tool::ToolFailure::recoverable(),
        ))])
        .expect("a retained live tool failure can be compacted");
        let durable = encode_compaction_replacement(&[failed(None)])
            .expect("the durable reconstruction encodes");

        assert_eq!(
            live, durable,
            "host failure classification is neither persisted nor model-visible"
        );
    }

    #[test]
    fn compaction_replacement_encodes_unreported_tool_usage_like_its_durable_reconstruction() {
        let result = |usage| AgentMessage::ToolResult {
            id: MessageId(1),
            tool_call_id: ToolCallId::new("usage-free-call").expect("fixture call ID"),
            tool_name: "read".into(),
            content: "contents".into(),
            details: None,
            usage: Box::new(usage),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        };

        assert_eq!(
            encode_compaction_replacement(&[result(None)]).expect("live result encodes"),
            encode_compaction_replacement(&[result(Some(Usage::default()))])
                .expect("durable reconstruction encodes"),
            "`ToolResultEntry` stores a non-optional usage, so unknown usage reopens as all-None"
        );
    }

    #[test]
    fn source_projection_orders_parallel_results_per_owning_assistant() {
        let first_assistant = stored_context_entry(
            "assistant-one",
            1,
            SessionEntry::AssistantMessage(tea_session::AssistantMessageEntry {
                content: "first calls".into(),
                tool_calls: vec![
                    tea_session::AssistantToolCall::new(
                        "reused-call",
                        "first",
                        tea_protocol::JsonValue::Null,
                    ),
                    tea_session::AssistantToolCall::new(
                        "second-call",
                        "second",
                        tea_protocol::JsonValue::Null,
                    ),
                ],
                stop_reason: Some("tool_use".into()),
                error_message: None,
                opaque_context: Vec::new(),
                metadata: BTreeMap::new(),
            }),
        );
        let second_result = stored_tool_result("result-second", 2, "second-call", "second");
        let first_result = stored_tool_result("result-first", 3, "reused-call", "first");
        let second_assistant = stored_context_entry(
            "assistant-two",
            4,
            SessionEntry::AssistantMessage(tea_session::AssistantMessageEntry {
                content: "second calls".into(),
                tool_calls: vec![tea_session::AssistantToolCall::new(
                    "reused-call",
                    "third",
                    tea_protocol::JsonValue::Null,
                )],
                stop_reason: Some("tool_use".into()),
                error_message: None,
                opaque_context: Vec::new(),
                metadata: BTreeMap::new(),
            }),
        );
        let third_result = stored_tool_result("result-third", 5, "reused-call", "third");
        let branch = vec![
            first_assistant,
            second_result,
            first_result,
            second_assistant,
            third_result,
        ];
        let selected = branch
            .iter()
            .map(|entry| entry.header.id.clone())
            .collect::<BTreeSet<_>>();

        let projected = source_ordered_selected_entries(&branch, &selected)
            .into_iter()
            .map(|entry| entry.header.id.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            projected,
            vec![
                "assistant-one",
                "result-first",
                "result-second",
                "assistant-two",
                "result-third",
            ],
        );
    }

    #[test]
    fn durable_compaction_replays_exact_replacement_without_deleting_raw_history() {
        use tea_session::{
            CompactionEntry, MemorySession, ProvisionedEntry, SessionHeader, SessionId,
            SessionWriter,
        };

        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("context-compaction-replay").expect("fixture session ID"),
            "fixture-workspace",
            BTreeMap::new(),
        ))
        .expect("fixture session creates");
        let source_id = EntryId::new("raw-user").expect("fixture source ID");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry {
                    id: source_id.clone(),
                    body: SessionEntry::UserMessage(tea_session::UserMessageEntry {
                        content: "uncompacted original task".into(),
                        metadata: BTreeMap::new(),
                    }),
                },
            )
            .expect("raw source commits");
        let replacement_messages = vec![AgentMessage::User {
            id: MessageId(1),
            content: "exact durable summary".into(),
        }];
        let replacement =
            encode_compaction_replacement(&replacement_messages).expect("replacement encodes");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry {
                    id: EntryId::new("compaction-entry").expect("fixture compaction ID"),
                    body: SessionEntry::Compaction(CompactionEntry {
                        covered_from: Some(source_id.clone()),
                        covered_to: Some(source_id.clone()),
                        retained_tail_boundary: None,
                        summary: "searchable durable summary".into(),
                        strategy_id: "fixture-v1".into(),
                        recovery_index_artifact: None,
                        harness_revision_id: None,
                        replacement: PayloadRef::Inline(replacement.clone()),
                        replacement_digest: compaction_replacement_digest(&replacement)
                            .expect("replacement digest"),
                        provider_request_id: None,
                    }),
                },
            )
            .expect("compaction commits");

        let snapshot = session.snapshot().expect("fixture snapshot");
        let derived = derive_default_snapshot_context(&snapshot, LaneId::main())
            .expect("durable compaction derives");

        assert_eq!(snapshot.entries().len(), 2, "raw history remains append-only");
        assert_eq!(derived.messages, replacement_messages);
        assert_eq!(derived.included_entries.len(), 1);
        assert_eq!(derived.omitted_entries, vec![source_id]);
    }

    fn stored_context_entry(id: &str, sequence: u64, body: SessionEntry) -> StoredEntry {
        StoredEntry {
            lane_id: LaneId::main(),
            header: tea_session::EntryHeader {
                id: EntryId::new(id).expect("fixture entry ID"),
                parent_id: None,
                seq: tea_session::Sequence(sequence),
                timestamp_ms: sequence,
            },
            body,
        }
    }

    fn stored_tool_result(
        id: &str,
        sequence: u64,
        tool_call_id: &str,
        tool_name: &str,
    ) -> StoredEntry {
        stored_context_entry(
            id,
            sequence,
            SessionEntry::ToolResult(tea_session::ToolResultEntry {
                tool_call_id: tool_call_id.into(),
                tool_name: tool_name.into(),
                full_result: PayloadRef::Inline(tea_protocol::JsonValue::String("full".into())),
                model_projection: tea_protocol::JsonValue::object([
                    ("content", tea_protocol::JsonValue::String("result".into())),
                    ("details", tea_protocol::JsonValue::Null),
                ]),
                is_error: false,
                terminate: false,
                usage: tea_session::Usage::default(),
                projection_strategy_id: "fixture".into(),
                artifact_policy_id: tea_session::ArtifactPolicyId::new("fixture-policy")
                    .expect("fixture artifact policy ID"),
            }),
        )
    }
}
