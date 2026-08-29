use ptytest::{
    Color as PtyColor, CommandSpec, ExitStatus, Key, ProtocolProfile, PtyTest, Scenario, Size,
    TestEnv,
};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tea_protocol::JsonValue;

const ROWS: u16 = 24;
const COLUMNS: u16 = 100;
const LOCAL_PROVIDER: &str = "local";
const LOCAL_MODEL: &str = tea_providers::local::LAGUNA_XS_2_1_MODEL;
const FIXTURE_MODEL: &str = "pty-fixture-model";
const LIFECYCLE_ROOT_MODEL: &str = "pty-root-subagent-lifecycle";
const DISABLED_STARTUP_SCREEN_DIGEST: &str =
    "92fac0f8058c91bf77b8b2491a89351846652596036aca5cf5c0df4f8ad43105";
const DISABLED_STARTUP_OUTPUT_DIGEST: &str =
    "2fa32b0ae888e5363a3e5d745ac2e6529e19dcceb58f92ba4516e9ba6230fec6";

// These real-binary scenarios each own a terminal and a loopback server. Keep
// their timing barriers independent instead of making one PTY's progress
// depend on another test thread receiving CPU time.
static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

fn pty_tea_home(label: &str) -> std::path::PathBuf {
    static NEXT_HOME: AtomicU64 = AtomicU64::new(1);
    let home = std::env::temp_dir().join(format!(
        "tea-pty-{label}-{}-{}",
        std::process::id(),
        NEXT_HOME.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&home).expect("PTY Tea home creates");
    home
}

/// The xterm-minimal peer writes its terminal-query replies to `raw_input`,
/// never `raw_output`. With a fixed size and profile there is therefore no
/// nondeterministic outbound framing to erase: retain every application byte
/// in this compatibility oracle. Keep this helper rather than open-coding an
/// identity conversion so any future unavoidable framing has one documented,
/// deliberately narrow normalization point.
fn normalize_startup_output(output: &[u8]) -> Vec<u8> {
    output.to_vec()
}

fn capture_mock_idle_startup(label: &str, config: Option<&str>) -> (String, Vec<u8>) {
    let tea_home = pty_tea_home(label);
    if let Some(config) = config {
        fs::write(tea_home.join("config.toml"), config).expect("TUI config writes");
    }
    let scenario = Scenario::new("feature-off PTY parity")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            "mock",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]))
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "mock idle startup",
            |screen| screen.contains("mock/mock") && !screen.contains("agents "),
        )
        .expect("feature-off idle terminal should render");
    assert!(
        terminal
            .wait_for_quiescence(
                terminal.deadline(Duration::from_secs(3)),
                Duration::from_millis(20),
            )
            .expect("startup compatibility boundary should be readable"),
        "startup output should quiesce before it is captured"
    );
    let screen = terminal.screen().to_string();
    let output = normalize_startup_output(terminal.raw_output());

    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit feature-off terminal");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("feature-off exit restores terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap feature-off terminal");
    let _ = fs::remove_dir_all(tea_home);
    (screen, output)
}

struct StreamingFixture {
    first_delta: Receiver<()>,
    release_response: Sender<()>,
    server: thread::JoinHandle<()>,
    url: String,
}

impl StreamingFixture {
    fn start() -> Self {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("offline mock HTTP server should bind");
        let address = listener
            .local_addr()
            .expect("offline mock HTTP server address");
        let (first_delta_sent, first_delta) = mpsc::channel();
        let (release_response, wait_for_release) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener
                .accept()
                .expect("streaming provider request should connect");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
            let first = br#"data: {"choices":[{"delta":{"content":"first "},"finish_reason":null}]}

"#;
            let second = br#"data: {"choices":[{"delta":{"content":"second"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}

data: [DONE]

"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first.len() + second.len()
                    )
                    .as_bytes(),
                )
                .expect("mock response headers should write");
            socket
                .write_all(first)
                .expect("first SSE record should write");
            socket.flush().expect("first SSE record should flush");
            first_delta_sent
                .send(())
                .expect("test waits for the first SSE record");
            wait_for_release
                .recv()
                .expect("test releases the final SSE records");
            socket
                .write_all(second)
                .expect("final SSE records should write");
        });
        Self {
            first_delta,
            release_response,
            server,
            url: format!("http://{address}/v1"),
        }
    }

    fn wait_for_first_delta(&self) {
        self.first_delta
            .recv_timeout(Duration::from_secs(3))
            .expect("tea should request the offline streaming fixture");
    }

    fn release(self) {
        self.release_response
            .send(())
            .expect("offline fixture should still await settlement");
        self.server
            .join()
            .expect("offline mock HTTP server should finish");
    }
}

