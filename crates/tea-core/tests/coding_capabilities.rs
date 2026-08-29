//! Regression coverage for the trusted host side of the Luau coding builtins.
//!
//! These tests exercise `CodingHost` directly so model-facing schema and
//! formatting changes cannot accidentally weaken path, process, or transaction
//! authority.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::coding::{
    CodingHost, CodingOperations, CommandEnvironment, CommandOutput, CommandTermination,
    EntryMetadata, LocalCodingOperations, OperationError, OperationFuture, SearchResult,
    SearchTruncation,
    PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1, WORKSPACE_READ_CAPABILITY_V1,
    WORKSPACE_SEARCH_CAPABILITY_V1,
};
use tea_core::effect::RunProvenance;
use tea_core::harness::extension::{
    ExtensionCapability, ExtensionCapabilityError, ExtensionCapabilityRequest,
};
use tea_core::scheduler::CancellationToken;
use tea_core::state::ToolCallId;
use tea_core::tool::ToolUpdateSink;
use tea_protocol::JsonValue;
use tea_session::Digest;

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tea-coding-capability-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the unix epoch")
                .as_nanos(),
            NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).expect("workspace creates");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(capability: &str, method: &str, arguments: &str) -> ExtensionCapabilityRequest {
    ExtensionCapabilityRequest {
        call_id: ToolCallId::new(format!("capability-{method}")).expect("call ID is valid"),
        tool_name: "capability-test".into(),
        provenance: RunProvenance::default(),
        capability: capability.into(),
        method: method.into(),
        arguments: JsonValue::parse(arguments).expect("test arguments are JSON"),
        updates: ToolUpdateSink::disabled(),
    }
}

fn invoke(
    capability: &dyn ExtensionCapability,
    request: ExtensionCapabilityRequest,
) -> Result<JsonValue, ExtensionCapabilityError> {
    smol::block_on(capability.invoke(request, CancellationToken::new()))
        .map(|response| response.value)
}

#[derive(Clone)]
struct RecordingProcessOperations {
    observed_timeouts: Arc<Mutex<Vec<Duration>>>,
    output: CommandOutput,
}

impl RecordingProcessOperations {
    fn new(output: CommandOutput) -> Self {
        Self {
            observed_timeouts: Arc::new(Mutex::new(Vec::new())),
            output,
        }
    }
}

impl CodingOperations for RecordingProcessOperations {
    fn read_file<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        Box::pin(async { Err(OperationError::new("read is not used by this process fixture")) })
    }

    fn metadata<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        Box::pin(async {
            Err(OperationError::new(
                "metadata is not used by this process fixture",
            ))
        })
    }

    fn find_files<'a>(
        &'a self,
        _root: &'a Path,
        _pattern: &'a str,
        _max_results: usize,
        _max_output_bytes: usize,
        _cancellation: CancellationToken,
    ) -> OperationFuture<'a, SearchResult> {
        Box::pin(async {
            Ok(SearchResult {
                matches: Vec::new(),
                truncation: SearchTruncation::Complete,
            })
        })
    }

    fn execute_command<'a>(
        &'a self,
        _command: &'a str,
        _cwd: &'a Path,
        timeout: Duration,
        _environment: &'a CommandEnvironment,
        _cancellation: CancellationToken,
        _updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        let observed_timeouts = Arc::clone(&self.observed_timeouts);
        let output = self.output.clone();
        Box::pin(async move {
            observed_timeouts
                .lock()
                .expect("timeout recorder lock")
                .push(timeout);
            Ok(output)
        })
    }
}

fn recording_host(
    workspace: &TempWorkspace,
    output: CommandOutput,
) -> (CodingHost, Arc<Mutex<Vec<Duration>>>) {
    let operations = RecordingProcessOperations::new(output);
    let observed_timeouts = Arc::clone(&operations.observed_timeouts);
    let host = CodingHost::with_operations(workspace.path(), Arc::new(operations))
        .expect("recording host configures");
    (host, observed_timeouts)
}

