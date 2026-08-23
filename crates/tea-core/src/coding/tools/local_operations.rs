//! Local filesystem and shell implementation of the coding operation contract.

use super::contract::{
    CodingOperations, CommandEnvironment, CommandOutput, ConditionalFileEdit, DirectoryEntry,
    EditTransaction, EditTransactionOutcome, EntryMetadata, FileSnapshot, GrepMatch, GrepOptions,
    OperationError, OperationFuture,
};
use super::search::{GlobMatcher, local_grep, walk_files};
use crate::scheduler::CancellationToken;
use crate::tool::{ToolUpdate, ToolUpdateSink};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

static COMMAND_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static EDIT_TRANSACTION_STAGE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Local filesystem publication is serialized across adapter instances. This
/// closes the check/publish race for the default host; remote adapters must
/// provide an equivalent transaction boundary themselves.
static LOCAL_EDIT_TRANSACTION_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

/// Standard local filesystem/process implementation.
///
/// Each blocking operation starts on a standard-library worker thread when
/// its future is first polled. This keeps the public default composition
/// executor-neutral while allowing independent parallel tool calls to begin
/// together. Callers that need a bounded/shared blocking pool can still
/// provide their own [`CodingOperations`] through `with_operations`.
#[derive(Clone, Debug)]
pub struct LocalCodingOperations;

struct BlockingOperationState<T> {
    result: Option<Result<T, OperationError>>,
    waker: Option<Waker>,
    work: Option<Box<dyn FnOnce() -> Result<T, OperationError> + Send>>,
}

struct BlockingOperation<T> {
    state: Arc<Mutex<BlockingOperationState<T>>>,
}

impl<T: Send + 'static> Future for BlockingOperation<T> {
    type Output = Result<T, OperationError>;

    fn poll(self: std::pin::Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let work = {
            let mut state = self
                .state
                .lock()
                .expect("blocking operation state mutex poisoned");
            if let Some(result) = state.result.take() {
                return Poll::Ready(result);
            }
            state.waker = Some(context.waker().clone());
            state.work.take()
        };
        if let Some(work) = work {
            let worker_state = Arc::clone(&self.state);
            let spawned = std::thread::Builder::new()
                .name("tea-coding-operation".into())
                .spawn(move || {
                    let result = work();
                    let waker = {
                        let mut state = worker_state
                            .lock()
                            .expect("blocking operation state mutex poisoned");
                        state.result = Some(result);
                        state.waker.take()
                    };
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                });
            if let Err(error) = spawned {
                let waker = {
                    let mut state = self
                        .state
                        .lock()
                        .expect("blocking operation state mutex poisoned");
                    state.result = Some(Err(OperationError::new(format!(
                        "cannot start local coding operation: {error}",
                    ))));
                    state.waker.take()
                };
                if let Some(waker) = waker {
                    waker.wake();
                }
            }
        }
        Poll::Pending
    }
}

fn blocking_operation<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, OperationError> + Send + 'static,
) -> BlockingOperation<T> {
    let state = Arc::new(Mutex::new(BlockingOperationState {
        result: None,
        waker: None,
        work: Some(Box::new(work)),
    }));
    BlockingOperation { state }
}

