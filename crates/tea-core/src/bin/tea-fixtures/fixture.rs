use tea_core::queue::QueueMode;
use tea_core::scheduler::ModelStream;
use tea_core::state::{ModelDescriptor, SerializedJson, StopReason, ThinkingLevel};
use tea_core::tool::ToolExecutionMode;

/// Parsed usage values retained for the canonical fixture result.
#[derive(Clone, Debug)]
pub(super) struct FixtureUsage {
    pub(super) input: u64,
    pub(super) output: u64,
    pub(super) cache_read: u64,
    pub(super) cache_write: u64,
    pub(super) total_tokens: u64,
}

#[derive(Clone, Debug)]
pub(super) struct FixtureToolResponse {
    pub(super) arguments: SerializedJson,
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) yield_once: bool,
    pub(super) updates: Vec<String>,
    pub(super) cancel_after_update: bool,
    pub(super) enqueue_during_execution: Option<FixtureActiveQueueArrival>,
    pub(super) terminate: bool,
}

/// A host fixture message injected only while the corresponding tool call is active.
/// The directive gives queue drains a deterministic source without a clock or background task.
#[derive(Clone, Debug)]
pub(super) enum FixtureActiveQueueArrival {
    Steer(String),
    FollowUp(String),
}

#[derive(Clone, Debug)]
pub(super) struct FixtureToolSpec {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) execution_mode: ToolExecutionMode,
    pub(super) parameters: SerializedJson,
    pub(super) responses: Vec<FixtureToolResponse>,
}

#[derive(Clone, Debug)]
pub(super) struct Fixture {
    pub(super) id: String,
    pub(super) system_prompt: String,
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) thinking_level: ThinkingLevel,
    pub(super) steering_mode: QueueMode,
    pub(super) follow_up_mode: QueueMode,
    pub(super) actions: Vec<FixtureAction>,
    pub(super) before_tool_policy: Option<FixtureBeforeToolPolicy>,
    pub(super) after_tool_replace: Option<FixtureAfterToolReplace>,
    pub(super) context_hooks: Option<FixtureContextHooks>,
    pub(super) should_stop_after_turn: bool,
    pub(super) hold_agent_end_observer: bool,
    pub(super) tools: Vec<FixtureToolSpec>,
    pub(super) streams: Vec<FixtureModelStream>,
    pub(super) last_usage: FixtureUsage,
    pub(super) last_stop_reason: StopReason,
}

/// One deterministic model turn, including adapter-only cancellation control.
///
/// The core provider contract intentionally receives a finite `ModelStream` in
/// this v1 harness. A cancellation checkpoint therefore rewrites the fixture
/// stream at parse time and marks the caller-owned token before returning it;
/// both adapters still expose the same partial response and aborted terminal
/// lifecycle without relying on wall-clock scheduling.
#[derive(Clone, Debug)]
pub(super) struct FixtureModelStream {
    pub(super) stream: ModelStream,
    pub(super) cancel_after_text_delta: bool,
}

#[derive(Clone, Debug)]
pub(super) struct FixtureBeforeToolPolicy {
    pub(super) tool_name: String,
    pub(super) reason: String,
    pub(super) terminate: bool,
    pub(super) yield_once: bool,
    pub(super) cancel_after_yield: bool,
}

#[derive(Clone, Debug)]
pub(super) struct FixtureAfterToolReplace {
    pub(super) tool_name: String,
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) terminate: Option<bool>,
}

#[derive(Clone, Debug)]
pub(super) struct FixtureContextHooks {
    pub(super) host_messages: Vec<String>,
    pub(super) transform_append_host_message: String,
    pub(super) convert_prefix: String,
    pub(super) next_host_messages: Vec<String>,
    pub(super) next_model: ModelDescriptor,
    pub(super) next_thinking_level: ThinkingLevel,
}

#[derive(Clone, Debug)]
pub(super) enum FixtureAction {
    Steer(String),
    FollowUp(String),
    Prompt(String),
    Continue,
}
