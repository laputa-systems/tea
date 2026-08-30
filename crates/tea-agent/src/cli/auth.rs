//! Parser for explicit Tea-owned provider authorization commands.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use lexopt::{Arg, Parser};

use super::command::{self, find_option, CommandSpec, OptionKey};
use super::{AuthCommand, CliCommand, CliError};

/// Parse immediately after the top-level `auth` command.
pub(crate) fn parse(parser: &mut Parser, recognize_control: bool) -> Result<CliCommand, CliError> {
    let operation = parser
        .next()
        .map_err(map_lexopt_error)?
        .ok_or(CliError::MissingValue("auth operation"))?;
    match operation {
        Arg::Short(_) | Arg::Long(_) => {
            let option = find_option(&operation, command::AUTH_OPTIONS)
                .ok_or_else(|| CliError::UnknownCommand(argument_os_string(&operation)))?;
            if recognize_control && option.key == OptionKey::Help {
                super::control_result(parser, CliCommand::CommandHelp(&command::AUTH))
            } else if recognize_control && option.key == OptionKey::Version {
                super::control_result(parser, CliCommand::Version)
            } else {
                Err(CliError::UnknownCommand(argument_os_string(&operation)))
            }
        }
        Arg::Value(operation) => {
            let spec = command_spec(&operation)
                .ok_or_else(|| CliError::UnknownCommand(operation.clone()))?;
            parse_leaf(parser, spec, recognize_control)
        }
    }
}

fn command_spec(operation: &OsStr) -> Option<&'static CommandSpec> {
    let operation = operation.to_str()?;
    [
        &command::AUTH_LOGIN,
        &command::AUTH_LOGOUT,
        &command::AUTH_STATUS,
    ]
    .iter()
    .find(|spec| spec.name == operation)
    .copied()
}

fn parse_leaf(
    parser: &mut Parser,
    spec: &'static CommandSpec,
    recognize_control: bool,
) -> Result<CliCommand, CliError> {
    let mut provider = None;
    let mut tea_home = None;
    let mut device = false;
    let mut no_open = false;
    let mut seen = Vec::new();

    while let Some(argument) = parser.next().map_err(map_lexopt_error)? {
        match argument {
            Arg::Short(_) | Arg::Long(_) => {
                let option = find_option(&argument, spec.options)
                    .ok_or_else(|| CliError::UnknownOption(argument_os_string(&argument)))?;
                if recognize_control && option.key == OptionKey::Help {
                    return super::control_result(parser, CliCommand::CommandHelp(spec));
                }
                if recognize_control && option.key == OptionKey::Version {
                    return super::control_result(parser, CliCommand::Version);
                }
                if seen.contains(&option.key) && !option.repeatable {
                    return Err(CliError::DuplicateOption(option.error_name));
                }
                seen.push(option.key);
                match option.key {
                    OptionKey::TeaHome => tea_home = Some(PathBuf::from(value(parser, option)?)),
                    OptionKey::Device => device = true,
                    OptionKey::NoOpen => no_open = true,
                    OptionKey::Help | OptionKey::Version => {
                        return Err(CliError::UnknownOption(argument_os_string(&argument)));
                    }
                    _ => return Err(CliError::UnknownOption(argument_os_string(&argument))),
                }
            }
            Arg::Value(value) => {
                if provider.replace(value.clone()).is_some() {
                    return Err(CliError::UnexpectedArgument(value));
                }
            }
        }
    }
    let provider = provider.ok_or(CliError::MissingValue("PROVIDER"))?;
    if provider.is_empty() {
        return Err(CliError::EmptyValue("PROVIDER"));
    }
    if std::ptr::eq(spec, &command::AUTH_LOGIN) {
        Ok(CliCommand::Auth(AuthCommand::Login {
            provider,
            device,
            no_open,
            tea_home,
        }))
    } else if std::ptr::eq(spec, &command::AUTH_LOGOUT) {
        if device || no_open {
            return Err(CliError::UnknownOption(OsString::from("auth login option")));
        }
        Ok(CliCommand::Auth(AuthCommand::Logout { provider, tea_home }))
    } else if std::ptr::eq(spec, &command::AUTH_STATUS) {
        if device || no_open {
            return Err(CliError::UnknownOption(OsString::from("auth login option")));
        }
        Ok(CliCommand::Auth(AuthCommand::Status { provider, tea_home }))
    } else {
        Err(CliError::UnknownCommand(OsString::from(spec.name)))
    }
}

fn value(parser: &mut Parser, option: &command::OptionSpec) -> Result<OsString, CliError> {
    let value = parser.value().map_err(|error| match error {
        lexopt::Error::MissingValue { .. } => CliError::MissingValue(option.error_name),
        other => map_lexopt_error(other),
    })?;
    if value.is_empty() {
        return Err(CliError::EmptyValue(option.error_name));
    }
    Ok(value)
}

fn argument_os_string(argument: &Arg<'_>) -> OsString {
    match argument {
        Arg::Short(short) => OsString::from(format!("-{short}")),
        Arg::Long(long) => OsString::from(format!("--{long}")),
        Arg::Value(value) => value.clone(),
    }
}

fn map_lexopt_error(error: lexopt::Error) -> CliError {
    match error {
        lexopt::Error::MissingValue {
            option: Some(option),
        } => CliError::MissingValueOwned(option),
        lexopt::Error::UnexpectedArgument(argument) => CliError::UnexpectedArgument(argument),
        lexopt::Error::UnexpectedOption(option) => CliError::UnknownOption(OsString::from(option)),
        lexopt::Error::UnexpectedValue { value, .. } => CliError::UnexpectedArgument(value),
        other => CliError::Lexopt(other.to_string()),
    }
}
