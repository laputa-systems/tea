//! Local, replay-safe resources used by the optional subagent host.
//!
//! This module deliberately contains no tool registration, provider wiring, or
//! async task ownership. Those layers consume the explicit workspace types in
//! a later phase; keeping Git mutation here makes the parent-worktree boundary
//! independently testable.

mod git;
mod host;
mod tasks;
mod workspace;

pub(crate) use host::TuiSubagentHost;
pub(crate) use tasks::SmolTaskRuntime;
pub(crate) use workspace::{
    GitWorkspaceDelta, GitWorkspaceEngine, WorkspaceApplyEvidence, WorkspaceApplyOutcome,
    WorkspaceApplyRequest, WorkspaceError, WorkspaceFinalization, WorkspaceLease,
    WorkspaceLeaseRequest, WorkspacePathState,
};
