//! Pure policy tests ported from Pi's `cache-warmer.test.ts`
//! ("derives eligibility and timing from retention and provider behavior").

use super::*;

fn opus_pricing() -> ModelPricing {
    ModelPricing {
        input: "5".into(),
        output: "25".into(),
        cache_read: "0.5".into(),
        cache_write: "6.25".into(),
    }
}

#[test]
fn timing_keeps_ninety_percent_and_a_ten_second_margin() {
    assert_eq!(
        warming_delay(Duration::from_secs(300)),
        Some(Duration::from_secs(270))
    );
    assert_eq!(
        warming_delay(Duration::from_secs(60)),
        Some(Duration::from_secs(50))
    );
    assert_eq!(warming_delay(Duration::from_secs(10)), None);
    assert_eq!(
        warming_delay(Duration::from_secs(3_600)),
        Some(Duration::from_secs(3_240))
    );
}

#[test]
fn replay_safety_follows_the_declared_capability_and_reasoning() {
    let capability = |minimal_output_replay| PromptCacheCapability {
        ttl_seconds: 300,
        minimal_output_replay,
    };
    assert!(replay_is_safe(
        &capability(MinimalOutputReplay::Safe),
        ThinkingLevel::High
    ));
    assert!(!replay_is_safe(
        &capability(MinimalOutputReplay::SafeWithoutThinking),
        ThinkingLevel::Medium
    ));
    assert!(replay_is_safe(
        &capability(MinimalOutputReplay::SafeWithoutThinking),
        ThinkingLevel::Off
    ));
    assert!(!replay_is_safe(
        &capability(MinimalOutputReplay::Unsafe),
        ThinkingLevel::Off
    ));
}

#[test]
fn economics_match_pi_and_unknown_values_stay_unavailable() {
    let decision = evaluate(Some(100_000), Some(&opus_pricing()));
    assert!((decision.miss_cost - 0.575).abs() < 1e-9);
    assert!((decision.warm_cost - 0.050025).abs() < 1e-9);
    assert!(decision.economics_available);
    assert_eq!(decision.action, WarmingAction::Warm);

    let small = evaluate(Some(5_000), Some(&opus_pricing()));
    assert!(small.economics_available);
    assert_eq!(small.action, WarmingAction::Stop);

    for unknown in [
        evaluate(None, Some(&opus_pricing())),
        evaluate(Some(0), Some(&opus_pricing())),
        evaluate(Some(100_000), None),
        evaluate(
            Some(100_000),
            Some(&ModelPricing {
                input: "unknown".into(),
                ..opus_pricing()
            }),
        ),
    ] {
        assert!(!unknown.economics_available);
        assert_eq!(unknown.action, WarmingAction::Stop);
    }
}

#[test]
fn estimated_cost_prices_cache_subsets_of_the_full_prompt() {
    let prices = Prices::parse(&opus_pricing()).expect("prices");
    let usage = Usage {
        input_tokens: Some(100_100),
        cache_read_tokens: Some(100_000),
        output_tokens: Some(1),
        ..Usage::default()
    };
    // 100 uncached input at $5/M + 100k cache reads at $0.50/M + 1 output at $25/M.
    assert!((prices.cost(&usage) - (0.0005 + 0.05 + 0.000025)).abs() < 1e-12);
}

mod driver {
    use super::*;
    use crate::testing::{ScriptedProvider, VirtualClock};
    use crate::transcript::Transcript;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn request() -> ModelRequest {
        ModelRequest {
            purpose: RequestPurpose::Turn,
            transcript: Transcript::default(),
            model: None,
            selected_model: None,
            thinking_level: ThinkingLevel::Off,
            session_id: None,
            max_output_tokens: None,
        }
    }

    #[test]
    fn a_stale_conversation_stops_the_next_refresh() {
        let clock = VirtualClock::new();
        let warmer = Arc::new(CacheWarmer::new(
            &CacheWarmingPolicy::new(Arc::new(clock.clone())),
            Some(100_000),
        ));
        let provider = ScriptedProvider::new([]);
        let stale = Arc::new(AtomicBool::new(false));
        let check_stale = Arc::clone(&stale);
        warmer.start(
            &request(),
            Arc::new(provider.clone()),
            Some(PromptCacheCapability {
                ttl_seconds: 300,
                minimal_output_replay: MinimalOutputReplay::Safe,
            }),
            Some(opus_pricing()),
            CurrentRequest {
                check: Box::new(move || {
                    check_stale
                        .load(Ordering::SeqCst)
                        .then_some("conversation context changed")
                }),
            },
        );
        let driver = Arc::clone(&warmer);
        let thread = std::thread::spawn(move || smol::block_on(driver.drive()));
        assert!(clock.wait_for_sleepers(1, Duration::from_secs(5)));
        stale.store(true, Ordering::SeqCst);
        clock.advance(Duration::from_secs(270));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !matches!(warmer.status(), CacheWarmingStatus::Inactive { .. }) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            warmer.status(),
            CacheWarmingStatus::Inactive {
                reason: "conversation context changed".into(),
                decision: None,
            }
        );
        assert_eq!(provider.request_count(), 0);
        smol::block_on(warmer.settle());
        thread.join().expect("driver exits after settlement");
        assert!(warmer.take_records().is_empty());
    }
}
