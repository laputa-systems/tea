use super::compaction::ProviderCompactor;
use super::state::ContextEstimate;
use super::*;
use std::ffi::OsString;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use tea_core::compaction::{
    AutomaticCompactionReason, AutomaticCompactionRequest, CompactionContext,
    CompactionRequestLayout, Compactor, OverflowRecovery, ProviderContext,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{Message, MessageId, SerializedJson, ToolCallId};
use tea_core::tool::ToolUpdate;
use tea_core::tool::{ToolDefinition, ToolExecutionMode};
use tea_core::{AgentToolResult, DefaultCodingTools, ModelDescriptor, ThinkingLevel, Usage};

fn test_tea_home(label: &str) -> PathBuf {
    static NEXT_HOME: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "tea-app-{label}-{}-{}",
        std::process::id(),
        NEXT_HOME.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).expect("test Tea home should be created");
    root
}

#[derive(Debug)]
struct ContextCheckingProvider;

#[derive(Debug)]
struct SummaryProvider {
    expected_model: ModelDescriptor,
}

#[derive(Debug, Clone)]
struct RecordingSummaryProvider {
    expected_model: ModelDescriptor,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for RecordingSummaryProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("summary request mutex poisoned")
            .push(request.clone());
        let events = if request.model.as_ref() == Some(&self.expected_model) {
            vec![
                ModelStreamEvent::TextDelta("updated summary".into()),
                ModelStreamEvent::End(tea_core::state::StopReason::EndTurn),
            ]
        } else {
            vec![ModelStreamEvent::Error {
                message: "summary request used the wrong model".into(),
            }]
        };
        Box::pin(std::future::ready(
            Ok(Box::new(ModelStream { events }) as _),
        ))
    }
}

impl ModelProvider for SummaryProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let events = if request.model.as_ref() == Some(&self.expected_model) {
            vec![
                ModelStreamEvent::TextDelta("summary text".into()),
                ModelStreamEvent::End(tea_core::state::StopReason::EndTurn),
            ]
        } else {
            vec![ModelStreamEvent::Error {
                message: "summary request used the wrong local model".into(),
            }]
        };
        Box::pin(std::future::ready(
            Ok(Box::new(ModelStream { events }) as _),
        ))
    }
}

impl ModelProvider for ContextCheckingProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let events = if request.context == r#"[{"content":"hello","role":"user"}]"# {
            vec![
                ModelStreamEvent::TextDelta("ok".into()),
                ModelStreamEvent::End(tea_core::state::StopReason::EndTurn),
            ]
        } else {
            vec![ModelStreamEvent::Error {
                message: "OpenRouter received invalid converted context".into(),
            }]
        };
        Box::pin(std::future::ready(
            Ok(Box::new(ModelStream { events }) as _),
        ))
    }
}

#[test]
fn cli_rejects_ambiguous_and_unknown_inputs() {
    assert!(matches!(
        CliOptions::parse(["tea", "--provider", "one", "--provider", "two"].map(OsString::from)),
        Err(CliError::DuplicateOption("--provider"))
    ));
    assert!(matches!(
        CliOptions::parse(["tea", "unexpected"].map(OsString::from)),
        Err(CliError::UnexpectedArgument(_))
    ));
}

#[test]
fn cli_help_accepts_short_and_long_forms() {
    assert_eq!(
        CliOptions::parse_command(["tea", "-h"].map(OsString::from)),
        Ok(CliCommand::Help)
    );
    assert_eq!(
        CliOptions::parse_command(["tea", "--help"].map(OsString::from)),
        Ok(CliCommand::Help)
    );
    assert!(CliOptions::help_text().contains("--provider <id>"));
}

#[test]
fn cli_parses_one_shot_prompt_and_thinking_level() {
    let CliCommand::Options(options) = CliOptions::parse_command(
        [
            "tea",
            "--provider",
            "openrouter",
            "--model",
            "poolside/laguna-xs-2.1:free",
            "--compaction-strategy",
            "tool_free_replay_summary_v1",
            "--local-base-url",
            "http://127.0.0.1:12345/v1",
            "--thinking",
            "high",
            "-p",
            "say hi",
        ]
        .map(OsString::from),
    )
    .expect("one-shot options parse") else {
        panic!("one-shot options unexpectedly parsed as help");
    };
    assert_eq!(options.provider(), Some(std::ffi::OsStr::new("openrouter")));
    assert_eq!(
        options.model(),
        Some(std::ffi::OsStr::new("poolside/laguna-xs-2.1:free"))
    );
    assert_eq!(
        options.compaction_strategy(),
        Some(std::ffi::OsStr::new("tool_free_replay_summary_v1"))
    );
    assert_eq!(
        options.local_base_url(),
        Some(std::ffi::OsStr::new("http://127.0.0.1:12345/v1"))
    );
    assert_eq!(options.prompt(), Some(std::ffi::OsStr::new("say hi")));
    assert_eq!(options.thinking_level(), ThinkingLevel::High);
}