#[test]
fn read_and_find_are_confined_and_reject_foreign_methods() {
    let workspace = TempWorkspace::new();
    fs::create_dir_all(workspace.path().join("src")).expect("source directory creates");
    fs::write(workspace.path().join("src/lib.rs"), "one\ntwo\nthree\n").expect("fixture writes");
    let host = CodingHost::new(workspace.path()).expect("host configures");

    let read = invoke(
        host.read_capability().as_ref(),
        request(
            WORKSPACE_READ_CAPABILITY_V1,
            "read",
            r#"{"path":"src/lib.rs","offset":2,"limit":1,"includeDigest":true}"#,
        ),
    )
    .expect("read succeeds");
    assert_eq!(read.get("content").and_then(JsonValue::as_str), Some("two"));
    assert_eq!(
        read.get("digest").and_then(JsonValue::as_str),
        Some(Digest::from_bytes(b"one\ntwo\nthree\n").to_hex().as_str())
    );

    let find = invoke(
        host.search_capability().as_ref(),
        request(
            WORKSPACE_SEARCH_CAPABILITY_V1,
            "find",
            r#"{"pattern":"*.rs"}"#,
        ),
    )
    .expect("optimized find succeeds");
    assert_eq!(
        find.get("matches").and_then(JsonValue::as_array),
        Some(&[JsonValue::String("src/lib.rs".into())][..])
    );

    let escape = invoke(
        host.read_capability().as_ref(),
        request(
            WORKSPACE_READ_CAPABILITY_V1,
            "read",
            r#"{"path":"../outside"}"#,
        ),
    )
    .expect_err("workspace escape is denied");
    assert!(matches!(escape, ExtensionCapabilityError::Execution { .. }));

    let foreign_method = invoke(
        host.read_capability().as_ref(),
        request(
            WORKSPACE_READ_CAPABILITY_V1,
            "commit",
            r#"{"path":"src/lib.rs"}"#,
        ),
    )
    .expect_err("read authority cannot mutate");
    assert!(matches!(
        foreign_method,
        ExtensionCapabilityError::MethodDenied { .. }
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = TempWorkspace::new();
        fs::write(outside.path().join("outside.rs"), "outside\n").expect("outside fixture writes");
        symlink(
            outside.path().join("outside.rs"),
            workspace.path().join("escape.rs"),
        )
        .expect("file symlink creates");
        symlink(outside.path(), workspace.path().join("escape-dir"))
            .expect("directory symlink creates");

        let symlink_read = invoke(
            host.read_capability().as_ref(),
            request(
                WORKSPACE_READ_CAPABILITY_V1,
                "read",
                r#"{"path":"escape.rs"}"#,
            ),
        )
        .expect_err("read cannot follow a symlink outside the workspace");
        assert!(matches!(
            symlink_read,
            ExtensionCapabilityError::Execution { .. }
        ));

        let confined_find = invoke(
            host.search_capability().as_ref(),
            request(
                WORKSPACE_SEARCH_CAPABILITY_V1,
                "find",
                r#"{"pattern":"*.rs"}"#,
            ),
        )
        .expect("find remains confined below the workspace root");
        assert_eq!(
            confined_find.get("matches").and_then(JsonValue::as_array),
            Some(&[JsonValue::String("src/lib.rs".into())][..])
        );
    }
}

#[test]
fn find_enforces_result_and_output_budgets_with_a_structured_receipt() {
    let workspace = TempWorkspace::new();
    fs::write(workspace.path().join("a.rs"), "a").expect("first fixture writes");
    fs::write(workspace.path().join("b.rs"), "b").expect("second fixture writes");
    for index in 0..250 {
        fs::write(
            workspace
                .path()
                .join(format!("{index:04}-{}.txt", "x".repeat(235))),
            "fixture",
        )
        .expect("byte-budget fixture writes");
    }
    let host = CodingHost::new(workspace.path()).expect("host configures");

    let result_limited = invoke(
        host.search_capability().as_ref(),
        request(
            WORKSPACE_SEARCH_CAPABILITY_V1,
            "find",
            r#"{"pattern":"*.rs","limit":1}"#,
        ),
    )
    .expect("result-limited search succeeds");
    assert_eq!(
        result_limited
            .get("matches")
            .and_then(JsonValue::as_array)
            .map(|matches| matches.len()),
        Some(1)
    );
    assert_eq!(
        result_limited.get("truncation").and_then(JsonValue::as_str),
        Some("result_limit")
    );

    let byte_limited = invoke(
        host.search_capability().as_ref(),
        request(
            WORKSPACE_SEARCH_CAPABILITY_V1,
            "find",
            r#"{"pattern":"*.txt"}"#,
        ),
    )
    .expect("byte-limited search succeeds");
    let matches = byte_limited
        .get("matches")
        .and_then(JsonValue::as_array)
        .expect("search response has matches");
    let rendered_bytes = matches
        .iter()
        .filter_map(JsonValue::as_str)
        .collect::<Vec<_>>()
        .join("\n")
        .len();
    assert!(rendered_bytes <= 50 * 1024);
    assert_eq!(
        byte_limited.get("truncation").and_then(JsonValue::as_str),
        Some("byte_budget")
    );

    let over_limit = invoke(
        host.search_capability().as_ref(),
        request(
            WORKSPACE_SEARCH_CAPABILITY_V1,
            "find",
            r#"{"pattern":"*.rs","limit":1001}"#,
        ),
    )
    .expect_err("the host independently rejects a result limit above 1000");
    assert!(matches!(
        over_limit,
        ExtensionCapabilityError::InvalidArguments { .. }
    ));

    let oversized_pattern = format!(r#"{{"pattern":"{}"}}"#, "*".repeat(4097));
    let overlong_pattern = invoke(
        host.search_capability().as_ref(),
        request(
            WORKSPACE_SEARCH_CAPABILITY_V1,
            "find",
            &oversized_pattern,
        ),
    )
    .expect_err("the host independently rejects overlong glob patterns");
    assert!(matches!(
        overlong_pattern,
        ExtensionCapabilityError::InvalidArguments { .. }
    ));
}

#[test]
fn unified_mutation_commits_creates_replacements_and_exact_edits_together() {
    let workspace = TempWorkspace::new();
    fs::write(workspace.path().join("existing.txt"), "alpha\n").expect("fixture writes");
    fs::write(workspace.path().join("replace.txt"), "old\n").expect("fixture writes");
    let host = CodingHost::new(workspace.path()).expect("host configures");

    let response = invoke(
        host.mutate_capability().as_ref(),
        request(
            WORKSPACE_MUTATE_CAPABILITY_V1,
            "commit",
            r#"{"files":[{"path":"existing.txt","edits":[{"oldText":"alpha","newText":"beta"}]},{"path":"replace.txt","content":"new\n"},{"path":"created.txt","content":"created\n"}]}"#,
        ),
    )
    .expect("one transaction commits");
    assert_eq!(
        response
            .get("preciseReplacements")
            .and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        response
            .get("modifiedExistingFiles")
            .and_then(JsonValue::as_u64),
        Some(2)
    );
    assert_eq!(
        response.get("createdFiles").and_then(JsonValue::as_u64),
        Some(1)
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("existing.txt")).unwrap(),
        "beta\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("replace.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("created.txt")).unwrap(),
        "created\n"
    );

    let rejected = invoke(
        host.mutate_capability().as_ref(),
        request(
            WORKSPACE_MUTATE_CAPABILITY_V1,
            "commit",
            r#"{"files":[{"path":"existing.txt","edits":[{"oldText":"missing","newText":"never"}]},{"path":"would-be-created.txt","content":"never\n"}]}"#,
        ),
    )
    .expect_err("all preconditions are checked before publication");
    assert!(matches!(
        rejected,
        ExtensionCapabilityError::Execution { .. }
    ));
    assert!(!workspace.path().join("would-be-created.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.path().join("existing.txt")).unwrap(),
        "beta\n"
    );
}

#[test]
fn process_is_separate_from_workspace_capabilities() {
    let workspace = TempWorkspace::new();
    let host = CodingHost::new(workspace.path()).expect("host configures");
    let response = invoke(
        host.process_capability().as_ref(),
        request(
            PROCESS_CAPABILITY_V1,
            "run",
            r#"{"command":"printf capability-process"}"#,
        ),
    )
    .expect("process capability runs a command");
    assert_eq!(
        response.get("content").and_then(JsonValue::as_str),
        Some("capability-process")
    );
    assert_eq!(
        response.get("exitCode").and_then(JsonValue::as_f64),
        Some(0.0)
    );
    assert_eq!(
        response.get("termination").and_then(JsonValue::as_str),
        Some("exited")
    );

    let denied = invoke(
        host.search_capability().as_ref(),
        request(
            WORKSPACE_SEARCH_CAPABILITY_V1,
            "run",
            r#"{"command":"true"}"#,
        ),
    )
    .expect_err("find authority cannot execute a process");
    assert!(matches!(
        denied,
        ExtensionCapabilityError::MethodDenied { .. }
    ));
}

#[cfg(unix)]
#[test]
fn local_process_capability_surfaces_timeout_and_signal_as_typed_receipts() {
    let workspace = TempWorkspace::new();
    let host = CodingHost::new(workspace.path()).expect("host configures");
    let timed_out = invoke(
        host.process_capability().as_ref(),
        request(
            PROCESS_CAPABILITY_V1,
            "run",
            r#"{"command":"printf started; sleep 5; touch should-not-exist","timeout":0.05}"#,
        ),
    )
    .expect("timeout is a settled process receipt");
    assert_eq!(
        timed_out.get("termination").and_then(JsonValue::as_str),
        Some("timed_out")
    );
    assert_eq!(timed_out.get("exitCode"), Some(&JsonValue::Null));
    assert!(
        timed_out
            .get("content")
            .and_then(JsonValue::as_str)
            .is_some_and(|content| content.contains("started"))
    );
    assert!(!workspace.path().join("should-not-exist").exists());

    let signaled = invoke(
        host.process_capability().as_ref(),
        request(
            PROCESS_CAPABILITY_V1,
            "run",
            r#"{"command":"kill -TERM $$"}"#,
        ),
    )
    .expect("signal is a settled process receipt");
    assert_eq!(
        signaled.get("termination").and_then(JsonValue::as_str),
        Some("signaled")
    );
    assert_eq!(signaled.get("signal").and_then(JsonValue::as_f64), Some(15.0));
    assert_eq!(signaled.get("exitCode"), Some(&JsonValue::Null));
}

#[test]
fn process_resolves_an_omitted_timeout_to_the_finite_host_default() {
    let workspace = TempWorkspace::new();
    let (host, observed_timeouts) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::Exited { code: 0 },
            stdout: b"recorded".to_vec(),
            stderr: Vec::new(),
        },
    );

    let response = invoke(
        host.process_capability().as_ref(),
        request(PROCESS_CAPABILITY_V1, "run", r#"{"command":"printf recorded"}"#),
    )
    .expect("process capability succeeds");

    assert_eq!(
        observed_timeouts.lock().expect("timeout recorder lock").as_slice(),
        &[Duration::from_secs(300)]
    );
    assert_eq!(
        response.get("termination").and_then(JsonValue::as_str),
        Some("exited")
    );
}