/// One live local-provider conversation that makes the root delegate, then
/// blocks the child after its first text delta. The terminal can therefore
/// exercise the real supervisor abort path while a physical lease exists.
struct SubagentLifecycleFixture {
    child_streamed: Receiver<()>,
    root_wait_started: Receiver<()>,
    release_child: Sender<()>,
    shutdown: Sender<()>,
    server: thread::JoinHandle<()>,
    url: String,
}

impl SubagentLifecycleFixture {
    fn start() -> Self {
        const CHILD_STREAM: &str = "CHILD_STREAM_MUST_NOT_REACH_ROOT_SCROLLBACK";

        let listener = TcpListener::bind("127.0.0.1:0").expect("subagent fixture should bind");
        listener
            .set_nonblocking(true)
            .expect("subagent fixture should become nonblocking");
        let address = listener
            .local_addr()
            .expect("subagent fixture address resolves");
        let (child_streamed_sender, child_streamed) = mpsc::channel();
        let (root_wait_sender, root_wait_started) = mpsc::channel();
        let (release_child, release_child_receiver) = mpsc::channel();
        let (shutdown, shutdown_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let spawn_arguments = format!(
                r#"{{"task_name":"lifecycle","task":"wait for root cancellation","model":"{LOCAL_MODEL}"}}"#
            );
            let wait_arguments = r#"{"targets":["lifecycle"]}"#;
            let child_first = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{CHILD_STREAM}\"}},\"finish_reason\":null}}]}}\n\n"
            );
            let child_final =
                r#"data: {"choices":[{"delta":{"content":""},"finish_reason":"stop"}]}

data: [DONE]

"#
                .to_owned();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut root_requests = 0_u8;
            let mut child_writer = None;
            let mut child_final = Some(child_final);
            let mut release_child_receiver = Some(release_child_receiver);
            let mut root_wait_sent = false;

            while child_writer.is_none() || !root_wait_sent {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if shutdown_receiver.try_recv().is_ok() {
                            return;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "subagent fixture did not receive the expected provider request"
                        );
                        // The standard library has no listener/channel select. This waits on the
                        // teardown channel rather than using a sleep as test synchronization.
                        let _ = shutdown_receiver.recv_timeout(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("subagent fixture accept fails: {error}"),
                };
                let request = read_complete_http_request(&mut socket);
                match request_model(&request).as_deref() {
                    Some(LIFECYCLE_ROOT_MODEL) => {
                        root_requests += 1;
                        match root_requests {
                            1 => write_sse_response(
                                &mut socket,
                                &tool_call_response("root-spawn", "spawn_agent", &spawn_arguments),
                            ),
                            2 => {
                                write_sse_response(
                                    &mut socket,
                                    &tool_call_response("root-wait", "wait_agent", wait_arguments),
                                );
                                root_wait_sender
                                    .send(())
                                    .expect("test waits for root wait_agent");
                                root_wait_sent = true;
                            }
                            unexpected => panic!(
                                "root made unexpected provider request {unexpected} before cancellation"
                            ),
                        }
                    }
                    Some(LOCAL_MODEL) => {
                        assert!(
                            child_writer.is_none(),
                            "one child should issue exactly one held provider request"
                        );
                        let child_final = child_final
                            .take()
                            .expect("child terminal body is used once");
                        let release_child_receiver = release_child_receiver
                            .take()
                            .expect("child release receiver is used once");
                        write_sse_prefix(&mut socket, &child_first, &child_final);
                        child_streamed_sender
                            .send(())
                            .expect("test waits for child streaming text");
                        child_writer = Some(thread::spawn(move || {
                            release_child_receiver
                                .recv()
                                .expect("test releases held child provider response");
                            let _ = socket.write_all(child_final.as_bytes());
                            let _ = socket.flush();
                        }));
                    }
                    Some(model) => panic!("fixture received an unexpected model {model:?}"),
                    None => panic!("fixture request does not name a model"),
                }
            }
            child_writer
                .expect("child writer was installed")
                .join()
                .expect("held child response writer exits");
        });
        Self {
            child_streamed,
            root_wait_started,
            release_child,
            shutdown,
            server,
            url: format!("http://{address}/v1"),
        }
    }

    fn wait_for_child_stream(&self) {
        self.child_streamed
            .recv_timeout(Duration::from_secs(5))
            .expect("child should stream before cancellation");
    }

    fn wait_for_root_wait(&self) {
        self.root_wait_started
            .recv_timeout(Duration::from_secs(5))
            .expect("root should call wait_agent before cancellation");
    }

    fn finish(self) {
        let _ = self.release_child.send(());
        let _ = self.shutdown.send(());
        self.server
            .join()
            .expect("subagent provider fixture should finish");
    }
}

