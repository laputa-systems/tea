use ptytest::{
    Color as PtyColor, CommandSpec, ExitStatus, Key, ProtocolProfile, PtyTest, Scenario, Size,
    TestEnv,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

const ROWS: u16 = 24;
const COLUMNS: u16 = 100;
const LOCAL_PROVIDER: &str = "local";
const LOCAL_MODEL: &str = tea_providers::local::LAGUNA_XS_2_1_MODEL;
const FIXTURE_MODEL: &str = "pty-fixture-model";

// These real-binary scenarios each own a terminal and a loopback server. Keep
// their timing barriers independent instead of making one PTY's progress
// depend on another test thread receiving CPU time.
static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

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
        .send_text(terminal.deadline(Duration::from_secs(3)), "/session")
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
        .send_text(terminal.deadline(Duration::from_secs(3)), "/model")
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
                        .row(11)
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
