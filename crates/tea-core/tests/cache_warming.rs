//! Active-work cache warming through the real agent loop, on virtual time.
//!
//! Ported behavior from Pi's `cache-warmer.test.ts` (streaming mode): replay
//! with preserved options, repeated refreshes, late deadlines, economics,
//! unsupported requests, context changes, replacement and settlement
//! cancellation, and no warming once a run has settled.

use std::sync::Arc;
use std::time::Duration;
use tea_core::Agent;
use tea_core::cache_warming::{
    CacheMaintenanceOutcome, CacheMaintenanceRecord, CacheWarmingPolicy, CacheWarmingStatus,
};
use tea_core::event::{AgentEvent, AgentEventKind};
use tea_core::run::RunHandle;
use tea_core::scheduler::{
    MinimalOutputReplay, ModelCapabilities, ModelPricing, PromptCacheCapability, RequestPurpose,
};
use tea_core::state::{ModelDescriptor, ThinkingLevel, Usage};
use tea_core::testing::{ScriptedProvider, ScriptedTurn, VirtualClock};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use tea_protocol::JsonValue;

const WAIT: Duration = Duration::from_secs(5);
const DELAY: Duration = Duration::from_secs(270);

fn capabilities(replay: MinimalOutputReplay) -> ModelCapabilities {
    ModelCapabilities {
        prompt_cache: Some(PromptCacheCapability {
            ttl_seconds: 300,
            minimal_output_replay: replay,
        }),
        pricing: Some(ModelPricing {
            input: "5".into(),
            output: "25".into(),
            cache_read: "0.5".into(),
            cache_write: "6.25".into(),
        }),
        ..ModelCapabilities::default()
    }
}

fn model() -> ModelDescriptor {
    ModelDescriptor {
        provider: "fixture".into(),
        model: "cached".into(),
        revision: None,
    }
}

fn prompt_usage(tokens: u64) -> Usage {
    Usage {
        input_tokens: Some(tokens),
        cache_read_tokens: Some(tokens.saturating_sub(10)),
        output_tokens: Some(10),
        ..Usage::default()
    }
}

struct Fixture {
    provider: ScriptedProvider,
    clock: VirtualClock,
    agent: Agent,
}

fn fixture(replay: MinimalOutputReplay, thinking: ThinkingLevel, prior_tokens: u64) -> Fixture {
    let provider = ScriptedProvider::with_capabilities(
        [ScriptedTurn::new().usage(prompt_usage(prior_tokens)).text("ready").stop()],
        capabilities(replay),
    );
    let clock = VirtualClock::new();
    let agent = Agent::builder()
        .system_prompt("You are tea.")
        .model(model())
        .thinking_level(thinking)
        .tool(Arc::new(SlowTool))
        .model_provider(Arc::new(provider.clone()))
        .cache_warming(CacheWarmingPolicy::new(Arc::new(clock.clone())))
        .build();
    // A completed earlier turn supplies the prompt size used for economics,
    // as Pi reads the branch's latest assistant usage.
    smol::block_on(agent.start_prompt("warm up").expect("run").drive()).expect("first run");
    Fixture {
        provider,
        clock,
        agent,
    }
}

fn spawn(run: RunHandle) -> (Arc<RunHandle>, std::thread::JoinHandle<()>) {
    let run = Arc::new(run);
    let driver = Arc::clone(&run);
    let thread = std::thread::spawn(move || {
        smol::block_on(driver.drive()).expect("run settles");
    });
    (run, thread)
}

fn records(events: &[AgentEvent]) -> Vec<CacheMaintenanceRecord> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::CacheMaintenance { record } => Some(record.clone()),
            _ => None,
        })
        .collect()
}

fn inactive_reason(run: &RunHandle) -> String {
    match run.cache_warming_status() {
        Some(CacheWarmingStatus::Inactive { reason, .. }) => reason,
        other => panic!("expected inactive warming, found {other:?}"),
    }
}

/// A tool whose execution waits on a gate, modelling a long tool wait.
struct SlowTool;

