use super::{
    CHILD_SUBAGENT_INSTRUCTION_SUFFIX, ROOT_SUBAGENT_INSTRUCTION_SUFFIX, SubagentModel,
    SubagentPolicy, append_child_subagent_instruction_suffix, append_root_subagent_surface,
    child_subagent_tool_definitions, root_subagent_tool_definitions,
    root_subagent_tool_presentations, root_subagent_tool_surface_digest,
};
use crate::state::ModelDescriptor;
use crate::tool::{CancellationSettlementMode, ToolDefinition, ToolExecutionMode};
use std::num::NonZeroU32;
use std::time::Duration;
use tea_protocol::JsonValue;

fn policy(max_concurrent: u32, total: u32, timeout: Duration) -> SubagentPolicy {
    SubagentPolicy {
        models: vec![SubagentModel {
            descriptor: ModelDescriptor {
                provider: "fixture".into(),
                model: "child".into(),
                revision: Some("pinned".into()),
            },
            display_name: "Fixture child".into(),
            context_window: None,
        }],
        max_concurrent: NonZeroU32::new(max_concurrent).expect("fixture limit is nonzero"),
        max_total_per_operation: NonZeroU32::new(total).expect("fixture limit is nonzero"),
        timeout,
    }
}

#[test]
fn policy_retains_revision_and_enforces_final_limits() {
    let valid = policy(4, 16, Duration::from_secs(900));
    valid.validate().expect("documented defaults are valid");
    assert_eq!(
        valid.models[0].descriptor.revision.as_deref(),
        Some("pinned"),
        "the provider descriptor must not be reduced to provider/model only"
    );
    assert!(policy(17, 17, Duration::from_secs(900)).validate().is_err());
    assert!(policy(4, 3, Duration::from_secs(900)).validate().is_err());
    assert!(policy(4, 65, Duration::from_secs(900)).validate().is_err());
    assert!(policy(4, 4, Duration::from_secs(29)).validate().is_err());
    assert!(policy(4, 4, Duration::from_secs(7_201)).validate().is_err());
    let mut ambiguous_enum = policy(1, 1, Duration::from_secs(30));
    ambiguous_enum.models.push(SubagentModel {
        descriptor: ModelDescriptor {
            provider: "fixture".into(),
            model: "child".into(),
            revision: Some("other-pin".into()),
        },
        display_name: "Other pin".into(),
        context_window: None,
    });
    assert!(
        ambiguous_enum.validate().is_err(),
        "two full descriptors cannot collapse into one model enum value"
    );
}

#[test]
fn policy_rejects_padded_or_control_bearing_durable_strings() {
    let mutations: [fn(&mut SubagentPolicy); 4] = [
        |policy: &mut SubagentPolicy| policy.models[0].descriptor.provider = " fixture".into(),
        |policy: &mut SubagentPolicy| policy.models[0].descriptor.model = "child\nmodel".into(),
        |policy: &mut SubagentPolicy| policy.models[0].descriptor.revision = Some("pinned ".into()),
        |policy: &mut SubagentPolicy| policy.models[0].display_name = "Fixture\tchild".into(),
    ];
    for mutate in mutations {
        let mut invalid = policy(1, 1, Duration::from_secs(30));
        mutate(&mut invalid);
        assert!(
            invalid.validate().is_err(),
            "immutable policy strings must retain one unambiguous prompt/session spelling"
        );
    }
}

