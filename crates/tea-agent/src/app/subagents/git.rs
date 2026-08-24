//! Narrow synchronous Git boundary for isolated subagent workspaces.
//!
//! Callers run this engine off the async polling path. Every command uses an
//! argument vector rather than a shell, and private indexes are passed only
//! through the process environment.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Debug)]
pub(super) struct GitRepository {
    root: PathBuf,
}

#[derive(Debug)]
pub(super) struct GitRun {
    pub(super) success: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitError {
    Unavailable { message: String },
    NotWorktree { path: PathBuf, message: String },
    Command {
        directory: PathBuf,
        arguments: Vec<String>,
        message: String,
    },
    Io { path: PathBuf, message: String },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { message } => write!(formatter, "Git is unavailable: {message}"),
            Self::NotWorktree { path, message } => {
                write!(formatter, "{} is not a Git worktree: {message}", path.display())
            }
            Self::Command {
                directory,
                arguments,
                message,
            } => write!(
                formatter,
                "git {} in {} failed: {message}",
                arguments.join(" "),
                directory.display()
            ),
            Self::Io { path, message } => write!(formatter, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for GitError {}

impl GitRepository {
    pub(super) fn discover(workspace: &Path) -> Result<Self, GitError> {
        let version = Command::new("git").arg("--version").output().map_err(|error| {
            GitError::Unavailable {
                message: error.to_string(),
            }
        })?;
        if !version.status.success() {
            return Err(GitError::Unavailable {
                message: output_message(&version),
            });
        }

        let probe = command_output(workspace, ["rev-parse", "--is-inside-work-tree"], &[])
            .map_err(|error| GitError::NotWorktree {
                path: workspace.to_path_buf(),
                message: error.to_string(),
            })?;
        if trim_line(&probe) != "true" {
            return Err(GitError::NotWorktree {
                path: workspace.to_path_buf(),
                message: "Git did not report a working tree".into(),
            });
        }
        let root = PathBuf::from(trim_line(&command_output(
            workspace,
            ["rev-parse", "--show-toplevel"],
            &[],
        )?));
        Ok(Self { root })
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn output<I, S>(
        &self,
        arguments: I,
        environment: &[(OsString, OsString)],
    ) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        command_output(&self.root, arguments, environment)
    }

    pub(super) fn output_in<I, S>(
        &self,
        directory: &Path,
        arguments: I,
        environment: &[(OsString, OsString)],
    ) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        command_output(directory, arguments, environment)
    }

    pub(super) fn optional_output<I, S>(
        &self,
        arguments: I,
        environment: &[(OsString, OsString)],
    ) -> Result<Option<Vec<u8>>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect::<Vec<_>>();
        let output = command(&self.root, &arguments, environment)?;
        if output.status.success() {
            Ok(Some(output.stdout))
        } else {
            Ok(None)
        }
    }

    pub(super) fn run_with_input<I, S>(
        &self,
        arguments: I,
        environment: &[(OsString, OsString)],
        input: &[u8],
    ) -> Result<GitRun, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let arguments = arguments
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect::<Vec<_>>();
        run_with_input(&self.root, &arguments, environment, input)
    }
}

pub(super) fn environment_with_index(index: &Path) -> Vec<(OsString, OsString)> {
    vec![(
        OsString::from("GIT_INDEX_FILE"),
        index.as_os_str().to_os_string(),
    )]
}

pub(super) fn synthetic_commit_environment() -> Vec<(OsString, OsString)> {
    BTreeMap::from([
        (OsString::from("GIT_AUTHOR_NAME"), OsString::from("Tea")),
        (OsString::from("GIT_AUTHOR_EMAIL"), OsString::from("tea@local.invalid")),
        (OsString::from("GIT_AUTHOR_DATE"), OsString::from("2000-01-01T00:00:00Z")),
        (OsString::from("GIT_COMMITTER_NAME"), OsString::from("Tea")),
        (
            OsString::from("GIT_COMMITTER_EMAIL"),
            OsString::from("tea@local.invalid"),
        ),
        (
            OsString::from("GIT_COMMITTER_DATE"),
            OsString::from("2000-01-01T00:00:00Z"),
        ),
    ])
    .into_iter()
    .collect()
}

pub(super) fn trim_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

fn command_output<I, S>(
    directory: &Path,
    arguments: I,
    environment: &[(OsString, OsString)],
) -> Result<Vec<u8>, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_os_string())
        .collect::<Vec<_>>();
    let output = command(directory, &arguments, environment)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(command_error(directory, &arguments, &output))
    }
}

fn command(
    directory: &Path,
    arguments: &[OsString],
    environment: &[(OsString, OsString)],
) -> Result<Output, GitError> {
    Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .output()
        .map_err(|error| GitError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })
}

fn run_with_input(
    directory: &Path,
    arguments: &[OsString],
    environment: &[(OsString, OsString)],
    input: &[u8],
) -> Result<GitRun, GitError> {
    let mut child = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?;
    child
        .stdin
        .as_mut()
        .expect("piped Git stdin is available")
        .write_all(input)
        .map_err(|error| GitError::Io {
            path: directory.to_path_buf(),
            message: error.to_string(),
        })?;
    let output = child.wait_with_output().map_err(|error| GitError::Io {
        path: directory.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(GitRun {
        success: output.status.success(),
    })
}

fn command_error(directory: &Path, arguments: &[OsString], output: &Output) -> GitError {
    GitError::Command {
        directory: directory.to_path_buf(),
        arguments: arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect(),
        message: output_message(output),
    }
}

fn output_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        stderr
    }
}
