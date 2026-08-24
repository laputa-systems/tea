//! Process-local coordination for durable root-to-child spawn transactions.
//!
//! The session graph is authoritative. This type retains only temporary
//! reservations and task handles needed while a host prepares a workspace or
//! drives an already accepted child operation. A new coordinator can rebuild
//! safely from the session prefix and never invents a child from in-memory
//! state alone.

use super::{
    ApplyAgentChangesResult, InterruptAgentResult, SpawnAgentRequest, SpawnedAgentHandle, SubagentServices,
    SubagentStatus, TaskHandle, WaitAgentsRequest, WaitAgentsResult, WorkspaceLease,
};
use crate::harness::HarnessError;
use crate::runtime::supervisor::SessionSupervisor;
use crate::scheduler::CancellationToken;
use crate::tool::{ToolCall, ToolContext};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Poll, Waker};
use std::time::Duration;
use tea_session::{AgentId, OperationId, SessionWriter, WorkspaceDeltaId};

/// Volatile coordination state that is never a durable source of truth.
#[derive(Default)]
struct CoordinatorState {
    pending_agents: BTreeSet<AgentId>,
    pending_total_by_root_operation: BTreeMap<OperationId, u32>,
    handles: BTreeMap<AgentId, Arc<dyn TaskHandle>>,
    workspaces: BTreeMap<AgentId, WorkspaceLease>,
    completed_before_install: BTreeSet<AgentId>,
    exposable_agents: BTreeSet<AgentId>,
}

/// A process-local wake source for durable child activity.
///
/// The durable graph remains authoritative: a generation is only advanced
/// after a terminal fact and operational cleanup are complete. Waiters retain
/// their executor wakers until that one next activity event, avoiding polling
/// and avoiding any dependency on a particular async runtime.
#[derive(Default)]
struct ActivityNotifier {
    state: Mutex<ActivityState>,
}

#[derive(Default)]
struct ActivityState {
    generation: u64,
    next_waiter_id: u64,
    waiters: Vec<(u64, Waker)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivityWake {
    Activity,
    Cancelled,
    TimedOut,
}

impl ActivityNotifier {
    fn generation(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.generation)
            .unwrap_or(u64::MAX)
    }

    fn notify(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.generation = state.generation.wrapping_add(1);
        let waiters = std::mem::take(&mut state.waiters);
        drop(state);
        for (_, waiter) in waiters {
            waiter.wake();
        }
    }

    fn wait_after(&self, observed: u64) -> ActivityWait<'_> {
        let waiter_id = self
            .state
            .lock()
            .map(|mut state| {
                let waiter_id = state.next_waiter_id;
                state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
                waiter_id
            })
            .unwrap_or(u64::MAX);
        ActivityWait {
            notifier: self,
            observed,
            waiter_id,
        }
    }

    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.state.lock().map(|state| state.waiters.len()).unwrap_or_default()
    }
}

/// Drop-safe registration for one activity generation.
///
/// A completed wait may be immediately dropped by tool cancellation or a
/// timeout. Removing its specific token here prevents stale executor wakers
/// from accumulating while the child set remains quiet.
struct ActivityWait<'a> {
    notifier: &'a ActivityNotifier,
    observed: u64,
    waiter_id: u64,
}

impl Future for ActivityWait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let Ok(mut state) = this.notifier.state.lock() else {
            return Poll::Ready(());
        };
        if state.generation != this.observed {
            return Poll::Ready(());
        }
        if let Some((_, waiter)) = state
            .waiters
            .iter_mut()
            .find(|(waiter_id, _)| *waiter_id == this.waiter_id)
        {
            if !waiter.will_wake(context.waker()) {
                *waiter = context.waker().clone();
            }
        } else {
            state.waiters.push((this.waiter_id, context.waker().clone()));
        }
        Poll::Pending
    }
}

impl Drop for ActivityWait<'_> {
    fn drop(&mut self) {
        let Ok(mut state) = self.notifier.state.lock() else {
            return;
        };
        state
            .waiters
            .retain(|(waiter_id, _)| *waiter_id != self.waiter_id);
    }
}

/// Root-only coordinator assembled only when explicit child services exist.
pub(crate) struct SubagentCoordinator<S> {
    supervisor: Weak<SessionSupervisor<S>>,
    services: SubagentServices,
    state: Mutex<CoordinatorState>,
    activity: ActivityNotifier,
}

