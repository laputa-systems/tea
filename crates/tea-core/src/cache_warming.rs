//! Active-work prompt-cache warming.
//!
//! This is a port of upstream Pi's `cache-warmer.ts` in its default
//! `streaming` mode: while an agent run is still working (a long generation or
//! a long tool wait), the most recent admitted model request is replayed with a
//! one-token output cap shortly before its provider prompt-cache entry expires.
//! Warming never happens between completed user turns, and a settled run never
//! keeps a session alive.
//!
//! The warmer is subordinate to its run. It is polled inside the run's own
//! [`crate::run::RunHandle::drive`] future rather than spawned, so it cannot
//! outlive the run, and its time comes from a host-supplied
//! [`MaintenanceClock`]. A maintenance request reuses the exact admitted
//! request: it never reruns routing, hooks, or context transforms, never
//! changes the physical model, and never executes returned tool calls. Its
//! usage is reported as a [`CacheMaintenanceRecord`], never as an assistant
//! message, and it does not touch the prompt-layout ledger.
//!
//! Decisions follow Pi: refresh at 90% of the declared lifetime while keeping
//! at least ten seconds of margin, skip a refresh whose timer fired so late
//! that half of that margin is gone (laptop suspension, a blocked executor),
//! stop when the expected savings are below $0.05 or unknown, and never warm
//! for more than an hour after the real request that started the warming.

use crate::scheduler::{
    CancellationToken, MinimalOutputReplay, ModelEventStream, ModelPricing, ModelProvider,
    ModelRequest, ModelStreamEvent, PromptCacheCapability, RequestPurpose,
};
use crate::state::{ModelDescriptor, StopReason, ThinkingLevel, Usage};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Warming never continues past this long after the real request that started it.
pub const MAX_WARMING_AGE: Duration = Duration::from_secs(60 * 60);
/// A refresh is sent only when it is expected to save at least this many dollars.
pub const MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
/// Minimum pre-expiry margin kept by a scheduled refresh.
const MINIMUM_MARGIN: Duration = Duration::from_secs(10);

/// Host-supplied time for cache maintenance.
///
/// `now` must advance while the host is suspended (wall-clock-like), so a
/// refresh delayed by suspension is recognized as late instead of being sent
/// to an expired cache entry. `sleep_until` may wake early or late; the warmer
/// re-reads `now` after every wake.
pub trait MaintenanceClock: Send + Sync {
    /// Current time as a duration since an arbitrary fixed origin.
    fn now(&self) -> Duration;

    /// Resolve no earlier than practical after `deadline` on this clock.
    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Explicit opt-in for active-work cache warming.
#[derive(Clone)]
pub struct CacheWarmingPolicy {
    clock: Arc<dyn MaintenanceClock>,
    prompt_tokens: Option<u64>,
}

impl CacheWarmingPolicy {
    /// Enable warming with a host clock.
    pub fn new(clock: Arc<dyn MaintenanceClock>) -> Self {
        Self {
            clock,
            prompt_tokens: None,
        }
    }

    /// Seed economics with the branch's last provider-reported prompt size,
    /// as Pi reads the latest assistant usage. A run's own usage replaces it.
    pub fn with_prompt_tokens(mut self, tokens: Option<u64>) -> Self {
        self.prompt_tokens = tokens.filter(|tokens| *tokens > 0);
        self
    }

    pub(crate) fn prompt_tokens(&self) -> Option<u64> {
        self.prompt_tokens
    }
}

impl std::fmt::Debug for CacheWarmingPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CacheWarmingPolicy")
            .finish_non_exhaustive()
    }
}

/// Delay from a request to its refresh: 90% of the lifetime while keeping at
/// least ten seconds of margin. `None` when the lifetime is too short to warm.
pub fn warming_delay(ttl: Duration) -> Option<Duration> {
    if ttl <= MINIMUM_MARGIN {
        return None;
    }
    let ninety_percent = Duration::from_millis((ttl.as_millis() * 9 / 10) as u64);
    Some(
        ninety_percent
            .min(ttl - MINIMUM_MARGIN)
            .max(Duration::from_millis(1)),
    )
}

/// Whether replaying `thinking_level` with a one-token output cap keeps the
/// request's cache entry, as declared by the provider.
pub fn replay_is_safe(capability: &PromptCacheCapability, thinking_level: ThinkingLevel) -> bool {
    match capability.minimal_output_replay {
        MinimalOutputReplay::Safe => true,
        MinimalOutputReplay::SafeWithoutThinking => thinking_level == ThinkingLevel::Off,
        MinimalOutputReplay::Unsafe => false,
    }
}

