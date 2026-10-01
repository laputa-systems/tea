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
    // settled; the relay then exits with the same status.
    let (code, message) = match &result {
        Ok(()) => (0, None),
        Err(error) => (2, Some(format!("tea: {error}"))),
    };
    if let Some(message) = &message {
        eprintln!("{message}");
    }
    link.close(code, message.as_deref());
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
    let size = detach::current_size();
    let mut args = args;
    if let Some(session) = options.attach_session() {
        let Some(directory) = find_session_directory(&tea_home, session) else {
            eprintln!("tea: no saved session {session} under {}", tea_home.display());
            return ExitCode::from(2);
        };
        match AttachmentRecord::read(&directory) {
            Ok(Some(record)) if record.session_id == session => {
                match detach::connect(&record.socket, Some(session), size) {
                    Ok(stream) => return finish(detach::relay(stream), None, None),
                    Err(message) if message.contains("another terminal") => {
                        eprintln!("tea: session {session} is already attached to a terminal: {message}");
                        return ExitCode::from(2);
                    }
                    Err(message) => {
                        // The record outlived its runtime (or the runtime moved
                        // on to another session): it is stale.
                        if !process_alive(record.pid) {
                            AttachmentRecord::retract(&directory, record.pid);
                        }
                        eprintln!(
                            "tea: no live runtime serves session {session} ({message}); reopening it"
                        );
                    }
                }
            }
            Ok(_) => {
                eprintln!("tea: no live runtime serves session {session}; reopening it");
            }
            Err(error) => {
                eprintln!("tea: cannot read the runtime record of session {session}: {error}");
                return ExitCode::from(2);
            }
        }
        args = attach_to_resume(args);
    }
    spawn_and_relay(&tea_home, args, size)
}

fn spawn_and_relay(tea_home: &Path, args: Vec<OsString>, size: (u16, u16)) -> ExitCode {
    let socket = match detach::new_socket_path(tea_home) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("tea: cannot prepare the session runtime socket: {error}");
            return ExitCode::from(2);
        }
    };
    let log = socket.with_extension("log");
    let stderr = match std::fs::File::create(&log) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("tea: cannot create the session runtime log: {error}");
            return ExitCode::from(2);
        }
    };
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("tea: cannot locate the tea executable: {error}");
            return ExitCode::from(2);
        }
    };
    // The runtime is not in the terminal's process group and holds no
    // terminal descriptor, so terminal hangup and job-control signals reach
    // only this relay.
    let mut child = match {
        use std::os::unix::process::CommandExt;
        Command::new(executable)
            .args(&args)
            .env(RUNTIME_SOCKET_ENV, &socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0)
            .spawn()
    } {
        Ok(child) => child,
        Err(error) => {
            eprintln!("tea: cannot start the session runtime: {error}");
            return ExitCode::from(2);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let stream = loop {
        if let Ok(Some(_)) = child.try_wait() {
            report_log(&log);
            let _ = std::fs::remove_file(&log);
            return ExitCode::from(2);
        }
        if socket.exists() {
            match detach::connect(&socket, None, size) {
                Ok(stream) => break stream,
                Err(message) if Instant::now() >= deadline => {
                    eprintln!("tea: cannot attach to the session runtime: {message}");
                    let _ = child.kill();
                    let _ = child.wait();
                    return ExitCode::from(2);
                }
                Err(_) => {}
            }
        } else if Instant::now() >= deadline {
            eprintln!("tea: the session runtime did not start");
            let _ = child.kill();
            let _ = child.wait();
            report_log(&log);
            return ExitCode::from(2);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    finish(detach::relay(stream), Some(child), Some(log))
}

fn finish(
    end: std::io::Result<RelayEnd>,
    child: Option<Child>,
    log: Option<PathBuf>,
) -> ExitCode {
    let code = match end {
        Ok(RelayEnd::Closed { code, message }) => {
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
                eprintln!("{message}");
            }
            if let Some(log) = &log {
                let _ = std::fs::remove_file(log);
                if let Some(directory) = log.parent() {
                    let _ = std::fs::remove_dir(directory);
                }
            }
            code
        }
        Ok(RelayEnd::Refused(message)) => {
            eprintln!("tea: {message}");
            2
        }
        Ok(RelayEnd::Lost) | Err(_) => {
            eprintln!(
                "tea: the session runtime exited unexpectedly; reopen the session with `tea --resume SESSION_ID` (durable recovery applies)"
            );
            if let Some(log) = &log {
                report_log(log);
            }
            2
        }
        // The terminal is gone; the runtime keeps any admitted work.
        Ok(RelayEnd::TerminalGone) => 1,
    };
    ExitCode::from(code)
}

fn report_log(log: &Path) {
    if let Ok(text) = std::fs::read_to_string(log) {
        let text = text.trim();
        if !text.is_empty() {
            eprintln!("{}", tea_core::tool::truncate_middle(text, 4_096));
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