#[test]
fn cli_rejects_unknown_thinking_level() {
    assert!(matches!(
        CliOptions::parse(["tea", "--thinking", "turbo"].map(OsString::from)),
        Err(CliError::InvalidValue {
            flag: "--thinking",
            ..
        })
    ));
}

#[test]
fn cli_parses_and_validates_explicit_local_context_capacity() {
    let options = CliOptions::parse(["tea", "--local-context-window", "32768"].map(OsString::from))
        .expect("local context capacity parses");
    assert_eq!(options.local_context_window(), NonZeroU64::new(32_768));
    assert!(matches!(
        CliOptions::parse(["tea", "--local-context-window", "0"].map(OsString::from)),
        Err(CliError::InvalidValue {
            flag: "--local-context-window",
            ..
        })
    ));
}

#[test]
fn cli_parses_explicit_tea_home() {
    let options = CliOptions::parse(["tea", "--tea-home", "/tmp/tea-test"].map(OsString::from))
        .expect("Tea home parses");
    assert_eq!(
        options.tea_home(),
        Some(std::path::Path::new("/tmp/tea-test"))
    );
    assert!(CliOptions::help_text().contains("--tea-home <path>"));
}

#[test]
fn startup_does_not_open_model_picker_without_a_saved_selection() {
    let tea_home = test_tea_home("startup");
    let options = CliOptions::parse(
        [
            "tea",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]
        .map(OsString::from),
    )
    .expect("startup options parse");
    let mut app = App::new(options);

    app.assemble_agent().expect("host should assemble");

    assert_eq!(app.state().surface(), UiSurface::None);
    assert!(app.state().selected_model.is_none());
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn selected_model_is_saved_and_restored_without_starting_the_picker() {
    let tea_home = test_tea_home("last-model");
    let options = CliOptions::parse(
        [
            "tea",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]
        .map(OsString::from),
    )
    .expect("startup options parse");
    let mut first = App::new(options.clone());
    first.assemble_agent().expect("first host should assemble");
    first
        .select_model(
            "local".into(),
            tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
        )
        .expect("local model should be selectable");
    assert!(tea_home.join("last-model.json").is_file());

    let mut second = App::new(options);
    second
        .assemble_agent()
        .expect("second host should assemble");

    assert_eq!(second.state().surface(), UiSurface::None);
    assert_eq!(
        second
            .state()
            .selected_model
            .as_ref()
            .map(|model| (model.provider.as_str(), model.model.as_str(),)),
        Some(("local", tea_core::provider::local::LAGUNA_XS_2_1_MODEL))
    );
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn event_projection_keeps_streaming_text_as_one_raw_line() {
    let mut state = AppState::new();
    let message = Message::Assistant {
        id: MessageId(2),
        content: "hello".into(),
        tool_calls: Vec::new(),
        stop_reason: None,
        error_message: None,
    };
    state.apply_event(&tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(1),
        kind: tea_core::event::AgentEventKind::MessageUpdate {
            message: message.clone(),
            text_delta: Some("hel".into()),
        },
    });
    state.apply_event(&tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(2),
        kind: tea_core::event::AgentEventKind::MessageUpdate {
            message,
            text_delta: Some("lo".into()),
        },
    });
    assert_eq!(state.transcript().len(), 1);
    assert!(matches!(
        &state.transcript()[0],
        TranscriptEntry::Assistant { text, streaming: true } if text == "hello"
    ));
}

#[test]
fn event_projection_groups_a_tool_lifecycle_in_one_readable_row() {
    let mut state = AppState::new();
    let call_id = ToolCallId::new("call-1").expect("fixture ID");
    let event = |sequence, kind| tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(sequence),
        kind,
    };
    state.apply_event(&event(
        1,
        tea_core::AgentEventKind::ToolExecutionStart {
            tool_call_id: call_id.clone(),
            tool_name: "shell".into(),
            arguments: SerializedJson::new(r#"{"command":"cargo test"}"#),
        },
    ));
    state.apply_event(&event(
        2,
        tea_core::AgentEventKind::ToolExecutionUpdate {
            tool_call_id: call_id.clone(),
            tool_name: "shell".into(),
            update: ToolUpdate {
                content: "compiling".into(),
                details: None,
            },
        },
    ));
    state.apply_event(&event(
        3,
        tea_core::AgentEventKind::ToolExecutionEnd {
            tool_call_id: call_id.clone(),
            tool_name: "shell".into(),
            result: AgentToolResult {
                tool_call_id: call_id,
                content: "exit 1".into(),
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: true,
                failure: None,
            },
        },
    ));

    assert_eq!(state.transcript().len(), 1);
    assert!(matches!(
        &state.transcript()[0],
        TranscriptEntry::Tool(tool)
            if tool.tool_name == "shell"
                && tool.settled_result.as_deref() == Some("exit 1")
                && tool.state == ToolState::Failed
    ));
}

