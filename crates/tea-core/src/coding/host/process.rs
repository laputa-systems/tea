//! Canonical local execution and settlement for the trusted process capability.
//!
//! Each invocation owns only the process scope created by that invocation. On
//! Unix that scope is a fresh process group whose ID is retained on this stack
//! frame only. The runner never consults process names, workspaces, agents, or
//! a global registry when it needs to stop a command.

use super::contract::{CommandEnvironment, CommandOutput, CommandTermination, OperationError};
use crate::scheduler::CancellationToken;
use crate::tool::{ToolUpdate, ToolUpdateSink};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static COMMAND_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

const STREAM_CHUNK_BYTES: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERMINATION_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);

/// Run one local `bash` invocation through the canonical bounded process
/// lifecycle.
///
/// This function is synchronous so embeddings can choose their own blocking
/// scheduler. It owns no shared lifecycle state: every call creates an
/// independent capture pair and, on Unix, an independent process group.
pub fn run_local_command(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    environment: &CommandEnvironment,
    cancellation: &CancellationToken,
    updates: ToolUpdateSink,
) -> Result<CommandOutput, OperationError> {
    run_local_command_with_observer(
        command,
        cwd,
        timeout,
        environment,
        cancellation,
        updates,
        &SystemChildObserver,
    )
}

/// A deliberately narrow, invocation-local observation seam.
///
/// Production uses `SystemChildObserver`; tests can inject a single runner's
/// post-spawn observation failure without mutable global fault state.
trait ChildObserver {
    fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>>;
}

struct SystemChildObserver;

impl ChildObserver for SystemChildObserver {
    fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
        child.try_wait()
    }
}

fn run_local_command_with_observer(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    environment: &CommandEnvironment,
    cancellation: &CancellationToken,
    updates: ToolUpdateSink,
    observer: &dyn ChildObserver,
) -> Result<CommandOutput, OperationError> {
    // No side effect has crossed the process boundary yet.
    if cancellation.is_cancelled() {
        return Err(OperationError::new("cancelled"));
    }

    let (captures, stdout, stderr) = CommandCaptures::create()?;
    let mut process = Command::new("bash");
    process.arg("-c").arg(command).current_dir(cwd);
    environment.apply(&mut process);
    configure_owned_process_scope(&mut process);
    // Capture files prevent a descendant from holding an executor pipe open.
    // They are still only invocation-local, and are tailed for live updates.
    process
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return Err(OperationError::new(error.to_string())),
    };
    let scope = OwnedProcessScope::for_child(&child);
    let started = Instant::now();
    let mut stdout_offset = 0;
    let mut stderr_offset = 0;

    loop {
        // Completion observation wins over a later cancellation. Once a
        // status is known, the only remaining work is foreground-scope cleanup.
        match observer.try_wait(&mut child) {
            Ok(Some(status)) => {
                let termination = match settle_remaining_scope(&mut child, &scope) {
                    Ok(()) => classify_exit_status(status),
                    Err(reason) => indeterminate(reason),
                };
                return Ok(captures.into_output(termination));
            }
            Ok(None) => {}
            Err(_) => {
                let termination = match terminate_owned_scope(&mut child, &scope) {
                    Ok(()) => indeterminate(
                        "could not observe the started command's final status",
                    ),
                    Err(reason) => indeterminate(reason),
                };
                return Ok(captures.into_output(termination));
            }
        }

        if cancellation.is_cancelled() {
            let termination = match terminate_owned_scope(&mut child, &scope) {
                Ok(()) => CommandTermination::Cancelled,
                Err(reason) => indeterminate(reason),
            };
            return Ok(captures.into_output(termination));
        }

        if started.elapsed() >= timeout {
            let termination = match terminate_owned_scope(&mut child, &scope) {
                Ok(()) => CommandTermination::TimedOut,
                Err(reason) => indeterminate(reason),
            };
            return Ok(captures.into_output(termination));
        }

        if emit_capture_updates(&captures.stdout_path, &mut stdout_offset, &updates).is_err()
            || emit_capture_updates(&captures.stderr_path, &mut stderr_offset, &updates).is_err()
        {
            let termination = match terminate_owned_scope(&mut child, &scope) {
                Ok(()) => indeterminate("could not stream output from the started command"),
                Err(reason) => indeterminate(reason),
            };
            return Ok(captures.into_output(termination));
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

fn configure_owned_process_scope(process: &mut Command) {
    #[cfg(unix)]
    {
        // `0` asks the standard library to make this child the leader of a
        // new process group. That group is this invocation's only kill target.
        process.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = process;
    }
}

enum OwnedProcessScope {
    #[cfg(unix)]
    ProcessGroup { id: u32 },
    #[cfg(not(unix))]
    DirectChild,
}

impl OwnedProcessScope {
    fn for_child(child: &Child) -> Self {
        #[cfg(unix)]
        {
            Self::ProcessGroup { id: child.id() }
        }
        #[cfg(not(unix))]
        {
            let _ = child;
            Self::DirectChild
        }
    }
}

/// Settle a completed shell's remaining foreground scope.
///
/// A shell can exit successfully while ordinary background children remain in
/// its group. That is not a completed foreground invocation, so the group is
/// terminated before the recorded shell status is returned.
fn settle_remaining_scope(child: &mut Child, scope: &OwnedProcessScope) -> Result<(), &'static str> {
    #[cfg(unix)]
    {
        let OwnedProcessScope::ProcessGroup { id } = scope;
        if process_group_is_gone(*id)? {
            return Ok(());
        }
        signal_process_group(*id)?;
        return wait_for_process_group_settlement(child, *id);
    }
    #[cfg(not(unix))]
    {
        let _ = scope;
        match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err("the shell completed but could not be reaped"),
            Err(_) => Err("could not observe the completed shell"),
        }
    }
}

