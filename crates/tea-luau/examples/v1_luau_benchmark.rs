//! Small, dependency-free Luau benchmark and isolation harness.
//!
//! Run it with the repository's pinned toolchain:
//!
//! ```text
//! cargo +nightly-2026-07-24 run -p tea-luau --example v1_luau_benchmark --release
//! ```
//!
//! The output is observational evidence, not a pass/fail performance gate. Wall-clock
//! values vary by machine, compiler flags, and system load; compare runs on the same host
//! and build mode. The semantic checks are the hard gate: every policy must retain its own
//! declaration marker and every benchmark hook must return `allow`.
//!
//! The harness covers three Luau runtime questions:
//!
//! * What is the cost of creating and dropping one sandboxed policy VM?
//! * What is the cost of repeatedly invoking a policy pre-tool hook?
//! * Can many independently loaded policies coexist without sharing declaration state?
//!
//! The final section loads 256 separate VMs and verifies each one. It intentionally does
//! not use threads, random input, or an external benchmark crate, so the workload remains
//! deterministic and easy to reproduce while still surfacing gross isolation regressions.

use std::hint::black_box;
use std::time::{Duration, Instant};
use tea_core::hooks::BeforeToolCall;
use tea_core::state::{SerializedJson, ToolCallId};
use tea_core::tool::ToolCall;
use tea_luau::LuaPolicy;

const STARTUP_SAMPLES: usize = 32;
const HOOK_SAMPLES: usize = 32;
const HOOK_CALLS_PER_SAMPLE: usize = 1_000;
const ISOLATED_POLICY_COUNT: usize = 256;

const SIMPLE_POLICY: &str = r#"
    return {
        prompt_sections = { { id = "benchmark", content = "benchmark policy" } },
        before_tool = function(_) return "allow" end,
    }
"#;

#[derive(Debug)]
struct DurationStats {
    count: usize,
    total: Duration,
    min: Duration,
    p50: Duration,
    p95: Duration,
    max: Duration,
}

impl DurationStats {
    fn from_samples(mut samples: Vec<Duration>) -> Self {
        assert!(
            !samples.is_empty(),
            "benchmark must produce at least one sample"
        );
        samples.sort_unstable();
        let total = samples
            .iter()
            .copied()
            .fold(Duration::ZERO, Duration::saturating_add);
        let percentile = |percent: usize| samples[((samples.len() - 1) * percent) / 100];
        Self {
            count: samples.len(),
            total,
            min: samples[0],
            p50: percentile(50),
            p95: percentile(95),
            max: samples[samples.len() - 1],
        }
    }

    fn print(&self, unit: &str) {
        let average_us = self.total.as_secs_f64() * 1_000_000.0 / self.count as f64;
        println!(
            "  {unit}: samples={} total={:?} min={:?} p50={:?} p95={:?} max={:?} avg={average_us:.3}us",
            self.count, self.total, self.min, self.p50, self.p95, self.max
        );
    }
}

fn benchmark_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::new("luau-benchmark-call").expect("static call ID is non-empty"),
        name: "execute_code".to_owned(),
        arguments: SerializedJson::new("{}"),
    }
}

fn check_allow(policy: &LuaPolicy, call: &ToolCall) -> Result<(), String> {
    let decision = policy
        .before_tool_call(call)
        .map_err(|error| error.to_string())?;
    if !matches!(decision, BeforeToolCall::Allow) {
        return Err(format!("benchmark hook returned {decision:?}"));
    }
    Ok(())
}

fn startup_and_teardown() -> Result<(), String> {
    let mut startup = Vec::with_capacity(STARTUP_SAMPLES);
    let mut lifecycle = Vec::with_capacity(STARTUP_SAMPLES);

    for _ in 0..STARTUP_SAMPLES {
        let started = Instant::now();
        let policy = LuaPolicy::load(SIMPLE_POLICY).map_err(|error| error.to_string())?;
        black_box(policy.prompt_sections());
        startup.push(started.elapsed());
        drop(policy);

        let started = Instant::now();
        let policy = LuaPolicy::load(SIMPLE_POLICY).map_err(|error| error.to_string())?;
        black_box(policy.prompt_sections());
        drop(policy);
        lifecycle.push(started.elapsed());
    }

    println!("startup / teardown (one fresh sandboxed VM per sample):");
    DurationStats::from_samples(startup).print("load only");
    DurationStats::from_samples(lifecycle).print("load + drop");
    Ok(())
}

fn hook_invocation() -> Result<(), String> {
    let policy = LuaPolicy::load(SIMPLE_POLICY).map_err(|error| error.to_string())?;
    let call = benchmark_call();
    let mut samples = Vec::with_capacity(HOOK_SAMPLES);

    for _ in 0..HOOK_SAMPLES {
        let started = Instant::now();
        for _ in 0..HOOK_CALLS_PER_SAMPLE {
            check_allow(&policy, &call)?;
        }
        samples.push(started.elapsed());
    }

    println!("hook invocation (one VM, {HOOK_CALLS_PER_SAMPLE} calls per sample):");
    DurationStats::from_samples(samples).print("batch");
    Ok(())
}

fn isolated_policies() -> Result<(), String> {
    let started = Instant::now();
    let mut policies = Vec::with_capacity(ISOLATED_POLICY_COUNT);
    for index in 0..ISOLATED_POLICY_COUNT {
        let source = format!(
            "return {{ prompt_sections = {{ {{ id = \"isolated-{index}\", content = \"isolated-policy-{index}\" }} }}, before_tool = function(_) return \"allow\" end }}"
        );
        policies.push(LuaPolicy::load(&source).map_err(|error| error.to_string())?);
    }
    let load_elapsed = started.elapsed();

    let call = benchmark_call();
    let verification_started = Instant::now();
    for (index, policy) in policies.iter().enumerate() {
        let expected = format!("isolated-policy-{index}");
        let actual = policy
            .prompt_sections()
            .first()
            .map(|section| section.content.as_str());
        if actual != Some(expected.as_str()) {
            return Err(format!(
                "policy {index} observed {actual:?}, expected {expected:?}",
            ));
        }
        check_allow(policy, &call)?;
    }
    let verification_elapsed = verification_started.elapsed();

    let teardown_started = Instant::now();
    drop(policies);
    let teardown_elapsed = teardown_started.elapsed();

    println!("isolated policy stress ({ISOLATED_POLICY_COUNT} independent VMs):");
    println!("  load all: {load_elapsed:?}");
    println!("  verify markers + hooks: {verification_elapsed:?}");
    println!("  drop all: {teardown_elapsed:?}");
    Ok(())
}

fn main() -> Result<(), String> {
    println!("tea-luau benchmark (observational; no timing thresholds)");
    startup_and_teardown()?;
    hook_invocation()?;
    isolated_policies()?;
    println!("semantic checks: passed");
    Ok(())
}
