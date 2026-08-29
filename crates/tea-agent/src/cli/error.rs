//! Typed failures at the lexopt boundary.

use std::ffi::OsString;
use std::fmt;

/// Errors produced before command business logic is entered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliError {
    /// An option had no following value.
    MissingValue(&'static str),
    /// An option had no following value and lexopt supplied a dynamic spelling.
    MissingValueOwned(String),
    /// An option was supplied more than once.
    DuplicateOption(&'static str),
    /// An option was supplied with an empty value.
    EmptyValue(&'static str),
    /// The option is not supported by the command's schema.
    UnknownOption(OsString),
    /// A persistence operation name is not part of the explicit command surface.
    UnknownSessionOperation(OsString),
    /// A top-level command name is not part of the explicit command surface.
    UnknownCommand(OsString),
    /// An option value is not valid for its declared domain.
    InvalidValue {
        /// Option whose value was rejected.
        flag: &'static str,
        /// Rejected value.
        value: OsString,
    },
    /// A positional argument is not accepted at this command boundary.
    UnexpectedArgument(OsString),
    /// The lexopt tokenizer found an otherwise unclassifiable malformed token.
    Lexopt(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(formatter, "missing value for {flag}"),
            Self::MissingValueOwned(flag) => write!(formatter, "missing value for {flag}"),
            Self::DuplicateOption(flag) => write!(formatter, "duplicate option {flag}"),
            Self::EmptyValue(flag) => write!(formatter, "empty value for {flag}"),
            Self::UnknownOption(option) => write!(formatter, "unknown option {option:?}"),
            Self::UnknownSessionOperation(operation) => {
                write!(formatter, "unknown session operation {operation:?}")
            }
            Self::UnknownCommand(command) => write!(formatter, "unknown command {command:?}"),
            Self::InvalidValue { flag, value } => {
                write!(formatter, "invalid value {value:?} for {flag}")
            }
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument {argument:?}")
            }
            Self::Lexopt(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for CliError {}
