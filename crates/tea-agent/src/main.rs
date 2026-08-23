//! `tea` command-line entry point.
#![allow(clippy::result_large_err)]

use tea_agent::{run_session_command, App, AppError, CliCommand, CliOptions};
use tea_protocol::JsonValue;

fn main() {
    if let Err(error) = run() {
        eprintln!("tea: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), AppError> {
    // Keep startup inputs explicit and local.  In particular, this does not inspect a Pi
    // installation or discover configuration from the environment.
    match CliOptions::parse_command(std::env::args_os())? {
        CliCommand::Help => {
            print!("{}", CliOptions::help_text());
            Ok(())
        }
        CliCommand::Session(command) => match run_session_command(command) {
            Ok(output) => {
                println!("{output}");
                Ok(())
            }
            Err(error) => {
                let output = JsonValue::object([
                    ("error", JsonValue::String(error.to_string())),
                    ("ok", JsonValue::Bool(false)),
                ])
                .to_json_string()
                .expect("session command error JSON is encodable");
                println!("{output}");
                Err(error)
            }
        },
        CliCommand::Options(options) => {
            let prompt = options.prompt().map(std::ffi::OsStr::to_owned);
            let mut app = App::new(options);
            match prompt {
                Some(prompt) => {
                    let prompt = prompt
                        .to_str()
                        .ok_or_else(|| AppError::Setup("-p/--prompt must be valid UTF-8".into()))?;
                    app.run_prompt(prompt.to_owned())
                }
                None => app.run(),
            }
        }
    }
}
