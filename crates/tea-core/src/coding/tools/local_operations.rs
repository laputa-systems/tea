//! Local filesystem and shell implementation of the coding operation contract.

use super::contract::{
    CodingOperations, CommandEnvironment, CommandOutput, DirectoryEntry, EntryMetadata, GrepMatch,
    GrepOptions, OperationError, OperationFuture,
};
use super::search::{GlobMatcher, local_grep, walk_files};
use crate::scheduler::CancellationToken;
use crate::tool::{ToolUpdate, ToolUpdateSink};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static COMMAND_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Standard local filesystem/process implementation.
#[derive(Clone, Debug)]
pub struct LocalCodingOperations;

impl CodingOperations for LocalCodingOperations {
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        Box::pin(async move {
            std::fs::read(path).map_err(|error| OperationError::new(error.to_string()))
        })
    }

    fn write_file<'a>(&'a self, path: &'a Path, content: &'a [u8]) -> OperationFuture<'a, ()> {
        Box::pin(async move {
            std::fs::write(path, content).map_err(|error| OperationError::new(error.to_string()))
        })
    }

    fn create_dir_all<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, ()> {
        Box::pin(async move {
            std::fs::create_dir_all(path).map_err(|error| OperationError::new(error.to_string()))
        })
    }

    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        Box::pin(async move {
            let metadata =
                std::fs::metadata(path).map_err(|error| OperationError::new(error.to_string()))?;
            Ok(EntryMetadata {
                is_directory: metadata.is_dir(),
            })
        })
    }

    fn read_dir<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<DirectoryEntry>> {
        Box::pin(async move {
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
        })
    }

    fn find_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        limit: usize,
    ) -> OperationFuture<'a, Vec<String>> {
        Box::pin(async move {
            let matcher = GlobMatcher::new(pattern)?;
            let mut output = Vec::new();
            walk_files(root, root, &matcher, limit, &mut output)?;
            output.sort();
            Ok(output)
        })
    }

    fn grep_files<'a>(
        &'a self,
        root: &'a Path,
        pattern: &'a str,
        options: GrepOptions,
    ) -> OperationFuture<'a, Vec<GrepMatch>> {
        Box::pin(async move { local_grep(root, pattern, options) })
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
        Box::pin(async move {
            execute_local_command(
                command,
                cwd,
                timeout_seconds,
                environment,
                &cancellation,
                updates,
            )
        })
    }
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
                && error.kind() != std::io::ErrorKind::InvalidInput {
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