#[test]
fn ctrl_o_opens_a_full_transcript_detail_viewer_and_preserves_live_state() {
    let mut state = AppState::new();
    state.welcome_line();
    state.push_entry(
        None,
        TranscriptEntry::User {
            text: "inspect this".into(),
        },
    );
    let call_id = ToolCallId::new("call-compact").expect("fixture ID");
    state.apply_event(&tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(1),
        kind: tea_core::event::AgentEventKind::ToolExecutionStart {
            tool_call_id: call_id,
            tool_name: "read".into(),
            arguments: SerializedJson::new(r#"{"path":"src/lib.rs"}"#),
        },
    });
    let TranscriptEntry::Tool(tool) = &state.transcript()[2] else {
        panic!("expected tool entry")
    };
    assert_eq!(tool.tool_name, "read");
    state.toggle_tool_detail();
    assert_eq!(state.surface(), UiSurface::ToolDetail);
    assert!(state.surface_lines().is_some_and(|lines| {
        lines.iter().any(|line| line == "Welcome")
            && lines.iter().any(|line| line == "User")
            && lines.iter().any(|line| line == "Tool: read (Started)")
            && lines.iter().any(|line| line.contains("src/lib.rs"))
    }));
    let grid = crate::render::render(&state, &tea_core::provider::ProviderRegistry::new(), 40, 10);
    let title = (0..40)
        .filter_map(|column| grid.get(column, 2))
        .map(|cell| cell.symbol)
        .collect::<String>();
    assert!(title.starts_with("Full detail"));
    assert_eq!(
        crate::render::composer_cursor_position(&state, 40, 10),
        Some((2, 0))
    );
    state.page_surface_down(2);
    assert_eq!(state.surface_offset(), 2);
    state.page_surface_up(1);
    assert_eq!(state.surface_offset(), 1);
    state.toggle_tool_detail();
    assert_eq!(state.surface(), UiSurface::None);
    assert_eq!(state.surface_offset(), 0);
    assert_eq!(state.transcript().len(), 3);
}

#[test]
fn temporary_surface_payload_does_not_enter_or_survive_transcript_close() {
    let mut state = AppState::new();
    state.set_surface_lines(UiSurface::Help, vec!["help text".into()]);
    assert_eq!(state.transcript().len(), 0);
    assert_eq!(
        state.surface_lines().map(|lines| lines.to_vec()),
        Some(vec!["help text".to_owned()])
    );
    let grid = crate::render::render(&state, &tea_core::provider::ProviderRegistry::new(), 20, 6);
    assert_eq!(grid.get(0, 0).expect("surface rail").symbol, '┃');
    assert_eq!(grid.get(0, 1).expect("surface divider").symbol, '─');
    assert_eq!(grid.get(0, 2).expect("surface payload").symbol, 'h');
    assert_eq!(
        crate::render::composer_cursor_position(&state, 20, 6),
        Some((2, 0))
    );
    state.close_surface();
    assert_eq!(state.surface(), UiSurface::None);
    assert!(state.surface_lines().is_none());
}

#[test]
fn event_projection_makes_provider_failure_and_abort_explicit() {
    let mut state = AppState::new();
    state.apply_event(&tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(1),
        kind: tea_core::AgentEventKind::MessageEnd {
            message: Message::Assistant {
                id: MessageId(2),
                content: String::new(),
                tool_calls: Vec::new(),
                stop_reason: Some(tea_core::state::StopReason::Error),
                error_message: Some("provider rejected the request".into()),
            },
        },
    });
    state.apply_event(&tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(2),
        kind: tea_core::AgentEventKind::TurnEnd {
            turn_id: tea_core::TurnId(1),
            reason: tea_core::state::StopReason::Aborted,
        },
    });

    assert_eq!(
        state
            .transcript()
            .iter()
            .map(|entry| match entry {
                TranscriptEntry::Error { text }
                | TranscriptEntry::Notice { text, .. }
                | TranscriptEntry::Welcome { text }
                | TranscriptEntry::User { text }
                | TranscriptEntry::Assistant { text, .. } => text.as_str(),
                TranscriptEntry::Tool(_) => "tool",
            })
            .collect::<Vec<_>>(),
        ["provider rejected the request"]
    );
    assert_eq!(state.status(), &UiStatus::Notice("turn aborted".into()));
}

#[test]
fn accounting_does_not_render_unknown_as_zero() {
    assert_eq!(
        format_usage(&Usage::default()),
        "provider reported no accounting"
    );
    assert_eq!(
        format_usage(&Usage {
            output_tokens: Some(0),
            ..Usage::default()
        }),
        "out 0"
    );
    assert_eq!(
        support::format_footer_usage(&Usage::default()),
        "in unknown out unknown reasoning unknown cache-read unknown cache-write unknown cost unknown"
    );
    assert_eq!(
        support::format_footer_usage(&Usage {
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(7),
            cost: Some("0.000001".into()),
            ..Usage::default()
        }),
        "in unknown out unknown reasoning unknown cache-read 0 cache-write 7 cost 0.000001"
    );
}

