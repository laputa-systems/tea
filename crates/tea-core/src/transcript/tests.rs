//! Transcript replay cases ported from upstream Pi
//! (`packages/ai/test/system-message-replay.test.ts` and
//! `transform-messages` behavior), adapted to Tea's section-only prompts.

use super::*;
use crate::state::{
    AgentToolCall, OpaqueProviderContextItem, PromptSection, SectionChange, SerializedJson,
    SystemPrompt,
};

fn tool(name: &str) -> ToolDeclaration {
    ToolDeclaration::new(
        name,
        format!("{name} tool"),
        JsonValue::object([("type", JsonValue::from("object"))]),
    )
}

fn changed(name: &str, description: &str) -> ToolDeclaration {
    ToolDeclaration {
        description: description.into(),
        ..tool(name)
    }
}

fn set(id: &str, content: &str) -> SectionChange {
    SectionChange {
        id: id.into(),
        content: Some(content.into()),
    }
}

fn remove(id: &str) -> SectionChange {
    SectionChange {
        id: id.into(),
        content: None,
    }
}

fn system(id: u64, update: ConfigurationUpdate) -> AgentMessage {
    AgentMessage::System {
        id: MessageId(id),
        update,
    }
}

fn user(id: u64, text: &str) -> AgentMessage {
    AgentMessage::User {
        id: MessageId(id),
        content: text.into(),
    }
}

fn assistant(id: u64, content: Vec<AssistantContent>, origin: Option<&str>) -> AgentMessage {
    AgentMessage::Assistant {
        id: MessageId(id),
        content,
        tool_calls: Vec::new(),
        stop_reason: Some(StopReason::Stop),
        error_message: None,
        opaque_context: Vec::new(),
        origin: origin.map(model),
    }
}

fn model(name: &str) -> ModelDescriptor {
    let (provider, model) = name.split_once('/').expect("provider/model");
    ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        revision: None,
    }
}

fn fixture() -> Transcript {
    Transcript::new(vec![
        system(
            1,
            ConfigurationUpdate {
                sections: vec![set("base", "base"), set("a", "<a>1</a>"), set("b", "<b>1</b>")],
                tools_added: vec![tool("first")],
                tools_removed: Vec::new(),
            },
        ),
        user(2, "hello"),
        system(
            3,
            ConfigurationUpdate {
                sections: vec![set("extra", "also do this")],
                ..ConfigurationUpdate::default()
            },
        ),
        assistant(4, vec![AssistantContent::text("ok")], None),
        system(
            5,
            ConfigurationUpdate {
                sections: vec![set("a", "<a>2</a>"), remove("b"), set("c", "<c>1</c>")],
                tools_added: vec![tool("second")],
                tools_removed: vec!["first".into()],
            },
        ),
    ])
}

#[test]
fn replays_sections_and_tools_into_the_current_configuration() {
    let transcript = fixture();
    let current = transcript.current_configuration();
    assert_eq!(
        current.sections,
        vec![
            PromptSection::new("base", "base"),
            PromptSection::new("a", "<a>2</a>"),
            PromptSection::new("extra", "also do this"),
            PromptSection::new("c", "<c>1</c>"),
        ]
    );
    assert_eq!(current.tools, vec![tool("second")]);
    assert_eq!(
        transcript.system_prompt(),
        "base\n\n<a>2</a>\n\nalso do this\n\n<c>1</c>"
    );
}

#[test]
fn collapse_keeps_only_non_system_messages_after_the_replayed_head() {
    let resolved = fixture().resolve(ConfigurationProjection::Collapsed);
    assert_eq!(resolved.leading, fixture().current_configuration());
    assert!(
        resolved
            .messages
            .iter()
            .all(|message| !matches!(message, AgentMessage::System { .. }))
    );
    assert_eq!(resolved.messages.len(), 2);
}

#[test]
fn in_place_projection_keeps_the_initial_configuration_and_later_updates() {
    let resolved = fixture().resolve(ConfigurationProjection::InPlace);
    assert_eq!(resolved.leading.system_prompt(), "base\n\n<a>1</a>\n\n<b>1</b>");
    assert_eq!(resolved.leading.tools, vec![tool("first")]);
    let roles = resolved
        .messages
        .iter()
        .map(|message| match message {
            AgentMessage::System { .. } => "system",
            AgentMessage::User { .. } => "user",
            AgentMessage::Assistant { .. } => "assistant",
            AgentMessage::ToolResult { .. } => "tool",
        })
        .collect::<Vec<_>>();
    assert_eq!(roles, ["user", "system", "assistant", "system"]);
    assert_eq!(resolved.current_configuration(), fixture().current_configuration());
}

#[test]
fn replay_of_a_transcript_without_system_messages_is_empty() {
    let transcript = Transcript::new(vec![user(1, "hi")]);
    assert_eq!(transcript.system_prompt(), "");
    assert!(transcript.tools().is_empty());
    let collapsed = transcript.resolve(ConfigurationProjection::Collapsed);
    assert_eq!(collapsed.messages, transcript.messages);
}

