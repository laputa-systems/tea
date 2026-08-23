use super::*;
use crate::error::ToolError;
use crate::scheduler::CancellationToken;
use crate::tool::{ToolCall, ToolContext, ToolUpdateSink};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::{SerializedJson, ToolCallId};

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default)]
struct BatchProbeOperations {
    snapshot_calls: AtomicUsize,
    commit_calls: AtomicUsize,
    committed: Mutex<Option<EditTransaction>>,
}

impl BatchProbeOperations {
    fn fail<'a, T: Send + 'a>(&'a self, operation: &str) -> OperationFuture<'a, T> {
        Box::pin(std::future::ready(Err(OperationError::new(format!(
            "unexpected host operation: {operation}",
        )))))
    }
}

impl CodingOperations for BatchProbeOperations {
    fn read_file<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        self.fail("read_file")
    }

    fn read_file_snapshots<'a>(
        &'a self,
        paths: &'a [PathBuf],
        max_total_bytes: usize,
    ) -> OperationFuture<'a, Vec<FileSnapshot>> {
        self.snapshot_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let mut total = 0_usize;
            let mut snapshots = Vec::with_capacity(paths.len());
            for path in paths {
                let content = fs::read(path).map_err(|error| OperationError::new(error.to_string()))?;
                total = total.saturating_add(content.len());
                if total > max_total_bytes {
                    return Err(OperationError::new("snapshot limit exceeded"));
                }
                snapshots.push(FileSnapshot {
                    path: path.clone(),
                    is_regular_file: true,
                    content,
                });
            }
            Ok(snapshots)
        })
    }

    fn write_file<'a>(&'a self, _path: &'a Path, _content: &'a [u8]) -> OperationFuture<'a, ()> {
        self.fail("write_file")
    }

    fn commit_edit_transaction<'a>(
        &'a self,
        transaction: &'a EditTransaction,
        _cancellation: CancellationToken,
    ) -> OperationFuture<'a, EditTransactionOutcome> {
        self.commit_calls.fetch_add(1, Ordering::Relaxed);
        *self.committed.lock().expect("committed transaction mutex") = Some(transaction.clone());
        Box::pin(std::future::ready(Ok(EditTransactionOutcome::Committed)))
    }

    fn create_dir_all<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, ()> {
        self.fail("create_dir_all")
    }

    fn metadata<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        self.fail("metadata")
    }

    fn read_dir<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, Vec<DirectoryEntry>> {
        self.fail("read_dir")
    }

    fn find_files<'a>(
        &'a self,
        _root: &'a Path,
        _pattern: &'a str,
        _limit: usize,
    ) -> OperationFuture<'a, Vec<String>> {
        self.fail("find_files")
    }

    fn grep_files<'a>(
        &'a self,
        _root: &'a Path,
        _pattern: &'a str,
        _options: GrepOptions,
    ) -> OperationFuture<'a, Vec<GrepMatch>> {
        self.fail("grep_files")
    }

    fn execute_command<'a>(
        &'a self,
        _command: &'a str,
        _cwd: &'a Path,
        _timeout_seconds: Option<f64>,
        _environment: &'a CommandEnvironment,
        _cancellation: CancellationToken,
        _updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        self.fail("execute_command")
    }
}

fn workspace() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pi-default-tools-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(name).unwrap(),
        name: name.into(),
        arguments: SerializedJson::new(arguments),
    }
}

fn context() -> ToolContext {
    ToolContext {
        cancellation: CancellationToken::new(),
        metadata: None,
    }
}