#[test]
fn usage_events_update_footer_projection_without_transcript_noise() {
    let mut state = AppState::new();
    state.apply_event(&tea_core::AgentEvent {
        run_id: tea_core::RunId(1),
        sequence: tea_core::EventSequence(1),
        kind: tea_core::event::AgentEventKind::ModelTurnUsage {
            accounting: tea_core::state::ModelTurnAccounting {
                run_id: tea_core::RunId(1),
                turn_id: tea_core::TurnId(1),
                model: None,
                usage: Usage {
                    output_tokens: Some(3),
                    ..Usage::default()
                },
            },
        },
    });
    assert!(state.transcript().is_empty());
    assert!(state.footer_lines(&tea_core::provider::ProviderRegistry::new())[1].contains("out 3"));
}

#[test]
fn footer_reports_unknown_context_and_unavailable_compaction_without_guessing() {
    let state = AppState::new();
    let registry = tea_core::provider::ProviderRegistry::new();
    assert_eq!(
        state.footer_lines(&registry)[1],
        "context unknown% used (unknown/unknown); automatic compaction unavailable"
    );
}

#[test]
fn footer_reports_catalog_context_capacity_for_selected_model() {
    let mut state = AppState::new();
    state.selected_model = Some(ModelDescriptor {
        provider: "openrouter".into(),
        model: "poolside/laguna-xs-2.1:free".into(),
        revision: None,
    });
    let registry = tea_core::provider::ProviderRegistry::new();
    assert_eq!(
        state.footer_lines(&registry)[1],
        "context unknown% used (unknown/262144); automatic compaction unavailable"
    );
}

#[test]
fn footer_reports_context_percentage_and_enabled_compaction() {
    let mut state = AppState::new();
    state.selected_model = Some(ModelDescriptor {
        provider: "openrouter".into(),
        model: "poolside/laguna-xs-2.1:free".into(),
        revision: None,
    });
    state.automatic_compaction_enabled = true;
    state.context_estimate = Some(ContextEstimate {
        tokens: Some(131_072),
        message_count: 4,
    });
    let registry = tea_core::provider::ProviderRegistry::new();
    assert_eq!(
        state.footer_lines(&registry)[1],
        "context 50% used (131072/262144; 4 messages); automatic compaction available"
    );
}

#[test]
fn local_compactor_summarizes_and_preserves_the_core_retained_suffix() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
            revision: None,
        };
        let compactor = ProviderCompactor::default();
        compactor.configure(
            model.clone(),
            Arc::new(SummaryProvider {
                expected_model: model.clone(),
            }),
        );
        let prefix = Message::User {
            id: MessageId(1),
            content: "old work".into(),
        };
        let retained = Message::Assistant {
            id: MessageId(2),
            content: "recent work".into(),
            tool_calls: Vec::new(),
            stop_reason: Some(tea_core::state::StopReason::EndTurn),
            error_message: None,
        };
        let request = AutomaticCompactionRequest {
            reason: AutomaticCompactionReason::Threshold,
            estimated_tokens_before: Some(300_000),
            context_budget_tokens: 32_768,
            reserved_tokens: 8_192,
            recent_tokens: 20_000,
            prefix_messages: vec![prefix.clone()],
            retained_messages: vec![retained.clone()],
            split_turn_prefix: Vec::new(),
            retry_provider_request: false,
        };
        let result = compactor
            .compact_automatic(
                CompactionContext {
                    version: tea_core::COMPACTION_CONTEXT_VERSION,
                    system_prompt: String::new(),
                    model: Some(model),
                    messages: vec![prefix, retained.clone()],
                    source_history_revision: 0,
                    host_messages: Vec::new(),
                    provider_context: None,
                },
                request,
                CancellationToken::new(),
            )
            .await
            .expect("provider-backed compaction succeeds");
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(
            &result.messages[0],
            Message::User { content, .. } if content.contains("summary text")
        ));
        assert_eq!(result.messages[1], retained);
    });
}