static SLOW_TOOL_SCHEMA: std::sync::LazyLock<JsonValue> =
    std::sync::LazyLock::new(|| JsonValue::object([("type", JsonValue::from("object"))]));

impl AgentTool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }

    fn description(&self) -> &str {
        "waits for the test"
    }

    fn schema(&self) -> &JsonValue {
        &SLOW_TOOL_SCHEMA
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            SLOW_TOOL_GATE.wait().await;
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content: "slow done".into(),
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

/// A process-wide async gate for the one test that uses the slow tool.
struct AsyncGate {
    open: std::sync::Mutex<bool>,
    wakers: std::sync::Mutex<Vec<std::task::Waker>>,
}

static SLOW_TOOL_GATE: AsyncGate = AsyncGate {
    open: std::sync::Mutex::new(false),
    wakers: std::sync::Mutex::new(Vec::new()),
};

impl AsyncGate {
    fn release(&self) {
        *self.open.lock().expect("gate") = true;
        for waker in self.wakers.lock().expect("gate").drain(..) {
            waker.wake();
        }
    }

    async fn wait(&self) {
        std::future::poll_fn(|context| {
            if *self.open.lock().expect("gate") {
                return std::task::Poll::Ready(());
            }
            self.wakers.lock().expect("gate").push(context.waker().clone());
            if *self.open.lock().expect("gate") {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
    }
}

#[test]
fn a_long_generation_is_refreshed_with_the_exact_request_and_attributed_usage() {
    let fixture = fixture(MinimalOutputReplay::Safe, ThinkingLevel::High, 100_000);
    fixture.provider.push_turn(
        ScriptedTurn::new()
            .pause("long")
            .usage(prompt_usage(100_200))
            .text("done")
            .stop(),
    );
    fixture.provider.push_maintenance_turn(
        ScriptedTurn::new()
            .usage(Usage {
                input_tokens: Some(100_100),
                cache_read_tokens: Some(100_000),
                output_tokens: Some(1),
                ..Usage::default()
            })
            .text("ignored maintenance output")
            .tool_call("never", "slow", "{}")
            .stop(),
    );
    let (run, thread) = spawn(fixture.agent.start_prompt("think hard").expect("run"));
    assert!(fixture.provider.wait_for_requests(2, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));
    assert!(matches!(
        run.cache_warming_status(),
        Some(CacheWarmingStatus::Scheduled { next_warm_at }) if next_warm_at == DELAY
    ));

    fixture.clock.advance(DELAY);
    assert!(fixture.provider.wait_for_requests(3, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));
    fixture.clock.advance(DELAY);
    assert!(fixture.provider.wait_for_requests(4, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));

    let real = fixture.provider.requests_for(RequestPurpose::Turn);
    let maintenance = fixture.provider.requests_for(RequestPurpose::CacheMaintenance);
    assert_eq!(maintenance.len(), 2);
    for request in &maintenance {
        // The admitted request is replayed exactly: same transcript, model,
        // reasoning, and session; only the purpose and output cap differ.
        assert_eq!(request.transcript, real[1].transcript);
        assert_eq!(request.model, real[1].model);
        assert_eq!(request.thinking_level, ThinkingLevel::High);
        assert_eq!(request.session_id, real[1].session_id);
        assert_eq!(request.max_output_tokens, Some(1));
    }

    fixture.provider.gate("long").release();
    thread.join().expect("driver thread");
    let events = run.events().events;
    let records = records(&events);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].outcome, CacheMaintenanceOutcome::Completed);
    assert_eq!(records[0].model, Some(model()));
    assert_eq!(records[0].usage.cache_read_tokens, Some(100_000));
    // 100 uncached + 100k cache reads + 1 output token, at listed prices.
    assert_eq!(records[0].estimated_cost.as_deref(), Some("0.050525"));
    assert_eq!(records[1].outcome, CacheMaintenanceOutcome::Completed);
    // Maintenance never became model context, never ran its tool call, and
    // is not a model turn.
    let messages = fixture.agent.snapshot().messages;
    assert!(!format!("{messages:?}").contains("ignored maintenance output"));
    assert_eq!(fixture.agent.snapshot().accounting.turns.len(), 2);
    // Attribution precedes the terminal event.
    let end = events
        .iter()
        .position(|event| matches!(event.kind, AgentEventKind::AgentEnd { .. }))
        .expect("agent end");
    assert!(events[..end]
        .iter()
        .any(|event| matches!(event.kind, AgentEventKind::CacheMaintenance { .. })));
    // A settled run keeps nothing alive.
    assert_eq!(fixture.clock.sleepers(), 0);
    fixture.clock.advance(Duration::from_secs(3_600));
    assert_eq!(fixture.provider.request_count(), 4);
}

#[test]
fn a_long_tool_wait_is_warmed_and_the_next_real_request_supersedes_warming() {
    let fixture = fixture(MinimalOutputReplay::Safe, ThinkingLevel::Off, 100_000);
    fixture.provider.push_turn(
        ScriptedTurn::new()
            .usage(prompt_usage(100_200))
            .tool_call("call-slow", "slow", "{}")
            .end_tool_use(),
    );
    fixture
        .provider
        .push_turn(ScriptedTurn::new().text("after tool").stop());
    // The second refresh is still in flight when the real continuation is
    // ready; it reports usage before holding.
    fixture.provider.push_maintenance_turn(ScriptedTurn::new().stop());
    fixture.provider.push_maintenance_turn(
        ScriptedTurn::new()
            .usage(Usage {
                input_tokens: Some(100_300),
                cache_read_tokens: Some(100_300),
                ..Usage::default()
            })
            .pause("held-refresh")
            .stop(),
    );
    let (run, thread) = spawn(fixture.agent.start_prompt("run the tool").expect("run"));
    assert!(fixture.provider.wait_for_requests(2, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));
    fixture.clock.advance(DELAY);
    assert!(fixture.provider.wait_for_requests(3, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));
    fixture.clock.advance(DELAY);
    assert!(fixture.provider.wait_for_requests(4, WAIT));
    assert!(fixture.provider.gate("held-refresh").wait_reached(1, WAIT));

    // The tool finishes; the real continuation dispatches immediately and
    // cancels the held refresh instead of waiting for it.
    SLOW_TOOL_GATE.release();
    thread.join().expect("driver thread");
    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[4].purpose, RequestPurpose::Turn);
    assert!(!fixture.provider.gate("held-refresh").is_released());

    let records = records(&run.events().events);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].outcome, CacheMaintenanceOutcome::Completed);
    // Billing evidence from the superseded refresh is kept.
    assert_eq!(records[1].outcome, CacheMaintenanceOutcome::Cancelled);
    assert_eq!(records[1].usage.cache_read_tokens, Some(100_300));
    assert!(fixture
        .agent
        .snapshot()
        .messages
        .iter()
        .all(|message| !format!("{message:?}").contains("held-refresh")));
}

