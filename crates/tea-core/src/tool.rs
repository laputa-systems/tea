//! Tool capability boundaries.
//!
//! A tool is an explicit host capability.  The core owns names, schemas, ordering, and result
//! placement; the host owns authority and the actual side effect. Schemas use
//! the stable protocol JSON value, while call arguments retain their exact
//! serialized form for provider correlation. The core validates arguments
//! through a private, replaceable JSON Schema adapter before invoking a tool.

use crate::error::ToolError;
use crate::scheduler::CancellationToken;
use crate::state::{SerializedJson, ToolCallId, Usage};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tea_protocol::JsonValue;

/// A stable, host-owned identifier for one class of retryable capability failure.
///
/// The generic core never derives this value from an error string. Hosts that
/// know a capability has failed (for example, because a transport or process
/// has exited) supply the same signature on equivalent failures.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct FailureSignature(String);

impl FailureSignature {
    /// Construct a non-empty stable failure signature.
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err("failure signature must not be empty");
        }
        Ok(Self(value))
    }

    /// Borrow the host-owned stable value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The model-recovery semantics of a tool failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolFailureDisposition {
    /// Cancellation interrupted the capability; it is never a circuit-breaker failure.
    Cancelled,
    /// Model arguments did not satisfy the registered tool schema.
    InvalidArguments,
    /// The tool failed, but another model turn may choose a recovery action.
    Recoverable,
    /// A temporary capability failure may be retried, subject to a circuit breaker.
    Retryable,
    /// The capability is no longer usable for this run.
    Fatal,
}

/// Structured, host-supplied recovery information attached to an error result.
///
/// This metadata is canonical state. A model-facing projection can expose a
/// bounded representation without changing the raw tool result retained for
/// audit consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolFailure {
    disposition: ToolFailureDisposition,
    signature: Option<FailureSignature>,
    recovery_guidance: Option<String>,
}

impl ToolFailure {
    /// Construct a cancellation classification that remains distinct from a capability failure.
    pub const fn cancelled() -> Self {
        Self {
            disposition: ToolFailureDisposition::Cancelled,
            signature: None,
            recovery_guidance: None,
        }
    }

    /// Construct an invalid-arguments classification.
    pub const fn invalid_arguments() -> Self {
        Self {
            disposition: ToolFailureDisposition::InvalidArguments,
            signature: None,
            recovery_guidance: None,
        }
    }

    /// Construct an ordinary recoverable failure classification.
    pub const fn recoverable() -> Self {
        Self {
            disposition: ToolFailureDisposition::Recoverable,
            signature: None,
            recovery_guidance: None,
        }
    }

    /// Construct a retryable failure with a stable capability signature.
    pub fn retryable(signature: FailureSignature) -> Self {
        Self {
            disposition: ToolFailureDisposition::Retryable,
            signature: Some(signature),
            recovery_guidance: None,
        }
    }

    /// Construct a fatal failure with a stable capability signature.
    pub fn fatal(signature: FailureSignature) -> Self {
        Self {
            disposition: ToolFailureDisposition::Fatal,
            signature: Some(signature),
            recovery_guidance: None,
        }
    }

    /// Attach bounded-by-the-host recovery guidance for the next model turn.
    pub fn with_recovery_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.recovery_guidance = Some(guidance.into());
        self
    }

    /// Return the recovery semantics.
    pub const fn disposition(&self) -> ToolFailureDisposition {
        self.disposition
    }

    /// Return the stable signature, when this failure participates in a circuit breaker.
    pub fn signature(&self) -> Option<&FailureSignature> {
        self.signature.as_ref()
    }

    /// Return host-supplied recovery guidance, when present.
    pub fn recovery_guidance(&self) -> Option<&str> {
        self.recovery_guidance.as_deref()
    }
}

/// Per-agent circuit-breaker configuration. State is allocated per run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToolFailureCircuitBreaker {
    /// End a run once this many consecutive identical retryable failures occur.
    ///
    /// `None` leaves retryable failures recoverable indefinitely. Fatal
    /// failures always end the run after their result has been recorded.
    pub max_consecutive_retryable_failures: Option<std::num::NonZeroU32>,
}

/// Bounded, deterministic model-facing tool-result presentation policy.
///
/// The policy is applied only to a cloned provider context. It never changes
/// canonical `AgentToolResult` or `AgentMessage::ToolResult` values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultProjectionPolicy {
    /// Maximum UTF-8 bytes retained from primary result content.
    pub max_content_bytes: usize,
    /// Maximum UTF-8 bytes retained from serialized host details.
    pub max_details_bytes: usize,
    /// Maximum combined bytes in a model-facing tool result.
    pub max_total_bytes: usize,
    /// Suppress repeated identical error payloads within one projected context.
    pub deduplicate_repeated_errors: bool,
}