#[test]
fn cache_friendly_compaction_appends_one_instruction_to_an_exact_source_prefix() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
            revision: None,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let compactor = ProviderCompactor::default();
        compactor.configure(
            model.clone(),
            Arc::new(RecordingSummaryProvider {
                expected_model: model.clone(),
                requests: Arc::clone(&requests),
            }),
        );
        let source = r#"[{"content":"old work","role":"user"}]"#;
        let active =
            r#"[{"content":"old work","role":"user"},{"content":"retained","role":"assistant"}]"#;
        let result = compactor
            .compact_automatic(
                CompactionContext {
                    version: tea_core::COMPACTION_CONTEXT_VERSION,
                    system_prompt: "unused standalone prompt".into(),
                    model: Some(model),
                    messages: vec![Message::User {
                        id: MessageId(1),
                        content: "old work".into(),
                    }],
                    source_history_revision: 0,
                    host_messages: Vec::new(),
                    provider_context: Some(ProviderContext {
                        system_prompt: "active system".into(),
                        context: source.into(),
                        active_context: Some(active.into()),
                        tools: vec![ToolDefinition {
                            name: "read".into(),
                            description: "read a workspace file".into(),
                            schema: tea_protocol::JsonValue::object([(
                                "type",
                                tea_protocol::JsonValue::from("object"),
                            )]),
                            execution_mode: ToolExecutionMode::Parallel,
                        }],
                    }),
                },
                AutomaticCompactionRequest {
                    reason: AutomaticCompactionReason::Threshold,
                    estimated_tokens_before: Some(30_000),
                    context_budget_tokens: 100_000,
                    reserved_tokens: 1_000,
                    recent_tokens: 20_000,
                    prefix_messages: vec![Message::User {
                        id: MessageId(1),
                        content: "old work".into(),
                    }],
                    retained_messages: Vec::new(),
                    split_turn_prefix: Vec::new(),
                    retry_provider_request: false,
                },
                CancellationToken::new(),
            )
            .await
            .expect("cache-friendly compaction succeeds");
        assert!(matches!(
            &result.messages[0],
            Message::User { content, .. } if content.contains("updated summary")
        ));
        let requests = requests.lock().expect("summary request mutex poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].system_prompt, "active system");
        assert_eq!(requests[0].tools.len(), 1);
        assert_eq!(requests[0].tools[0].name, "read");
        let converted =
            tea_protocol::JsonValue::parse(&requests[0].context).expect("summary context is JSON");
        let tea_protocol::JsonValue::Array(messages) = converted else {
            panic!("summary context is not a message array")
        };
        assert_eq!(messages.len(), 2);
        assert!(messages[1]
            .get("content")
            .and_then(tea_protocol::JsonValue::as_str)
            .is_some_and(|content| content.contains("Update the existing compacted summary")));
    });
}

#[test]
fn cache_friendly_compaction_falls_back_when_a_transform_breaks_the_prefix() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
            revision: None,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let compactor = ProviderCompactor::default();
        compactor.configure(
            model.clone(),
            Arc::new(RecordingSummaryProvider {
                expected_model: model.clone(),
                requests: Arc::clone(&requests),
            }),
        );
        let result = compactor
            .compact_automatic(
                CompactionContext {
                    version: tea_core::COMPACTION_CONTEXT_VERSION,
                    system_prompt: "standalone".into(),
                    model: Some(model),
                    messages: vec![Message::User {
                        id: MessageId(1),
                        content: "old work".into(),
                    }],
                    source_history_revision: 0,
                    host_messages: Vec::new(),
                    provider_context: Some(ProviderContext {
                        system_prompt: "active system".into(),
                        context: r#"[{"content":"old work","role":"user"}]"#.into(),
                        active_context: Some(
                            r#"[{"content":"injected metadata","role":"user"},{"content":"old work","role":"user"}]"#.into(),
                        ),
                        tools: Vec::new(),
                    }),
                },
                AutomaticCompactionRequest {
                    reason: AutomaticCompactionReason::Threshold,
                    estimated_tokens_before: Some(30_000),
                    context_budget_tokens: 100_000,
                    reserved_tokens: 1_000,
                    recent_tokens: 20_000,
                    prefix_messages: vec![Message::User {
                        id: MessageId(1),
                        content: "old work".into(),
                    }],
                    retained_messages: Vec::new(),
                    split_turn_prefix: Vec::new(),
                    retry_provider_request: false,
                },
                CancellationToken::new(),
            )
            .await
            .expect("standalone fallback succeeds");
        assert!(matches!(
            &result.messages[0],
            Message::User { content, .. } if content.contains("updated summary")
        ));
        let requests = requests.lock().expect("summary request mutex poisoned");
        assert_eq!(requests.len(), 1);
        assert!(requests[0]
            .system_prompt
            .contains("You compact coding-agent conversation history"));
        assert!(!requests[0].system_prompt.contains("active system"));
    });
}

