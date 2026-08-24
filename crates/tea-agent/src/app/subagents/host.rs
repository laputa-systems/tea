//! Concrete terminal authority for isolated child lanes.
//!
//! The durable supervisor owns the child graph and immutable harness catalog.
//! This module owns only the process-local pieces which cannot cross that
//! boundary: a Git lease, model adapter, concrete coding tools, and the
//! operational maps required to finalize or clean up a live lease.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tea_core::coding::TeaCodingToolsV2;
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::runtime::{
    ApplyWorkspaceDeltaRequest, FinalizeSubagentRequest, HarnessIdentity, PreparedSubagent,
    ReopenSubagentRequest, RuntimeServices, SubagentHost, SubagentHostError, SubagentHostFuture,
    SubagentModel, WorkspaceApplyOutcome, WorkspaceDelta, WorkspaceFinalization,
    WorkspaceLease as CoreWorkspaceLease,
};
use tea_core::state::{ModelDescriptor, ThinkingLevel};
use tea_session::{
    AgentId, ArtifactId, ArtifactStore, SessionId, WorkspaceDeltaId, WorkspaceLeaseId,
};

use super::{
    GitWorkspaceDelta, GitWorkspaceEngine, WorkspaceApplyOutcome as GitWorkspaceApplyOutcome,
    WorkspaceApplyRequest, WorkspaceFinalization as GitWorkspaceFinalization,
    WorkspaceLease as GitWorkspaceLease, WorkspaceLeaseRequest,
};
use crate::app::host::host_configuration;
use crate::app::nonblocking_operations::NonblockingCodingOperations;
use crate::app::picker::automatic_compaction_policy;
use crate::app::provider_factory::ProviderFactory;

/// Terminal-owned child host. Its maps are disposable operational state; the
/// session graph and immutable harness catalog remain authoritative on reopen.
pub(crate) struct TuiSubagentHost {
    workspace: PathBuf,
    session_directory: PathBuf,
    session_id: SessionId,
    logical_workspace_label: String,
    factory: Arc<ProviderFactory>,
    artifacts: Arc<dyn ArtifactStore>,
    child_harnesses: BTreeMap<ChildHarnessKey, HarnessIdentity>,
    engine: GitWorkspaceEngine,
    leases: Mutex<BTreeMap<WorkspaceLeaseId, GitWorkspaceLease>>,
    deltas: Mutex<BTreeMap<WorkspaceDeltaId, GitWorkspaceDelta>>,
}

impl std::fmt::Debug for TuiSubagentHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TuiSubagentHost")
            .field("workspace", &self.workspace)
            .field("session_directory", &self.session_directory)
            .field("session_id", &self.session_id)
            .field("logical_workspace_label", &self.logical_workspace_label)
            .field("child_harness_count", &self.child_harnesses.len())
            .finish_non_exhaustive()
    }
}

/// Complete descriptor identity for one pre-seeded child harness.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ChildHarnessKey {
    provider: String,
    model: String,
    revision: Option<String>,
}

impl From<&ModelDescriptor> for ChildHarnessKey {
    fn from(descriptor: &ModelDescriptor) -> Self {
        Self {
            provider: descriptor.provider.clone(),
            model: descriptor.model.clone(),
            revision: descriptor.revision.clone(),
        }
    }
}

impl TuiSubagentHost {
    /// Bind terminal-local authority to child harness identities already staged
    /// in the session's immutable resolver catalog.
    pub(crate) fn new(
        workspace: PathBuf,
        session_directory: PathBuf,
        session_id: SessionId,
        logical_workspace_label: String,
        factory: Arc<ProviderFactory>,
        artifacts: Arc<dyn ArtifactStore>,
        child_harnesses: impl IntoIterator<Item = (ModelDescriptor, HarnessIdentity)>,
    ) -> Self {
        Self {
            workspace,
            session_directory,
            session_id,
            logical_workspace_label,
            factory,
            artifacts,
            child_harnesses: child_harnesses
                .into_iter()
                .map(|(descriptor, identity)| (ChildHarnessKey::from(&descriptor), identity))
                .collect(),
            engine: GitWorkspaceEngine::default(),
            leases: Mutex::new(BTreeMap::new()),
            deltas: Mutex::new(BTreeMap::new()),
        }
    }