#[test]
fn a_late_first_configuration_replays_as_the_initial_prompt() {
    // A session recorded before configuration messages gains its first one
    // after earlier history. That first recorded configuration leads.
    let transcript = Transcript::new(vec![
        user(1, "old session"),
        system(
            2,
            ConfigurationUpdate {
                sections: vec![set("preamble", "You are tea.")],
                tools_added: vec![tool("x")],
                tools_removed: Vec::new(),
            },
        ),
    ]);
    assert_eq!(transcript.system_prompt(), "You are tea.");
    let in_place = transcript.resolve(ConfigurationProjection::InPlace);
    assert_eq!(in_place.leading.tools, vec![tool("x")]);
    assert_eq!(in_place.messages, vec![user(1, "old session")]);
}

#[test]
fn renders_framed_section_updates() {
    let update = ConfigurationUpdate {
        sections: vec![set("a", "<a>2</a>"), remove("b"), set("c", "<c>1</c>")],
        ..ConfigurationUpdate::default()
    };
    assert_eq!(
        update.render_update_text(),
        [
            "Updated system prompt section \"a\":\n\n<a>2</a>",
            "Removed system prompt section \"b\".",
            "Updated system prompt section \"c\":\n\n<c>1</c>",
        ]
        .join("\n\n")
    );
}

#[test]
fn tool_state_changes_treat_changed_definitions_as_removal_plus_addition() {
    let previous = EffectiveConfiguration {
        sections: Vec::new(),
        tools: vec![tool("a"), tool("b")],
    };
    let desired = EffectiveConfiguration {
        sections: Vec::new(),
        tools: vec![changed("b", "changed"), tool("c")],
    };
    let update = previous.diff(&desired).expect("changed tools");
    assert_eq!(update.tools_added, vec![changed("b", "changed"), tool("c")]);
    assert_eq!(update.tools_removed, vec!["a".to_owned(), "b".to_owned()]);
    assert!(previous.diff(&previous).is_none());
}

#[test]
fn order_alone_never_produces_an_update() {
    let replayed = EffectiveConfiguration {
        sections: vec![PromptSection::new("a", "1"), PromptSection::new("b", "2")],
        tools: vec![tool("x"), tool("y")],
    };
    let desired = EffectiveConfiguration {
        sections: vec![PromptSection::new("b", "2"), PromptSection::new("a", "1")],
        tools: vec![tool("y"), tool("x")],
    };
    assert!(replayed.diff(&desired).is_none());
}

#[test]
fn applying_a_diff_reaches_the_desired_content() {
    let current = fixture().current_configuration();
    let desired = EffectiveConfiguration::new(
        &SystemPrompt::new(vec![
            PromptSection::new("base", "base v2"),
            PromptSection::new("d", "<d/>"),
        ])
        .expect("valid prompt"),
        vec![tool("second"), tool("third")],
    );
    let update = current.diff(&desired).expect("changed configuration");
    let mut replayed = current.clone();
    replayed.apply(&update);
    assert!(replayed.same_content(&desired));
}

#[test]
fn detects_non_additive_tool_history_and_redefinitions() {
    assert!(fixture().has_non_additive_tool_changes());
    assert!(!fixture().has_tool_redefinitions());
    let additive = Transcript::new(vec![
        system(
            1,
            ConfigurationUpdate {
                tools_added: vec![tool("a")],
                ..ConfigurationUpdate::default()
            },
        ),
        system(
            2,
            ConfigurationUpdate {
                tools_added: vec![tool("b")],
                ..ConfigurationUpdate::default()
            },
        ),
    ]);
    assert!(!additive.has_non_additive_tool_changes());
    assert_eq!(additive.declared_tools(), vec![tool("a"), tool("b")]);
    let redeclared = Transcript::new(vec![
        system(
            1,
            ConfigurationUpdate {
                tools_added: vec![tool("a")],
                ..ConfigurationUpdate::default()
            },
        ),
        system(
            2,
            ConfigurationUpdate {
                tools_added: vec![changed("a", "changed")],
                ..ConfigurationUpdate::default()
            },
        ),
    ]);
    assert!(redeclared.has_non_additive_tool_changes());
    assert!(redeclared.has_tool_redefinitions());
    assert_eq!(redeclared.declared_tools(), vec![changed("a", "changed")]);
}

fn signature(provider: &str, payload: &str) -> OpaqueProviderContextItem {
    OpaqueProviderContextItem::new(provider, "thinking_signature", None, payload)
        .expect("valid signature")
}

#[test]
fn same_model_replay_keeps_thinking_and_signatures() {
    let content = vec![
        AssistantContent::Thinking {
            text: "plan".into(),
            signature: Some(signature("anthropic", "sig")),
        },
        AssistantContent::RedactedThinking {
            data: signature("anthropic", "cipher"),
        },
        AssistantContent::text("answer"),
    ];
    let transcript = Transcript::new(vec![assistant(1, content.clone(), Some("anthropic/m"))]);
    let prepared = transcript.prepared_for(Some(&model("anthropic/m")));
    assert_eq!(prepared.messages, vec![assistant(1, content, Some("anthropic/m"))]);
}