/// Stop the scope after timeout, cancellation, or post-spawn observation loss.
///
/// Unix uses `SIGKILL` so a shell cannot trap a graceful signal and continue
/// with trailing statements after Tea claims the invocation stopped.
fn terminate_owned_scope(child: &mut Child, scope: &OwnedProcessScope) -> Result<(), &'static str> {
    #[cfg(unix)]
    {
        let OwnedProcessScope::ProcessGroup { id } = scope;
        if !process_group_is_gone(*id)? {
            signal_process_group(*id)?;
        }
        return wait_for_process_group_settlement(child, *id);
    }
    #[cfg(not(unix))]
    {
        let _ = scope;
        match child.kill() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(_) => return Err("could not terminate the started shell"),
        }
        wait_for_direct_child_settlement(child)?;
        // This platform has no invocation-local descendant containment here.
        // Do not claim clean cancellation/timeout when it cannot be proven.
        Err("this platform cannot prove that command descendants have stopped")
    }
}

#[cfg(unix)]
fn wait_for_process_group_settlement(child: &mut Child, process_group: u32) -> Result<(), &'static str> {
    let started = Instant::now();
    let mut child_reaped = false;
    loop {
        if !child_reaped {
            match child.try_wait() {
                Ok(Some(_)) => child_reaped = true,
                Ok(None) => {}
                Err(_) => return Err("could not reap the started shell after process-group cleanup"),
            }
        }
        if child_reaped && process_group_is_gone(process_group)? {
            return Ok(());
        }
        if started.elapsed() >= TERMINATION_SETTLE_TIMEOUT {
            return Err("could not establish that the owned process group is gone");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(unix))]
fn wait_for_direct_child_settlement(child: &mut Child) -> Result<(), &'static str> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if started.elapsed() < TERMINATION_SETTLE_TIMEOUT => {
                std::thread::sleep(POLL_INTERVAL)
            }
            Ok(None) => return Err("could not establish that the started shell is gone"),
            Err(_) => return Err("could not reap the started shell"),
        }
    }
}

#[cfg(unix)]
fn signal_process_group(process_group: u32) -> Result<(), &'static str> {
    let output = Command::new("/bin/kill")
        .env("LC_ALL", "C")
        .arg("-KILL")
        .arg(format!("-{process_group}"))
        .output()
        .map_err(|_| "could not request termination of the owned process group")?;
    if output.status.success() || output_reports_no_such_process(&output) {
        Ok(())
    } else {
        Err("could not request termination of the owned process group")
    }
}

