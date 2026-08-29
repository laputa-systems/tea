//! Smol-backed coding operations owned by the terminal host.
//!
//! `tea-core` deliberately leaves filesystem and process scheduling to its
//! caller.  The terminal therefore moves synchronous local operations onto
//! Smol's blocking pool.  Bash retains the capture-file boundary used by the
//! core local adapter: descendants may inherit the files, but never hold the
//! terminal's executor or a pipe open.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tea_core::coding::{
    CodingOperations, CommandEnvironment, CommandOutput, EditTransaction, EditTransactionOutcome,
    EntryMetadata, FileSnapshot, LocalCodingOperations, OperationError, OperationFuture,
};
use tea_core::scheduler::CancellationToken;
use tea_core::tool::{ToolUpdate, ToolUpdateSink};

static COMMAND_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

const STREAM_CHUNK_BYTES: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Application-owned nonblocking implementation of the standard coding port.
#[derive(Clone, Debug, Default)]
pub(super) struct NonblockingCodingOperations;

impl CodingOperations for NonblockingCodingOperations {
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        let path = path.to_path_buf();
        Box::pin(smol::unblock(move || {
            fs::read(path).map_err(|error| OperationError::new(error.to_string()))
        }))
    }

    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        let path = path.to_path_buf();
        Box::pin(smol::unblock(move || {
            let metadata =
                fs::metadata(path).map_err(|error| OperationError::new(error.to_string()))?;
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
        Box::pin(smol::unblock(move || {
            let local = LocalCodingOperations;
            smol::block_on(local.read_file_snapshots(&paths, max_total_bytes))
        }))
    }

    fn commit_edit_transaction<'a>(
        &'a self,
        transaction: &'a EditTransaction,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, EditTransactionOutcome> {
        let transaction = transaction.clone();
        Box::pin(smol::unblock(move || {
            let local = LocalCodingOperations;
            smol::block_on(local.commit_edit_transaction(&transaction, cancellation))
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
        // Search semantics remain the core adapter's canonical implementation;
        // only its synchronous execution is moved off the terminal executor.
        Box::pin(smol::unblock(move || {
            let local = LocalCodingOperations;
            smol::block_on(local.find_files(&root, &pattern, limit))
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
        Box::pin(smol::unblock(move || {
            execute_command_blocking(
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

fn execute_command_blocking(
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
    let timeout = match timeout_seconds {
        Some(value) if value.is_finite() && value > 0.0 => Some(
            Duration::try_from_secs_f64(value)
                .map_err(|_| OperationError::new("invalid timeout"))?,
        ),
        Some(_) => return Err(OperationError::new("invalid timeout")),
        None => None,
    };
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
    process
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            remove_captures(&stdout_path, &stderr_path);
            return Err(OperationError::new(error.to_string()));
        }
    };

    let started = Instant::now();
    let mut stdout_offset = 0;
    let mut stderr_offset = 0;
    loop {
        if cancellation.is_cancelled() {
            let result = terminate_and_reap(&mut child);
            remove_captures(&stdout_path, &stderr_path);
            return result
                .map(|_| ())
                .and_then(|_| Err(OperationError::new("cancelled")));
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            let result = terminate_and_reap(&mut child);
            remove_captures(&stdout_path, &stderr_path);
            return result
                .map(|_| ())
                .and_then(|_| Err(OperationError::new("command timed out")));
        }

        if let Err(error) = emit_capture_updates(
            &stdout_path,
            &mut stdout_offset,
            &updates,
            STREAM_CHUNK_BYTES,
        ) {
            let _ = terminate_and_reap(&mut child);
            remove_captures(&stdout_path, &stderr_path);
            return Err(error);
        }
        if let Err(error) = emit_capture_updates(
            &stderr_path,
            &mut stderr_offset,
            &updates,
            STREAM_CHUNK_BYTES,
        ) {
            let _ = terminate_and_reap(&mut child);
            remove_captures(&stdout_path, &stderr_path);
            return Err(error);
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = terminate_and_reap(&mut child);
                remove_captures(&stdout_path, &stderr_path);
                return Err(OperationError::new(error.to_string()));
            }
        };
        if let Some(status) = status {
            let stdout = read_command_capture(&stdout_path);
            let stderr = read_command_capture(&stderr_path);
            remove_captures(&stdout_path, &stderr_path);
            if cancellation.is_cancelled() {
                return Err(OperationError::new("cancelled"));
            }
            return Ok(CommandOutput {
                exit_code: status.code(),
                stdout: stdout?,
                stderr: stderr?,
            });
        }
        // This sleep is on Smol's blocking pool, never in the future's poll;
        // the operation future remains available to drive other terminal work.
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn emit_capture_updates(
    path: &Path,
    offset: &mut u64,
    updates: &ToolUpdateSink,
    chunk_limit: usize,
) -> Result<(), OperationError> {
    // Updates are intentionally bounded lossy UTF-8 windows.  A multibyte
    // codepoint split across windows may be rendered with replacement text;
    // the complete, lossless command output remains authoritative in the
    // settled `CommandOutput` returned below.
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(OperationError::new(error.to_string())),
    };
    while *offset < length {
        let mut file = File::open(path).map_err(|error| OperationError::new(error.to_string()))?;
        file.seek(SeekFrom::Start(*offset))
            .map_err(|error| OperationError::new(error.to_string()))?;
        let remaining = (length - *offset) as usize;
        let mut bytes = vec![0; remaining.min(chunk_limit)];
        let read = file
            .read(&mut bytes)
            .map_err(|error| OperationError::new(error.to_string()))?;
        if read == 0 {
            break;
        }
        *offset += read as u64;
        updates.emit(ToolUpdate {
            content: String::from_utf8_lossy(&bytes[..read]).into_owned(),
            details: None,
        });
    }
    Ok(())
}

fn terminate_and_reap(child: &mut Child) -> Result<ExitStatus, OperationError> {
    if let Err(error) = child.kill() {
        if error.kind() != std::io::ErrorKind::InvalidInput {
            return Err(OperationError::new(error.to_string()));
        }
    }
    child
        .wait()
        .map_err(|error| OperationError::new(error.to_string()))
}

fn command_capture_file(stream: &str) -> Result<(PathBuf, File), OperationError> {
    for _ in 0..16 {
        let sequence = COMMAND_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tea-agent-command-{}-{}-{stream}",
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

fn remove_captures(stdout: &Path, stderr: &Path) {
    let _ = fs::remove_file(stdout);
    let _ = fs::remove_file(stderr);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tea_core::coding::{CodingHost, PROCESS_CAPABILITY_V1};
    use tea_core::effect::RunProvenance;
    use tea_core::harness::extension::ExtensionCapabilityRequest;
    use tea_core::state::ToolCallId;
    use tea_core::tool::ToolUpdateSink;
    use tea_protocol::JsonValue;

    fn workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tea-agent-nonblocking-{}",
            COMMAND_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("workspace creates");
        path
    }

    #[test]
    fn terminal_operations_overlap_on_the_blocking_pool() {
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
            let operations = NonblockingCodingOperations;
            let environment = CommandEnvironment::empty();
            operations
                .execute_command(
                    "touch first.started; while [ ! -e first.release ]; do sleep 0.01; done; printf first-done",
                    &first_root,
                    None,
                    &environment,
                    CancellationToken::new(),
                    ToolUpdateSink::disabled(),
                )
                .await
        });
        let second = smol::spawn(async move {
            let operations = NonblockingCodingOperations;
            let environment = CommandEnvironment::empty();
            operations
                .execute_command(
                    "touch second.started; while [ ! -e second.release ]; do sleep 0.01; done; printf second-done",
                    &second_root,
                    None,
                    &environment,
                    CancellationToken::new(),
                    ToolUpdateSink::disabled(),
                )
                .await
        });
        // Both started markers must be observable before either command is
        // released; a serial adapter cannot satisfy this barrier. The bounded
        // watchdog ensures a broken implementation cannot hang this test.
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

    #[test]
    fn bash_streams_an_update_before_the_tool_finishes() {
        let root = workspace();
        let host =
            CodingHost::with_operations(&root, std::sync::Arc::new(NonblockingCodingOperations))
                .expect("host configures");
        let capability = host.process_capability();
        let (sender, receiver) = smol::channel::bounded(1);
        let updates = ToolUpdateSink::new(move |update| {
            let _ = sender.try_send(update);
        });
        let request = ExtensionCapabilityRequest {
            call_id: ToolCallId::new("stream").expect("call ID"),
            tool_name: "bash".into(),
            provenance: RunProvenance::default(),
            capability: PROCESS_CAPABILITY_V1.into(),
            method: "run".into(),
            arguments: JsonValue::object([(
                "command",
                JsonValue::String("printf first; sleep 0.2; printf second".into()),
            )]),
            updates,
        };
        let task =
            smol::spawn(async move { capability.invoke(request, CancellationToken::new()).await });
        let update = smol::block_on(async {
            smol::future::race(receiver.recv(), async {
                smol::Timer::after(Duration::from_secs(1)).await;
                Err(smol::channel::RecvError)
            })
            .await
        })
        .expect("streamed update arrives");
        assert!(update.content.contains("first"));
        let result = smol::block_on(task).expect("bash settles");
        let content = result
            .value
            .get("content")
            .and_then(JsonValue::as_str)
            .expect("process result includes content");
        assert!(content.contains("first"));
        assert!(content.contains("second"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_kills_and_reaps_the_shell_promptly() {
        let root = workspace();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_root = root.clone();
        let task = smol::spawn(async move {
            let operations = NonblockingCodingOperations;
            let environment = CommandEnvironment::empty();
            operations
                .execute_command(
                    "sleep 30",
                    &task_root,
                    None,
                    &environment,
                    task_cancellation,
                    ToolUpdateSink::disabled(),
                )
                .await
        });
        smol::block_on(smol::Timer::after(Duration::from_millis(50)));
        let started = Instant::now();
        cancellation.cancel();
        let result = smol::block_on(task);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(
            result.expect_err("cancellation should fail"),
            OperationError::new("cancelled")
        );
        let _ = fs::remove_dir_all(root);
    }
}