#[test]
fn tool_free_replay_is_a_distinct_opt_in_provider_compatibility_candidate() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
            revision: None,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let compactor =
            ProviderCompactor::with_strategy(ProviderCompactionStrategy::ToolFreeReplaySummaryV1);
        compactor.configure(
            model.clone(),
            Arc::new(RecordingSummaryProvider {
                expected_model: model.clone(),
                requests: Arc::clone(&requests),
            }),
        );
        let source = r#"[{"content":"old work","role":"user"}]"#;
        let result = compactor
            .compact_automatic(
                CompactionContext {
                    version: tea_core::COMPACTION_CONTEXT_VERSION,
                    system_prompt: String::new(),
                    model: Some(model),
                    messages: vec![Message::User {
                        id: MessageId(1),
                        content: "old work".into(),
                    }],
                    source_history_revision: 0,
                    host_messages: Vec::new(),
                    provider_context: Some(ProviderContext {
                        system_prompt: "active system".into(),
                        context: source.into(),
                        active_context: Some(source.into()),
                        tools: vec![ToolDefinition {
                            name: "read".into(),
                            description: "read a workspace file".into(),
                            schema: tea_protocol::JsonValue::object([(
                                "type",
                                tea_protocol::JsonValue::from("object"),
                            )]),
                            execution_mode: ToolExecutionMode::Parallel,
                        }],
                    }),
                },
                AutomaticCompactionRequest {
                    reason: AutomaticCompactionReason::Threshold,
                    estimated_tokens_before: Some(30_000),
                    context_budget_tokens: 100_000,
                    reserved_tokens: 1_000,
                    recent_tokens: 20_000,
                    prefix_messages: vec![Message::User {
                        id: MessageId(1),
                        content: "old work".into(),
                    }],
                    retained_messages: Vec::new(),
                    split_turn_prefix: Vec::new(),
                    retry_provider_request: false,
                },
                CancellationToken::new(),
            )
            .await
            .expect("tool-free replay succeeds");
        assert_eq!(
            compactor.strategy().id,
            tea_core::TOOL_FREE_REPLAY_SUMMARY_V1
        );
        assert_eq!(
            result.request_layout,
            Some(CompactionRequestLayout::ExactReplay)
        );
        assert_eq!(result.source_is_active_context_prefix, Some(true));
        assert!(requests.lock().expect("request mutex")[0].tools.is_empty());
    });
}