#[test]
fn process_explicit_timeout_replaces_the_host_default() {
    let workspace = TempWorkspace::new();
    let (host, observed_timeouts) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::Exited { code: 0 },
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );

    invoke(
        host.process_capability().as_ref(),
        request(
            PROCESS_CAPABILITY_V1,
            "run",
            r#"{"command":"true","timeout":0.25}"#,
        ),
    )
    .expect("explicit timeout succeeds");

    assert_eq!(
        observed_timeouts.lock().expect("timeout recorder lock").as_slice(),
        &[Duration::from_millis(250)]
    );
}

#[test]
fn process_rejects_a_timeout_above_the_existing_maximum() {
    let workspace = TempWorkspace::new();
    let (host, observed_timeouts) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::Exited { code: 0 },
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );

    let error = invoke(
        host.process_capability().as_ref(),
        request(
            PROCESS_CAPABILITY_V1,
            "run",
            r#"{"command":"true","timeout":2147.483648}"#,
        ),
    )
    .expect_err("the process timeout maximum remains fixed");

    assert!(matches!(error, ExtensionCapabilityError::InvalidArguments { .. }));
    assert!(
        observed_timeouts
            .lock()
            .expect("timeout recorder lock")
            .is_empty(),
        "invalid timeout input must not reach the trusted process adapter"
    );
}

