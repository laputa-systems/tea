//! Request-construction cases ported from upstream Pi.

use super::*;
use crate::anthropic::payload::build_request;

fn config(model: &str) -> AnthropicConfig {
    AnthropicConfig::try_new("test-key", model).expect("valid config")
}

fn compat_with(edit: impl FnOnce(&mut AnthropicCompat)) -> AnthropicCompat {
    let mut compat = AnthropicCompat::conservative();
    edit(&mut compat);
    compat
}

fn build(config: &AnthropicConfig, request: &ModelRequest) -> (JsonValue, Vec<&'static str>) {
    let built = build_request(config, request).expect("request builds");
    (built.body, built.betas)
}

fn messages(body: &JsonValue) -> Vec<JsonValue> {
    body.get("messages")
        .and_then(JsonValue::as_array)
        .expect("messages")
        .to_vec()
}

fn roles(body: &JsonValue) -> Vec<String> {
    messages(body)
        .iter()
        .map(|message| {
            message
                .get("role")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn tool_names(body: &JsonValue) -> Vec<String> {
    body.get("tools")
        .and_then(JsonValue::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    tool.get("name")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn ephemeral() -> JsonValue {
    JsonValue::object([("type", JsonValue::from("ephemeral"))])
}

/// The `transcript-tool-changes.test.ts` fixture context.
fn tool_change_history() -> Vec<AgentMessage> {
    vec![
        system(
            1,
            &[
                ("base", Some("base prompt")),
                ("rules", Some("<rules>\nold rules\n</rules>")),
                ("docs", Some("<docs>\nread docs\n</docs>")),
            ],
            &[tool("base_tool")],
            &[],
        ),
        user(2, "before"),
        system(
            3,
            &[
                ("guidance", Some("updated guidance")),
                ("rules", Some("<rules>\nnew rules\n</rules>")),
                ("docs", None),
            ],
            &[tool("late_tool")],
            &["base_tool"],
        ),
    ]
}

fn native_compat() -> AnthropicCompat {
    compat_with(|compat| {
        compat.mid_conversation_system_messages = true;
        compat.mid_conversation_tool_changes = true;
    })
}

// transcript-tool-changes.test.ts: "sends Anthropic updates and tool changes in
// native system messages"
#[test]
fn native_updates_send_tool_changes_in_place_with_a_stable_deferred_tool_list() {
    let config = config("claude-opus-4-8").with_compat(native_compat());
    let (body, betas) = build(
        &config,
        &request("claude-opus-4-8", tool_change_history(), ThinkingLevel::Off),
    );
    assert!(betas.contains(&"mid-conversation-tool-changes-2026-07-01"));
    assert_eq!(
        body.get("system"),
        Some(&JsonValue::Array(vec![JsonValue::object([
            ("type", JsonValue::from("text")),
            (
                "text",
                JsonValue::from(
                    "base prompt\n\n<rules>\nold rules\n</rules>\n\n<docs>\nread docs\n</docs>"
                )
            ),
            ("cache_control", ephemeral()),
        ])]))
    );
    let tools = body.get("tools").and_then(JsonValue::as_array).expect("tools");
    assert_eq!(
        tool_names(&body),
        ["base_tool", "__tea_deferred_placeholder__", "late_tool"]
    );
    assert_eq!(tools[0].get("cache_control"), Some(&ephemeral()));
    assert!(tools[0].get("defer_loading").is_none());
    assert_eq!(tools[1].get("defer_loading"), Some(&JsonValue::Bool(true)));
    assert!(tools[1].get("cache_control").is_none());
    assert_eq!(tools[2].get("defer_loading"), Some(&JsonValue::Bool(true)));
    assert!(tools[2].get("cache_control").is_none());

    let update = messages(&body).pop().expect("update message");
    assert_eq!(update.get("role").and_then(JsonValue::as_str), Some("system"));
    let blocks = update.get("content").and_then(JsonValue::as_array).expect("blocks");
    let kinds = blocks
        .iter()
        .map(|block| block.get("type").and_then(JsonValue::as_str).unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["text", "tool_removal", "tool_addition"]);
    let text = blocks[0].get("text").and_then(JsonValue::as_str).expect("text");
    assert!(text.contains("updated guidance"));
    assert!(text.contains("<rules>\nnew rules\n</rules>"));
    assert!(text.contains("Removed system prompt section \"docs\""));
    assert_eq!(
        blocks[1].get("tool").and_then(|tool| tool.get("name")).and_then(JsonValue::as_str),
        Some("base_tool")
    );
    assert_eq!(
        blocks[2].get("tool").and_then(|tool| tool.get("name")).and_then(JsonValue::as_str),
        Some("late_tool")
    );
    // The last system message carries the conversation cache breakpoint.
    assert_eq!(blocks[2].get("cache_control"), Some(&ephemeral()));

    // The placeholder is declared from the first request.
    let (initial, _) = build(
        &config,
        &request(
            "claude-opus-4-8",
            tool_change_history()[..2].to_vec(),
            ThinkingLevel::Off,
        ),
    );
    assert_eq!(
        tool_names(&initial),
        ["base_tool", "__tea_deferred_placeholder__"]
    );
}

// transcript-tool-changes.test.ts: "sends the current Anthropic tool list when
// native tool changes cannot express the history"
#[test]
fn redefinitions_and_missing_initial_tools_fall_back_to_the_current_tool_list() {
    let config = config("claude-opus-4-8").with_compat(native_compat());
    let mut redefined = tool("base_tool");
    redefined.description = "changed".into();
    let histories = [
        vec![
            system(1, &[("base", Some("base prompt"))], &[tool("base_tool")], &[]),
            system(2, &[("guidance", Some("updated guidance"))], &[redefined.clone()], &["base_tool"]),
        ],
        vec![
            system(1, &[("base", Some("base prompt"))], &[], &[]),
            system(2, &[("guidance", Some("updated guidance"))], &[redefined.clone()], &[]),
        ],
    ];
    for history in histories {
        let (body, betas) = build(
            &config,
            &request("claude-opus-4-8", history, ThinkingLevel::Off),
        );
        assert!(!betas.contains(&"mid-conversation-tool-changes-2026-07-01"));
        let tools = body.get("tools").and_then(JsonValue::as_array).expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].get("description").and_then(JsonValue::as_str), Some("changed"));
        assert_eq!(tools[0].get("cache_control"), Some(&ephemeral()));
        assert!(tools[0].get("defer_loading").is_none());
        let last = messages(&body).pop().expect("update");
        let kinds = last
            .get("content")
            .and_then(JsonValue::as_array)
            .expect("blocks")
            .iter()
            .map(|block| block.get("type").and_then(JsonValue::as_str).unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["text"]);
    }
}

// transcript-tool-changes.test.ts: "folds Anthropic updates into the system
// prompt without native support"
#[test]
fn without_native_support_updates_fold_into_the_leading_prompt() {
    let config = config("claude-haiku-4-5");
    let (body, betas) = build(
        &config,
        &request("claude-haiku-4-5", tool_change_history(), ThinkingLevel::Off),
    );
    assert!(!betas.contains(&"mid-conversation-tool-changes-2026-07-01"));
    assert_eq!(
        body.get("system")
            .and_then(JsonValue::as_array)
            .and_then(|blocks| blocks[0].get("text"))
            .and_then(JsonValue::as_str),
        Some("base prompt\n\n<rules>\nnew rules\n</rules>\n\nupdated guidance")
    );
    assert_eq!(tool_names(&body), ["late_tool"]);
    assert_eq!(roles(&body), ["user"]);
}

// transcript-tool-changes.test.ts: "requires both Anthropic capabilities for
// native tool changes"
#[test]
fn native_tool_changes_require_both_capabilities() {
    let config = config("claude-opus-4-8").with_compat(compat_with(|compat| {
        compat.mid_conversation_tool_changes = true;
    }));
    let (body, betas) = build(
        &config,
        &request("claude-opus-4-8", tool_change_history(), ThinkingLevel::Off),
    );
    assert!(!betas.contains(&"mid-conversation-tool-changes-2026-07-01"));
    assert_eq!(tool_names(&body), ["late_tool"]);
    assert_eq!(roles(&body), ["user"]);
}

fn managed_assistant(effort: Option<&str>) -> AgentMessage {
    assistant(
        2,
        Some("claude-fable-5-1"),
        vec![
            AssistantContent::Thinking {
                text: "reasoning".into(),
                signature: Some(signature("signature")),
            },
            AssistantContent::text("answer"),
        ],
        Vec::new(),
        effort
            .map(|effort| {
                OpaqueProviderContextItem::new(PROVIDER_ID, payload::EFFORT_CONTEXT_KIND, None, effort)
                    .expect("effort item")
            })
            .into_iter()
            .collect(),
    )
}

fn effort_marker(effort: &str) -> JsonValue {
    JsonValue::object([
        ("role", JsonValue::from("system")),
        ("content", JsonValue::Array(Vec::new())),
        (
            "output_config",
            JsonValue::object([("effort", JsonValue::from(effort))]),
        ),
    ])
}

fn plain_user(text: &str) -> JsonValue {
    JsonValue::object([
        ("role", JsonValue::from("user")),
        ("content", JsonValue::from(text)),
    ])
}

// anthropic-mid-conversation-effort.test.ts: "reconstructs an exact historical
// marker prefix and appends the current marker"
#[test]
fn managed_effort_reconstructs_the_historical_marker_prefix() {
    let config = config("claude-fable-5-1").with_cache_retention(CacheRetention::None);
    let first = build_request(
        &config,
        &request("claude-fable-5-1", vec![user(1, "one")], ThinkingLevel::Low),
    )
    .expect("first request");
    let second = build_request(
        &config,
        &request(
            "claude-fable-5-1",
            vec![user(1, "one"), managed_assistant(Some("low")), user(3, "two")],
            ThinkingLevel::High,
        ),
    )
    .expect("second request");
    assert_eq!(
        messages(&first.body),
        vec![plain_user("one"), effort_marker("low")]
    );
    let second_messages = messages(&second.body);
    assert_eq!(second_messages[..2], messages(&first.body)[..]);
    assert_eq!(second_messages.last(), Some(&effort_marker("high")));
    for body in [&first.body, &second.body] {
        assert_eq!(
            body.get("output_config"),
            Some(&JsonValue::object([("effort", JsonValue::from("high"))]))
        );
        assert_eq!(
            body.get("thinking"),
            Some(&JsonValue::object([
                ("type", JsonValue::from("adaptive")),
                ("display", JsonValue::from("summarized")),
                (
                    "block_binding",
                    JsonValue::object([(
                        "prefix_mismatch_behavior",
                        JsonValue::from("drop_block")
                    )])
                ),
            ]))
        );
    }
    assert_eq!(first.managed_effort, Some("low"));
    assert!(first.betas.contains(&"mid-conversation-output-config-2026-07-01"));
    assert!(first.betas.contains(&"thinking-binding-controls-2026-08-01"));
}

// anthropic-mid-conversation-effort.test.ts: "preserves native effort %s"
#[test]
fn managed_effort_preserves_each_native_level() {
    let config = config("claude-fable-5-1").with_cache_retention(CacheRetention::None);
    for (level, effort) in [
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ] {
        let built = build_request(&config, &request("claude-fable-5-1", vec![user(1, "one")], level))
            .expect("builds");
        assert_eq!(messages(&built.body).last(), Some(&effort_marker(effort)));
        assert_eq!(built.managed_effort, Some(effort));
    }
}

// anthropic-mid-conversation-effort.test.ts: "does not invent markers for
// legacy or other-provider assistants"
#[test]
fn managed_effort_does_not_invent_historical_markers() {
    let config = config("claude-fable-5-1").with_cache_retention(CacheRetention::None);
    let other_provider = OpaqueProviderContextItem::new(
        "other-provider",
        payload::EFFORT_CONTEXT_KIND,
        None,
        "low",
    )
    .expect("item");
    let mut other = managed_assistant(None);
    if let AgentMessage::Assistant { opaque_context, .. } = &mut other {
        opaque_context.push(other_provider);
    }
    let built = build_request(
        &config,
        &request(
            "claude-fable-5-1",
            vec![
                user(1, "one"),
                managed_assistant(None),
                user(3, "two"),
                other,
                user(5, "three"),
            ],
            ThinkingLevel::Medium,
        ),
    )
    .expect("builds");
    let markers = messages(&built.body)
        .into_iter()
        .filter(|message| message.get("role").and_then(JsonValue::as_str) == Some("system"))
        .collect::<Vec<_>>();
    assert_eq!(markers, vec![effort_marker("medium")]);
}

// anthropic-mid-conversation-effort.test.ts: "leaves unsupported models on
// top-level effort"
#[test]
fn adaptive_models_without_managed_effort_use_top_level_effort() {
    let config = config("claude-opus-4-8")
        .with_compat(compat_with(|compat| {
            compat.reasoning = true;
            compat.force_adaptive_thinking = true;
            compat.thinking_can_be_disabled = true;
        }))
        .with_cache_retention(CacheRetention::None);
    let built = build_request(
        &config,
        &request("claude-opus-4-8", vec![user(1, "one")], ThinkingLevel::Low),
    )
    .expect("builds");
    assert_eq!(messages(&built.body), vec![plain_user("one")]);
    assert_eq!(
        built.body.get("output_config"),
        Some(&JsonValue::object([("effort", JsonValue::from("low"))]))
    );
    assert_eq!(
        built.body.get("thinking"),
        Some(&JsonValue::object([
            ("type", JsonValue::from("adaptive")),
            ("display", JsonValue::from("summarized")),
        ]))
    );
    assert_eq!(built.managed_effort, None);
}

// anthropic-thinking-disable.test.ts
#[test]
fn thinking_off_disables_only_where_the_model_allows_it() {
    let budget = config("claude-haiku-4-5");
    let (body, _) = build(&budget, &request("claude-haiku-4-5", vec![user(1, "Hello")], ThinkingLevel::Off));
    assert_eq!(
        body.get("thinking"),
        Some(&JsonValue::object([("type", JsonValue::from("disabled"))]))
    );
    assert!(body.get("output_config").is_none());

    let adaptive = config("claude-opus-4-8").with_compat(compat_with(|compat| {
        compat.reasoning = true;
        compat.force_adaptive_thinking = true;
        compat.thinking_can_be_disabled = true;
        compat.native_xhigh = true;
    }));
    let (body, _) = build(&adaptive, &request("claude-opus-4-8", vec![user(1, "Hello")], ThinkingLevel::Off));
    assert_eq!(
        body.get("thinking"),
        Some(&JsonValue::object([("type", JsonValue::from("disabled"))]))
    );
    let (body, _) = build(&adaptive, &request("claude-opus-4-8", vec![user(1, "Hello")], ThinkingLevel::XHigh));
    assert_eq!(
        body.get("output_config"),
        Some(&JsonValue::object([("effort", JsonValue::from("xhigh"))]))
    );

    // Claude Fable cannot disable thinking: no thinking field is sent.
    let fable = config("claude-fable-5").with_compat(compat_with(|compat| {
        compat.reasoning = true;
        compat.force_adaptive_thinking = true;
    }));
    let (body, _) = build(&fable, &request("claude-fable-5", vec![user(1, "Hello")], ThinkingLevel::Off));
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
}

// simple-options.ts adjustMaxTokensForThinking and the interleaved beta
#[test]
fn budget_thinking_adds_a_budget_beneath_the_output_ceiling() {
    let config = config("claude-haiku-4-5").with_cache_retention(CacheRetention::None);
    let built = build_request(
        &config,
        &request("claude-haiku-4-5", vec![user(1, "Hello")], ThinkingLevel::Medium),
    )
    .expect("builds");
    assert_eq!(
        built.body.get("thinking"),
        Some(&JsonValue::object([
            ("type", JsonValue::from("enabled")),
            ("budget_tokens", JsonValue::from(8_192_u64)),
            ("display", JsonValue::from("summarized")),
        ]))
    );
    assert_eq!(built.body.get("max_tokens"), Some(&JsonValue::from(64_000_u64)));
    assert!(built.betas.contains(&"interleaved-thinking-2025-05-14"));

    // An explicit small cap keeps room for the answer.
    let mut small = request("claude-haiku-4-5", vec![user(1, "Hello")], ThinkingLevel::High);
    small.max_output_tokens = Some(2_000);
    let built = build_request(&config, &small).expect("builds");
    assert_eq!(built.body.get("max_tokens"), Some(&JsonValue::from(18_384_u64)));
    assert_eq!(
        built.body.get("thinking").and_then(|thinking| thinking.get("budget_tokens")),
        Some(&JsonValue::from(16_384_u64))
    );

    let (_, betas) = build(&config, &request("claude-haiku-4-5", vec![user(1, "Hello")], ThinkingLevel::Off));
    assert!(!betas.contains(&"interleaved-thinking-2025-05-14"));
}

// anthropic-temperature-compat.test.ts
#[test]
fn temperature_is_sent_only_without_thinking_on_supporting_models() {
    let config = config("claude-haiku-4-5")
        .with_temperature(0.5)
        .expect("valid temperature");
    let (body, _) = build(&config, &request("claude-haiku-4-5", vec![user(1, "Hi")], ThinkingLevel::Off));
    assert!(body.get("temperature").is_some());
    let (body, _) = build(&config, &request("claude-haiku-4-5", vec![user(1, "Hi")], ThinkingLevel::Low));
    assert!(body.get("temperature").is_none());
    let opus = config_for_managed_temperature();
    let (body, _) = build(&opus, &request("claude-opus-5-5", vec![user(1, "Hi")], ThinkingLevel::Off));
    assert!(body.get("temperature").is_none());
}

fn config_for_managed_temperature() -> AnthropicConfig {
    config("claude-opus-5-5")
        .with_temperature(0.5)
        .expect("valid temperature")
}

// cache-retention.test.ts (payload cases)
#[test]
fn cache_retention_controls_breakpoints_and_lifetime() {
    let history = vec![
        system(1, &[("base", Some("system"))], &[tool("read")], &[]),
        user(2, "hello"),
    ];
    let short = config("claude-haiku-4-5");
    let (body, _) = build(&short, &request("claude-haiku-4-5", history.clone(), ThinkingLevel::Off));
    let last_user = messages(&body).pop().expect("user");
    assert_eq!(
        last_user.get("content"),
        Some(&JsonValue::Array(vec![JsonValue::object([
            ("type", JsonValue::from("text")),
            ("text", JsonValue::from("hello")),
            ("cache_control", ephemeral()),
        ])]))
    );
    let long = config("claude-haiku-4-5").with_cache_retention(CacheRetention::Long);
    let (body, _) = build(&long, &request("claude-haiku-4-5", history.clone(), ThinkingLevel::Off));
    let one_hour = JsonValue::object([
        ("type", JsonValue::from("ephemeral")),
        ("ttl", JsonValue::from("1h")),
    ]);
    assert_eq!(
        body.get("system").and_then(JsonValue::as_array).and_then(|blocks| blocks[0].get("cache_control")),
        Some(&one_hour)
    );
    assert_eq!(
        body.get("tools").and_then(JsonValue::as_array).and_then(|tools| tools[0].get("cache_control")),
        Some(&one_hour)
    );
    let unsupported = config("claude-haiku-4-5")
        .with_cache_retention(CacheRetention::Long)
        .with_compat(compat_with(|compat| compat.supports_long_cache_retention = false));
    let (body, _) = build(&unsupported, &request("claude-haiku-4-5", history.clone(), ThinkingLevel::Off));
    assert_eq!(
        body.get("system").and_then(JsonValue::as_array).and_then(|blocks| blocks[0].get("cache_control")),
        Some(&ephemeral())
    );
    let none = config("claude-haiku-4-5").with_cache_retention(CacheRetention::None);
    let (body, _) = build(&none, &request("claude-haiku-4-5", history, ThinkingLevel::Off));
    assert!(!body.to_json_string().expect("JSON").contains("cache_control"));
}

// anthropic-empty-thinking-signature-compat.test.ts; Tea deliberately drops
// unsigned thinking instead of converting it to answer text.
#[test]
fn unsigned_thinking_is_dropped_unless_empty_signatures_are_accepted() {
    let history = vec![
        user(1, "go"),
        assistant(
            2,
            Some("claude-haiku-4-5"),
            vec![
                AssistantContent::thinking("unsigned reasoning"),
                AssistantContent::text("answer"),
            ],
            Vec::new(),
            Vec::new(),
        ),
        user(3, "again"),
    ];
    let strict = config("claude-haiku-4-5");
    let (body, _) = build(&strict, &request("claude-haiku-4-5", history.clone(), ThinkingLevel::Off));
    let encoded = body.to_json_string().expect("JSON");
    assert!(!encoded.contains("unsigned reasoning"));
    let assistant_blocks = messages(&body)[1]
        .get("content")
        .and_then(JsonValue::as_array)
        .expect("blocks")
        .to_vec();
    assert_eq!(
        assistant_blocks,
        vec![JsonValue::object([
            ("type", JsonValue::from("text")),
            ("text", JsonValue::from("answer")),
        ])]
    );

    let lenient = config("claude-haiku-4-5").with_compat(compat_with(|compat| {
        compat.reasoning = true;
        compat.allow_empty_signature = true;
    }));
    let (body, _) = build(&lenient, &request("claude-haiku-4-5", history, ThinkingLevel::Off));
    let blocks = messages(&body)[1]
        .get("content")
        .and_then(JsonValue::as_array)
        .expect("blocks")
        .to_vec();
    assert_eq!(
        blocks[0],
        JsonValue::object([
            ("type", JsonValue::from("thinking")),
            ("thinking", JsonValue::from("unsigned reasoning")),
            ("signature", JsonValue::from("")),
        ])
    );
}

#[test]
fn same_model_signed_and_redacted_thinking_replay_and_cross_model_thinking_does_not() {
    let redacted = OpaqueProviderContextItem::new(
        PROVIDER_ID,
        payload::REDACTED_THINKING_CONTEXT_KIND,
        None,
        "cipher",
    )
    .expect("item");
    let history = vec![
        user(1, "go"),
        assistant(
            2,
            Some("claude-haiku-4-5"),
            vec![
                AssistantContent::Thinking {
                    text: "plan".into(),
                    signature: Some(signature("sig")),
                },
                AssistantContent::RedactedThinking { data: redacted },
                AssistantContent::text("answer"),
            ],
            Vec::new(),
            Vec::new(),
        ),
        user(3, "again"),
    ];
    let config = config("claude-haiku-4-5");
    let (body, _) = build(&config, &request("claude-haiku-4-5", history.clone(), ThinkingLevel::Off));
    let kinds = messages(&body)[1]
        .get("content")
        .and_then(JsonValue::as_array)
        .expect("blocks")
        .iter()
        .map(|block| block.get("type").and_then(JsonValue::as_str).unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["thinking", "redacted_thinking", "text"]);

    // The same history replayed to a different model loses its thinking.
    let other = AnthropicConfig::try_new("test-key", "claude-sonnet-5-5").expect("config");
    let (body, _) = build(&other, &request("claude-sonnet-5-5", history, ThinkingLevel::Off));
    let encoded = body.to_json_string().expect("JSON");
    assert!(!encoded.contains("plan") && !encoded.contains("cipher") && !encoded.contains("\"sig\""));
}

// tool-call-id-normalization.test.ts and Pi's consecutive tool-result grouping
#[test]
fn tool_ids_are_normalized_consistently_and_results_are_grouped() {
    let long_foreign_id = format!("call|{}", "x".repeat(80));
    let history = vec![
        user(1, "go"),
        AgentMessage::Assistant {
            id: MessageId(2),
            content: Vec::new(),
            tool_calls: vec![call(&long_foreign_id, "read"), call("toolu_ok", "read")],
            stop_reason: Some(crate::state::StopReason::ToolUse),
            error_message: None,
            opaque_context: Vec::new(),
            origin: None,
        },
        AgentMessage::ToolResult {
            id: MessageId(3),
            tool_call_id: ToolCallId::new(long_foreign_id.clone()).expect("id"),
            tool_name: "read".into(),
            content: "first".into(),
            details: Some(SerializedJson::new(r#"{"bytes":5}"#)),
            usage: Box::new(None),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        },
        AgentMessage::ToolResult {
            id: MessageId(4),
            tool_call_id: ToolCallId::new("toolu_ok").expect("id"),
            tool_name: "read".into(),
            content: "second".into(),
            details: None,
            usage: Box::new(None),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: true,
            failure: None,
        },
    ];
    let config = config("claude-haiku-4-5").with_cache_retention(CacheRetention::None);
    let (body, _) = build(&config, &request("claude-haiku-4-5", history, ThinkingLevel::Off));
    let all = messages(&body);
    assert_eq!(roles(&body), ["user", "assistant", "user"]);
    let uses = all[1].get("content").and_then(JsonValue::as_array).expect("uses");
    let normalized = uses[0].get("id").and_then(JsonValue::as_str).expect("id").to_owned();
    assert_eq!(normalized.len(), 64);
    assert!(normalized.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    let results = all[2].get("content").and_then(JsonValue::as_array).expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("tool_use_id").and_then(JsonValue::as_str), Some(normalized.as_str()));
    assert!(results[0]
        .get("content")
        .and_then(JsonValue::as_str)
        .is_some_and(|content| content.starts_with("first\n[tool details (serialized JSON):")));
    assert_eq!(results[1].get("is_error"), Some(&JsonValue::Bool(true)));
    assert_eq!(
        uses[0].get("input"),
        Some(&JsonValue::object([("path", JsonValue::from("README.md"))]))
    );
}

// anthropic-eager-tool-input-compat.test.ts
#[test]
fn tool_input_streaming_is_eager_unless_the_model_needs_the_fine_grained_beta() {
    let history = vec![system(1, &[("base", Some("s"))], &[tool("read")], &[]), user(2, "go")];
    let eager = config("claude-haiku-4-5");
    let (body, betas) = build(&eager, &request("claude-haiku-4-5", history.clone(), ThinkingLevel::Off));
    assert_eq!(
        body.get("tools").and_then(JsonValue::as_array).and_then(|tools| tools[0].get("eager_input_streaming")),
        Some(&JsonValue::Bool(true))
    );
    assert!(!betas.contains(&"fine-grained-tool-streaming-2025-05-14"));
    let legacy = config("claude-haiku-4-5")
        .with_compat(compat_with(|compat| compat.eager_tool_input_streaming = false));
    let (body, betas) = build(&legacy, &request("claude-haiku-4-5", history, ThinkingLevel::Off));
    assert!(body
        .get("tools")
        .and_then(JsonValue::as_array)
        .and_then(|tools| tools[0].get("eager_input_streaming"))
        .is_none());
    assert!(betas.contains(&"fine-grained-tool-streaming-2025-05-14"));
}

#[test]
fn host_notes_are_system_content_in_place_and_user_content_otherwise() {
    let mut in_place = request("claude-opus-5-5", vec![user(1, "go")], ThinkingLevel::High);
    in_place.transcript.host_notes = vec!["continue the goal".into()];
    let (body, _) = build(&config("claude-opus-5-5"), &in_place);
    let notes = messages(&body)
        .into_iter()
        .filter(|message| {
            message
                .get("content")
                .and_then(JsonValue::as_array)
                .is_some_and(|blocks| !blocks.is_empty())
                && message.get("role").and_then(JsonValue::as_str) == Some("system")
        })
        .count();
    assert_eq!(notes, 1);
    let mut collapsed = request("claude-haiku-4-5", vec![user(1, "go")], ThinkingLevel::Off);
    collapsed.transcript.host_notes = vec!["continue the goal".into()];
    let (body, _) = build(&config("claude-haiku-4-5"), &collapsed);
    assert_eq!(roles(&body), ["user", "user"]);
}

#[test]
fn a_per_request_output_cap_overrides_the_configured_ceiling() {
    let mut warm = request("claude-opus-5-5", vec![user(1, "go")], ThinkingLevel::High);
    warm.max_output_tokens = Some(1);
    let (body, _) = build(&config("claude-opus-5-5"), &warm);
    assert_eq!(body.get("max_tokens"), Some(&JsonValue::from(1_u64)));
}

#[test]
fn capabilities_follow_the_catalog_and_cache_retention() {
    let provider = AnthropicProvider::new(config("claude-opus-5-5"));
    let capabilities = provider.capabilities(None);
    assert!(capabilities.configuration_updates.system_messages);
    assert!(capabilities.configuration_updates.tool_changes);
    assert!(capabilities.exposes_thinking);
    assert_eq!(
        capabilities.prompt_cache,
        Some(crate::scheduler::PromptCacheCapability {
            ttl_seconds: 300,
            minimal_output_replay: true,
        })
    );
    // Budget thinking derives its budget from the output cap, so a one-token
    // replay is not cache-equivalent (Pi's isReplayable).
    let haiku = AnthropicProvider::new(config("claude-haiku-4-5").with_cache_retention(CacheRetention::Long));
    let capabilities = haiku.capabilities(None);
    assert_eq!(
        capabilities.prompt_cache,
        Some(crate::scheduler::PromptCacheCapability {
            ttl_seconds: 3_600,
            minimal_output_replay: false,
        })
    );
    assert!(capabilities.pricing.is_some());
    let none = AnthropicProvider::new(config("claude-haiku-4-5").with_cache_retention(CacheRetention::None));
    assert_eq!(none.capabilities(None).prompt_cache, None);
    let custom = AnthropicProvider::new(config("claude-private-model"));
    let capabilities = custom.capabilities(None);
    assert!(!capabilities.exposes_thinking && capabilities.pricing.is_none());
}

// anthropic-cache-write-1h-cost.test.ts, with claude-opus-4-8 prices.
#[test]
fn one_hour_cache_writes_are_priced_at_twice_input() {
    let pricing = crate::scheduler::ModelPricing {
        input: "5".into(),
        output: "25".into(),
        cache_read: "0.5".into(),
        cache_write: "6.25".into(),
    };
    let split = AnthropicUsage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 1_000_000,
        cache_write_1h: 400_000,
        reasoning: None,
    };
    assert_eq!(estimate_cost(split, &pricing).as_deref(), Some("7.75"));
    let unsplit = AnthropicUsage {
        cache_write_1h: 0,
        ..split
    };
    assert_eq!(estimate_cost(unsplit, &pricing).as_deref(), Some("6.25"));
    // The cache-warmer reference case: 100k cached tokens plus one output token.
    let warm = AnthropicUsage {
        input: 0,
        output: 1,
        cache_read: 100_000,
        cache_write: 0,
        cache_write_1h: 0,
        reasoning: None,
    };
    assert_eq!(estimate_cost(warm, &pricing).as_deref(), Some("0.050025"));
}