impl Default for ToolResultProjectionPolicy {
    fn default() -> Self {
        Self {
            // The built-in workspace/process tools retain up to 50 KiB. Keep the default
            // projection above that size so ordinary large reads remain intact instead of
            // silently dropping the middle of the evidence before the model can inspect it.
            max_content_bytes: 64 * 1024,
            // Keep diagnostic prose from being clipped at the old 16 KiB ceiling. The
            // provider already receives bounded tool content, so give structured details
            // enough room for a complete investigation trace while retaining a finite total.
            max_details_bytes: 64 * 1024,
            max_total_bytes: 128 * 1024,
            deduplicate_repeated_errors: true,
        }
    }
}

impl ToolResultProjectionPolicy {
    /// Reject limits that cannot represent the deterministic truncation marker.
    pub fn validate(&self) -> Result<(), &'static str> {
        const LONGEST_ERROR_STATUS: &str = "[tool error status: invalid_arguments]";
        let minimum_error_total = LONGEST_ERROR_STATUS
            .len()
            .saturating_add(1)
            .saturating_add(truncation_marker().len());
        if self.max_content_bytes < truncation_marker().len()
            || self.max_details_bytes < truncation_marker().len()
            || self.max_total_bytes < minimum_error_total
        {
            return Err(
                "tool result projection limits must preserve an error status and truncation marker",
            );
        }
        Ok(())
    }
}

/// A curated tool result for a provider representation that has no structured-details field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelToolResult {
    /// Bounded text including marked fallback metadata where necessary.
    pub content: String,
    /// Model-visible error state.
    pub is_error: bool,
}

/// Curate one raw result for a text-only provider tool-result representation.
///
/// `seen_error_payloads` is caller-owned so duplicate suppression is scoped to
/// one context projection rather than durable agent state.
pub fn project_tool_result_as_text(
    content: &str,
    details: Option<&crate::state::SerializedJson>,
    is_error: bool,
    failure: Option<&ToolFailure>,
    policy: &ToolResultProjectionPolicy,
    seen_error_payloads: &mut BTreeMap<u64, ()>,
) -> ModelToolResult {
    debug_assert!(policy.validate().is_ok());
    let fingerprint = error_fingerprint(content, details, failure);
    if is_error
        && policy.deduplicate_repeated_errors
        && seen_error_payloads.contains_key(&fingerprint)
    {
        return ModelToolResult {
            content: "[repeated tool error omitted; see the earlier matching result]".into(),
            is_error: true,
        };
    }
    if is_error {
        seen_error_payloads.insert(fingerprint, ());
    }

    let mut projected = String::new();
    if is_error {
        let status = failure
            .map(|failure| match failure.disposition() {
                ToolFailureDisposition::Cancelled => "cancelled",
                ToolFailureDisposition::InvalidArguments => "invalid_arguments",
                ToolFailureDisposition::Recoverable => "recoverable",
                ToolFailureDisposition::Retryable => "retryable",
                ToolFailureDisposition::Fatal => "fatal",
            })
            .unwrap_or("recoverable");
        append_projected_section(
            &mut projected,
            &format!("[tool error status: {status}]"),
            policy.max_total_bytes,
        );
        if let Some(guidance) = failure.and_then(ToolFailure::recovery_guidance) {
            append_projected_section(
                &mut projected,
                &format!(
                    "[recovery guidance: {}]",
                    truncate_middle(guidance, policy.max_details_bytes)
                ),
                policy.max_total_bytes,
            );
        }
    }
    append_projected_section(
        &mut projected,
        &truncate_middle(content, policy.max_content_bytes),
        policy.max_total_bytes,
    );
    if let Some(details) = details {
        append_projected_section(
            &mut projected,
            &format!(
                "[tool details (serialized JSON): {}]",
                truncate_middle(details.as_str(), policy.max_details_bytes)
            ),
            policy.max_total_bytes,
        );
    }
    ModelToolResult {
        content: projected,
        is_error,
    }
}

