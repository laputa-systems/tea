//! Crash isolation for headful sessions.
//!
//! A headful `tea` is two processes. The **runtime** process is the ordinary
//! terminal application — durable harness, providers, tools, subagents, MCP
//! servers — rendering into a virtual terminal. The **relay** process is what
//! the user sees: it owns the real terminal's raw mode and forwards input
//! bytes and window size to the runtime and output bytes back, over one
//! private Unix socket. The relay holds no semantic state or authority, so
//! killing it (or closing its terminal) leaves admitted work running; the
//! runtime finishes the work, settles it durably, and exits once it is idle
//! with no terminal attached.
//!
//! The boundary is one-to-one and session-local. A runtime serves exactly one
//! terminal at a time and owns at most one root session (switching with `/new`
//! or `/resume` replaces it in the same process). While attached to a session
//! it publishes `runtime.json` in that session's directory; `tea --attach ID`
//! reads that record directly and connects to that one runtime, which also
//! confirms the session in the handshake. There is no broker, no shared
//! listener, and no enumeration of live runtimes.
//!
//! Wire format: frames of `[kind: u8][length: u32 big-endian][payload]`.

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::retry_on_intr;
use rustix::termios::{tcgetattr, tcgetwinsize, tcsetattr, OptionalActions};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tea_protocol::JsonValue;

/// Environment variable that starts the runtime half; set only by the relay.
pub const RUNTIME_SOCKET_ENV: &str = "TEA_RUNTIME_SOCKET";
/// Environment variable that keeps a headful session in one process.
pub const IN_PROCESS_ENV: &str = "TEA_IN_PROCESS";
/// Name of the attachment record inside a session directory.
pub const ATTACHMENT_RECORD: &str = "runtime.json";

const PROTOCOL_VERSION: u64 = 1;
const MAX_FRAME: usize = 4 * 1024 * 1024;

const HELLO: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const DETACH: u8 = 4;
const WELCOME: u8 = 10;
const OUTPUT: u8 = 11;
const CLOSE: u8 = 12;
const REFUSE: u8 = 13;

fn write_frame(stream: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut header = [0_u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&length.to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_frame(stream: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0_u8; 5];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok(Some((header[0], payload)))
}

fn json_payload(value: &JsonValue) -> Vec<u8> {
    value.to_json_string().unwrap_or_default().into_bytes()
}

fn parse_payload(payload: &[u8]) -> Option<JsonValue> {
    JsonValue::parse(std::str::from_utf8(payload).ok()?).ok()
}

/// A private directory for runtime sockets.
///
/// Socket paths are short (`/tmp/tea-<owner>-<home digest>/<id>.sock`)
/// because session directories can exceed the platform's socket-path limit.
/// The directory is created `0700` and must belong to the owner of the Tea
/// home; it is never listed.
pub fn socket_directory(tea_home: &Path) -> io::Result<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(tea_home)?;
    let owner = fs::metadata(tea_home)?.uid();
    let digest = tea_session::Digest::from_bytes(tea_home.to_string_lossy().as_bytes()).to_hex();
    let directory = PathBuf::from("/tmp").join(format!("tea-{owner}-{}", &digest[..16]));
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime socket directory {} is not a private directory owned by this user",
                directory.display()
            ),
        ));
    }
    Ok(directory)
}

/// A fresh socket path for one runtime.
pub fn new_socket_path(tea_home: &Path) -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    Ok(socket_directory(tea_home)?.join(format!("{}-{nanos:08x}.sock", std::process::id())))
}

/// The runtime's attachment record in a session directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachmentRecord {
    /// Runtime process id.
    pub pid: u32,
    /// Runtime socket.
    pub socket: PathBuf,
    /// The session the runtime serves.
    pub session_id: String,
}

impl AttachmentRecord {
    /// Write atomically into `session_directory`.
    pub fn publish(&self, session_directory: &Path) -> io::Result<()> {
        let body = JsonValue::object([
            ("protocol", JsonValue::from(PROTOCOL_VERSION)),
            ("pid", JsonValue::from(u64::from(self.pid))),
            (
                "socket",
                JsonValue::from(self.socket.to_string_lossy().as_ref()),
            ),
            ("session_id", JsonValue::from(self.session_id.as_str())),
        ]);
        let temporary = session_directory.join(format!(".{ATTACHMENT_RECORD}.{}", self.pid));
        fs::write(&temporary, json_payload(&body))?;
        fs::rename(&temporary, session_directory.join(ATTACHMENT_RECORD))
    }

