//! Nested tool calls from a trusted composition tool through the real loop:
//! shared validation, hooks, effect attribution, ordering, cancellation, and
//! no detached work.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tea_core::Agent;
use tea_core::effect::{EffectAction, EffectFuture, EffectGate, EffectOutcome, EffectSubject};
use tea_core::error::{HookError, ToolError};
use tea_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet};
use tea_core::state::{AgentMessage, SerializedJson};
use tea_core::testing::{ScriptedProvider, ScriptedTurn};
use tea_core::tool::{
    AgentTool, AgentToolResult, CompositionAccess, ToolCall, ToolContext, ToolExecutionMode,
    ToolExposure, ToolFuture, ToolUpdateSink,
};
use tea_core::tool_search::ToolSearchTool;
use tea_protocol::JsonValue;

/// A leaf tool that records its execution window.
struct Leaf {
    name: &'static str,
    exposure: ToolExposure,
    mode: ToolExecutionMode,
    log: Arc<Mutex<Vec<String>>>,
    schema: JsonValue,
    gate: Option<Arc<async_gate::Gate>>,
}

fn leaf(
    name: &'static str,
    exposure: ToolExposure,
    mode: ToolExecutionMode,
    log: &Arc<Mutex<Vec<String>>>,
) -> Leaf {
    Leaf {
        name,
        exposure,
        mode,
        log: Arc::clone(log),
        schema: JsonValue::object([
            ("type", JsonValue::from("object")),
            (
                "properties",
                JsonValue::object([("n", JsonValue::object([("type", JsonValue::from("integer"))]))]),
            ),
            ("required", JsonValue::Array(vec![JsonValue::from("n")])),
        ]),
        gate: None,
    }
}

impl AgentTool for Leaf {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "leaf"
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.mode
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let n = JsonValue::parse(call.arguments.as_str())
                .ok()
                .and_then(|value| value.get("n").and_then(JsonValue::as_u64))
                .unwrap_or_default();
            self.log
                .lock()
                .expect("log")
                .push(format!("start {} {n}", self.name));
            if let Some(gate) = &self.gate {
                gate.wait().await;
            }
            self.log
                .lock()
                .expect("log")
                .push(format!("end {} {n}", self.name));
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content: format!("{} {n}", self.name),
                details: None,
                usage: None,
                added_tool_names: vec!["ghost".into()],
                terminate: false,
                is_error: false,
                failure: None,
            })
        })
    }
}

mod async_gate {
    use std::sync::Mutex;
    use std::task::{Poll, Waker};

    #[derive(Default)]
    pub struct Gate {
        state: Mutex<(bool, Vec<Waker>)>,
    }

    impl Gate {
        pub fn open(&self) {
            let wakers = {
                let mut state = self.state.lock().expect("gate");
                state.0 = true;
                std::mem::take(&mut state.1)
            };
            wakers.into_iter().for_each(Waker::wake);
        }

        pub async fn wait(&self) {
            std::future::poll_fn(|context| {
                let mut state = self.state.lock().expect("gate");
                if state.0 {
                    Poll::Ready(())
                } else {
                    state.1.push(context.waker().clone());
                    Poll::Pending
                }
            })
            .await;
        }
    }
}

/// A composition tool driven by a plan: each step is a batch of concurrent
/// calls (name, arguments). It reports every nested result.
struct Composer {
    plan: Vec<Vec<(&'static str, &'static str)>>,
    schema: JsonValue,
}

impl AgentTool for Composer {
    fn name(&self) -> &str {
        "compose"
    }
    fn description(&self) -> &str {
        "compose"
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn composition_access(&self) -> CompositionAccess {
        CompositionAccess::Calls
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let composition = context.composition.expect("composition granted");
            let mut lines = Vec::new();
            for batch in &self.plan {
                let futures = batch
                    .iter()
                    .map(|(name, arguments)| composition.call(name, SerializedJson::new(*arguments)))
                    .collect::<Vec<_>>();
                for future in futures {
                    let result = future.await;
                    lines.push(format!(
                        "{}{}",
                        if result.is_error { "error: " } else { "" },
                        result.content.replace('\n', " ")
                    ));
                }
            }
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content: lines.join("\n"),
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

fn composer(plan: Vec<Vec<(&'static str, &'static str)>>) -> Arc<Composer> {
    Arc::new(Composer {
        plan,
        schema: JsonValue::object([("type", JsonValue::from("object"))]),
    })
}

/// Records effect subjects in order.
#[derive(Default)]
struct RecordingGate {
    trace: Mutex<Vec<String>>,
}

impl EffectGate for RecordingGate {
    fn before<'a>(&'a self, action: EffectAction) -> EffectFuture<'a> {
        let label = match action.subject() {
            EffectSubject::ToolExecution { call } => Some(format!("tool {}", call.name)),
            EffectSubject::NestedToolExecution {
                parent_tool_call_id,
                call,
            } => Some(format!("nested {parent_tool_call_id} {} {}", call.id, call.name)),
            _ => None,
        };
        if let Some(label) = label {
            self.trace.lock().expect("trace").push(label);
        }
        Box::pin(std::future::ready(Ok(())))
    }
    fn after<'a>(&'a self, action: EffectAction, _outcome: EffectOutcome) -> EffectFuture<'a> {
        if let EffectSubject::NestedToolExecution { call, .. } = action.subject() {
            self.trace
                .lock()
                .expect("trace")
                .push(format!("settled {}", call.id));
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Blocks one tool by name and annotates every result.
struct Policy {
    seen: Mutex<Vec<String>>,
}

impl HookSet for Policy {
    fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        self.seen.lock().expect("seen").push(call.name.clone());
        if call.name == "forbidden" {
            return Ok(BeforeToolCall::Block {
                reason: "forbidden by policy".into(),
            });
        }
        Ok(BeforeToolCall::Allow)
    }
    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        Ok(AfterToolCall::default())
    }
    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        Ok(context)
    }
}