impl CodingOperations for LocalCodingOperations {
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        let path = path.to_path_buf();
        Box::pin(blocking_operation(move || {
            std::fs::read(path).map_err(|error| OperationError::new(error.to_string()))
        }))
    }

    fn write_file<'a>(&'a self, path: &'a Path, content: &'a [u8]) -> OperationFuture<'a, ()> {
        let path = path.to_path_buf();
        let content = content.to_vec();
        Box::pin(blocking_operation(move || {
            std::fs::write(path, content).map_err(|error| OperationError::new(error.to_string()))
        }))
    }

    fn commit_edit_transaction<'a>(
        &'a self,
        transaction: &'a EditTransaction,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, EditTransactionOutcome> {
        let transaction = transaction.clone();
        Box::pin(blocking_operation(move || {
            commit_local_edit_transaction(&transaction, &cancellation)
        }))
    }

    fn create_dir_all<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, ()> {
        let path = path.to_path_buf();
        Box::pin(blocking_operation(move || {
            std::fs::create_dir_all(path).map_err(|error| OperationError::new(error.to_string()))
        }))
    }

    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        let path = path.to_path_buf();
        Box::pin(blocking_operation(move || {
            let metadata =
                std::fs::metadata(path).map_err(|error| OperationError::new(error.to_string()))?;
            Ok(EntryMetadata {
                is_directory: metadata.is_dir(),
                is_regular_file: metadata.is_file(),
            })
        }))
    }

    fn read_file_snapshots<'a>(
        &'a self,
        paths: &'a [PathBuf],
        max_total_bytes: usize,
    ) -> OperationFuture<'a, Vec<FileSnapshot>> {
        let paths = paths.to_vec();
        Box::pin(blocking_operation(move || {
            let mut snapshots = Vec::with_capacity(paths.len());
            let mut declared_total_bytes = 0_usize;
            let mut returned_total_bytes = 0_usize;
            for path in paths {
                let metadata =
                    fs::metadata(&path).map_err(|error| OperationError::new(error.to_string()))?;
                let is_regular_file = metadata.is_file();
                if is_regular_file {
                    declared_total_bytes = declared_total_bytes
                        .saturating_add(usize::try_from(metadata.len()).unwrap_or(usize::MAX));
                    if declared_total_bytes > max_total_bytes {
                        return Err(OperationError::new(format!(
                            "complete edit snapshots exceed the {max_total_bytes} byte transaction limit",
                        )));
                    }
                }
                let content = if is_regular_file {
                    fs::read(&path).map_err(|error| OperationError::new(error.to_string()))?
                } else {
                    Vec::new()
                };
                returned_total_bytes = returned_total_bytes.saturating_add(content.len());
                if returned_total_bytes > max_total_bytes {
                    return Err(OperationError::new(format!(
                        "complete edit snapshots exceed the {max_total_bytes} byte transaction limit",
                    )));
                }
                snapshots.push(FileSnapshot {
                    path: path.clone(),
                    is_regular_file,
                    content,
                });
            }
            Ok(snapshots)
        }))
    }

    fn read_dir<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<DirectoryEntry>> {
        let path = path.to_path_buf();
        Box::pin(blocking_operation(move || {
            let mut entries = Vec::new();
            for entry in
                std::fs::read_dir(path).map_err(|error| OperationError::new(error.to_string()))?
            {
                let entry = entry.map_err(|error| OperationError::new(error.to_string()))?;
                let metadata = entry
                    .metadata()
                    .map_err(|error| OperationError::new(error.to_string()))?;
                entries.push(DirectoryEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_directory: metadata.is_dir(),
                });
            }
            Ok(entries)
        }))
    }

    fn find_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        limit: usize,
    ) -> OperationFuture<'a, Vec<String>> {
        let root = root.to_path_buf();
        let pattern = pattern.to_owned();
        Box::pin(blocking_operation(move || {
            let matcher = GlobMatcher::new(&pattern)?;
            let mut output = Vec::new();
            walk_files(&root, &root, &matcher, limit, &mut output)?;
            output.sort();
            Ok(output)
        }))
    }

    fn grep_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        options: GrepOptions,
    ) -> OperationFuture<'a, Vec<GrepMatch>> {
        let root = root.to_path_buf();
        let pattern = pattern.to_owned();
        Box::pin(blocking_operation(move || {
            local_grep(&root, &pattern, options)
        }))
    }

    fn execute_command<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout_seconds: Option<f64>,
        environment: &'a CommandEnvironment,
        cancellation: CancellationToken,
        updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        let command = command.to_owned();
        let cwd = cwd.to_path_buf();
        let environment = environment.clone();
        Box::pin(blocking_operation(move || {
            execute_local_command(
                &command,
                &cwd,
                timeout_seconds,
                &environment,
                &cancellation,
                updates,
            )
        }))
    }
}