#[test]
fn root_tool_surface_is_ordered_and_has_the_required_execution_contract() {
    let policy = SubagentPolicy {
        models: vec![
            SubagentModel {
                descriptor: ModelDescriptor {
                    provider: "fixture".into(),
                    model: "model-a".into(),
                    revision: Some("r1".into()),
                },
                display_name: "Model A".into(),
                context_window: None,
            },
            SubagentModel {
                descriptor: ModelDescriptor {
                    provider: "fixture".into(),
                    model: "model-b".into(),
                    revision: Some("r2".into()),
                },
                display_name: "Model B".into(),
                context_window: None,
            },
        ],
        max_concurrent: NonZeroU32::new(2).expect("nonzero fixture limit"),
        max_total_per_operation: NonZeroU32::new(4).expect("nonzero fixture limit"),
        timeout: Duration::from_secs(900),
    };
    let definitions = root_subagent_tool_definitions(&policy).expect("surface builds");
    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        [
            "spawn_agent",
            "wait_agent",
            "list_agents",
            "interrupt_agent",
            "apply_agent_changes",
        ]
    );
    assert_eq!(
        execution_contracts(&definitions),
        [
            (ToolExecutionMode::Sequential, false, CancellationSettlementMode::AwaitFuture),
            (ToolExecutionMode::Sequential, false, CancellationSettlementMode::DropFuture),
            (ToolExecutionMode::Parallel, false, CancellationSettlementMode::DropFuture),
            (ToolExecutionMode::Sequential, false, CancellationSettlementMode::AwaitFuture),
            (ToolExecutionMode::Sequential, true, CancellationSettlementMode::AwaitFuture),
        ]
    );
    let model_enum = definitions[0]
        .schema
        .as_object()
        .and_then(|root| root.get("properties"))
        .and_then(JsonValue::as_object)
        .and_then(|properties| properties.get("model"))
        .and_then(JsonValue::as_object)
        .and_then(|model| model.get("enum"))
        .and_then(JsonValue::as_array)
        .expect("spawn schema has an ordered model enum")
        .iter()
        .map(|value| value.as_str().expect("enum value is a string"))
        .collect::<Vec<_>>();
    assert_eq!(model_enum, ["model-a", "model-b"]);
    assert_eq!(
        root_subagent_tool_presentations(&policy)
            .expect("presentations build")
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        [
            "spawn_agent",
            "wait_agent",
            "list_agents",
            "interrupt_agent",
            "apply_agent_changes",
        ]
    );
    assert_eq!(
        root_subagent_tool_surface_digest(&policy).expect("digest builds"),
        root_subagent_tool_surface_digest(&policy).expect("digest is deterministic"),
    );
    let mut changed_catalog = policy.clone();
    changed_catalog.models[1].descriptor.model = "model-c".into();
    assert_ne!(
        root_subagent_tool_surface_digest(&policy).expect("original digest builds"),
        root_subagent_tool_surface_digest(&changed_catalog)
            .expect("changed catalog digest builds"),
        "the model allowlist deliberately changes the root prompt/tool domain",
    );
    let mut enabled_prompt = "existing root prompt".to_owned();
    let mut enabled_tools = vec![fixture_tool("read")];
    let digest = append_root_subagent_surface(
        &mut enabled_prompt,
        &mut enabled_tools,
        Some(&policy),
    )
    .expect("enabled root surface builds")
    .expect("enabled root surface returns its persisted digest");
    assert_eq!(
        digest,
        root_subagent_tool_surface_digest(&policy).expect("surface digest is stable")
    );
    assert_eq!(
        enabled_prompt,
        format!("existing root prompt\n\n{ROOT_SUBAGENT_INSTRUCTION_SUFFIX}")
    );
    assert_eq!(
        enabled_tools
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        [
            "read",
            "spawn_agent",
            "wait_agent",
            "list_agents",
            "interrupt_agent",
            "apply_agent_changes",
        ]
    );
}

