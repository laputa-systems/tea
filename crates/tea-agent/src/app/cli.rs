use std::ffi::{OsStr, OsString};
use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;
use tea_core::ThinkingLevel;

/// Explicit command-line inputs accepted by the v0 terminal host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliOptions {
    provider: Option<OsString>,
    model: Option<OsString>,
    compaction_strategy: Option<OsString>,
    local_base_url: Option<OsString>,
    local_context_window: Option<NonZeroU64>,
    cwd: Option<PathBuf>,
    prompt: Option<OsString>,
    tea_home: Option<PathBuf>,
    thinking: Option<ThinkingLevel>,
}

impl CliOptions {
    /// Parse explicit provider/model, compaction, local endpoint/capacity, thinking, prompt, and
    /// workspace options.
    pub fn parse<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        match parse_impl(args, false)? {
            CliCommand::Options(options) => Ok(options),
            CliCommand::Help => unreachable!("help is disabled for CliOptions::parse"),
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
        "Usage: tea [OPTIONS]\n\nOptions:\n    -h, --help                  Show this help text\n        --provider <id>         Select a compiled provider\n        --model <id>            Select a compiled model\n        --compaction-strategy <id>\n                                Select an explicit compaction strategy experiment\n        --local-base-url <url>  Set the local provider API root\n        --local-context-window <tokens>\n                                Set explicit local context capacity for automatic compaction\n        --thinking <level>      Set reasoning level (off, minimal, low, medium, high, xhigh, max)\n    -p, --prompt <message>      Stream one response and exit (requires provider/model)\n        --cwd <path>            Use path as the explicit workspace\n        --tea-home <path>      Use path as the explicit Tea extension home (default: ~/.tea)\n"
    }

    /// Borrow the explicitly selected provider, if supplied.
    pub fn provider(&self) -> Option<&OsStr> {
        self.provider.as_deref()
    }

    /// Borrow the explicitly selected model, if supplied.
    pub fn model(&self) -> Option<&OsStr> {
        self.model.as_deref()
    }

    /// Borrow the explicitly selected compaction strategy, if supplied.
    pub fn compaction_strategy(&self) -> Option<&OsStr> {
        self.compaction_strategy.as_deref()
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

    fn set(&mut self, slot: OptionSlot, value: OsString) -> Result<(), CliError> {
        let destination = match slot {
            OptionSlot::Provider => &mut self.provider,
            OptionSlot::Model => &mut self.model,
            OptionSlot::CompactionStrategy => &mut self.compaction_strategy,
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
}

fn parse_impl<I>(args: I, recognize_help: bool) -> Result<CliCommand, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = args.into_iter();
    let _program = arguments.next();
    let mut options = CliOptions::default();
    while let Some(argument) = arguments.next() {
        let slot = match argument.to_string_lossy().as_ref() {
            "-h" | "--help" if recognize_help => return Ok(CliCommand::Help),
            "--provider" => OptionSlot::Provider,
            "--model" => OptionSlot::Model,
            "--compaction-strategy" => OptionSlot::CompactionStrategy,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OptionSlot {
    Provider,
    Model,
    CompactionStrategy,
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
            Self::CompactionStrategy => "--compaction-strategy",
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
    /// The option is not part of v0.
    UnknownOption(OsString),
    /// An option value is not valid for its declared domain.
    InvalidValue {
        /// Option whose value was rejected.
        flag: &'static str,
        /// Rejected value.
        value: OsString,
    },
    /// Positional arguments are not part of v0.
    UnexpectedArgument(OsString),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(formatter, "missing value for {flag}"),
            Self::DuplicateOption(flag) => write!(formatter, "duplicate option {flag}"),
            Self::EmptyValue(flag) => write!(formatter, "empty value for {flag}"),
            Self::UnknownOption(option) => write!(formatter, "unknown option {option:?}"),
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
