//! Rustix-backed terminal ownership and input decoding.
//!
//! Portable presentation is delegated to [`tea_tui::InlineTerminal`]; this
//! module remains the sole owner of raw mode, resize/input polling, and
//! bracketed-paste lifecycle.

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::{retry_on_intr, Errno};
use rustix::termios::{tcgetattr, tcgetwinsize, tcsetattr, OptionalActions, Termios};
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, stdin, stdout, Read, Stdin, Stdout, Write};
use std::time::{Duration, Instant};
use tea_tui::InlineTerminal;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// A terminal key code understood by the application input surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyCode {
    Backspace,
    Delete,
    Down,
    End,
    Enter,
    Esc,
    Home,
    Left,
    PageDown,
    PageUp,
    Right,
    Tab,
    Up,
    Char(char),
}

/// Modifier flags attached to a terminal key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const SHIFT: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Key delivery state. Input is decoded from a raw terminal stream as presses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

/// A decoded terminal key event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
}

/// Events delivered by the terminal host to the application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    FocusGained,
    FocusLost,
    Mouse,
}

/// Errors from terminal setup, input, output, or restoration.
#[derive(Debug)]
pub enum TerminalError {
    /// The underlying terminal operation failed.
    Io(io::Error),
    /// A suspended guard was asked to resume after it was already active.
    AlreadyActive,
    /// A suspended guard was asked to resume after it had been permanently restored.
    Inactive,
}

impl fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "terminal I/O failed: {error}"),
            Self::AlreadyActive => formatter.write_str("terminal is already active"),
            Self::Inactive => formatter.write_str("terminal guard is inactive"),
        }
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::AlreadyActive | Self::Inactive => None,
        }
    }
}

impl From<io::Error> for TerminalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<Errno> for TerminalError {
    fn from(error: Errno) -> Self {
        Self::Io(error.into())
    }
}

/// RAII owner of raw mode, terminal input, and bracketed paste.
///
/// Normal conversation stays on the main screen. The contained portable
/// renderer enters the alternate screen only for explicit temporary surfaces.
pub struct TerminalGuard {
    input: Stdin,
    renderer: InlineTerminal<Stdout>,
    original_termios: Option<Termios>,
    active: bool,
    last_size: Option<(u16, u16)>,
    decoder: InputDecoder,
}

