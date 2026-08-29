use std::ffi::{OsStr, OsString};
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use tea_core::state::ThinkingLevel;

/// Explicit command-line inputs accepted by the v1 terminal host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliOptions {
    provider: Option<OsString>,
    model: Option<OsString>,
    local_base_url: Option<OsString>,
    local_context_window: Option<NonZeroU64>,
    cwd: Option<PathBuf>,
    prompt: Option<OsString>,
    tea_home: Option<PathBuf>,
    thinking: Option<ThinkingLevel>,
}

impl CliOptions {
    /// Parse explicit provider/model, local endpoint/capacity, thinking, prompt, and workspace
    /// options.
    pub fn parse<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        match parse_impl(args, false)? {
            CliCommand::Options(options) => Ok(options),
            CliCommand::Help => unreachable!("help is disabled for CliOptions::parse"),
            CliCommand::Version => unreachable!("version is disabled for CliOptions::parse"),
            CliCommand::Session(_) => {
                unreachable!("session commands are disabled for CliOptions::parse")
            }
        }
    }

    /// Parse startup arguments, including the conventional `-h`/`--help` command.
    pub fn parse_command<I>(args: I) -> Result<CliCommand, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        parse_impl(args, true)
    }

    /// Render the command-line usage text.
    pub const fn help_text() -> &'static str {
        "Usage: tea [OPTIONS]\n       tea session <inspect|dump|repair|rebuild-meta|verify|gc|export|restore> ...\n\nRead-only session commands:\n       tea session inspect <session-id> [--tea-home <path>]\n       tea session dump <session-id> [--tea-home <path>]\n\nOptions:\n    -h, --help                  Show this help text\n    -V, --version               Show the package version and Git revision\n        --provider <id>         Select a compiled provider\n        --model <id>            Select a compiled model\n        --local-base-url <url>  Set the local provider API root\n        --local-context-window <tokens>\n                                Set explicit local context capacity for automatic compaction\n        --thinking <level>      Set reasoning level (off, minimal, low, medium, high, xhigh, max)\n    -p, --prompt <message>      Stream one response and exit (requires provider/model)\n        --cwd <path>            Use path as the explicit workspace\n        --tea-home <path>       Use path as the explicit Tea extension home (default: ~/.tea)\n\nSession commands emit one JSON object to stdout. `inspect` and `dump` search all workspace roots below Tea home; other session commands take explicit directories. `gc` is a dry run unless --apply is supplied.\n"
    }

    /// Borrow the explicitly selected provider, if supplied.
    pub fn provider(&self) -> Option<&OsStr> {
        self.provider.as_deref()
    }

    /// Borrow the explicitly selected model, if supplied.
    pub fn model(&self) -> Option<&OsStr> {
        self.model.as_deref()
    }

    /// Borrow the explicit local provider API root, if supplied.
    pub fn local_base_url(&self) -> Option<&OsStr> {
        self.local_base_url.as_deref()
    }

    /// Return the explicit local context capacity, if supplied.
    pub fn local_context_window(&self) -> Option<NonZeroU64> {
        self.local_context_window
    }

    /// Borrow the explicit workspace authority, if supplied.
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// Borrow the one-shot prompt, if supplied.
    pub fn prompt(&self) -> Option<&OsStr> {
        self.prompt.as_deref()
    }

    /// Borrow the explicit Tea extension home override, if supplied.
    pub fn tea_home(&self) -> Option<&std::path::Path> {
        self.tea_home.as_deref()
    }

    /// Return the selected reasoning budget, defaulting to disabled.
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.thinking.unwrap_or_default()
    }

    pub(super) fn set_thinking_level(&mut self, level: ThinkingLevel) {
        self.thinking = Some(level);
    }

    fn set(&mut self, slot: OptionSlot, value: OsString) -> Result<(), CliError> {
        let destination = match slot {
            OptionSlot::Provider => &mut self.provider,
            OptionSlot::Model => &mut self.model,
            OptionSlot::LocalBaseUrl => &mut self.local_base_url,
            OptionSlot::Prompt => &mut self.prompt,
            OptionSlot::TeaHome => {
                if self.tea_home.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError::DuplicateOption(slot.name()));
                }
                return Ok(());
            }
            OptionSlot::Cwd => {
                if self.cwd.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError::DuplicateOption(slot.name()));
                }
                return Ok(());
            }
            OptionSlot::Thinking => {
                let level = parse_thinking_level(&value)?;
                if self.thinking.replace(level).is_some() {
                    return Err(CliError::DuplicateOption(slot.name()));
                }
                return Ok(());
            }
            OptionSlot::LocalContextWindow => {
                let context_window = parse_local_context_window(&value)?;
                if self.local_context_window.replace(context_window).is_some() {
                    return Err(CliError::DuplicateOption(slot.name()));
                }
                return Ok(());
            }
        };
        if destination.replace(value).is_some() {
            Err(CliError::DuplicateOption(slot.name()))
        } else {
            Ok(())
        }
    }
}