    async fn prepare_workspace(
        &self,
        agent_id: AgentId,
        workspace_lease_id: WorkspaceLeaseId,
        reattach: bool,
    ) -> Result<GitWorkspaceLease, SubagentHostError> {
        if workspace_lease_id != WorkspaceLeaseId::derive(&agent_id) {
            return Err(host_error("subagent workspace lease does not match its child identity"));
        }
        let request = WorkspaceLeaseRequest {
                repository: self.workspace.clone(),
                session_directory: self.session_directory.clone(),
                session_id: self.session_id.clone(),
                agent_id,
                workspace_lease_id,
                logical_workspace_label: self.logical_workspace_label.clone(),
            };
        let engine = self.engine;
        // Git may block on repository locks and must never run on the Smol
        // executor polling path. The request remains fully owned by this host
        // boundary and preserves its typed workspace error across offload.
        let lease = smol::unblock(move || {
            if reattach {
                engine.reopen(request)
            } else {
                engine.prepare(request)
            }
        })
        .await
        .map_err(workspace_error)?;
        self.leases
            .lock()
            .map_err(|_| host_error("subagent workspace lease map is poisoned"))?
            .insert(lease.workspace_lease_id().clone(), lease.clone());
        Ok(lease)
    }

    fn prepared(
        &self,
        lease: GitWorkspaceLease,
        model: SubagentModel,
        thinking: ThinkingLevel,
    ) -> Result<PreparedSubagent, SubagentHostError> {
        let harness_identity = self
            .child_harnesses
            .get(&ChildHarnessKey::from(&model.descriptor))
            .cloned()
            .ok_or_else(|| host_error("selected child model has no pre-seeded harness identity"))?;
        let configured = self
            .factory
            .configured(&model.descriptor)
            .map_err(app_error)?;
        let compactor = self.factory.compactor(&configured).map_err(app_error)?;
        let tools = TeaCodingToolsV2::with_operations(
            lease.worktree_path(),
            Arc::new(NonblockingCodingOperations),
        )
        .map_err(|error| host_error(format!("invalid isolated child workspace: {error}")))?;
        // Tool authority is the lease worktree; model-facing host context is
        // the stable original workspace label retained by the lease.
        let configuration = host_configuration(tools, lease.logical_workspace_label())
            .map_err(app_error)?;
        // The persisted model catalog carries the context capacity that
        // seeded this child's immutable runtime policy. Do not re-read a
        // changed current registry on reopen and accidentally mismatch that
        // snapshot; the factory still preserves the selected local endpoint
        // while it lazily builds the adapter itself.
        let automatic_compaction = model
            .context_window
            .map(automatic_compaction_policy)
            .unwrap_or_else(AutomaticCompactionPolicy::disabled);
        let runtime_services = RuntimeServices::from_agent_configuration(
            Arc::clone(&configured.provider),
            configuration,
        )
        .model(model.descriptor.clone())
        .thinking_level(thinking)
        .automatic_compaction(automatic_compaction)
        .compactor(compactor);
        Ok(PreparedSubagent {
            workspace: CoreWorkspaceLease {
                id: lease.workspace_lease_id().clone(),
                logical_workspace: lease.logical_workspace_label().into(),
            },
            harness_identity,
            runtime_services,
        })
    }

    fn lookup_lease(
        &self,
        workspace: &CoreWorkspaceLease,
    ) -> Result<GitWorkspaceLease, SubagentHostError> {
        let lease = self
            .leases
            .lock()
            .map_err(|_| host_error("subagent workspace lease map is poisoned"))?
            .get(&workspace.id)
            .cloned()
            .ok_or_else(|| host_error("subagent workspace lease is not attached in this process"))?;
        if lease.logical_workspace_label() != workspace.logical_workspace {
            return Err(host_error("subagent workspace lease logical label does not match"));
        }
        Ok(lease)
    }
}