#[test]
fn structured_and_incremental_candidates_emit_marker_and_distinct_layouts() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
            revision: None,
        };
        let structured_requests = Arc::new(Mutex::new(Vec::new()));
        let structured =
            ProviderCompactor::with_strategy(ProviderCompactionStrategy::StructuredCheckpointV1);
        structured.configure(
            model.clone(),
            Arc::new(RecordingSummaryProvider {
                expected_model: model.clone(),
                requests: Arc::clone(&structured_requests),
            }),
        );
        let first = Message::User {
            id: MessageId(1),
            content: "inspect src/main.rs then run tests".into(),
        };
        let structured_result = structured
            .compact(
                CompactionContext {
                    version: tea_core::COMPACTION_CONTEXT_VERSION,
                    system_prompt: String::new(),
                    model: Some(model.clone()),
                    messages: vec![first.clone()],
                    source_history_revision: 0,
                    host_messages: Vec::new(),
                    provider_context: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("structured candidate succeeds");
        assert_eq!(structured.strategy().id, tea_core::STRUCTURED_CHECKPOINT_V1);
        assert!(matches!(
            &structured_result.messages[0],
            Message::User { content, .. } if content.starts_with("<!-- tea-checkpoint:v1 generation=1 -->")
        ));
        assert!(structured_requests.lock().expect("request mutex")[0]
            .system_prompt
            .contains("durable checkpoint"));

        let prior = structured_result.messages[0].clone();
        let incremental_requests = Arc::new(Mutex::new(Vec::new()));
        let incremental = ProviderCompactor::with_strategy(
            ProviderCompactionStrategy::IncrementalCheckpointUpdateV1,
        );
        incremental.configure(
            model.clone(),
            Arc::new(RecordingSummaryProvider {
                expected_model: model.clone(),
                requests: Arc::clone(&incremental_requests),
            }),
        );
        let incremental_result = incremental
            .compact(
                CompactionContext {
                    version: tea_core::COMPACTION_CONTEXT_VERSION,
                    system_prompt: String::new(),
                    model: Some(model),
                    messages: vec![
                        prior,
                        Message::User {
                            id: MessageId(2),
                            content: "new constrained work".into(),
                        },
                    ],
                    source_history_revision: 0,
                    host_messages: Vec::new(),
                    provider_context: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("incremental candidate succeeds");
        assert_eq!(
            incremental.strategy().id,
            tea_core::INCREMENTAL_CHECKPOINT_UPDATE_V1
        );
        assert_eq!(
            incremental_result.request_layout,
            Some(CompactionRequestLayout::IncrementalCheckpointUpdate)
        );
        assert!(matches!(
            &incremental_result.messages[0],
            Message::User { content, .. } if content.starts_with("<!-- tea-checkpoint:v1 generation=2 -->")
        ));
        assert!(incremental_requests.lock().expect("request mutex")[0]
            .context
            .contains("Previous tea checkpoint"));
    });
}

#[test]
fn local_catalog_selection_enables_automatic_compaction() {
    let workspace = std::env::current_dir().expect("test workspace");
    let tools = DefaultCodingTools::new(workspace).expect("default tools");
    let compactor = Arc::new(ProviderCompactor::default());
    let compactor_capability: Arc<dyn Compactor> = compactor.clone();
    let agent = build_host_agent(tools)
        .expect("host agent builder")
        .compactor(compactor_capability)
        .build();
    let mut app = App::new(CliOptions::default());
    app.attach_agent(agent);
    app.compactor = Some(compactor);

    app.select_model(
        "local".into(),
        tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
    )
    .expect("local model selection");

    let policy = app.agent().expect("attached agent").automatic_compaction();
    assert!(policy.enabled);
    assert_eq!(policy.context_budget.tokens(), 32_768);
    assert_eq!(policy.reserved_tokens, 8_192);
    assert_eq!(policy.recent_tokens, 16_384);
    assert_eq!(policy.overflow_recovery, OverflowRecovery::CompactAndRetry);
    assert_eq!(policy.max_compactions_per_run, 4);
    assert_eq!(policy.max_overflow_retries_per_run, 1);
    assert_eq!(
        app.state().footer_lines(&app.registry)[1],
        "context unknown% used (unknown/32768); automatic compaction available"
    );
}

#[test]
fn custom_local_model_enables_automatic_compaction_with_explicit_capacity() {
    let workspace = std::env::current_dir().expect("test workspace");
    let tools = DefaultCodingTools::new(workspace).expect("default tools");
    let compactor = Arc::new(ProviderCompactor::default());
    let compactor_capability: Arc<dyn Compactor> = compactor.clone();
    let agent = build_host_agent(tools)
        .expect("host agent builder")
        .compactor(compactor_capability)
        .build();
    let options = CliOptions::parse(
        [
            "tea",
            "--provider",
            "local",
            "--model",
            "Qwen3.5-4B-MLX-4bit",
            "--local-context-window",
            "32768",
        ]
        .map(OsString::from),
    )
    .expect("local capacity options parse");
    let mut app = App::new(options);
    app.attach_agent(agent);
    app.compactor = Some(compactor);
    app.select_model("local".into(), "Qwen3.5-4B-MLX-4bit".into())
        .expect("custom local model selection");

    let policy = app.agent().expect("attached agent").automatic_compaction();
    assert!(policy.enabled);
    assert_eq!(policy.context_budget.tokens(), 32_768);
    assert_eq!(
        app.state().footer_lines(&app.registry)[1],
        "context unknown% used (unknown/32768); automatic compaction available"
    );
}

#[test]
fn civil_date_epoch_is_stable_without_a_time_dependency() {
    assert_eq!(support::civil_from_days(0), (1970, 1, 1));
    assert_eq!(support::civil_from_days(20_000), (2024, 10, 4));
}

#[test]
fn headless_host_agent_sends_openai_compatible_context() {
    smol::block_on(async {
        let workspace = std::env::current_dir().expect("test workspace");
        let tools = DefaultCodingTools::new(workspace).expect("default tools");
        let agent = build_host_agent(tools)
            .expect("host agent builder")
            .model(ModelDescriptor {
                provider: "openrouter".into(),
                model: "inclusionai/ling-3.0-tiny:free".into(),
                revision: None,
            })
            .model_provider(Arc::new(ContextCheckingProvider))
            .build();

        agent
            .start_prompt("hello")
            .expect("start prompt")
            .drive()
            .await
            .expect("headless host request should be valid JSON");
    });
}

#[test]
fn local_provider_is_selectable_without_a_credential() {
    let workspace = std::env::current_dir().expect("test workspace");
    let tools = DefaultCodingTools::new(workspace).expect("default tools");
    let agent = build_host_agent(tools).expect("host agent builder").build();
    let mut app = App::new(CliOptions::default());
    app.attach_agent(agent);

    app.select_model(
        "local".into(),
        tea_core::provider::local::LAGUNA_XS_2_1_MODEL.into(),
    )
    .expect("local provider should configure without a key");

    assert_eq!(
        app.state()
            .selected_model
            .as_ref()
            .map(|model| (model.provider.as_str(), model.model.as_str(),)),
        Some(("local", "Laguna-XS-2.1-5bit"))
    );
    assert!(app.agent().expect("attached agent").has_model_provider());
}

#[test]
fn local_provider_accepts_an_explicit_api_root() {
    let workspace = std::env::current_dir().expect("test workspace");
    let tools = DefaultCodingTools::new(workspace).expect("default tools");
    let agent = build_host_agent(tools).expect("host agent builder").build();
    let options = CliOptions::parse(
        [
            "tea",
            "--provider",
            "local",
            "--model",
            "Qwen3.5-4B-MLX-4bit",
            "--local-base-url",
            "http://127.0.0.1:12345/v1",
        ]
        .map(OsString::from),
    )
    .expect("local endpoint options parse");
    let mut app = App::new(options);
    app.attach_agent(agent);
    app.select_model("local".into(), "Qwen3.5-4B-MLX-4bit".into())
        .expect("local provider should accept explicit endpoint");
    assert_eq!(
        app.options().local_base_url(),
        Some(std::ffi::OsStr::new("http://127.0.0.1:12345/v1"))
    );
}

#[test]
fn clear_refuses_an_active_core_agent_without_cancelling_it() {
    let agent = tea_core::Agent::builder().build();
    let active = agent.start_prompt("active").expect("run starts");
    let mut app = App::new(CliOptions::default());
    app.attach_agent(agent.clone());

    app.dispatch_command("/clear").expect("command is handled");

    assert!(matches!(
        app.state().status(),
        UiStatus::Notice(notice) if notice == "/clear is unavailable while a run is active"
    ));
    assert!(matches!(
        agent.snapshot().phase,
        tea_core::state::AgentPhase::Running(_)
    ));
    active.abort().expect("fixture cleanup");
}

#[test]
fn active_commands_refuse_without_replacing_or_compacting_the_agent() {
    let agent = tea_core::Agent::builder().build();
    let active = agent.start_prompt("active").expect("run starts");
    let mut app = App::new(CliOptions::default());
    app.attach_agent(agent.clone());

    for command in [
        "/model",
        "/compact",
        "/session",
        "/resume",
        "/new",
        "/reload-extensions",
        "/clear",
    ] {
        app.dispatch_command(command).expect("command is handled");
        assert!(matches!(
            app.state().status(),
            UiStatus::Notice(notice) if notice == &format!("{command} is unavailable while a run is active")
        ));
        assert!(matches!(
            agent.snapshot().phase,
            tea_core::state::AgentPhase::Running(_)
        ));
    }
    active.abort().expect("fixture cleanup");
}

#[test]
fn queue_commands_project_core_owned_steering_and_follow_up_prompts() {
    let agent = tea_core::Agent::builder().build();
    let mut app = App::new(CliOptions::default());
    app.attach_agent(agent.clone());

    app.dispatch_command("/steer inspect the error")
        .expect("steering command is handled");
    app.dispatch_command("/followup summarize the result")
        .expect("follow-up command is handled");

    assert_eq!(
        app.state()
            .queued_lines()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "queued steering #1 (next active turn; one at a time): inspect the error",
            "queued follow-up #1 (next idle boundary; one at a time): summarize the result",
        ]
    );
    let queues = agent.queue_snapshot();
    assert_eq!(queues.steering.snapshot()[0].content, "inspect the error");
    assert_eq!(
        queues.follow_up.snapshot()[0].content,
        "summarize the result"
    );
}