fn read_complete_http_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = socket
            .read(&mut buffer)
            .expect("provider request should remain readable");
        assert_ne!(read, 0, "provider request must not close before its body");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).expect("provider headers are UTF-8");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| {
                value
                    .trim()
                    .parse::<usize>()
                    .expect("provider content length is numeric")
            })
        })
        .expect("provider request includes content length");
    while bytes.len() < header_end + content_length {
        let read = socket
            .read(&mut buffer)
            .expect("provider request body should remain readable");
        assert_ne!(read, 0, "provider request must not close before its body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8(bytes).expect("provider request is UTF-8")
}

fn request_model(request: &str) -> Option<String> {
    let (_, body) = request.split_once("\r\n\r\n")?;
    JsonValue::parse(body)
        .ok()?
        .as_object()?
        .get("model")?
        .as_str()
        .map(str::to_owned)
}

fn tool_call_response(id: &str, name: &str, arguments: &str) -> String {
    let arguments = JsonValue::String(arguments.into())
        .to_json_string()
        .expect("tool arguments encode");
    format!(
        r#"data: {{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"{id}","function":{{"name":"{name}","arguments":{arguments}}}}}]}},"finish_reason":"tool_calls"}}]}}

data: [DONE]

"#
    )
}

fn write_sse_response(socket: &mut TcpStream, body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .expect("fixture response headers write");
    socket
        .write_all(body.as_bytes())
        .expect("fixture response body writes");
    socket.flush().expect("fixture response flushes");
}

fn write_sse_prefix(socket: &mut TcpStream, first: &str, final_body: &str) {
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                first.len() + final_body.len()
            )
            .as_bytes(),
        )
        .expect("held fixture response headers write");
    socket
        .write_all(first.as_bytes())
        .expect("held child stream writes");
    socket.flush().expect("held child stream flushes");
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .expect("Git fixture command starts");
    assert!(
        output.status.success(),
        "git command failed in {}: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_git_workspace(tea_home: &Path) -> PathBuf {
    let workspace = tea_home.join("workspace");
    fs::create_dir(&workspace).expect("fixture workspace creates");
    git(&workspace, &["init"]);
    git(&workspace, &["config", "user.name", "Tea PTY Fixture"]);
    git(
        &workspace,
        &["config", "user.email", "pty-fixture@example.invalid"],
    );
    fs::write(workspace.join("tracked.txt"), "original\n").expect("fixture file writes");
    git(&workspace, &["add", "tracked.txt"]);
    git(&workspace, &["commit", "-m", "initial"]);
    fs::canonicalize(workspace).expect("fixture workspace canonicalizes")
}