/// Pi's warm-or-stop decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmingAction {
    /// Send the refresh.
    Warm,
    /// Stop warming until the next real request.
    Stop,
}

/// Inputs and outcome of one decision. All dollar values are listed-price
/// estimates, never provider-reported charges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WarmingDecision {
    /// Estimated price of the refresh: a cache read of the prompt plus one output token.
    pub warm_cost: f64,
    /// Estimated extra price of the next real request if the entry is lost.
    pub miss_cost: f64,
    /// `miss_cost - warm_cost` (the continuation probability is 1 while a run is active).
    pub expected_savings: f64,
    /// False when the prompt size or the model's prices are unknown.
    pub economics_available: bool,
    /// `Warm` when the expected savings reach [`MINIMUM_EXPECTED_SAVINGS`].
    pub action: WarmingAction,
}

/// Evaluate one refresh from the last provider-reported prompt size.
pub fn evaluate(prompt_tokens: Option<u64>, pricing: Option<&ModelPricing>) -> WarmingDecision {
    let prices = pricing.and_then(Prices::parse);
    let tokens = prompt_tokens.unwrap_or(0) as f64;
    let (hit, miss, warm) = match prices {
        Some(prices) => {
            let hit = prices.cache_read * tokens;
            let miss = if prices.cache_write > 0.0 {
                prices.cache_write * tokens
            } else {
                prices.input * tokens
            };
            (hit, miss, hit + prices.output)
        }
        None => (0.0, 0.0, 0.0),
    };
    let miss_cost = (miss - hit).max(0.0);
    let expected_savings = miss_cost - warm;
    let economics_available = tokens > 0.0 && prices.is_some() && (hit > 0.0 || miss > 0.0);
    WarmingDecision {
        warm_cost: warm,
        miss_cost,
        expected_savings,
        economics_available,
        action: if economics_available && expected_savings >= MINIMUM_EXPECTED_SAVINGS {
            WarmingAction::Warm
        } else {
            WarmingAction::Stop
        },
    }
}

/// Listed per-token prices in dollars.
#[derive(Clone, Copy)]
struct Prices {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

impl Prices {
    fn parse(pricing: &ModelPricing) -> Option<Self> {
        let per_token = |value: &str| {
            value
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value / 1_000_000.0)
        };
        Some(Self {
            input: per_token(&pricing.input)?,
            output: per_token(&pricing.output)?,
            cache_read: per_token(&pricing.cache_read)?,
            cache_write: per_token(&pricing.cache_write)?,
        })
    }

    fn cost(self, usage: &Usage) -> f64 {
        // Tea reports the full prompt as input; cache reads and writes are
        // subsets priced at their own rates.
        let input = usage.input_tokens.unwrap_or(0);
        let read = usage.cache_read_tokens.unwrap_or(0);
        let write = usage.cache_write_tokens.unwrap_or(0);
        let uncached = input.saturating_sub(read).saturating_sub(write);
        uncached as f64 * self.input
            + read as f64 * self.cache_read
            + write as f64 * self.cache_write
            + usage.output_tokens.unwrap_or(0) as f64 * self.output
    }
}

/// Observable warmer state, for diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub enum CacheWarmingStatus {
    /// Nothing is scheduled.
    Inactive {
        /// Why nothing is scheduled.
        reason: String,
        /// The decision that stopped warming, when one was made.
        decision: Option<WarmingDecision>,
    },
    /// A refresh is armed.
    Scheduled {
        /// Clock time of the next decision.
        next_warm_at: Duration,
    },
    /// A maintenance request is in flight.
    Refreshing {
        /// The decision that sent it.
        decision: WarmingDecision,
    },
}

impl Default for CacheWarmingStatus {
    fn default() -> Self {
        Self::Inactive {
            reason: "waiting for first request".into(),
            decision: None,
        }
    }
}

/// How one maintenance request ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheMaintenanceOutcome {
    /// The provider accepted and finished the refresh.
    Completed,
    /// The provider failed the refresh; warming continues best-effort.
    Failed {
        /// Bounded diagnostic.
        message: String,
    },
    /// The refresh was cancelled because a real request superseded it or the
    /// run settled. Usage observed before cancellation is still reported.
    Cancelled,
}