impl fmt::Debug for TerminalGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalGuard")
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl TerminalGuard {
    /// Enter the terminal modes owned by the application.
    pub fn enter() -> Result<Self, TerminalError> {
        let mut guard = Self {
            input: stdin(),
            renderer: InlineTerminal::new(stdout()),
            original_termios: None,
            active: false,
            last_size: None,
            decoder: InputDecoder::default(),
        };
        guard.activate()?;
        Ok(guard)
    }

    fn activate(&mut self) -> Result<(), TerminalError> {
        let original = tcgetattr(&self.input)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(&self.input, OptionalActions::Now, &raw)?;

        if let Err(error) = self
            .write_mode_sequences(true)
            .and_then(|()| self.flush_io())
        {
            let _ = tcsetattr(&self.input, OptionalActions::Now, &original);
            return Err(TerminalError::Io(error));
        }

        let size = match self.size() {
            Ok(size) => size,
            Err(error) => {
                let _ = self
                    .write_mode_sequences(false)
                    .and_then(|()| self.flush_io());
                let _ = tcsetattr(&self.input, OptionalActions::Now, &original);
                return Err(error);
            }
        };
        self.original_termios = Some(original);
        self.active = true;
        self.last_size = Some(size);
        self.decoder = InputDecoder::default();
        Ok(())
    }

    /// Restore all owned terminal modes. Dropping the guard performs the same best-effort action.
    pub fn restore(&mut self) -> Result<(), TerminalError> {
        if !self.active {
            return Ok(());
        }
        let presentation_result = self.renderer.finish();
        let command_result = self
            .write_mode_sequences(false)
            .and_then(|()| self.flush_io());
        let raw_result = self
            .original_termios
            .take()
            .map(|termios| tcsetattr(&self.input, OptionalActions::Now, &termios))
            .unwrap_or(Ok(()));
        self.active = false;
        self.last_size = None;
        presentation_result.map_err(TerminalError::Io)?;
        command_result.map_err(TerminalError::Io)?;
        raw_result.map_err(TerminalError::from)
    }

    /// Temporarily restore the user's normal terminal for an external program.
    pub fn suspend(&mut self) -> Result<(), TerminalError> {
        self.restore()
    }

    /// Re-enter the terminal modes suspended by [`Self::suspend`].
    pub fn resume(&mut self) -> Result<(), TerminalError> {
        if self.active {
            return Err(TerminalError::AlreadyActive);
        }
        self.activate()
    }

    /// Whether this guard currently owns terminal modes.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Poll for a synchronous terminal input event.
    pub fn poll_event(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<TerminalEvent>, TerminalError> {
        if !self.active {
            return Err(TerminalError::Inactive);
        }

        if let Some(event) = self.decoder.next_event(Instant::now()) {
            return Ok(Some(event));
        }

        let now = Instant::now();
        let wait = self
            .decoder
            .timeout_until_escape(now)
            .map_or(timeout, |remaining| remaining.min(timeout));
        let timespec = duration_to_timespec(wait);
        // Child-workspace cleanup reaps short-lived `git` processes.  On
        // platforms which surface that SIGCHLD to the foreground thread, an
        // otherwise idle terminal poll can be interrupted.  A signal does not
        // change terminal ownership or input state, so retry the syscall
        // rather than tear down an interactive session mid-cancellation.
        let ready = retry_on_intr(|| {
            let mut fds = [PollFd::new(&self.input, PollFlags::IN)];
            poll(&mut fds, Some(&timespec))
        })?;
        if ready != 0 {
            let mut bytes = [0_u8; 4096];
            let count = loop {
                match self.input.read(&mut bytes) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => break result?,
                }
            };
            self.decoder.push(&bytes[..count]);
        }

        if let Some(event) = self.decoder.next_event(Instant::now()) {
            return Ok(Some(event));
        }
        let size = self.size()?;
        if self.last_size != Some(size) {
            self.last_size = Some(size);
            return Ok(Some(TerminalEvent::Resize(size.0, size.1)));
        }
        Ok(None)
    }

    /// Return the currently available terminal dimensions.
    pub fn size(&self) -> Result<(u16, u16), TerminalError> {
        let size = retry_on_intr(|| tcgetwinsize(&self.input))?;
        Ok((size.ws_col, size.ws_row))
    }

    /// Borrow the portable renderer that owns mutable-tail bookkeeping.
    pub fn renderer_mut(&mut self) -> Result<&mut InlineTerminal<Stdout>, TerminalError> {
        if !self.active {
            return Err(TerminalError::Inactive);
        }
        Ok(&mut self.renderer)
    }

    fn flush_io(&mut self) -> Result<(), io::Error> {
        self.renderer.writer_mut().flush()
    }

    fn write_mode_sequences(&mut self, active: bool) -> Result<(), io::Error> {
        if active {
            write!(self.renderer.writer_mut(), "\x1b[?25l\x1b[?2004h")
        } else {
            write!(self.renderer.writer_mut(), "\x1b[?2004l\x1b[?25h")
        }
    }
}

fn duration_to_timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: duration.subsec_nanos().into(),
    }
}

#[derive(Default)]
struct InputDecoder {
    bytes: VecDeque<u8>,
    paste: bool,
    escape_deadline: Option<Instant>,
}

