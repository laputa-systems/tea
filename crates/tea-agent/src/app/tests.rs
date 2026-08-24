use super::compaction::ProviderCompactor;
use super::state::ContextEstimate;
use super::*;
use std::ffi::OsString;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tea_core::compaction::{
    AutomaticCompactionReason, AutomaticCompactionRequest, CompactionContext, Compactor,
    OverflowRecovery, ProviderContext,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentMessage, MessageId, SerializedJson, ToolCallId};
use tea_core::state::{ModelDescriptor, ThinkingLevel, Usage};
use tea_core::tool::AgentToolResult;
use tea_core::tool::ToolUpdate;
use tea_core::tool::{ToolDefinition, ToolExecutionMode};
use tea_session::{
    ArtifactStore, CustomEntry, DurabilityMode, EntryId, HarnessCatalogFact, JsonValue,
    JsonlSession, LaneId, Metadata, PayloadRef, ProvisionedEntry, SessionEntry, SessionFact,
    SessionHeader, SessionId, SessionWriter,
};
use tea_tui::{Color, Size, Style, StyledLine};

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

fn style_at(line: &StyledLine, index: usize) -> Style {
    let mut consumed = 0;
    for span in line.spans() {
        let count = span.text.chars().count();
        if index < consumed + count {
            return span.style;
        }
        consumed += count;
    }
    panic!("style index {index} is outside line {:?}", line.text());
}

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
                ModelStreamEvent::End(tea_core::state::StopReason::Stop),
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
                ModelStreamEvent::End(tea_core::state::StopReason::Stop),
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
    assert!(matches!(
        CliOptions::parse(
            [
                "tea",
                "--compaction-strategy",
                "tool_free_replay_summary_v1"
            ]
            .map(OsString::from)
        ),
        Err(CliError::UnknownOption(_))
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
fn new_reaps_a_completed_task_but_never_drops_a_pending_task_receiver() {
    let options = CliOptions::parse(["tea"].map(OsString::from)).expect("test options parse");
    let mut app = App::new(options);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.durable_task = Some(receiver);

    // The special `/new` path reaches `new_session`, but an empty receiver
    // still owns a worker that can publish terminal durable effects.
    app.dispatch_command("/new")
        .expect("command dispatch succeeds");
    assert!(
        app.durable_task.is_some(),
        "pending task ownership is retained"
    );
    assert_eq!(
        app.state.footer_notice(),
        Some(("new session requires an idle agent", false))
    );

    sender
        .send(Ok(()))
        .expect("test task completion is delivered");
    app.dispatch_command("/new")
        .expect("completed task is reaped first");
    assert!(app.durable_task.is_none(), "terminal receiver is reaped");
    assert_eq!(
        app.state.footer_notice(),
        Some(("new session will begin with the next prompt", false))
    );
}

#[test]
fn cli_parses_explicit_machine_session_commands() {
    assert_eq!(
        CliOptions::parse_command(
            ["tea", "session", "inspect", "/tmp/session.tea"].map(OsString::from)
        ),
        Ok(CliCommand::Session(SessionCommand::Inspect {
            directory: PathBuf::from("/tmp/session.tea"),
        }))
    );
    assert_eq!(
        CliOptions::parse_command(
            ["tea", "session", "gc", "/tmp/session.tea", "--apply"].map(OsString::from)
        ),
        Ok(CliCommand::Session(SessionCommand::Gc {
            directory: PathBuf::from("/tmp/session.tea"),
            additional_roots: Vec::new(),
            apply: true,
        }))
    );
    assert_eq!(
        CliOptions::parse_command(
            ["tea", "session", "rebuild-meta", "/tmp/session.tea"].map(OsString::from)
        ),
        Ok(CliCommand::Session(SessionCommand::RebuildMeta {
            directory: PathBuf::from("/tmp/session.tea"),
        }))
    );
    assert!(matches!(
        CliOptions::parse_command(
            ["tea", "session", "unknown", "/tmp/session.tea"].map(OsString::from)
        ),
        Err(CliError::UnknownSessionOperation(_))
    ));
}

#[test]
fn machine_session_inspect_and_verify_emit_authenticated_json() {
    let home = test_tea_home("machine-session-command");
    let directory = home.join("session.tea");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("machine-session-command").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("machine-session-command-entry").expect("valid entry ID"),
                "inspect me",
            ),
        )
        .expect("entry commits");
    let expected_digest = session.snapshot().expect("snapshot").last_digest().to_hex();
    drop(session);

    for command in [
        SessionCommand::Inspect {
            directory: directory.clone(),
        },
        SessionCommand::Verify {
            directory: directory.clone(),
            additional_roots: Vec::new(),
        },
    ] {
        let output = run_session_command(command).expect("machine command succeeds");
        let fields = JsonValue::parse(&output)
            .expect("machine output is JSON")
            .as_object()
            .expect("machine output is an object")
            .clone();
        assert_eq!(
            fields.get("through_digest").and_then(JsonValue::as_str),
            Some(expected_digest.as_str())
        );
        assert_eq!(
            fields.get("through_seq").and_then(JsonValue::as_u64),
            Some(1)
        );
    }
    let _ = fs::remove_dir_all(home);
}

