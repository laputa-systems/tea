use super::*;
use crate::error::ToolError;
use crate::scheduler::CancellationToken;
use crate::tool::{ToolCall, ToolContext, ToolUpdateSink};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::{SerializedJson, ToolCallId};

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

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
