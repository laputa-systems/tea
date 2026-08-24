//! Public policy values and private VM state.

use mlua::{Function, Lua};
use std::fmt;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use tea_core::tool::{CancellationSettlementMode, ToolExecutionMode};
use tea_protocol::JsonValue;

/// One named v1 prompt contribution owned by a policy bundle.
///
/// The host namespaces this ID with the immutable plugin identity during
/// harness composition.  A policy cannot append revision IDs, paths, or other
/// runtime churn through this structure; it contributes only its exact stable
/// source text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyPromptSection {
    /// Bundle-local stable section label.
    pub id: String,
    /// Exact model-visible section text.
    pub content: String,
}

/// Resource limits applied to one Lua policy virtual machine.
///
/// `max_interrupt_checks` bounds initial policy evaluation and each hook
/// invocation separately. Luau invokes the interrupt handler at loop and
/// function-call boundaries, so the value is a deterministic host budget
/// rather than an exact instruction count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyLimits {
    /// Largest accepted policy source in bytes.
    pub max_source_bytes: usize,
    /// Largest Luau VM allocation total in bytes.
    pub max_memory_bytes: usize,
    /// Largest number of Luau interrupt checks permitted per evaluation.
    pub max_interrupt_checks: usize,
}

impl Default for PolicyLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        }
    }
}

/// A prompt-facing tool declared by a policy but not granted any authority.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyTool {
    /// Stable tool name sent to the model.
    pub name: String,
    /// Prompt-facing explanation of the tool.
    pub description: String,
    /// JSON Schema for the tool arguments.
    pub schema: JsonValue,
    /// Host-owned capability name that must be explicitly bound by an embedder.
    pub capability: String,
    /// Whether the core may overlap calls to this tool.
    pub execution_mode: ToolExecutionMode,
    /// Whether this tool must be the sole call in an assistant batch.
    pub requires_exclusive_batch: bool,
    /// How a started invocation settles after run cancellation.
    pub cancellation_settlement_mode: CancellationSettlementMode,
    /// Optional self-contained Luau source for this tool's coroutine handler.
    ///
    /// The source must evaluate to a function accepted by
    /// [`tool_handler::LuaToolHandler`]. It remains inert until an embedding
    /// deliberately adapts it into an explicit Rust capability; declaring a
    /// handler never grants a world effect by itself.
    pub handler_source: Option<String>,
}

/// Visibility requested by a bounded ABI-v1 plugin-memory proposal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyMemoryVisibility {
    /// The host context policy may later select the entry for model context.
    ModelVisible,
    /// The entry remains durable and queryable, but is never automatically
    /// serialized into model context.
    ExternalOnly,
}

/// Retention requested by a bounded ABI-v1 plugin-memory proposal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyMemoryRetention {
    /// Retain under ordinary session/export reachability rules.
    Session,
    /// Retain only while a host checkpoint retains the entry.
    Checkpoint,
}

/// One policy proposal for Rust to append a typed semantic memory entry.
///
/// The plugin cannot provide an entry ID, parent pointer, artifact ID, or
/// session writer. The host fills plugin identity and durable placement only
/// after retaining the raw tool result that caused this proposal.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyMemoryProposal {
    /// Plugin-defined, portable memory kind.
    pub kind: String,
    /// Bounded structured inline content.
    pub content: JsonValue,
    /// Bounded evidence labels validated by the host before persistence.
    pub provenance: Vec<String>,
    /// Requested context visibility.
    pub visibility: PolicyMemoryVisibility,
    /// Requested retention class.
    pub retention: PolicyMemoryRetention,
}

/// The complete output of one ABI-v1 post-tool policy callback.
///
/// [`tea_core::hooks::AfterToolCall`] remains the only part that changes a
/// core transcript. A memory proposal is a separate Rust-owned semantic
/// append that is not visible to `tea-core` and cannot alter the completed
/// external effect.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyAfterToolOutput {
    /// Bounded model-facing replacement fields.
    pub projection: tea_core::hooks::AfterToolCall,
    /// Optional typed memory proposal awaiting host validation.
    pub memory: Option<PolicyMemoryProposal>,
}

/// One metadata-only semantic entry exposed to an ABI-v1 context policy.
///
/// The policy receives neither message bodies nor raw tool/artifact payloads.
/// It can reason only over stable entry identities, broad semantic kinds, and
/// the Rust-owned protected/model-visible classifications.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyContextEntry {
    /// Stable immutable semantic entry identity.
    pub id: String,
    /// Broad host-owned entry class such as `user`, `assistant`, or `tool`.
    pub kind: String,
    /// Whether Rust considers this entry eligible for model projection.
    pub model_visible: bool,
    /// Whether Rust will reject an attempt to omit this entry.
    pub protected: bool,
}

