//! Explicit live-provider compaction canary with content-free output.

use std::env;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use tea_agent::build_compacting_host_agent;
use tea_core::event::AgentEventKind;
use tea_core::provider::openrouter::{OpenRouterConfig, OpenRouterProvider};
use tea_core::state::ModelDescriptor;
use tea_core::DefaultCodingTools;

const CONTINUATION_FACT: &str = "amber-orchid-47";

struct Args {
    model: String,
    cwd: PathBuf,
    pressure_bytes: usize,
    context_window: NonZeroU64,
    continuation_check: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut model = None;
        let mut cwd = None;
        let mut pressure_bytes = 5_000_usize;
        let mut context_window = NonZeroU64::new(5_000).expect("fixed non-zero window");
        let mut continuation_check = false;
        let mut arguments = env::args_os().skip(1);
        while let Some(flag) = arguments.next() {
            let flag = flag.to_string_lossy().into_owned();
            if flag == "--continuation-check" {
                if continuation_check {
                    return Err("duplicate option --continuation-check".into());
                }
                continuation_check = true;
                continue;
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--model" => set_once(&mut model, value, "--model")?,
                "--cwd" => set_once(&mut cwd, value, "--cwd")?,
                "--pressure-bytes" => {
                    pressure_bytes = value
                        .to_string_lossy()
                        .parse()
                        .map_err(|_| "--pressure-bytes must be a positive integer".to_owned())?;
                }
                "--context-window" => {
                    context_window =
                        NonZeroU64::new(value.to_string_lossy().parse().map_err(|_| {
                            "--context-window must be a positive integer".to_owned()
                        })?)
                        .ok_or_else(|| "--context-window must be non-zero".to_owned())?;
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        let model = model
            .ok_or_else(|| "missing required option --model".to_owned())?
            .into_string()
            .map_err(|_| "--model must be valid UTF-8".to_owned())?;
        if model.trim().is_empty() || pressure_bytes == 0 {
            return Err("--model and --pressure-bytes must not be empty or zero".into());
        }
        Ok(Self {
            model,
            cwd: cwd
                .map(PathBuf::from)
                .unwrap_or(env::current_dir().map_err(|error| error.to_string())?),
            pressure_bytes,
            context_window,
            continuation_check,
        })
    }
}

fn set_once(
    destination: &mut Option<std::ffi::OsString>,
    value: std::ffi::OsString,
    flag: &str,
) -> Result<(), String> {
    if destination.replace(value).is_some() {
        Err(format!("duplicate option {flag}"))
    } else {
        Ok(())
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("tea-compaction-canary: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse()?;
    let api_key = env::var("OPENROUTER_API_KEY")
        .map_err(|_| "OPENROUTER_API_KEY must be supplied by the caller".to_owned())?;
    let provider = Arc::new(OpenRouterProvider::new(
        OpenRouterConfig::try_new(api_key, args.model.clone())
            .map_err(|error| error.to_string())?,
    ));
    let agent = build_compacting_host_agent(
        DefaultCodingTools::new(&args.cwd).map_err(|error| error.to_string())?,
        ModelDescriptor {
            provider: "openrouter".into(),
            model: args.model.clone(),
            revision: None,
        },
        provider,
        args.context_window,
    )
    .map_err(|error| error.to_string())?;
    let pressure = "x".repeat(args.pressure_bytes);
    // Follow-ups keep all pressured turns in one run. Automatic compaction is
    // an in-run next-request transition, not a mutation between idle runs.
    for turn in 2..=4 {
        let instruction = if args.continuation_check && turn == 4 {
            "What durable canary fact was introduced in turn 1? Reply with only that exact fact."
                .to_owned()
        } else {
            format!("Canary turn {turn}. Reply with exactly ACK_{turn}; do not call tools.")
        };
        agent
            .follow_up(format!("{instruction} Context padding: {pressure}"))
            .map_err(|error| error.to_string())?;
    }
    let run = agent
        .start_prompt(format!(
            "Canary turn 1. The durable canary fact is {CONTINUATION_FACT}. Reply with exactly ACK_1; do not call tools. Context padding: {pressure}"
        ))
        .map_err(|error| error.to_string())?;
    let drive_failed = smol::block_on(run.drive()).is_err();
    let continuation_fact_survived = args.continuation_check
        && agent
            .snapshot()
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                tea_core::AgentMessage::Assistant { content, .. } => {
                    Some(content.trim() == CONTINUATION_FACT)
                }
                tea_core::AgentMessage::User { .. } | tea_core::AgentMessage::ToolResult { .. } => {
                    None
                }
            })
            .unwrap_or(false);
    let lifecycle: Vec<_> = run
        .events()
        .into_iter()
        .filter_map(|event| match event.kind {
            AgentEventKind::CompactionLifecycle { record } => Some(record),
            _ => None,
        })
        .collect();
    let committed = lifecycle.iter().any(|record| {
        matches!(
            record,
            tea_core::CompactionLifecycleRecord::Terminal {
                outcome: tea_core::CompactionTerminalOutcome::Committed,
                ..
            }
        )
    });
    let terminal_count = lifecycle
        .iter()
        .filter(|record| matches!(record, tea_core::CompactionLifecycleRecord::Terminal { .. }))
        .count();
    let provider_usage_records = lifecycle
        .iter()
        .filter(|record| {
            matches!(
                record,
                tea_core::CompactionLifecycleRecord::ProviderUsageObserved {
                    usage: Some(usage),
                    ..
                } if usage.is_reported()
            )
        })
        .count();
    let provider_cache_accounting_records = lifecycle
        .iter()
        .filter(|record| {
            matches!(
                record,
                tea_core::CompactionLifecycleRecord::ProviderUsageObserved {
                    usage: Some(usage),
                    ..
                } if usage.cache_read_tokens.is_some() || usage.cache_write_tokens.is_some()
            )
        })
        .count();
    let adapter_request_observations = lifecycle
        .iter()
        .filter(|record| {
            matches!(
                record,
                tea_core::CompactionLifecycleRecord::ProviderUsageObserved {
                    request_observation: Some(_),
                    ..
                }
            )
        })
        .count();
    if !committed || (args.continuation_check && !continuation_fact_survived) {
        let automatic_starts = run
            .events()
            .iter()
            .filter(|event| matches!(event.kind, AgentEventKind::AutomaticCompactionStart { .. }))
            .count();
        let turns = run
            .events()
            .iter()
            .filter(|event| matches!(event.kind, AgentEventKind::TurnStart { .. }))
            .count();
        println!(
            "{{\"schema_version\":\"tea-compaction-canary/v2\",\"model\":\"{}\",\"strategy_id\":\"{}\",\"compaction_lifecycle_records\":{},\"terminal_records\":{},\"continuation_fact_checked\":{},\"continuation_fact_survived\":{},\"run_failed\":{},\"committed\":{}}}",
            args.model.replace('"', "\\\""),
            tea_core::CACHE_REPLAY_SUMMARY_V0,
            lifecycle.len(),
            terminal_count,
            args.continuation_check,
            continuation_fact_survived,
            drive_failed,
            committed,
        );
        return Err(format!(
            "the canary did not satisfy compaction/continuation acceptance (turns: {turns}, automatic starts: {automatic_starts}, lifecycle records: {})",
            lifecycle.len(),
        ));
    }
    println!(
        "{{\"schema_version\":\"tea-compaction-canary/v2\",\"model\":\"{}\",\"strategy_id\":\"{}\",\"compaction_lifecycle_records\":{},\"terminal_records\":{},\"provider_usage_records\":{},\"provider_cache_accounting_records\":{},\"adapter_request_observations\":{},\"continuation_fact_checked\":{},\"continuation_fact_survived\":{},\"run_failed\":{},\"committed\":true}}",
        args.model.replace('"', "\\\""),
        tea_core::CACHE_REPLAY_SUMMARY_V0,
        lifecycle.len(),
        terminal_count,
        provider_usage_records,
        provider_cache_accounting_records,
        adapter_request_observations,
        args.continuation_check,
        continuation_fact_survived,
        drive_failed,
    );
    Ok(())
}