#[test]
fn machine_session_verify_reports_finalized_orphans_separately() {
    let home = test_tea_home("machine-session-verify-orphan");
    let directory = home.join("session.tea");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("machine-session-verify-orphan").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let artifact = session
        .artifact_store()
        .expect("object store opens")
        .put(b"unreferenced immutable evidence", "text/plain")
        .expect("orphan object publishes");
    drop(session);

    let output = run_session_command(SessionCommand::Verify {
        directory,
        additional_roots: Vec::new(),
    })
    .expect("verify succeeds with an orphan");
    let output = JsonValue::parse(&output).expect("machine output is JSON");
    let fields = output.as_object().expect("machine output is an object");
    assert_eq!(
        fields.get("artifact_count").and_then(JsonValue::as_u64),
        Some(0)
    );
    assert_eq!(
        fields
            .get("orphan_artifacts")
            .and_then(JsonValue::as_array)
            .expect("verify reports orphan array"),
        &[JsonValue::object([
            (
                "artifact_id",
                JsonValue::String(artifact.artifact_id.to_hex())
            ),
            ("byte_len", JsonValue::from(artifact.byte_len)),
        ])],
        "orphan reporting contains identities and lengths, never artifact bytes"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn machine_session_verify_rejects_an_invalid_immutable_harness_catalog() {
    let home = test_tea_home("machine-session-verify-harness-catalog");
    let directory = home.join("session.tea");
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("machine-session-verify-harness-catalog").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let catalog = session
        .artifact_store()
        .expect("object store opens")
        .put(br#"{\"not_a_harness_catalog\":true}"#, "application/json")
        .expect("invalid catalog bytes still publish as an immutable object");
    session
        .append_fact(SessionFact::HarnessCatalog(HarnessCatalogFact {
            schema_version: 1,
            artifact_id: catalog.artifact_id,
            byte_len: catalog.byte_len,
        }))
        .expect("catalog fact commits");
    drop(session);

    let error = run_session_command(SessionCommand::Verify {
        directory,
        additional_roots: Vec::new(),
    })
    .expect_err("operator verification rejects an invalid harness manifest");
    assert!(
        error.to_string().contains("harness catalog"),
        "verification reports the manifest contract rather than accepting a rehashed blob: {error}"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn machine_session_verify_reports_a_stale_head_cache_without_repairing_it() {
    let home = test_tea_home("machine-session-verify-head");
    let directory = home.join("session.tea");
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("machine-session-verify-head").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    drop(session);
    let head_path = directory.join("HEAD");
    std::fs::write(&head_path, "stale derived cache\n").expect("stale cache writes");

    let output = run_session_command(SessionCommand::Verify {
        directory: directory.clone(),
        additional_roots: Vec::new(),
    })
    .expect("read-only verify succeeds");
    let output = JsonValue::parse(&output).expect("machine output is JSON");
    assert_eq!(
        output
            .as_object()
            .and_then(|fields| fields.get("head_cache_current")),
        Some(&JsonValue::Bool(false)),
        "verify reports cache disagreement without making the cache authoritative"
    );
    assert_eq!(
        std::fs::read_to_string(&head_path).expect("HEAD rereads"),
        "stale derived cache\n",
        "read-only verification does not repair a derived cache"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn machine_session_verify_reports_stale_metadata_cache_without_repairing_it() {
    let home = test_tea_home("machine-session-verify-meta");
    let directory = home.join("session.tea");
    let mut metadata = Metadata::new();
    metadata.insert(
        "tea.self_extension_mode".into(),
        JsonValue::String("off".into()),
    );
    metadata.insert("tea.thinking".into(), JsonValue::String("medium".into()));
    let session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("machine-session-verify-meta").expect("valid session ID"),
            "workspace-test",
            metadata,
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    drop(session);
    run_session_command(SessionCommand::RebuildMeta {
        directory: directory.clone(),
    })
    .expect("metadata cache rebuilds");
    let current_output = run_session_command(SessionCommand::Verify {
        directory: directory.clone(),
        additional_roots: Vec::new(),
    })
    .expect("verification accepts the reconstructed cache");
    let current_output = JsonValue::parse(&current_output).expect("machine output is JSON");
    assert_eq!(
        current_output
            .as_object()
            .and_then(|fields| fields.get("metadata_cache_current")),
        Some(&JsonValue::Bool(true)),
        "rebuild-meta produces a cache for the current authoritative prefix"
    );
    let metadata_path = directory.join("meta.json");
    let stale_metadata = std::fs::read_to_string(&metadata_path)
        .expect("metadata cache reads")
        .replacen("\"through_seq\":0", "\"through_seq\":1", 1);
    assert_ne!(
        stale_metadata,
        std::fs::read_to_string(&metadata_path).expect("metadata cache rereads"),
        "fixture changes one valid cache field while keeping its schema intact"
    );
    std::fs::write(&metadata_path, &stale_metadata).expect("stale metadata writes");

    let output = run_session_command(SessionCommand::Verify {
        directory: directory.clone(),
        additional_roots: Vec::new(),
    })
    .expect("read-only verify succeeds");
    let output = JsonValue::parse(&output).expect("machine output is JSON");
    assert_eq!(
        output
            .as_object()
            .and_then(|fields| fields.get("metadata_cache_current")),
        Some(&JsonValue::Bool(false)),
        "verify diagnoses a stale metadata cache without trusting it"
    );
    assert_eq!(
        std::fs::read_to_string(&metadata_path).expect("metadata cache rereads"),
        stale_metadata,
        "read-only verification does not repair a derived metadata cache"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn machine_session_export_and_restore_preserve_the_authenticated_prefix() {
    let home = test_tea_home("machine-session-restore");
    let source = home.join("source.tea");
    let export = home.join("export.tea");
    let restore = home.join("restore.tea");
    let mut session = JsonlSession::create(
        &source,
        SessionHeader::new(
            SessionId::new("machine-session-restore").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("machine-session-restore-entry").expect("valid entry ID"),
                "portable evidence",
            ),
        )
        .expect("entry commits");
    let expected = session.snapshot().expect("snapshot");
    drop(session);

    run_session_command(SessionCommand::Export {
        source: source.clone(),
        destination: export.clone(),
        additional_roots: Vec::new(),
    })
    .expect("machine export succeeds");
    let manifest_path = export.join("export.json");
    let manifest = fs::read(&manifest_path).expect("export manifest reads");
    fs::write(&manifest_path, "not a canonical export manifest\n")
        .expect("corrupt export manifest writes");
    assert!(
        run_session_command(SessionCommand::Restore {
            source: export.clone(),
            destination: restore.clone(),
        })
        .is_err(),
        "restore validates its manifest before it prepares a destination"
    );
    assert!(
        !restore.exists(),
        "a rejected export manifest never leaves a published restore destination"
    );
    fs::write(&manifest_path, manifest).expect("valid export manifest restores");
    run_session_command(SessionCommand::Restore {
        source: export,
        destination: restore.clone(),
    })
    .expect("machine restore succeeds");
    let restored = JsonlSession::open(&restore, DurabilityMode::Strict).expect("restored opens");
    assert_eq!(restored.snapshot().expect("restored snapshot"), expected);
    drop(restored);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn machine_session_export_manifest_lists_artifact_identities_and_lengths() {
    let home = test_tea_home("machine-session-export-manifest-lengths");
    let source = home.join("source.tea");
    let export = home.join("export.tea");
    let mut session = JsonlSession::create(
        &source,
        SessionHeader::new(
            SessionId::new("machine-session-export-manifest-lengths").expect("valid session ID"),
            "workspace-test",
            Metadata::new(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    let artifact = session
        .artifact_store()
        .expect("object store opens")
        .put(b"immutable exported evidence", "text/plain")
        .expect("artifact publishes");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("machine-session-export-manifest-entry").expect("valid entry ID"),
                body: SessionEntry::Custom(CustomEntry {
                    type_name: "trusted.export-evidence".into(),
                    payload: PayloadRef::Artifact {
                        artifact_id: artifact.artifact_id,
                        byte_len: artifact.byte_len,
                        media_type: artifact.media_type.clone(),
                    },
                    model_visible: false,
                }),
            },
        )
        .expect("artifact reference commits");
    drop(session);

    run_session_command(SessionCommand::Export {
        source,
        destination: export.clone(),
        additional_roots: Vec::new(),
    })
    .expect("machine export succeeds");
    let manifest = JsonValue::parse(
        &fs::read_to_string(export.join("export.json")).expect("export manifest reads"),
    )
    .expect("export manifest is JSON");
    let fields = manifest.as_object().expect("export manifest is an object");
    assert_eq!(
        fields.get("artifacts"),
        Some(&JsonValue::Array(vec![JsonValue::object([
            (
                "artifact_id",
                JsonValue::String(artifact.artifact_id.to_hex())
            ),
            ("byte_len", JsonValue::from(artifact.byte_len)),
        ])])),
        "an export manifest carries complete immutable-object descriptors"
    );

    let _ = fs::remove_dir_all(home);
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
fn startup_does_not_open_model_picker_without_an_explicit_selection() {
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

    app.assemble_host().expect("host should assemble");

    assert_eq!(app.state().surface(), UiSurface::None);
    assert!(app.state().selected_model.is_none());
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn session_resumption_restores_its_model_without_global_preferences() {
    let tea_home = test_tea_home("session-model");
    let workspace = tea_home.join("workspace");
    fs::create_dir(&workspace).expect("workspace should be created");
    let workspace = fs::canonicalize(workspace).expect("workspace should canonicalize");
    let first_options = CliOptions::parse(
        [
            "tea",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
            "--cwd",
            workspace.to_str().expect("UTF-8 workspace path"),
            "--provider",
            mock::PROVIDER_ID,
        ]
        .map(OsString::from),
    )
    .expect("first startup options parse");
    let mut first = App::new(first_options);
    first.assemble_host().expect("first host should assemble");
    let harness = first
        .ensure_durable_harness()
        .expect("first durable session should create");
    let session_id = harness
        .snapshot()
        .expect("created session snapshot")
        .header()
        .session_id
        .to_string();
    drop(harness);
    drop(first);

    assert!(
        !tea_home.join("last-model.json").exists(),
        "a model selection must not create a global preference"
    );
    let sessions = super::durable::list_host_sessions(&tea_home, &workspace)
        .expect("session should be listed");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session_id);
    let session_directory = tea_home
        .join("sessions")
        .join(
            fs::read_dir(tea_home.join("sessions"))
                .expect("workspace directory should be present")
                .next()
                .expect("workspace directory entry should be present")
                .expect("workspace directory should be readable")
                .file_name(),
        )
        .join(format!("{session_id}.tea"));
    fs::remove_file(session_directory.join("meta.json"))
        .expect("derived session metadata cache should be removable");
    fs::write(
        tea_home.join("last-model.json"),
        r#"{"model":"ignored-legacy-model","provider":"local","version":1}"#,
    )
    .expect("legacy global preference fixture should be written");

    let resumed_options = CliOptions::parse(
        [
            "tea",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
            "--cwd",
            workspace.to_str().expect("UTF-8 workspace path"),
        ]
        .map(OsString::from),
    )
    .expect("resumed startup options parse");
    let mut resumed = App::new(resumed_options);
    resumed.assemble_host().expect("resumed host should assemble");
    assert!(
        resumed.state().selected_model.is_none(),
        "a legacy global preference must not choose the startup model"
    );

    resumed
        .resume_session(&session_id)
        .expect("session resumption should restore its durable model");
    assert_eq!(
        resumed.state().selected_model.as_ref(),
        Some(&ModelDescriptor {
            provider: mock::PROVIDER_ID.into(),
            model: mock::DEFAULT_MODEL_ID.into(),
            revision: None,
        })
    );
    assert!(resumed.durable_harness.is_some());
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn event_projection_keeps_streaming_text_as_one_raw_line() {
    let mut state = AppState::new();
    let message = AgentMessage::Assistant {
        id: MessageId(2),
        content: "hello".into(),
        tool_calls: Vec::new(),
        stop_reason: None,
        error_message: None,
    };
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(1),
        kind: tea_core::event::AgentEventKind::MessageUpdate {
            message: message.clone(),
            text_delta: Some("hel".into()),
        },
    });
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(2),
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
    let event = |sequence, kind| tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(sequence),
        kind,
    };
    state.apply_event(&event(
        1,
        tea_core::event::AgentEventKind::ToolExecutionStart {
            tool_call_id: call_id.clone(),
            tool_name: "shell".into(),
            arguments: SerializedJson::new(r#"{"command":"cargo test"}"#),
        },
    ));
    state.apply_event(&event(
        2,
        tea_core::event::AgentEventKind::ToolExecutionUpdate {
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
        tea_core::event::AgentEventKind::ToolExecutionEnd {
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
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(1),
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
    let presentation = crate::render::surface_presentation(
        &state,
        &tea_providers::ProviderRegistry::new(),
        Size {
            width: 40,
            height: 10,
        },
    );
    assert!(presentation.lines[2].text().starts_with("Full detail"));
    assert_eq!(
        presentation.cursor,
        Some(tea_tui::Cursor {
            column: 2,
            row: 0,
            visible: true,
        })
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
    let presentation = crate::render::surface_presentation(
        &state,
        &tea_providers::ProviderRegistry::new(),
        Size {
            width: 20,
            height: 6,
        },
    );
    assert_eq!(presentation.lines[0].text().chars().next(), Some('┃'));
    assert_eq!(presentation.lines[1].text().chars().next(), Some('─'));
    assert_eq!(presentation.lines[2].text().chars().next(), Some('h'));
    assert_eq!(
        presentation.cursor,
        Some(tea_tui::Cursor {
            column: 2,
            row: 0,
            visible: true,
        })
    );
    state.close_surface();
    assert_eq!(state.surface(), UiSurface::None);
    assert!(state.surface_lines().is_none());
}

#[test]
fn event_projection_makes_provider_failure_and_abort_explicit() {
    let mut state = AppState::new();
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(1),
        kind: tea_core::event::AgentEventKind::MessageEnd {
            message: AgentMessage::Assistant {
                id: MessageId(2),
                content: String::new(),
                tool_calls: Vec::new(),
                stop_reason: Some(tea_core::state::StopReason::Error),
                error_message: Some("provider rejected the request".into()),
            },
        },
    });
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(2),
        kind: tea_core::event::AgentEventKind::TurnEnd {
            turn_id: tea_core::state::TurnId(1),
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
}

#[test]
fn usage_events_update_footer_projection_without_transcript_noise() {
    let mut state = AppState::new();
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(1),
        kind: tea_core::event::AgentEventKind::ModelTurnUsage {
            accounting: tea_core::state::ModelTurnAccounting {
                run_id: tea_core::state::RunId(1),
                turn_id: tea_core::state::TurnId(1),
                model: None,
                usage: Usage {
                    output_tokens: Some(3),
                    ..Usage::default()
                },
            },
        },
    });
    state.apply_event(&tea_core::event::AgentEvent {
        run_id: tea_core::state::RunId(1),
        sequence: tea_core::event::EventSequence(2),
        kind: tea_core::event::AgentEventKind::ModelTurnUsage {
            accounting: tea_core::state::ModelTurnAccounting {
                run_id: tea_core::state::RunId(1),
                turn_id: tea_core::state::TurnId(2),
                model: None,
                usage: Usage {
                    input_tokens: Some(5),
                    cache_read_tokens: Some(7),
                    cost: Some("0.25".into()),
                    ..Usage::default()
                },
            },
        },
    });
    assert!(state.transcript().is_empty());
    assert!(state.footer_lines(&tea_providers::ProviderRegistry::new())[1].contains("↑5"));
    assert!(state.footer_lines(&tea_providers::ProviderRegistry::new())[1].contains("↓3"));
    assert!(state.footer_lines(&tea_providers::ProviderRegistry::new())[1].contains("R7"));
    assert!(state.footer_lines(&tea_providers::ProviderRegistry::new())[1].contains("$0.25"));
}

#[test]
fn thinking_command_changes_the_footer_effort_setting() {
    let mut app = App::new(CliOptions::parse(["tea"].map(OsString::from)).expect("options"));

    app.dispatch_command("/thinking high")
        .expect("thinking command should dispatch");

    assert!(app.state().footer_lines(&app.registry)[0].contains("effort high"));
}

#[test]
fn thinking_command_without_a_value_reports_usage() {
    let mut app = App::new(CliOptions::parse(["tea"].map(OsString::from)).expect("options"));

    app.dispatch_command("/thinking")
        .expect("thinking command should dispatch");

    assert!(
        matches!(app.state().status(), UiStatus::Notice(text) if text.contains("/thinking <off|minimal"))
    );
}

#[test]
fn footer_reports_unknown_context_and_unavailable_compaction_without_guessing() {
    let state = AppState::new();
    let registry = tea_providers::ProviderRegistry::new();
    assert_eq!(state.footer_lines(&registry)[1], "ctx ?%/?");
}

#[test]
fn footer_reports_catalog_context_capacity_for_selected_model() {
    let mut state = AppState::new();
    state.selected_model = Some(ModelDescriptor {
        provider: "openrouter".into(),
        model: "poolside/laguna-xs-2.1:free".into(),
        revision: None,
    });
    let registry = tea_providers::ProviderRegistry::new();
    assert_eq!(state.footer_lines(&registry)[1], "ctx ?%/262k");
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
    let registry = tea_providers::ProviderRegistry::new();
    assert_eq!(state.footer_lines(&registry)[1], "ctx 50%/262k (auto)");
}

#[test]
fn local_compactor_summarizes_and_preserves_the_core_retained_suffix() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_providers::local::LAGUNA_XS_2_1_MODEL.into(),
            revision: None,
        };
        let compactor = ProviderCompactor::default();
        compactor.configure(
            model.clone(),
            Arc::new(SummaryProvider {
                expected_model: model.clone(),
            }),
        );
        let prefix = AgentMessage::User {
            id: MessageId(1),
            content: "old work".into(),
        };
        let retained = AgentMessage::Assistant {
            id: MessageId(2),
            content: "recent work".into(),
            tool_calls: Vec::new(),
            stop_reason: Some(tea_core::state::StopReason::Stop),
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
                    version: tea_core::compaction::COMPACTION_CONTEXT_VERSION,
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
            AgentMessage::User { content, .. } if content.contains("summary text")
        ));
        assert_eq!(result.messages[1], retained);
    });
}

#[test]
fn cache_friendly_compaction_appends_one_instruction_to_an_exact_source_prefix() {
    smol::block_on(async {
        let model = ModelDescriptor {
            provider: "local".into(),
            model: tea_providers::local::LAGUNA_XS_2_1_MODEL.into(),
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
                    version: tea_core::compaction::COMPACTION_CONTEXT_VERSION,
                    system_prompt: "unused standalone prompt".into(),
                    model: Some(model),
                    messages: vec![AgentMessage::User {
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
                            requires_exclusive_batch: false,
                            cancellation_settlement_mode:
                                tea_core::tool::CancellationSettlementMode::DropFuture,
                        }],
                    }),
                },
                AutomaticCompactionRequest {
                    reason: AutomaticCompactionReason::Threshold,
                    estimated_tokens_before: Some(30_000),
                    context_budget_tokens: 100_000,
                    reserved_tokens: 1_000,
                    recent_tokens: 20_000,
                    prefix_messages: vec![AgentMessage::User {
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
            AgentMessage::User { content, .. } if content.contains("updated summary")
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
            model: tea_providers::local::LAGUNA_XS_2_1_MODEL.into(),
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
                    version: tea_core::compaction::COMPACTION_CONTEXT_VERSION,
                    system_prompt: "standalone".into(),
                    model: Some(model),
                    messages: vec![AgentMessage::User {
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
                    prefix_messages: vec![AgentMessage::User {
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
            AgentMessage::User { content, .. } if content.contains("updated summary")
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
fn local_catalog_selection_enables_automatic_compaction() {
    let tea_home = test_tea_home("local-catalog");
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
    app.assemble_host().expect("host should assemble");

    app.select_model(
        "local".into(),
        tea_providers::local::LAGUNA_XS_2_1_MODEL.into(),
    )
    .expect("local model selection");

    let policy = &app.automatic_compaction;
    assert!(policy.enabled);
    assert_eq!(policy.context_budget.tokens(), 32_768);
    assert_eq!(policy.reserved_tokens, 8_192);
    assert_eq!(policy.recent_tokens, 16_384);
    assert_eq!(policy.overflow_recovery, OverflowRecovery::CompactAndRetry);
    assert_eq!(policy.max_compactions_per_run, 4);
    assert_eq!(policy.max_overflow_retries_per_run, 1);
    assert_eq!(
        app.state().footer_lines(&app.registry)[1],
        "ctx ?%/33k (auto)"
    );
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn custom_local_model_enables_automatic_compaction_with_explicit_capacity() {
    let tea_home = test_tea_home("custom-local");
    let options = CliOptions::parse(
        [
            "tea",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
            "--local-context-window",
            "32768",
        ]
        .map(OsString::from),
    )
    .expect("local capacity options parse");
    let mut app = App::new(options);
    app.assemble_host().expect("host should assemble");
    app.select_model("local".into(), "Qwen3.5-4B-MLX-4bit".into())
        .expect("custom local model selection");

    let policy = &app.automatic_compaction;
    assert!(policy.enabled);
    assert_eq!(policy.context_budget.tokens(), 32_768);
    assert_eq!(
        app.state().footer_lines(&app.registry)[1],
        "ctx ?%/33k (auto)"
    );
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn civil_date_epoch_is_stable_without_a_time_dependency() {
    assert_eq!(support::civil_from_days(0), (1970, 1, 1));
    assert_eq!(support::civil_from_days(20_000), (2024, 10, 4));
}

#[test]
fn local_provider_is_selectable_without_a_credential() {
    let tea_home = test_tea_home("local-provider");
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
    app.assemble_host().expect("host should assemble");

    app.select_model(
        "local".into(),
        tea_providers::local::LAGUNA_XS_2_1_MODEL.into(),
    )
    .expect("local provider should configure without a key");

    assert_eq!(
        app.state()
            .selected_model
            .as_ref()
            .map(|model| (model.provider.as_str(), model.model.as_str(),)),
        Some(("local", "Laguna-XS-2.1-5bit"))
    );
    assert!(app.configured_provider.is_some());
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn local_provider_accepts_an_explicit_api_root() {
    let tea_home = test_tea_home("local-api-root");
    let options = CliOptions::parse(
        [
            "tea",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
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
    app.assemble_host().expect("host should assemble");
    app.select_model("local".into(), "Qwen3.5-4B-MLX-4bit".into())
        .expect("local provider should accept explicit endpoint");
    assert_eq!(
        app.options().local_base_url(),
        Some(std::ffi::OsStr::new("http://127.0.0.1:12345/v1"))
    );
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn prompt_history_returns_to_the_live_draft_after_navigation() {
    let mut state = AppState::new();
    state.record_history("first prompt");
    state.record_history("second prompt");

    state.composer_mut().replace_from_editor("unfinished draft");
    state.begin_history_navigation();
    assert_eq!(state.history_previous().as_deref(), Some("second prompt"));
    assert_eq!(state.history_previous().as_deref(), Some("first prompt"));
    assert_eq!(state.history_next().as_deref(), Some("second prompt"));
    assert_eq!(state.history_next().as_deref(), Some("unfinished draft"));
    assert_eq!(state.history_next(), None);
}

#[test]
fn prompt_history_preserves_adjacent_durable_messages() {
    let mut state = AppState::new();
    state.record_history("repeat this");
    state.record_history("repeat this");

    state.begin_history_navigation();
    assert_eq!(state.history_previous().as_deref(), Some("repeat this"));
    assert_eq!(state.history_previous().as_deref(), Some("repeat this"));
}

#[test]
fn restored_session_user_messages_rebuild_prompt_history() {
    let mut state = AppState::new();
    state.record_history("from the prior terminal process");
    state.restore_messages(&[
        AgentMessage::User {
            id: MessageId(1),
            content: "durable first prompt".into(),
        },
        AgentMessage::Assistant {
            id: MessageId(2),
            content: "durable response".into(),
            tool_calls: Vec::new(),
            stop_reason: None,
            error_message: None,
        },
        AgentMessage::User {
            id: MessageId(3),
            content: "durable latest prompt".into(),
        },
    ]);

    state.begin_history_navigation();
    assert_eq!(
        state.history_previous().as_deref(),
        Some("durable latest prompt")
    );
    assert_eq!(
        state.history_previous().as_deref(),
        Some("durable first prompt")
    );
    assert_eq!(
        state.history_previous().as_deref(),
        Some("durable first prompt")
    );
}

#[test]
fn reverse_history_search_selects_messages_without_discarding_the_draft() {
    let mut state = AppState::new();
    state.record_history("inspect the session schema");
    state.record_history("repair the terminal renderer");
    state.record_history("inspect the terminal layout");
    state.composer_mut().replace_from_editor("unfinished draft");

    state.begin_or_advance_history_search();
    assert_eq!(state.composer().text(), "");
    state
        .composer_mut()
        .insert_str("inspect")
        .expect("search query is one line");
    state.reset_history_search_selection();
    let results = state
        .history_search_results()
        .expect("reverse search should be active");
    assert_eq!(
        results.matches,
        vec![
            "inspect the terminal layout".to_owned(),
            "inspect the session schema".to_owned(),
        ]
    );

    state.move_history_search(1);
    assert!(state.accept_history_search());
    assert_eq!(state.composer().text(), "inspect the session schema");
    assert!(!state.history_search_is_active());

    state.composer_mut().replace_from_editor("another draft");
    state.begin_or_advance_history_search();
    state.cancel_history_search();
    assert_eq!(state.composer().text(), "another draft");
}

#[test]
fn reverse_history_search_renders_highlighted_session_message_excerpts() {
    let mut state = AppState::new();
    state.record_history("review the persistence contract");
    state.record_history("ship highlighted history search excerpts");
    state.begin_or_advance_history_search();
    state
        .composer_mut()
        .insert_str("history")
        .expect("search query is one line");
    state.reset_history_search_selection();

    let presentation = crate::render::main_presentation(
        &state,
        &tea_providers::ProviderRegistry::new(),
        Size {
            width: 80,
            height: 12,
        },
        0,
    );
    let line = presentation
        .live
        .iter()
        .find(|line| line.text().contains("highlighted history search"))
        .expect("matching history excerpt should be visible");
    let highlighted = line
        .text()
        .find("history")
        .expect("excerpt should include the matching query");
    assert_eq!(style_at(line, highlighted).foreground, Some(Color::Cyan));
}

#[test]
fn queued_message_coalesces_and_restores_only_into_an_empty_composer() {
    let mut state = AppState::new();
    state.queue_message("first instruction".into());
    state.queue_message("second instruction".into());
    assert_eq!(
        state.queued_message(),
        Some("first instruction\n\nsecond instruction")
    );

    state.composer_mut().replace_from_editor("live draft");
    assert!(!state.restore_queued_message());
    assert_eq!(state.composer().text(), "live draft");

    state.composer_mut().clear();
    assert!(state.restore_queued_message());
    assert_eq!(
        state.composer().text(),
        "first instruction\n\nsecond instruction"
    );
    assert_eq!(state.queued_message(), None);
}

#[test]
fn mock_provider_uses_a_default_model_without_credentials() {
    let tea_home = test_tea_home("mock-provider");
    let options = CliOptions::parse(
        [
            "tea",
            "--provider",
            "mock",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 path"),
        ]
        .map(OsString::from),
    )
    .expect("mock startup options parse");
    let mut app = App::new(options);

    app.assemble_host().expect("mock host should assemble");

    assert_eq!(
        app.state().selected_model.as_ref(),
        Some(&ModelDescriptor {
            provider: "mock".into(),
            model: "mock".into(),
            revision: None,
        })
    );
    assert_eq!(
        app.configuration
            .as_ref()
            .expect("mock configuration")
            .tools
            .names()
            .collect::<Vec<_>>(),
        ["edit"]
    );
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn command_completion_expands_a_slash_prefix_without_submitting_it() {
    let mut app = App::new(CliOptions::default());
    app.state.composer_mut().insert_str("/hel").expect("prefix");

    app.complete_command();

    assert_eq!(app.state.composer().text(), "/help ");
}

#[test]
fn command_completion_includes_durable_session_commands() {
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

#[test]
fn bundled_extension_commands_feed_completion_and_help() {
    let mut app = App::new(CliOptions::default());
    app.state
        .set_extension_commands(super::durable::bundled_host_commands().expect("goal bundle resolves"));
    app.state.composer_mut().insert_str("/go").expect("prefix");
    app.complete_command();
    assert_eq!(app.state.composer().text(), "/goal ");

    app.dispatch_command("/help")
        .expect("help dispatch succeeds");
    assert!(app.state.surface_lines().is_some_and(|lines| {
        lines.iter().any(|line| line.contains("/goal"))
    }));
}
