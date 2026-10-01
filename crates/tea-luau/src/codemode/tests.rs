//! Codemode through the real agent loop and scripted provider.

use super::*;
use std::sync::Arc;
use tea_core::state::AgentMessage;
use tea_core::testing::{ScriptedProvider, ScriptedTurn};
use tea_core::tool::{ToolExposure, ToolRegistry};
use tea_core::Agent;

/// Returns `n` numbered lines, or fails when `fail` is set.
struct Lines {
    name: &'static str,
    exposure: ToolExposure,
    schema: JsonValue,
    calls: Arc<Mutex<Vec<String>>>,
}

impl AgentTool for Lines {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Return numbered lines."
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = JsonValue::parse(call.arguments.as_str()).expect("validated JSON");
            self.calls
                .lock()
                .expect("calls")
                .push(call.arguments.as_str().to_owned());
            if arguments.get("fail").and_then(JsonValue::as_bool) == Some(true) {
                return Err(ToolError::Execution {
                    tool: self.name.into(),
                    message: "lines failed on request".into(),
                });
            }
            let count = arguments.get("n").and_then(JsonValue::as_u64).unwrap_or(0);
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content: (1..=count)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: false,
                failure: None,
            })
        })
    }
}

fn lines_tool(
    name: &'static str,
    exposure: ToolExposure,
    calls: &Arc<Mutex<Vec<String>>>,
) -> Arc<Lines> {
    Arc::new(Lines {
        name,
        exposure,
        schema: JsonValue::object([
            ("type", JsonValue::from("object")),
            (
                "properties",
                JsonValue::object([
                    ("n", JsonValue::object([("type", JsonValue::from("integer"))])),
                    ("fail", JsonValue::object([("type", JsonValue::from("boolean"))])),
                ]),
            ),
            ("additionalProperties", JsonValue::Bool(false)),
        ]),
        calls: Arc::clone(calls),
    })
}

struct Run {
    content: String,
    is_error: bool,
    messages: Vec<AgentMessage>,
    calls: Vec<String>,
}

fn run_script(script: &str, limits: CodemodeLimits) -> Run {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let arguments = JsonValue::object([("script", JsonValue::from(script))])
        .to_json_string()
        .expect("JSON");
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-code", CODEMODE_TOOL_NAME, &arguments)
            .end_tool_use(),
        ScriptedTurn::new().text("done").stop(),
    ]);
    let mut registry = ToolRegistry::default();
    registry.insert(lines_tool("lines", ToolExposure::Direct, &calls));
    registry.insert(lines_tool("hidden_lines", ToolExposure::Composition, &calls));
    registry.insert(lines_tool("issue_tracker", ToolExposure::Deferred, &calls));
    let listed = registry.declarations(ToolExposure::Direct);
    registry.insert(Arc::new(CodemodeTool::new(&listed, limits)));
    let agent = Agent::builder()
        .system_prompt("p")
        .tools(registry)
        .model_provider(Arc::new(provider))
        .build();
    smol::block_on(agent.start_prompt("go").expect("run").drive()).expect("run settles");
    let messages = agent.snapshot().messages;
    let (content, is_error) = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } if tool_name == CODEMODE_TOOL_NAME => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("codemode result");
    let calls = calls.lock().expect("calls").clone();
    Run {
        content,
        is_error,
        messages,
        calls,
    }
}