/// Bounded, metadata-only context view passed to one policy hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyContextInput {
    /// Ordered entries from the immutable lane branch.
    pub entries: Vec<PolicyContextEntry>,
}

/// A bounded model-facing annotation proposed by a context policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyContextAnnotation {
    /// Bundle-local stable annotation ID.
    pub id: String,
    /// Exact bounded annotation content.
    pub content: String,
}

/// An ABI-v1 policy's typed context-projection proposal.
///
/// Rust maps these opaque IDs to its immutable semantic tree and validates
/// every root, pairing, recovery, and provider-limit invariant before a
/// provider request is built. Returning this value cannot append, delete, or
/// rewrite any durable entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PolicyContextProjectionPatch {
    /// Explicit entries to retain; an empty list leaves host default selection.
    pub retain_entries: Vec<String>,
    /// Model-eligible entries to remove only from this request's projection.
    pub omit_eligible_entries: Vec<String>,
    /// Policy-local annotations appended after selected semantic entries.
    pub annotations: Vec<PolicyContextAnnotation>,
    /// Model-visible plugin-memory entries to select explicitly.
    pub selected_memory: Vec<String>,
    /// A registered compaction strategy request; Rust decides whether to run it.
    pub requested_compaction_strategy: Option<String>,
}

/// A loaded, sandboxed Luau policy.
pub struct LuaPolicy {
    pub(super) runtime: Mutex<PolicyRuntime>,
    pub(super) prompt_sections: Vec<PolicyPromptSection>,
    pub(super) tools: Vec<PolicyTool>,
}

pub(super) struct PolicyRuntime {
    pub(super) lua: Lua,
    pub(super) before_tool_call: Option<Function>,
    /// Optional ABI-v1 model-projection hook. It receives only the completed
    /// model-facing result and can never alter the durable raw result or its
    /// usage accounting.
    pub(super) after_tool_call: Option<Function>,
    /// Optional ABI-v1 metadata-only context-projection policy.
    pub(super) context_projection: Option<Function>,
    /// ABI-v1 lifecycle callbacks keyed by a bundle-local stable registration
    /// ID. These callbacks are deliberately retained inside the policy VM:
    /// Rust owns the resulting durable record and a policy never receives a
    /// session writer or a shared plugin-memory object.
    pub(super) resume_hooks: Vec<PolicyResumeHook>,
    pub(super) interrupt_budget: Arc<AtomicUsize>,
    pub(super) max_interrupt_checks: usize,
}

/// One stable ABI-v1 lifecycle registration.
///
/// A single registration may contribute independent operation and epoch
/// state, then receive exactly those two values during recovery. Keeping the
/// three callbacks together gives Rust one durable registration identity and
/// prevents a hook from reading another plugin's state by naming it.
pub(super) struct PolicyResumeHook {
    pub(super) id: String,
    pub(super) before_operation: Option<Function>,
    pub(super) before_epoch: Option<Function>,
    pub(super) before_resume: Option<Function>,
}

/// A policy loading or evaluation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyError {
    /// The source exceeds the configured boundary before entering the VM.
    SourceTooLarge {
        /// Source length in bytes.
        actual: usize,
        /// Configured maximum length in bytes.
        limit: usize,
    },
    /// A configured VM resource limit is zero.
    InvalidLimit {
        /// Stable configuration field name.
        field: &'static str,
    },
    /// The extension failed to meet the policy-table contract.
    Contract {
        /// Searchable explanation of the mismatch.
        message: String,
    },
    /// The Luau VM rejected or interrupted evaluation.
    Runtime {
        /// Host-safe diagnostic from the Luau VM.
        message: String,
    },
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceTooLarge { actual, limit } => {
                write!(
                    formatter,
                    "policy source is {actual} bytes, exceeding {limit} bytes"
                )
            }
            Self::InvalidLimit { field } => {
                write!(formatter, "policy limit {field} must be non-zero")
            }
            Self::Contract { message } => write!(formatter, "invalid policy contract: {message}"),
            Self::Runtime { message } => write!(formatter, "Luau policy failed: {message}"),
        }
    }
}

impl std::error::Error for PolicyError {}