#[test]
fn a_refresh_after_its_safe_deadline_is_skipped() {
    let fixture = fixture(MinimalOutputReplay::Safe, ThinkingLevel::Off, 100_000);
    fixture
        .provider
        .push_turn(ScriptedTurn::new().pause("long").text("done").stop());
    let (run, thread) = spawn(fixture.agent.start_prompt("go").expect("run"));
    assert!(fixture.provider.wait_for_requests(2, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));
    // A five-minute cache is refreshed at 4m30s and keeps 15 s of the 30 s
    // margin. Simulate a timer delayed by host suspension.
    fixture.clock.set(Duration::from_millis(285_001));
    let deadline = std::time::Instant::now() + WAIT;
    while inactive_reason_or_none(&run).as_deref() != Some("cache refresh deadline missed") {
        assert!(std::time::Instant::now() < deadline, "deadline was not detected");
        std::thread::yield_now();
    }
    assert_eq!(fixture.provider.request_count(), 2);
    fixture.provider.gate("long").release();
    thread.join().expect("driver thread");
    assert!(records(&run.events().events).is_empty());
}

fn inactive_reason_or_none(run: &RunHandle) -> Option<String> {
    match run.cache_warming_status() {
        Some(CacheWarmingStatus::Inactive { reason, .. }) => Some(reason),
        _ => None,
    }
}

