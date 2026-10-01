//! Durable attribution of active-work cache maintenance.

use super::*;
use crate::cache_warming::CacheWarmingPolicy;
use crate::scheduler::{
    MinimalOutputReplay, ModelCapabilities, ModelPricing, PromptCacheCapability, RequestPurpose,
};
use crate::testing::{ScriptedProvider, ScriptedTurn, VirtualClock};

fn temporary_session_directory(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "tea-core-cache-warming-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
    ))
}

#[test]
fn maintenance_is_a_durable_record_outside_context_and_seeds_from_lane_usage() {
    let directory = temporary_session_directory("durable");
    let store = Arc::new(MemoryArtifactStore::default());
    let provider = ScriptedProvider::with_capabilities(
        [
            ScriptedTurn::new()
                .usage(crate::state::Usage {
                    input_tokens: Some(100_000),
                    ..crate::state::Usage::default()
                })
                .text("first")
                .stop(),
            ScriptedTurn::new().pause("long").text("second").stop(),
        ],
        ModelCapabilities {
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
        },
    );
    provider.push_maintenance_turn(
        ScriptedTurn::new()
            .usage(crate::state::Usage {
                input_tokens: Some(100_000),
                cache_read_tokens: Some(100_000),
                output_tokens: Some(1),
                ..crate::state::Usage::default()
            })
            .text("maintenance text never persists")
            .stop(),
    );
    let clock = VirtualClock::new();
    let (manager, identity, _) = fixture_manager(Arc::new(provider.clone()), store.clone());
    let services = RuntimeServices::new(Arc::new(provider.clone()), ToolRegistry::default())
        .cache_warming(CacheWarmingPolicy::new(Arc::new(clock.clone())));
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("runtime-cache-warming").expect("session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    append_initial_revision(&mut session, &identity);
    let runtime = Arc::new(
        SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: services,
            artifacts: store.clone(),
            rollover_budget: 1,
            subagents: None,
        })
        .expect("supervisor creates"),
    );
    smol::block_on(runtime.run_root_prompt("first")).expect("first operation settles");

    // The second operation is a fresh epoch; its warmer is seeded from the
    // lane's durable usage, so its long first generation can be priced.
    let driver = Arc::clone(&runtime);
    let thread = std::thread::spawn(move || {
        smol::block_on(driver.run_root_prompt("second")).expect("second operation settles");
    });
    assert!(provider.wait_for_requests(2, Duration::from_secs(5)));
    assert!(clock.wait_for_sleepers(1, Duration::from_secs(5)));
    clock.advance(Duration::from_secs(270));
    assert!(provider.wait_for_requests(3, Duration::from_secs(5)));
    assert!(clock.wait_for_sleepers(1, Duration::from_secs(5)));
    provider.gate("long").release();
    thread.join().expect("driver thread");
    assert_eq!(provider.requests_for(RequestPurpose::CacheMaintenance).len(), 1);

    let snapshot = runtime.snapshot().expect("snapshot");
    let records = snapshot
        .records()
        .iter()
        .filter_map(|stored| match &stored.record {
            LaneRecord::CacheMaintenance(record) => Some(record.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].outcome,
        tea_session::CacheMaintenanceOutcome::Completed
    );
    assert_eq!(records[0].usage.cache_read_tokens, Some(100_000));
    assert_eq!(records[0].estimated_cost.as_deref(), Some("0.050025"));
    // Maintenance never became a durable provider request or a session entry.
    let started = snapshot
        .records()
        .iter()
        .filter(|stored| matches!(stored.record, LaneRecord::ProviderRequestStarted(_)))
        .count();
    assert_eq!(started, 2);
    let context = crate::runtime::context::derive_default_snapshot_context(&snapshot, LaneId::main())
        .expect("context derives");
    assert!(!format!("{:?}", context.messages).contains("maintenance text"));
    drop(runtime);

    // The record survives reopen and snapshot validation.
    let reopened = JsonlSession::open(&directory, DurabilityMode::Strict).expect("reopens");
    let reopened_snapshot = reopened.snapshot().expect("snapshot");
    let reopened_records = reopened_snapshot
        .records()
        .iter()
        .filter(|stored| matches!(stored.record, LaneRecord::CacheMaintenance(_)))
        .count();
    assert_eq!(reopened_records, 1);
    tea_session::verify_session(&reopened_snapshot, store.as_ref(), Vec::new())
        .expect("session verifies");
    let _ = std::fs::remove_dir_all(&directory);
}