#[test]
fn nested_calls_share_validation_hooks_and_attribution_but_stay_out_of_the_transcript() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-compose", "compose", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("done").stop(),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let policy = Arc::new(Policy {
        seen: Mutex::new(Vec::new()),
    });
    let agent = Agent::builder()
        .system_prompt("p")
        .tool(Arc::new(leaf("fast", ToolExposure::Direct, ToolExecutionMode::Parallel, &log)))
        .tool(Arc::new(leaf(
            "hidden",
            ToolExposure::Composition,
            ToolExecutionMode::Parallel,
            &log,
        )))
        .tool(Arc::new(leaf(
            "forbidden",
            ToolExposure::Direct,
            ToolExecutionMode::Parallel,
            &log,
        )))
        .tool(Arc::new(ToolSearchTool::default()))
        .tool(composer(vec![
            vec![("fast", r#"{"n":1}"#), ("hidden", r#"{"n":2}"#)],
            vec![
                ("forbidden", r#"{"n":3}"#),
                ("fast", r#"{"n":"x"}"#),
                ("fast", "not json"),
                ("missing", "{}"),
                ("tool_search", r#"{"query":"x"}"#),
                ("compose", "{}"),
            ],
        ]))
        .hooks(Arc::clone(&policy) as Arc<dyn HookSet>)
        .effect_gate(Arc::clone(&gate) as Arc<dyn EffectGate>)
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("go").expect("run").drive()).expect("run settles");

    let messages = agent.snapshot().messages;
    let results = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                tool_name, content, ..
            } => Some((tool_name.clone(), content.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Only the model-issued composition call joins the transcript.
    assert_eq!(results.len(), 1);
    let (name, content) = &results[0];
    assert_eq!(name, "compose");
    let lines = content.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "fast 1");
    // Script-only tools are callable through composition.
    assert_eq!(lines[1], "hidden 2");
    assert_eq!(lines[2], "error: forbidden by policy");
    assert!(lines[3].starts_with("error: Validation failed for tool \"fast\""), "{lines:?}");
    assert!(lines[4].contains("invalid JSON"), "{lines:?}");
    assert_eq!(lines[5], "error: Tool missing not found");
    assert_eq!(lines[6], "error: Tool tool_search cannot be called from a script");
    assert_eq!(lines[7], "error: Tool compose cannot be called from a script");
    // A nested result cannot load tools for the model.
    assert!(!format!("{messages:?}").contains("ghost"));

    // Hooks saw nested calls; the effect gate attributed them to the parent.
    let seen = policy.seen.lock().expect("seen").clone();
    assert!(seen.contains(&"fast".to_owned()) && seen.contains(&"forbidden".to_owned()));
    let trace = gate.trace.lock().expect("trace").clone();
    assert_eq!(trace[0], "tool compose");
    assert!(trace.contains(&"nested call-compose call-compose.1 fast".to_owned()));
    assert!(trace.contains(&"nested call-compose call-compose.2 hidden".to_owned()));
    assert!(trace.contains(&"settled call-compose.1".to_owned()));
    // Blocked and invalid calls never began an execution effect.
    assert!(!trace.iter().any(|entry| entry.contains(" forbidden")));
    assert_eq!(
        trace.iter().filter(|entry| entry.starts_with("nested")).count(),
        2
    );
}

#[test]
fn sequential_nested_tools_never_overlap_and_parallel_ones_may() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(async_gate::Gate::default());
    let mut slow = leaf("slow", ToolExposure::Direct, ToolExecutionMode::Parallel, &log);
    slow.gate = Some(Arc::clone(&release));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-compose", "compose", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("done").stop(),
    ]);
    let agent = Agent::builder()
        .system_prompt("p")
        .tool(Arc::new(slow))
        .tool(Arc::new(leaf("fast", ToolExposure::Direct, ToolExecutionMode::Parallel, &log)))
        .tool(Arc::new(leaf(
            "serial",
            ToolExposure::Direct,
            ToolExecutionMode::Sequential,
            &log,
        )))
        .tool(composer(vec![vec![
            ("slow", r#"{"n":1}"#),
            ("fast", r#"{"n":2}"#),
            ("serial", r#"{"n":3}"#),
            ("fast", r#"{"n":4}"#),
        ]]))
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = agent.start_prompt("go").expect("run");
    let observed = Arc::clone(&log);
    let opener = std::thread::spawn(move || {
        // `fast 2` overlaps `slow 1`; the sequential call waits for both.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !observed.lock().expect("log").contains(&"end fast 2".to_owned()) {
            assert!(std::time::Instant::now() < deadline, "parallel calls did not overlap");
            std::thread::yield_now();
        }
        assert!(!observed.lock().expect("log").contains(&"start serial 3".to_owned()));
        release.open();
    });
    smol::block_on(run.drive()).expect("run settles");
    opener.join().expect("opener");
    let log = log.lock().expect("log").clone();
    let position = |entry: &str| log.iter().position(|line| line == entry).expect(entry);
    assert!(position("start fast 2") < position("end slow 1"));
    assert!(position("end slow 1") < position("start serial 3"));
    assert!(position("end fast 2") < position("start serial 3"));
    assert!(position("end serial 3") < position("start fast 4"));
}

/// A composition tool that submits calls and returns without awaiting them.
struct Abandoner {
    schema: JsonValue,
}

impl AgentTool for Abandoner {
    fn name(&self) -> &str {
        "abandon"
    }
    fn description(&self) -> &str {
        "abandon"
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn composition_access(&self) -> CompositionAccess {
        CompositionAccess::Calls
    }
    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let composition = context.composition.expect("composition granted");
            // The first call is awaited until it has started; the second is
            // submitted behind a sequential call and never awaited.
            let started = composition.call("serial", SerializedJson::new(r#"{"n":1}"#));
            let _abandoned = composition.call("serial", SerializedJson::new(r#"{"n":2}"#));
            drop(started);
            Err::<AgentToolResult, _>(ToolError::Execution {
                tool: "abandon".into(),
                message: "script failed".into(),
            })
        })
    }
}

#[test]
fn a_settling_composition_tool_leaves_no_detached_nested_work() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-abandon", "abandon", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("done").stop(),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let agent = Agent::builder()
        .system_prompt("p")
        .tool(Arc::new(leaf(
            "serial",
            ToolExposure::Direct,
            ToolExecutionMode::Sequential,
            &log,
        )))
        .tool(Arc::new(Abandoner {
            schema: JsonValue::object([("type", JsonValue::from("object"))]),
        }))
        .effect_gate(Arc::clone(&gate) as Arc<dyn EffectGate>)
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("go").expect("run").drive()).expect("run settles");
    // The composition tool failed before the service accepted its requests,
    // so neither call started, and nothing runs after the run settled.
    let trace = gate.trace.lock().expect("trace").clone();
    let starts = trace.iter().filter(|entry| entry.starts_with("nested")).count();
    let settles = trace.iter().filter(|entry| entry.starts_with("settled")).count();
    assert_eq!(starts, settles, "every started nested call settled: {trace:?}");
    assert!(log.lock().expect("log").is_empty());
    let messages = agent.snapshot().messages;
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::ToolResult { tool_name, is_error: true, .. } if tool_name == "abandon"
    )));
}