/// Add one bounded provider-visible section without allowing payload text to
/// displace a preceding error-status header.
fn append_projected_section(output: &mut String, section: &str, total_limit: usize) {
    if section.is_empty() {
        return;
    }
    let separator_bytes = usize::from(!output.is_empty());
    let available = total_limit.saturating_sub(output.len());
    if available <= separator_bytes {
        return;
    }
    if separator_bytes != 0 {
        output.push('\n');
    }
    output.push_str(&truncate_middle(section, available - separator_bytes));
}

/// The explicit marker inserted by deterministic oversized-result truncation.
pub const fn truncation_marker() -> &'static str {
    "… [truncated] …"
}

/// Preserve useful UTF-8 prefix and suffixes within a byte budget.
pub fn truncate_middle(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.into();
    }
    let marker = truncation_marker();
    if limit <= marker.len() {
        return marker[..utf8_prefix_end(marker, limit)].into();
    }
    let remaining = limit - marker.len();
    let prefix_limit = remaining / 2;
    let suffix_limit = remaining - prefix_limit;
    let prefix_end = utf8_prefix_end(value, prefix_limit);
    let suffix_start = utf8_suffix_start(value, suffix_limit);
    format!(
        "{}{}{}",
        &value[..prefix_end],
        marker,
        &value[suffix_start..]
    )
}

fn utf8_prefix_end(value: &str, limit: usize) -> usize {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn utf8_suffix_start(value: &str, limit: usize) -> usize {
    let mut start = value.len().saturating_sub(limit);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    start
}

fn error_fingerprint(
    content: &str,
    details: Option<&crate::state::SerializedJson>,
    failure: Option<&ToolFailure>,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for value in [
        content,
        details
            .map(crate::state::SerializedJson::as_str)
            .unwrap_or_default(),
        failure
            .and_then(ToolFailure::signature)
            .map(FailureSignature::as_str)
            .unwrap_or_default(),
    ] {
        for byte in value.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    if let Some(failure) = failure {
        let disposition = match failure.disposition() {
            ToolFailureDisposition::Cancelled => 1,
            ToolFailureDisposition::InvalidArguments => 2,
            ToolFailureDisposition::Recoverable => 3,
            ToolFailureDisposition::Retryable => 4,
            ToolFailureDisposition::Fatal => 5,
        };
        hash ^= disposition;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        if let Some(guidance) = failure.recovery_guidance() {
            for byte in guidance.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

/// A boxed future used so callers may drive tools on their own executor.
pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AgentToolResult, ToolError>> + Send + 'a>>;

/// Whether calls to a tool may overlap within one assistant message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolExecutionMode {
    /// The scheduler must await this call before starting another call in the batch.
    Sequential,
    /// The scheduler may execute this call concurrently with other parallel calls.
    #[default]
    Parallel,
}

/// An assistant-requested tool invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    /// Stable call identifier.
    pub id: ToolCallId,
    /// Registered capability name.
    pub name: String,
    /// Serialized JSON arguments.
    pub arguments: SerializedJson,
}

/// A tool result to be inserted into model context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentToolResult {
    /// Call to which the result belongs.
    pub tool_call_id: ToolCallId,
    /// Serialized result content.
    pub content: String,
    /// Optional serialized host details.
    pub details: Option<SerializedJson>,
    /// Optional provider/accounting usage attached by the capability.
    pub usage: Option<Usage>,
    /// Names of capabilities added for a later model request, when an explicit
    /// host policy supports dynamic tool exposure.
    pub added_tool_names: Vec<String>,
    /// Whether this finalized result asks to stop after the current batch.
    ///
    /// The scheduler stops before another model request only when every
    /// finalized call in the batch has this flag set. An after-tool hook may
    /// replace the flag explicitly.
    pub terminate: bool,
    /// Whether the result represents a tool failure.
    pub is_error: bool,
    /// Optional typed failure and recovery metadata supplied by the host.
    ///
    /// Successful results must leave this as `None`. The generic core uses the
    /// classification only for its run-local circuit breaker; it never parses
    /// result text to infer transport or capability state.
    pub failure: Option<ToolFailure>,
}

/// A partial update emitted during tool execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUpdate {
    /// Human/model-visible update content.
    pub content: String,
    /// Optional serialized host details.
    pub details: Option<SerializedJson>,
}

/// A cancellation handle shared by model and tool operations.
#[derive(Clone, Default)]
pub struct ToolUpdateSink {
    callback: Option<Arc<dyn Fn(ToolUpdate) + Send + Sync>>,
}

impl std::fmt::Debug for ToolUpdateSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolUpdateSink")
            .field("enabled", &self.callback.is_some())
            .finish()
    }
}