/// Apply the local adapter's limited transaction protocol.
///
/// It provides validation atomicity: every conditional preimage is re-read and
/// compared before any requested file is written. After that commit point,
/// ordinary filesystem writes can fail or a process can crash between files;
/// this implementation attempts rollback on an ordinary write error and emits
/// `Indeterminate` if that rollback cannot be established. It does not claim
/// crash atomicity, cross-process locking, or durable recovery receipts.
fn commit_local_edit_transaction(
    transaction: &EditTransaction,
    cancellation: &CancellationToken,
) -> Result<EditTransactionOutcome, OperationError> {
    if cancellation.is_cancelled() {
        return Err(OperationError::new("cancelled"));
    }
    if transaction.files.is_empty() {
        return Ok(EditTransactionOutcome::RolledBack {
            reason: "transaction contains no files".into(),
        });
    }
    let _transaction_guard = LOCAL_EDIT_TRANSACTION_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("local edit transaction guard mutex poisoned");
    // This is the complete precondition phase. No target mutation can occur
    // before all files are observed unchanged and cancellation is checked.
    for edit in &transaction.files {
        let metadata =
            fs::metadata(&edit.path).map_err(|error| OperationError::new(error.to_string()))?;
        if !metadata.is_file() {
            let outcome = EditTransactionOutcome::RolledBack {
                reason:
                    "a requested path is no longer an ordinary regular file; no files were written"
                        .into(),
            };
            return Ok(outcome);
        }
        let current =
            fs::read(&edit.path).map_err(|error| OperationError::new(error.to_string()))?;
        if current != edit.expected_content {
            let outcome = EditTransactionOutcome::RolledBack {
                reason: "a file changed after its original snapshot; no files were written".into(),
            };
            return Ok(outcome);
        }
    }
    if cancellation.is_cancelled() {
        return Err(OperationError::new("cancelled"));
    }

    // Stage both publications and exact rollback bytes before the first target
    // mutation. Unix publication uses same-directory rename replacement;
    // platforms without replace-on-rename may briefly remove one pathname.
    // The transaction does not claim a globally atomic multi-file view.
    let mut staged = Vec::with_capacity(transaction.files.len());
    for edit in &transaction.files {
        match stage_local_edit(edit) {
            Ok(value) => staged.push(value),
            Err(error) => {
                cleanup_staged(&staged);
                let outcome = EditTransactionOutcome::RolledBack {
                    reason: format!(
                        "could not stage the transaction; no target files were changed: {}",
                        crate::tool::truncate_middle(error.message(), 256),
                    ),
                };
                return Ok(outcome);
            }
        }
    }
    if cancellation.is_cancelled() {
        cleanup_staged(&staged);
        return Err(OperationError::new("cancelled"));
    }

    // Commit has now been requested. Do not turn a later cancellation into an
    // untruthful cancelled result: settle a receipt after publication.
    for index in 0..staged.len() {
        if let Err(error) =
            publish_staged_file(&staged[index].replacement_path, &staged[index].edit.path)
        {
            // Restore every target from a separately staged original, including
            // the currently failing path. `rename` should not partially mutate
            // a path on error, but recovery must not rely on that assumption.
            let mut rollback_ok = true;
            for entry in staged.iter().rev() {
                if publish_staged_file(&entry.rollback_path, &entry.edit.path).is_err() {
                    rollback_ok = false;
                }
            }
            cleanup_staged(&staged);
            let outcome = if rollback_ok {
                EditTransactionOutcome::RolledBack {
                    reason: format!(
                        "a staged local publication failed and every published file was restored: {}",
                        crate::tool::truncate_middle(&error.to_string(), 256),
                    ),
                }
            } else {
                EditTransactionOutcome::Indeterminate {
                    reason: "a staged local publication failed and rollback could not be verified; inspect every requested file before retrying".into(),
                }
            };
            return Ok(outcome);
        }
    }
    cleanup_staged(&staged);
    Ok(EditTransactionOutcome::Committed)
}

struct StagedLocalEdit<'a> {
    edit: &'a ConditionalFileEdit,
    replacement_path: PathBuf,
    rollback_path: PathBuf,
}