#[cfg(unix)]
fn process_group_is_gone(process_group: u32) -> Result<bool, &'static str> {
    let output = Command::new("/bin/kill")
        .env("LC_ALL", "C")
        .arg("-0")
        .arg(format!("-{process_group}"))
        .output()
        .map_err(|_| "could not verify the owned process group")?;
    if output.status.success() {
        Ok(false)
    } else if output_reports_no_such_process(&output) {
        Ok(true)
    } else {
        // A non-English or otherwise unrecognizable diagnostic is not proof
        // that the group is gone, so settle honestly as indeterminate.
        Err("could not verify that the owned process group is gone")
    }
}

#[cfg(unix)]
fn output_reports_no_such_process(output: &std::process::Output) -> bool {
    String::from_utf8_lossy(&output.stderr)
        .to_ascii_lowercase()
        .contains("no such process")
}

fn classify_exit_status(status: ExitStatus) -> CommandTermination {
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return CommandTermination::Signaled { signal };
    }
    match status.code() {
        Some(code) => CommandTermination::Exited { code },
        None => indeterminate("the operating system did not report a command exit status"),
    }
}

fn indeterminate(detail: &'static str) -> CommandTermination {
    CommandTermination::Indeterminate {
        reason: format!("{detail}; inspect state before retrying"),
    }
}

struct CommandCaptures {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl CommandCaptures {
    fn create() -> Result<(Self, File, File), OperationError> {
        let (stdout_path, stdout) = command_capture_file("stdout")?;
        let (stderr_path, stderr) = match command_capture_file("stderr") {
            Ok(capture) => capture,
            Err(error) => {
                let _ = fs::remove_file(&stdout_path);
                return Err(error);
            }
        };
        Ok((
            Self {
                stdout_path,
                stderr_path,
            },
            stdout,
            stderr,
        ))
    }

    fn into_output(self, termination: CommandTermination) -> CommandOutput {
        let (stdout, stdout_failed) = read_command_capture(&self.stdout_path);
        let (stderr, stderr_failed) = read_command_capture(&self.stderr_path);
        let termination = if stdout_failed || stderr_failed {
            match termination {
                CommandTermination::Indeterminate { .. } => termination,
                _ => indeterminate("could not read captured output after the command started"),
            }
        } else {
            termination
        };
        CommandOutput {
            termination,
            stdout,
            stderr,
        }
    }
}

impl Drop for CommandCaptures {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

fn command_capture_file(stream: &str) -> Result<(PathBuf, File), OperationError> {
    for _ in 0..16 {
        let sequence = COMMAND_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tea-core-command-{}-{}-{stream}",
            std::process::id(),
            sequence,
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(OperationError::new(format!(
                    "cannot create command capture: {error}"
                )));
            }
        }
    }
    Err(OperationError::new(
        "cannot allocate a unique command capture after 16 attempts",
    ))
}

fn read_command_capture(path: &Path) -> (Vec<u8>, bool) {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return (Vec::new(), true),
    };
    let mut bytes = Vec::new();
    match file.read_to_end(&mut bytes) {
        Ok(_) => (bytes, false),
        Err(_) => (bytes, true),
    }
}