fn assert_no_operational_worktrees(tea_home: &Path) {
    let sessions = tea_home.join("sessions");
    let Ok(workspace_directories) = fs::read_dir(sessions) else {
        return;
    };
    for workspace in workspace_directories {
        let workspace = workspace.expect("session workspace directory reads").path();
        let sessions = fs::read_dir(&workspace).expect("session directory reads");
        for session in sessions {
            let session = session.expect("session directory entry reads").path();
            let subagents = session.join("subagents");
            let Ok(leases) = fs::read_dir(subagents) else {
                continue;
            };
            for lease in leases {
                let worktree = lease
                    .expect("workspace lease directory reads")
                    .path()
                    .join("worktree");
                assert!(
                    !worktree.exists(),
                    "root shutdown must clean operational child worktree {}",
                    worktree.display()
                );
            }
        }
    }
}

#[test]
fn enabled_subagent_footer_is_visible_in_the_mutable_idle_tail() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let tea_home = pty_tea_home("subagent-footer");
    fs::write(
        tea_home.join("config.toml"),
        "[features]\nsubagents = true\n",
    )
    .expect("enabled TUI config writes");
    let scenario = Scenario::new("enabled subagent idle footer")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            "mock",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]))
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "enabled idle footer",
            |screen| screen.contains("mock/mock") && screen.contains("agents 0/4"),
        )
        .expect("enabled feature adds only the compact idle activity footer");
    assert!(
        !terminal.terminal_state().modes.alternate_screen,
        "enabled idle footer stays in the normal scrollback-native screen"
    );
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit enabled idle terminal");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("enabled footer exit restores terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap enabled footer terminal");
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn ctrl_c_commits_the_final_status_tail_before_returning_to_the_shell() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let tea_home = pty_tea_home("status-exit");
    let scenario = Scenario::new("status survives Ctrl-C")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            "mock",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]))
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "mock status before exit",
            |screen| screen.contains("mock/mock") && screen.contains("ctx ?%/16k"),
        )
        .expect("status should render before exit");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit mock terminal");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    assert!(
        terminal.screen().contains("ctx ?%/16k"),
        "the final status tail should remain in terminal scrollback after Ctrl-C"
    );
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap tea");
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn explicitly_disabled_subagents_match_missing_config_presentation_and_output_bytes() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let missing = capture_mock_idle_startup("subagents-missing", None);
    let explicitly_disabled = capture_mock_idle_startup(
        "subagents-disabled",
        Some("[features]\nsubagents = false\n"),
    );

    assert_eq!(
        tea_session::Digest::from_bytes(missing.0.as_bytes()).to_hex(),
        DISABLED_STARTUP_SCREEN_DIGEST,
        "the feature-disabled startup presentation is a pinned compatibility surface"
    );
    assert_eq!(
        tea_session::Digest::from_bytes(&missing.1).to_hex(),
        DISABLED_STARTUP_OUTPUT_DIGEST,
        "the feature-disabled startup PTY bytes are a pinned compatibility surface"
    );

    assert_eq!(
        explicitly_disabled.0, missing.0,
        "an explicit disabled config must retain the missing-config presentation exactly"
    );
    assert_eq!(
        explicitly_disabled.1, missing.1,
        "an explicit disabled config must retain every application PTY output byte"
    );
}