impl InputDecoder {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes.iter().copied());
    }

    fn timeout_until_escape(&self, now: Instant) -> Option<Duration> {
        self.escape_deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn next_event(&mut self, now: Instant) -> Option<TerminalEvent> {
        if self.paste {
            if let Some(end) = find_subslice(&self.bytes, PASTE_END) {
                let text = self.take(end);
                self.take(PASTE_END.len());
                self.paste = false;
                return Some(TerminalEvent::Paste(
                    String::from_utf8_lossy(&text).into_owned(),
                ));
            }
            return None;
        }

        let first = *self.bytes.front()?;
        if first == 0x1b {
            if self.bytes.len() == 1 {
                let deadline = self
                    .escape_deadline
                    .get_or_insert_with(|| now + Duration::from_millis(30));
                if now < *deadline {
                    return None;
                }
                self.bytes.pop_front();
                self.escape_deadline = None;
                return Some(key(KeyCode::Esc));
            }
            if let Some(event) = self.escape_sequence() {
                self.escape_deadline = None;
                return Some(event);
            }
            if self.escape_sequence_prefix() {
                let deadline = self
                    .escape_deadline
                    .get_or_insert_with(|| now + Duration::from_millis(30));
                if now < *deadline {
                    return None;
                }
            }
            if self
                .bytes
                .get(1)
                .is_some_and(|byte| *byte >= 0x20 && *byte != b'[' && *byte != b'O')
            {
                self.bytes.pop_front();
                let byte = self.bytes.pop_front()?;
                return Some(TerminalEvent::Key(KeyEvent {
                    code: KeyCode::Char(byte as char),
                    modifiers: KeyModifiers::ALT,
                    kind: KeyEventKind::Press,
                }));
            }
            self.bytes.pop_front();
            self.escape_deadline = None;
            return Some(key(KeyCode::Esc));
        }

        self.escape_deadline = None;
        match first {
            b'\r' | b'\n' => {
                self.bytes.pop_front();
                Some(key(KeyCode::Enter))
            }
            b'\t' => {
                self.bytes.pop_front();
                Some(key(KeyCode::Tab))
            }
            0x7f | 0x08 => {
                self.bytes.pop_front();
                Some(key(KeyCode::Backspace))
            }
            0x00..=0x1a => {
                self.bytes.pop_front();
                Some(TerminalEvent::Key(KeyEvent {
                    code: KeyCode::Char(if first == 0 {
                        '@'
                    } else {
                        (first + b'a' - 1) as char
                    }),
                    modifiers: KeyModifiers::CONTROL,
                    kind: KeyEventKind::Press,
                }))
            }
            byte if byte.is_ascii() => {
                self.bytes.pop_front();
                Some(key(KeyCode::Char(byte as char)))
            }
            _ => self.next_utf8(),
        }
    }

    fn escape_sequence(&mut self) -> Option<TerminalEvent> {
        let sequences: &[(&[u8], KeyCode, KeyModifiers)] = &[
            (b"\x1b[A", KeyCode::Up, KeyModifiers::default()),
            (b"\x1b[B", KeyCode::Down, KeyModifiers::default()),
            (b"\x1b[C", KeyCode::Right, KeyModifiers::default()),
            (b"\x1b[D", KeyCode::Left, KeyModifiers::default()),
            (b"\x1b[H", KeyCode::Home, KeyModifiers::default()),
            (b"\x1b[F", KeyCode::End, KeyModifiers::default()),
            (b"\x1b[1~", KeyCode::Home, KeyModifiers::default()),
            (b"\x1b[4~", KeyCode::End, KeyModifiers::default()),
            (b"\x1b[3~", KeyCode::Delete, KeyModifiers::default()),
            (b"\x1b[5~", KeyCode::PageUp, KeyModifiers::default()),
            (b"\x1b[6~", KeyCode::PageDown, KeyModifiers::default()),
            (b"\x1b[Z", KeyCode::Tab, KeyModifiers::SHIFT),
            (b"\x1bOA", KeyCode::Up, KeyModifiers::default()),
            (b"\x1bOB", KeyCode::Down, KeyModifiers::default()),
            (b"\x1bOC", KeyCode::Right, KeyModifiers::default()),
            (b"\x1bOD", KeyCode::Left, KeyModifiers::default()),
        ];
        if self.starts_with(PASTE_START) {
            self.take(PASTE_START.len());
            self.paste = true;
            return self.next_event(Instant::now());
        }
        for (sequence, code, modifiers) in sequences {
            if self.starts_with(sequence) {
                self.take(sequence.len());
                return Some(TerminalEvent::Key(KeyEvent {
                    code: *code,
                    modifiers: *modifiers,
                    kind: KeyEventKind::Press,
                }));
            }
        }
        None
    }

    fn escape_sequence_prefix(&self) -> bool {
        let candidates: &[&[u8]] = &[
            PASTE_START,
            b"\x1b[A",
            b"\x1b[B",
            b"\x1b[C",
            b"\x1b[D",
            b"\x1b[H",
            b"\x1b[F",
            b"\x1b[1~",
            b"\x1b[4~",
            b"\x1b[3~",
            b"\x1b[5~",
            b"\x1b[6~",
            b"\x1b[Z",
            b"\x1bOA",
            b"\x1bOB",
            b"\x1bOC",
            b"\x1bOD",
        ];
        candidates.iter().any(|candidate| {
            self.bytes.len() < candidate.len()
                && self
                    .bytes
                    .iter()
                    .copied()
                    .eq(candidate.iter().take(self.bytes.len()).copied())
        })
    }

    fn next_utf8(&mut self) -> Option<TerminalEvent> {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        match std::str::from_utf8(&bytes) {
            Ok(text) => {
                let character = text.chars().next()?;
                self.take(character.len_utf8());
                Some(key(KeyCode::Char(character)))
            }
            Err(error) if error.valid_up_to() != 0 => {
                let text = std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?;
                let character = text.chars().next()?;
                self.take(character.len_utf8());
                Some(key(KeyCode::Char(character)))
            }
            Err(error) if error.error_len().is_none() => None,
            Err(_) => {
                self.bytes.pop_front();
                Some(key(KeyCode::Char('\u{fffd}')))
            }
        }
    }

    fn starts_with(&self, expected: &[u8]) -> bool {
        self.bytes.len() >= expected.len()
            && self
                .bytes
                .iter()
                .take(expected.len())
                .copied()
                .eq(expected.iter().copied())
    }

    fn take(&mut self, count: usize) -> Vec<u8> {
        self.bytes.drain(..count).collect()
    }
}