fn stage_local_edit(edit: &ConditionalFileEdit) -> Result<StagedLocalEdit<'_>, OperationError> {
    let permissions = fs::metadata(&edit.path)
        .map_err(|error| OperationError::new(error.to_string()))?
        .permissions();
    let replacement_path = stage_local_file(
        &edit.path,
        "replacement",
        &edit.replacement_content,
        &permissions,
    )?;
    let rollback_path =
        match stage_local_file(&edit.path, "rollback", &edit.expected_content, &permissions) {
            Ok(path) => path,
            Err(error) => {
                let _ = fs::remove_file(&replacement_path);
                return Err(error);
            }
        };
    Ok(StagedLocalEdit {
        edit,
        replacement_path,
        rollback_path,
    })
}

fn stage_local_file(
    target: &Path,
    kind: &str,
    bytes: &[u8],
    permissions: &fs::Permissions,
) -> Result<PathBuf, OperationError> {
    let parent = target
        .parent()
        .ok_or_else(|| OperationError::new("target has no parent directory"))?;
    let base = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("edit-target");
    for _ in 0..32 {
        let sequence = EDIT_TRANSACTION_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".tea-edit-v2-{}-{}-{}-{}",
            std::process::id(),
            sequence,
            kind,
            base
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = match options.open(&candidate) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(OperationError::new(error.to_string())),
        };
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&candidate);
            return Err(OperationError::new(error.to_string()));
        }
        if let Err(error) = fs::set_permissions(&candidate, permissions.clone()) {
            let _ = fs::remove_file(&candidate);
            return Err(OperationError::new(error.to_string()));
        }
        return Ok(candidate);
    }
    Err(OperationError::new(
        "cannot allocate a unique staged edit file after 32 attempts",
    ))
}

fn cleanup_staged(staged: &[StagedLocalEdit<'_>]) {
    for entry in staged {
        let _ = fs::remove_file(&entry.replacement_path);
        let _ = fs::remove_file(&entry.rollback_path);
    }
}

fn publish_staged_file(staged: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if let Err(error) = fs::remove_file(target)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error);
        }
    }
    fs::rename(staged, target)
}

/// Execute the local shell through the caller-owned tool future.
///
/// The child is deliberately the shell only. On cancellation it is reaped so
/// the tool future settles, but detached descendants are not killed: a host
/// may intentionally start a durable worker in the explicit workspace.
fn execute_local_command(
    command: &str,
    cwd: &Path,
    timeout_seconds: Option<f64>,
    environment: &CommandEnvironment,
    cancellation: &CancellationToken,
    updates: ToolUpdateSink,
) -> Result<CommandOutput, OperationError> {
    if cancellation.is_cancelled() {
        return Err(OperationError::new("cancelled"));
    }
    let (stdout_path, stdout) = command_capture_file("stdout")?;
    let (stderr_path, stderr) = match command_capture_file("stderr") {
        Ok(capture) => capture,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            return Err(error);
        }
    };
    let mut process = Command::new("bash");
    process.arg("-c").arg(command).current_dir(cwd);
    environment.apply(&mut process);
    // Capture to private files rather than pipes.  A command such as
    // `long_task &` can legitimately leave descendants running; those
    // descendants inherit pipes from `Command::output`, which makes
    // the caller wait for the background task rather than its shell.
    // Files preserve foreground output while we wait only for the shell
    // that this tool actually started.
    process
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Err(OperationError::new(error.to_string()));
        }
    };
    let status = wait_for_shell_or_cancellation(&mut child, cancellation)?;
    let stdout = read_command_capture(&stdout_path);
    let stderr = read_command_capture(&stderr_path);
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);
    let stdout = stdout?;
    let stderr = stderr?;
    if cancellation.is_cancelled() {
        return Err(OperationError::new("cancelled"));
    }
    if let Some(timeout) = timeout_seconds {
        // Validation happens at the tool boundary. Retaining this branch
        // documents that the local adapter does not claim to enforce a tool
        // timeout after a blocking child has started.
        let _ = timeout;
    }
    let mut update = Vec::new();
    update.extend_from_slice(&stdout);
    update.extend_from_slice(&stderr);
    if !update.is_empty() {
        updates.emit(ToolUpdate {
            content: String::from_utf8_lossy(&update).into_owned(),
            details: None,
        });
    }
    Ok(CommandOutput {
        exit_code: status.code(),
        stdout,
        stderr,
    })
}

