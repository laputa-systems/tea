//! The sole command-line boundary for the tea binary.
//!
//! lexopt::Parser is constructed here (and in the nested session module);
//! command metadata, validation, and generated help all come from
//! command.rs. The application modules receive typed values only after this
//! boundary has accepted the complete command.

mod error;
pub mod command;
mod dump;
mod export;
mod gc;
pub mod help;
mod inspect;
mod rebuild_meta;
mod repair;
mod restore;
mod session;
mod verify;
mod version;

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::process::ExitCode;

use lexopt::{Arg, Parser};
use tea_core::state::ThinkingLevel;
use tea_protocol::JsonValue;

use crate::app::{run_session_command, App, AppError};
use command::{CommandSpec, OptionKey, OptionSpec, ROOT_OPTIONS};
pub use error::CliError;

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
    /// Parse startup options without interpreting help, version, or subcommands.
    pub fn parse<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        match parse_impl(args, false)? {
            CliCommand::Options(options) => Ok(options),
            CliCommand::Help | CliCommand::Version | CliCommand::CommandHelp(_) => {
                Err(CliError::UnknownOption(OsString::from("--help")))
            }
            CliCommand::Session(_) => Err(CliError::UnexpectedArgument(OsString::from("session"))),
        }
    }

    /// Parse the complete command line, including control commands.
    pub fn parse_command<I>(args: I) -> Result<CliCommand, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        parse_impl(args, true)
    }

    /// Render generated top-level help. Kept as a compatibility façade for
    /// library users; the text itself is rendered from the shared schema.
    pub fn help_text() -> String {
        help::render_root()
    }

    pub fn provider(&self) -> Option<&OsStr> {
        self.provider.as_deref()
    }
    pub fn model(&self) -> Option<&OsStr> {
        self.model.as_deref()
    }
    pub fn local_base_url(&self) -> Option<&OsStr> {
        self.local_base_url.as_deref()
    }
    pub fn local_context_window(&self) -> Option<NonZeroU64> {
        self.local_context_window
    }
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }
    pub fn prompt(&self) -> Option<&OsStr> {
        self.prompt.as_deref()
    }
    pub fn tea_home(&self) -> Option<&std::path::Path> {
        self.tea_home.as_deref()
    }
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.thinking.unwrap_or_default()
    }
    pub(super) fn set_thinking_level(&mut self, level: ThinkingLevel) {
        self.thinking = Some(level);
    }

    fn set(&mut self, option: &'static OptionSpec, value: OsString) -> Result<(), CliError> {
        let destination = match option.key {
            OptionKey::Provider => &mut self.provider,
            OptionKey::Model => &mut self.model,
            OptionKey::LocalBaseUrl => &mut self.local_base_url,
            OptionKey::Prompt => &mut self.prompt,
            OptionKey::TeaHome => {
                if self.tea_home.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError::DuplicateOption(option.error_name));
                }
                return Ok(());
            }
            OptionKey::Cwd => {
                if self.cwd.replace(PathBuf::from(value)).is_some() {
                    return Err(CliError::DuplicateOption(option.error_name));
                }
                return Ok(());
            }
            OptionKey::Thinking => {
                let level = parse_thinking_level(&value)?;
                if self.thinking.replace(level).is_some() {
                    return Err(CliError::DuplicateOption(option.error_name));
                }
                return Ok(());
            }
            OptionKey::LocalContextWindow => {
                let context_window = parse_local_context_window(&value)?;
                if self.local_context_window.replace(context_window).is_some() {
                    return Err(CliError::DuplicateOption(option.error_name));
                }
                return Ok(());
            }
            OptionKey::Help | OptionKey::Version | OptionKey::Root | OptionKey::Apply => {
                return Err(CliError::UnknownOption(OsString::from(option.error_name)));
            }
        };
        if destination.replace(value).is_some() {
            Err(CliError::DuplicateOption(option.error_name))
        } else {
            Ok(())
        }
    }
}

