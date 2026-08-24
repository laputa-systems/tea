//! Replay-safe Git worktree leases for local subagents.
//!
//! The engine has one authority boundary: it may create objects and hidden
//! refs in the repository plus an operational worktree under the durable
//! session directory. It never writes the user's index, branch, or checked-out
//! files until an explicit delta application succeeds.

use super::git::{
    environment_with_index, synthetic_commit_environment, trim_line, GitError, GitRepository,
};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use tea_session::{AgentId, Digest, NormalizedPath, SessionId, WorkspaceDeltaId, WorkspaceLeaseId};

/// Inputs that bind one physical child workspace to durable session identity.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceLeaseRequest {
    /// Any directory inside the user's Git worktree.
    pub(crate) repository: PathBuf,
    /// Session-owned directory under which this engine may create its lease.
    pub(crate) session_directory: PathBuf,
    /// Durable session identity used in hidden Git refs.
    pub(crate) session_id: SessionId,
    /// Durable child identity used in hidden Git refs.
    pub(crate) agent_id: AgentId,
    /// The deterministic lease identity derived from `agent_id`.
    pub(crate) workspace_lease_id: WorkspaceLeaseId,
    /// Stable prompt-facing workspace label. It is never replaced by the
    /// physical child worktree path.
    pub(crate) logical_workspace_label: String,
}

/// A prepared detached Git worktree and the immutable spawn snapshot it uses.
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceLease {
    repository_root: PathBuf,
    workspace_lease_id: WorkspaceLeaseId,
    base_commit: String,
    base_ref: String,
    result_ref: String,
    worktree_path: PathBuf,
    private_directory: PathBuf,
    logical_workspace_label: String,
}

impl WorkspaceLease {
    /// Physical authority for child coding tools only.
    pub(crate) fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// Stable label for model-facing prompt composition.
    pub(crate) fn logical_workspace_label(&self) -> &str {
        &self.logical_workspace_label
    }

    /// Synthetic commit that exactly represents the parent at spawn time.
    pub(crate) fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub(crate) fn workspace_lease_id(&self) -> &WorkspaceLeaseId {
        &self.workspace_lease_id
    }
}

/// Immutable result of finalizing a child worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitWorkspaceDelta {
    pub(crate) workspace_lease_id: WorkspaceLeaseId,
    base_ref: String,
    result_ref: String,
    pub(crate) base_commit: String,
    pub(crate) result_commit: String,
    /// Sorted, normalized, repository-relative paths. Rename pairs are
    /// retained as delete-plus-add paths so application can verify both sides.
    pub(crate) changed_paths: Vec<NormalizedPath>,
    /// `git diff --binary --full-index --no-ext-diff` bytes.
    pub(crate) patch: Vec<u8>,
}

impl GitWorkspaceDelta {
    /// Reconstruct a disposable Git delta handle from the immutable durable
    /// description and its verified patch object.
    ///
    /// The hidden refs are deterministic from session and child identity. The
    /// subsequent engine authentication proves those refs still name exactly
    /// this patch and path set before any parent mutation is attempted.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_durable(
        session_id: &SessionId,
        agent_id: &AgentId,
        workspace_lease_id: WorkspaceLeaseId,
        delta_id: WorkspaceDeltaId,
        base_commit: String,
        result_commit: String,
        changed_paths: Vec<String>,
        patch: Vec<u8>,
    ) -> Result<Self, WorkspaceError> {
        if workspace_lease_id != WorkspaceLeaseId::derive(agent_id) {
            return Err(WorkspaceError::LeaseMismatch {
                agent_id: agent_id.clone(),
            });
        }
        if delta_id != WorkspaceDeltaId::derive(&workspace_lease_id, &base_commit, &result_commit) {
            return Err(WorkspaceError::InvalidRequest {
                message: "durable workspace delta ID does not match its lease and commits".into(),
            });
        }
        let changed_paths = changed_paths
            .into_iter()
            .map(|path| {
                NormalizedPath::new(&path).map_err(|error| WorkspaceError::InvalidChangedPath {
                    path,
                    message: error.to_string(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if changed_paths.is_empty() || changed_paths.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(WorkspaceError::InvalidRequest {
                message: "durable workspace delta paths must be nonempty, sorted, and unique"
                    .into(),
            });
        }
        Ok(Self {
            workspace_lease_id,
            base_ref: base_ref_name(session_id, agent_id),
            result_ref: result_ref_name(session_id, agent_id),
            base_commit,
            result_commit,
            changed_paths,
            patch,
        })
    }
}

/// Finalization can durably retain a no-change result without inventing an
/// invalid empty session delta.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceFinalization {
    NoChanges {
        base_commit: String,
        result_commit: String,
    },
    Delta(GitWorkspaceDelta),
}

/// One explicit local Git workspace engine. It owns no async task state.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GitWorkspaceEngine;

/// Request to apply an immutable finalized delta to a parent working tree.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkspaceApplyRequest<'a> {
    /// Any directory inside the target parent worktree.
    pub(crate) repository: &'a Path,
    /// Session-owned directory for the temporary private apply index.
    pub(crate) session_directory: &'a Path,
    /// Delta whose commits and patch are retained by hidden refs.
    pub(crate) delta: &'a GitWorkspaceDelta,
}

/// Digest-level evidence for one path before and after an apply attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceApplyEvidence {
    pub(crate) path: NormalizedPath,
    pub(crate) before: WorkspacePathState,
    pub(crate) after: WorkspacePathState,
    pub(crate) expected_applied: WorkspacePathState,
}

/// The exact content state relevant to one delta path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspacePathState {
    Missing,
    GitEntry {
        mode: String,
        object_type: String,
        content: Digest,
    },
}

/// Honest application classification. `Indeterminate` is a terminal recovery
/// state: callers must not automatically run the patch again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceApplyOutcome {
    Applied {
        evidence: Vec<WorkspaceApplyEvidence>,
    },
    Conflict {
        conflicts: Vec<NormalizedPath>,
        evidence: Vec<WorkspaceApplyEvidence>,
    },
    RolledBack {
        evidence: Vec<WorkspaceApplyEvidence>,
    },
    Indeterminate {
        evidence: Vec<WorkspaceApplyEvidence>,
    },
}

