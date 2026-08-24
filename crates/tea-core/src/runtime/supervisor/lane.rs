//! Volatile state owned by one durable session lane.

use crate::agent::Agent;
use crate::runtime::RuntimeServices;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tea_core::state::ThinkingLevel;
use tea_session::LaneId;

/// Process-local execution state for exactly one durable lane.
///
/// All durable state remains in the session reducer. This object deliberately
/// keeps only executable authority and live observations that must never be
/// shared with another lane.
pub(crate) struct LaneRuntime {
    pub(crate) lane_id: LaneId,
    pub(crate) active: AtomicBool,
    /// Root-only sticky cancellation requested after durable acceptance but
    /// potentially before a core epoch installs its live agent. This remains
    /// process-local; the operation WAL still owns durable recovery.
    pub(crate) abort_requested: AtomicBool,
    pub(crate) active_agent: Mutex<Option<Agent>>,
    pub(crate) thinking_level: Mutex<ThinkingLevel>,
    pub(crate) runtime_services: RuntimeServices,
    pub(crate) prompt_layout_ledger: Arc<crate::measurement::PromptLayoutLedger>,
}

impl LaneRuntime {
    pub(crate) fn new(lane_id: LaneId, runtime_services: RuntimeServices) -> Self {
        let prompt_layout_ledger = Arc::new(
            crate::measurement::PromptLayoutLedger::new(runtime_services.prompt_layout_scope())
                .policy(runtime_services.prompt_layout_policy_value()),
        );
        let thinking_level = runtime_services.thinking_level_value();
        Self {
            lane_id,
            active: AtomicBool::new(false),
            abort_requested: AtomicBool::new(false),
            active_agent: Mutex::new(None),
            thinking_level: Mutex::new(thinking_level),
            runtime_services,
            prompt_layout_ledger,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measurement::PromptContinuity;
    use crate::scheduler::{CancellationToken, ModelFuture, ModelProvider, ModelRequest};
    use crate::state::ModelDescriptor;
    use crate::tool::ToolRegistry;

    struct UnusedProvider;

    impl ModelProvider for UnusedProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            panic!("lane-ledger test never dispatches a provider request")
        }
    }

    fn request(context: &str) -> ModelRequest {
        ModelRequest {
            system_prompt: "stable child prompt".into(),
            context: context.into(),
            tools: Vec::new(),
            model: Some(ModelDescriptor {
                provider: "fixture".into(),
                model: "child".into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::Off,
        }
    }

    #[test]
    fn every_lane_owns_a_distinct_prompt_layout_ledger() {
        let services = RuntimeServices::new(Arc::new(UnusedProvider), ToolRegistry::default());
        let root = LaneRuntime::new(LaneId::main(), services.clone());
        let first_child = LaneRuntime::new(
            LaneId::new("agent-first").expect("valid child lane ID"),
            services.clone(),
        );
        let second_child = LaneRuntime::new(
            LaneId::new("agent-second").expect("valid child lane ID"),
            services,
        );

        assert!(
            !Arc::ptr_eq(
                &root.prompt_layout_ledger,
                &first_child.prompt_layout_ledger
            ) && !Arc::ptr_eq(
                &first_child.prompt_layout_ledger,
                &second_child.prompt_layout_ledger
            )
        );
        assert_eq!(
            root.prompt_layout_ledger
                .observe(&request("root"))
                .continuity,
            PromptContinuity::FirstRequest
        );
        assert_eq!(
            first_child
                .prompt_layout_ledger
                .observe(&request("first child"))
                .continuity,
            PromptContinuity::FirstRequest
        );
        assert_eq!(
            second_child
                .prompt_layout_ledger
                .observe(&request("second child"))
                .continuity,
            PromptContinuity::FirstRequest,
            "a second child never inherits another lane's predecessor"
        );
    }
}