/// A parsed tea command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliCommand {
    Options(CliOptions),
    Help,
    Version,
    /// Detailed help for a command selected before required-value validation.
    CommandHelp(&'static CommandSpec),
    Session(SessionCommand),
}

/// An explicit persistence operation over a durable session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCommand {
    Inspect { session_id: OsString, tea_home: Option<PathBuf> },
    InspectPath { directory: PathBuf },
    Dump { session_id: OsString, tea_home: Option<PathBuf> },
    Repair { directory: PathBuf },
    RebuildMeta { directory: PathBuf },
    Verify { directory: PathBuf, additional_roots: Vec<OsString> },
    Gc { directory: PathBuf, additional_roots: Vec<OsString>, apply: bool },
    Export { source: PathBuf, destination: PathBuf, additional_roots: Vec<OsString> },
    Restore { source: PathBuf, destination: PathBuf },
}

fn parse_impl<I>(args: I, recognize_control: bool) -> Result<CliCommand, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut parser = Parser::from_iter(args);
    let first = parser
        .next()
        .map_err(map_lexopt_error)?
        .map(OwnedArg::from);
    match first {
        None => Ok(CliCommand::Options(CliOptions::default())),
        Some(OwnedArg::Value(command))
            if command == OsStr::new(command::SESSION.name) =>
        {
            if recognize_control {
                session::parse(&mut parser, true)
            } else {
                Err(CliError::UnexpectedArgument(command))
            }
        }
        Some(OwnedArg::Value(command)) if recognize_control => {
            Err(CliError::UnknownCommand(command))
        }
        Some(argument) => parse_root_tokens(&mut parser, argument, recognize_control),
    }
}

fn parse_root_tokens(
    parser: &mut Parser,
    first: OwnedArg,
    recognize_control: bool,
) -> Result<CliCommand, CliError> {
    let mut options = CliOptions::default();
    let mut argument = Some(first);
    loop {
        let current = match argument.take() {
            Some(current) => current,
            None => match parser.next().map_err(map_lexopt_error)? {
                Some(current) => current.into(),
                None => break,
            },
        };
        match current {
            OwnedArg::Value(value) => return Err(CliError::UnexpectedArgument(value)),
            OwnedArg::Short(_) | OwnedArg::Long(_) => {
                let option = find_owned_option(&current, ROOT_OPTIONS)
                    .ok_or_else(|| CliError::UnknownOption(unknown_owned_option(&current)))?;
                if option.key == OptionKey::Help {
                    if recognize_control {
                        return control_result(parser, CliCommand::Help);
                    }
                    return Err(CliError::UnknownOption(unknown_owned_option(&current)));
                }
                if option.key == OptionKey::Version {
                    if recognize_control {
                        return control_result(parser, CliCommand::Version);
                    }
                    return Err(CliError::UnknownOption(unknown_owned_option(&current)));
                }
                let value = option_value(parser, option)?;
                options.set(option, value)?;
            }
        }
    }
    Ok(CliCommand::Options(options))
}

fn control_result(parser: &mut Parser, command: CliCommand) -> Result<CliCommand, CliError> {
    match parser.next() {
        Err(error) => Err(map_lexopt_error(error)),
        Ok(_) => Ok(command),
    }
}

fn option_value(parser: &mut Parser, option: &'static OptionSpec) -> Result<OsString, CliError> {
    let value = parser.value().map_err(|error| match error {
        lexopt::Error::MissingValue { .. } => CliError::MissingValue(option.error_name),
        other => map_lexopt_error(other),
    })?;
    if value.is_empty() {
        return Err(CliError::EmptyValue(option.error_name));
    }
    Ok(value)
}

#[derive(Clone, Debug)]
enum OwnedArg {
    Short(char),
    Long(String),
    Value(OsString),
}

impl From<Arg<'_>> for OwnedArg {
    fn from(argument: Arg<'_>) -> Self {
        match argument {
            Arg::Short(short) => Self::Short(short),
            Arg::Long(long) => Self::Long(long.to_owned()),
            Arg::Value(value) => Self::Value(value),
        }
    }
}