fn wait_for_shell_or_cancellation(
    child: &mut Child,
    cancellation: &CancellationToken,
) -> Result<ExitStatus, OperationError> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| OperationError::new(error.to_string()))?
        {
            return Ok(status);
        }
        if cancellation.is_cancelled() {
            // `kill` reports InvalidInput when the shell won the race and
            // exited after `try_wait`; `wait` still reaps that shell and gives
            // the caller one deterministic settlement path.
            if let Err(error) = child.kill()
                && error.kind() != std::io::ErrorKind::InvalidInput
            {
                return Err(OperationError::new(error.to_string()));
            }
            return child
                .wait()
                .map_err(|error| OperationError::new(error.to_string()));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Create a private output capture that is safe for commands that daemonize.
fn command_capture_file(stream: &str) -> Result<(PathBuf, File), OperationError> {
    for _ in 0..16 {
        let sequence = COMMAND_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tea-core-command-{}-{}-{stream}",
            std::process::id(),
            sequence,
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(OperationError::new(format!(
                    "cannot create command capture: {error}"
                )));
            }
        }
    }
    Err(OperationError::new(
        "cannot allocate a unique command capture after 16 attempts",
    ))
}

fn read_command_capture(path: &Path) -> Result<Vec<u8>, OperationError> {
    let mut file = File::open(path)
        .map_err(|error| OperationError::new(format!("cannot read command capture: {error}")))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| OperationError::new(format!("cannot read command capture: {error}")))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tea-core-local-operations-{}",
            COMMAND_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("workspace creates");
        path
    }

    #[test]
    fn default_local_operations_begin_parallel_commands_before_either_completes() {
        let root = workspace();
        struct ReleaseGuard {
            root: PathBuf,
        }
        impl Drop for ReleaseGuard {
            fn drop(&mut self) {
                let _ = fs::write(self.root.join("first.release"), b"");
                let _ = fs::write(self.root.join("second.release"), b"");
            }
        }
        let release_guard = ReleaseGuard { root: root.clone() };
        let first_root = root.clone();
        let second_root = root.clone();
        let first = smol::spawn(async move {
            LocalCodingOperations
                .execute_command(
                    "touch first.started; while [ ! -e first.release ]; do sleep 0.01; done; printf first-done",
                    &first_root,
                    None,
                    &CommandEnvironment::empty(),
                    CancellationToken::new(),
                    ToolUpdateSink::disabled(),
                )
                .await
        });
        let second = smol::spawn(async move {
            LocalCodingOperations
                .execute_command(
                    "touch second.started; while [ ! -e second.release ]; do sleep 0.01; done; printf second-done",
                    &second_root,
                    None,
                    &CommandEnvironment::empty(),
                    CancellationToken::new(),
                    ToolUpdateSink::disabled(),
                )
                .await
        });
        let both_started = smol::block_on(async {
            for _ in 0..200 {
                if root.join("first.started").is_file() && root.join("second.started").is_file() {
                    return true;
                }
                smol::Timer::after(Duration::from_millis(10)).await;
            }
            false
        });
        fs::write(root.join("first.release"), b"").expect("first command releases");
        fs::write(root.join("second.release"), b"").expect("second command releases");
        let (first, second) = smol::block_on(async { smol::future::zip(first, second).await });
        assert!(
            both_started,
            "both commands must start before either is released"
        );
        assert_eq!(first.expect("first command").stdout, b"first-done");
        assert_eq!(second.expect("second command").stdout, b"second-done");
        drop(release_guard);
        let _ = fs::remove_dir_all(root);
    }
}
