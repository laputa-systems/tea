//! Public operation contracts and value types for trusted coding capabilities.

use crate::scheduler::CancellationToken;
use crate::tool::ToolUpdateSink;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;

/// A future returned by a host operation adapter.
pub type OperationFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, OperationError>> + Send + 'a>>;

/// A host-side failure from a coding-tool operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationError {
    message: String,
}

impl OperationError {
    /// Construct an operation failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Borrow the host-provided message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OperationError {}

/// Metadata needed by coding capabilities without exposing `std::fs::Metadata`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryMetadata {
    /// Whether the entry is a directory.
    pub is_directory: bool,
    /// Whether the entry is an ordinary regular file.
    pub is_regular_file: bool,
}

/// Output from an explicit shell operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Process exit code, if the process exited normally.
    pub exit_code: Option<i32>,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// One complete file snapshot returned by a batch read operation.
///
/// Adapters must inspect file kind before reading. `is_regular_file = false`
/// lets the tool reject directories, devices, and FIFOs without treating them
/// as text input; a local adapter must not open a non-regular file at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSnapshot {
    /// The requested canonical path, retained so a remote host cannot reorder
    /// or substitute snapshots silently.
    pub path: PathBuf,
    /// Whether the inspected entry was an ordinary regular file.
    pub is_regular_file: bool,
    /// Complete file content when `is_regular_file` is true.
    pub content: Vec<u8>,
}

/// One conditional replacement in an edit transaction.
///
/// The adapter must compare `expected_content` with the current complete file
/// before its commit point. `path` is already a canonical, in-workspace path.
/// Replacements are distinct from conditional creations in the same request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalFileEdit {
    /// Canonical existing file path selected by the tool boundary.
    pub path: PathBuf,
    /// Complete original bytes read and validated by the tool.
    pub expected_content: Vec<u8>,
    /// Complete replacement bytes derived from that original snapshot.
    pub replacement_content: Vec<u8>,
}

/// One conditional creation in an edit transaction.
///
/// `path` is already a canonical in-workspace path whose parent exists. The
/// host must establish that it is absent during the transaction's complete
/// precondition phase, then publish it as part of the same transaction as any
/// replacements.  Keeping creation in the transaction type prevents the
/// model-facing edit primitive from degenerating into a loop of writes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalFileCreate {
    /// Canonical absent target selected by the workspace authority.
    pub path: PathBuf,
    /// Complete bytes to publish if and only if the target remains absent.
    pub content: Vec<u8>,
}

/// One explicit conditional edit commit request.
///
/// The core deliberately does not synthesize a per-file mutation loop when this operation
/// is not available. Mutations have replay semantics of `Never` until a durable host
/// invocation identity is carried through the tool context; a provider tool
/// call ID alone is not safe to use as an idempotency key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditTransaction {
    /// All file replacements that must be conditionally committed together.
    pub files: Vec<ConditionalFileEdit>,
    /// All files that must remain absent until the transaction publishes.
    pub creates: Vec<ConditionalFileCreate>,
}

/// Receipt from exactly one host-side conditional transaction operation.
///
/// `Committed` means the adapter accepted the complete request. `RolledBack`
/// means no requested replacement remains visible after an adapter-observed
/// failure. `Indeterminate` means the adapter cannot honestly establish either
/// condition; callers must inspect/reconcile before retrying. Neither this
/// enum nor the local adapter claims crash-atomic multi-file replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditTransactionOutcome {
    /// The adapter reports the complete replacement set committed.
    Committed,
    /// The adapter reports no requested replacement remains visible.
    RolledBack {
        /// Stable, bounded explanation suitable for the model.
        reason: String,
    },
    /// The final filesystem state may contain a subset of the request.
    Indeterminate {
        /// Stable, bounded explanation and recovery direction.
        reason: String,
    },
}

