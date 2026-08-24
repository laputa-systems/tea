//! Executor- and workspace-neutral host ports for child lanes.

use super::{
    ApplyWorkspaceDeltaRequest, FinalizeSubagentRequest, PreparedSubagent, ReopenSubagentRequest,
    SubagentHostError, SubagentTaskError, WorkspaceApplyOutcome, WorkspaceFinalization,
    WorkspaceLease,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Boxed host future without imposing an async runtime on `tea-core`.
pub type SubagentHostFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SubagentHostError>> + Send + 'a>>;

/// Host authority for isolated child workspaces and patch application.
pub trait SubagentHost: Send + Sync {
    /// Prepare or reuse one child workspace before its lane is made runnable.
    fn prepare<'a>(
        &'a self,
        request: super::PrepareSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent>;

    /// Reattach host authority during durable recovery.
    fn reopen<'a>(
        &'a self,
        request: ReopenSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent>;

    /// Freeze an isolated child workspace into a durable delta.
    fn finalize<'a>(
        &'a self,
        request: FinalizeSubagentRequest,
    ) -> SubagentHostFuture<'a, WorkspaceFinalization>;

    /// Preflight and apply a child delta without modifying the user index.
    fn apply<'a>(
        &'a self,
        request: ApplyWorkspaceDeltaRequest,
    ) -> SubagentHostFuture<'a, WorkspaceApplyOutcome>;

    /// Remove only operational workspace state after its durable outcome exists.
    fn cleanup<'a>(&'a self, lease: WorkspaceLease) -> SubagentHostFuture<'a, ()>;
}

/// A supervisor-owned task handle. Both operations must be idempotent.
pub trait TaskHandle: Send + Sync {
    /// Request cancellation of the task's owned child operation.
    fn cancel(&self);

    /// Observe settlement exactly once from the host runtime's perspective.
    fn join<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Executor-neutral structured-concurrency host port.
pub trait TaskRuntime: Send + Sync {
    /// Accept supervisor ownership of a child task. Implementations must not detach it.
    fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<Arc<dyn TaskHandle>, SubagentTaskError>;

    /// Provide cancellation-aware time passage without selecting a runtime in core.
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}
