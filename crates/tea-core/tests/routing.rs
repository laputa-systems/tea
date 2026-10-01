//! Virtual-model routing through the real agent loop.

use std::sync::{Arc, Mutex};
use tea_core::Agent;
use tea_core::error::CoreError;
use tea_core::routing::{
    ContinuationPolicy, ModelRouter, Route, RouteError, RouteReason, RouteRequest, VirtualModel,
    VIRTUAL_PROVIDER,
};
use tea_core::state::{AgentMessage, ModelDescriptor, ThinkingLevel};
use tea_core::testing::{ScriptedProvider, ScriptedTurn};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use tea_protocol::JsonValue;

fn model(provider: &str, id: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.into(),
        model: id.into(),
        revision: None,
    }
}

fn planner() -> ModelDescriptor {
    model("fixture", "planner")
}

fn builder() -> ModelDescriptor {
    model("fixture", "builder")
}

fn selection() -> ModelDescriptor {
    model(VIRTUAL_PROVIDER, "plan-build")
}

/// Plans on the planner until an `edit` succeeds, then builds on the builder.
/// Records each call's reason.
struct PlanBuild {
    calls: Mutex<Vec<(RouteReason, Option<String>)>>,
    target_override: Option<ModelDescriptor>,
}

impl ModelRouter for PlanBuild {
    fn route(&self, request: &RouteRequest<'_>) -> Result<Route, RouteError> {
        let phase = request
            .state
            .and_then(|state| state.get("phase"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        self.calls
            .lock()
            .expect("calls")
            .push((request.reason, phase.clone()));
        if let Some(target) = &self.target_override {
            return Ok(Route {
                target: target.clone(),
                thinking_level: None,
                state: None,
            });
        }
        let edited = request.messages.iter().any(|message| {
            matches!(message, AgentMessage::ToolResult { tool_name, is_error: false, .. } if tool_name == "edit")
        });
        let building = phase.as_deref() == Some("build") || edited;
        Ok(Route {
            target: if building { builder() } else { planner() },
            thinking_level: Some(if building {
                ThinkingLevel::Low
            } else {
                ThinkingLevel::High
            }),
            state: Some(JsonValue::object([(
                "phase",
                JsonValue::from(if building { "build" } else { "plan" }),
            )])),
        })
    }
}

struct Edit {
    schema: JsonValue,
}

impl AgentTool for Edit {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "edit"
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "edited".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

fn virtual_model(router: Arc<PlanBuild>, continuations: ContinuationPolicy) -> VirtualModel {
    VirtualModel {
        descriptor: selection(),
        name: "Plan, then build".into(),
        targets: vec![planner(), builder()],
        continuations,
        router,
        state_namespace: Some("plan-build".into()),
        state: None,
    }
}

fn agent(provider: &ScriptedProvider, virtual_models: Vec<VirtualModel>) -> Agent {
    Agent::builder()
        .system_prompt("p")
        .model(selection())
        .thinking_level(ThinkingLevel::Medium)
        .tool(Arc::new(Edit {
            schema: JsonValue::object([("type", JsonValue::from("object"))]),
        }))
        .virtual_models(virtual_models)
        .model_provider(Arc::new(provider.clone()))
        .build()
}

#[test]
fn a_user_turn_is_routed_and_its_tool_continuation_stays_on_the_same_physical_model() {
    let router = Arc::new(PlanBuild {
        calls: Mutex::new(Vec::new()),
        target_override: None,
    });
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().tool_call("call-edit", "edit", "{}").end_tool_use(),
        ScriptedTurn::new().text("planned and edited").stop(),
        ScriptedTurn::new().text("built").stop(),
    ]);
    let agent = agent(
        &provider,
        vec![virtual_model(Arc::clone(&router), ContinuationPolicy::Sticky)],
    );
    smol::block_on(agent.start_prompt("plan it").expect("run").drive()).expect("first run");
    smol::block_on(agent.start_prompt("now build").expect("run").drive()).expect("second run");

    let requests = provider.requests();
    // The tool continuation stayed on the planner even though the edit would
    // have moved a routed continuation to the builder.
    assert_eq!(requests[0].model, Some(planner()));
    assert_eq!(requests[1].model, Some(planner()));
    assert_eq!(requests[2].model, Some(builder()));
    assert!(requests
        .iter()
        .all(|request| request.selected_model == Some(selection())));
    assert_eq!(requests[0].thinking_level, ThinkingLevel::High);
    assert_eq!(requests[1].thinking_level, ThinkingLevel::Medium);
    assert_eq!(requests[2].thinking_level, ThinkingLevel::Low);
    // The router ran only for user turns and saw the state it stored.
    assert_eq!(
        router.calls.lock().expect("calls").clone(),
        [
            (RouteReason::User, None),
            (RouteReason::User, Some("plan".into()))
        ]
    );
    // Responses record the physical model that produced them; the selection
    // stays virtual.
    let origins = agent
        .snapshot()
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Assistant { origin, .. } => origin.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(origins, [planner(), planner(), builder()]);
    assert_eq!(agent.snapshot().model, Some(selection()));
}

#[test]
fn a_routed_continuation_policy_consults_the_router_after_tools() {
    let router = Arc::new(PlanBuild {
        calls: Mutex::new(Vec::new()),
        target_override: None,
    });
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().tool_call("call-edit", "edit", "{}").end_tool_use(),
        ScriptedTurn::new().text("built").stop(),
    ]);
    let agent = agent(
        &provider,
        vec![virtual_model(Arc::clone(&router), ContinuationPolicy::Routed)],
    );
    smol::block_on(agent.start_prompt("go").expect("run").drive()).expect("run");
    let requests = provider.requests();
    assert_eq!(requests[0].model, Some(planner()));
    assert_eq!(requests[1].model, Some(builder()));
    assert_eq!(
        router.calls.lock().expect("calls").clone(),
        [
            (RouteReason::User, None),
            (RouteReason::Continuation, Some("plan".into()))
        ]
    );
}

