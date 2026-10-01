//! MCP client behavior against the fake server over in-memory pipes, plus
//! process-failure handling with ordinary system commands.

use super::*;
use std::io::BufReader;
use tea_core::scheduler::CancellationToken;
use tea_core::state::ToolCallId;

fn config(call_timeout: Duration) -> McpServerConfig {
    McpServerConfig {
        name: "fixture".into(),
        command: "unused".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: None,
        exposure: ToolExposure::Deferred,
        startup_timeout: Duration::from_secs(5),
        call_timeout,
    }
}

struct Piped {
    connection: Arc<Connection>,
    server: Option<std::thread::JoinHandle<bool>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Piped {
    fn drop(&mut self) {
        self.connection.shutdown("test finished");
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn piped() -> Piped {
    let (client_reader, server_writer) = std::io::pipe().expect("pipe");
    let (server_reader, client_writer) = std::io::pipe().expect("pipe");
    let server = std::thread::spawn(move || {
        fixture::serve(BufReader::new(server_reader), server_writer)
    });
    let (connection, reader) =
        Connection::start("fixture", Box::new(client_reader), Box::new(client_writer))
            .expect("connection starts");
    Piped {
        connection,
        server: Some(server),
        reader: Some(reader),
    }
}

fn tools(piped: &Piped, call_timeout: Duration) -> BTreeMap<String, Arc<dyn AgentTool>> {
    let config = config(call_timeout);
    initialize(&config, &piped.connection).expect("initialize");
    list_tools(&config, &piped.connection)
        .expect("tools/list")
        .into_iter()
        .map(|tool| (tool.name().to_owned(), tool))
        .collect()
}

fn call(tool: &Arc<dyn AgentTool>, arguments: &str) -> Result<AgentToolResult, ToolError> {
    call_with(tool, arguments, CancellationToken::new())
}

fn call_with(
    tool: &Arc<dyn AgentTool>,
    arguments: &str,
    cancellation: CancellationToken,
) -> Result<AgentToolResult, ToolError> {
    smol::block_on(tool.execute(
        ToolCall {
            id: ToolCallId::new("call-mcp").expect("id"),
            name: tool.name().to_owned(),
            arguments: SerializedJson::new(arguments),
        },
        ToolContext {
            cancellation,
            provenance: Default::default(),
            composition: None,
        },
        ToolUpdateSink::default(),
    ))
}

#[test]
fn initialization_pages_tools_tolerates_noise_and_answers_server_pings() {
    let piped = piped();
    let tools = tools(&piped, Duration::from_secs(5));
    assert_eq!(
        tools.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "mcp__fixture__add",
            "mcp__fixture__cancelled_count",
            "mcp__fixture__change_tools",
            "mcp__fixture__crash",
            "mcp__fixture__echo",
            "mcp__fixture__fail",
            "mcp__fixture__image",
            "mcp__fixture__lookup_issue",
            "mcp__fixture__slow",
        ]
    );
    assert!(tools
        .values()
        .all(|tool| tool.exposure() == ToolExposure::Deferred));
    assert!(piped
        .connection
        .diagnostics()
        .iter()
        .any(|line| line.contains("ignored non-JSON output: fixture: starting")));
    // The server is still answering after our ping response.
    let echo = call(&tools["mcp__fixture__echo"], r#"{"text":"hi"}"#).expect("echo");
    assert_eq!(echo.content, "hi");
}

#[test]
fn results_keep_text_and_structure_and_name_unsupported_content() {
    let piped = piped();
    let tools = tools(&piped, Duration::from_secs(5));

    let sum = call(&tools["mcp__fixture__add"], r#"{"a":2,"b":3.5}"#).expect("add");
    assert_eq!(sum.content, r#"{"sum":5.5}"#);
    assert!(sum
        .details
        .as_ref()
        .is_some_and(|details| details.as_str().contains("structuredContent")));

    let image = call(&tools["mcp__fixture__image"], "{}").expect("image");
    assert_eq!(
        image.content,
        "a tiny image follows\n[unsupported image content omitted: image/png, 12 base64 bytes]"
    );
    assert!(image
        .details
        .as_ref()
        .is_some_and(|details| details.as_str().contains(r#""unsupportedContent":["image"]"#)));
    assert!(!image.content.contains("iVBOR"));

    let failed = call(&tools["mcp__fixture__fail"], "{}").expect("tool error is a result");
    assert!(failed.is_error);
    assert_eq!(failed.content, "the fixture failed on purpose");

    let protocol = call(&tools["mcp__fixture__echo"], "{}").expect("missing text echoes empty");
    assert_eq!(protocol.content, "");
}

#[test]
fn cancellation_and_timeouts_notify_the_server() {
    let piped = piped();
    let tools = tools(&piped, Duration::from_millis(100));
    let cancellation = CancellationToken::new();
    let canceller = cancellation.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        canceller.cancel();
    });
    let cancelled = call_with(&tools["mcp__fixture__slow"], "{}", cancellation);
    trigger.join().expect("trigger");
    assert!(matches!(cancelled, Err(ToolError::Cancelled { .. })));

    let timed_out = call(&tools["mcp__fixture__slow"], "{}");
    assert!(
        matches!(&timed_out, Err(ToolError::Execution { message, .. }) if message.contains("did not answer within")),
        "{timed_out:?}"
    );
    let count = call(&tools["mcp__fixture__cancelled_count"], "{}").expect("count");
    assert_eq!(count.content, "2");
}

#[test]
fn a_changed_tool_list_is_noticed_and_relisted() {
    let piped = piped();
    let tools = tools(&piped, Duration::from_secs(5));
    assert!(!piped.connection.take_tools_changed());
    call(&tools["mcp__fixture__change_tools"], "{}").expect("change");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !piped.connection.take_tools_changed() {
        assert!(Instant::now() < deadline, "list change was not observed");
        std::thread::yield_now();
    }
    let relisted = list_tools(&config(Duration::from_secs(5)), &piped.connection).expect("relist");
    assert!(relisted
        .iter()
        .any(|tool| tool.name() == "mcp__fixture__late_tool"));
}

#[test]
fn a_closed_connection_fails_calls_clearly() {
    let piped = piped();
    let tools = tools(&piped, Duration::from_secs(5));
    piped.connection.shutdown("test closed it");
    let result = call(&tools["mcp__fixture__echo"], r#"{"text":"x"}"#);
    assert!(
        matches!(&result, Err(ToolError::Execution { message, .. }) if message.contains("closed")),
        "{result:?}"
    );
}

#[test]
fn input_schemas_are_reduced_to_the_validated_vocabulary() {
    let piped = piped();
    let tools = tools(&piped, Duration::from_secs(5));
    let schema = tools["mcp__fixture__lookup_issue"].schema().clone();
    let id = schema
        .get("properties")
        .and_then(|properties| properties.get("id"))
        .expect("id property");
    assert_eq!(id.get("type").and_then(JsonValue::as_str), Some("string"));
    assert_eq!(
        id.get("description").and_then(JsonValue::as_str),
        Some("Issue key such as TEA-1.")
    );
    assert!(id.get("pattern").is_none() && id.get("$ref").is_none());
    let url = schema
        .get("properties")
        .and_then(|properties| properties.get("url"))
        .expect("url property");
    assert!(url.get("format").is_none());
    assert!(schema.get("$defs").is_none());
    assert_eq!(
        normalize_input_schema(None),
        JsonValue::object([
            ("properties", JsonValue::object(Vec::<(&str, JsonValue)>::new())),
            ("type", JsonValue::from("object")),
        ])
    );
    // A cyclic reference becomes unconstrained instead of looping.
    let cyclic = JsonValue::parse(
        r##"{"type":"object","properties":{"node":{"$ref":"#/$defs/Node"}},"$defs":{"Node":{"type":"object","properties":{"next":{"$ref":"#/$defs/Node"}}}}}"##,
    )
    .expect("schema");
    let normalized = normalize_input_schema(Some(&cyclic));
    assert!(normalized.to_json_string().expect("json").len() < 4_096);
}

#[test]
fn resources_links_and_unknown_content_are_identified() {
    let result = JsonValue::parse(
        r#"{"content":[
            {"type":"resource","resource":{"uri":"file:///a.txt","text":"alpha"}},
            {"type":"resource","resource":{"uri":"file:///b.bin","mimeType":"application/octet-stream","blob":"AAAA"}},
            {"type":"resource_link","uri":"https://example.invalid/x","name":"x"},
            {"type":"hologram"}
        ]}"#,
    )
    .expect("result");
    let mapped = map_call_result(
        ToolCall {
            id: ToolCallId::new("call").expect("id"),
            name: "t".into(),
            arguments: SerializedJson::new("{}"),
        },
        "server",
        "tool",
        &result,
    );
    assert_eq!(
        mapped.content,
        "[resource file:///a.txt]\nalpha\n\
[unsupported binary resource omitted: file:///b.bin, application/octet-stream, 4 base64 bytes]\n\
[resource link: x <https://example.invalid/x>]\n\
[unsupported content type \"hologram\" omitted]"
    );
    assert!(!mapped.is_error);
}

#[test]
fn tool_names_are_provider_safe() {
    assert_eq!(tool_name("git hub", "list.issues"), "mcp__git_hub__list_issues");
    assert_eq!(tool_name("s", &"x".repeat(100)).len(), 64);
}

#[cfg(unix)]
#[test]
fn servers_that_cannot_start_or_exit_early_fail_without_blocking() {
    let missing = McpServerConfig {
        name: "missing".into(),
        command: "/nonexistent/tea-mcp-server".into(),
        ..config(Duration::from_secs(5))
    };
    let exits = McpServerConfig {
        name: "exits".into(),
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "echo boom >&2; exit 3".into()],
        ..config(Duration::from_secs(5))
    };
    let manager = McpManager::start(vec![missing, exits], std::env::temp_dir());
    assert!(manager.snapshot().is_empty());
    let statuses = manager.statuses();
    assert!(
        matches!(&statuses[0].1, ServerStatus::Failed(message) if message.contains("could not start")),
        "{statuses:?}"
    );
    assert!(
        matches!(&statuses[1].1, ServerStatus::Failed(message) if message.contains("initialize failed")),
        "{statuses:?}"
    );
    manager.shutdown();
}

/// Path of the fake server binary built beside this test executable.
#[cfg(feature = "mcp-fixture")]
pub(crate) fn fixture_binary() -> PathBuf {
    let name = format!("tea-mcp-fixture{}", std::env::consts::EXE_SUFFIX);
    let executable = std::env::current_exe().expect("test executable");
    executable
        .ancestors()
        .skip(1)
        .take(8)
        .flat_map(|ancestor| [ancestor.join(&name), ancestor.join("debug").join(&name)])
        .find(|candidate| candidate.is_file())
        .expect("build the fixture with `cargo test -p tea-agent --features mcp-fixture`")
}

#[cfg(feature = "mcp-fixture")]
pub(crate) fn fixture_config(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        command: fixture_binary().to_string_lossy().into_owned(),
        ..config(Duration::from_secs(5))
    }
}

#[cfg(all(unix, feature = "mcp-fixture"))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(all(unix, feature = "mcp-fixture"))]
#[test]
fn a_real_server_process_serves_crashes_and_shuts_down_cleanly() {
    let manager = McpManager::start(
        vec![fixture_config("first"), fixture_config("second")],
        std::env::temp_dir(),
    );
    let tools = manager.snapshot();
    assert_eq!(tools.len(), 18, "both servers are ready within the startup grace");
    let pids = manager.pids();
    assert_eq!(pids.len(), 2);
    let find = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name() == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert_eq!(
        call(&find("mcp__first__echo"), r#"{"text":"real"}"#)
            .expect("echo")
            .content,
        "real"
    );

    // A crash fails the in-flight call clearly and removes the server's tools
    // from the next epoch without affecting the other server.
    let crashed = call(&find("mcp__first__crash"), "{}");
    assert!(
        matches!(&crashed, Err(ToolError::Execution { message, .. }) if message.contains("closed")),
        "{crashed:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(manager.statuses()[0].1, ServerStatus::Exited(_)) {
        assert!(Instant::now() < deadline, "crash was not observed");
        std::thread::yield_now();
    }
    let remaining = manager.snapshot();
    assert_eq!(remaining.len(), 9);
    assert!(remaining.iter().all(|tool| tool.name().starts_with("mcp__second__")));
    assert!(!process_alive(pids[0]));

    manager.shutdown();
    assert!(!process_alive(pids[1]), "shutdown reaps the server");
    assert!(manager.snapshot().is_empty());
}