#[test]
fn workspace_rejects_escape_and_symlink_escape() {
    let root = workspace();
    let outside = workspace();
    fs::write(outside.join("secret.txt"), "secret").unwrap();
    let tools = DefaultCodingTools::new(&root).unwrap();
    assert!(tools.workspace().resolve_existing("../secret.txt").is_err());
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    #[cfg(unix)]
    assert!(
        tools
            .workspace()
            .resolve_existing("link/secret.txt")
            .is_err()
    );
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn separate_default_toolsets_cannot_cross_workspace_authority() {
    let first = workspace();
    let second = workspace();
    let first_tools = DefaultCodingTools::new(&first).unwrap();
    let second_tools = DefaultCodingTools::new(&second).unwrap();
    let first_write = smol::block_on(first_tools.write().execute(
        call("write", r#"{"path":"owned.txt","content":"first"}"#),
        context(),
        ToolUpdateSink::disabled(),
    ));
    let second_write = smol::block_on(second_tools.write().execute(
        call("write", r#"{"path":"owned.txt","content":"second"}"#),
        context(),
        ToolUpdateSink::disabled(),
    ));
    assert!(first_write.is_ok());
    assert!(second_write.is_ok());

    let escape_path = second_tools.workspace().as_path().to_string_lossy();
    let escaped = smol::block_on(first_tools.write().execute(
        call(
            "write",
            &format!(r#"{{"path":"{escape_path}/escaped.txt","content":"no"}}"#),
        ),
        context(),
        ToolUpdateSink::disabled(),
    ));
    assert!(matches!(
        escaped,
        Err(ToolError::Execution { tool, .. }) if tool == "write"
    ));
    assert_eq!(
        fs::read_to_string(first.join("owned.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(second.join("owned.txt")).unwrap(),
        "second"
    );
    assert!(!second.join("escaped.txt").exists());
    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
fn default_tools_are_ordered_and_writable() {
    let root = workspace();
    let tools = DefaultCodingTools::new(&root).unwrap();
    let captured = crate::coding::PiDefaultCodingProfile::pinned_default().unwrap();
    let executable_definitions = tools
        .all_tools()
        .iter()
        .map(|tool| crate::tool::ToolDefinition::from_tool(tool.as_ref()))
        .collect::<Vec<_>>();
    for (executable, pinned) in executable_definitions
        .iter()
        .zip(captured.standard_tool_definitions())
    {
        assert_eq!(executable.name, pinned.name);
        assert_eq!(executable.description, pinned.description);
        assert_eq!(
            executable.schema.to_json_string().unwrap(),
            pinned.schema.to_json_string().unwrap(),
            "schema differs for tool {}",
            executable.name
        );
    }
    assert_eq!(
        executable_definitions,
        captured.standard_tool_definitions(),
        "the executable profile must expose the capture's exact names, descriptions, schemas, and order"
    );
    assert_eq!(
        tools.registry().names().collect::<Vec<_>>(),
        vec!["read", "bash", "edit", "write"]
    );
    let write = tools.write();
    smol::block_on(write.execute(
        call("write", r#"{"path":"src/a.txt","content":"one\ntwo\n"}"#),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .unwrap();
    let read = tools.read();
    let result = smol::block_on(read.execute(
        call("read", r#"{"path":"src/a.txt","offset":2,"limit":1}"#),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .unwrap();
    assert_eq!(result.content, "two");
    let edit = tools.edit();
    smol::block_on(edit.execute(
        call(
            "edit",
            r#"{"path":"src/a.txt","edits":[{"oldText":"two","newText":"TWO"}]}"#,
        ),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("src/a.txt")).unwrap(),
        "one\nTWO\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tea_v2_edit_applies_all_files_from_one_original_snapshot() {
    let root = workspace();
    fs::write(root.join("first.txt"), "before first\n").unwrap();
    fs::write(root.join("second.txt"), "before second\n").unwrap();
    let tools = crate::coding::TeaCodingToolsV2::new(&root).unwrap();

    let result = smol::block_on(tools.edit().execute(
        call(
            "edit",
            r#"{"files":[{"path":"first.txt","edits":[{"oldText":"before","newText":"after"}]},{"path":"second.txt","edits":[{"oldText":"before","newText":"after"}]}]}"#,
        ),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .expect("v2 transaction should commit");

    assert_eq!(result.content, "Applied 2 replacements in 2 files.");
    assert_eq!(fs::read_to_string(root.join("first.txt")).unwrap(), "after first\n");
    assert_eq!(fs::read_to_string(root.join("second.txt")).unwrap(), "after second\n");
    assert!(tools.edit().requires_exclusive_batch());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tea_v2_edit_crosses_one_batch_read_and_one_transaction_boundary() {
    let root = workspace();
    fs::write(root.join("first.txt"), "before first\n").unwrap();
    fs::write(root.join("second.txt"), "before second\n").unwrap();
    let host = Arc::new(BatchProbeOperations::default());
    let operations: Arc<dyn CodingOperations> = host.clone();
    let tools = crate::coding::TeaCodingToolsV2::with_operations(&root, operations).unwrap();

    let result = smol::block_on(tools.edit().execute(
        call(
            "edit",
            r#"{"files":[{"path":"first.txt","edits":[{"oldText":"before","newText":"after"}]},{"path":"second.txt","edits":[{"oldText":"before","newText":"after"}]}]}"#,
        ),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .expect("batch probe transaction should commit");

    assert_eq!(result.content, "Applied 2 replacements in 2 files.");
    assert_eq!(host.snapshot_calls.load(Ordering::Relaxed), 1);
    assert_eq!(host.commit_calls.load(Ordering::Relaxed), 1);
    let committed = host
        .committed
        .lock()
        .expect("committed transaction mutex")
        .clone()
        .expect("transaction was captured");
    assert_eq!(committed.files.len(), 2);
    assert_eq!(committed.files[0].replacement_content, b"after first\n");
    assert_eq!(committed.files[1].replacement_content, b"after second\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tea_v2_edit_rejects_any_invalid_file_before_writing_another() {
    let root = workspace();
    fs::write(root.join("first.txt"), "before first\n").unwrap();
    fs::write(root.join("second.txt"), "before second\n").unwrap();
    let tools = crate::coding::TeaCodingToolsV2::new(&root).unwrap();

    let error = smol::block_on(tools.edit().execute(
        call(
            "edit",
            r#"{"files":[{"path":"first.txt","edits":[{"oldText":"before","newText":"after"}]},{"path":"second.txt","edits":[{"oldText":"missing","newText":"after"}]}]}"#,
        ),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .expect_err("a nonmatching file must reject the whole transaction");

    assert!(matches!(error, ToolError::Execution { tool, .. } if tool == "edit"));
    assert_eq!(fs::read_to_string(root.join("first.txt")).unwrap(), "before first\n");
    assert_eq!(fs::read_to_string(root.join("second.txt")).unwrap(), "before second\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tea_v2_edit_rejects_duplicate_canonical_paths_and_stale_digests() {
    let root = workspace();
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("nested/file.txt"), "before\n").unwrap();
    let tools = crate::coding::TeaCodingToolsV2::new(&root).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("nested/file.txt"), root.join("alias.txt")).unwrap();
    #[cfg(unix)]
    let duplicate_alias = "alias.txt";
    #[cfg(not(unix))]
    let duplicate_alias = "nested/../nested/file.txt";

    let duplicate = smol::block_on(tools.edit().execute(
        call(
            "edit",
            &format!(
                r#"{{"files":[{{"path":"nested/file.txt","edits":[{{"oldText":"before","newText":"after"}}]}},{{"path":"{duplicate_alias}","edits":[{{"oldText":"before","newText":"after"}}]}}]}}"#
            ),
        ),
        context(),
        ToolUpdateSink::disabled(),
    ));
    assert!(matches!(duplicate, Err(ToolError::InvalidArguments { tool, .. }) if tool == "edit"));

    let stale = smol::block_on(tools.edit().execute(
        call(
            "edit",
            r#"{"files":[{"path":"nested/file.txt","expectedDigest":"0000000000000000000000000000000000000000000000000000000000000000","edits":[{"oldText":"before","newText":"after"}]}]}"#,
        ),
        context(),
        ToolUpdateSink::disabled(),
    ));
    assert!(matches!(stale, Err(ToolError::Execution { tool, .. }) if tool == "edit"));
    assert_eq!(fs::read_to_string(root.join("nested/file.txt")).unwrap(), "before\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tea_v2_edit_rejects_overlap_non_utf8_non_regular_and_oversized_snapshots() {
    let root = workspace();
    fs::write(root.join("overlap.txt"), "abcdef\n").unwrap();
    fs::write(root.join("binary.dat"), [0xff, 0xfe]).unwrap();
    fs::create_dir(root.join("directory")).unwrap();
    fs::write(root.join("oversized.txt"), vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    let tools = crate::coding::TeaCodingToolsV2::new(&root).unwrap();

    for arguments in [
        r#"{"files":[{"path":"overlap.txt","edits":[{"oldText":"bcd","newText":"B"},{"oldText":"cde","newText":"C"}]}]}"#,
        r#"{"files":[{"path":"binary.dat","edits":[{"oldText":"x","newText":"y"}]}]}"#,
        r#"{"files":[{"path":"directory","edits":[{"oldText":"x","newText":"y"}]}]}"#,
        r#"{"files":[{"path":"oversized.txt","edits":[{"oldText":"x","newText":"y"}]}]}"#,
    ] {
        let error = smol::block_on(tools.edit().execute(
            call("edit", arguments),
            context(),
            ToolUpdateSink::disabled(),
        ))
        .expect_err("every invalid snapshot plan must be rejected");
        assert!(matches!(error, ToolError::Execution { tool, .. } if tool == "edit"));
    }

    assert_eq!(fs::read_to_string(root.join("overlap.txt")).unwrap(), "abcdef\n");
    assert_eq!(fs::read(root.join("binary.dat")).unwrap(), [0xff, 0xfe]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn local_edit_transaction_revalidates_every_preimage_and_honors_precommit_cancellation() {
    let root = workspace();
    let first = root.join("first.txt");
    let second = root.join("second.txt");
    fs::write(&first, "first-original").unwrap();
    fs::write(&second, "second-changed").unwrap();
    let transaction = EditTransaction {
        files: vec![
            ConditionalFileEdit {
                path: first.clone(),
                expected_content: b"first-original".to_vec(),
                replacement_content: b"first-replacement".to_vec(),
            },
            ConditionalFileEdit {
                path: second.clone(),
                expected_content: b"second-original".to_vec(),
                replacement_content: b"second-replacement".to_vec(),
            },
        ],
    };
    let operations = LocalCodingOperations;
    let stale = smol::block_on(
        operations.commit_edit_transaction(&transaction, CancellationToken::new()),
    )
    .expect("stale precondition has a transaction outcome");
    assert!(matches!(stale, EditTransactionOutcome::RolledBack { .. }));
    assert_eq!(fs::read(&first).unwrap(), b"first-original");
    assert_eq!(fs::read(&second).unwrap(), b"second-changed");

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = smol::block_on(operations.commit_edit_transaction(&transaction, cancellation))
        .expect_err("precommit cancellation must not produce a commit receipt");
    assert_eq!(cancelled, OperationError::new("cancelled"));
    assert_eq!(fs::read(&first).unwrap(), b"first-original");
    assert_eq!(fs::read(&second).unwrap(), b"second-changed");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tea_v2_profile_is_explicit_and_matches_the_v2_registry() {
    let root = workspace();
    let tools = crate::coding::TeaCodingToolsV2::new(&root).unwrap();
    let profile = crate::coding::TeaDefaultCodingProfileV2::pinned_default().unwrap();
    let registry = tools.registry();
    profile.validate_registry(&registry).unwrap();
    assert_eq!(profile.profile_id(), "tea-default-coding-profile/v2");
    assert_eq!(
        profile.contract_digest().to_hex(),
        "a4dd43b61caddb4eef9f0af1541487ca0ccd9d7cd7d221ab18d8b547900833b3",
    );
    assert_eq!(
        profile.active_tool_names().collect::<Vec<_>>(),
        ["read", "bash", "edit", "write"]
    );
    let executable = tools
        .coding_tools()
        .iter()
        .map(|tool| crate::tool::ToolDefinition::from_tool(tool.as_ref()))
        .collect::<Vec<_>>();
    assert_eq!(executable, profile.tool_definitions());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pinned_builder_uses_the_explicit_workspace_in_the_captured_prompt() {
    let root = workspace();
    let tools = DefaultCodingTools::new(&root).unwrap();
    let workspace_text = tools
        .workspace()
        .as_path()
        .to_string_lossy()
        .replace('\\', "/");
    let profile = crate::coding::PiDefaultCodingProfile::pinned_default()
        .expect("pinned profile capture is valid");
    let registry = tools.registry();
    profile
        .validate_registry(&registry)
        .expect("pinned profile accepts the complete default registry");
    let agent = crate::Agent::builder()
        .system_prompt(profile.system_prompt_for_workspace(tools.workspace().as_path()))
        .tools(registry)
        .build();

    let snapshot = agent.snapshot();
    assert!(
        snapshot
            .system_prompt
            .contains(&format!("Current working directory: {workspace_text}"))
    );
    assert!(
        !snapshot
            .system_prompt
            .contains("Current working directory: /fixture/workspace")
    );
    assert_eq!(
        agent
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>(),
        ["read", "bash", "edit", "write"]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bash_uses_explicit_workspace_and_empty_environment() {
    let root = workspace();
    let tools = DefaultCodingTools::new(&root).unwrap();
    let result = smol::block_on(tools.bash().execute(
        call("bash", r#"{"command":"printf '%s' \"$PI_SECRET\"; pwd"}"#),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .unwrap();
    assert_eq!(
        result.content,
        tools.workspace().as_path().to_string_lossy()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bash_returns_when_a_background_descendant_keeps_output_open() {
    let root = workspace();
    let tools = DefaultCodingTools::new(&root).unwrap();
    let bash = tools.bash();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        sender
                .send(smol::block_on(bash.execute(
                    call(
                        "bash",
                        r#"{"command":"sh -c 'echo $$ > .background-pid; exec sleep 30' & echo launched"}"#,
                    ),
                    context(),
                    ToolUpdateSink::disabled(),
                )))
                .expect("test receiver must remain open");
    });

    let result = match receiver.recv_timeout(Duration::from_millis(500)) {
        Ok(result) => result.expect("bash should succeed"),
        Err(error) => {
            stop_background_test_process(&root);
            let _ = receiver.recv_timeout(Duration::from_secs(1));
            worker
                .join()
                .expect("bash worker should settle after cleanup");
            panic!("bash waited for its background descendant: {error}");
        }
    };
    assert_eq!(result.content, "launched");
    stop_background_test_process(&root);
    worker.join().expect("bash worker should settle");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bash_cancellation_kills_the_foreground_shell_and_settles_promptly() {
    let root = workspace();
    let tools = DefaultCodingTools::new(&root).unwrap();
    let bash = tools.bash();
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        sender
            .send(smol::block_on(bash.execute(
                call(
                    "bash",
                    r#"{"command":"echo $$ > .foreground-pid; exec sleep 30"}"#,
                ),
                ToolContext {
                    cancellation: worker_cancellation,
                    metadata: None,
                },
                ToolUpdateSink::disabled(),
            )))
            .expect("test receiver must remain open");
    });

    wait_for_test_process(&root, ".foreground-pid");
    cancellation.cancel();
    let result = match receiver.recv_timeout(Duration::from_millis(500)) {
        Ok(result) => result,
        Err(error) => {
            stop_test_process(&root, ".foreground-pid");
            let _ = receiver.recv_timeout(Duration::from_secs(1));
            worker
                .join()
                .expect("bash worker should settle after cleanup");
            panic!("bash did not settle after cancellation: {error}");
        }
    };
    assert!(matches!(
        result,
        Err(ToolError::Cancelled { tool }) if tool == "bash"
    ));
    worker.join().expect("bash worker should settle");
    fs::remove_dir_all(root).unwrap();
}

fn stop_background_test_process(root: &Path) {
    stop_test_process(root, ".background-pid");
}

fn wait_for_test_process(root: &Path, filename: &str) {
    for _ in 0..100 {
        if root.join(filename).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn stop_test_process(root: &Path, filename: &str) {
    if let Ok(pid) = fs::read_to_string(root.join(filename)) {
        let _ = Command::new("kill").arg("-TERM").arg(pid.trim()).status();
    }
}

#[test]
fn grep_and_find_are_explicit_and_deterministic() {
    let root = workspace();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "TODO: one\nclean\n").unwrap();
    fs::write(root.join("src/b.txt"), "TODO: two\n").unwrap();
    let tools = DefaultCodingTools::new(&root).unwrap();
    let grep = smol::block_on(tools.grep().execute(
        call("grep", r#"{"pattern":"TODO","glob":"**/*.rs"}"#),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .unwrap();
    assert_eq!(grep.content, "src/a.rs:1: TODO: one");
    let find = smol::block_on(tools.find().execute(
        call("find", r#"{"pattern":"**/*.rs"}"#),
        context(),
        ToolUpdateSink::disabled(),
    ))
    .unwrap();
    assert_eq!(find.content, "src/a.rs");
    fs::remove_dir_all(root).unwrap();
}