    /// Read a session's record, if one exists.
    pub fn read(session_directory: &Path) -> io::Result<Option<Self>> {
        let bytes = match fs::read(session_directory.join(ATTACHMENT_RECORD)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid runtime.json");
        let value = parse_payload(&bytes).ok_or_else(invalid)?;
        Ok(Some(Self {
            pid: value
                .get("pid")
                .and_then(JsonValue::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(invalid)?,
            socket: PathBuf::from(
                value
                    .get("socket")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(invalid)?,
            ),
            session_id: value
                .get("session_id")
                .and_then(JsonValue::as_str)
                .ok_or_else(invalid)?
                .to_owned(),
        }))
    }

    /// Remove the record if it still names this runtime.
    pub fn retract(session_directory: &Path, pid: u32) {
        if Self::read(session_directory)
            .ok()
            .flatten()
            .is_some_and(|record| record.pid == pid)
        {
            let _ = fs::remove_file(session_directory.join(ATTACHMENT_RECORD));
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime side
// ---------------------------------------------------------------------------

struct LinkState {
    stream: Option<UnixStream>,
    input: VecDeque<u8>,
    size: (u16, u16),
    /// Increments on every new attachment.
    generation: u64,
    /// Session the runtime currently serves, for handshake checks.
    session: Option<String>,
    closed: bool,
}

/// The runtime's end of the relay connection: a virtual terminal.
pub struct RemoteLink {
    socket: PathBuf,
    state: Mutex<LinkState>,
    changed: Condvar,
}

impl std::fmt::Debug for RemoteLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteLink")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl RemoteLink {
    /// Bind the runtime socket and start accepting one terminal at a time.
    pub fn bind(socket: PathBuf) -> io::Result<Arc<Self>> {
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let link = Arc::new(Self {
            socket,
            state: Mutex::new(LinkState {
                stream: None,
                input: VecDeque::new(),
                size: (80, 24),
                generation: 0,
                session: None,
                closed: false,
            }),
            changed: Condvar::new(),
        });
        let accepting = Arc::clone(&link);
        std::thread::Builder::new()
            .name("tea-runtime-accept".into())
            .spawn(move || accepting.accept_loop(listener))?;
        Ok(link)
    }

    /// The bound socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LinkState> {
        self.state.lock().expect("runtime link poisoned")
    }

    fn accept_loop(self: Arc<Self>, listener: UnixListener) {
        for stream in listener.incoming() {
            if self.lock().closed {
                return;
            }
            let Ok(stream) = stream else { continue };
            let link = Arc::clone(&self);
            let _ = std::thread::Builder::new()
                .name("tea-runtime-terminal".into())
                .spawn(move || link.serve(stream));
        }
    }

    fn serve(&self, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let hello = match read_frame(&mut stream) {
            Ok(Some((HELLO, payload))) => parse_payload(&payload),
            _ => None,
        };
        let Some(hello) = hello else {
            let _ = write_frame(
                &mut stream,
                REFUSE,
                &json_payload(&JsonValue::object([(
                    "message",
                    JsonValue::from("expected a tea terminal handshake"),
                )])),
            );
            return;
        };
        let _ = stream.set_read_timeout(None);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let refuse = |stream: &mut UnixStream, message: String| {
            let _ = write_frame(
                stream,
                REFUSE,
                &json_payload(&JsonValue::object([("message", JsonValue::from(message))])),
            );
        };
        {
            let mut state = self.lock();
            if hello.get("protocol").and_then(JsonValue::as_u64) != Some(PROTOCOL_VERSION) {
                drop(state);
                refuse(&mut stream, "incompatible tea terminal protocol".into());
                return;
            }
            if state.closed {
                drop(state);
                refuse(&mut stream, "the session runtime is shutting down".into());
                return;
            }
            if state.stream.is_some() {
                drop(state);
                refuse(
                    &mut stream,
                    "another terminal is attached to this session runtime".into(),
                );
                return;
            }
            if let Some(expected) = hello.get("session").and_then(JsonValue::as_str) {
                if state.session.as_deref() != Some(expected) {
                    let serving = state.session.clone().unwrap_or_else(|| "no session".into());
                    drop(state);
                    refuse(
                        &mut stream,
                        format!("this runtime now serves {serving}, not session {expected}"),
                    );
                    return;
                }
            }
            let size = (
                hello
                    .get("cols")
                    .and_then(JsonValue::as_u64)
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(80),
                hello
                    .get("rows")
                    .and_then(JsonValue::as_u64)
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(24),
            );
            let welcome = JsonValue::object([(
                "session",
                state
                    .session
                    .as_deref()
                    .map_or(JsonValue::Null, JsonValue::from),
            )]);
            if write_frame(&mut stream, WELCOME, &json_payload(&welcome)).is_err() {
                return;
            }
            let Ok(writer) = stream.try_clone() else {
                return;
            };
            state.stream = Some(writer);
            state.size = size;
            state.generation += 1;
            state.input.clear();
        }
        self.changed.notify_all();
        let generation = self.lock().generation;
        loop {
            match read_frame(&mut stream) {
                Ok(Some((INPUT, payload))) => {
                    self.lock().input.extend(payload);
                    self.changed.notify_all();
                }
                Ok(Some((RESIZE, payload))) if payload.len() == 4 => {
                    self.lock().size = (
                        u16::from_be_bytes([payload[0], payload[1]]),
                        u16::from_be_bytes([payload[2], payload[3]]),
                    );
                    self.changed.notify_all();
                }
                Ok(Some((DETACH, _))) | Ok(None) | Err(_) => break,
                Ok(Some(_)) => {}
            }
        }
        let mut state = self.lock();
        if state.generation == generation {
            state.stream = None;
            state.input.clear();
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Whether no terminal is attached.
    pub fn is_detached(&self) -> bool {
        self.lock().stream.is_none()
    }

    /// Attachment generation; changes when a terminal attaches.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Current virtual terminal size.
    pub fn size(&self) -> (u16, u16) {
        self.lock().size
    }

    /// Record the session this runtime serves, for attach handshakes.
    pub fn set_session(&self, session: Option<String>) {
        self.lock().session = session;
    }

    /// Wait up to `timeout` for input, a resize, or an attachment change,
    /// then take pending input bytes.
    pub fn wait_input(&self, timeout: Duration, seen: (u64, (u16, u16))) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while state.input.is_empty() && (state.generation, state.size) == seen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .expect("runtime link poisoned")
                .0;
        }
        state.input.drain(..).collect()
    }

    /// Send output to the attached terminal; discarded while detached.
    pub fn write_output(&self, bytes: &[u8]) {
        let mut state = self.lock();
        if let Some(stream) = state.stream.as_mut() {
            if write_frame(stream, OUTPUT, bytes).is_err() {
                // A terminal that cannot keep up is treated as gone; the
                // runtime keeps working and the user can reattach.
                if let Some(stream) = state.stream.take() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }

    /// Tell the attached terminal the runtime is finishing, then stop
    /// accepting terminals and remove the socket.
    pub fn close(&self, code: u8, message: Option<&str>) {
        let mut state = self.lock();
        state.closed = true;
        if let Some(mut stream) = state.stream.take() {
            let body = JsonValue::object([
                ("code", JsonValue::from(u64::from(code))),
                ("message", message.map_or(JsonValue::Null, JsonValue::from)),
            ]);
            let _ = write_frame(&mut stream, CLOSE, &json_payload(&body));
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        drop(state);
        // Wake the accept loop so its thread can observe `closed`, then
        // remove the socket (and its private directory once empty).
        let _ = UnixStream::connect(&self.socket);
        let _ = fs::remove_file(&self.socket);
        if let Some(directory) = self.socket.parent() {
            let _ = fs::remove_dir(directory);
        }
        self.changed.notify_all();
    }
}

/// `Write` adapter for the runtime's renderer.
#[derive(Clone, Debug)]
pub struct RemoteOutput {
    link: Arc<RemoteLink>,
    buffer: Vec<u8>,
}

impl RemoteOutput {
    /// Buffer renderer output for one link.
    pub fn new(link: Arc<RemoteLink>) -> Self {
        Self {
            link,
            buffer: Vec::new(),
        }
    }
}

impl Write for RemoteOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= 64 * 1024 {
            self.flush()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.link.write_output(&self.buffer);
            self.buffer.clear();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Relay side
// ---------------------------------------------------------------------------

/// How a relay session ended.
#[derive(Debug, Eq, PartialEq)]
pub enum RelayEnd {
    /// The runtime finished and reported its exit status.
    Closed { code: u8, message: Option<String> },
    /// The runtime refused the attachment.
    Refused(String),
    /// The runtime connection ended without a close (runtime failure).
    Lost,
    /// The local terminal went away.
    TerminalGone,
}

/// Connect to a runtime and perform the handshake. `session` is the exact
/// session an `--attach` expects; a fresh runtime is attached without one.
pub fn connect(socket: &Path, session: Option<&str>, size: (u16, u16)) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
    let hello = JsonValue::object([
        ("protocol", JsonValue::from(PROTOCOL_VERSION)),
        ("session", session.map_or(JsonValue::Null, JsonValue::from)),
        ("cols", JsonValue::from(u64::from(size.0))),
        ("rows", JsonValue::from(u64::from(size.1))),
    ]);
    write_frame(&mut stream, HELLO, &json_payload(&hello)).map_err(|error| error.to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    match read_frame(&mut stream) {
        Ok(Some((WELCOME, _))) => {
            let _ = stream.set_read_timeout(None);
            Ok(stream)
        }
        Ok(Some((REFUSE, payload))) => Err(parse_payload(&payload)
            .and_then(|value| value.get("message").and_then(JsonValue::as_str).map(str::to_owned))
            .unwrap_or_else(|| "the runtime refused the terminal".into())),
        _ => Err("the runtime did not complete the terminal handshake".into()),
    }
}

/// Relay the controlling terminal to a connected runtime until it ends.
///
/// The relay owns raw mode on the real terminal and restores it on every
/// exit path. It writes nothing of its own except, after a runtime failure,
/// the sequences that leave the terminal usable.
pub fn relay(stream: UnixStream) -> io::Result<RelayEnd> {
    let stdin = io::stdin();
    let original = tcgetattr(&stdin)?;
    let mut raw = original.clone();
    raw.make_raw();
    tcsetattr(&stdin, OptionalActions::Now, &raw)?;
    let result = relay_raw(&stdin, stream);
    let _ = tcsetattr(&stdin, OptionalActions::Now, &original);
    if matches!(result, Ok(RelayEnd::Lost) | Err(_)) {
        // The runtime could not restore its modes: leave the alternate
        // screen, show the cursor, and stop bracketed paste.
        let mut stdout = io::stdout();
        let _ = stdout.write_all(b"\x1b[?1049l\x1b[?2004l\x1b[?25h\r\n");
        let _ = stdout.flush();
    }
    result
}

fn terminal_size(stdin: &io::Stdin) -> (u16, u16) {
    retry_on_intr(|| tcgetwinsize(stdin))
        .map(|size| (size.ws_col, size.ws_row))
        .unwrap_or((80, 24))
}

/// The terminal size the relay will report.
pub fn current_size() -> (u16, u16) {
    terminal_size(&io::stdin())
}

fn relay_raw(stdin: &io::Stdin, stream: UnixStream) -> io::Result<RelayEnd> {
    let end: Arc<Mutex<Option<RelayEnd>>> = Arc::default();
    let mut reader = stream.try_clone()?;
    let reader_end = Arc::clone(&end);
    let output = std::thread::Builder::new()
        .name("tea-relay-output".into())
        .spawn(move || {
            let mut stdout = io::stdout();
            let finish = loop {
                match read_frame(&mut reader) {
                    Ok(Some((OUTPUT, bytes))) => {
                        if stdout.write_all(&bytes).and_then(|()| stdout.flush()).is_err() {
                            break RelayEnd::TerminalGone;
                        }
                    }
                    Ok(Some((CLOSE, payload))) => {
                        let value = parse_payload(&payload);
                        break RelayEnd::Closed {
                            code: value
                                .as_ref()
                                .and_then(|value| value.get("code"))
                                .and_then(JsonValue::as_u64)
                                .and_then(|code| u8::try_from(code).ok())
                                .unwrap_or(0),
                            message: value
                                .as_ref()
                                .and_then(|value| value.get("message"))
                                .and_then(JsonValue::as_str)
                                .map(str::to_owned),
                        };
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break RelayEnd::Lost,
                }
            };
            *reader_end.lock().expect("relay end poisoned") = Some(finish);
        })?;
    let mut writer = stream;
    let mut size = terminal_size(stdin);
    let mut input = stdin.lock();
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 20_000_000,
    };
    loop {
        if let Some(end) = end.lock().expect("relay end poisoned").take() {
            let _ = output.join();
            return Ok(end);
        }
        let ready = retry_on_intr(|| {
            let mut fds = [PollFd::new(&*stdin, PollFlags::IN)];
            poll(&mut fds, Some(&timeout))
        })?;
        if ready != 0 {
            let mut bytes = [0_u8; 4096];
            let count = match input.read(&mut bytes) {
                Ok(0) | Err(_) => {
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    let _ = output.join();
                    return Ok(RelayEnd::TerminalGone);
                }
                Ok(count) => count,
            };
            if write_frame(&mut writer, INPUT, &bytes[..count]).is_err() {
                let _ = output.join();
                return Ok(end.lock().expect("relay end poisoned").take().unwrap_or(RelayEnd::Lost));
            }
        }
        let current = terminal_size(stdin);
        if current != size {
            size = current;
            let mut payload = [0_u8; 4];
            payload[..2].copy_from_slice(&size.0.to_be_bytes());
            payload[2..].copy_from_slice(&size.1.to_be_bytes());
            let _ = write_frame(&mut writer, RESIZE, &payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_reject_oversized_lengths() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, INPUT, b"abc").expect("write");
        let mut cursor = io::Cursor::new(buffer);
        assert_eq!(
            read_frame(&mut cursor).expect("read"),
            Some((INPUT, b"abc".to_vec()))
        );
        assert_eq!(read_frame(&mut cursor).expect("eof"), None);
        let mut oversized = io::Cursor::new(vec![INPUT, 0xff, 0xff, 0xff, 0xff]);
        assert!(read_frame(&mut oversized).is_err());
    }

    fn temporary(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tea-detach-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("temporary directory");
        path
    }

    #[test]
    fn attachment_records_round_trip_and_only_their_owner_retracts_them() {
        let directory = temporary("record");
        let record = AttachmentRecord {
            pid: 42,
            socket: PathBuf::from("/tmp/x.sock"),
            session_id: "session".into(),
        };
        record.publish(&directory).expect("publish");
        assert_eq!(AttachmentRecord::read(&directory).expect("read"), Some(record));
        AttachmentRecord::retract(&directory, 7);
        assert!(AttachmentRecord::read(&directory).expect("read").is_some());
        AttachmentRecord::retract(&directory, 42);
        assert_eq!(AttachmentRecord::read(&directory).expect("read"), None);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_runtime_link_serves_one_terminal_and_checks_the_exact_session() {
        let home = temporary("link");
        let socket = new_socket_path(&home).expect("socket path");
        let link = RemoteLink::bind(socket.clone()).expect("bind");
        link.set_session(Some("session-a".into()));

        let mut first = connect(&socket, Some("session-a"), (100, 30)).expect("attach");
        let deadline = Instant::now() + Duration::from_secs(5);
        while link.is_detached() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(link.size(), (100, 30));
        let generation = link.generation();

        // One terminal at a time; a different session is refused outright.
        let competing = connect(&socket, Some("session-a"), (80, 24));
        assert!(competing.is_err_and(|message| message.contains("another terminal")));
        write_frame(&mut first, INPUT, b"hi").expect("input");
        assert_eq!(link.wait_input(Duration::from_secs(5), (generation, (100, 30))), b"hi");
        link.write_output(b"frame");
        assert_eq!(read_frame(&mut first).expect("output"), Some((OUTPUT, b"frame".to_vec())));

        // Loss detaches without closing the runtime.
        drop(first);
        while !link.is_detached() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let stale = connect(&socket, Some("session-b"), (80, 24));
        assert!(stale.is_err_and(|message| message.contains("not session session-b")));
        let mut second = connect(&socket, None, (80, 24)).expect("reattach");
        while link.generation() == generation {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        link.close(0, None);
        assert_eq!(
            read_frame(&mut second).expect("close").map(|(kind, _)| kind),
            Some(CLOSE)
        );
        assert!(!socket.exists());
        let _ = fs::remove_dir_all(home);
    }
}