#[test]
fn process_receipts_make_each_settled_lifecycle_outcome_unambiguous() {
    let workspace = TempWorkspace::new();
    let (signaled_host, _) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::Signaled { signal: 15 },
            stdout: b"partial".to_vec(),
            stderr: Vec::new(),
        },
    );
    let signaled = invoke(
        signaled_host.process_capability().as_ref(),
        request(PROCESS_CAPABILITY_V1, "run", r#"{"command":"ignored"}"#),
    )
    .expect("signal settlement is a process receipt");
    assert_eq!(
        signaled.get("termination").and_then(JsonValue::as_str),
        Some("signaled")
    );
    assert_eq!(signaled.get("signal").and_then(JsonValue::as_f64), Some(15.0));
    assert_eq!(signaled.get("exitCode"), Some(&JsonValue::Null));
    assert!(signaled.get("reason").is_none());

    let (timed_out_host, _) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::TimedOut,
            stdout: b"partial".to_vec(),
            stderr: Vec::new(),
        },
    );
    let timed_out = invoke(
        timed_out_host.process_capability().as_ref(),
        request(PROCESS_CAPABILITY_V1, "run", r#"{"command":"ignored"}"#),
    )
    .expect("timeout settlement is a process receipt");
    assert_eq!(
        timed_out.get("termination").and_then(JsonValue::as_str),
        Some("timed_out")
    );
    assert_eq!(timed_out.get("exitCode"), Some(&JsonValue::Null));
    assert!(timed_out.get("signal").is_none());

    let (indeterminate_host, _) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::Indeterminate {
                reason: "could not prove cleanup".into(),
            },
            stdout: Vec::new(),
            stderr: b"partial failure".to_vec(),
        },
    );
    let indeterminate = invoke(
        indeterminate_host.process_capability().as_ref(),
        request(PROCESS_CAPABILITY_V1, "run", r#"{"command":"ignored"}"#),
    )
    .expect("indeterminate settlement is a process receipt");
    assert_eq!(
        indeterminate.get("termination").and_then(JsonValue::as_str),
        Some("indeterminate")
    );
    assert_eq!(
        indeterminate.get("reason").and_then(JsonValue::as_str),
        Some(
            "could not prove cleanup; command termination is indeterminate; side effects may already exist; inspect state before retrying"
        )
    );
    assert_eq!(indeterminate.get("exitCode"), Some(&JsonValue::Null));
    assert!(indeterminate.get("signal").is_none());

    let (cancelled_host, _) = recording_host(
        &workspace,
        CommandOutput {
            termination: CommandTermination::Cancelled,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );
    let cancelled = invoke(
        cancelled_host.process_capability().as_ref(),
        request(PROCESS_CAPABILITY_V1, "run", r#"{"command":"ignored"}"#),
    )
    .expect_err("clean command cancellation remains runtime control flow");
    assert!(matches!(cancelled, ExtensionCapabilityError::Cancelled));
}

#[cfg(unix)]
#[test]
fn separate_coding_hosts_keep_process_cleanup_local_to_their_workspace_invocation() {
    let first_workspace = TempWorkspace::new();
    let second_workspace = TempWorkspace::new();
    let operations: Arc<dyn CodingOperations> = Arc::new(LocalCodingOperations);
    let first_host = CodingHost::with_operations(first_workspace.path(), Arc::clone(&operations))
        .expect("first host configures");
    let second_host = CodingHost::with_operations(second_workspace.path(), operations)
        .expect("second host configures");
    let first_cancellation = CancellationToken::new();
    let first_task_cancellation = first_cancellation.clone();
    let first_capability = first_host.process_capability();
    let first_request = request(
        PROCESS_CAPABILITY_V1,
        "run",
        r#"{"command":"sleep 5 & child=$!; echo $child > child.pid; touch started; wait $child"}"#,
    );
    let first_task = smol::spawn(async move {
        first_capability
            .invoke(first_request, first_task_cancellation)
            .await
    });
    let second_capability = second_host.process_capability();
    let second_request = request(
        PROCESS_CAPABILITY_V1,
        "run",
        r#"{"command":"sleep 5 & child=$!; echo $child > child.pid; touch started; for _ in $(seq 1 500); do [ -e release ] && break; sleep 0.01; done; kill $child; wait $child || true; printf second-done"}"#,
    );
    let second_task = smol::spawn(async move {
        second_capability
            .invoke(second_request, CancellationToken::new())
            .await
    });

    for _ in 0..200 {
        if first_workspace.path().join("started").is_file()
            && second_workspace.path().join("started").is_file()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(first_workspace.path().join("started").is_file());
    assert!(second_workspace.path().join("started").is_file());
    let second_child = fs::read_to_string(second_workspace.path().join("child.pid"))
        .expect("second child PID is recorded")
        .trim()
        .to_owned();

    first_cancellation.cancel();
    let first = smol::block_on(first_task);
    assert!(matches!(first, Err(ExtensionCapabilityError::Cancelled)));
    assert!(
        Command::new("/bin/kill")
            .arg("-0")
            .arg(&second_child)
            .status()
            .expect("kill checks the second child")
            .success(),
        "cancelling the first host must not signal the second host's child"
    );

    fs::write(second_workspace.path().join("release"), b"").expect("second host releases");
    let second = smol::block_on(second_task).expect("second host command settles");
    assert_eq!(
        second.value.get("termination").and_then(JsonValue::as_str),
        Some("exited")
    );
    assert!(
        second
            .value
            .get("content")
            .and_then(JsonValue::as_str)
            .is_some_and(|content| content.contains("second-done"))
    );
}