fn find_owned_option(
    argument: &OwnedArg,
    options: &'static [OptionSpec],
) -> Option<&'static OptionSpec> {
    match argument {
        OwnedArg::Short(short) => options.iter().find(|spec| {
            spec.short == Some(*short) || spec.aliases.iter().any(|alias| *alias == *short)
        }),
        OwnedArg::Long(long) => options.iter().find(|spec| spec.long == long),
        OwnedArg::Value(_) => None,
    }
}

fn unknown_owned_option(argument: &OwnedArg) -> OsString {
    match argument {
        OwnedArg::Short(short) => OsString::from(format!("-{short}")),
        OwnedArg::Long(long) => OsString::from(format!("--{long}")),
        OwnedArg::Value(value) => value.clone(),
    }
}

fn map_lexopt_error(error: lexopt::Error) -> CliError {
    match error {
        lexopt::Error::MissingValue { option: Some(option) } => CliError::MissingValueOwned(option),
        lexopt::Error::UnexpectedArgument(argument) => CliError::UnexpectedArgument(argument),
        lexopt::Error::UnexpectedOption(option) => CliError::UnknownOption(OsString::from(option)),
        lexopt::Error::UnexpectedValue { value, .. } => CliError::UnexpectedArgument(value),
        other => CliError::Lexopt(other.to_string()),
    }
}

fn parse_thinking_level(value: &OsStr) -> Result<ThinkingLevel, CliError> {
    let level = value.to_str().ok_or_else(|| CliError::InvalidValue {
        flag: "--thinking",
        value: value.to_owned(),
    })?;
    match level {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(CliError::InvalidValue { flag: "--thinking", value: value.to_owned() }),
    }
}

fn parse_local_context_window(value: &OsStr) -> Result<NonZeroU64, CliError> {
    value
        .to_str()
        .and_then(|text| text.parse::<u64>().ok())
        .and_then(NonZeroU64::new)
        .ok_or_else(|| CliError::InvalidValue {
            flag: "--local-context-window",
            value: value.to_owned(),
        })
}

