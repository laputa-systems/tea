//! Headful crash isolation with the real binary in real PTYs.
//!
//! The visible `tea` is a disposable relay; a session-owned runtime process
//! owns admitted work. These scenarios kill or hang up the relay mid-work,
//! then reattach to the exact session and check that the work settled once:
//! one provider request, one accepted input, one answer.

use ptytest::{CommandSpec, ExitStatus, Key, ProtocolProfile, PtyTest, Scenario, Size, TestEnv};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MODEL: &str = "pty-isolation-model";

static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn tea_home(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let home = std::env::temp_dir().join(format!(
        "tea-iso-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&home).expect("tea home");
    home
}

/// A local provider that streams `first ` immediately, holds the rest until
/// released, and counts every request it receives.
struct HeldProvider {
    url: String,
    requests: Arc<AtomicUsize>,
    first_delta: Receiver<()>,
    release: Sender<()>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl HeldProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}/v1", listener.local_addr().expect("address"));
        let requests = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (first_sent, first_delta) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let counted = Arc::clone(&requests);
        let stop = Arc::clone(&shutdown);
        let server = thread::spawn(move || {
            let mut released = Some(released);
            while !stop.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                socket.set_nonblocking(false).expect("blocking socket");
                let mut request = [0_u8; 65_536];
                let _ = socket.read(&mut request);
                let index = counted.fetch_add(1, Ordering::SeqCst);
                let first = "data: {\"choices\":[{\"delta\":{\"content\":\"first \"},\"finish_reason\":null}]}\n\n";
                let rest = "data: {\"choices\":[{\"delta\":{\"content\":\"second\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n";
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first.len() + rest.len()
                    )
                    .as_bytes(),
                );
                let _ = socket.write_all(first.as_bytes());
                let _ = socket.flush();
                if index == 0 {
                    let _ = first_sent.send(());
                    if let Some(released) = released.take() {
                        let _ = released.recv();
                    }
                }
                let _ = socket.write_all(rest.as_bytes());
                let _ = socket.flush();
            }
        });
        Self {
            url,
            requests,
            first_delta,
            release,
            shutdown,
            server: Some(server),
        }
    }

    fn wait_for_first_delta(&self) {
        self.first_delta
            .recv_timeout(Duration::from_secs(10))
            .expect("tea requested the held response");
    }

    fn release(&self) {
        let _ = self.release.send(());
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for HeldProvider {
    fn drop(&mut self) {
        self.release();
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn spawn_tea(home: &Path, provider: &HeldProvider, extra: &[&str], label: &str) -> PtyTest {
    let mut args = vec![
        "--tea-home".to_owned(),
        home.to_str().expect("UTF-8 home").to_owned(),
        "--provider".to_owned(),
        "local".to_owned(),
        "--local-base-url".to_owned(),
        provider.url.clone(),
    ];
    if !extra.iter().any(|arg| *arg == "--attach") {
        args.push("--model".to_owned());
        args.push(MODEL.to_owned());
    }
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    let scenario = Scenario::new(label)
        .expect("valid label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args(args))
        .size(Size::new(100, 24).expect("size"))
        .environment(TestEnv::hermetic().expect("hermetic environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    PtyTest::spawn(scenario).expect("tea starts in a PTY")
}

/// The one session directory below `home`.
fn session_directory(home: &Path) -> Option<PathBuf> {
    fs::read_dir(home.join("sessions"))
        .ok()?
        .filter_map(Result::ok)
        .flat_map(|workspace| fs::read_dir(workspace.path()).into_iter().flatten())
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "tea"))
}

/// (pid, session id) from the session's runtime record.
fn runtime_record(directory: &Path) -> Option<(u32, String)> {
    let text = fs::read_to_string(directory.join("runtime.json")).ok()?;
    let value = tea_protocol::JsonValue::parse(&text).ok()?;
    Some((
        u32::try_from(value.get("pid")?.as_u64()?).ok()?,
        value.get("session_id")?.as_str()?.to_owned(),
    ))
}

fn wait_for<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn submit(terminal: &mut PtyTest, text: &str) {
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "model ready",
            |screen| screen.contains(&format!("local/{MODEL}")),
        )
        .expect("model selected");
    terminal
        .send_text(terminal.deadline(Duration::from_secs(3)), text)
        .expect("type prompt");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(3)),
            "typed prompt",
            |screen| screen.contains(text),
        )
        .expect("prompt renders");
    terminal
        .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit prompt");
}

/// Count committed user and assistant entries by reading the inert session
/// log. Reading files neither opens nor advances the session.
fn entry_counts(directory: &Path) -> (usize, usize) {
    let mut users = 0;
    let mut assistants = 0;
    let text = fs::read_to_string(directory.join("session.jsonl")).unwrap_or_default();
    for line in text.lines() {
        let Ok(commit) = tea_protocol::JsonValue::parse(line) else {
            continue;
        };
        let items = commit
            .get("mutation")
            .and_then(|mutation| mutation.get("items"))
            .and_then(tea_protocol::JsonValue::as_array)
            .unwrap_or_default();
        for item in items {
            if item.get("kind").and_then(tea_protocol::JsonValue::as_str) != Some("entry") {
                continue;
            }
            match item
                .get("payload")
                .and_then(|payload| payload.get("entry"))
                .and_then(|entry| entry.get("type"))
                .and_then(tea_protocol::JsonValue::as_str)
            {
                Some("user_message") => users += 1,
                Some("assistant_message") => assistants += 1,
                _ => {}
            }
        }
    }
    (users, assistants)
}

/// Quit normally. Ctrl+C first settles anything the session still holds
/// open (such as an interrupted operation awaiting recovery), then exits.
fn quit(terminal: PtyTest) {
    let _ = quit_with(terminal, ExitStatus::Code(0));
}

fn quit_with(mut terminal: PtyTest, expected: ExitStatus) -> String {
    for _ in 0..4 {
        terminal
            .send_key(terminal.deadline(Duration::from_secs(3)), Key::Ctrl('c'))
            .expect("quit");
        match terminal.wait_for_exit(terminal.deadline(Duration::from_secs(3))) {
            Ok(status) => {
                assert_eq!(status, expected);
                let output = String::from_utf8_lossy(terminal.raw_output()).into_owned();
                terminal
                    .finish(terminal.deadline(Duration::from_secs(3)))
                    .expect("reap");
                return output;
            }
            Err(_) => continue,
        }
    }
    panic!("tea did not quit");
}

#[test]
fn killing_the_terminal_mid_generation_keeps_the_work_and_reattach_restores_it_once() {
    let _lock = lock();
    let home = tea_home("kill");
    let provider = HeldProvider::start();
    let mut terminal = spawn_tea(&home, &provider, &[], "isolation kill");
    submit(&mut terminal, "survive the terminal");
    provider.wait_for_first_delta();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "first delta",
            |screen| screen.contains("first"),
        )
        .expect("streamed text renders");
    let directory = wait_for("session directory", Duration::from_secs(5), || {
        session_directory(&home)
    });
    let (runtime, session) = wait_for("runtime record", Duration::from_secs(5), || {
        runtime_record(&directory)
    });

    // Kill the visible process group: the relay. The runtime is in its own
    // process group and holds no terminal, so it survives.
    terminal.signal(9).expect("kill the terminal relay");
    assert!(matches!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(5)))
            .expect("relay exits"),
        ExitStatus::Signal(9)
    ));
    let _ = terminal.finish(terminal.deadline(Duration::from_secs(3)));
    assert!(alive(runtime), "the session runtime outlives its terminal");

    // The admitted generation completes and settles durably with no
    // terminal attached; the detached runtime then exits by itself.
    provider.release();
    wait_for("detached runtime exit", Duration::from_secs(15), || {
        (!alive(runtime)).then_some(())
    });
    assert!(runtime_record(&directory).is_none(), "the record is withdrawn");
    assert_eq!(entry_counts(&directory), (1, 1));
    assert_eq!(provider.request_count(), 1);

    // Reattach by exact session: no runtime remains, so the session is
    // reopened from durable state and shows the settled answer.
    let mut reattached = spawn_tea(&home, &provider, &["--attach", &session], "isolation reattach");
    reattached
        .wait_for_screen(
            reattached.deadline(Duration::from_secs(10)),
            "restored transcript",
            |screen| screen.contains("survive the terminal") && screen.contains("first second"),
        )
        .expect("the reopened session shows the settled turn");
    quit(reattached);
    // Nothing was replayed: still one request, one input, one answer.
    assert_eq!(provider.request_count(), 1);
    assert_eq!(entry_counts(&directory), (1, 1));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn a_hung_up_terminal_can_reattach_to_the_live_runtime_while_work_continues() {
    let _lock = lock();
    let home = tea_home("live");
    let provider = HeldProvider::start();
    let mut terminal = spawn_tea(&home, &provider, &[], "isolation hangup");
    submit(&mut terminal, "keep running");
    provider.wait_for_first_delta();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "first delta",
            |screen| screen.contains("first"),
        )
        .expect("streamed text renders");
    let directory = wait_for("session directory", Duration::from_secs(5), || {
        session_directory(&home)
    });
    let (runtime, session) = wait_for("runtime record", Duration::from_secs(5), || {
        runtime_record(&directory)
    });

    // Terminal disappearance (the PTY master closes).
    terminal.hangup();
    let _ = terminal.wait_for_exit(terminal.deadline(Duration::from_secs(5)));
    let _ = terminal.finish(terminal.deadline(Duration::from_secs(3)));
    assert!(alive(runtime));

    // Reattach to the still-working runtime: the live view is presented
    // again from runtime state, including the in-flight streamed text.
    let mut reattached = spawn_tea(&home, &provider, &["--attach", &session], "isolation live");
    reattached
        .wait_for_screen(
            reattached.deadline(Duration::from_secs(10)),
            "live view",
            |screen| screen.contains("keep running") && screen.contains("first"),
        )
        .expect("the live runtime re-presents its view");

    // A second terminal cannot attach while one is attached.
    let mut competing =
        spawn_tea(&home, &provider, &["--attach", &session], "isolation competing");
    assert_eq!(
        competing
            .wait_for_exit(competing.deadline(Duration::from_secs(10)))
            .expect("competing attach exits"),
        ExitStatus::Code(2)
    );
    assert!(competing.raw_output().windows(16).any(|window| window == b"already attached"));
    let _ = competing.finish(competing.deadline(Duration::from_secs(3)));

    provider.release();
    reattached
        .wait_for_screen(
            reattached.deadline(Duration::from_secs(10)),
            "settled answer",
            |screen| screen.contains("first second"),
        )
        .expect("the work finishes in the reattached view");
    // Quit only once idle so this is a normal quit, not a cancellation.
    let deadline = Instant::now() + Duration::from_secs(10);
    while entry_counts(&directory).1 == 0 {
        assert!(Instant::now() < deadline, "answer never became durable");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(200));
    quit(reattached);
    wait_for("runtime exit after quit", Duration::from_secs(10), || {
        (!alive(runtime)).then_some(())
    });
    assert_eq!(provider.request_count(), 1);
    assert_eq!(entry_counts(&directory), (1, 1));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn stale_records_are_ignored_and_an_idle_detached_runtime_exits() {
    let _lock = lock();
    let home = tea_home("stale");
    let provider = HeldProvider::start();
    provider.release();
    let mut terminal = spawn_tea(&home, &provider, &[], "isolation idle");
    submit(&mut terminal, "quick");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(10)),
            "answer",
            |screen| screen.contains("first second"),
        )
        .expect("answer renders");
    let directory = wait_for("session directory", Duration::from_secs(5), || {
        session_directory(&home)
    });
    let (runtime, session) = wait_for("runtime record", Duration::from_secs(5), || {
        runtime_record(&directory)
    });
    while entry_counts(&directory).1 == 0 {
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(200));

    // Losing the terminal while idle: the runtime does not linger.
    terminal.signal(9).expect("kill the relay");
    let _ = terminal.wait_for_exit(terminal.deadline(Duration::from_secs(5)));
    let _ = terminal.finish(terminal.deadline(Duration::from_secs(3)));
    wait_for("idle detached runtime exit", Duration::from_secs(10), || {
        (!alive(runtime)).then_some(())
    });
    assert!(runtime_record(&directory).is_none());

    // A stale record (dead process, missing socket) does not block reopening.
    fs::write(
        directory.join("runtime.json"),
        format!(
            "{{\"protocol\":1,\"pid\":{runtime},\"socket\":\"/tmp/tea-missing.sock\",\"session_id\":\"{session}\"}}"
        ),
    )
    .expect("stale record");
    let mut reopened = spawn_tea(&home, &provider, &["--attach", &session], "isolation stale");
    reopened
        .wait_for_screen(
            reopened.deadline(Duration::from_secs(10)),
            "reopened",
            |screen| screen.contains("quick") && screen.contains("first second"),
        )
        .expect("a stale record falls back to reopening the session");
    quit(reopened);
    assert_eq!(provider.request_count(), 1);
    assert_eq!(entry_counts(&directory), (1, 1));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn a_runtime_failure_restores_the_terminal_and_leaves_honest_recovery() {
    let _lock = lock();
    let home = tea_home("crash");
    let provider = HeldProvider::start();
    let mut terminal = spawn_tea(&home, &provider, &[], "isolation runtime crash");
    let baseline = terminal.terminal_baseline();
    submit(&mut terminal, "ambiguous work");
    provider.wait_for_first_delta();
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(5)),
            "first delta",
            |screen| screen.contains("first"),
        )
        .expect("streamed text renders");
    let directory = wait_for("session directory", Duration::from_secs(5), || {
        session_directory(&home)
    });
    let (runtime, session) = wait_for("runtime record", Duration::from_secs(5), || {
        runtime_record(&directory)
    });

    // The runtime dies mid-request: the relay reports it and restores the
    // terminal instead of hanging.
    Command::new("kill")
        .args(["-9", &runtime.to_string()])
        .status()
        .expect("kill runtime");
    assert_eq!(
        terminal
            .wait_for_exit(terminal.deadline(Duration::from_secs(10)))
            .expect("relay exits"),
        ExitStatus::Code(2)
    );
    let output = String::from_utf8_lossy(terminal.raw_output()).into_owned();
    assert!(output.contains("session runtime exited unexpectedly"), "{output}");
    terminal
        .assert_terminal_restored(&baseline)
        .expect("relay restores terminal modes");
    let _ = terminal.finish(terminal.deadline(Duration::from_secs(3)));

    // Reattaching reopens the session; the interrupted request is an
    // ambiguous effect, so durable recovery applies rather than a replay.
    provider.release();
    let mut reopened = spawn_tea(&home, &provider, &["--attach", &session], "isolation recover");
    reopened
        .wait_for_screen(
            reopened.deadline(Duration::from_secs(10)),
            "reopened",
            |screen| screen.contains("ambiguous work"),
        )
        .expect("the accepted input is still there");
    reopened
        .send_text(reopened.deadline(Duration::from_secs(3)), "another")
        .expect("type");
    reopened
        .send_key(reopened.deadline(Duration::from_secs(3)), Key::Enter)
        .expect("submit");
    reopened
        .wait_for_screen(
            reopened.deadline(Duration::from_secs(10)),
            "recovery notice",
            |screen| screen.contains("/continue"),
        )
        .expect("recovery is explicit");
    // Quitting does not hide the ambiguous effect either: it reports it.
    let output = quit_with(reopened, ExitStatus::Code(2));
    assert!(output.contains("requires recovery"), "{output}");
    assert_eq!(provider.request_count(), 1, "nothing was replayed implicitly");
    assert_eq!(entry_counts(&directory), (1, 0));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn switching_sessions_hands_off_to_a_fresh_runtime_instead_of_reusing_one() {
    let _lock = lock();
    let home = tea_home("switch");
    let provider = HeldProvider::start();
    provider.release();
    let mut terminal = spawn_tea(&home, &provider, &[], "isolation switch");
    submit(&mut terminal, "first session");
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(10)),
            "answer",
            |screen| screen.contains("first second"),
        )
        .expect("answer renders");
    let first = wait_for("first session", Duration::from_secs(5), || session_directory(&home));
    let (runtime, _) = wait_for("first record", Duration::from_secs(5), || runtime_record(&first));
    while entry_counts(&first).1 == 0 {
        thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..50 {
        terminal
            .send_text(terminal.deadline(Duration::from_secs(3)), "/new")
            .expect("type /new");
        terminal
            .send_key(terminal.deadline(Duration::from_secs(3)), Key::Enter)
            .expect("submit /new");
        terminal
            .wait_for_screen(
                terminal.deadline(Duration::from_secs(3)),
                "/new outcome",
                |screen| screen.contains("new session"),
            )
            .expect("a /new outcome renders");
        if !terminal.screen().contains("requires an idle agent") {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    submit(&mut terminal, "second session");
    let second = wait_for("second session", Duration::from_secs(10), || {
        fs::read_dir(home.join("sessions"))
            .ok()?
            .filter_map(Result::ok)
            .flat_map(|workspace| fs::read_dir(workspace.path()).into_iter().flatten())
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path != &first && path.extension().is_some_and(|ext| ext == "tea"))
    });
    let (second_runtime, _) =
        wait_for("second record", Duration::from_secs(5), || runtime_record(&second));
    // A session runtime is never repurposed: `/new` ended the first runtime
    // and the relay started a fresh one bound to the new session.
    assert_ne!(second_runtime, runtime);
    wait_for("first runtime exit", Duration::from_secs(10), || {
        (!alive(runtime)).then_some(())
    });
    assert!(runtime_record(&first).is_none());
    quit(terminal);
    wait_for("second runtime exit", Duration::from_secs(10), || {
        (!alive(second_runtime)).then_some(())
    });
    assert!(runtime_record(&second).is_none());
    let _ = fs::remove_dir_all(home);
}

/// A local provider that answers the first request with a `bash` tool call
/// and the second with text, counting requests.
struct ToolProvider {
    url: String,
    requests: Arc<AtomicUsize>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl ToolProvider {
    fn start(command: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture binds");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}/v1", listener.local_addr().expect("address"));
        let requests = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let counted = Arc::clone(&requests);
        let stop = Arc::clone(&shutdown);
        let arguments = tea_protocol::JsonValue::String(
            tea_protocol::JsonValue::object([("command", tea_protocol::JsonValue::from(command))])
                .to_json_string()
                .expect("arguments"),
        )
        .to_json_string()
        .expect("encoded arguments");
        let server = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                socket.set_nonblocking(false).expect("blocking socket");
                let mut request = [0_u8; 65_536];
                let _ = socket.read(&mut request);
                let body = if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call-bash\",\"function\":{{\"name\":\"bash\",\"arguments\":{arguments}}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                    )
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"bash finished\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned()
                };
                let _ = socket.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = socket.flush();
            }
        });
        Self {
            url,
            requests,
            shutdown,
            server: Some(server),
        }
    }
}