#[test]
fn provider_failure_restores_the_submitted_prompt_for_an_explicit_resubmit() {
    let mut app = App::new(CliOptions::default());
    app.submitted_prompt = Some("inspect the failing test".into());
    let (sender, receiver) = sync_channel(1);
    sender
        .send(Err(tea_core::CoreError::ModelError {
            message: "rate limited".into(),
        }))
        .expect("test receiver remains open");
    app.active_task = Some(receiver);

    app.reap_task();

    assert_eq!(app.state().composer().text(), "inspect the failing test");
    assert!(matches!(
        app.state().status(),
        UiStatus::Notice(notice) if notice.contains("prompt restored for explicit re-submit")
    ));
}

#[test]
fn prompt_history_returns_to_the_live_draft_after_navigation() {
    let mut state = AppState::new();
    state.record_history("first prompt");
    state.record_history("second prompt");
    state.record_history("second prompt");

    assert_eq!(state.history_previous().as_deref(), Some("second prompt"));
    assert_eq!(state.history_previous().as_deref(), Some("first prompt"));
    assert_eq!(state.history_next().as_deref(), Some("second prompt"));
    assert_eq!(state.history_next().as_deref(), Some(""));
    assert_eq!(state.history_next(), None);
}

#[test]
fn command_completion_expands_a_slash_prefix_without_submitting_it() {
    let mut app = App::new(CliOptions::default());
    app.state.composer_mut().insert_str("/hel").expect("prefix");

    app.complete_command();

    assert_eq!(app.state.composer().text(), "/help ");
}

#[test]
fn command_completion_includes_extension_reload() {
    let mut app = App::new(CliOptions::default());
    app.state
        .composer_mut()
        .insert_str("/reload-e")
        .expect("prefix");

    app.complete_command();

    assert_eq!(app.state.composer().text(), "/reload-extensions ");
}

#[test]
fn command_completion_includes_linear_session_commands() {
    let mut app = App::new(CliOptions::default());
    app.state
        .composer_mut()
        .insert_str("/sess")
        .expect("prefix");
    app.complete_command();
    assert_eq!(app.state.composer().text(), "/session ");

    app.state.composer_mut().clear();
    app.state.composer_mut().insert_str("/new").expect("prefix");
    app.complete_command();
    assert_eq!(app.state.composer().text(), "/new ");
}
