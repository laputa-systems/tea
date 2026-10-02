//! Headful process split: start or reattach the session runtime and relay
//! the terminal to it. See [`crate::detach`] for the boundary itself.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::detach::{
    self, AttachmentRecord, RelayEnd, RemoteLink, IN_PROCESS_ENV, RUNTIME_SOCKET_ENV,
};

use super::CliOptions;

/// Whether a headful invocation should split into relay and runtime.
pub(super) fn isolation_enabled() -> bool {
    if std::env::var_os(IN_PROCESS_ENV).is_some_and(|value| value == "1") {
        return false;
    }
    rustix::termios::isatty(std::io::stdin()) && rustix::termios::isatty(std::io::stdout())
}

/// The runtime half, started by a relay with [`RUNTIME_SOCKET_ENV`].
pub(super) fn run_runtime(options: CliOptions, socket: OsString) -> ExitCode {
    // Tools and child processes must never see the internal handoff.
    std::env::remove_var(RUNTIME_SOCKET_ENV);
    let link = match RemoteLink::bind(PathBuf::from(socket)) {
        Ok(link) => link,
        Err(error) => {
            eprintln!("tea: session runtime could not bind its terminal socket: {error}");
            return ExitCode::from(2);
        }
    };
    let mut app = App::new(options);
    let result = app.run_runtime(std::sync::Arc::clone(&link));
    // Close only after the application, including any owned root work, has
    // settled; the relay then exits with the same status, or starts the
    // runtime for the session the user switched to.
    let (code, message) = match &result {
        Ok(()) => (0, None),
        Err(error) => (2, Some(format!("tea: {error}"))),
    };
    if let Some(message) = &message {
        eprintln!("{message}");
    }
    let (handoff, unconsumed) = match app.take_handoff() {
        Some((arguments, unconsumed)) => (
            arguments
                .into_iter()
                .map(|argument| argument.into_string().ok())
                .collect::<Option<Vec<_>>>(),
            unconsumed,
        ),
        None => (None, 0),
    };
    link.close_with_handoff(code, message.as_deref(), handoff.as_deref(), unconsumed);
    drop(app);
    ExitCode::from(code)
}

/// The relay half: what the user's `tea` process becomes.
pub(super) fn run_relay(options: &CliOptions, args: Vec<OsString>) -> ExitCode {
    let tea_home = match crate::app::tea_home_for(options) {
        Ok(home) => home,
        Err(error) => {
            eprintln!("tea: {error}");
            return ExitCode::from(2);
        }
    };
    // Raw mode spans every runtime of this relay session, so input typed
    // during a session handoff reaches the next runtime unchanged.
    let raw = match detach::RawTerminal::enter() {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!("tea: cannot enter raw terminal mode: {error}");
            return ExitCode::from(2);
        }
    };
    let (code, reset) = relay_session(options, &tea_home, args);
    if reset {
        raw.reset_presentation();
    }
    drop(raw);
    for message in std::mem::take(&mut *PENDING_MESSAGES.lock().expect("messages")) {
        eprintln!("{message}");
    }
    code
}

/// Diagnostics printed after the terminal is restored.
static PENDING_MESSAGES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn report(message: String) {
    PENDING_MESSAGES.lock().expect("messages").push(message);
}

/// Returns the exit code and whether the terminal needs a presentation reset.
fn relay_session(options: &CliOptions, tea_home: &Path, args: Vec<OsString>) -> (ExitCode, bool) {
    let tea_home = tea_home.to_path_buf();
    let mut args = args;
    if let Some(session) = options.attach_session() {
        let Some(directory) = find_session_directory(&tea_home, session) else {
            report(format!("tea: no saved session {session} under {}", tea_home.display()));
            return (ExitCode::from(2), false);
        };
        match AttachmentRecord::read(&directory) {
            Ok(Some(record)) if record.session_id == session => {
                match detach::connect(&record.socket, Some(session), detach::current_size()) {
                    Ok(stream) => match finish(detach::relay(stream, &[]), None, None) {
                        Finished::Exit(code, reset) => return (code, reset),
                        Finished::Handoff(next, replay) => {
                            return relay_runtimes(&tea_home, strings(next), replay);
                        }
                    },
                    Err(message) if message.contains("another terminal") => {
                        report(format!(
                            "tea: session {session} is already attached to a terminal: {message}"
                        ));
                        return (ExitCode::from(2), false);
                    }
                    Err(message) => {
                        // The record outlived its runtime (or the runtime
                        // ended for another reason): it is stale.
                        if !process_alive(record.pid) {
                            AttachmentRecord::retract(&directory, record.pid);
                        }
                        report(format!(
                            "tea: no live runtime serves session {session} ({message}); reopened it"
                        ));
                    }
                }
            }
            Ok(_) => {
                report(format!("tea: no live runtime serves session {session}; reopened it"));
            }
            Err(error) => {
                report(format!(
                    "tea: cannot read the runtime record of session {session}: {error}"
                ));
                return (ExitCode::from(2), false);
            }
        }
        args = attach_to_resume(args);
    }
    relay_runtimes(&tea_home, args, Vec::new())
}

fn strings(arguments: Vec<String>) -> Vec<OsString> {
    arguments.into_iter().map(OsString::from).collect()
}