impl ToolUpdateSink {
    /// Create a sink that discards updates.
    pub const fn disabled() -> Self {
        Self { callback: None }
    }

    /// Create a sink backed by a host callback.
    pub fn new(callback: impl Fn(ToolUpdate) + Send + Sync + 'static) -> Self {
        Self {
            callback: Some(Arc::new(callback)),
        }
    }

    /// Deliver one update to the host, if configured.
    pub fn emit(&self, update: ToolUpdate) {
        if let Some(callback) = &self.callback {
            callback(update);
        }
    }
}

/// Context supplied to an explicit tool capability.
#[derive(Clone, Debug)]
pub struct ToolContext {
    /// Cancellation state owned by the run.
    pub cancellation: CancellationToken,
    /// Arbitrary serialized host metadata for this execution.
    pub metadata: Option<SerializedJson>,
}

/// How the scheduler settles a started tool after run cancellation.
///
/// Most tools use [`Self::DropFuture`], preserving Tea's prompt cancellation
/// behavior. A host-side transaction may opt into [`Self::AwaitFuture`] only
/// when dropping its future after commit request could conceal a durable
/// outcome. Such a tool must observe cancellation before its commit point and
/// must eventually settle a receipt; this mode is trusted and can delay run
/// cancellation if an adapter violates that contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CancellationSettlementMode {
    /// Drop a pending future and synthesize a cancelled tool result.
    #[default]
    DropFuture,
    /// Keep polling a started future until it returns a terminal receipt.
    AwaitFuture,
}

/// A registered executable capability.
pub trait AgentTool: Send + Sync {
    /// Stable tool name used by assistant calls.
    fn name(&self) -> &str;
    /// Prompt-facing description.
    fn description(&self) -> &str;
    /// Raw JSON Schema-compatible value for arguments.
    ///
    /// This intentionally uses the protocol JSON representation rather than a
    /// Rust schema DSL or a Serde value. The validator adapter remains private
    /// to the core and must not leak its dependency types here.
    fn schema(&self) -> &JsonValue;
    /// Borrow the parameter schema using upstream Pi's terminology.
    ///
    /// `schema` remains the wire-oriented Rust spelling; both methods expose
    /// the same value and neither performs validation.
    fn parameters(&self) -> &JsonValue {
        self.schema()
    }
    /// Execution ordering policy.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }
    /// Whether this capability may run only as the sole call in one assistant
    /// tool batch.
    ///
    /// This is a scheduling boundary for host-owned transactional tools.  The
    /// run rejects the entire batch before any sibling capability starts when
    /// an exclusive tool appears beside another call.  Ordinary tools remain
    /// composable by default.
    fn requires_exclusive_batch(&self) -> bool {
        false
    }
    /// Select how a started future settles after the enclosing run is
    /// cancelled. This exceptional mode is intended for transactional host
    /// boundaries and defaults to immediate future dropping.
    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        CancellationSettlementMode::DropFuture
    }
    /// Execute the call on the caller-owned executor.
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a>;
}

/// Prompt-facing, non-executable description of a tool.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    /// Stable tool name.
    pub name: String,
    /// Description supplied to the model.
    pub description: String,
    /// Raw JSON Schema-compatible value.
    pub schema: JsonValue,
    /// Scheduling mode.
    pub execution_mode: ToolExecutionMode,
}

impl ToolDefinition {
    /// Build a definition from a capability.
    pub fn from_tool(tool: &dyn AgentTool) -> Self {
        Self {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            schema: tool.schema().clone(),
            execution_mode: tool.execution_mode(),
        }
    }

    /// Borrow the parameter schema using upstream Pi's terminology.
    pub fn parameters(&self) -> &JsonValue {
        &self.schema
    }
}

/// Ordered registry of explicit tools.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    order: Vec<String>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("names", &self.order)
            .finish()
    }
}

impl ToolRegistry {
    /// Add a tool, replacing an existing tool with the same name without changing order.
    pub fn insert(&mut self, tool: Arc<dyn AgentTool>) {
        let name = tool.name().to_owned();
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Remove a named tool and return it to the caller.
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn AgentTool>> {
        let removed = self.tools.remove(name);
        if removed.is_some() {
            self.order.retain(|entry| entry != name);
        }
        removed
    }

    /// Find an executable tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn AgentTool>> {
        self.tools.get(name)
    }

    /// Return registered names in prompt/source order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(String::as_str)
    }

    /// Return prompt definitions in registry order.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| ToolDefinition::from_tool(tool.as_ref()))
            .collect()
    }

    /// Whether no capabilities are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