impl Drop for ToolProvider {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

#[test]
fn losing_the_terminal_during_a_bash_tool_wait_lets_the_runtime_finish_the_turn() {
    let _lock = lock();
    let home = tea_home("bash");
    let workspace = home.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let gate = workspace.join("go");
    let marker = workspace.join("done.txt");
    // The command waits for the test, so the terminal is lost mid-tool.
    let command = format!(
        "while [ ! -e {} ]; do sleep 0.05; done; echo finished > {}",
        gate.display(),
        marker.display()
    );
    let provider = ToolProvider::start(&command);
    let scenario = Scenario::new("isolation bash")
        .expect("valid label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--tea-home",
            home.to_str().expect("UTF-8"),
            "--cwd",
            workspace.to_str().expect("UTF-8"),
            "--provider",
            "local",
            "--model",
            MODEL,
            "--local-base-url",
            provider.url.as_str(),
        ]))
        .size(Size::new(100, 24).expect("size"))
        .environment(TestEnv::hermetic().expect("hermetic environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut terminal = PtyTest::spawn(scenario).expect("tea starts");
    submit(&mut terminal, "run the slow command");
    wait_for("bash request", Duration::from_secs(10), || {
        (provider.requests.load(Ordering::SeqCst) >= 1).then_some(())
    });
    terminal
        .wait_for_screen(
            terminal.deadline(Duration::from_secs(10)),
            "bash running",
            |screen| screen.contains("bash"),
        )
        .expect("the tool row renders");
    let directory = wait_for("session directory", Duration::from_secs(5), || {
        session_directory(&home)
    });
    let (runtime, session) = wait_for("runtime record", Duration::from_secs(5), || {
        runtime_record(&directory)
    });
    terminal.signal(9).expect("kill the relay");
    let _ = terminal.wait_for_exit(terminal.deadline(Duration::from_secs(5)));
    let _ = terminal.finish(terminal.deadline(Duration::from_secs(3)));
    assert!(alive(runtime));

    // The runtime-owned command keeps running and completes; the runtime
    // then asks the model for the next step and settles the turn.
    fs::write(&gate, "").expect("release the command");
    wait_for("command completion", Duration::from_secs(15), || {
        marker.exists().then_some(())
    });
    wait_for("detached runtime exit", Duration::from_secs(20), || {
        (!alive(runtime)).then_some(())
    });
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
    assert_eq!(entry_counts(&directory), (1, 2));

    let scenario = Scenario::new("isolation bash reattach")
        .expect("valid label")
        .command(CommandSpec::new(env!("CARGO_BIN_EXE_tea")).args([
            "--tea-home",
            home.to_str().expect("UTF-8"),
            "--cwd",
            workspace.to_str().expect("UTF-8"),
            "--provider",
            "local",
            "--local-base-url",
            provider.url.as_str(),
            "--attach",
            session.as_str(),
        ]))
        .size(Size::new(100, 24).expect("size"))
        .environment(TestEnv::hermetic().expect("hermetic environment"))
        .protocol_profile(ProtocolProfile::xterm_minimal_v1());
    let mut reopened = PtyTest::spawn(scenario).expect("tea reattaches");
    reopened
        .wait_for_screen(
            reopened.deadline(Duration::from_secs(10)),
            "settled turn",
            |screen| screen.contains("bash finished"),
        )
        .expect("the settled tool turn is visible");
    quit(reopened);
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
    let _ = fs::remove_dir_all(home);
}