/// A parsed `tea` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliCommand {
    /// Start the terminal host with explicit startup options.
    Options(CliOptions),
    /// Print command-line usage and exit.
    Help,
    /// Print the package and source revision identity and exit.
    Version,
    /// Run one explicit, machine-readable durable-session operation.
    Session(SessionCommand),
}

/// An explicit persistence operation over a durable session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCommand {
    /// Read-only inspection addressed by session ID, searching all workspace roots.
    Inspect { session_id: OsString, tea_home: Option<PathBuf> },
    /// Legacy path-addressed read-only replay and prefix identity inspection.
    InspectPath { directory: PathBuf },
    /// Dump the authoritative JSONL records addressed by session ID.
    Dump { session_id: OsString, tea_home: Option<PathBuf> },
    /// Explicitly remove only an unterminated final JSONL tail.
    Repair { directory: PathBuf },
    /// Rebuild disposable `HEAD` and terminal-host `meta.json` caches from a
    /// validated authoritative prefix.
    RebuildMeta { directory: PathBuf },
    /// Replay and verify session-owned immutable objects.
    Verify {
        directory: PathBuf,
        additional_roots: Vec<OsString>,
    },
    /// Plan collection, or apply the plan when requested.
    Gc {
        directory: PathBuf,
        additional_roots: Vec<OsString>,
        apply: bool,
    },
    /// Create a non-overwriting portable export.
    Export {
        source: PathBuf,
        destination: PathBuf,
        additional_roots: Vec<OsString>,
    },
    /// Restore an export into a new non-overwriting directory.
    Restore {
        source: PathBuf,
        destination: PathBuf,
    },
}

fn parse_impl<I>(args: I, recognize_help: bool) -> Result<CliCommand, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = args.into_iter();
    let _program = arguments.next();
    let first = arguments.next();
    if recognize_help && first.as_deref() == Some(OsStr::new("session")) {
        return parse_session_command(arguments);
    }
    let mut arguments = first.into_iter().chain(arguments);
    let mut options = CliOptions::default();
    while let Some(argument) = arguments.next() {
        let slot = match argument.to_string_lossy().as_ref() {
            "-h" | "--help" if recognize_help => return Ok(CliCommand::Help),
            "-V" | "--version" if recognize_help => return Ok(CliCommand::Version),
            "--provider" => OptionSlot::Provider,
            "--model" => OptionSlot::Model,
            "--local-base-url" => OptionSlot::LocalBaseUrl,
            "--local-context-window" => OptionSlot::LocalContextWindow,
            "--thinking" => OptionSlot::Thinking,
            "-p" | "--prompt" => OptionSlot::Prompt,
            "--cwd" => OptionSlot::Cwd,
            "--tea-home" => OptionSlot::TeaHome,
            _ if argument.as_os_str().to_string_lossy().starts_with('-') => {
                return Err(CliError::UnknownOption(argument));
            }
            _ => return Err(CliError::UnexpectedArgument(argument)),
        };
        let value = arguments
            .next()
            .ok_or_else(|| CliError::MissingValue(slot.name()))?;
        if value.is_empty() {
            return Err(CliError::EmptyValue(slot.name()));
        }
        options.set(slot, value)?;
    }
    Ok(CliCommand::Options(options))
}

fn parse_session_command<I>(mut arguments: I) -> Result<CliCommand, CliError>
where
    I: Iterator<Item = OsString>,
{
    let operation = arguments
        .next()
        .ok_or(CliError::MissingValue("session operation"))?;
    let operation_name = operation.to_string_lossy();
    let required_path = |arguments: &mut I, label| {
        arguments
            .next()
            .map(PathBuf::from)
            .ok_or(CliError::MissingValue(label))
    };
    let command = match operation_name.as_ref() {
        "inspect" => {
            let session_id = arguments.next().ok_or(CliError::MissingValue("session ID"))?;
            let tea_home = parse_session_tea_home(&mut arguments)?;
            SessionCommand::Inspect { session_id, tea_home }
        }
        "dump" => {
            let session_id = arguments.next().ok_or(CliError::MissingValue("session ID"))?;
            let tea_home = parse_session_tea_home(&mut arguments)?;
            SessionCommand::Dump { session_id, tea_home }
        }
        "repair" => SessionCommand::Repair {
            directory: required_path(&mut arguments, "session directory")?,
        },
        "rebuild-meta" => SessionCommand::RebuildMeta {
            directory: required_path(&mut arguments, "session directory")?,
        },
        "verify" => SessionCommand::Verify {
            directory: required_path(&mut arguments, "session directory")?,
            additional_roots: parse_root_flags(&mut arguments)?,
        },
        "gc" => {
            let directory = required_path(&mut arguments, "session directory")?;
            let (additional_roots, apply) = parse_gc_flags(&mut arguments)?;
            SessionCommand::Gc {
                directory,
                additional_roots,
                apply,
            }
        }
        "export" => SessionCommand::Export {
            source: required_path(&mut arguments, "source session directory")?,
            destination: required_path(&mut arguments, "export destination directory")?,
            additional_roots: parse_root_flags(&mut arguments)?,
        },
        "restore" => SessionCommand::Restore {
            source: required_path(&mut arguments, "source export directory")?,
            destination: required_path(&mut arguments, "restore destination directory")?,
        },
        _ => return Err(CliError::UnknownSessionOperation(operation)),
    };
    if let Some(argument) = arguments.next() {
        return Err(CliError::UnexpectedArgument(argument));
    }
    Ok(CliCommand::Session(command))
}

