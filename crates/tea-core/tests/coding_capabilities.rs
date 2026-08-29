//! Regression coverage for the trusted host side of the Luau coding bundle.
//!
//! These tests exercise `CodingHost` directly so model-facing schema and
//! formatting changes cannot accidentally weaken path, process, or transaction
//! authority.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tea_core::coding::{
    CodingHost, PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1,
    WORKSPACE_READ_CAPABILITY_V1, WORKSPACE_SEARCH_CAPABILITY_V1,
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
