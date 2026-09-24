//! Volatile state owned by one durable session lane.

use super::ExtensionContinuation;
use crate::agent::Agent;
use crate::runtime::RuntimeServices;
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tea_core::state::ThinkingLevel;
use tea_session::LaneId;

/// Process-local notification that one claimed lane drive has released.
///
/// This only joins work owned by the current process. Durable recovery remains
/// the reducer's responsibility, so a release after an effect-gate failure
/// intentionally wakes close waiters even though the durable lane may still
/// require explicit recovery.
#[derive(Default)]
pub(crate) struct LaneDriveNotifier {
    state: Mutex<LaneDriveState>,
}

#[derive(Default)]
struct LaneDriveState {
    generation: u64,
    next_waiter_id: u64,
    waiters: BTreeMap<u64, Waker>,
}

impl LaneDriveNotifier {
    pub(crate) fn wait_after_current(self: &Arc<Self>) -> LaneDriveWait {
        let (generation, waiter_id) = self
            .state
            .lock()
            .map(|mut state| {
                let waiter_id = state.next_waiter_id;
                state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
                (state.generation, waiter_id)
            })
            .unwrap_or((u64::MAX, u64::MAX));
        LaneDriveWait {
            notifier: Arc::clone(self),
            generation,
            waiter_id,
        }
    }

    pub(crate) fn notify_release(&self) {
        let waiters = match self.state.lock() {
            Ok(mut state) => {
                state.generation = state.generation.wrapping_add(1);
                std::mem::take(&mut state.waiters)
            }
            Err(_) => return,
        };
        for waiter in waiters.into_values() {
            // Executor-owned wake code is observational. It cannot prevent a
            // completed drive from releasing its lane or a close from joining.
            let _ = catch_unwind(AssertUnwindSafe(|| waiter.wake()));
        }
    }
}

/// A drop-safe waiter for the next local drive release.
pub(crate) struct LaneDriveWait {
    notifier: Arc<LaneDriveNotifier>,
    generation: u64,
    waiter_id: u64,
}

impl Future for LaneDriveWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let Ok(mut state) = this.notifier.state.lock() else {
            return Poll::Ready(());
        };
        if state.generation != this.generation {
            return Poll::Ready(());
        }
        match state.waiters.get_mut(&this.waiter_id) {
            Some(waiter) if waiter.will_wake(context.waker()) => {}
            Some(waiter) => *waiter = context.waker().clone(),
            None => {
                state
                    .waiters
                    .insert(this.waiter_id, context.waker().clone());
            }
        }
        Poll::Pending
    }
}

impl Drop for LaneDriveWait {
    fn drop(&mut self) {
        let Ok(mut state) = self.notifier.state.lock() else {
            return;
        };
        if state.generation == self.generation {
            state.waiters.remove(&self.waiter_id);
        }
    }
}

/// Process-local execution state for exactly one durable lane.
///
/// All durable state remains in the session reducer. This object deliberately
/// keeps only executable authority and live observations that must never be
/// shared with another lane.
pub(crate) struct LaneRuntime {
    pub(crate) lane_id: LaneId,
    pub(crate) active: AtomicBool,
    /// Wakes close/cancel joiners only after the locally owned drive claim
    /// releases. It never grants recovery or automatic goal authority.
    pub(crate) drive_notifier: Arc<LaneDriveNotifier>,
    /// Root-only sticky cancellation requested after durable acceptance but
    /// potentially before a core epoch installs its live agent. This remains
    /// process-local; the operation WAL still owns durable recovery.
    pub(crate) abort_requested: AtomicBool,
    pub(crate) active_agent: Mutex<Option<Agent>>,
    /// A command or idle callback may request one later ordinary operation.
    /// This is intentionally process-local: reopening restores goal state but
    /// never acquires permission to run it without a fresh host decision.
    pub(crate) pending_extension_continuation: Mutex<Option<ExtensionContinuation>>,
    pub(crate) thinking_level: Mutex<ThinkingLevel>,
    pub(crate) runtime_services: RuntimeServices,
    pub(crate) prompt_layout_ledger: Arc<crate::measurement::PromptLayoutLedger>,
}

impl LaneRuntime {
    pub(crate) fn new(lane_id: LaneId, runtime_services: RuntimeServices) -> Self {
        let prompt_layout_ledger = Arc::new(
            crate::measurement::PromptLayoutLedger::new(runtime_services.prompt_layout_scope())
                .policy(runtime_services.prompt_layout_policy_value()),
        );
        let thinking_level = runtime_services.thinking_level_value();
        Self {
            lane_id,
            active: AtomicBool::new(false),
            drive_notifier: Arc::new(LaneDriveNotifier::default()),
            abort_requested: AtomicBool::new(false),
            active_agent: Mutex::new(None),
            pending_extension_continuation: Mutex::new(None),
            thinking_level: Mutex::new(thinking_level),
            runtime_services,
            prompt_layout_ledger,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measurement::PromptContinuity;
    use crate::scheduler::{CancellationToken, ModelFuture, ModelProvider, ModelRequest};
    use crate::state::ModelDescriptor;
    use crate::tool::ToolRegistry;

    struct UnusedProvider;

    impl ModelProvider for UnusedProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            panic!("lane-ledger test never dispatches a provider request")
        }
    }

    fn request(context: &str) -> ModelRequest {
        ModelRequest {
            system_prompt: "stable child prompt".into(),
            context: context.into(),
            tools: Vec::new(),
            model: Some(ModelDescriptor {
                provider: "fixture".into(),
                model: "child".into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::Off,
            session_id: None,
        }
    }

    #[test]
    fn every_lane_owns_a_distinct_prompt_layout_ledger() {
        let services = RuntimeServices::new(Arc::new(UnusedProvider), ToolRegistry::default());
        let root = LaneRuntime::new(LaneId::main(), services.clone());
        let first_child = LaneRuntime::new(
            LaneId::new("agent-first").expect("valid child lane ID"),
            services.clone(),
        );
        let second_child = LaneRuntime::new(
            LaneId::new("agent-second").expect("valid child lane ID"),
            services,
        );

        assert!(
            !Arc::ptr_eq(
                &root.prompt_layout_ledger,
                &first_child.prompt_layout_ledger
            ) && !Arc::ptr_eq(
                &first_child.prompt_layout_ledger,
                &second_child.prompt_layout_ledger
            )
        );
        assert_eq!(
            root.prompt_layout_ledger
                .observe(&request("root"))
                .continuity,
            PromptContinuity::FirstRequest
        );
        assert_eq!(
            first_child
                .prompt_layout_ledger
                .observe(&request("first child"))
                .continuity,
            PromptContinuity::FirstRequest
        );
        assert_eq!(
            second_child
                .prompt_layout_ledger
                .observe(&request("second child"))
                .continuity,
            PromptContinuity::FirstRequest,
            "a second child never inherits another lane's predecessor"
        );
    }
}