fn find_subslice(bytes: &VecDeque<u8>, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || bytes.len() < needle.len() {
        return None;
    }
    let values: Vec<u8> = bytes.iter().copied().collect();
    values
        .windows(needle.len())
        .position(|window| window == needle)
}

const fn key(code: KeyCode) -> TerminalEvent {
    TerminalEvent::Key(KeyEvent {
        code,
        modifiers: KeyModifiers(0),
        kind: KeyEventKind::Press,
    })
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::{InputDecoder, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, TerminalEvent};
    use std::time::{Duration, Instant};

    #[test]
    fn decodes_keys_and_utf8() {
        let mut decoder = InputDecoder::default();
        decoder.push(b"a\x1b[A\x7f\r\x03\x12");
        decoder.push("é".as_bytes());
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(key(KeyCode::Char('a')))
        );
        assert_eq!(decoder.next_event(Instant::now()), Some(key(KeyCode::Up)));
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(key(KeyCode::Backspace))
        );
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(key(KeyCode::Enter))
        );
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
            }))
        );
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(TerminalEvent::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
            }))
        );
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(key(KeyCode::Char('é')))
        );
    }

    #[test]
    fn decodes_bracketed_paste() {
        let mut decoder = InputDecoder::default();
        decoder.push(b"\x1b[200~line one\nline two\x1b[201~");
        assert_eq!(
            decoder.next_event(Instant::now()),
            Some(TerminalEvent::Paste("line one\nline two".into()))
        );
    }

    #[test]
    fn delays_a_bare_escape() {
        let mut decoder = InputDecoder::default();
        decoder.push(b"\x1b");
        let now = Instant::now();
        assert_eq!(decoder.next_event(now), None);
        assert_eq!(
            decoder.next_event(now + Duration::from_millis(31)),
            Some(key(KeyCode::Esc))
        );
    }

    const fn key(code: KeyCode) -> TerminalEvent {
        TerminalEvent::Key(KeyEvent {
            code,
            modifiers: KeyModifiers(0),
            kind: KeyEventKind::Press,
        })
    }
}