/// One attributed provider maintenance operation. It is never model context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheMaintenanceRecord {
    /// Physical model that served the refresh (the admitted request's model).
    pub model: Option<ModelDescriptor>,
    /// Selected model identity of the admitted request, when it differed.
    pub selected_model: Option<ModelDescriptor>,
    /// How the refresh ended.
    pub outcome: CacheMaintenanceOutcome,
    /// Provider-reported usage; unknown fields stay `None`.
    pub usage: Usage,
    /// Listed-price estimate of this refresh in decimal dollars, when prices
    /// and usage are known. Never a provider-reported charge.
    pub estimated_cost: Option<String>,
}

/// Snapshot of conversation facts that make an admitted request current.
pub(crate) struct CurrentRequest {
    pub(crate) check: Box<dyn Fn() -> Option<&'static str> + Send + Sync>,
}

struct WarmPlan {
    request: ModelRequest,
    provider: Arc<dyn ModelProvider>,
    pricing: Option<ModelPricing>,
    current: CurrentRequest,
    ttl: Duration,
    delay: Duration,
    started_at: Duration,
}

#[derive(Clone, Copy)]
struct Schedule {
    next_warm_at: Duration,
    /// Latest safe time to send this refresh: half the planned margin.
    refresh_deadline_at: Duration,
}

#[derive(Default)]
struct WarmerState {
    generation: u64,
    plan: Option<(Arc<WarmPlan>, Schedule)>,
    in_flight: Option<CancellationToken>,
    records: Vec<CacheMaintenanceRecord>,
    status: CacheWarmingStatus,
    prompt_tokens: Option<u64>,
    closed: bool,
    driver: Option<Waker>,
    idle: Vec<Waker>,
}

/// The per-run warmer. Shared between the run loop, which starts, observes,
/// and settles it, and the driver future polled beside the run.
pub(crate) struct CacheWarmer {
    clock: Arc<dyn MaintenanceClock>,
    state: Mutex<WarmerState>,
}

