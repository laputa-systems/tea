//! Executable evidence for the pinned default coding-tool factories.
//!
//! These tests deliberately construct every tool over a unique temporary workspace.  They do
//! not depend on the repository checkout, process cwd, home directory, or ambient credentials.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tea_core::coding::DefaultCodingTools;
use tea_core::coding::{
    CodingOperations, CommandOutput, DirectoryEntry, EntryMetadata, GrepOptions, OperationError,
    OperationFuture,
};
use tea_core::error::ToolError;
use tea_core::scheduler::CancellationToken;
use tea_core::state::{SerializedJson, ToolCallId};
use tea_core::tool::{AgentTool, ToolCall, ToolContext, ToolUpdateSink};

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pi-default-tools-integration-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after unix epoch")
                .as_nanos(),
            NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).expect("temporary workspace should be creatable");
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

fn call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new(format!("integration-{name}"))
            .expect("integration tool-call IDs are non-empty"),
        name: name.to_owned(),
        arguments: SerializedJson::new(arguments),
    }
}

fn context() -> ToolContext {
    ToolContext {
        cancellation: CancellationToken::new(),
        provenance: tea_core::effect::RunProvenance::default(),
    }
}

fn execute(
    tool: &dyn AgentTool,
    name: &str,
    arguments: &str,
) -> Result<tea_core::tool::AgentToolResult, ToolError> {
    smol::block_on(tool.execute(call(name, arguments), context(), ToolUpdateSink::disabled()))
}

/// An adapter that fails at the first host operation.  Keeping this separate from missing-path
/// tests proves that a valid tool call reaches, and preserves, an explicit host failure.
#[derive(Debug)]
struct FailingOperations;

impl FailingOperations {
    fn fail<'a, T: Send + 'a>(&'a self) -> OperationFuture<'a, T> {
        Box::pin(std::future::ready(Err(OperationError::new(
            "fixture host failure",
        ))))
    }
}

impl CodingOperations for FailingOperations {
    fn read_file<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        self.fail()
    }

    fn write_file<'a>(&'a self, _path: &'a Path, _content: &'a [u8]) -> OperationFuture<'a, ()> {
        self.fail()
    }

    fn create_dir_all<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, ()> {
        self.fail()
    }

    fn metadata<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        self.fail()
    }

    fn read_dir<'a>(&'a self, _path: &'a Path) -> OperationFuture<'a, Vec<DirectoryEntry>> {
        self.fail()
    }

    fn find_files<'a>(
        &'a self,
        _root: &'a Path,
        _pattern: &'a str,
        _limit: usize,
    ) -> OperationFuture<'a, Vec<String>> {
        self.fail()
    }

    fn grep_files<'a>(
        &'a self,
        _root: &'a Path,
        _pattern: &'a str,
        _options: GrepOptions,
    ) -> OperationFuture<'a, Vec<tea_core::coding::GrepMatch>> {
        self.fail()
    }

    fn execute_command<'a>(
        &'a self,
        _command: &'a str,
        _cwd: &'a Path,
        _timeout_seconds: Option<f64>,
        _environment: &'a tea_core::coding::CommandEnvironment,
        _cancellation: CancellationToken,
        _updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        self.fail()
    }
}

