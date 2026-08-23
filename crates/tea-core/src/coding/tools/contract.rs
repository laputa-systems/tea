//! Public operation contracts and value types for standard coding tools.

use crate::scheduler::CancellationToken;
use crate::tool::ToolUpdateSink;
use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
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

/// Metadata needed by the standard tools without exposing `std::fs::Metadata`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryMetadata {
    /// Whether the entry is a directory.
    pub is_directory: bool,
}

/// A directory entry returned by an operation adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    /// Entry name, not an ambient absolute path.
    pub name: String,
    /// Whether the entry is a directory.
    pub is_directory: bool,
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

/// Explicit environment policy for [`bash`](DefaultCodingTools::bash) calls.
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

    pub(crate) fn apply(&self, command: &mut Command) {
        command.env_clear();
        command.envs(self.variables.iter().map(|(key, value)| (key, value)));
    }
}

/// Explicit host operations used by all standard tools.
///
/// Every path has already been checked against the [`WorkspaceRoot`] before it
/// reaches this boundary.  An adapter may therefore map the path to a remote
/// namespace, while retaining the same tool schemas and result semantics.
pub trait CodingOperations: Send + Sync {
    /// Read all bytes from one file.
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>>;
    /// Write all bytes to one file.
    fn write_file<'a>(&'a self, path: &'a Path, content: &'a [u8]) -> OperationFuture<'a, ()>;
    /// Create a directory and all missing parents.
    fn create_dir_all<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, ()>;
    /// Inspect one path.
    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata>;
    /// List one directory.
    fn read_dir<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<DirectoryEntry>>;
    /// Find paths below `root` using a glob pattern.
    fn find_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        limit: usize,
    ) -> OperationFuture<'a, Vec<String>>;
    /// Search files below `root` for a pattern.
    fn grep_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        options: GrepOptions,
    ) -> OperationFuture<'a, Vec<GrepMatch>>;
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

/// Options passed to a grep operation adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrepOptions {
    /// Search case-insensitively.
    pub ignore_case: bool,
    /// Treat `pattern` literally rather than as the supported regex subset.
    pub literal: bool,
    /// Number of context lines on each side of a match.
    pub context: usize,
    /// Maximum number of matching lines.
    pub limit: usize,
    /// Optional basename/path glob filter.
    pub glob: Option<String>,
}

/// One grep result line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrepMatch {
    /// Path relative to the search root, using `/` separators.
    pub path: String,
    /// One-indexed source line.
    pub line: usize,
    /// Rendered matching line.
    pub text: String,
}
