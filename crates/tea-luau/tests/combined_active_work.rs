//! Combined scenario: codemode and discovery run during one active
//! operation while cache maintenance is eligible, under a virtual model.
//!
//! Maintenance must replay the admitted physical request without routing or
//! executing tools, must not enter model context, and must stop when the run
//! settles; codemode's nested calls must still go through the run.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tea_core::cache_warming::{CacheMaintenanceOutcome, CacheWarmingPolicy};
use tea_core::event::AgentEventKind;
use tea_core::routing::{
    ContinuationPolicy, ModelRouter, Route, RouteError, RouteRequest, VirtualModel,
};
use tea_core::scheduler::{
    MinimalOutputReplay, ModelCapabilities, ModelPricing, PromptCacheCapability, RequestPurpose,
};
use tea_core::state::{AgentMessage, ModelDescriptor, Usage};
use tea_core::testing::{ScriptedProvider, ScriptedTurn, VirtualClock};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolExposure, ToolFuture, ToolUpdateSink,
};
use tea_core::tool_search::ToolSearchTool;
use tea_core::Agent;
use tea_luau::codemode::{CodemodeLimits, CodemodeTool};
use tea_protocol::JsonValue;

fn physical() -> ModelDescriptor {
    ModelDescriptor {
        provider: "fixture".into(),
        model: "physical".into(),
        revision: None,
    }
}

fn selection() -> ModelDescriptor {
    ModelDescriptor {
        provider: "virtual".into(),
        model: "only".into(),
        revision: None,
    }
}

struct CountingRouter(AtomicUsize);

impl ModelRouter for CountingRouter {
    fn route(&self, _request: &RouteRequest<'_>) -> Result<Route, RouteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Route {
            target: physical(),
            thinking_level: None,
            state: None,
        })
    }
}

/// A deferred tool that waits for the test, modelling a long tool wait
/// inside a codemode script.
struct SlowLookup {
    started: Arc<AtomicBool>,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    schema: JsonValue,
}

impl AgentTool for SlowLookup {
    fn name(&self) -> &str {
        "issue_lookup"
    }
    fn description(&self) -> &str {
        "Look up an issue in the tracker."
    }
    fn schema(&self) -> &JsonValue {
        &self.schema
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }
    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let release = Arc::clone(&self.release);
        self.started.store(true, Ordering::SeqCst);
        Box::pin(async move {
            // Block on a helper thread so the run's executor stays free.
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let (lock, ready) = &*release;
                let mut open = lock.lock().expect("release");
                while !*open {
                    open = ready.wait(open).expect("release");
                }
                let _ = sender.send(());
            });
            let receiver = Arc::new(Mutex::new(receiver));
            std::future::poll_fn(move |context| {
                match receiver.lock().expect("receiver").try_recv() {
                    Ok(()) => std::task::Poll::Ready(()),
                    Err(_) => {
                        context.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                }
            })
            .await;
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content: "TEA-1 is open".into(),
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
fn codemode_and_discovery_during_eligible_maintenance_stay_owned_by_the_run() {
    let script = r#"
        local found = search_tools("issue tracker")
        local text = call(found[1].name, {})
        return "lookup: " .. text
    "#;
    let arguments = JsonValue::object([("script", JsonValue::from(script))])
        .to_json_string()
        .expect("JSON");
    let capabilities = ModelCapabilities {
        prompt_cache: Some(PromptCacheCapability {
            ttl_seconds: 300,
            minimal_output_replay: MinimalOutputReplay::Safe,
        }),
        pricing: Some(ModelPricing {
            input: "5".into(),
            output: "25".into(),
            cache_read: "0.5".into(),
            cache_write: "6.25".into(),
        }),
        ..ModelCapabilities::default()
    };
    let provider = ScriptedProvider::with_capabilities(
        [
            ScriptedTurn::new()
                .usage(Usage {
                    input_tokens: Some(100_000),
                    ..Usage::default()
                })
                .tool_call("call-search", "tool_search", r#"{"query":"issue tracker"}"#)
                .end_tool_use(),
            ScriptedTurn::new()
                .usage(Usage {
                    input_tokens: Some(100_200),
                    ..Usage::default()
                })
                .tool_call("call-code", "codemode", &arguments)
                .end_tool_use(),
            ScriptedTurn::new().text("TEA-1 is open.").stop(),
        ],
        capabilities,
    );
    // A maintenance replay "answers" with a tool call; it must never run.
    provider.push_maintenance_turn(
        ScriptedTurn::new()
            .usage(Usage {
                input_tokens: Some(100_200),
                cache_read_tokens: Some(100_200),
                output_tokens: Some(1),
                ..Usage::default()
            })
            .tool_call("never", "issue_lookup", "{}")
            .stop(),
    );
    let clock = VirtualClock::new();
    let router = Arc::new(CountingRouter(AtomicUsize::new(0)));
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let agent = Agent::builder()
        .system_prompt("p")
        .model(selection())
        .tool(Arc::new(ToolSearchTool::default()))
        .tool(Arc::new(SlowLookup {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            schema: JsonValue::object([("type", JsonValue::from("object"))]),
        }))
        .tool(Arc::new(CodemodeTool::new(&[], CodemodeLimits::default())))
        .virtual_models(vec![VirtualModel {
            descriptor: selection(),
            name: "only".into(),
            targets: vec![physical()],
            continuations: ContinuationPolicy::Sticky,
            router: Arc::clone(&router) as Arc<dyn ModelRouter>,
            state_namespace: None,
            state: None,
        }])
        .cache_warming(CacheWarmingPolicy::new(Arc::new(clock.clone())))
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = Arc::new(agent.start_prompt("is TEA-1 open?").expect("run"));
    let driver = Arc::clone(&run);
    let thread = std::thread::spawn(move || smol::block_on(driver.drive()));

    // The codemode script is now blocked in a nested deferred tool call.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "nested call never started");
        std::thread::yield_now();
    }
    assert!(clock.wait_for_sleepers(1, Duration::from_secs(5)));
    clock.advance(Duration::from_secs(270));
    assert!(provider.wait_for_requests(3, Duration::from_secs(5)));
    let maintenance = provider.requests_for(RequestPurpose::CacheMaintenance);
    assert_eq!(maintenance.len(), 1);
    let real = provider.requests_for(RequestPurpose::Turn);
    // The replay is the admitted physical request: no extra routing.
    assert_eq!(maintenance[0].transcript, real[1].transcript);
    assert_eq!(maintenance[0].model, Some(physical()));
    assert_eq!(maintenance[0].selected_model, Some(selection()));
    assert_eq!(router.0.load(Ordering::SeqCst), 1, "only the user turn was routed");

    *release.0.lock().expect("release") = true;
    release.1.notify_all();
    thread.join().expect("driver").expect("run settles");

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
    // Discovery loaded the deferred tool; codemode called it through the
    // run; the maintenance tool call never executed.
    assert_eq!(results[0].0, "tool_search");
    assert!(results[0].1.contains("issue_lookup"));
    assert_eq!(results[1].0, "codemode");
    assert!(results[1].1.contains("lookup: TEA-1 is open"), "{}", results[1].1);
    assert_eq!(results.len(), 2);
    let records = run
        .events()
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::CacheMaintenance { record } => Some(record.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, CacheMaintenanceOutcome::Completed);
    // Warming stopped with the run.
    assert_eq!(clock.sleepers(), 0);
    clock.advance(Duration::from_secs(3_600));
    assert_eq!(provider.request_count(), 4);
}