#[test]
fn enabled_subagents_hide_child_streaming_and_cleanup_before_ctrl_c_exit() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let tea_home = pty_tea_home("subagent-lifecycle");
    let workspace = create_git_workspace(&tea_home);
    fs::write(
        tea_home.join("config.toml"),
        format!("[features]\nsubagents = true\n\n[subagents]\nmodels = [\"{LOCAL_MODEL}\"]\n"),
    )
    .expect("enabled TUI config writes");
    let fixture = SubagentLifecycleFixture::start();
    let scenario = Scenario::new("enabled subagent lifecycle")
        .expect("valid scenario label")
        .command(
            CommandSpec::new(env!("CARGO_BIN_EXE_tea"))
                .args([
                    "--provider",
                    LOCAL_PROVIDER,
                    "--model",
                    LIFECYCLE_ROOT_MODEL,
                    "--local-base-url",
                    fixture.url.as_str(),
                    "--tea-home",
                    tea_home.to_str().expect("UTF-8 test path"),
                ])
                .current_dir(&workspace),
        )
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "enabled lifecycle idle footer",
            |screen| {
                screen.contains(&format!("{LOCAL_PROVIDER}/{LIFECYCLE_ROOT_MODEL}"))
                    && screen.contains("agents 0/4")
            },
        )
        .expect("enabled terminal should start with no active child");
    terminal
        .send_text(
            terminal.deadline(Duration::from_secs(3)),
            "delegate one live child",
        )
        .expect("submit lifecycle prompt");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "typed lifecycle prompt",
            |screen| screen.contains("delegate one live child"),
        )
        .expect("lifecycle prompt should reach the composer before submission");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit lifecycle prompt");

    fixture.wait_for_child_stream();
    fixture.wait_for_root_wait();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "live child footer count",
            |screen| screen.contains("agents 1/4"),
        )
        .expect("child acceptance should update the live footer");
    assert!(
        !terminal
            .screen()
            .contains("CHILD_STREAM_MUST_NOT_REACH_ROOT_SCROLLBACK"),
        "child streaming text must not become a root conversation row"
    );
    assert!(
        !String::from_utf8_lossy(terminal.raw_output())
            .contains("CHILD_STREAM_MUST_NOT_REACH_ROOT_SCROLLBACK"),
        "child streaming text must not enter native terminal scrollback bytes"
    );

    // This reaches `App::handle_control_c`, which keeps the root receiver as
    // the join boundary while core aborts, joins, finalizes, and cleans every
    // child. Release the held provider only after that terminal action.
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("request structured root cancellation");
    fixture.finish();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "settled child footer count",
            |screen| screen.contains("agents 0/4"),
        )
        .expect("root cancellation should join the child before becoming idle");
    assert_no_operational_worktrees(&tea_home);

    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("clear the prompt restored by structured cancellation");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit after structured cancellation has settled");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(5)))
            .expect("wait for terminal exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("structured shutdown restores terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap terminal after structured shutdown");
    assert_no_operational_worktrees(&tea_home);
    let _ = fs::remove_dir_all(tea_home);
}

fn start_overflow_fixture() -> StreamingFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("overflow fixture should bind");
    let address = listener
        .local_addr()
        .expect("overflow fixture should expose its address");
    let (first_delta_sent, first_delta) = mpsc::channel();
    let (release_response, wait_for_release) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener
            .accept()
            .expect("overflow fixture should accept one request");
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request);
        let first =
            br#"data: {"choices":[{"delta":{"content":"overflow start\n"},"finish_reason":null}]}

"#;
        let body = "committed overflow row\\n".repeat(32);
        let final_records = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{body}overflow final\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":2,\"completion_tokens\":2}}}}\n\ndata: [DONE]\n\n"
        );
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    first.len() + final_records.len()
                )
                .as_bytes(),
            )
            .expect("overflow fixture headers should write");
        socket
            .write_all(first)
            .expect("first overflow delta writes");
        socket.flush().expect("first overflow delta flushes");
        first_delta_sent
            .send(())
            .expect("test waits for the first overflow delta");
        wait_for_release
            .recv()
            .expect("test releases the overflowing completion");
        socket
            .write_all(final_records.as_bytes())
            .expect("overflow final records write");
    });
    StreamingFixture {
        first_delta,
        release_response,
        server,
        url: format!("http://{address}/v1"),
    }
}