/// Execute a complete command and convert its outcome to the process exit code.
pub fn run<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    match CliOptions::parse_command(args.into_iter().map(Into::into)) {
        Ok(CliCommand::Help) => {
            print!("{}", help::render_root());
            ExitCode::SUCCESS
        }
        Ok(CliCommand::CommandHelp(command)) => {
            print!("{}", help::render_command(command));
            ExitCode::SUCCESS
        }
        Ok(CliCommand::Version) => {
            println!("{}", version::line());
            ExitCode::SUCCESS
        }
        Ok(CliCommand::Session(command)) => match run_session_command(command) {
            Ok(output) => {
                println!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                print_session_error(&error);
                eprintln!("tea: {error}");
                ExitCode::from(2)
            }
        },
        Ok(CliCommand::Options(options)) => {
            let prompt = options.prompt().map(OsStr::to_owned);
            let mut app = App::new(options);
            let result = match prompt {
                Some(prompt) => match prompt.to_str() {
                    Some(prompt) => app.run_prompt(prompt.to_owned()),
                    None => Err(AppError::Setup("-p/--prompt must be valid UTF-8".into())),
                },
                None => app.run(),
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("tea: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Err(error) => {
            eprintln!("tea: {error}");
            eprintln!("{}", help::usage_hint());
            ExitCode::from(2)
        }
    }
}

fn print_session_error(error: &AppError) {
    let output = JsonValue::object([
        ("error", JsonValue::String(error.to_string())),
        ("ok", JsonValue::Bool(false)),
    ])
    .to_json_string()
    .expect("session command error JSON is encodable");
    println!("{output}");
}

#[cfg(test)]
mod tests {
    use super::{help, CliCommand, CliError, CliOptions, SessionCommand};
    use super::command;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use lexopt::{Arg, Parser};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn schema_is_complete_and_help_is_generated() {
        command::validate_schema().expect("command metadata is valid");
        let help = help::render_root();
        for name in [
            "inspect", "dump", "repair", "rebuild-meta", "verify", "gc", "export", "restore",
        ] {
            assert!(help.contains(&format!("tea session {name}")), "missing {name} reference");
        }
        assert!(!help.contains("Commands:"), "root help must not add a duplicate index");
    }

    #[test]
    fn every_session_command_accepts_help_before_required_values() {
        for operation in [
            "session",
            "inspect",
            "dump",
            "repair",
            "rebuild-meta",
            "verify",
            "gc",
            "export",
            "restore",
        ] {
            for help_flag in ["-h", "--help"] {
                let invocation = if operation == "session" {
                    args(&["tea", "session", help_flag])
                } else {
                    args(&["tea", "session", operation, help_flag])
                };
                assert!(
                    matches!(CliOptions::parse_command(invocation), Ok(CliCommand::CommandHelp(_))),
                    "{operation} did not accept {help_flag}"
                );
            }
        }
    }

    #[test]
    fn lexopt_clusters_equals_values_and_repeated_session_roots() {
        let mut parser = Parser::from_iter(args(&["tea", "-phello", "--provider=mock", "tail"]));
        assert!(matches!(parser.next().expect("short token parses"), Some(Arg::Short('p'))));
        assert_eq!(parser.value().expect("short value parses"), OsString::from("hello"));
        assert!(matches!(parser.next().expect("long token parses"), Some(Arg::Long("provider"))));
        assert_eq!(parser.value().expect("equals value parses"), OsString::from("mock"));
        assert!(matches!(parser.next().expect("positional token parses"), Some(Arg::Value(value)) if value == OsString::from("tail")));
        assert_eq!(
            CliOptions::parse_command(args(&["tea", "-phello", "--provider=mock"])),
            Ok(CliCommand::Options(CliOptions {
                prompt: Some(OsString::from("hello")),
                provider: Some(OsString::from("mock")),
                ..CliOptions::default()
            }))
        );
        assert!(matches!(
            CliOptions::parse_command(args(&[
                "tea", "session", "verify", "/tmp/session", "--root=a", "--root", "b"
            ])),
            Ok(CliCommand::Session(SessionCommand::Verify { additional_roots, .. }))
                if additional_roots == args(&["a", "b"])
        ));
    }

    #[test]
    fn command_help_and_version_are_successful_control_paths() {
        assert!(matches!(
            CliOptions::parse_command(args(&["tea", "session", "inspect", "--help"])),
            Ok(CliCommand::CommandHelp(_))
        ));
        assert_eq!(
            CliOptions::parse_command(args(&["tea", "-v"])),
            Ok(CliCommand::Version)
        );
        assert_eq!(
            CliOptions::parse_command(args(&["tea", "-V"])),
            Ok(CliCommand::Version)
        );
        assert_eq!(crate::build_info::GIT_SHA.len(), 7);
        assert!(crate::build_info::GIT_SHA.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(matches!(
            CliOptions::parse_command(args(&["tea", "not-a-command"])),
            Err(CliError::UnknownCommand(_))
        ));
        assert!(matches!(
            CliOptions::parse_command(args(&["tea", "session", "inspect", "--"])),
            Err(CliError::MissingValue("SESSION_ID"))
        ));
        assert_eq!(
            CliOptions::parse_command(args(&["tea", "session", "repair", "--", "-leading"])),
            Ok(CliCommand::Session(SessionCommand::Repair {
                directory: PathBuf::from("-leading"),
            }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn positional_values_preserve_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let session_id = OsString::from_vec(vec![b's', b'i', 0xff]);
        let parsed = CliOptions::parse_command(
            [OsString::from("tea"), OsString::from("session"), OsString::from("inspect"), session_id.clone()]
        )
        .expect("non-UTF-8 session ID remains an opaque CLI value");
        assert_eq!(
            parsed,
            CliCommand::Session(SessionCommand::Inspect {
                session_id,
                tea_home: None,
            })
        );
    }
}
