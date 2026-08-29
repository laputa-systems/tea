//! The tea session command and its leaf command parsers.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use lexopt::{Arg, Parser};

use super::command::SESSION_OPTIONS;
use super::command::{find_option, CommandSpec, OptionKey};
use super::{
    dump, export, gc, inspect, rebuild_meta, repair, restore, verify, CliCommand, CliError,
    SessionCommand,
};

/// Parse the parser state immediately after the top-level session value.
pub(crate) fn parse(parser: &mut Parser, recognize_control: bool) -> Result<CliCommand, CliError> {
    let operation = parser
        .next()
        .map_err(map_lexopt_error)?
        .ok_or(CliError::MissingValue("session operation"))?;
    match operation {
        Arg::Short(_) | Arg::Long(_) => {
            let option = find_option(&operation, SESSION_OPTIONS)
                .ok_or_else(|| CliError::UnknownSessionOperation(argument_os_string(&operation)))?;
            if recognize_control && option.key == OptionKey::Help {
                super::control_result(parser, CliCommand::CommandHelp(&super::command::SESSION))
            } else if recognize_control && option.key == OptionKey::Version {
                super::control_result(parser, CliCommand::Version)
            } else {
                Err(CliError::UnknownSessionOperation(argument_os_string(
                    &operation,
                )))
            }
        }
        Arg::Value(operation) => {
            let spec = command_spec(&operation)
                .ok_or_else(|| CliError::UnknownSessionOperation(operation.clone()))?;
            parse_leaf(parser, spec, recognize_control)
        }
    }
}

fn command_spec(operation: &OsStr) -> Option<&'static CommandSpec> {
    let operation = operation.to_str()?;
    let specs: &[&'static CommandSpec] = &[
        &inspect::SPEC,
        &dump::SPEC,
        &repair::SPEC,
        &rebuild_meta::SPEC,
        &verify::SPEC,
        &gc::SPEC,
        &export::SPEC,
        &restore::SPEC,
    ];
    specs.iter().find(|spec| spec.name == operation).copied()
}

fn parse_leaf(
    parser: &mut Parser,
    spec: &'static CommandSpec,
    recognize_control: bool,
) -> Result<CliCommand, CliError> {
    let mut positionals = Vec::new();
    let mut tea_home = None;
    let mut roots = Vec::new();
    let mut apply = false;
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
                    OptionKey::Help | OptionKey::Version => {
                        return Err(CliError::UnknownOption(argument_os_string(&argument)));
                    }
                    OptionKey::TeaHome => {
                        tea_home = Some(PathBuf::from(value(parser, option)?));
                    }
                    OptionKey::Root => roots.push(value(parser, option)?),
                    OptionKey::Apply => apply = true,
                    _ => return Err(CliError::UnknownOption(argument_os_string(&argument))),
                }
            }
            Arg::Value(value) => {
                if positionals.len() >= spec.positionals.len() {
                    return Err(CliError::UnexpectedArgument(value));
                }
                if value.is_empty() {
                    return Err(CliError::EmptyValue(
                        spec.positionals[positionals.len()].name,
                    ));
                }
                positionals.push(value);
            }
        }
    }

    if positionals.len() < spec.positionals.iter().filter(|item| item.required).count() {
        let missing = spec
            .positionals
            .get(positionals.len())
            .map(|item| item.name)
            .unwrap_or("argument");
        return Err(CliError::MissingValue(missing));
    }

    let command = if std::ptr::eq(spec, &inspect::SPEC) {
        SessionCommand::Inspect {
            session_id: positionals[0].clone(),
            tea_home,
        }
    } else if std::ptr::eq(spec, &dump::SPEC) {
        SessionCommand::Dump {
            session_id: positionals[0].clone(),
            tea_home,
        }
    } else if std::ptr::eq(spec, &repair::SPEC) {
        SessionCommand::Repair {
            directory: PathBuf::from(&positionals[0]),
        }
    } else if std::ptr::eq(spec, &rebuild_meta::SPEC) {
        SessionCommand::RebuildMeta {
            directory: PathBuf::from(&positionals[0]),
        }
    } else if std::ptr::eq(spec, &verify::SPEC) {
        SessionCommand::Verify {
            directory: PathBuf::from(&positionals[0]),
            additional_roots: roots,
        }
    } else if std::ptr::eq(spec, &gc::SPEC) {
        SessionCommand::Gc {
            directory: PathBuf::from(&positionals[0]),
            additional_roots: roots,
            apply,
        }
    } else if std::ptr::eq(spec, &export::SPEC) {
        SessionCommand::Export {
            source: PathBuf::from(&positionals[0]),
            destination: PathBuf::from(&positionals[1]),
            additional_roots: roots,
        }
    } else if std::ptr::eq(spec, &restore::SPEC) {
        SessionCommand::Restore {
            source: PathBuf::from(&positionals[0]),
            destination: PathBuf::from(&positionals[1]),
        }
    } else {
        return Err(CliError::UnknownSessionOperation(OsString::from(spec.name)));
    };
    Ok(CliCommand::Session(command))
}

fn value(parser: &mut Parser, option: &super::command::OptionSpec) -> Result<OsString, CliError> {
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
