//! Smol-backed scheduling for the terminal's coding operations.
//!
//! Process lifecycle belongs to Tea core's canonical local runner. The terminal
//! only chooses Smol's blocking pool for that runner and keeps no independent
//! timeout, cancellation, capture, or cleanup algorithm.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tea_core::coding::{
    CodingOperations, CommandEnvironment, CommandOutput, EditTransaction, EditTransactionOutcome,
    EntryMetadata, FileSnapshot, LocalCodingOperations, OperationError, OperationFuture,
    SearchResult, run_local_command,
};
use tea_core::scheduler::CancellationToken;
use tea_core::tool::ToolUpdateSink;

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
        max_results: usize,
        max_output_bytes: usize,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, SearchResult> {
        let root = root.to_path_buf();
        let pattern = pattern.to_owned();
        let cancellation = cancellation.clone();
        // Search semantics remain the core adapter's canonical implementation;
        // only its synchronous execution is moved off the terminal executor.
        Box::pin(smol::unblock(move || {
            let local = LocalCodingOperations;
            smol::block_on(local.find_files(
                &root,
                &pattern,
                max_results,
                max_output_bytes,
                cancellation,
            ))
        }))
    }

    fn execute_command<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        timeout: Duration,
        environment: &'a CommandEnvironment,
        cancellation: CancellationToken,
        updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        let command = command.to_owned();
        let cwd = cwd.to_path_buf();
        let environment = environment.clone();
        Box::pin(smol::unblock(move || {
            run_local_command(
                &command,
                &cwd,
                timeout,
                &environment,
                &cancellation,
                updates,
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    use tea_core::coding::{CodingHost, PROCESS_CAPABILITY_V1};
    use tea_core::effect::RunProvenance;
    use tea_core::harness::extension::ExtensionCapabilityRequest;
    use tea_core::state::ToolCallId;
    use tea_core::tool::ToolUpdateSink;
    use tea_protocol::JsonValue;

    static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);

    fn workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tea-agent-nonblocking-{}",
            NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed)
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
                    Duration::from_secs(300),
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
                    Duration::from_secs(300),
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
    fn cancellation_settles_the_owned_process_scope_promptly() {
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
                    Duration::from_secs(300),
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
        assert!(matches!(
            result.expect("process cancellation settles a receipt").termination,
            tea_core::coding::CommandTermination::Cancelled
        ));
        let _ = fs::remove_dir_all(root);
    }
}