#[test]
fn unapproved_targets_and_missing_routers_fail_clearly_without_dispatch() {
    let rogue = Arc::new(PlanBuild {
        calls: Mutex::new(Vec::new()),
        target_override: Some(model("elsewhere", "expensive")),
    });
    let provider = ScriptedProvider::new([ScriptedTurn::new().text("never").stop()]);
    let routed = agent(&provider, vec![virtual_model(rogue, ContinuationPolicy::Sticky)]);
    let error = smol::block_on(routed.start_prompt("go").expect("run").drive())
        .expect_err("unapproved target fails");
    assert!(
        matches!(&error, CoreError::ModelRouting { message } if message.contains("elsewhere/expensive") && message.contains("not a host-approved target")),
        "{error}"
    );
    assert_eq!(provider.request_count(), 0);

    let provider = ScriptedProvider::new([ScriptedTurn::new().text("never").stop()]);
    let unrouted = agent(&provider, Vec::new());
    let error = smol::block_on(unrouted.start_prompt("go").expect("run").drive())
        .expect_err("missing router fails");
    assert!(
        matches!(&error, CoreError::ModelRouting { message } if message.contains("has no router")),
        "{error}"
    );
    assert_eq!(provider.request_count(), 0);
}

#[test]
fn a_physical_selection_is_never_routed() {
    let router = Arc::new(PlanBuild {
        calls: Mutex::new(Vec::new()),
        target_override: None,
    });
    let provider = ScriptedProvider::new([ScriptedTurn::new().text("direct").stop()]);
    let agent = Agent::builder()
        .system_prompt("p")
        .model(builder())
        .virtual_models(vec![virtual_model(Arc::clone(&router), ContinuationPolicy::Sticky)])
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("go").expect("run").drive()).expect("run");
    assert_eq!(provider.requests()[0].model, Some(builder()));
    assert_eq!(provider.requests()[0].selected_model, None);
    assert!(router.calls.lock().expect("calls").is_empty());
}