#[test]
fn real_binary_renders_streamed_text_before_the_fixture_settles() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let fixture = StreamingFixture::start();
    let scenario = Scenario::new("streaming provider fixture")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            LOCAL_PROVIDER,
            "--model",
            FIXTURE_MODEL,
            "--local-base-url",
            fixture.url.as_str(),
        ]))
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "model readiness",
            |screen| {
                screen.contains("tea")
                    && screen.contains(&format!("{LOCAL_PROVIDER}/{FIXTURE_MODEL}"))
            },
        )
        .expect("model selection should render");
    let active = terminal.terminal_state();
    assert!(
        !active.modes.alternate_screen,
        "normal conversation remains on the main screen"
    );
    assert!(
        active.modes.bracketed_paste,
        "TerminalGuard enables bracketed paste"
    );
    assert!(
        active.modes.cursor_visible,
        "the local composer owns a visible cursor"
    );
    terminal
        .resize(Size::new(40, 10).expect("constant narrow terminal size"))
        .expect("resize through the kernel PTY");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "narrow redraw",
            |screen| {
                screen.size() == Size::new(40, 10).expect("constant narrow terminal size")
                    && screen.contains(&format!("{LOCAL_PROVIDER}/{FIXTURE_MODEL}"))
            },
        )
        .expect("application remains rendered after terminal resize");

    terminal
        .send_text(
            terminal.deadline(Duration::from_secs(3)),
            "stream offline response",
        )
        .expect("send streaming prompt");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "typed prompt",
            |screen| screen.contains("stream offline response"),
        )
        .expect("typed prompt should render");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit streaming command");
    fixture.wait_for_first_delta();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "first released streaming token",
            |screen| screen.contains("first"),
        )
        .expect("first token should render before fixture settlement");
    terminal
        .drain(terminal.deadline(Duration::from_secs(3)))
        .expect("drain available output");
    assert!(
        !terminal.screen().contains("second"),
        "terminal displayed unreleased response content"
    );

    fixture.release();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "stream completion",
            |screen| screen.contains("first") && screen.contains("second"),
        )
        .expect("complete response should render");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "idle after completion",
            |screen| {
                screen.contains(&format!("{LOCAL_PROVIDER}/{FIXTURE_MODEL}"))
                    && !screen.contains("Thinking")
            },
        )
        .expect("application should become idle");
    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), "/new")
        .expect("open a fresh linear session");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit new-session command");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "new session",
            |screen| screen.contains("new session"),
        )
        .expect("new session should reset the idle transcript");
    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), "/resume")
        .expect("open the saved-session picker");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit session-picker command");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "saved session picker",
            |screen| screen.contains("Sessions"),
        )
        .expect("settled session should be discoverable in the picker");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("resume the saved session");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "resumed session transcript",
            |screen| screen.contains("first") && screen.contains("second"),
        )
        .expect("resuming should rebuild the visible transcript");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("send clean interrupt");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("normal exit restores applicable terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap tea");
}