#[test]
fn a_script_composes_calls_and_returns_only_its_output() {
    let run = run_script(
        r#"
        local text = tools.lines({ n = 50 })
        local count = 0
        for _ in string.gmatch(text, "line") do count += 1 end
        local hidden = call("hidden_lines", { n = 2 })
        print("lines:", count)
        return { last = string.match(text, "line 50"), hidden = hidden }
        "#,
        CodemodeLimits::default(),
    );
    assert!(!run.is_error, "{}", run.content);
    assert!(run.content.starts_with("Script completed after 2 tool calls (0 failed)."));
    assert!(run.content.contains("lines:\t50"));
    assert!(run.content.contains(r#""last":"line 50""#));
    // The 50-line intermediate result never reached the model.
    assert!(!run.content.contains("line 49"));
    let results = run
        .messages
        .iter()
        .filter(|message| matches!(message, AgentMessage::ToolResult { .. }))
        .count();
    assert_eq!(results, 1);
    assert_eq!(run.calls.len(), 2);
}

#[test]
fn parallel_calls_and_partial_failure_are_reported_honestly() {
    let run = run_script(
        r#"
        local results = parallel({
            { name = "lines", args = { n = 1 } },
            { "lines", { fail = true } },
            { name = "lines", args = { n = "bad" } },
        })
        for index, result in ipairs(results) do
            print(index, result.ok)
        end
        local ok, message = try_call("missing_tool", {})
        print("missing", ok)
        call("lines", { n = 2 })
        call("lines", { fail = true })
        print("unreachable")
        "#,
        CodemodeLimits::default(),
    );
    assert!(run.is_error);
    assert!(
        run.content
            .starts_with("Script failed after 6 tool calls (4 failed). Calls that ran are not undone."),
        "{}",
        run.content
    );
    assert!(run.content.contains("1\ttrue"));
    assert!(run.content.contains("2\tfalse"));
    assert!(run.content.contains("3\tfalse"));
    assert!(run.content.contains("missing\tfalse"));
    assert!(!run.content.contains("unreachable"));
    assert!(run.content.contains("Script error:"));
    assert!(run.content.contains("lines failed on request"));
    // Invalid and missing calls never executed; real calls did and stay done.
    assert_eq!(
        run.calls,
        [r#"{"n":1}"#, r#"{"fail":true}"#, r#"{"n":2}"#, r#"{"fail":true}"#]
    );
}

#[test]
fn discovery_helpers_and_json_are_available_without_ambient_authority() {
    let run = run_script(
        r#"
        local found = search_tools("issue tracker")
        print(found[1].name)
        local schema = describe_tool("lines").schema
        print(schema.properties.n.type)
        print(describe_tool("codemode") == nil, describe_tool("nope") == nil)
        print(json.encode(json.decode('{"a":[1,2]}')))
        print(os == nil, io == nil, require == nil, debug == nil)
        "#,
        CodemodeLimits::default(),
    );
    assert!(!run.is_error, "{}", run.content);
    assert!(run.content.contains("issue_tracker\n"));
    assert!(run.content.contains("integer\n"));
    assert!(run.content.contains("true\ttrue\n"));
    assert!(run.content.contains(r#"{"a":[1,2]}"#));
    assert!(run.content.contains("true\ttrue\ttrue\ttrue"), "{}", run.content);
}

#[test]
fn limits_bound_computation_calls_and_output() {
    let looping = run_script("while true do end", CodemodeLimits::default());
    assert!(looping.is_error);
    assert!(looping.content.contains("computation budget"), "{}", looping.content);

    let limits = CodemodeLimits {
        max_calls: 2,
        ..CodemodeLimits::default()
    };
    let many = run_script(
        "for i = 1, 5 do call('lines', { n = 1 }) end",
        limits,
    );
    assert!(many.is_error);
    assert!(many.content.contains("limit of 2 tool calls"));
    assert_eq!(many.calls.len(), 2);

    let limits = CodemodeLimits {
        max_output_bytes: 200,
        ..CodemodeLimits::default()
    };
    let verbose = run_script("for i = 1, 500 do print('row', i) end", limits);
    assert!(!verbose.is_error);
    assert!(verbose.content.len() < 600, "{}", verbose.content.len());
    assert!(verbose.content.contains("row\t1"));
    assert!(verbose.content.contains("discarded") || verbose.content.contains("…"));

    let syntax = run_script("this is not luau", CodemodeLimits::default());
    assert!(syntax.is_error);
    assert!(syntax.content.contains("does not compile"), "{}", syntax.content);
}


/// Waits for run cancellation, recording that it started.
struct Waiter {
    started: Arc<std::sync::atomic::AtomicBool>,
    schema: JsonValue,
}

impl AgentTool for Waiter {
    fn name(&self) -> &str {
        "wait_forever"
    }
    fn description(&self) -> &str {
        "Waits."
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            context.cancellation.cancelled().await;
            Err(ToolError::Cancelled {
                tool: "wait_forever".into(),
            })
        })
    }
}

#[test]
fn cancelling_the_run_cancels_the_script_and_settles_its_calls() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let arguments = JsonValue::object([(
        "script",
        JsonValue::from("print('before') call('wait_forever', {}) print('after')"),
    )])
    .to_json_string()
    .expect("JSON");
    let provider = ScriptedProvider::new([ScriptedTurn::new()
        .tool_call("call-code", CODEMODE_TOOL_NAME, &arguments)
        .end_tool_use()]);
    let mut registry = ToolRegistry::default();
    registry.insert(Arc::new(Waiter {
        started: Arc::clone(&started),
        schema: JsonValue::object([("type", JsonValue::from("object"))]),
    }));
    registry.insert(Arc::new(CodemodeTool::new(&[], CodemodeLimits::default())));
    let agent = Agent::builder()
        .system_prompt("p")
        .tools(registry)
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = Arc::new(agent.start_prompt("go").expect("run"));
    let driver = Arc::clone(&run);
    let thread = std::thread::spawn(move || smol::block_on(driver.drive()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !started.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "nested call never started");
        std::thread::yield_now();
    }
    run.abort().expect("abort");
    let _ = thread.join().expect("driver");
    let messages = agent.snapshot().messages;
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::ToolResult { tool_name, is_error: true, .. } if tool_name == CODEMODE_TOOL_NAME
    )));
    let content = messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult { tool_name, content, .. } if tool_name == CODEMODE_TOOL_NAME => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("codemode result");
    assert!(!content.contains("after"), "{content}");
}