/// Submits a gated call, lets it start, and settles without awaiting it.
struct Starter {
    schema: JsonValue,
}

impl AgentTool for Starter {
    fn name(&self) -> &str {
        "starter"
    }
    fn description(&self) -> &str {
        "starter"
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn composition_access(&self) -> CompositionAccess {
        CompositionAccess::Calls
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let composition = context.composition.expect("composition granted");
            let pending = composition.call("slow", SerializedJson::new(r#"{"n":7}"#));
            // Yield once so the owning run starts the call.
            let mut yielded = false;
            std::future::poll_fn(|context| {
                if yielded {
                    std::task::Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
            drop(pending);
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content: "returned early".into(),
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

#[test]
fn a_started_nested_call_settles_before_the_composition_result() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(async_gate::Gate::default());
    let mut slow = leaf("slow", ToolExposure::Direct, ToolExecutionMode::Parallel, &log);
    slow.gate = Some(Arc::clone(&release));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-starter", "starter", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("done").stop(),
    ]);
    let gate = Arc::new(RecordingGate::default());
    let agent = Agent::builder()
        .system_prompt("p")
        .tool(Arc::new(slow))
        .tool(Arc::new(Starter {
            schema: JsonValue::object([("type", JsonValue::from("object"))]),
        }))
        .effect_gate(Arc::clone(&gate) as Arc<dyn EffectGate>)
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = agent.start_prompt("go").expect("run");
    let observed = Arc::clone(&log);
    let opener = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !observed.lock().expect("log").contains(&"start slow 7".to_owned()) {
            assert!(std::time::Instant::now() < deadline, "nested call never started");
            std::thread::yield_now();
        }
        release.open();
    });
    smol::block_on(run.drive()).expect("run settles");
    opener.join().expect("opener");
    assert_eq!(
        log.lock().expect("log").clone(),
        ["start slow 7", "end slow 7"]
    );
    let trace = gate.trace.lock().expect("trace").clone();
    assert!(trace.contains(&"settled call-starter.1".to_owned()), "{trace:?}");
    assert_eq!(provider.request_count(), 2);
}