impl SubagentHost for TuiSubagentHost {
    fn prepare<'a>(
        &'a self,
        request: tea_core::runtime::PrepareSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent> {
        Box::pin(async move {
            if request.session_id != self.session_id {
                return Err(host_error("subagent prepare request belongs to another session"));
            }
            let agent_id = request.agent_id;
            let workspace_lease_id = WorkspaceLeaseId::derive(&agent_id);
            let lease = self
                .prepare_workspace(agent_id, workspace_lease_id, false)
                .await?;
            self.prepared(lease, request.model, request.thinking)
        })
    }

    fn reopen<'a>(
        &'a self,
        request: ReopenSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent> {
        Box::pin(async move {
            if request.session_id != self.session_id {
                return Err(host_error("subagent reopen request belongs to another session"));
            }
            let lease = self
                .prepare_workspace(request.agent_id, request.workspace_lease_id, true)
                .await?;
            self.prepared(lease, request.model, request.thinking)
        })
    }

    fn finalize<'a>(
        &'a self,
        request: FinalizeSubagentRequest,
    ) -> SubagentHostFuture<'a, WorkspaceFinalization> {
        Box::pin(async move {
            let lease = self.lookup_lease(&request.workspace)?;
            let engine = self.engine;
            let finalization = smol::unblock(move || engine.finalize(&lease))
                .await
                .map_err(workspace_error)?;
            match finalization {
                GitWorkspaceFinalization::NoChanges { .. } => Ok(WorkspaceFinalization::NoChanges),
                GitWorkspaceFinalization::Delta(delta) => {
                    let artifacts = Arc::clone(&self.artifacts);
                    let patch_bytes = delta.patch.clone();
                    let patch = smol::unblock(move || {
                        artifacts.put(&patch_bytes, "application/vnd.tea.git-patch")
                    })
                    .await
                    .map_err(|error| host_error(format!("could not retain child patch: {error}")))?;
                    let id = WorkspaceDeltaId::derive(
                        &delta.workspace_lease_id,
                        &delta.base_commit,
                        &delta.result_commit,
                    );
                    let core_delta = WorkspaceDelta {
                        id: id.clone(),
                        agent_id: request.agent_id,
                        workspace_lease_id: delta.workspace_lease_id.clone(),
                        base_commit: delta.base_commit.clone(),
                        result_commit: delta.result_commit.clone(),
                        changed_paths: delta
                            .changed_paths
                            .iter()
                            .map(|path| path.as_str().to_owned())
                            .collect(),
                        patch_artifact: patch.artifact_id,
                    };
                    self.deltas
                        .lock()
                        .map_err(|_| host_error("subagent workspace delta map is poisoned"))?
                        .insert(id, delta);
                    Ok(WorkspaceFinalization::Delta(core_delta))
                }
            }
        })
    }

    fn apply<'a>(
        &'a self,
        request: ApplyWorkspaceDeltaRequest,
    ) -> SubagentHostFuture<'a, WorkspaceApplyOutcome> {
        Box::pin(async move {
            let artifact_id = request.delta.patch_artifact;
            let artifacts = Arc::clone(&self.artifacts);
            let patch = smol::unblock(move || artifacts.get(artifact_id))
                .await
                .map_err(|error| host_error(format!("could not load child patch artifact: {error}")))?;
            if ArtifactId::from_bytes(&patch) != request.delta.patch_artifact {
                return Err(host_error("child patch artifact does not match its durable identity"));
            }
            // Do not trust the volatile cache after reopen. Reconstruct the
            // exact Git handle from the durable delta plus content-addressed
            // patch, then let the engine authenticate hidden refs before it
            // can alter the parent worktree.
            let delta = GitWorkspaceDelta::from_durable(
                &self.session_id,
                &request.delta.agent_id,
                request.delta.workspace_lease_id.clone(),
                request.delta.id.clone(),
                request.delta.base_commit.clone(),
                request.delta.result_commit.clone(),
                request.delta.changed_paths.clone(),
                patch,
            )
            .map_err(workspace_error)?;
            {
                let mut cache = self
                    .deltas
                    .lock()
                    .map_err(|_| host_error("subagent workspace delta map is poisoned"))?;
                if let Some(cached) = cache.get(&request.delta.id) {
                    if cached != &delta {
                        return Err(host_error("cached child workspace delta disagrees with durable request"));
                    }
                } else {
                    cache.insert(request.delta.id.clone(), delta.clone());
                }
            }
            let engine = self.engine;
            let workspace = self.workspace.clone();
            let session_directory = self.session_directory.clone();
            let outcome = smol::unblock(move || {
                engine.apply(WorkspaceApplyRequest {
                    repository: &workspace,
                    session_directory: &session_directory,
                    delta: &delta,
                })
            })
            .await
            .map_err(workspace_error)?;
            match outcome {
                GitWorkspaceApplyOutcome::Applied { evidence }
                | GitWorkspaceApplyOutcome::AlreadyApplied { evidence } => {
                    Ok(WorkspaceApplyOutcome::Applied {
                        changed_paths: evidence
                            .into_iter()
                            .map(|entry| entry.path.as_str().to_owned())
                            .collect(),
                    })
                }
                GitWorkspaceApplyOutcome::Conflict { conflicts, .. } => {
                    Ok(WorkspaceApplyOutcome::Conflict {
                        conflicting_paths: conflicts
                            .into_iter()
                            .map(|path| path.as_str().to_owned())
                            .collect(),
                    })
                }
                GitWorkspaceApplyOutcome::RolledBack { .. } => {
                    Ok(WorkspaceApplyOutcome::RolledBack {
                        diagnostic: "child workspace apply was proven rolled back".into(),
                    })
                }
                GitWorkspaceApplyOutcome::Indeterminate { .. } => {
                    Ok(WorkspaceApplyOutcome::Indeterminate {
                        diagnostic: "child workspace apply outcome is not safe to retry".into(),
                    })
                }
            }
        })
    }

    fn cleanup<'a>(&'a self, lease: CoreWorkspaceLease) -> SubagentHostFuture<'a, ()> {
        Box::pin(async move {
            if lease.logical_workspace != self.logical_workspace_label {
                return Err(host_error("subagent workspace lease logical label does not match"));
            }
            let engine = self.engine;
            let workspace = self.workspace.clone();
            let session_directory = self.session_directory.clone();
            let workspace_lease_id = lease.id.clone();
            // Cleanup is valid after a restart even when no process-local
            // prepare/reopen call repopulated `leases`. Its authority is the
            // session-owned deterministic lease path, never an ambient tree.
            smol::unblock(move || {
                engine.cleanup_durable_lease(&workspace, &session_directory, &workspace_lease_id)
            })
                .await
                .map_err(workspace_error)?;
            self.leases
                .lock()
                .map_err(|_| host_error("subagent workspace lease map is poisoned"))?
                .remove(&lease.id);
            Ok(())
        })
    }
}