#[test]
fn disabled_root_bytes_are_identical_and_children_never_receive_collaboration_tools() {
    let mut root_prompt = "existing root prompt\nwith deliberate bytes".to_owned();
    let original_root_prompt = root_prompt.clone();
    let mut root_tools = vec![fixture_tool("read")];
    let original_root_tools = root_tools.clone();
    assert_eq!(
        append_root_subagent_surface(&mut root_prompt, &mut root_tools, None)
            .expect("disabled surface is valid"),
        None,
    );
    assert_eq!(root_prompt, original_root_prompt);
    assert_eq!(root_tools, original_root_tools);

    let mut child_prompt = "child base prompt".to_owned();
    append_child_subagent_instruction_suffix(&mut child_prompt);
    assert_eq!(
        child_prompt,
        format!("child base prompt\n\n{CHILD_SUBAGENT_INSTRUCTION_SUFFIX}")
    );
    assert!(child_subagent_tool_definitions().is_empty());
    assert!(
        child_subagent_tool_definitions()
            .iter()
            .all(|tool| !root_collaboration_name(&tool.name)),
        "children cannot inherit root collaboration capabilities",
    );
}

#[test]
fn root_surface_fixture_is_stable() {
    let policy = SubagentPolicy {
        models: vec![SubagentModel {
            descriptor: ModelDescriptor {
                provider: "fixture".into(),
                model: "openai/gpt-5.6-luna".into(),
                revision: Some("fixture-revision".into()),
            },
            display_name: "Fixture Luna".into(),
            context_window: None,
        }],
        max_concurrent: NonZeroU32::new(1).expect("nonzero fixture limit"),
        max_total_per_operation: NonZeroU32::new(1).expect("nonzero fixture limit"),
        timeout: Duration::from_secs(30),
    };
    let definitions = root_subagent_tool_definitions(&policy).expect("surface builds");
    let expected = JsonValue::parse(include_str!("root-tool-surface.fixture.json"))
        .expect("checked-in tool-surface fixture parses");
    assert_eq!(root_surface_fixture_value(&definitions), expected);
}

fn execution_contracts(
    definitions: &[ToolDefinition],
) -> Vec<(ToolExecutionMode, bool, CancellationSettlementMode)> {
    definitions
        .iter()
        .map(|definition| {
            (
                definition.execution_mode,
                definition.requires_exclusive_batch,
                definition.cancellation_settlement_mode,
            )
        })
        .collect()
}

fn fixture_tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: "fixture tool".into(),
        schema: JsonValue::object([("type", JsonValue::String("object".into()))]),
        execution_mode: ToolExecutionMode::Parallel,
        requires_exclusive_batch: false,
        cancellation_settlement_mode: CancellationSettlementMode::DropFuture,
    }
}

fn root_collaboration_name(name: &str) -> bool {
    matches!(
        name,
        "spawn_agent" | "wait_agent" | "list_agents" | "interrupt_agent" | "apply_agent_changes"
    )
}

fn root_surface_fixture_value(definitions: &[ToolDefinition]) -> JsonValue {
    JsonValue::object([
        (
            "root_instruction_suffix",
            JsonValue::String(ROOT_SUBAGENT_INSTRUCTION_SUFFIX.into()),
        ),
        (
            "child_instruction_suffix",
            JsonValue::String(CHILD_SUBAGENT_INSTRUCTION_SUFFIX.into()),
        ),
        (
            "tools",
            JsonValue::Array(
                definitions
                    .iter()
                    .map(|definition| {
                        JsonValue::object([
                            ("name", JsonValue::String(definition.name.clone())),
                            (
                                "description",
                                JsonValue::String(definition.description.clone()),
                            ),
                            ("schema", definition.schema.clone()),
                            (
                                "execution_mode",
                                JsonValue::String(
                                    match definition.execution_mode {
                                        ToolExecutionMode::Sequential => "sequential",
                                        ToolExecutionMode::Parallel => "parallel",
                                    }
                                    .into(),
                                ),
                            ),
                            (
                                "requires_exclusive_batch",
                                JsonValue::Bool(definition.requires_exclusive_batch),
                            ),
                            (
                                "cancellation_settlement_mode",
                                JsonValue::String(
                                    match definition.cancellation_settlement_mode {
                                        CancellationSettlementMode::DropFuture => "drop_future",
                                        CancellationSettlementMode::AwaitFuture => "await_future",
                                    }
                                    .into(),
                                ),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}