#[test]
fn real_binary_keeps_native_multiline_editing_and_history_inside_a_pty() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let scenario = Scenario::new("native composer interaction")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            LOCAL_PROVIDER,
            "--model",
            LOCAL_MODEL,
        ]))
        .size(Size::new(80, 16).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "local model readiness",
            |screen| {
                screen.contains("tea")
                    && screen.contains(&format!("{LOCAL_PROVIDER}/{LOCAL_MODEL}"))
                    && screen.row(2).is_some_and(|row| row.starts_with("┃"))
            },
        )
        .expect("local model selection should render");
    let startup = terminal.screen();
    assert!(startup.row(0).is_some_and(|row| row.starts_with("tea v")));
    assert!(startup.row(2).is_some_and(|row| row.starts_with("┃")));
    assert!(startup.row(4).is_some_and(|row| {
        row.starts_with(&format!("{LOCAL_PROVIDER}/{LOCAL_MODEL} · effort off"))
    }));
    let cursor = startup.cursor();
    assert_eq!((cursor.row, cursor.column, cursor.visible), (2, 2, true));
    assert!(
        startup
            .cell(2, 0)
            .is_some_and(|cell| cell.attributes().bold),
        "the composer rail retains the text emphasis role"
    );

    terminal
        .send_bytes(
            terminal.deadline(Duration::from_secs(3)),
            b"\x1b[200~first line\n  second line\x1b[201~",
        )
        .expect("send bracketed multiline paste");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "multiline composer",
            |screen| screen.contains("first line") && screen.contains("second line"),
        )
        .expect("multiline paste should remain visible in the composer");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("clear multiline composer");

    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), "/models")
        .expect("send model command");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("open model selector");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "model selector",
            |screen| {
                screen.contains("Models")
                    && screen.row(15).is_some_and(|row| row.contains("Enter"))
                    && screen.row(3).is_some_and(|row| row.starts_with("❯ "))
            },
        )
        .expect("model selector should show a selectable compiled model");
    assert!(
        terminal.terminal_state().modes.alternate_screen,
        "a temporary model-picker surface borrows the alternate screen"
    );
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Escape)
        .expect("close model selector");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "model selector closed",
            |screen| !screen.contains("Models"),
        )
        .expect("Esc should close the model selector");
    assert!(
        !terminal.terminal_state().modes.alternate_screen,
        "closing a temporary surface returns to main-screen conversation"
    );

    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), "/")
        .expect("open inline slash completion");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "slash completion menu",
            |screen| {
                screen.row(3).is_some_and(|row| row.starts_with('─'))
                    && screen.row(4).is_some_and(|row| row.starts_with("  /help"))
                    && screen
                        .row(10)
                        .is_some_and(|row| row.starts_with("↑↓ Navigate"))
            },
        )
        .expect("leading slash should open the measured inline menu");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Down)
        .expect("move slash completion selection");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "moved slash completion selection",
            |screen| {
                screen
                    .cell(5, 2)
                    .is_some_and(|cell| cell.attributes().foreground == PtyColor::Indexed(14))
            },
        )
        .expect("Down should move the accent role to the next command row");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Escape)
        .expect("close slash completion without submitting");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("clear slash completion draft");

    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), "/he")
        .expect("send command prefix");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Tab)
        .expect("complete command prefix");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "command completion",
            |screen| screen.contains("/help"),
        )
        .expect("Tab should complete the command");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit help command");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "help output",
            |screen| {
                screen.row(0).is_some_and(|row| row.starts_with("┃"))
                    && screen.contains("General")
                    && screen.contains("show keybindings and commands")
                    && screen.row(14).is_some_and(|row| row.starts_with('─'))
                    && screen
                        .row(15)
                        .is_some_and(|row| row.starts_with("↑↓ Navigate"))
            },
        )
        .expect("help command should render a temporary command surface");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Escape)
        .expect("close help surface before returning to the composer");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "help surface closed",
            |screen| screen.contains("tea") && !screen.contains("General"),
        )
        .expect("Esc should remove temporary help content");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Up)
        .expect("recall command history");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "history recall",
            |screen| screen.contains("/help"),
        )
        .expect("Up should restore the submitted command");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("clear recalled command");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("send clean interrupt");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("normal exit restores applicable terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap tea");
}