fn parse_session_tea_home<I>(arguments: &mut I) -> Result<Option<PathBuf>, CliError>
where
    I: Iterator<Item = OsString>,
{
    match arguments.next() {
        None => Ok(None),
        Some(flag) if flag == "--tea-home" => arguments
            .next()
            .map(PathBuf::from)
            .ok_or(CliError::MissingValue("--tea-home path"))
            .map(Some),
        Some(argument) => Err(CliError::UnexpectedArgument(argument)),
    }
}

fn parse_root_flags<I>(arguments: &mut I) -> Result<Vec<OsString>, CliError>
where
    I: Iterator<Item = OsString>,
{
    let mut roots = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument != "--root" {
            return Err(CliError::UnexpectedArgument(argument));
        }
        roots.push(
            arguments
                .next()
                .ok_or(CliError::MissingValue("--root artifact ID"))?,
        );
    }
    Ok(roots)
}

fn parse_gc_flags<I>(arguments: &mut I) -> Result<(Vec<OsString>, bool), CliError>
where
    I: Iterator<Item = OsString>,
{
    let mut roots = Vec::new();
    let mut apply = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--root" => roots.push(
                arguments
                    .next()
                    .ok_or(CliError::MissingValue("--root artifact ID"))?,
            ),
            "--apply" if !apply => apply = true,
            _ => return Err(CliError::UnexpectedArgument(argument)),
        }
    }
    Ok((roots, apply))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OptionSlot {
    Provider,
    Model,
    LocalBaseUrl,
    LocalContextWindow,
    Thinking,
    Prompt,
    Cwd,
    TeaHome,
}

impl OptionSlot {
    const fn name(self) -> &'static str {
        match self {
            Self::Provider => "--provider",
            Self::Model => "--model",
            Self::LocalBaseUrl => "--local-base-url",
            Self::LocalContextWindow => "--local-context-window",
            Self::Thinking => "--thinking",
            Self::Prompt => "-p/--prompt",
            Self::Cwd => "--cwd",
            Self::TeaHome => "--tea-home",
        }
    }
}

/// Errors produced by direct command-line parsing.
#[derive(Debug, Eq, PartialEq)]
pub enum CliError {
    /// An option had no following value.
    MissingValue(&'static str),
    /// An option was supplied more than once.
    DuplicateOption(&'static str),
    /// An option was supplied with an empty value.
    EmptyValue(&'static str),
    /// The option is not supported by v1.
    UnknownOption(OsString),
    /// A persistence operation name is not part of the explicit command surface.
    UnknownSessionOperation(OsString),
    /// An option value is not valid for its declared domain.
    InvalidValue {
        /// Option whose value was rejected.
        flag: &'static str,
        /// Rejected value.
        value: OsString,
    },
    /// Positional arguments are not supported by v1.
    UnexpectedArgument(OsString),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(formatter, "missing value for {flag}"),
            Self::DuplicateOption(flag) => write!(formatter, "duplicate option {flag}"),
            Self::EmptyValue(flag) => write!(formatter, "empty value for {flag}"),
            Self::UnknownOption(option) => write!(formatter, "unknown option {option:?}"),
            Self::UnknownSessionOperation(operation) => {
                write!(formatter, "unknown session operation {operation:?}")
            }
            Self::InvalidValue { flag, value } => {
                write!(formatter, "invalid value {value:?} for {flag}")
            }
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument {argument:?}")
            }
        }
    }
}

fn parse_thinking_level(value: &OsStr) -> Result<ThinkingLevel, CliError> {
    match value.to_string_lossy().as_ref() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(CliError::InvalidValue {
            flag: "--thinking",
            value: value.to_owned(),
        }),
    }
}

fn parse_local_context_window(value: &OsStr) -> Result<NonZeroU64, CliError> {
    value
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(NonZeroU64::new)
        .ok_or_else(|| CliError::InvalidValue {
            flag: "--local-context-window",
            value: value.to_owned(),
        })
}

impl std::error::Error for CliError {}