#[test]
fn cross_model_replay_drops_thinking_instead_of_flattening_it() {
    let transcript = Transcript::new(vec![assistant(
        1,
        vec![
            AssistantContent::Thinking {
                text: "private plan".into(),
                signature: Some(signature("anthropic", "sig")),
            },
            AssistantContent::RedactedThinking {
                data: signature("anthropic", "cipher"),
            },
            AssistantContent::text("answer"),
        ],
        Some("anthropic/m"),
    )]);
    for target in ["anthropic/other", "openrouter/m"] {
        let prepared = transcript.prepared_for(Some(&model(target)));
        assert_eq!(
            prepared.messages,
            vec![assistant(
                1,
                vec![AssistantContent::text("answer")],
                Some("anthropic/m")
            )],
            "{target}"
        );
    }
    // An unknown origin is never treated as the same model.
    let unknown = Transcript::new(vec![assistant(
        1,
        vec![AssistantContent::thinking("plan"), AssistantContent::text("a")],
        None,
    )]);
    assert_eq!(
        unknown.prepared_for(Some(&model("anthropic/m"))).messages,
        vec![assistant(1, vec![AssistantContent::text("a")], None)]
    );
}

#[test]
fn errored_and_aborted_turns_are_not_replayed() {
    let mut failed = assistant(2, vec![AssistantContent::text("partial")], None);
    if let AgentMessage::Assistant { stop_reason, .. } = &mut failed {
        *stop_reason = Some(StopReason::Error);
    }
    let mut aborted = assistant(3, vec![AssistantContent::text("partial")], None);
    if let AgentMessage::Assistant { stop_reason, .. } = &mut aborted {
        *stop_reason = Some(StopReason::Aborted);
    }
    let transcript = Transcript::new(vec![user(1, "go"), failed, aborted, user(4, "again")]);
    assert_eq!(
        transcript.prepared_for(None).messages,
        vec![user(1, "go"), user(4, "again")]
    );
}

fn call(id: &str) -> AgentToolCall {
    AgentToolCall {
        id: ToolCallId::new(id).expect("call id"),
        name: "read".into(),
        arguments: SerializedJson::new("{}"),
    }
}

fn result(id: u64, call_id: &str) -> AgentMessage {
    AgentMessage::ToolResult {
        id: MessageId(id),
        tool_call_id: ToolCallId::new(call_id).expect("call id"),
        tool_name: "read".into(),
        content: "ok".into(),
        details: None,
        usage: Box::new(None),
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    }
}

#[test]
fn system_updates_between_calls_and_results_move_after_the_results() {
    let mut caller = assistant(1, Vec::new(), None);
    if let AgentMessage::Assistant {
        tool_calls,
        stop_reason,
        ..
    } = &mut caller
    {
        *tool_calls = vec![call("a"), call("b")];
        *stop_reason = Some(StopReason::ToolUse);
    }
    let update = system(
        2,
        ConfigurationUpdate {
            tools_added: vec![tool("late")],
            ..ConfigurationUpdate::default()
        },
    );
    let transcript = Transcript::new(vec![
        caller.clone(),
        update.clone(),
        result(3, "a"),
        result(4, "b"),
    ]);
    assert_eq!(
        transcript.prepared_for(None).messages,
        vec![caller, result(3, "a"), result(4, "b"), update]
    );
}

#[test]
fn orphaned_tool_calls_receive_an_explicit_error_result() {
    let mut caller = assistant(1, Vec::new(), None);
    if let AgentMessage::Assistant {
        tool_calls,
        stop_reason,
        ..
    } = &mut caller
    {
        *tool_calls = vec![call("a"), call("b")];
        *stop_reason = Some(StopReason::ToolUse);
    }
    let transcript = Transcript::new(vec![caller.clone(), result(2, "a"), user(3, "next")]);
    let prepared = transcript.prepared_for(None).messages;
    assert_eq!(prepared.len(), 4);
    assert!(matches!(
        &prepared[2],
        AgentMessage::ToolResult { tool_call_id, is_error: true, content, .. }
            if tool_call_id.as_str() == "b" && content == "No result provided"
    ));
    assert_eq!(prepared[3], user(3, "next"));
}

#[test]
fn canonical_json_excludes_message_identities_and_opaque_payloads() {
    let first = Transcript::new(vec![user(1, "hi")]);
    let second = Transcript::new(vec![user(99, "hi")]);
    assert_eq!(first.canonical_json(), second.canonical_json());
    let with_signature = Transcript::new(vec![assistant(
        1,
        vec![AssistantContent::Thinking {
            text: "t".into(),
            signature: Some(signature("anthropic", "very-private")),
        }],
        Some("anthropic/m"),
    )]);
    let encoded = with_signature
        .canonical_json()
        .to_json_string()
        .expect("encode");
    assert!(!encoded.contains("very-private"));
}