fn host_error(message: impl Into<String>) -> SubagentHostError {
    SubagentHostError {
        message: message.into(),
    }
}

fn workspace_error(error: impl std::fmt::Display) -> SubagentHostError {
    host_error(error.to_string())
}

fn app_error(error: impl std::fmt::Display) -> SubagentHostError {
    host_error(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestRepository {
        directory: PathBuf,
        root: PathBuf,
        session: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let directory = std::env::temp_dir().join(format!(
                "tea-agent-subagent-host-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let root = directory.join("repository");
            let session = directory.join("session");
            fs::create_dir_all(&root).expect("test repository directory creates");
            git(&root, &["init"]);
            git(&root, &["config", "user.name", "Tea Test"]);
            git(&root, &["config", "user.email", "tea-test@example.invalid"]);
            fs::write(root.join("tracked.txt"), "original\n").expect("test file writes");
            git(&root, &["add", "tracked.txt"]);
            git(&root, &["commit", "-m", "initial"]);
            Self {
                directory,
                root,
                session,
            }
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("Git test command starts");
        assert!(
            output.status.success(),
            "git command failed in {}: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn reopened_host(
        repository: &TestRepository,
        session_id: SessionId,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> TuiSubagentHost {
        TuiSubagentHost::new(
            repository.root.clone(),
            repository.session.clone(),
            session_id,
            "logical repository".into(),
            Arc::new(ProviderFactory::new(
                tea_providers::ProviderRegistry::new(),
                None,
                None,
                "logical repository".into(),
            )),
            artifacts,
            Vec::new(),
        )
    }

    #[test]
    fn apply_reconstructs_and_authenticates_a_delta_after_host_reopen() {
        let repository = TestRepository::new();
        let session_id = SessionId::new("host-apply-reopen").expect("session ID is valid");
        let agent_id = AgentId::new("agent-apply-reopen").expect("agent ID is valid");
        let workspace_lease_id = WorkspaceLeaseId::derive(&agent_id);
        let lease = GitWorkspaceEngine
            .prepare(WorkspaceLeaseRequest {
                repository: repository.root.clone(),
                session_directory: repository.session.clone(),
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                workspace_lease_id: workspace_lease_id.clone(),
                logical_workspace_label: "logical repository".into(),
            })
            .expect("child workspace prepares");
        fs::write(lease.worktree_path().join("tracked.txt"), "child result\n")
            .expect("child edit writes");
        let GitWorkspaceFinalization::Delta(delta) = GitWorkspaceEngine
            .finalize(&lease)
            .expect("child result finalizes")
        else {
            panic!("child edit must produce a delta");
        };
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(tea_session::MemoryArtifactStore::default());
        let patch = artifacts
            .put(&delta.patch, "application/vnd.tea.git-patch")
            .expect("patch artifact stores");
        let core_delta = WorkspaceDelta {
            id: WorkspaceDeltaId::derive(
                &delta.workspace_lease_id,
                &delta.base_commit,
                &delta.result_commit,
            ),
            agent_id,
            workspace_lease_id: delta.workspace_lease_id.clone(),
            base_commit: delta.base_commit.clone(),
            result_commit: delta.result_commit.clone(),
            changed_paths: delta
                .changed_paths
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect(),
            patch_artifact: patch.artifact_id,
        };
        // A fresh host deliberately has no volatile `deltas` or `leases`
        // maps. The durable object and hidden refs are the only authority.
        let host = reopened_host(&repository, session_id, artifacts);
        let outcome = smol::block_on(host.apply(ApplyWorkspaceDeltaRequest {
            delta: core_delta.clone(),
            target_lane_id: tea_session::LaneId::main(),
        }))
        .expect("reopened host authenticates and applies durable delta");
        assert!(matches!(outcome, WorkspaceApplyOutcome::Applied { .. }));
        assert_eq!(
            fs::read(repository.root.join("tracked.txt")).expect("parent file reads"),
            b"child result\n"
        );
        assert!(matches!(
            smol::block_on(host.apply(ApplyWorkspaceDeltaRequest {
                delta: core_delta,
                target_lane_id: tea_session::LaneId::main(),
            }))
            .expect("idempotent reopened apply succeeds"),
            WorkspaceApplyOutcome::Applied { .. }
        ));
    }

    #[test]
    fn cleanup_reopens_no_volatile_lease_before_removing_terminal_worktree() {
        let repository = TestRepository::new();
        let session_id = SessionId::new("host-cleanup-reopen").expect("session ID is valid");
        let agent_id = AgentId::new("agent-cleanup-reopen").expect("agent ID is valid");
        let workspace_lease_id = WorkspaceLeaseId::derive(&agent_id);
        let lease = GitWorkspaceEngine
            .prepare(WorkspaceLeaseRequest {
                repository: repository.root.clone(),
                session_directory: repository.session.clone(),
                session_id: session_id.clone(),
                agent_id,
                workspace_lease_id: workspace_lease_id.clone(),
                logical_workspace_label: "logical repository".into(),
            })
            .expect("child workspace prepares");
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(tea_session::MemoryArtifactStore::default());
        let host = reopened_host(&repository, session_id, artifacts);

        smol::block_on(host.cleanup(CoreWorkspaceLease {
            id: workspace_lease_id,
            logical_workspace: "logical repository".into(),
        }))
        .expect("terminal cleanup does not need a live prepare map entry");
        assert!(!lease.worktree_path().exists());
    }
}