/// A local workspace boundary failure. It is intentionally independent from
/// app/runtime errors so later host wiring can translate it at one edge.
#[derive(Debug)]
pub(crate) enum WorkspaceError {
    Git(GitError),
    Io { path: PathBuf, message: String },
    InvalidRequest { message: String },
    LeaseMismatch { agent_id: AgentId },
    SubmoduleUnsupported { path: String },
    MissingOperationalWorktree { path: PathBuf },
    InvalidChangedPath { path: String, message: String },
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(error) => error.fmt(formatter),
            Self::Io { path, message } => write!(formatter, "{}: {message}", path.display()),
            Self::InvalidRequest { message } => formatter.write_str(message),
            Self::LeaseMismatch { agent_id } => write!(
                formatter,
                "workspace lease does not match deterministic agent {agent_id}"
            ),
            Self::SubmoduleUnsupported { path } => {
                write!(
                    formatter,
                    "submodule {path:?} is unsupported in v1 child workspaces"
                )
            }
            Self::MissingOperationalWorktree { path } => {
                write!(
                    formatter,
                    "child operational worktree is missing: {}",
                    path.display()
                )
            }
            Self::InvalidChangedPath { path, message } => {
                write!(formatter, "changed Git path {path:?} is invalid: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<GitError> for WorkspaceError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}

impl GitWorkspaceEngine {
    /// Build or reopen the one replay-stable detached worktree for an agent.
    pub(crate) fn prepare(
        &self,
        request: WorkspaceLeaseRequest,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        self.prepare_inner(request, false)
    }

    /// Reattach an already durable, operational child lease.
    ///
    /// Unlike [`Self::prepare`], recovery must never turn a missing active
    /// worktree into a fresh checkout: that would silently discard edits that
    /// existed only in the child worktree when the host died. A terminal
    /// result ref is sufficient to reattach a completed lease without a
    /// worktree because finalization can be replayed from the retained refs.
    pub(crate) fn reopen(
        &self,
        request: WorkspaceLeaseRequest,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        self.prepare_inner(request, true)
    }

    fn prepare_inner(
        &self,
        request: WorkspaceLeaseRequest,
        require_existing_worktree: bool,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        validate_request(&request)?;
        let repository = GitRepository::discover(&request.repository)?;
        ensure_directory(&request.session_directory)?;
        let private_directory = request
            .session_directory
            .join("subagents")
            .join(request.workspace_lease_id.as_str());
        ensure_directory(&private_directory)?;

        let base_ref = base_ref_name(&request.session_id, &request.agent_id);
        let result_ref = result_ref_name(&request.session_id, &request.agent_id);
        let base_commit = match resolve_ref(&repository, &base_ref)? {
            Some(commit) => commit,
            None => create_spawn_snapshot(&repository, &private_directory, &base_ref)?,
        };
        let worktree_path = private_directory.join("worktree");
        if worktree_path.exists() {
            let existing = trim_line(&repository.output_in(
                &worktree_path,
                ["rev-parse", "--verify", "HEAD"],
                &[],
            )?);
            if existing != base_commit {
                return Err(WorkspaceError::InvalidRequest {
                    message: format!(
                        "replayed workspace lease points at {existing}, expected durable base {base_commit}"
                    ),
                });
            }
        } else if resolve_ref(&repository, &result_ref)?.is_none() {
            if require_existing_worktree {
                return Err(WorkspaceError::MissingOperationalWorktree {
                    path: worktree_path,
                });
            }
            repository.output(
                [
                    "worktree",
                    "add",
                    "--detach",
                    "--force",
                    worktree_path.to_string_lossy().as_ref(),
                    base_commit.as_str(),
                ],
                &[],
            )?;
        }

        Ok(WorkspaceLease {
            repository_root: repository.root().to_path_buf(),
            workspace_lease_id: request.workspace_lease_id,
            base_commit,
            base_ref,
            result_ref,
            worktree_path,
            private_directory,
            logical_workspace_label: request.logical_workspace_label,
        })
    }

    /// Finalize a child tree through a private index. Repeating after the
    /// result ref exists returns byte-for-byte equivalent delta material.
    pub(crate) fn finalize(
        &self,
        lease: &WorkspaceLease,
    ) -> Result<WorkspaceFinalization, WorkspaceError> {
        let repository = GitRepository::discover(&lease.repository_root)?;
        if let Some(result_commit) = resolve_ref(&repository, &lease.result_ref)? {
            return finalization_from_commits(
                &repository,
                &lease.workspace_lease_id,
                &lease.base_ref,
                &lease.result_ref,
                &lease.base_commit,
                &result_commit,
            );
        }
        if !lease.worktree_path.exists() {
            return Err(WorkspaceError::MissingOperationalWorktree {
                path: lease.worktree_path.clone(),
            });
        }

        let index = lease.private_directory.join("result.index");
        remove_file_if_present(&index)?;
        remove_file_if_present(&index.with_extension("index.lock"))?;
        let index_environment = environment_with_index(&index);
        repository.output_in(
            &lease.worktree_path,
            ["read-tree", lease.base_commit.as_str()],
            &index_environment,
        )?;
        repository.output_in(
            &lease.worktree_path,
            ["add", "-A", "--", "."],
            &index_environment,
        )?;
        reject_submodules(&repository, &index_environment)?;
        let result_tree = trim_line(&repository.output_in(
            &lease.worktree_path,
            ["write-tree"],
            &index_environment,
        )?);
        let result_commit = create_synthetic_commit(
            &repository,
            &result_tree,
            Some(&lease.base_commit),
            "tea child workspace result v1",
        )?;
        update_ref(&repository, &lease.result_ref, &result_commit)?;
        finalization_from_commits(
            &repository,
            &lease.workspace_lease_id,
            &lease.base_ref,
            &lease.result_ref,
            &lease.base_commit,
            &result_commit,
        )
    }

    /// Remove only operational worktree state. Hidden base/result refs remain
    /// so durable completion and cleanup can both be replayed safely.
    pub(crate) fn cleanup(&self, lease: &WorkspaceLease) -> Result<(), WorkspaceError> {
        self.cleanup_paths(
            &lease.repository_root,
            &lease.worktree_path,
            &lease.private_directory,
        )
    }

    /// Remove a durable lease's operational resources without requiring a
    /// process-local `WorkspaceLease`. A terminal child fact may survive a
    /// host restart before `prepare` or `reopen` rebuilt the volatile map.
    /// The caller supplies a session-scoped deterministic lease ID; hidden
    /// base/result refs are deliberately retained.
    pub(crate) fn cleanup_durable_lease(
        &self,
        repository: &Path,
        session_directory: &Path,
        workspace_lease_id: &WorkspaceLeaseId,
    ) -> Result<(), WorkspaceError> {
        let private_directory = session_directory
            .join("subagents")
            .join(workspace_lease_id.as_str());
        self.cleanup_paths(
            repository,
            &private_directory.join("worktree"),
            &private_directory,
        )
    }

    fn cleanup_paths(
        &self,
        repository: &Path,
        worktree_path: &Path,
        private_directory: &Path,
    ) -> Result<(), WorkspaceError> {
        let repository = GitRepository::discover(repository)?;
        if worktree_path.exists() {
            repository.output(
                [
                    "worktree",
                    "remove",
                    "--force",
                    worktree_path.to_string_lossy().as_ref(),
                ],
                &[],
            )?;
        }
        for name in [
            "base.index",
            "base.index.lock",
            "result.index",
            "result.index.lock",
        ] {
            remove_file_if_present(&private_directory.join(name))?;
        }
        Ok(())
    }

    /// Apply a finalized delta after a nonmutating three-way preflight, using
    /// a private index reconstructed from the parent worktree even if Git
    /// internally needs index state for its three-way merge.
    pub(crate) fn apply(
        &self,
        request: WorkspaceApplyRequest<'_>,
    ) -> Result<WorkspaceApplyOutcome, WorkspaceError> {
        let repository = GitRepository::discover(request.repository)?;
        ensure_directory(request.session_directory)?;
        authenticate_delta(&repository, request.delta)?;
        let expected = expected_states(&repository, request.delta)?;
        let before = observed_states(repository.root(), &request.delta.changed_paths)?;
        if states_match(&before, &expected) {
            // The workspace engine has no durable session authority. Matching
            // result bytes could be a user edit or a crash after Git mutated
            // the worktree but before `WorkspaceDeltaAppliedFact`; only core's
            // already-committed fact may certify an idempotent success.
            return Ok(WorkspaceApplyOutcome::Indeterminate {
                evidence: apply_evidence(&request.delta.changed_paths, &before, &before, &expected),
            });
        }
        if before
            .iter()
            .zip(&expected)
            .any(|(before, expected)| before == expected)
        {
            return Ok(WorkspaceApplyOutcome::Indeterminate {
                evidence: apply_evidence(&request.delta.changed_paths, &before, &before, &expected),
            });
        }

        let preflight_index =
            preflight_index_path(request.session_directory, &request.delta.workspace_lease_id);
        build_apply_index(&repository, &preflight_index)?;
        let preflight_environment = environment_with_index(&preflight_index);
        let checked = repository.run_with_input(
            [
                "apply",
                "--3way",
                "--check",
                "--cached",
                "--whitespace=nowarn",
            ],
            &preflight_environment,
            &request.delta.patch,
        )?;
        let preflight_succeeded = if checked.success {
            repository
                .run_with_input(
                    ["apply", "--3way", "--cached", "--whitespace=nowarn"],
                    &preflight_environment,
                    &request.delta.patch,
                )?
                .success
        } else {
            false
        };
        let preflight_expected = if preflight_succeeded {
            states_in_private_index(
                &repository,
                &preflight_environment,
                &request.delta.changed_paths,
            )?
        } else {
            None
        };
        remove_private_index(&preflight_index)?;
        if !preflight_succeeded || preflight_expected.is_none() {
            let after = observed_states(repository.root(), &request.delta.changed_paths)?;
            let evidence = apply_evidence(&request.delta.changed_paths, &before, &after, &expected);
            if states_match(&after, &before) {
                return Ok(WorkspaceApplyOutcome::Conflict {
                    conflicts: request.delta.changed_paths.clone(),
                    evidence,
                });
            }
            return Ok(WorkspaceApplyOutcome::Indeterminate { evidence });
        }

        let expected_after_apply = preflight_expected.expect("checked stage-0 preflight state");
        let index = apply_index_path(request.session_directory, &request.delta.workspace_lease_id);
        build_apply_index(&repository, &index)?;
        let index_environment = environment_with_index(&index);
        let applied = repository.run_with_input(
            ["apply", "--3way", "--whitespace=nowarn"],
            &index_environment,
            &request.delta.patch,
        )?;
        let actual_indexed_expected = states_in_private_index(
            &repository,
            &index_environment,
            &request.delta.changed_paths,
        )?;
        let after = observed_states(repository.root(), &request.delta.changed_paths)?;
        let evidence = apply_evidence(
            &request.delta.changed_paths,
            &before,
            &after,
            &expected_after_apply,
        );
        remove_private_index(&index)?;
        // A three-way application can preserve nonoverlapping parent edits,
        // so its expected state comes from the independently resolved stage-0
        // preflight index, not only the child result tree.
        if applied.success
            && actual_indexed_expected.as_ref() == Some(&expected_after_apply)
            && states_match(&after, &expected_after_apply)
        {
            return Ok(WorkspaceApplyOutcome::Applied { evidence });
        }
        if states_match(&after, &before) {
            return Ok(WorkspaceApplyOutcome::RolledBack { evidence });
        }
        Ok(WorkspaceApplyOutcome::Indeterminate { evidence })
    }
}

fn validate_request(request: &WorkspaceLeaseRequest) -> Result<(), WorkspaceError> {
    if request.logical_workspace_label.is_empty()
        || request
            .logical_workspace_label
            .chars()
            .any(char::is_control)
    {
        return Err(WorkspaceError::InvalidRequest {
            message: "logical workspace label must be a nonempty printable string".into(),
        });
    }
    if request.workspace_lease_id != WorkspaceLeaseId::derive(&request.agent_id) {
        return Err(WorkspaceError::LeaseMismatch {
            agent_id: request.agent_id.clone(),
        });
    }
    Ok(())
}

fn base_ref_name(session_id: &SessionId, agent_id: &AgentId) -> String {
    format!(
        "refs/tea/sessions/{}/agents/{}/base",
        session_id.as_str(),
        agent_id.as_str()
    )
}

fn result_ref_name(session_id: &SessionId, agent_id: &AgentId) -> String {
    format!(
        "refs/tea/sessions/{}/agents/{}/result",
        session_id.as_str(),
        agent_id.as_str()
    )
}

fn create_spawn_snapshot(
    repository: &GitRepository,
    private_directory: &Path,
    base_ref: &str,
) -> Result<String, WorkspaceError> {
    let index = private_directory.join("base.index");
    remove_file_if_present(&index)?;
    remove_file_if_present(&index.with_extension("index.lock"))?;
    let environment = environment_with_index(&index);
    let head = resolve_ref(repository, "HEAD")?;
    match head.as_deref() {
        Some(head) => {
            repository.output(["read-tree", head], &environment)?;
        }
        None => {
            repository.output(["read-tree", "--empty"], &environment)?;
        }
    }
    repository.output(["add", "-A", "--", "."], &environment)?;
    reject_submodules(repository, &environment)?;
    let tree = trim_line(&repository.output(["write-tree"], &environment)?);
    let base_commit = create_synthetic_commit(
        repository,
        &tree,
        head.as_deref(),
        "tea child workspace base v1",
    )?;
    update_ref(repository, base_ref, &base_commit)?;
    Ok(base_commit)
}

fn create_synthetic_commit(
    repository: &GitRepository,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String, WorkspaceError> {
    let mut arguments = vec![OsString::from("commit-tree"), OsString::from(tree)];
    if let Some(parent) = parent {
        arguments.push(OsString::from("-p"));
        arguments.push(OsString::from(parent));
    }
    arguments.push(OsString::from("-m"));
    arguments.push(OsString::from(message));
    Ok(trim_line(
        &repository.output(arguments, &synthetic_commit_environment())?,
    ))
}

fn resolve_ref(
    repository: &GitRepository,
    reference: &str,
) -> Result<Option<String>, WorkspaceError> {
    Ok(repository
        .optional_output(["rev-parse", "--verify", "--quiet", reference], &[])?
        .map(|value| trim_line(&value)))
}

fn update_ref(
    repository: &GitRepository,
    reference: &str,
    commit: &str,
) -> Result<(), WorkspaceError> {
    repository.output(["update-ref", reference, commit], &[])?;
    Ok(())
}

fn reject_submodules(
    repository: &GitRepository,
    environment: &[(OsString, OsString)],
) -> Result<(), WorkspaceError> {
    let staged = repository.output(["ls-files", "--stage", "-z"], environment)?;
    for entry in staged
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.starts_with(b"160000 ") {
            let path = entry
                .splitn(2, |byte| *byte == b'\t')
                .nth(1)
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .unwrap_or_else(|| "<unknown>".into());
            return Err(WorkspaceError::SubmoduleUnsupported { path });
        }
    }
    Ok(())
}

fn finalization_from_commits(
    repository: &GitRepository,
    workspace_lease_id: &WorkspaceLeaseId,
    base_ref: &str,
    result_ref: &str,
    base_commit: &str,
    result_commit: &str,
) -> Result<WorkspaceFinalization, WorkspaceError> {
    let changed_paths = changed_paths(repository, base_commit, result_commit)?;
    if changed_paths.is_empty() {
        return Ok(WorkspaceFinalization::NoChanges {
            base_commit: base_commit.into(),
            result_commit: result_commit.into(),
        });
    }
    let patch = repository.output(
        [
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            base_commit,
            result_commit,
        ],
        &[],
    )?;
    if patch.is_empty() {
        return Err(WorkspaceError::InvalidRequest {
            message: "Git reported changed paths but produced an empty binary patch".into(),
        });
    }
    Ok(WorkspaceFinalization::Delta(GitWorkspaceDelta {
        workspace_lease_id: workspace_lease_id.clone(),
        base_ref: base_ref.into(),
        result_ref: result_ref.into(),
        base_commit: base_commit.into(),
        result_commit: result_commit.into(),
        changed_paths,
        patch,
    }))
}

fn changed_paths(
    repository: &GitRepository,
    base_commit: &str,
    result_commit: &str,
) -> Result<Vec<NormalizedPath>, WorkspaceError> {
    let output = repository.output(
        [
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            base_commit,
            result_commit,
        ],
        &[],
    )?;
    let mut paths = BTreeSet::new();
    for raw_path in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path =
            std::str::from_utf8(raw_path).map_err(|error| WorkspaceError::InvalidChangedPath {
                path: String::from_utf8_lossy(raw_path).into_owned(),
                message: format!("Git path is not UTF-8: {error}"),
            })?;
        let normalized =
            NormalizedPath::new(path).map_err(|error| WorkspaceError::InvalidChangedPath {
                path: path.into(),
                message: error.to_string(),
            })?;
        paths.insert(normalized);
    }
    Ok(paths.into_iter().collect())
}

fn expected_states(
    repository: &GitRepository,
    delta: &GitWorkspaceDelta,
) -> Result<Vec<WorkspacePathState>, WorkspaceError> {
    delta
        .changed_paths
        .iter()
        .map(|path| state_at_treeish(repository, &delta.result_commit, path))
        .collect()
}

fn states_in_private_index(
    repository: &GitRepository,
    environment: &[(OsString, OsString)],
    paths: &[NormalizedPath],
) -> Result<Option<Vec<WorkspacePathState>>, WorkspaceError> {
    let Some(tree) = repository.optional_output(["write-tree"], environment)? else {
        return Ok(None);
    };
    let tree = trim_line(&tree);
    paths
        .iter()
        .map(|path| state_at_treeish(repository, &tree, path))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn state_at_treeish(
    repository: &GitRepository,
    commit: &str,
    path: &NormalizedPath,
) -> Result<WorkspacePathState, WorkspaceError> {
    let entry = repository.output(
        ["ls-tree", "--full-tree", "-z", commit, "--", path.as_str()],
        &[],
    )?;
    let Some(entry) = entry
        .split(|byte| *byte == 0)
        .find(|entry| !entry.is_empty())
    else {
        return Ok(WorkspacePathState::Missing);
    };
    let mut entry_parts = entry.splitn(2, |byte| *byte == b'\t');
    let header = entry_parts
        .next()
        .expect("splitn always returns its first slice");
    if entry_parts.next().is_none() {
        return Err(WorkspaceError::InvalidRequest {
            message: format!("Git emitted malformed tree entry for {}", path.as_str()),
        });
    }
    let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(WorkspaceError::InvalidRequest {
            message: format!(
                "Git emitted malformed tree entry header for {}",
                path.as_str()
            ),
        });
    }
    let mode = std::str::from_utf8(fields[0]).map_err(|error| WorkspaceError::InvalidRequest {
        message: format!("Git emitted a non-UTF-8 tree mode: {error}"),
    })?;
    let object_type =
        std::str::from_utf8(fields[1]).map_err(|error| WorkspaceError::InvalidRequest {
            message: format!("Git emitted a non-UTF-8 tree object type: {error}"),
        })?;
    if object_type != "blob" || !matches!(mode, "100644" | "100755" | "120000") {
        return Err(WorkspaceError::InvalidRequest {
            message: format!(
                "Git path {} has unsupported mode/type {mode} {object_type}",
                path.as_str()
            ),
        });
    }
    let object = format!("{commit}:{}", path.as_str());
    let bytes = repository.output(["show", object.as_str()], &[])?;
    Ok(WorkspacePathState::GitEntry {
        mode: mode.into(),
        object_type: object_type.into(),
        content: Digest::from_bytes(bytes),
    })
}

fn observed_states(
    repository_root: &Path,
    paths: &[NormalizedPath],
) -> Result<Vec<WorkspacePathState>, WorkspaceError> {
    paths
        .iter()
        .map(|path| {
            let path_on_disk = repository_root.join(path.as_str());
            observed_path_state(&path_on_disk)
        })
        .collect()
}

fn observed_path_state(path: &Path) -> Result<WorkspacePathState, WorkspaceError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspacePathState::Missing);
        }
        Err(error) => {
            return Err(WorkspaceError::Io {
                path: path.to_path_buf(),
                message: error.to_string(),
            });
        }
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|error| WorkspaceError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        return Ok(WorkspacePathState::GitEntry {
            mode: "120000".into(),
            object_type: "blob".into(),
            content: Digest::from_bytes(path_bytes(&target)),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(WorkspaceError::InvalidRequest {
            message: format!(
                "workspace path {} is not a regular file or symlink",
                path.display()
            ),
        });
    }
    let bytes = fs::read(path).map_err(|error| WorkspaceError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(WorkspacePathState::GitEntry {
        mode: observed_regular_mode(&metadata),
        object_type: "blob".into(),
        content: Digest::from_bytes(bytes),
    })
}

#[cfg(unix)]
fn observed_regular_mode(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o111 == 0 {
        "100644".into()
    } else {
        "100755".into()
    }
}

#[cfg(not(unix))]
fn observed_regular_mode(_metadata: &fs::Metadata) -> String {
    "100644".into()
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

fn states_match(left: &[WorkspacePathState], right: &[WorkspacePathState]) -> bool {
    left == right
}

fn apply_evidence(
    paths: &[NormalizedPath],
    before: &[WorkspacePathState],
    after: &[WorkspacePathState],
    expected_applied: &[WorkspacePathState],
) -> Vec<WorkspaceApplyEvidence> {
    paths
        .iter()
        .cloned()
        .zip(before.iter().cloned())
        .zip(after.iter().cloned())
        .zip(expected_applied.iter().cloned())
        .map(
            |(((path, before), after), expected_applied)| WorkspaceApplyEvidence {
                path,
                before,
                after,
                expected_applied,
            },
        )
        .collect()
}

fn apply_index_path(session_directory: &Path, workspace_lease_id: &WorkspaceLeaseId) -> PathBuf {
    session_directory
        .join("subagents")
        .join("apply")
        .join(format!("{}.index", workspace_lease_id.as_str()))
}

fn preflight_index_path(
    session_directory: &Path,
    workspace_lease_id: &WorkspaceLeaseId,
) -> PathBuf {
    session_directory
        .join("subagents")
        .join("apply")
        .join(format!("{}.preflight.index", workspace_lease_id.as_str()))
}

fn build_apply_index(
    repository: &GitRepository,
    private_index: &Path,
) -> Result<(), WorkspaceError> {
    let directory = private_index
        .parent()
        .expect("private index path has a parent");
    ensure_directory(directory)?;
    remove_file_if_present(private_index)?;
    remove_file_if_present(&private_index.with_extension("index.lock"))?;
    let environment = environment_with_index(private_index);
    match resolve_ref(repository, "HEAD")?.as_deref() {
        Some(head) => repository.output(["read-tree", head], &environment)?,
        None => repository.output(["read-tree", "--empty"], &environment)?,
    };
    repository.output(["add", "-A", "--", "."], &environment)?;
    Ok(())
}

fn remove_private_index(private_index: &Path) -> Result<(), WorkspaceError> {
    remove_file_if_present(private_index)?;
    remove_file_if_present(&private_index.with_extension("index.lock"))
}

fn authenticate_delta(
    repository: &GitRepository,
    delta: &GitWorkspaceDelta,
) -> Result<(), WorkspaceError> {
    if resolve_ref(repository, &delta.base_ref)?.as_deref() != Some(delta.base_commit.as_str())
        || resolve_ref(repository, &delta.result_ref)?.as_deref()
            != Some(delta.result_commit.as_str())
    {
        return Err(WorkspaceError::InvalidRequest {
            message: "workspace delta commits are not retained by their durable hidden refs".into(),
        });
    }
    let canonical = finalization_from_commits(
        repository,
        &delta.workspace_lease_id,
        &delta.base_ref,
        &delta.result_ref,
        &delta.base_commit,
        &delta.result_commit,
    )?;
    let WorkspaceFinalization::Delta(expected) = canonical else {
        return Err(WorkspaceError::InvalidRequest {
            message: "workspace delta names an unchanged result tree".into(),
        });
    };
    if expected.patch != delta.patch || expected.changed_paths != delta.changed_paths {
        return Err(WorkspaceError::InvalidRequest {
            message:
                "workspace delta patch or changed paths disagree with its retained Git commits"
                    .into(),
        });
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), WorkspaceError> {
    fs::create_dir_all(path).map_err(|error| WorkspaceError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn remove_file_if_present(path: &Path) -> Result<(), WorkspaceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WorkspaceError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);
    static TEST_GIT_CONFIG: OnceLock<PathBuf> = OnceLock::new();

    struct TestRepository {
        directory: PathBuf,
        root: PathBuf,
        session: PathBuf,
    }

    impl TestRepository {
        fn new(label: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "tea-agent-subagent-workspace-{label}-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            let root = directory.join("repository");
            let session = directory.join("session");
            fs::create_dir_all(&root).expect("test repository directory creates");
            run_git(&root, ["init"]);
            Self {
                directory,
                root,
                session,
            }
        }

        fn unborn(label: &str) -> Self {
            Self::new(label)
        }

        fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
            let path = self.root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("test file parent creates");
            }
            fs::write(path, bytes).expect("test file writes");
        }

        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(self.root.join(path)).expect("test file reads")
        }

        fn commit_all(&self, message: &str) {
            run_git(&self.root, ["add", "-A"]);
            run_git(
                &self.root,
                [
                    "-c",
                    "user.name=Tea Test",
                    "-c",
                    "user.email=tea-test@example.invalid",
                    "commit",
                    "-m",
                    message,
                ],
            );
        }

        fn index_bytes(&self) -> Option<Vec<u8>> {
            match fs::read(self.root.join(".git/index")) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("test index reads: {error}"),
            }
        }

        fn head(&self) -> String {
            String::from_utf8(run_git(&self.root, ["rev-parse", "HEAD"]))
                .expect("Git head is UTF-8")
                .trim()
                .into()
        }

        fn request(&self, agent: &str) -> WorkspaceLeaseRequest {
            let agent_id = AgentId::new(agent).expect("valid test agent ID");
            WorkspaceLeaseRequest {
                repository: self.root.clone(),
                session_directory: self.session.clone(),
                session_id: SessionId::new("workspace-test-session")
                    .expect("valid test session ID"),
                workspace_lease_id: WorkspaceLeaseId::derive(&agent_id),
                agent_id,
                logical_workspace_label: "repository".into(),
            }
        }

        fn apply_request<'a>(&'a self, delta: &'a GitWorkspaceDelta) -> WorkspaceApplyRequest<'a> {
            WorkspaceApplyRequest {
                repository: &self.root,
                session_directory: &self.session,
                delta,
            }
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn run_git<const N: usize>(directory: &Path, arguments: [&str; N]) -> Vec<u8> {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", test_git_config())
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .expect("Git test command starts");
        assert!(
            output.status.success(),
            "git command failed in {}: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn test_git_config() -> &'static Path {
        TEST_GIT_CONFIG
            .get_or_init(|| {
                let path = std::env::temp_dir()
                    .join(format!("tea-agent-empty-git-config-{}", std::process::id()));
                fs::write(&path, b"").expect("empty isolated Git config writes");
                path
            })
            .as_path()
    }

    fn prepare(repository: &TestRepository, agent: &str) -> WorkspaceLease {
        GitWorkspaceEngine
            .prepare(repository.request(agent))
            .expect("workspace lease prepares")
    }

    fn finalized_delta(repository: &TestRepository, agent: &str) -> GitWorkspaceDelta {
        let lease = prepare(repository, agent);
        fs::write(lease.worktree_path().join("tracked.txt"), "child result\n")
            .expect("child edit writes");
        match GitWorkspaceEngine
            .finalize(&lease)
            .expect("child workspace finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("child edit must produce a delta"),
        }
    }

    #[test]
    fn workspace_snapshot_captures_dirty_deletion_untracked_binary_and_rename_without_touching_parent(
    ) {
        let repository = TestRepository::new("snapshot");
        repository.write("tracked.txt", "committed\n");
        repository.write("deleted.txt", "delete me\n");
        repository.write("rename-from.txt", "rename me\n");
        repository.write(".gitignore", "*.ignored\n");
        repository.commit_all("initial");
        repository.write("tracked.txt", "dirty tracked\n");
        fs::remove_file(repository.root.join("deleted.txt")).expect("tracked file deletes");
        fs::rename(
            repository.root.join("rename-from.txt"),
            repository.root.join("renamed.txt"),
        )
        .expect("tracked file renames");
        repository.write("untracked.txt", "include me\n");
        repository.write("binary.bin", [0, 159, 146, 150, 255]);
        repository.write("skip.ignored", "never snapshot\n");
        let parent_index = repository.index_bytes();
        let parent_head = repository.head();
        let parent_tracked = repository.read("tracked.txt");

        let lease = prepare(&repository, "agent-snapshot");
        assert_eq!(lease.logical_workspace_label(), "repository");
        assert_ne!(
            lease.logical_workspace_label(),
            lease.worktree_path().to_string_lossy()
        );
        assert_eq!(
            fs::read(lease.worktree_path().join("tracked.txt")).unwrap(),
            parent_tracked
        );
        assert!(!lease.worktree_path().join("deleted.txt").exists());
        assert_eq!(
            fs::read(lease.worktree_path().join("untracked.txt")).unwrap(),
            b"include me\n"
        );
        assert_eq!(
            fs::read(lease.worktree_path().join("binary.bin")).unwrap(),
            [0, 159, 146, 150, 255]
        );
        assert!(!lease.worktree_path().join("skip.ignored").exists());
        assert!(!lease.worktree_path().join("rename-from.txt").exists());
        assert_eq!(
            fs::read(lease.worktree_path().join("renamed.txt")).unwrap(),
            b"rename me\n"
        );
        assert_eq!(repository.index_bytes(), parent_index);
        assert_eq!(repository.head(), parent_head);
        assert_eq!(repository.read("tracked.txt"), b"dirty tracked\n");
        assert!(!repository.root.join("deleted.txt").exists());
        assert!(repository.root.join("skip.ignored").exists());
    }

    #[test]
    fn workspace_prepare_replays_the_hidden_base_and_two_children_are_isolated() {
        let repository = TestRepository::new("replay-and-isolation");
        repository.write("tracked.txt", "original\n");
        repository.commit_all("initial");
        let first = prepare(&repository, "agent-first");
        fs::write(
            first.worktree_path().join("tracked.txt"),
            "first child only\n",
        )
        .expect("first child edit writes");
        let replayed = GitWorkspaceEngine
            .prepare(repository.request("agent-first"))
            .expect("same durable lease reopens");
        assert_eq!(replayed.base_commit(), first.base_commit());
        assert_eq!(
            fs::read(replayed.worktree_path().join("tracked.txt")).unwrap(),
            b"first child only\n"
        );
        let second = prepare(&repository, "agent-second");
        assert_eq!(
            fs::read(second.worktree_path().join("tracked.txt")).unwrap(),
            b"original\n"
        );
        assert_eq!(repository.read("tracked.txt"), b"original\n");
        run_git(
            &repository.root,
            ["show-ref", "--verify", first.base_ref.as_str()],
        );
        assert_ne!(first.worktree_path(), second.worktree_path());
    }

    #[test]
    fn workspace_reopen_refuses_to_recreate_a_missing_active_worktree() {
        let repository = TestRepository::new("missing-active-reopen");
        repository.write("tracked.txt", "original\n");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-active");
        fs::write(
            lease.worktree_path().join("tracked.txt"),
            "unfinalized child edit\n",
        )
        .expect("child edit writes");
        GitWorkspaceEngine
            .cleanup(&lease)
            .expect("test removes only the operational worktree");

        let reopened = GitWorkspaceEngine.reopen(repository.request("agent-active"));

        assert!(matches!(
            reopened,
            Err(WorkspaceError::MissingOperationalWorktree { path }) if path == lease.worktree_path()
        ));
        assert!(
            !lease.worktree_path().exists(),
            "recovery must not manufacture a replacement checkout over unfinalized edits"
        );
    }

    #[test]
    fn workspace_durable_cleanup_does_not_require_a_live_lease_handle() {
        let repository = TestRepository::new("durable-cleanup");
        repository.write("tracked.txt", "original\n");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-terminal");
        fs::write(lease.worktree_path().join("tracked.txt"), "child result\n")
            .expect("child edit writes");
        let _ = GitWorkspaceEngine
            .finalize(&lease)
            .expect("terminal child finalizes before process restart");

        GitWorkspaceEngine
            .cleanup_durable_lease(
                &repository.root,
                &repository.session,
                lease.workspace_lease_id(),
            )
            .expect("recovered cleanup removes the unowned operational tree");
        GitWorkspaceEngine
            .cleanup_durable_lease(
                &repository.root,
                &repository.session,
                lease.workspace_lease_id(),
            )
            .expect("recovered cleanup is idempotent when the tree is already absent");
        assert!(!lease.worktree_path().exists());
        assert!(matches!(
            GitWorkspaceEngine.finalize(&lease),
            Ok(WorkspaceFinalization::Delta(_))
        ));
    }

    #[test]
    fn workspace_rejects_submodules_and_supports_unborn_repositories() {
        let repository = TestRepository::new("submodule");
        repository.write("tracked.txt", "parent\n");
        repository.commit_all("initial");
        let child_repository = repository.directory.join("child-repository");
        fs::create_dir_all(&child_repository).expect("child repository creates");
        run_git(&child_repository, ["init"]);
        fs::write(child_repository.join("nested.txt"), "nested\n").expect("nested file writes");
        run_git(&child_repository, ["add", "nested.txt"]);
        run_git(
            &child_repository,
            [
                "-c",
                "user.name=Tea Test",
                "-c",
                "user.email=tea-test@example.invalid",
                "commit",
                "-m",
                "nested",
            ],
        );
        run_git(
            &repository.root,
            [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                child_repository.to_string_lossy().as_ref(),
                "vendor",
            ],
        );
        assert!(matches!(
            GitWorkspaceEngine.prepare(repository.request("agent-submodule")),
            Err(WorkspaceError::SubmoduleUnsupported { .. })
        ));

        let unborn = TestRepository::unborn("unborn");
        unborn.write("first.txt", "unborn contents\n");
        let lease = prepare(&unborn, "agent-unborn");
        assert_eq!(
            fs::read(lease.worktree_path().join("first.txt")).unwrap(),
            b"unborn contents\n"
        );
        fs::write(lease.worktree_path().join("first.txt"), "child result\n")
            .expect("unborn child edit writes");
        assert!(matches!(
            GitWorkspaceEngine.finalize(&lease),
            Ok(WorkspaceFinalization::Delta(_))
        ));
    }

    #[test]
    fn workspace_finalization_emits_binary_full_index_patch_and_replays_after_cleanup() {
        let repository = TestRepository::new("finalize");
        repository.write("tracked.txt", "original\n");
        repository.write("binary.bin", [0, 1, 2, 3]);
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-finalize");
        fs::write(lease.worktree_path().join("tracked.txt"), "child result\n")
            .expect("child text edit writes");
        fs::write(lease.worktree_path().join("binary.bin"), [255, 0, 254, 1])
            .expect("child binary edit writes");
        fs::rename(
            lease.worktree_path().join("tracked.txt"),
            lease.worktree_path().join("renamed.txt"),
        )
        .expect("child rename succeeds");

        let first = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("finalization succeeds")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("edits must produce a delta"),
        };
        let path_names = first
            .changed_paths
            .iter()
            .map(|path| path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(path_names, vec!["binary.bin", "renamed.txt", "tracked.txt"]);
        assert!(first
            .patch
            .windows(b"GIT binary patch".len())
            .any(|window| window == b"GIT binary patch"));
        let replayed = GitWorkspaceEngine
            .finalize(&lease)
            .expect("finalization replays");
        assert_eq!(replayed, WorkspaceFinalization::Delta(first.clone()));
        GitWorkspaceEngine
            .cleanup(&lease)
            .expect("cleanup succeeds");
        assert!(!lease.worktree_path().exists());
        GitWorkspaceEngine.cleanup(&lease).expect("cleanup replays");
        let reopened = GitWorkspaceEngine
            .prepare(repository.request("agent-finalize"))
            .expect("terminal lease reopens from durable refs");
        assert!(!reopened.worktree_path().exists());
        GitWorkspaceEngine
            .cleanup(&reopened)
            .expect("terminal cleanup remains a fixed point");
        assert_eq!(
            GitWorkspaceEngine
                .finalize(&reopened)
                .expect("result ref replays after cleanup"),
            WorkspaceFinalization::Delta(first.clone())
        );
        run_git(
            &repository.root,
            ["show-ref", "--verify", lease.base_ref.as_str()],
        );
        run_git(
            &repository.root,
            ["show-ref", "--verify", lease.result_ref.as_str()],
        );
        assert_eq!(repository.read("tracked.txt"), b"original\n");
    }

    #[test]
    fn workspace_finalization_records_and_replays_a_no_change_result() {
        let repository = TestRepository::new("finalize-no-changes");
        repository.write("tracked.txt", "original\n");
        repository.commit_all("initial");
        let parent_head = repository.head();
        let parent_index = repository.index_bytes();
        let lease = prepare(&repository, "agent-no-changes");

        let first = GitWorkspaceEngine
            .finalize(&lease)
            .expect("unchanged child workspace finalizes");
        let (base_commit, result_commit) = match &first {
            WorkspaceFinalization::NoChanges {
                base_commit,
                result_commit,
            } => (base_commit, result_commit),
            WorkspaceFinalization::Delta(_) => panic!("unchanged child must not produce a delta"),
        };
        assert_eq!(base_commit, lease.base_commit());
        assert_ne!(base_commit, result_commit);
        assert_eq!(
            GitWorkspaceEngine
                .finalize(&lease)
                .expect("no-change finalization replays"),
            first
        );
        GitWorkspaceEngine
            .cleanup(&lease)
            .expect("no-change workspace cleans up");
        let reopened = GitWorkspaceEngine
            .prepare(repository.request("agent-no-changes"))
            .expect("no-change result ref reopens");
        assert!(!reopened.worktree_path().exists());
        assert_eq!(
            GitWorkspaceEngine
                .finalize(&reopened)
                .expect("no-change finalization replays after cleanup"),
            first
        );
        assert_eq!(repository.read("tracked.txt"), b"original\n");
        assert_eq!(repository.head(), parent_head);
        assert_eq!(repository.index_bytes(), parent_index);
    }

    #[test]
    fn workspace_applies_clean_binary_deltas_without_touching_the_parent_index() {
        let repository = TestRepository::new("apply-clean");
        repository.write("tracked.txt", "original\n");
        repository.write("binary.bin", [0, 1, 2, 3]);
        repository.write("unrelated.txt", "committed\n");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-apply-clean");
        fs::write(lease.worktree_path().join("tracked.txt"), "child result\n")
            .expect("child text edit writes");
        fs::write(lease.worktree_path().join("binary.bin"), [255, 0, 254, 1])
            .expect("child binary edit writes");
        let delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("edits must produce a delta"),
        };
        repository.write("unrelated.txt", "staged user change\n");
        run_git(&repository.root, ["add", "unrelated.txt"]);
        repository.write("unrelated.txt", "unstaged user change\n");
        let index_before = repository.index_bytes();
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Applied { .. })
        ));
        assert_eq!(repository.read("tracked.txt"), b"child result\n");
        assert_eq!(repository.read("binary.bin"), [255, 0, 254, 1]);
        assert_eq!(repository.read("unrelated.txt"), b"unstaged user change\n");
        assert_eq!(repository.index_bytes(), index_before);
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Indeterminate { .. })
        ));

        let mut tampered_patch = delta.clone();
        tampered_patch.patch.push(b'!');
        let tracked_before = repository.read("tracked.txt");
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&tampered_patch)),
            Err(WorkspaceError::InvalidRequest { .. })
        ));
        assert_eq!(repository.read("tracked.txt"), tracked_before);
        let mut tampered_paths = delta.clone();
        tampered_paths.changed_paths.pop();
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&tampered_paths)),
            Err(WorkspaceError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn workspace_apply_does_not_certify_a_preexisting_result_without_a_durable_fact() {
        let repository = TestRepository::new("apply-preexisting-result");
        repository.write("tracked.txt", "parent state\n");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-preexisting-result");
        fs::write(lease.worktree_path().join("tracked.txt"), "child state\n")
            .expect("child edit writes");
        let delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("child delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("fixture must produce a delta"),
        };

        repository.write("tracked.txt", "child state\n");
        let outcome = GitWorkspaceEngine
            .apply(repository.apply_request(&delta))
            .expect("preexisting result is classified");

        assert!(
            matches!(outcome, WorkspaceApplyOutcome::Indeterminate { .. }),
            "matching bytes without a durable applied fact cannot prove Tea committed the delta"
        );
    }

    #[test]
    fn workspace_apply_handles_three_way_conflict_and_indeterminate_without_unapproved_mutation() {
        let repository = TestRepository::new("apply-classification");
        repository.write(
            "tracked.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        );
        repository.write("second.txt", "second original\n");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-three-way");
        fs::write(
            lease.worktree_path().join("tracked.txt"),
            "one\ntwo child\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        )
        .expect("child edit writes");
        let three_way_delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("child edit must produce delta"),
        };
        repository.write(
            "tracked.txt",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine parent\nten\n",
        );
        let index_before = repository.index_bytes();
        let three_way_outcome =
            GitWorkspaceEngine.apply(repository.apply_request(&three_way_delta));
        assert!(
            matches!(&three_way_outcome, Ok(WorkspaceApplyOutcome::Applied { .. })),
            "three-way application should merge nonoverlapping parent and child edits: {three_way_outcome:?}"
        );
        assert_eq!(
            repository.read("tracked.txt"),
            b"one\ntwo child\nthree\nfour\nfive\nsix\nseven\neight\nnine parent\nten\n"
        );
        assert_eq!(repository.index_bytes(), index_before);

        let conflict_repository = TestRepository::new("apply-conflict");
        conflict_repository.write("tracked.txt", "original\n");
        conflict_repository.commit_all("initial");
        let conflict_delta = finalized_delta(&conflict_repository, "agent-conflict");
        conflict_repository.write("tracked.txt", "parent conflict\n");
        let before = conflict_repository.read("tracked.txt");
        let index_before = conflict_repository.index_bytes();
        let conflict_outcome =
            GitWorkspaceEngine.apply(conflict_repository.apply_request(&conflict_delta));
        assert!(
            matches!(
                &conflict_outcome,
                Ok(WorkspaceApplyOutcome::Conflict { .. })
            ),
            "conflicting preflight must leave the parent unchanged: {conflict_outcome:?}"
        );
        assert_eq!(conflict_repository.read("tracked.txt"), before);
        assert_eq!(conflict_repository.index_bytes(), index_before);

        let indeterminate_repository = TestRepository::new("apply-indeterminate");
        indeterminate_repository.write("tracked.txt", "tracked original\n");
        indeterminate_repository.write("second.txt", "second original\n");
        indeterminate_repository.commit_all("initial");
        let lease = prepare(&indeterminate_repository, "agent-indeterminate");
        fs::write(lease.worktree_path().join("tracked.txt"), "tracked child\n")
            .expect("first child edit writes");
        fs::write(lease.worktree_path().join("second.txt"), "second child\n")
            .expect("second child edit writes");
        let delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("child edits must produce delta"),
        };
        indeterminate_repository.write("tracked.txt", "tracked child\n");
        let second_before = indeterminate_repository.read("second.txt");
        assert!(matches!(
            GitWorkspaceEngine.apply(indeterminate_repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Indeterminate { .. })
        ));
        assert_eq!(indeterminate_repository.read("second.txt"), second_before);
    }

    #[test]
    fn workspace_apply_rejects_a_binary_conflict_without_touching_parent_bytes_or_index() {
        let repository = TestRepository::new("apply-binary-conflict");
        let base = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let child = [255, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let parent = [254, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        repository.write("binary.bin", base);
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-binary-conflict");
        fs::write(lease.worktree_path().join("binary.bin"), child)
            .expect("child binary edit writes");
        let delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("binary delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("binary edit must produce a delta"),
        };
        repository.write("binary.bin", parent);
        let parent_index = repository.index_bytes();

        let outcome = GitWorkspaceEngine.apply(repository.apply_request(&delta));
        assert!(
            matches!(outcome, Ok(WorkspaceApplyOutcome::Conflict { .. })),
            "binary preflight must classify divergence as conflict: {outcome:?}"
        );
        assert_eq!(repository.read("binary.bin"), parent);
        assert_eq!(repository.index_bytes(), parent_index);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_apply_classifies_executable_mode_changes() {
        use std::os::unix::fs::PermissionsExt as _;

        let repository = TestRepository::new("apply-executable");
        run_git(&repository.root, ["config", "core.filemode", "true"]);
        repository.write("script.sh", "#!/bin/sh\nprintf tea\n");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-executable");
        let script = lease.worktree_path().join("script.sh");
        let mut permissions = fs::metadata(&script)
            .expect("child script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("child script becomes executable");
        let delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("mode delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => panic!("mode change must produce a delta"),
        };
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Applied { .. })
        ));
        assert_ne!(
            fs::metadata(repository.root.join("script.sh"))
                .expect("parent script metadata")
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Indeterminate { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_apply_compares_symlink_targets_without_following_them() {
        use std::os::unix::fs::symlink;

        let repository = TestRepository::new("apply-symlink");
        repository.write("target-a", "A\n");
        repository.write("target-b", "B\n");
        symlink("target-a", repository.root.join("link")).expect("base symlink creates");
        repository.commit_all("initial");
        let lease = prepare(&repository, "agent-symlink");
        let link = lease.worktree_path().join("link");
        fs::remove_file(&link).expect("child symlink removes");
        symlink("target-b", &link).expect("child symlink updates");
        let delta = match GitWorkspaceEngine
            .finalize(&lease)
            .expect("symlink delta finalizes")
        {
            WorkspaceFinalization::Delta(delta) => delta,
            WorkspaceFinalization::NoChanges { .. } => {
                panic!("symlink target change must produce a delta")
            }
        };
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Applied { .. })
        ));
        assert_eq!(
            fs::read_link(repository.root.join("link")).expect("parent symlink reads"),
            PathBuf::from("target-b")
        );
        assert!(matches!(
            GitWorkspaceEngine.apply(repository.apply_request(&delta)),
            Ok(WorkspaceApplyOutcome::Indeterminate { .. })
        ));
    }
}