impl<S> SubagentCoordinator<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Bind explicit services to one supervisor without creating a global
    /// provider, workspace, executor, or fallback implementation.
    pub(crate) fn new(
        supervisor: Weak<SessionSupervisor<S>>,
        services: SubagentServices,
    ) -> Self {
        Self {
            supervisor,
            services,
            state: Mutex::new(CoordinatorState::default()),
            activity: ActivityNotifier::default(),
        }
    }

    /// Borrow the explicitly supplied host services.
    pub(crate) fn services(&self) -> &SubagentServices {
        &self.services
    }

    /// Accept one root spawn request after the durable parent tool-start
    /// record has committed. The supervisor derives all durable identity and
    /// performs the ordered session transaction.
    pub(crate) async fn spawn(
        self: &Arc<Self>,
        call: ToolCall,
        context: ToolContext,
        request: SpawnAgentRequest,
    ) -> Result<SpawnedAgentHandle, HarnessError> {
        let supervisor = self.supervisor.upgrade().ok_or_else(|| {
            HarnessError::invalid_state("subagent coordinator outlived its supervisor")
        })?;
        supervisor
            .accept_subagent_spawn(self, call, context.provenance, request)
            .await
    }

    /// Observe selected durable child results through the supervisor-owned
    /// notifier. The caller's cancellation token is part of `ToolContext`,
    /// never inferred from a host runtime.
    pub(crate) async fn wait(
        self: &Arc<Self>,
        context: ToolContext,
        request: WaitAgentsRequest,
    ) -> Result<WaitAgentsResult, HarnessError> {
        let supervisor = self.supervisor.upgrade().ok_or_else(|| {
            HarnessError::invalid_state("subagent coordinator outlived its supervisor")
        })?;
        supervisor
            .wait_subagents(self, &context.provenance, context.cancellation, request)
            .await
    }

    /// Return sorted, parent-owned child status without changing durable
    /// parent context.
    pub(crate) fn list(&self, context: &ToolContext) -> Result<Vec<SubagentStatus>, HarnessError> {
        let supervisor = self.supervisor.upgrade().ok_or_else(|| {
            HarnessError::invalid_state("subagent coordinator outlived its supervisor")
        })?;
        supervisor.list_subagents(&context.provenance)
    }

    /// Cancel, join, and durably settle one parent-owned child.
    pub(crate) async fn interrupt(
        self: &Arc<Self>,
        context: &ToolContext,
        target: &str,
    ) -> Result<InterruptAgentResult, HarnessError> {
        let supervisor = self.supervisor.upgrade().ok_or_else(|| {
            HarnessError::invalid_state("subagent coordinator outlived its supervisor")
        })?;
        supervisor.interrupt_subagent(self, &context.provenance, target).await
    }

    /// Apply one cleanup-ready child delta through the host's explicit root
    /// workspace authority. The supervisor validates provenance and appends
    /// the durable proof only after the host reports a committed result.
    pub(crate) async fn apply(
        self: &Arc<Self>,
        call: ToolCall,
        context: ToolContext,
        delta_id: WorkspaceDeltaId,
    ) -> Result<ApplyAgentChangesResult, HarnessError> {
        let supervisor = self.supervisor.upgrade().ok_or_else(|| {
            HarnessError::invalid_state("subagent coordinator outlived its supervisor")
        })?;
        supervisor
            .apply_subagent_changes(self, &context.provenance, &call, delta_id)
            .await
    }

    /// Reserve a prospective agent while its host workspace preparation is in
    /// flight. The caller supplies durable counts so a stale process-local map
    /// cannot increase capacity.
    pub(crate) fn reserve(
        &self,
        agent_id: AgentId,
        parent_operation_id: &OperationId,
        durable_active: u32,
        durable_total: u32,
    ) -> Result<(), HarnessError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| HarnessError::invalid_state("subagent coordinator mutex is poisoned"))?;
        if state.pending_agents.contains(&agent_id) {
            return Err(HarnessError::invalid_state(
                "the same durable spawn intent is already being prepared",
            ));
        }
        let pending_total = state
            .pending_total_by_root_operation
            .get(parent_operation_id)
            .copied()
            .unwrap_or_default();
        let pending_active = state.pending_agents.len() as u32;
        if durable_active.saturating_add(pending_active) >= self.services.policy.max_concurrent.get() {
            return Err(HarnessError::invalid_state(
                "subagent concurrent-operation limit is exhausted",
            ));
        }
        if durable_total.saturating_add(pending_total)
            >= self.services.policy.max_total_per_operation.get()
        {
            return Err(HarnessError::invalid_state(
                "subagent total-spawn limit is exhausted for this root operation",
            ));
        }
        state.pending_agents.insert(agent_id);
        *state
            .pending_total_by_root_operation
            .entry(parent_operation_id.clone())
            .or_default() += 1;
        Ok(())
    }

    /// Release a failed pre-acceptance reservation.
    pub(crate) fn release_reservation(&self, agent_id: &AgentId, parent_operation_id: &OperationId) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if !state.pending_agents.remove(agent_id) {
            return;
        }
        let remove = match state.pending_total_by_root_operation.get_mut(parent_operation_id) {
            Some(count) => {
                *count = count.saturating_sub(1);
                *count == 0
            }
            None => false,
        };
        if remove {
            state.pending_total_by_root_operation.remove(parent_operation_id);
        }
    }

    /// Promote a provisional reservation into a durable child task.
    ///
    /// Once the lane, graph fact, and operation are committed, the next
    /// capacity calculation sees this agent in the durable graph.  Removing
    /// both volatile counters here avoids counting the same child twice.
    pub(crate) fn install_handle(
        &self,
        agent_id: AgentId,
        parent_operation_id: &OperationId,
        workspace: WorkspaceLease,
        handle: Arc<dyn TaskHandle>,
    ) {
        if let Ok(mut state) = self.state.lock() {
            let was_pending = state.pending_agents.remove(&agent_id);
            if was_pending {
                let remove = match state
                    .pending_total_by_root_operation
                    .get_mut(parent_operation_id)
                {
                    Some(count) => {
                        *count = count.saturating_sub(1);
                        *count == 0
                    }
                    None => false,
                };
                if remove {
                    state
                        .pending_total_by_root_operation
                        .remove(parent_operation_id);
                }
            }
            if state.completed_before_install.remove(&agent_id) {
                return;
            }
            state.workspaces.insert(agent_id.clone(), workspace);
            state.handles.insert(agent_id, handle);
        }
    }

    /// Return whether this process already owns the live task for an accepted
    /// durable child.  Durable replay must not reserve capacity a second time.
    pub(crate) fn has_handle(&self, agent_id: &AgentId) -> bool {
        self.state
            .lock()
            .map(|state| state.handles.contains_key(agent_id))
            .unwrap_or(false)
    }

    /// Snapshot a live handle without making the volatile map authoritative.
    pub(crate) fn handle(&self, agent_id: &AgentId) -> Option<Arc<dyn TaskHandle>> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.handles.get(agent_id).cloned())
    }

    /// Return the host lease retained while this process owns the task.
    pub(crate) fn workspace(&self, agent_id: &AgentId) -> Option<WorkspaceLease> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.workspaces.get(agent_id).cloned())
    }

    /// Mark the owned future complete without dropping its final handle from
    /// inside that future's own poll. An executor may synchronously cancel or
    /// join in `TaskHandle::drop`; removal is therefore reserved for an
    /// external join/reap point.
    pub(crate) fn task_completed(&self, agent_id: &AgentId) {
        if let Ok(mut state) = self.state.lock() {
            if !state.handles.contains_key(agent_id) {
                state.completed_before_install.insert(agent_id.clone());
            }
        }
    }

    /// Record a task that stopped before cleanup without self-dropping the
    /// executor handle. Root or interrupt settlement retains the workspace
    /// lease and later reaps from outside the child future.
    pub(crate) fn task_stopped_before_cleanup(&self, agent_id: &AgentId) {
        self.task_completed(agent_id);
    }

    /// Remove a completed task handle only after an external caller has
    /// awaited `join`. This is the sole handle-drop boundary in core.
    pub(crate) fn reap_task(&self, agent_id: &AgentId) {
        if let Ok(mut state) = self.state.lock() {
            state.handles.remove(agent_id);
            state.workspaces.remove(agent_id);
        }
    }

    /// Return the current durable-activity generation before reading a graph
    /// snapshot. Callers then register against this exact generation, closing
    /// the append/read/register race without a poll loop.
    pub(crate) fn activity_generation(&self) -> u64 {
        self.activity.generation()
    }

    /// Mark a durable terminal child visible to parent waiters only after its
    /// operational cleanup has completed, then wake event-driven waiters.
    /// A terminal fact by itself is intentionally not a parent-facing result.
    pub(crate) fn mark_exposable_and_notify(&self, agent_id: AgentId) {
        if let Ok(mut state) = self.state.lock() {
            state.exposable_agents.insert(agent_id);
        }
        self.activity.notify();
    }

    /// Return whether this process has completed the cleanup boundary for a
    /// terminal child. Reopen recovery must establish this again before a
    /// resumed root may wait on historical terminal facts.
    pub(crate) fn is_exposable(&self, agent_id: &AgentId) -> bool {
        self.state
            .lock()
            .map(|state| state.exposable_agents.contains(agent_id))
            .unwrap_or(false)
    }

    /// Await one terminal/interruption activity, root cancellation, or the
    /// caller-owned timeout. `timeout` is pinned by the outer wait operation,
    /// so repeated unrelated child activity never extends the deadline.
    pub(crate) async fn wait_for_activity(
        &self,
        observed: u64,
        cancellation: CancellationToken,
        timeout: &mut Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> ActivityWake {
        let mut cancelled = Box::pin(cancellation.cancelled());
        let mut activity = self.activity.wait_after(observed);
        std::future::poll_fn(|context| {
            if cancellation.is_cancelled() {
                return Poll::Ready(ActivityWake::Cancelled);
            }
            if Pin::new(&mut activity).poll(context).is_ready() {
                return Poll::Ready(ActivityWake::Activity);
            }
            if cancelled.as_mut().poll(context).is_ready() {
                return Poll::Ready(ActivityWake::Cancelled);
            }
            if timeout.as_mut().poll(context).is_ready() {
                return Poll::Ready(ActivityWake::TimedOut);
            }
            Poll::Pending
        })
        .await
    }

    /// Construct the host timer through the explicit task-runtime port.
    pub(crate) fn timeout(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.services.tasks.sleep(duration)
    }

    #[cfg(test)]
    pub(crate) fn activity_waiter_count_for_test(&self) -> usize {
        self.activity.waiter_count()
    }
}