#[test]
fn standard_tools_succeed_in_an_explicit_temp_workspace() {
    let workspace = TempWorkspace::new();
    fs::create_dir(workspace.path().join("src")).unwrap();
    fs::write(
        workspace.path().join("src/lib.rs"),
        "fn main() {\n    TODO: change me\n}\n",
    )
    .unwrap();
    fs::write(workspace.path().join("notes.txt"), "TODO: notes\n").unwrap();

    let tools = DefaultCodingTools::new(workspace.path()).unwrap();

    let read = execute(tools.read().as_ref(), "read", r#"{"path":"src/lib.rs"}"#).unwrap();
    assert!(read.content.contains("TODO: change me"));

    let bash = execute(
        tools.bash().as_ref(),
        "bash",
        r#"{"command":"printf 'hello'; pwd"}"#,
    )
    .unwrap();
    assert!(bash.content.starts_with("hello"));
    assert!(
        bash.content
            .contains(tools.workspace().as_path().to_string_lossy().as_ref())
    );

    let edit = execute(
        tools.edit().as_ref(),
        "edit",
        r#"{"path":"src/lib.rs","edits":[{"oldText":"TODO: change me","newText":"changed"}]}"#,
    )
    .unwrap();
    assert_eq!(
        edit.content,
        "Successfully replaced 1 block(s) in src/lib.rs."
    );
    assert!(
        fs::read_to_string(workspace.path().join("src/lib.rs"))
            .unwrap()
            .contains("changed")
    );

    let write = execute(
        tools.write().as_ref(),
        "write",
        r#"{"path":"generated/out.txt","content":"generated"}"#,
    )
    .unwrap();
    assert!(write.content.contains("Successfully wrote 9 bytes"));
    assert_eq!(
        fs::read_to_string(workspace.path().join("generated/out.txt")).unwrap(),
        "generated"
    );

    let grep = execute(
        tools.grep().as_ref(),
        "grep",
        r#"{"pattern":"TODO","glob":"**/*.txt"}"#,
    )
    .unwrap();
    assert_eq!(grep.content, "notes.txt:1: TODO: notes");

    let find = execute(tools.find().as_ref(), "find", r#"{"pattern":"**/*.rs"}"#).unwrap();
    assert_eq!(find.content, "src/lib.rs");

    let ls = execute(tools.ls().as_ref(), "ls", r#"{"path":"src"}"#).unwrap();
    assert_eq!(ls.content, "lib.rs");
}

#[test]
fn read_truncates_from_the_head_and_empty_bash_is_explicit() {
    let workspace = TempWorkspace::new();
    let content = (0..2_100)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(workspace.path().join("large.txt"), content).unwrap();
    let tools = DefaultCodingTools::new(workspace.path()).unwrap();

    let read = execute(tools.read().as_ref(), "read", r#"{"path":"large.txt"}"#).unwrap();
    assert!(read.content.starts_with("line-0\nline-1"));
    assert!(read.content.contains("[truncated]"));
    assert!(!read.content.contains("line-2099"));

    let bash = execute(tools.bash().as_ref(), "bash", r#"{"command":"true"}"#).unwrap();
    assert_eq!(bash.content, "(no output)");
}

#[test]
fn standard_tools_reject_invalid_arguments_before_host_operations() {
    let workspace = TempWorkspace::new();
    fs::write(workspace.path().join("file.txt"), "content").unwrap();
    let tools = DefaultCodingTools::new(workspace.path()).unwrap();

    let cases = [
        ("read", tools.read(), r#"{"path":42}"#),
        ("bash", tools.bash(), r#"{"command":"true","timeout":0}"#),
        ("edit", tools.edit(), r#"{"path":"file.txt","edits":[]}"#),
        ("write", tools.write(), r#"{"path":"file.txt"}"#),
        ("grep", tools.grep(), r#"{"pattern":"[unterminated"}"#),
        ("find", tools.find(), r#"{"pattern":""}"#),
        ("ls", tools.ls(), r#"{"limit":0}"#),
    ];

    for (name, tool, arguments) in cases {
        let error = execute(tool.as_ref(), name, arguments)
            .expect_err("invalid arguments must fail before host execution");
        assert!(
            matches!(&error, ToolError::InvalidArguments { tool: actual, .. } if actual == name),
            "{name} returned unexpected error: {error:?}"
        );
    }
}

#[test]
fn standard_tools_preserve_explicit_host_operation_failures() {
    let workspace = TempWorkspace::new();
    fs::write(workspace.path().join("file.txt"), "content\n").unwrap();
    let tools =
        DefaultCodingTools::with_operations(workspace.path(), Arc::new(FailingOperations)).unwrap();

    let cases = [
        ("read", tools.read(), r#"{"path":"file.txt"}"#),
        ("bash", tools.bash(), r#"{"command":"true"}"#),
        (
            "edit",
            tools.edit(),
            r#"{"path":"file.txt","edits":[{"oldText":"content","newText":"updated"}]}"#,
        ),
        (
            "write",
            tools.write(),
            r#"{"path":"new.txt","content":"new"}"#,
        ),
        ("grep", tools.grep(), r#"{"pattern":"content"}"#),
        ("find", tools.find(), r#"{"pattern":"**/*.txt"}"#),
        ("ls", tools.ls(), r#"{"path":"."}"#),
    ];

    for (name, tool, arguments) in cases {
        let error = execute(tool.as_ref(), name, arguments)
            .expect_err("a valid call must surface the host failure");
        assert!(
            matches!(&error, ToolError::Execution { tool: actual, message: detail }
                if actual == name && detail == "fixture host failure"),
            "{name} returned unexpected error: {error:?}"
        );
    }
}
