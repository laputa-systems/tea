//! Virtual models: a selected model that routes each request to a physical
//! model from a host-approved set.
//!
//! Adapted from upstream Pi's `core/virtual-models.ts`. The selection (the
//! agent's model descriptor) may name a virtual model; everything below the
//! routing step sees only physical models. A routed request carries the
//! physical model in [`crate::scheduler::ModelRequest::model`] and the
//! selection in `selected_model`, so the provider, capabilities, context
//! limits, continuation data, cache eligibility, and accounting follow the
//! physical dispatch, and the assistant message records it as its origin.
//!
//! Tea is stricter than Pi in three ways:
//!
//! - **Approved targets.** A router may only pick a target the host approved
//!   for that virtual model; anything else fails the run clearly instead of
//!   silently choosing another provider.
//! - **Sticky continuations.** Tool continuations and retries stay on the
//!   physical model already used in the run unless the virtual model's
//!   explicit [`ContinuationPolicy::Routed`] asks the router again.
//! - **Bounded state.** Router state is one JSON value persisted through the
//!   host (the extension's bounded per-lane state in durable sessions), so it
//!   follows branches and reopen. The router itself never touches storage.
//!
//! Cache-maintenance replays reuse the admitted physical request and never
//! call the router.

use crate::state::{AgentMessage, ModelDescriptor, ThinkingLevel};
use std::sync::Arc;
use tea_protocol::JsonValue;

/// Provider identity of every virtual selection.
pub const VIRTUAL_PROVIDER: &str = "virtual";

/// Why a request is being routed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteReason {
    /// The first request after user input.
    User,
    /// A request after tool results or other non-user messages.
    Continuation,
    /// A retry after a failed request (for example after overflow compaction).
    Retry,
}

impl RouteReason {
    /// Stable lowercase label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Continuation => "continuation",
            Self::Retry => "retry",
        }
    }
}

/// Whether continuations consult the router.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContinuationPolicy {
    /// Continuations and retries stay on the run's current physical model.
    #[default]
    Sticky,
    /// Every request is routed (an explicit, authorized policy choice).
    Routed,
}

/// Everything a router may consider for one request.
#[derive(Debug)]
pub struct RouteRequest<'a> {
    /// The selected virtual model.
    pub selected: &'a ModelDescriptor,
    /// The selected reasoning level.
    pub thinking_level: ThinkingLevel,
    /// Why this request is routed.
    pub reason: RouteReason,
    /// Physical model of the latest successful response, if any.
    pub previous: Option<&'a ModelDescriptor>,
    /// Host-approved targets, in approval order.
    pub targets: &'a [ModelDescriptor],
    /// Router state last stored on this branch.
    pub state: Option<&'a JsonValue>,
    /// The canonical conversation for this request.
    pub messages: &'a [AgentMessage],
}

/// A router's decision.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    /// Physical model to dispatch; must be one of the approved targets.
    pub target: ModelDescriptor,
    /// Reasoning level for the physical request, when the router changes it.
    pub thinking_level: Option<ThinkingLevel>,
    /// Replacement state, when it changed.
    pub state: Option<JsonValue>,
}

/// A router failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteError {
    /// Bounded diagnostic.
    pub message: String,
}

impl RouteError {
    /// Construct a router failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Deterministic request-aware routing. Implementations must be cheap and
/// must not perform I/O; classifier calls and speculative dispatch are out of
/// scope.
pub trait ModelRouter: Send + Sync {
    /// Pick the physical model for one request.
    fn route(&self, request: &RouteRequest<'_>) -> Result<Route, RouteError>;
}

/// One selectable virtual model for a run.
#[derive(Clone)]
pub struct VirtualModel {
    /// Selection identity (`provider` is [`VIRTUAL_PROVIDER`]).
    pub descriptor: ModelDescriptor,
    /// Display name.
    pub name: String,
    /// Host-approved physical targets, in approval order.
    pub targets: Vec<ModelDescriptor>,
    /// Whether continuations consult the router.
    pub continuations: ContinuationPolicy,
    /// The router.
    pub router: Arc<dyn ModelRouter>,
    /// Persistent state namespace (the owning extension's identity), when the
    /// router keeps state.
    pub state_namespace: Option<String>,
    /// Latest persisted state at the start of this run.
    pub state: Option<JsonValue>,
}

impl std::fmt::Debug for VirtualModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualModel")
            .field("descriptor", &self.descriptor)
            .field("targets", &self.targets)
            .field("continuations", &self.continuations)
            .field("state_namespace", &self.state_namespace)
            .finish_non_exhaustive()
    }
}

impl VirtualModel {
    /// Whether `model` is an approved target.
    pub fn approves(&self, model: &ModelDescriptor) -> bool {
        self.targets.iter().any(|target| target == model)
    }
}

/// Physical model of the latest successful assistant response.
pub fn latest_physical(messages: &[AgentMessage]) -> Option<&ModelDescriptor> {
    messages.iter().rev().find_map(|message| match message {
        AgentMessage::Assistant {
            origin: Some(origin),
            stop_reason,
            ..
        } if !matches!(
            stop_reason,
            Some(crate::state::StopReason::Error | crate::state::StopReason::Cancelled)
        ) =>
        {
            Some(origin)
        }
        _ => None,
    })
}

/// Parse `provider/model` (the model part may itself contain `/`).
pub fn parse_descriptor(value: &str) -> Option<ModelDescriptor> {
    let (provider, model) = value.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then(|| ModelDescriptor {
        provider: provider.to_owned(),
        model: model.to_owned(),
        revision: None,
    })
}

/// Format a descriptor as `provider/model`.
pub fn format_descriptor(model: &ModelDescriptor) -> String {
    format!("{}/{}", model.provider, model.model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_round_trip_with_nested_model_paths() {
        let model = parse_descriptor("openrouter/openai/gpt-5.6-luna").expect("parses");
        assert_eq!(model.provider, "openrouter");
        assert_eq!(model.model, "openai/gpt-5.6-luna");
        assert_eq!(format_descriptor(&model), "openrouter/openai/gpt-5.6-luna");
        assert!(parse_descriptor("no-slash").is_none());
        assert!(parse_descriptor("/model").is_none());
    }
}
