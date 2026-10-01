//! The public plan/build router example: a deterministic, stateful virtual
//! model resolved through the Luau extension engine.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tea_core::harness::extension::{
    ExtensionCapabilityBindings, ExtensionEngine, ExtensionLimits, ExtensionMemoryCollector,
    ExtensionSourceTree, ExtensionVirtualModel,
};
use tea_core::hooks::NoHooks;
use tea_core::routing::{ContinuationPolicy, Route, RouteReason, RouteRequest};
use tea_core::state::{AgentMessage, MessageId, ModelDescriptor, ThinkingLevel, ToolCallId};
use tea_luau::LuauExtensionEngine;
use tea_protocol::JsonValue;

const MANIFEST: &str = include_str!("../examples/plan_build_router/manifest.json");
const SOURCE: &str = include_str!("../examples/plan_build_router/init.luau");

fn source() -> ExtensionSourceTree {
    ExtensionSourceTree {
        extension_id: "plan-build".into(),
        files: BTreeMap::from([
            ("manifest.json".into(), MANIFEST.into()),
            ("init.luau".into(), SOURCE.into()),
        ]),
        expected_capabilities: Some(BTreeSet::from(["extension.state".into()])),
        limits: ExtensionLimits {
            max_source_bytes: 16 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 100_000,
        },
    }
}

fn model(provider: &str, id: &str) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.into(),
        model: id.into(),
        revision: None,
    }
}

fn resolve() -> ExtensionVirtualModel {
    let descriptor = LuauExtensionEngine
        .describe(&source())
        .expect("example describes");
    assert_eq!(descriptor.state_version.as_deref(), Some("plan-build.v1"));
    assert!(descriptor.tools.is_empty() && descriptor.prompt_sections.is_empty());
    let mut resolved = LuauExtensionEngine
        .resolve(
            &source(),
            ExtensionCapabilityBindings::default(),
            Arc::new(NoHooks),
            0,
            Arc::new(ExtensionMemoryCollector::default()),
        )
        .expect("example resolves");
    assert_eq!(resolved.virtual_models.len(), 1);
    resolved.virtual_models.remove(0)
}

fn user(id: u64, text: &str) -> AgentMessage {
    AgentMessage::User {
        id: MessageId(id),
        content: text.into(),
    }
}

fn tool_result(id: u64, name: &str, is_error: bool) -> AgentMessage {
    AgentMessage::ToolResult {
        id: MessageId(id),
        tool_call_id: ToolCallId::new(format!("call-{id}")).expect("id"),
        tool_name: name.into(),
        content: "ok".into(),
        details: None,
        is_error,
        added_tool_names: Vec::new(),
        usage: Box::new(None),
        terminate: false,
        failure: None,
    }
}

fn route(
    router: &ExtensionVirtualModel,
    state: Option<&JsonValue>,
    targets: &[ModelDescriptor],
    messages: &[AgentMessage],
) -> Route {
    router
        .router
        .route(&RouteRequest {
            selected: &model("virtual", "plan-build"),
            thinking_level: ThinkingLevel::Medium,
            reason: RouteReason::User,
            previous: None,
            targets,
            state,
            messages,
        })
        .expect("routes")
}

fn phase(route: &Route) -> Option<&str> {
    route
        .state
        .as_ref()
        .and_then(|state| state.get("phase"))
        .and_then(JsonValue::as_str)
}

#[test]
fn the_example_plans_first_builds_after_an_edit_and_can_return_to_planning() {
    let router = resolve();
    assert_eq!(router.id, "plan-build");
    assert_eq!(router.continuations, ContinuationPolicy::Sticky);
    assert_eq!(router.targets, None);
    let targets = [model("anthropic", "claude-opus-5-5"), model("anthropic", "claude-haiku-4-5")];

    let first = route(&router, None, &targets, &[user(1, "design the parser")]);
    assert_eq!(first.target, targets[0]);
    assert_eq!(phase(&first), Some("plan"));
    let plan = first.state.clone().expect("state");

    // Still planning while nothing was edited; the state is unchanged.
    let second = route(
        &router,
        Some(&plan),
        &targets,
        &[
            user(1, "design the parser"),
            tool_result(2, "read", false),
            user(3, "keep going"),
        ],
    );
    assert_eq!(second.target, targets[0]);
    assert!(second.state.is_none());

    // A successful edit in the previous turn moves the next turn to build.
    let messages = [
        user(1, "design the parser"),
        tool_result(2, "edit", false),
        user(3, "continue"),
    ];
    let third = route(&router, Some(&plan), &targets, &messages);
    assert_eq!(third.target, targets[1]);
    assert_eq!(phase(&third), Some("build"));
    let build = third.state.clone().expect("state");
    assert_eq!(route(&router, Some(&build), &targets, &messages).target, targets[1]);

    // A failed edit does not count.
    let failed = route(
        &router,
        Some(&plan),
        &targets,
        &[user(1, "x"), tool_result(2, "edit", true), user(3, "y")],
    );
    assert_eq!(failed.target, targets[0]);

    // "plan:" returns to the planning model explicitly.
    let back = route(&router, Some(&build), &targets, &[user(9, "Plan: rethink it")]);
    assert_eq!(back.target, targets[0]);
    assert_eq!(phase(&back), Some("plan"));

    // A single approved model serves both phases.
    let single = route(&router, Some(&build), &targets[..1], &messages);
    assert_eq!(single.target, targets[0]);
}