#[test]
fn real_binary_reopens_tool_detail_after_escape_in_a_pty() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let tea_home = pty_tea_home("tool-detail-reopen");
    let scenario = Scenario::new("tool detail reopen")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            "mock",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]))
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "mock readiness",
            |screen| screen.contains("mock/mock"),
        )
        .expect("mock terminal should become ready");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('o'))
        .expect("open tool detail");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "tool detail open",
            |screen| screen.contains("Full detail"),
        )
        .expect("Ctrl-O should open tool detail");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Escape)
        .expect("close tool detail");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "tool detail closed",
            |screen| !screen.contains("Full detail"),
        )
        .expect("Escape should close tool detail");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('o'))
        .expect("reopen tool detail");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "tool detail reopened",
            |screen| screen.contains("Full detail"),
        )
        .expect("Ctrl-O should reopen tool detail after Escape");

    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Escape)
        .expect("close reopened tool detail");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit mock terminal");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("tool detail exit restores terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap tea");
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn mock_keeps_submitted_user_message_visible_after_acceptance() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let tea_home = pty_tea_home("mock-submitted-message");
    let scenario = Scenario::new("mock submitted message")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            "mock",
            "--tea-home",
            tea_home.to_str().expect("UTF-8 test path"),
        ]))
        .size(Size::new(COLUMNS, ROWS).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("real tea binary should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "mock readiness",
            |screen| screen.contains("mock/mock"),
        )
        .expect("mock terminal should become ready");
    terminal
        .send_text(
            terminal.deadline(Duration::from_secs(3)),
            "submitted user message",
        )
        .expect("prompt should type");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("prompt should submit");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "submitted message",
            |screen| screen.to_string().matches("submitted user message").count() == 1,
        )
        .expect("submitted message should remain visible");

    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("cancel active mock request");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "cancelled submitted message",
            |screen| screen.contains("submitted user message"),
        )
        .expect("cancel redraw must preserve the submitted user message");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("clear restored prompt");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit mock terminal");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("mock exit restores terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap mock terminal");
    let _ = fs::remove_dir_all(tea_home);
}

#[test]
fn real_binary_keeps_an_overflowing_settled_transcript_in_main_screen_flow() {
    let _lock = PTY_TEST_LOCK.lock().expect("PTY test lock is not poisoned");
    let fixture = start_overflow_fixture();
    let scenario = Scenario::new("overflowing transcript fixture")
        .expect("valid scenario label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--provider",
            LOCAL_PROVIDER,
            "--model",
            FIXTURE_MODEL,
            "--local-base-url",
            fixture.url.as_str(),
        ]))
        .size(Size::new(40, 10).expect("constant terminal size"))
        .environment(TestEnv::hermetic().expect("create hermetic test environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("tea should start in a PTY");
    let baseline = terminal.terminal_baseline();

    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "ready",
            |screen| screen.contains(&format!("{LOCAL_PROVIDER}/{FIXTURE_MODEL}")),
        )
        .expect("overflow fixture should become ready");
    assert!(!terminal.terminal_state().modes.alternate_screen);
    terminal
        .send_text(
            terminal.deadline(Duration::from_secs(3)),
            "overflow transcript",
        )
        .expect("type overflow prompt");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "typed overflow prompt",
            |screen| screen.contains("overflow transcript"),
        )
        .expect("overflow prompt should reach the composer before submission");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit overflow prompt");
    fixture.wait_for_first_delta();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "first overflow delta",
            |screen| screen.contains("overflow start"),
        )
        .expect("the mutable suffix should stream before settlement");
    fixture.release();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "overflow completion",
            |screen| {
                let final_rows = (0..10)
                    .filter(|row| {
                        screen
                            .row(*row)
                            .is_some_and(|line| line.contains("overflow final"))
                    })
                    .count();
                screen.contains("overflow final")
                    && screen.contains(&format!("{LOCAL_PROVIDER}/{FIXTURE_MODEL}"))
                    && (0..10).any(|row| screen.row(row).is_some_and(|line| line.starts_with('┃')))
                    && final_rows == 1
            },
        )
        .expect("settled overflowing content should leave one coherent live tail");
    assert!(!terminal.terminal_state().modes.alternate_screen);
    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), "/")
        .expect("terminal remains interactive after overflowing output");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "interactive",
            |screen| screen.contains("/help"),
        )
        .expect("slash completion remains interactive");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("clear slash draft");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
        .expect("exit cleanly");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(3)))
            .expect("wait for tea exit"),
        ExitStatus::Code(0)
    );
    terminal
        .assert_terminal_restored(&baseline)
        .expect("overflow exit restores terminal modes");
    terminal
        .finish(terminal.deadline(Duration::from_secs(3)))
        .expect("reap tea");
}