#[test]
fn unprofitable_unknown_and_unsafe_requests_are_not_warmed() {
    for (replay, thinking, prior, expected) in [
        (
            MinimalOutputReplay::Safe,
            ThinkingLevel::Off,
            5_000,
            "expected savings below threshold",
        ),
        (
            MinimalOutputReplay::SafeWithoutThinking,
            ThinkingLevel::High,
            100_000,
            "request cannot be replayed safely",
        ),
    ] {
        let fixture = fixture(replay, thinking, prior);
        fixture
            .provider
            .push_turn(ScriptedTurn::new().pause("long").text("done").stop());
        let (run, thread) = spawn(fixture.agent.start_prompt("go").expect("run"));
        assert!(fixture.provider.wait_for_requests(2, WAIT));
        if expected == "expected savings below threshold" {
            assert!(fixture.clock.wait_for_sleepers(1, WAIT));
            fixture.clock.advance(DELAY);
            let deadline = std::time::Instant::now() + WAIT;
            while inactive_reason_or_none(&run).is_none() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            match run.cache_warming_status() {
                Some(CacheWarmingStatus::Inactive {
                    decision: Some(decision),
                    ..
                }) => assert!(decision.economics_available),
                other => panic!("expected a stopping decision, found {other:?}"),
            }
        }
        assert_eq!(inactive_reason(&run), expected);
        assert_eq!(fixture.provider.request_count(), 2);
        fixture.provider.gate("long").release();
        thread.join().expect("driver thread");
    }

    // Without a declared cache lifetime nothing is scheduled.
    let provider = ScriptedProvider::new([ScriptedTurn::new().pause("long").text("x").stop()]);
    let clock = VirtualClock::new();
    let agent = Agent::builder()
        .system_prompt("p")
        .model(model())
        .model_provider(Arc::new(provider.clone()))
        .cache_warming(CacheWarmingPolicy::new(Arc::new(clock.clone())))
        .build();
    let (run, thread) = spawn(agent.start_prompt("go").expect("run"));
    assert!(provider.wait_for_requests(1, WAIT));
    assert_eq!(inactive_reason(&run), "cache lifetime unavailable");
    assert_eq!(clock.sleepers(), 0);
    provider.gate("long").release();
    thread.join().expect("driver thread");
}

#[test]
fn refreshes_never_extend_warming_past_one_hour_after_the_real_request() {
    let fixture = fixture(MinimalOutputReplay::Safe, ThinkingLevel::Off, 100_000);
    fixture
        .provider
        .push_turn(ScriptedTurn::new().pause("long").text("done").stop());
    let (run, thread) = spawn(fixture.agent.start_prompt("go").expect("run"));
    assert!(fixture.provider.wait_for_requests(2, WAIT));
    assert!(fixture.clock.wait_for_sleepers(1, WAIT));
    // Refreshes run every 270 s from the real request; the thirteenth, at
    // 58m30s, is the last one that starts within the hour.
    for refresh in 0..13 {
        fixture.clock.advance(DELAY);
        assert!(
            fixture.provider.wait_for_requests(3 + refresh, WAIT),
            "refresh {refresh}"
        );
        if refresh < 12 {
            assert!(fixture.clock.wait_for_sleepers(1, WAIT), "refresh {refresh}");
        }
    }
    let deadline = std::time::Instant::now() + WAIT;
    while inactive_reason_or_none(&run).is_none() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(inactive_reason(&run), "one-hour safety limit reached");
    fixture.clock.advance(Duration::from_secs(3_600));
    assert_eq!(fixture.provider.request_count(), 15);
    fixture.provider.gate("long").release();
    thread.join().expect("driver thread");
    assert_eq!(records(&run.events().events).len(), 13);
}