fn emit_capture_updates(
    path: &Path,
    offset: &mut u64,
    updates: &ToolUpdateSink,
) -> std::io::Result<()> {
    let length = fs::metadata(path)?.len();
    while *offset < length {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(*offset))?;
        let remaining = length - *offset;
        let mut bytes = vec![0; usize::try_from(remaining.min(STREAM_CHUNK_BYTES as u64)).unwrap_or(0)];
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        *offset += read as u64;
        updates.emit(ToolUpdate {
            content: String::from_utf8_lossy(&bytes[..read]).into_owned(),
            details: None,
            activity: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::contract::CodingOperations;
    use super::super::local_operations::LocalCodingOperations;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);

    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tea-process-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock is after epoch")
                    .as_nanos(),
                NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir_all(&path).expect("process workspace creates");
            Self { path }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    struct ProcessGroupGuard {
        process_group: Mutex<Option<u32>>,
    }

    #[cfg(unix)]
    impl ProcessGroupGuard {
        fn new() -> Self {
            Self {
                process_group: Mutex::new(None),
            }
        }

        fn arm(&self, process_group: u32) {
            *self.process_group.lock().expect("process group guard lock") = Some(process_group);
        }

        fn disarm(&self) {
            *self.process_group.lock().expect("process group guard lock") = None;
        }
    }

    #[cfg(unix)]
    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            let process_group = self
                .process_group
                .lock()
                .expect("process group guard lock")
                .take();
            if let Some(process_group) = process_group {
                let _ = Command::new("/bin/kill")
                    .env("LC_ALL", "C")
                    .arg("-KILL")
                    .arg(format!("-{process_group}"))
                    .output();
            }
        }
    }

    fn run(command: &str, root: &Path, timeout: Duration) -> CommandOutput {
        run_local_command(
            command,
            root,
            timeout,
            &CommandEnvironment::empty(),
            &CancellationToken::new(),
            ToolUpdateSink::disabled(),
        )
        .expect("local command starts and settles")
    }

    fn start(
        command: impl Into<String>,
        root: PathBuf,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> (Receiver<Result<CommandOutput, OperationError>>, JoinHandle<()>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let command = command.into();
        let task = thread::spawn(move || {
            let result = run_local_command(
                &command,
                &root,
                timeout,
                &CommandEnvironment::empty(),
                &cancellation,
                ToolUpdateSink::disabled(),
            );
            let _ = sender.send(result);
        });
        (receiver, task)
    }

    fn start_with_local_operations(
        operations: Arc<LocalCodingOperations>,
        command: impl Into<String>,
        root: PathBuf,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> (Receiver<Result<CommandOutput, OperationError>>, JoinHandle<()>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let command = command.into();
        let task = thread::spawn(move || {
            let environment = CommandEnvironment::empty();
            let result = smol::block_on(operations.execute_command(
                &command,
                &root,
                timeout,
                &environment,
                cancellation,
                ToolUpdateSink::disabled(),
            ));
            let _ = sender.send(result);
        });
        (receiver, task)
    }

    fn receive(
        receiver: Receiver<Result<CommandOutput, OperationError>>,
        task: JoinHandle<()>,
    ) -> CommandOutput {
        let result = match receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => panic!("command did not settle within the test bound"),
            Err(RecvTimeoutError::Disconnected) => panic!("command worker disconnected"),
        };
        task.join().expect("command worker joins");
        result.expect("local command settles without a pre-spawn error")
    }

    fn wait_for_file(path: &Path) -> bool {
        for _ in 0..200 {
            if path.is_file() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn read_pid(path: &Path) -> u32 {
        fs::read_to_string(path)
            .expect("pid fixture writes")
            .trim()
            .parse()
            .expect("pid fixture is numeric")
    }

    #[cfg(unix)]
    fn wait_for_process_to_stop(pid: u32) -> bool {
        for _ in 0..200 {
            if process_is_stopped(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(target_os = "linux")]
    fn process_is_stopped(pid: u32) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
            Ok(stat) => stat
                .rsplit_once(") ")
                .and_then(|(_, fields)| fields.chars().next())
                .is_some_and(|state| state == 'Z'),
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn process_is_stopped(pid: u32) -> bool {
        let kill_probe = match Command::new("/bin/kill")
            .env("LC_ALL", "C")
            .arg("-0")
            .arg(pid.to_string())
            .output()
        {
            Ok(output) => output,
            Err(_) => return false,
        };
        if !kill_probe.status.success() {
            return output_reports_no_such_process(&kill_probe);
        }
        let output = match Command::new("/bin/ps")
            .arg("-o")
            .arg("stat=")
            .arg("-p")
            .arg(pid.to_string())
            .output()
        {
            Ok(output) => output,
            Err(_) => return false,
        };
        if !output.status.success() {
            return false;
        }
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        state.is_empty() || state.starts_with('Z')
    }

    #[test]
    fn successful_and_nonzero_commands_have_normal_exit_settlements() {
        let root = TempWorkspace::new();
        let success = run("printf success", &root.path, Duration::from_secs(1));
        assert_eq!(success.termination, CommandTermination::Exited { code: 0 });
        assert_eq!(success.stdout, b"success");
        assert!(success.stderr.is_empty());

        let failure = run(
            "printf failure >&2; exit 7",
            &root.path,
            Duration::from_secs(1),
        );
        assert_eq!(failure.termination, CommandTermination::Exited { code: 7 });
        assert_eq!(failure.stderr, b"failure");
    }

    #[cfg(unix)]
    #[test]
    fn signal_termination_preserves_the_kernel_signal() {
        let root = TempWorkspace::new();
        let output = run("kill -TERM $$", &root.path, Duration::from_secs(1));
        assert_eq!(
            output.termination,
            CommandTermination::Signaled { signal: 15 }
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_stops_the_owned_group_preserves_output_and_blocks_trailing_work() {
        let root = TempWorkspace::new();
        let guard = ProcessGroupGuard::new();
        let cancellation = CancellationToken::new();
        let (receiver, task) = start(
            r#"echo $$ > scope.pid
sleep 30 & child=$!
echo "$child" > child.pid
printf started
wait "$child"
touch post-timeout-marker"#,
            root.path.clone(),
            Duration::from_millis(100),
            cancellation.clone(),
        );
        assert!(wait_for_file(&root.path.join("scope.pid")));
        guard.arm(read_pid(&root.path.join("scope.pid")));
        assert!(wait_for_file(&root.path.join("child.pid")));
        let child = read_pid(&root.path.join("child.pid"));

        let output = receive(receiver, task);
        assert_eq!(output.termination, CommandTermination::TimedOut);
        assert_eq!(output.stdout, b"started");
        assert!(!root.path.join("post-timeout-marker").exists());
        assert!(
            wait_for_process_to_stop(child),
            "the timed-out command's child must be dead before settlement"
        );
        assert!(
            !cancellation.is_cancelled(),
            "a command timeout must not cancel the caller's broader operation"
        );
        guard.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn timeout_stops_an_owned_grandchild_process() {
        let root = TempWorkspace::new();
        let guard = ProcessGroupGuard::new();
        let (receiver, task) = start(
            r#"echo $$ > scope.pid
bash -c 'sleep 30 & grandchild=$!; echo "$grandchild" > grandchild.pid; wait "$grandchild"' &
child=$!
echo "$child" > child.pid
wait "$child"
touch post-timeout-marker"#,
            root.path.clone(),
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        assert!(wait_for_file(&root.path.join("scope.pid")));
        guard.arm(read_pid(&root.path.join("scope.pid")));
        assert!(wait_for_file(&root.path.join("grandchild.pid")));
        let grandchild = read_pid(&root.path.join("grandchild.pid"));

        let output = receive(receiver, task);
        assert_eq!(output.termination, CommandTermination::TimedOut);
        assert!(!root.path.join("post-timeout-marker").exists());
        assert!(
            wait_for_process_to_stop(grandchild),
            "the timed-out command's grandchild must be dead before settlement"
        );
        guard.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn successful_shell_exit_does_not_leave_a_background_descendant() {
        let root = TempWorkspace::new();
        let guard = ProcessGroupGuard::new();
        let (receiver, task) = start(
            r#"echo $$ > scope.pid
sleep 30 & child=$!
echo "$child" > child.pid
printf done"#,
            root.path.clone(),
            Duration::from_secs(1),
            CancellationToken::new(),
        );
        assert!(wait_for_file(&root.path.join("scope.pid")));
        guard.arm(read_pid(&root.path.join("scope.pid")));
        assert!(wait_for_file(&root.path.join("child.pid")));
        let child = read_pid(&root.path.join("child.pid"));

        let output = receive(receiver, task);
        assert_eq!(output.termination, CommandTermination::Exited { code: 0 });
        assert_eq!(output.stdout, b"done");
        assert!(
            wait_for_process_to_stop(child),
            "a successful foreground shell must not leak its background child"
        );
        guard.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_the_owned_group_and_blocks_trailing_work() {
        let root = TempWorkspace::new();
        let guard = ProcessGroupGuard::new();
        let cancellation = CancellationToken::new();
        let (receiver, task) = start(
            r#"echo $$ > scope.pid
sleep 30 & child=$!
echo "$child" > child.pid
touch started
wait "$child"
touch post-cancellation-marker"#,
            root.path.clone(),
            Duration::from_secs(30),
            cancellation.clone(),
        );
        assert!(wait_for_file(&root.path.join("scope.pid")));
        guard.arm(read_pid(&root.path.join("scope.pid")));
        assert!(wait_for_file(&root.path.join("child.pid")));
        let child = read_pid(&root.path.join("child.pid"));

        cancellation.cancel();
        let output = receive(receiver, task);
        assert_eq!(output.termination, CommandTermination::Cancelled);
        assert!(!root.path.join("post-cancellation-marker").exists());
        assert!(
            wait_for_process_to_stop(child),
            "the cancelled command's child must be dead before settlement"
        );
        guard.disarm();
    }

    struct CancellingObserver {
        cancellation: CancellationToken,
    }

    impl ChildObserver for CancellingObserver {
        fn try_wait(&self, child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
            let status = child.try_wait()?;
            if status.is_some() {
                self.cancellation.cancel();
            }
            Ok(status)
        }
    }

    #[test]
    fn late_cancellation_cannot_erase_an_observed_completion() {
        let root = TempWorkspace::new();
        let cancellation = CancellationToken::new();
        let observer = CancellingObserver {
            cancellation: cancellation.clone(),
        };
        let output = run_local_command_with_observer(
            "printf complete",
            &root.path,
            Duration::from_secs(1),
            &CommandEnvironment::empty(),
            &cancellation,
            ToolUpdateSink::disabled(),
            &observer,
        )
        .expect("completion settles");
        assert_eq!(output.termination, CommandTermination::Exited { code: 0 });
        assert!(cancellation.is_cancelled());
    }

    struct FailAfterStarted {
        started: PathBuf,
    }

    impl ChildObserver for FailAfterStarted {
        fn try_wait(&self, _child: &mut Child) -> std::io::Result<Option<ExitStatus>> {
            assert!(wait_for_file(&self.started));
            Err(std::io::Error::other("injected post-spawn observation failure"))
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_post_spawn_observation_failure_is_indeterminate() {
        let root = TempWorkspace::new();
        let observer = FailAfterStarted {
            started: root.path.join("started"),
        };
        let output = run_local_command_with_observer(
            "touch started; sleep 30",
            &root.path,
            Duration::from_secs(30),
            &CommandEnvironment::empty(),
            &CancellationToken::new(),
            ToolUpdateSink::disabled(),
            &observer,
        )
        .expect("post-spawn failures settle a receipt");
        assert!(matches!(
            output.termination,
            CommandTermination::Indeterminate { ref reason }
                if reason.contains("inspect state before retrying")
        ));
    }

    #[test]
    fn a_pre_spawn_failure_remains_an_operation_error() {
        let root = TempWorkspace::new();
        let missing = root.path.join("missing-workspace");
        let error = run_local_command(
            "printf never-runs",
            &missing,
            Duration::from_secs(1),
            &CommandEnvironment::empty(),
            &CancellationToken::new(),
            ToolUpdateSink::disabled(),
        )
        .expect_err("a missing cwd prevents spawn");
        assert_ne!(error.message(), "cancelled");
    }

    #[test]
    fn canonical_runner_emits_live_output_before_completion() {
        let root = TempWorkspace::new();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let root_path = root.path.clone();
        let task = thread::spawn(move || {
            let updates = ToolUpdateSink::new(move |update| {
                let _ = sender.send(update.content);
            });
            let output = run_local_command(
                "printf first; sleep 0.2; printf second",
                &root_path,
                Duration::from_secs(1),
                &CommandEnvironment::empty(),
                &CancellationToken::new(),
                updates,
            );
            task_finished.store(true, Ordering::Release);
            output
        });
        let update = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first update arrives before command completion");
        assert!(update.contains("first"));
        assert!(!finished.load(Ordering::Acquire));
        let output = task.join().expect("streaming task joins").expect("command settles");
        assert_eq!(output.termination, CommandTermination::Exited { code: 0 });
        assert_eq!(output.stdout, b"firstsecond");
    }

    #[cfg(unix)]
    #[test]
    fn timing_out_one_invocation_never_signals_another_process_group() {
        let root = TempWorkspace::new();
        let first_guard = ProcessGroupGuard::new();
        let second_guard = ProcessGroupGuard::new();
        let operations = Arc::new(LocalCodingOperations);
        let (first_receiver, first_task) = start_with_local_operations(
            Arc::clone(&operations),
            r#"echo $$ > first.scope.pid
sleep 30 & child=$!
echo "$child" > first.child.pid
touch first.started
wait "$child""#,
            root.path.clone(),
            Duration::from_millis(100),
            CancellationToken::new(),
        );
        let (second_receiver, second_task) = start_with_local_operations(
            Arc::clone(&operations),
            r#"echo $$ > second.scope.pid
sleep 30 & child=$!
echo "$child" > second.child.pid
touch second.started
while [ ! -e second.release ]; do sleep 0.01; done
kill "$child"
wait "$child" || true
printf second-done"#,
            root.path.clone(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        assert!(wait_for_file(&root.path.join("first.scope.pid")));
        first_guard.arm(read_pid(&root.path.join("first.scope.pid")));
        assert!(wait_for_file(&root.path.join("second.scope.pid")));
        second_guard.arm(read_pid(&root.path.join("second.scope.pid")));
        assert!(wait_for_file(&root.path.join("first.child.pid")));
        assert!(wait_for_file(&root.path.join("second.child.pid")));
        let second_child = read_pid(&root.path.join("second.child.pid"));

        let first = receive(first_receiver, first_task);
        assert_eq!(first.termination, CommandTermination::TimedOut);
        assert!(
            !wait_for_process_to_stop(second_child),
            "cleanup for the first command must not signal the second command's child"
        );

        fs::write(root.path.join("second.release"), b"").expect("second command releases");
        let second = receive(second_receiver, second_task);
        assert_eq!(second.termination, CommandTermination::Exited { code: 0 });
        assert_eq!(second.stdout, b"second-done");
        first_guard.disarm();
        second_guard.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_one_invocation_never_signals_another_process_group() {
        let root = TempWorkspace::new();
        let first_guard = ProcessGroupGuard::new();
        let second_guard = ProcessGroupGuard::new();
        let first_cancellation = CancellationToken::new();
        let (first_receiver, first_task) = start(
            r#"echo $$ > first.scope.pid
sleep 30 & child=$!
echo "$child" > first.child.pid
touch first.started
wait "$child""#,
            root.path.clone(),
            Duration::from_secs(30),
            first_cancellation.clone(),
        );
        let (second_receiver, second_task) = start(
            r#"echo $$ > second.scope.pid
sleep 30 & child=$!
echo "$child" > second.child.pid
touch second.started
while [ ! -e second.release ]; do sleep 0.01; done
kill "$child"
wait "$child" || true
printf second-done"#,
            root.path.clone(),
            Duration::from_secs(30),
            CancellationToken::new(),
        );
        assert!(wait_for_file(&root.path.join("first.scope.pid")));
        first_guard.arm(read_pid(&root.path.join("first.scope.pid")));
        assert!(wait_for_file(&root.path.join("second.scope.pid")));
        second_guard.arm(read_pid(&root.path.join("second.scope.pid")));
        assert!(wait_for_file(&root.path.join("second.child.pid")));
        let second_child = read_pid(&root.path.join("second.child.pid"));

        first_cancellation.cancel();
        let first = receive(first_receiver, first_task);
        assert_eq!(first.termination, CommandTermination::Cancelled);
        assert!(
            !wait_for_process_to_stop(second_child),
            "cancellation for the first command must not signal the second command's child"
        );

        fs::write(root.path.join("second.release"), b"").expect("second command releases");
        let second = receive(second_receiver, second_task);
        assert_eq!(second.termination, CommandTermination::Exited { code: 0 });
        first_guard.disarm();
        second_guard.disarm();
    }
}