/// Start a runtime and relay to it. When the user switches sessions, the
/// runtime ends and hands off: the relay starts one fresh runtime for the
/// next session. A runtime is never reused for another session.
fn relay_runtimes(tea_home: &Path, args: Vec<OsString>, replay: Vec<u8>) -> (ExitCode, bool) {
    let mut args = args;
    let mut replay = replay;
    loop {
        let (stream, child, log) = match spawn_runtime(tea_home, &args) {
            Ok(started) => started,
            Err(code) => return (code, false),
        };
        match finish(detach::relay(stream, &replay), Some(child), Some(log)) {
            Finished::Exit(code, reset) => return (code, reset),
            Finished::Handoff(next, unconsumed) => {
                args = strings(next);
                replay = unconsumed;
            }
        }
    }
}

fn spawn_runtime(
    tea_home: &Path,
    args: &[OsString],
) -> Result<(std::os::unix::net::UnixStream, Child, PathBuf), ExitCode> {
    let size = detach::current_size();
    let socket = detach::new_socket_path(tea_home).map_err(|error| {
        report(format!("tea: cannot prepare the session runtime socket: {error}"));
        ExitCode::from(2)
    })?;
    let log = socket.with_extension("log");
    let stderr = std::fs::File::create(&log).map_err(|error| {
        report(format!("tea: cannot create the session runtime log: {error}"));
        ExitCode::from(2)
    })?;
    let executable = std::env::current_exe().map_err(|error| {
        report(format!("tea: cannot locate the tea executable: {error}"));
        ExitCode::from(2)
    })?;
    // The runtime is not in the terminal's process group and holds no
    // terminal descriptor, so terminal hangup and job-control signals reach
    // only this relay.
    let mut child = {
        use std::os::unix::process::CommandExt;
        Command::new(executable)
            .args(args)
            .env(RUNTIME_SOCKET_ENV, &socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0)
            .spawn()
    }
    .map_err(|error| {
        report(format!("tea: cannot start the session runtime: {error}"));
        ExitCode::from(2)
    })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            report_log(&log);
            remove_log(&log);
            return Err(ExitCode::from(2));
        }
        if socket.exists() {
            match detach::connect(&socket, None, size) {
                Ok(stream) => return Ok((stream, child, log)),
                Err(message) if Instant::now() >= deadline => {
                    report(format!("tea: cannot attach to the session runtime: {message}"));
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ExitCode::from(2));
                }
                Err(_) => {}
            }
        } else if Instant::now() >= deadline {
            report(format!("tea: the session runtime did not start"));
            let _ = child.kill();
            let _ = child.wait();
            report_log(&log);
            return Err(ExitCode::from(2));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

enum Finished {
    /// Exit with this code; `true` when the terminal needs a reset.
    Exit(ExitCode, bool),
    /// Start a runtime with these arguments and replay this input to it.
    Handoff(Vec<String>, Vec<u8>),
}

fn remove_log(log: &Path) {
    let _ = std::fs::remove_file(log);
    if let Some(directory) = log.parent() {
        let _ = std::fs::remove_dir(directory);
    }
}

fn finish(end: std::io::Result<RelayEnd>, child: Option<Child>, log: Option<PathBuf>) -> Finished {
    let code = match end {
        Ok(RelayEnd::Closed {
            code,
            message,
            handoff,
            replay,
        }) => {
            if let Some(mut child) = child {
                // The runtime closes only after settling; reap it.
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if !matches!(child.try_wait(), Ok(None)) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            if let Some(message) = message {
                report(message);
            }
            if let Some(log) = &log {
                remove_log(log);
            }
            if let Some(next) = handoff {
                return Finished::Handoff(next, replay);
            }
            (code, false)
        }
        Ok(RelayEnd::Refused(message)) => {
            report(format!("tea: {message}"));
            (2, false)
        }
        Ok(RelayEnd::Lost) | Err(_) => {
            report(
                "tea: the session runtime exited unexpectedly; reopen the session with `tea --resume SESSION_ID` (durable recovery applies)".into(),
            );
            if let Some(log) = &log {
                report_log(log);
            }
            (2, true)
        }
        // The terminal is gone; the runtime keeps any admitted work.
        Ok(RelayEnd::TerminalGone) => (1, false),
    };
    Finished::Exit(ExitCode::from(code.0), code.1)
}

fn report_log(log: &Path) {
    if let Ok(text) = std::fs::read_to_string(log) {
        let text = text.trim();
        if !text.is_empty() {
            report(tea_core::tool::truncate_middle(text, 4_096));
        }
    }
}

fn attach_to_resume(args: Vec<OsString>) -> Vec<OsString> {
    let mut rewritten = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--attach" {
            rewritten.push(OsString::from("--resume"));
            if let Some(value) = iter.next() {
                rewritten.push(value);
            }
        } else if let Some(value) = arg
            .to_str()
            .and_then(|arg| arg.strip_prefix("--attach="))
        {
            rewritten.push(OsString::from(format!("--resume={value}")));
        } else {
            rewritten.push(arg);
        }
    }
    rewritten
}

/// Locate a session directory by identity. Session directories are inert
/// files; reading them neither opens nor advances a session.
fn find_session_directory(tea_home: &Path, session: &str) -> Option<PathBuf> {
    let name = format!("{session}.tea");
    std::fs::read_dir(tea_home.join("sessions"))
        .ok()?
        .filter_map(Result::ok)
        .map(|workspace| workspace.path().join(&name))
        .find(|candidate| candidate.is_dir())
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_arguments_become_resume_arguments_for_a_fresh_runtime() {
        let args = ["--provider", "mock", "--attach", "abc", "--attach=def"]
            .map(OsString::from)
            .to_vec();
        assert_eq!(
            attach_to_resume(args),
            ["--provider", "mock", "--resume", "abc", "--resume=def"].map(OsString::from)
        );
    }
}