impl CacheWarmer {
    pub(crate) fn new(policy: &CacheWarmingPolicy, prompt_tokens: Option<u64>) -> Self {
        Self {
            clock: Arc::clone(&policy.clock),
            state: Mutex::new(WarmerState {
                prompt_tokens,
                ..WarmerState::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WarmerState> {
        self.state.lock().expect("cache warmer mutex poisoned")
    }

    pub(crate) fn status(&self) -> CacheWarmingStatus {
        self.lock().status.clone()
    }

    /// Replace any previous warming with the exact admitted `request`.
    pub(crate) fn start(
        &self,
        request: &ModelRequest,
        provider: Arc<dyn ModelProvider>,
        capability: Option<PromptCacheCapability>,
        pricing: Option<ModelPricing>,
        current: CurrentRequest,
    ) {
        let mut state = self.lock();
        Self::clear(&mut state);
        let Some(capability) = capability else {
            Self::stop_locked(&mut state, "cache lifetime unavailable", None);
            return;
        };
        if !replay_is_safe(&capability, request.thinking_level) {
            Self::stop_locked(&mut state, "request cannot be replayed safely", None);
            return;
        }
        let ttl = Duration::from_secs(capability.ttl_seconds);
        let Some(delay) = warming_delay(ttl) else {
            Self::stop_locked(&mut state, "cache lifetime unavailable", None);
            return;
        };
        let mut maintenance = request.clone();
        maintenance.purpose = RequestPurpose::CacheMaintenance;
        maintenance.max_output_tokens = Some(1);
        let started_at = self.clock.now();
        let plan = Arc::new(WarmPlan {
            request: maintenance,
            provider,
            pricing,
            current,
            ttl,
            delay,
            started_at,
        });
        Self::schedule_locked(&mut state, plan, started_at);
    }

    /// Record the provider-reported prompt size of the latest real request.
    pub(crate) fn observe_prompt_tokens(&self, tokens: Option<u64>) {
        if let Some(tokens) = tokens.filter(|tokens| *tokens > 0) {
            self.lock().prompt_tokens = Some(tokens);
        }
    }

    /// Stop warming because the run settled, cancel any refresh in flight,
    /// and wait until its outcome has been recorded.
    pub(crate) async fn settle(&self) {
        {
            let mut state = self.lock();
            if !state.closed {
                state.closed = true;
                Self::clear(&mut state);
                if !matches!(state.status, CacheWarmingStatus::Inactive { .. }) {
                    state.status = CacheWarmingStatus::Inactive {
                        reason: "agent run settled".into(),
                        decision: None,
                    };
                }
                if let Some(waker) = state.driver.take() {
                    waker.wake();
                }
            }
        }
        std::future::poll_fn(|context| {
            let mut state = self.lock();
            if state.in_flight.is_none() {
                Poll::Ready(())
            } else {
                state.idle.push(context.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    /// Take completed maintenance records for durable attribution.
    pub(crate) fn take_records(&self) -> Vec<CacheMaintenanceRecord> {
        std::mem::take(&mut self.lock().records)
    }

    fn clear(state: &mut WarmerState) {
        state.generation = state.generation.wrapping_add(1);
        state.plan = None;
        if let Some(token) = &state.in_flight {
            token.cancel();
        }
        if let Some(waker) = state.driver.take() {
            waker.wake();
        }
    }

    fn stop_locked(state: &mut WarmerState, reason: &str, decision: Option<WarmingDecision>) {
        Self::clear(state);
        state.status = CacheWarmingStatus::Inactive {
            reason: reason.into(),
            decision,
        };
    }

    /// Arm the next refresh from `now`. Refreshes never move `started_at`, so
    /// they cannot extend their own lifetime.
    fn schedule_locked(state: &mut WarmerState, plan: Arc<WarmPlan>, now: Duration) {
        let next_warm_at = now + plan.delay;
        // A timer can run late after suspension or executor blockage. Keep
        // half of the planned pre-expiry margin for that lateness and for
        // dispatch; a later refresh would likely be a full-price write.
        let refresh_deadline_at = next_warm_at + (plan.ttl - plan.delay) / 2;
        let horizon = plan.started_at + MAX_WARMING_AGE;
        if next_warm_at > horizon || now >= horizon {
            Self::stop_locked(state, "one-hour safety limit reached", None);
            return;
        }
        state.status = CacheWarmingStatus::Scheduled { next_warm_at };
        state.plan = Some((
            plan,
            Schedule {
                next_warm_at,
                refresh_deadline_at,
            },
        ));
        if let Some(waker) = state.driver.take() {
            waker.wake();
        }
    }

    /// Drive scheduled refreshes until the warmer settles. Polled beside the
    /// run that owns it; never spawned.
    pub(crate) async fn drive(&self) {
        loop {
            let next = std::future::poll_fn(|context| {
                let mut state = self.lock();
                if state.closed {
                    return Poll::Ready(None);
                }
                match &state.plan {
                    Some((plan, schedule)) if state.in_flight.is_none() => {
                        Poll::Ready(Some((state.generation, Arc::clone(plan), *schedule)))
                    }
                    _ => {
                        state.driver = Some(context.waker().clone());
                        Poll::Pending
                    }
                }
            })
            .await;
            let Some((generation, plan, schedule)) = next else {
                return;
            };
            let superseded = first_of(
                self.clock.sleep_until(schedule.next_warm_at),
                self.generation_changed(generation),
            )
            .await;
            if superseded == Winner::Second {
                continue;
            }
            self.refresh(generation, plan, schedule).await;
        }
    }

    fn generation_changed(&self, generation: u64) -> impl Future<Output = ()> + '_ {
        std::future::poll_fn(move |context| {
            let mut state = self.lock();
            if state.generation != generation || state.closed {
                Poll::Ready(())
            } else {
                state.driver = Some(context.waker().clone());
                Poll::Pending
            }
        })
    }

    async fn refresh(&self, generation: u64, plan: Arc<WarmPlan>, schedule: Schedule) {
        let token = {
            let mut state = self.lock();
            if state.generation != generation || state.closed {
                return;
            }
            if let Some(reason) = (plan.current.check)() {
                Self::stop_locked(&mut state, reason, None);
                return;
            }
            let now = self.clock.now();
            if now < schedule.next_warm_at {
                // An early wake; wait again for the same plan.
                if let Some(waker) = state.driver.take() {
                    waker.wake();
                }
                return;
            }
            if now > schedule.refresh_deadline_at {
                Self::stop_locked(&mut state, "cache refresh deadline missed", None);
                return;
            }
            let decision = evaluate(state.prompt_tokens, plan.pricing.as_ref());
            if decision.action == WarmingAction::Stop {
                let reason = if decision.economics_available {
                    "expected savings below threshold"
                } else {
                    "cache economics unavailable"
                };
                Self::stop_locked(&mut state, reason, Some(decision));
                return;
            }
            let token = CancellationToken::new();
            state.in_flight = Some(token.clone());
            state.status = CacheWarmingStatus::Refreshing { decision };
            token
        };
        let (outcome, usage) = send_maintenance(&plan, token).await;
        let mut state = self.lock();
        state.in_flight = None;
        for waker in state.idle.drain(..) {
            waker.wake();
        }
        let completed = outcome == CacheMaintenanceOutcome::Completed;
        if completed || usage.is_reported() {
            let estimated_cost = plan
                .pricing
                .as_ref()
                .and_then(Prices::parse)
                .filter(|_| usage.is_reported())
                .map(|prices| format!("{:.6}", prices.cost(&usage)));
            state.records.push(CacheMaintenanceRecord {
                model: plan.request.model.clone(),
                selected_model: plan.request.selected_model.clone(),
                outcome,
                usage,
                estimated_cost,
            });
        }
        if state.generation == generation && !state.closed {
            let now = self.clock.now();
            Self::schedule_locked(&mut state, plan, now);
        }
    }
}

/// Send one maintenance request and drain it. Text, thinking, and tool calls
/// are discarded; tool calls are never executed.
async fn send_maintenance(
    plan: &WarmPlan,
    token: CancellationToken,
) -> (CacheMaintenanceOutcome, Usage) {
    let mut usage = Usage::default();
    let mut stream: Box<dyn ModelEventStream> =
        match plan.provider.stream(plan.request.clone(), token.clone()).await {
            Ok(stream) => stream,
            Err(error) => {
                return (
                    CacheMaintenanceOutcome::Failed {
                        message: bounded(&error.to_string()),
                    },
                    usage,
                );
            }
        };
    let mut failure = None;
    loop {
        match stream.next_event(token.clone()).await {
            Ok(Some(ModelStreamEvent::Usage(update))) => usage.merge(update),
            Ok(Some(ModelStreamEvent::Error { message }))
            | Ok(Some(ModelStreamEvent::ContextOverflow { message })) => {
                failure = Some(message);
            }
            Ok(Some(ModelStreamEvent::End(reason))) => {
                let outcome = match (reason, failure) {
                    (StopReason::Cancelled, _) => CacheMaintenanceOutcome::Cancelled,
                    (_, Some(message)) => CacheMaintenanceOutcome::Failed {
                        message: bounded(&message),
                    },
                    (StopReason::Error, None) => CacheMaintenanceOutcome::Failed {
                        message: "provider error".into(),
                    },
                    _ => CacheMaintenanceOutcome::Completed,
                };
                return (outcome, usage);
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                let outcome = if token.is_cancelled() {
                    CacheMaintenanceOutcome::Cancelled
                } else {
                    CacheMaintenanceOutcome::Failed {
                        message: bounded(
                            failure
                                .as_deref()
                                .unwrap_or("maintenance stream ended without a terminal event"),
                        ),
                    }
                };
                return (outcome, usage);
            }
            Err(error) => {
                let outcome = if token.is_cancelled() {
                    CacheMaintenanceOutcome::Cancelled
                } else {
                    CacheMaintenanceOutcome::Failed {
                        message: bounded(&error.to_string()),
                    }
                };
                return (outcome, usage);
            }
        }
    }
}

fn bounded(message: &str) -> String {
    const LIMIT: usize = 512;
    if message.len() <= LIMIT {
        return message.to_owned();
    }
    let mut end = LIMIT;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Winner {
    First,
    Second,
}

/// Resolve when either future resolves, preferring the first.
async fn first_of(
    first: impl Future<Output = ()>,
    second: impl Future<Output = ()>,
) -> Winner {
    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    std::future::poll_fn(move |context: &mut Context<'_>| {
        if first.as_mut().poll(context).is_ready() {
            return Poll::Ready(Winner::First);
        }
        if second.as_mut().poll(context).is_ready() {
            return Poll::Ready(Winner::Second);
        }
        Poll::Pending
    })
    .await
}

/// Poll `main` to completion while also polling `maintenance`. Maintenance
/// never outlives `main`: it is dropped when `main` resolves.
pub(crate) async fn with_maintenance<T>(
    main: impl Future<Output = T>,
    maintenance: impl Future<Output = ()>,
) -> T {
    let mut main = std::pin::pin!(main);
    let mut maintenance = std::pin::pin!(maintenance);
    let mut maintenance_done = false;
    std::future::poll_fn(move |context: &mut Context<'_>| {
        if let Poll::Ready(value) = main.as_mut().poll(context) {
            return Poll::Ready(value);
        }
        if !maintenance_done && maintenance.as_mut().poll(context).is_ready() {
            maintenance_done = true;
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests;