/// Explicit environment policy for trusted process-capability calls.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandEnvironment {
    variables: Vec<(OsString, OsString)>,
}

impl CommandEnvironment {
    /// Create an empty environment.  This is the default and is deterministic.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Copy the current process environment explicitly.
    ///
    /// Calling this method is an intentional authority decision by the embedding;
    /// the default coding profile never calls it implicitly.
    pub fn inherited() -> Self {
        Self {
            variables: std::env::vars_os().collect(),
        }
    }

    /// Add or replace one environment variable.
    pub fn with(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let name = name.into();
        if let Some((_, current)) = self.variables.iter_mut().find(|(key, _)| *key == name) {
            *current = value.into();
        } else {
            self.variables.push((name, value.into()));
        }
        self
    }

    /// Apply this explicit environment policy to a command before spawning it.
    pub fn apply(&self, command: &mut Command) {
        command.env_clear();
        command.envs(self.variables.iter().map(|(key, value)| (key, value)));
    }
}

/// Explicit host operations used by the coding capabilities.
///
/// Every path has already been checked against the [`WorkspaceRoot`] before it
/// reaches this boundary.  An adapter may therefore map the path to a remote
/// namespace, while retaining the same tool schemas and result semantics.
pub trait CodingOperations: Send + Sync {
    /// Read all bytes from one file.
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>>;
    /// Read complete snapshots for one edit plan through one host operation.
    ///
    /// The compatibility implementation is deliberately sequential, but the mutation capability
    /// calls this method once so remote adapters can batch or pipeline all
    /// snapshots. Adapters should override it when crossing an RPC/runtime
    /// boundary, must inspect kind before opening each path, and must reject a
    /// response whose complete contents exceed `max_total_bytes`.
    fn read_file_snapshots<'a>(
        &'a self,
        paths: &'a [PathBuf],
        max_total_bytes: usize,
    ) -> OperationFuture<'a, Vec<FileSnapshot>> {
        Box::pin(async move {
            let mut snapshots = Vec::with_capacity(paths.len());
            let mut total_bytes = 0_usize;
            for path in paths {
                let metadata = self.metadata(path).await?;
                let content = if !metadata.is_regular_file {
                    Vec::new()
                } else {
                    self.read_file(path).await?
                };
                total_bytes = total_bytes.saturating_add(content.len());
                if total_bytes > max_total_bytes {
                    return Err(OperationError::new(format!(
                        "complete edit snapshots exceed the {max_total_bytes} byte transaction limit",
                    )));
                }
                snapshots.push(FileSnapshot {
                    path: path.clone(),
                    is_regular_file: metadata.is_regular_file,
                    content,
                });
            }
            Ok(snapshots)
        })
    }
    /// Conditionally commit a complete multi-file edit transaction.
    ///
    /// This is intentionally one host operation. The capability never falls
    /// back to repeatedly publishing files, because that would make its
    /// all-file precondition and recovery contract false for remote hosts.
    ///
    /// A cancellation observed before the adapter's commit point must leave all
    /// files untouched and return `OperationError("cancelled")`. Once the
    /// adapter has requested commit it must settle with one receipt instead of
    /// treating future cancellation as permission to discard the result.
    fn commit_edit_transaction<'a>(
        &'a self,
        _transaction: &'a EditTransaction,
        _cancellation: CancellationToken,
    ) -> OperationFuture<'a, EditTransactionOutcome> {
        Box::pin(async {
            Err(OperationError::new(
                "conditional edit transactions are not supported by this coding-operations adapter",
            ))
        })
    }
    /// Inspect one path.
    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata>;
    /// Find paths below `root` using a glob pattern.
    fn find_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        limit: usize,
    ) -> OperationFuture<'a, Vec<String>>;
    /// Execute one command in the explicit workspace.
    fn execute_command<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout_seconds: Option<f64>,
        environment: &'a CommandEnvironment,
        cancellation: CancellationToken,
        updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput>;
}
